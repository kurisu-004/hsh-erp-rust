-- 018: 新增一级菜单"生产管理"及其两个二级菜单（制定工序 + 生产队列迁移）
-- 2026-09-11 part-worker-pool-federated-rocket 方案
--
-- 目的：
-- 1. INSERT 一级菜单 `production_group`（code='production_group'，title='生产管理'，path=NULL，
--    icon='Operation'，sort_order=27）
-- 2. INSERT 二级菜单 `part_process_chain`（code='part_process_chain'，title='制定工序'，
--    path='/parts/process-chains'，icon='SetUp'，sort_order=10，parent_id=production_group.id）
-- 3. UPDATE 现有 `worker_queue` 行：parent_id → production_group.id，title → '生产队列'，
--    sort_order → 20（仅当 worker_queue 已存在时）
-- 4. INSERT t_role_menu 三行：production_group + part_process_chain + worker_queue(新归属)
--    都授予 MANAGER + CLERK + INSPECTOR（每个组合一条；每行仅在对应菜单存在时插入）
-- 5. DELETE 旧 t_role_menu 行：移除 auth_group 下的 worker_queue 关系（避免角色守卫查出的
--    菜单树同时含两个 parent 的 worker_queue → 重复菜单）
--
-- 幂等性：
--   - INSERT 菜单按 code 走 WHERE NOT EXISTS
--   - UPDATE worker_queue 用 WHERE code='worker_queue' 守卫
--   - INSERT t_role_menu 用 ON CONFLICT DO NOTHING
--   - DELETE 用 menu_id 精确匹配（不影响其它菜单的 role 行）
-- 整个变更包在 BEGIN; ... COMMIT; 里，事务化。

BEGIN;

-- 临时序列：菜单雪花 id 本由 App 生成；migration 用专用序列生成大 id 兜底。
-- 与 `t_menu_id_seq` 物理隔离，避免与 App 雪花 id 撞号（同 migration 015 backfill 模式）。
CREATE TEMP SEQUENCE IF NOT EXISTS tmp_menu_migration_seq
    START WITH 100000000
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

-- 1. INSERT 一级菜单 production_group（按 code 幂等）
INSERT INTO public.t_menu (
    id, parent_id, code, title, path, icon, sort_order, is_active,
    version, created_at, created_by, updated_at, updated_by
)
SELECT
    nextval('tmp_menu_migration_seq'),
    NULL,
    'production_group',
    '生产管理',
    NULL,
    'Operation',
    27,
    true,
    0, now(), 0, now(), 0
WHERE NOT EXISTS (
    SELECT 1 FROM public.t_menu WHERE code = 'production_group' AND deleted_at IS NULL
);

-- 2. INSERT 二级菜单 part_process_chain（按 code 幂等）
INSERT INTO public.t_menu (
    id, parent_id, code, title, path, icon, sort_order, is_active,
    version, created_at, created_by, updated_at, updated_by
)
SELECT
    nextval('tmp_menu_migration_seq'),
    (SELECT id FROM public.t_menu WHERE code = 'production_group' AND deleted_at IS NULL LIMIT 1),
    'part_process_chain',
    '制定工序',
    '/parts/process-chains',
    'SetUp',
    10,
    true,
    0, now(), 0, now(), 0
WHERE NOT EXISTS (
    SELECT 1 FROM public.t_menu WHERE code = 'part_process_chain' AND deleted_at IS NULL
);

-- 3. UPDATE 现有 worker_queue 行的 parent_id / title / sort_order
--    仅在 worker_queue 存在时才执行 UPDATE（防止 dev/test 库无菜单时 UPDATE 0 行的语义歧义）
UPDATE public.t_menu
SET parent_id  = (SELECT id FROM public.t_menu WHERE code = 'production_group' AND deleted_at IS NULL LIMIT 1),
    title      = '生产队列',
    sort_order = 20,
    version    = version + 1,
    updated_at = now()
WHERE code = 'worker_queue'
  AND (parent_id IS NULL
       OR parent_id <> (SELECT id FROM public.t_menu WHERE code = 'production_group' AND deleted_at IS NULL LIMIT 1));

-- 4. DELETE 旧 t_role_menu 行：移除 worker_queue 与任意 role 的旧关联（保留单次执行，
--    步骤 5 重新 INSERT）。注意：worker_queue 实际存在时（生产库），auth_group 下的
--    旧关系一并被删；如果 worker_queue 不存在（dev/test 库），DELETE 0 行无影响。
DELETE FROM public.t_role_menu rm
USING public.t_menu m
WHERE rm.menu_id = m.id
  AND m.code = 'worker_queue'
  AND m.deleted_at IS NULL;

-- 5. INSERT t_role_menu：production_group + part_process_chain + worker_queue 都授予
--    MANAGER + CLERK + INSPECTOR。逐菜单插入：
--    - 仅在对应菜单存在时插入（dev/test 库无 worker_queue 时跳过该菜单）
--    - ON CONFLICT DO NOTHING 保证幂等
INSERT INTO public.t_role_menu (
    id, role, menu_id, version, created_at, created_by, updated_at, updated_by
)
SELECT
    nextval('tmp_menu_migration_seq'),
    r.role,
    m.id,
    0, now(), 0, now(), 0
FROM (
    VALUES ('MANAGER'),('CLERK'),('INSPECTOR')
) AS r(role)
CROSS JOIN (
    SELECT id, code FROM public.t_menu
    WHERE code IN ('production_group', 'part_process_chain')
      AND deleted_at IS NULL
) AS m
ON CONFLICT (role, menu_id) WHERE deleted_at IS NULL DO NOTHING;

-- worker_queue 仅在存在时插入（生产库必有，dev 库可能没有）
INSERT INTO public.t_role_menu (
    id, role, menu_id, version, created_at, created_by, updated_at, updated_by
)
SELECT
    nextval('tmp_menu_migration_seq'),
    r.role,
    (SELECT id FROM public.t_menu WHERE code = 'worker_queue' AND deleted_at IS NULL LIMIT 1),
    0, now(), 0, now(), 0
FROM (
    VALUES ('MANAGER'),('CLERK'),('INSPECTOR')
) AS r(role)
WHERE EXISTS (SELECT 1 FROM public.t_menu WHERE code = 'worker_queue' AND deleted_at IS NULL)
ON CONFLICT (role, menu_id) WHERE deleted_at IS NULL DO NOTHING;

COMMIT;
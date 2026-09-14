-- 024: 补建「生产队列」菜单（worker_queue）
-- 2026-09-14 follow-up 修复
--
-- 背景：
-- migration 018 "add production_menus" (2026-09-11) 创建 production_group + process_work_type +
-- part_process_chain，但 UPDATE worker_queue（假设 worker_queue 已存在）。实际 worker_queue 从未
-- 在任何 migration 里 INSERT——前端 router / production 管理组下第三个子菜单「生产队列」一直缺失。
--
-- 本迁移补建 worker_queue 菜单（挂在 production_group 下，sort_order=20）+ MANAGER + CLERK +
-- INSPECTOR 三角色授权（与 018 给 production_group + part_process_chain 的授权策略一致）。
--
-- 幂等：
--   - INSERT 菜单按 code 走 WHERE NOT EXISTS
--   - INSERT t_role_menu 用 ON CONFLICT (role, menu_id) WHERE deleted_at IS NULL DO NOTHING
-- 整个变更包在 BEGIN; ... COMMIT; 里，事务化。

BEGIN;

-- 临时序列：菜单雪花 id 本由 App 生成；migration 用专用序列生成大 id 兜底。
-- 与 `t_menu_id_seq` 物理隔离，避免与 App 雪花 id 撞号（同 migration 015 / 018 / 023 模式）。
CREATE TEMP SEQUENCE IF NOT EXISTS tmp_menu_migration_seq
    START WITH 100000000
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

-- 1. INSERT 二级菜单 worker_queue（挂在 production_group 下；按 code 幂等）
INSERT INTO public.t_menu (
    id, parent_id, code, title, path, icon, sort_order, is_active,
    version, created_at, created_by, updated_at, updated_by
)
SELECT
    nextval('tmp_menu_migration_seq'),
    (SELECT id FROM public.t_menu WHERE code = 'production_group' AND deleted_at IS NULL LIMIT 1),
    'worker_queue',
    '生产队列',
    '/workers/queue',
    'Operation',
    20,
    true,
    0, now(), 0, now(), 0
WHERE NOT EXISTS (
    SELECT 1 FROM public.t_menu WHERE code = 'worker_queue' AND deleted_at IS NULL
);

-- 2. INSERT t_role_menu：worker_queue 授予 MANAGER + CLERK + INSPECTOR（与 018 同款策略）
INSERT INTO public.t_role_menu (
    id, role, menu_id, version, created_at, created_by, updated_at, updated_by
)
SELECT
    nextval('tmp_menu_migration_seq'),
    'MANAGER',
    (SELECT id FROM public.t_menu WHERE code = 'worker_queue' AND deleted_at IS NULL LIMIT 1),
    0, now(), 0, now(), 0
WHERE EXISTS (
    SELECT 1 FROM public.t_menu WHERE code = 'worker_queue' AND deleted_at IS NULL
)
ON CONFLICT (role, menu_id) WHERE deleted_at IS NULL DO NOTHING;

INSERT INTO public.t_role_menu (
    id, role, menu_id, version, created_at, created_by, updated_at, updated_by
)
SELECT
    nextval('tmp_menu_migration_seq'),
    'CLERK',
    (SELECT id FROM public.t_menu WHERE code = 'worker_queue' AND deleted_at IS NULL LIMIT 1),
    0, now(), 0, now(), 0
WHERE EXISTS (
    SELECT 1 FROM public.t_menu WHERE code = 'worker_queue' AND deleted_at IS NULL
)
ON CONFLICT (role, menu_id) WHERE deleted_at IS NULL DO NOTHING;

INSERT INTO public.t_role_menu (
    id, role, menu_id, version, created_at, created_by, updated_at, updated_by
)
SELECT
    nextval('tmp_menu_migration_seq'),
    'INSPECTOR',
    (SELECT id FROM public.t_menu WHERE code = 'worker_queue' AND deleted_at IS NULL LIMIT 1),
    0, now(), 0, now(), 0
WHERE EXISTS (
    SELECT 1 FROM public.t_menu WHERE code = 'worker_queue' AND deleted_at IS NULL
)
ON CONFLICT (role, menu_id) WHERE deleted_at IS NULL DO NOTHING;

COMMIT;
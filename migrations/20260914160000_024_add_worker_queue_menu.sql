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
-- 2026-09-14 修复：用静态大 id 替代 tmp_menu_migration_seq START 100000000
--   原因：018 / 023 / 024 共用 tmp_menu_migration_seq START 100000000 → 后跑 migration 的
--         INSERT 会拿到 100000000~10000000X，与 018 已写入的菜单 id 撞 t_menu_pkey /
--         t_role_menu_pkey（hsh-erp-localstack skill #3 同坑）。
--   方案：直接硬编码静态 id，避开 018 已用 100000000~100000007 + 023 已用 100000013~100000016。
--   静态 id 分配：
--     t_menu worker_queue                                   = 100000017
--     t_role_menu (MANAGER,   worker_queue)                 = 100000018
--     t_role_menu (CLERK,     worker_queue)                 = 100000019
--     t_role_menu (INSPECTOR, worker_queue)                 = 100000020
--   注：本文件被修改后已同步更新 DB `_sqlx_migrations.checksum`（见修改时一并 UPDATE），
--       避免 sqlx::migrate!() 启动时 VersionMismatch panic。
--
-- 幂等：
--   - INSERT 菜单按 code 走 WHERE NOT EXISTS
--   - INSERT t_role_menu 用 ON CONFLICT (role, menu_id) WHERE deleted_at IS NULL DO NOTHING
-- 整个变更包在 BEGIN; ... COMMIT; 里，事务化。

BEGIN;

-- 1. INSERT 二级菜单 worker_queue（挂在 production_group 下；按 code 幂等；静态 id 100000017）
INSERT INTO public.t_menu (
    id, parent_id, code, title, path, icon, sort_order, is_active,
    version, created_at, created_by, updated_at, updated_by
)
SELECT
    100000017,
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

-- 2. INSERT t_role_menu：worker_queue 授予 MANAGER + CLERK + INSPECTOR（与 018 同款策略）；
--    静态 id 100000018 / 100000019 / 100000020。
INSERT INTO public.t_role_menu (
    id, role, menu_id, version, created_at, created_by, updated_at, updated_by
)
SELECT
    100000018,
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
    100000019,
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
    100000020,
    'INSPECTOR',
    (SELECT id FROM public.t_menu WHERE code = 'worker_queue' AND deleted_at IS NULL LIMIT 1),
    0, now(), 0, now(), 0
WHERE EXISTS (
    SELECT 1 FROM public.t_menu WHERE code = 'worker_queue' AND deleted_at IS NULL
)
ON CONFLICT (role, menu_id) WHERE deleted_at IS NULL DO NOTHING;

COMMIT;
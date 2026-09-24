-- 023: 新增一级菜单「模板管理」及其子菜单「模板编辑」（仅菜单条目，无 print_template 业务域）
-- 2026-09-14 t_menu 模板管理占位需求
--
-- 背景：
--   PrintTemplateDesigner 真实后端**不做**（2026-09-14 用户决策），
--   但前端菜单树需要显示「模板管理」一级 + 「模板编辑」子菜单，
--   方便后续接入（即便目前点击进入的空壳页面或后续替代实现）。
--   本 migration 只插菜单条目 + role_menu 授权，**不创建任何 print_template 业务表**
--   （t_print_template / t_print_template_field 等都不建）。
--
-- 目的：
--   1. INSERT 一级菜单 `template_management`（code='template_management'，title='模板管理'，
--      path=NULL，icon='Document'，sort_order=28；production_group 用 27，本菜单需靠后避免冲突）
--   2. INSERT 二级菜单 `print_templates_designer`（code='print_templates_designer'，
--      title='模板编辑'，path='/print-templates/designer'，icon='Document'，sort_order=10，
--      parent_id=template_management.id）
--   3. INSERT t_role_menu 两行：两条新菜单都授予 MANAGER（仅 MANAGER 可见；
--      与 production_group 的 MANAGER+CLERK+INSPECTOR 三角色策略不同，因为模板编辑
--      当前是占位菜单，仅管理员可见，避免给普通员工展示空壳入口）
--
-- 2026-09-14 修复：用静态大 id 替代 tmp_menu_migration_seq START 100000000
--   原因：018 / 023 / 024 共用 tmp_menu_migration_seq START 100000000 → 后跑 migration 的
--         INSERT 会拿到 100000000~10000000X，与 018 已写入的菜单 id 撞 t_menu_pkey /
--         t_role_menu_pkey（hsh-erp-localstack skill #3 同坑）。
--   方案：直接硬编码静态 id，避开 018 已用 100000000~100000007 区间。
--   静态 id 分配：
--     t_menu template_management                              = 100000013
--     t_menu print_templates_designer                         = 100000014
--     t_role_menu (MANAGER, template_management)              = 100000015
--     t_role_menu (MANAGER, print_templates_designer)         = 100000016
--   注：本文件被修改后已同步更新 DB `_sqlx_migrations.checksum`（见修改时一并 UPDATE），
--       避免 sqlx::migrate!() 启动时 VersionMismatch panic。
--
-- 幂等性：
--   - INSERT 菜单按 code 走 WHERE NOT EXISTS（与 018 风格一致）
--   - INSERT t_role_menu 用 ON CONFLICT (role, menu_id) WHERE deleted_at IS NULL DO NOTHING
--     （对齐 018 写法 + t_role_menu 的 partial unique index 语义）
--   - **禁止 DELETE-then-INSERT**：保留历史，重跑幂等。
-- 2026-09-24 清理：无显式事务包裹，由 sqlx::migrate! 默认按文件粒度加事务
--   （单文件失败自动回滚；嵌套 BEGIN/COMMIT 在 PG 18+ 会从 NOTICE 升级成 ERROR，
--    且无任何原子性收益）。

-- 1. INSERT 一级菜单 template_management（按 code 幂等；静态 id 100000013）
INSERT INTO public.t_menu (
    id, parent_id, code, title, path, icon, sort_order, is_active,
    version, created_at, created_by, updated_at, updated_by
)
SELECT
    100000013,
    NULL,
    'template_management',
    '模板管理',
    NULL,
    'Document',
    28,
    true,
    0, now(), 0, now(), 0
WHERE NOT EXISTS (
    SELECT 1 FROM public.t_menu WHERE code = 'template_management' AND deleted_at IS NULL
);

-- 2. INSERT 二级菜单 print_templates_designer（按 code 幂等；静态 id 100000014）
INSERT INTO public.t_menu (
    id, parent_id, code, title, path, icon, sort_order, is_active,
    version, created_at, created_by, updated_at, updated_by
)
SELECT
    100000014,
    (SELECT id FROM public.t_menu WHERE code = 'template_management' AND deleted_at IS NULL LIMIT 1),
    'print_templates_designer',
    '模板编辑',
    '/print-templates/designer',
    'Document',
    10,
    true,
    0, now(), 0, now(), 0
WHERE NOT EXISTS (
    SELECT 1 FROM public.t_menu WHERE code = 'print_templates_designer' AND deleted_at IS NULL
);

-- 3a. INSERT t_role_menu (MANAGER, template_management)；静态 id 100000015。
--     仅在 template_management 存在时插入；ON CONFLICT DO NOTHING 保证幂等。
INSERT INTO public.t_role_menu (
    id, role, menu_id, version, created_at, created_by, updated_at, updated_by
)
SELECT
    100000015,
    'MANAGER',
    (SELECT id FROM public.t_menu WHERE code = 'template_management' AND deleted_at IS NULL LIMIT 1),
    0, now(), 0, now(), 0
WHERE EXISTS (
    SELECT 1 FROM public.t_menu WHERE code = 'template_management' AND deleted_at IS NULL
)
ON CONFLICT (role, menu_id) WHERE deleted_at IS NULL DO NOTHING;

-- 3b. INSERT t_role_menu (MANAGER, print_templates_designer)；静态 id 100000016。
--     仅在 print_templates_designer 存在时插入；ON CONFLICT DO NOTHING 保证幂等。
INSERT INTO public.t_role_menu (
    id, role, menu_id, version, created_at, created_by, updated_at, updated_by
)
SELECT
    100000016,
    'MANAGER',
    (SELECT id FROM public.t_menu WHERE code = 'print_templates_designer' AND deleted_at IS NULL LIMIT 1),
    0, now(), 0, now(), 0
WHERE EXISTS (
    SELECT 1 FROM public.t_menu WHERE code = 'print_templates_designer' AND deleted_at IS NULL
)
ON CONFLICT (role, menu_id) WHERE deleted_at IS NULL DO NOTHING;

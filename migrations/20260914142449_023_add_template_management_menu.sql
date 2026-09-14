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
-- 幂等性：
--   - INSERT 菜单按 code 走 WHERE NOT EXISTS（与 018 风格一致）
--   - INSERT t_role_menu 用 ON CONFLICT (role, menu_id) WHERE deleted_at IS NULL DO NOTHING
--     （对齐 018 写法 + t_role_menu 的 partial unique index 语义）
--   - 整个变更包在 BEGIN; ... COMMIT; 里，事务化。
--   - **禁止 DELETE-then-INSERT**：保留历史，重跑幂等。

BEGIN;

-- 临时序列：菜单雪花 id 本由 App 生成；migration 用专用序列生成大 id 兜底。
-- 与 `t_menu_id_seq` 物理隔离，避免与 App 雪花 id 撞号（同 migration 018 模式）。
CREATE TEMP SEQUENCE IF NOT EXISTS tmp_menu_migration_seq
    START WITH 100000000
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;

-- 1. INSERT 一级菜单 template_management（按 code 幂等）
INSERT INTO public.t_menu (
    id, parent_id, code, title, path, icon, sort_order, is_active,
    version, created_at, created_by, updated_at, updated_by
)
SELECT
    nextval('tmp_menu_migration_seq'),
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

-- 2. INSERT 二级菜单 print_templates_designer（按 code 幂等）
INSERT INTO public.t_menu (
    id, parent_id, code, title, path, icon, sort_order, is_active,
    version, created_at, created_by, updated_at, updated_by
)
SELECT
    nextval('tmp_menu_migration_seq'),
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

-- 3. INSERT t_role_menu：template_management + print_templates_designer 都授予 MANAGER。
--    仅在对应菜单存在时插入；ON CONFLICT DO NOTHING 保证幂等。
INSERT INTO public.t_role_menu (
    id, role, menu_id, version, created_at, created_by, updated_at, updated_by
)
SELECT
    nextval('tmp_menu_migration_seq'),
    r.role,
    m.id,
    0, now(), 0, now(), 0
FROM (
    VALUES ('MANAGER')
) AS r(role)
CROSS JOIN (
    SELECT id, code FROM public.t_menu
    WHERE code IN ('template_management', 'print_templates_designer')
      AND deleted_at IS NULL
) AS m
ON CONFLICT (role, menu_id) WHERE deleted_at IS NULL DO NOTHING;

COMMIT;

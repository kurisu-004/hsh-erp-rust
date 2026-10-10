-- ============================================================================
-- seeds/rbac.sql —— RBAC 基线种子：内置角色 + 权限点 + 系统角色授权矩阵
-- ============================================================================
-- 2026-10-14 新增（iam::rbac 子模块配套）。
--
-- ## 与 seeds/menu.sql 的分工
--   * menu.sql 管「**看得见的入口**」：t_menu 菜单树 + t_role_menu 角色→菜单授权；
--   * 本文件管「**能做什么操作**」：t_role 角色定义 + t_permission 权限点 +
--     t_role_permission 角色→权限点授权。
-- 两者都是声明式、幂等、启动钩子自动跑（应用顺序 menu → rbac → [admin]，见
-- `src/infra/seed.rs`）。菜单与权限点是两个独立的授权维度，不要互相推算。
--
-- ## 幂等语义（连跑两次结果一致、不报错）
--   * t_role / t_permission：`ON CONFLICT (code) WHERE deleted_at IS NULL
--     DO NOTHING`。冲突目标必须带 `WHERE deleted_at IS NULL` —— 与 migration
--     里的 partial 唯一索引 `uk_t_role_code` / `uk_t_permission_code` 逐字对应，
--     少了它 PG 不会把该索引当 arbiter，重跑直接报错。
--     DO NOTHING（而非 DO UPDATE）是有意的：管理端可以改系统角色的中文名 /
--     描述 / 启停状态，重跑 seed 不得把这些修改覆盖回基线。插入只负责「从无到有」。
--   * t_role_permission：`WHERE NOT EXISTS` + `ON CONFLICT ... DO NOTHING`。
--     同样**不复活**管理端软删过的授权行 —— 撤销授权必须在重启后仍然生效。
--     `NOT EXISTS` 还挡住另一种崩溃：同一 (role_code, permission_code) 的旧行
--     已被软删（不在 partial 唯一索引里，ON CONFLICT 命中不到），带静态 id 的
--     INSERT 会撞 PK；先判「这张表里是否已有该组合（无论软删与否）」即绕开。
--
-- ## ID 段（与其它种子 / 生产雪花物理不相交）
--   * 9200000000000000+  本文件 t_role
--   * 9300000000000000+  本文件 t_permission
--   * 9400000000000000+  本文件 t_role_permission
--   * 9000000000000xxx   seeds/menu.sql 的 t_menu 静态段
--   * 9100000000000xxx   seeds/menu.sql 的 t_role_menu 派生段
--   * 900000000000000001 seeds/admin.sql
--   * ≥ 2.07e17          生产雪花 ID / fixture ID
--
-- ## ADMIN 元角色**不写任何授权行**
--   * `t_role_permission` 不写 ADMIN：通配在 `CurrentUser::require_permission`
--     里短路（`has_role(Role::Admin)` 恒真），写进去会把「元角色」降级成
--     「逐项勾选」，漏改一行即静默失效；
--   * `t_role_menu` 同样不写 ADMIN（见 menu.sql 的授权段）：ADMIN 的菜单由
--     `AccountService::menus_for_roles` / `RbacService::list_menus` 走全量查询。
--
-- ## `t_role.code` 与 Rust `Role` 枚举是两张表
-- 本文件的 6 个 code 与 `auth::rbac::Role` 枚举一一对应（枚举只覆盖这 6 项）；
-- 管理端新建的自定义角色只进 `t_role.code` varchar（并入 `t_user_role.role`），
-- **不进枚举** —— 授权判定时 `parse_role_string` 对它们返回 None，登录态角色
-- 列表走原始字符串。
-- ============================================================================

BEGIN;

-- ---------------------------------------------------------------------------
-- 1. t_role：内置角色（恰 6 行，is_system = true）
-- ---------------------------------------------------------------------------
-- 管理端可停用系统角色、不可删除、不可改 code（RbacService::update_role 硬切）。
-- name / description 是角色面板展示文案，改这里对已存在的库不生效（DO NOTHING）。
-- 6 行与 Rust `Role` 枚举一一对应：ADMIN / MANAGER / CLERK / INSPECTOR /
-- CNC_PROGRAMMER / SHELF_ACCOUNT。
INSERT INTO t_role (
    id, code, name, description, is_system, is_active,
    version, created_at, created_by, updated_at, updated_by
) VALUES
    (9200000000000001, 'ADMIN',          '超级管理员', '元角色：代码级通配全部菜单与权限，不写授权数据；后端拒写其 grants', true, true, 0, now(), 0, now(), 0),
    (9200000000000002, 'MANAGER',        '管理员',     '日常管理：账号、角色面板、生产配置与全部业务操作',                   true, true, 0, now(), 0, now(), 0),
    (9200000000000003, 'CLERK',          '文员',       '业务操作：零件、送货单、外协收发、生产队列下发',                     true, true, 0, now(), 0, now(), 0),
    (9200000000000004, 'INSPECTOR',      '品检员',     '品检流转：待品检、外协收发、图纸打印',                               true, true, 0, now(), 0, now(), 0),
    (9200000000000005, 'CNC_PROGRAMMER', 'CNC 程序员', '待编程处理与图纸查看',                                               true, true, 0, now(), 0, now(), 0),
    (9200000000000006, 'SHELF_ACCOUNT',  '货架一体机', '扫码台专用账号，必须绑定具体货架',                                   true, true, 0, now(), 0, now(), 0)
ON CONFLICT (code) WHERE deleted_at IS NULL DO NOTHING;

COMMIT;

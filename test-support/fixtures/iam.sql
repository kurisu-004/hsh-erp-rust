-- ============================================================================
--  iam 域集成测试 fixture (PR13 Phase I, 2026-09-24)
--
--  加载入口：test-support::fixture::load_iam_fixture(pool)
--  加载方式：sqlx::raw_sql(include_str!("../../fixtures/iam.sql")).execute(pool).await
--
--  ## 设计原则
--  - 所有 ID 走常量 9_000_000_000_000_000_110+ 区段（process_chain 1-9 /
--    part 10-49 / delivery 50-52 / production 60-69 / assembly 70+ /
--    shelf 80+ / statistics 90+ / outsource 100-104，物理不相交）。
--  - bcrypt cost=12 哈希复用 part.sql 字面值（同一明文 "changeme",
--    cost=12 → 同一哈希输出）。
--  - 时间列：审计字段用 now()。
--  - 不引 trigger / 不调 hsh_erp_rust 函数 —— SQL 仅 INSERT 静态行。
--
--  ## ID 段分配（110+）
--  MANAGER_USER_ID    110   fx_iam_manager（MANAGER role，active）
--  CLERK_USER_ID      111   fx_iam_clerk（CLERK role，active）
--  LONELY_USER_ID     112   fx_iam_lonely（无 role，active，for "no role" 测试）
--  TARGET_USER_ID     113   fx_iam_target（无 role，active，for SHELF_ACCOUNT 添加目标）
--  INACTIVE_USER_ID   114   fx_iam_inactive（is_active=false，for 停用登录测试）
--  MANAGER_ROLE_ID    115   MANAGER role（fx_iam_manager）
--  CLERK_ROLE_ID      116   CLERK role（fx_iam_clerk）
--  SHELF_A_ID         117   FX-SH-A1（PRODUCTION zone，SHELF_ACCOUNT 添加测试用）
--  SHELF_B_ID         118   FX-SH-B1（INSPECTION zone，duplicate role 测试）
--
--  ## 独立加载（不依赖 part 域基线）
--  iam 测试不需要 customer / process / work_type / part fixture；本 fixture
--  只覆盖 t_user / t_user_role / t_shelf 三张表共 9 行。load_iam_fixture 内部
--  不调 load_part_fixture。
--
--  ## 不预置 t_menu / t_role_menu / t_user 的部分字段
--  api.rs 的 _unused_silencer 仅为压 unused 警告，无测试实际使用 insert_menu /
--  add_role_menu，本 fixture 不预置菜单行（消除未使用菜单 ID 字段）。
-- ============================================================================

-- ---- 用户（密码明文 "changeme"，bcrypt cost=12）----
-- 5 用户对应 5 类场景：
--   manager  → /iam/users MANAGER 权限、login_success、/iam/me、refresh、change-password
--   clerk    → /iam/users CLERK 40300 守卫
--   lonely   → "无角色 403 20606"
--   target   → "添加 SHELF_ACCOUNT role 成功 / 重复 409"
--   inactive → "已停用账号 40101"
INSERT INTO t_user (id, username, password_hash, full_name, is_active, refresh_token_version, version, created_at, updated_at) VALUES
  (9000000000000000110, 'fx_iam_manager',  '$2b$12$KjlHPD7WcAbXC1v6RZxXKOfFHGJJDGS4bxfKGPRnLBmWmqFMvS/4W', 'FX IAM Manager',  true,  0, 0, now(), now()),
  (9000000000000000111, 'fx_iam_clerk',    '$2b$12$KjlHPD7WcAbXC1v6RZxXKOfFHGJJDGS4bxfKGPRnLBmWmqFMvS/4W', 'FX IAM Clerk',    true,  0, 0, now(), now()),
  (9000000000000000112, 'fx_iam_lonely',   '$2b$12$KjlHPD7WcAbXC1v6RZxXKOfFHGJJDGS4bxfKGPRnLBmWmqFMvS/4W', 'FX IAM Lonely',   true,  0, 0, now(), now()),
  (9000000000000000113, 'fx_iam_target',   '$2b$12$KjlHPD7WcAbXC1v6RZxXKOfFHGJJDGS4bxfKGPRnLBmWmqFMvS/4W', 'FX IAM Target',   true,  0, 0, now(), now()),
  (9000000000000000114, 'fx_iam_inactive', '$2b$12$KjlHPD7WcAbXC1v6RZxXKOfFHGJJDGS4bxfKGPRnLBmWmqFMvS/4W', 'FX IAM Inactive', false, 0, 0, now(), now());

-- ---- 用户角色（MANAGER + CLERK 无 scope；其余 3 用户无角色）----
INSERT INTO t_user_role (id, user_id, role, scope_type, scope_id, version, created_at, updated_at) VALUES
  (9000000000000000115, 9000000000000000110, 'MANAGER', NULL, NULL, 0, now(), now()),
  (9000000000000000116, 9000000000000000111, 'CLERK',   NULL, NULL, 0, now(), now());

-- ---- 货架（PRODUCTION + INSPECTION 各一，对应 add_role 添加 SHELF_ACCOUNT scope）----
INSERT INTO t_shelf (id, code, name, zone, is_active, display_order, version, created_at, updated_at) VALUES
  (9000000000000000117, 'FX-SH-A1', 'FX 货架 A1', 'PRODUCTION',  true, 0, 0, now(), now()),
  (9000000000000000118, 'FX-SH-B1', 'FX 货架 B1', 'INSPECTION', true, 0, 0, now(), now());
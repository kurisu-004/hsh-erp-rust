-- ============================================================================
--  guard_dn_in_use 域集成测试 fixture (PR13 Phase I, 2026-09-24)
--
--  加载入口：test-support::fixture::load_guard_dn_in_use_fixture(pool)
--  加载方式：sqlx::raw_sql(include_str!("../../fixtures/guard_dn_in_use.sql")).execute(pool).await
--
--  ## 设计原则
--  - 所有 ID 走常量 9_000_000_000_000_000_190+ 区段（physical 不相交）。
--  - bcrypt cost=12 哈希复用 iam.sql 字面值（同一明文 "changeme"）。
--  - 时间列：审计字段用 now()。
--
--  ## ID 段分配（190+）
--  MANAGER_USER_ID  190   fx_guard_dn_manager（MANAGER role，active）
--  MANAGER_ROLE_ID  191   baseline MANAGER role
--
--  ## 不预置 t_part / t_customer / t_part_batch / t_assembly
--  guard_dn_in_use_api.rs 保留 `mod helpers;`（依赖 tests/part/helpers.rs 的
--  `insert_l1` / `insert_l2` / `insert_part_with_status` / `insert_batch` /
--  `login_manager`），不在本 fixture 重复 —— 这些 helper 走 snowflake ID 而
--  非常量 ID（避免 uk_t_part_* 唯一约束撞车）。fixture 仅提供 baseline
--  MANAGER user / role。
-- ============================================================================

-- ---- MANAGER 用户 ----
INSERT INTO t_user (id, username, password_hash, full_name, is_active, refresh_token_version, version, created_at, updated_at)
VALUES
  (9000000000000000190, 'fx_guard_dn_manager', '$2b$12$KjlHPD7WcAbXC1v6RZxXKOfFHGJJDGS4bxfKGPRnLBmWmqFMvS/4W', 'FX Guard DN Manager', true, 0, 0, now(), now());

-- ---- MANAGER role ----
INSERT INTO t_user_role (id, user_id, role, scope_type, scope_id, version, created_at, updated_at)
VALUES
  (9000000000000000191, 9000000000000000190, 'MANAGER', NULL, NULL, 0, now(), now());
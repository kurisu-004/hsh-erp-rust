-- ============================================================================
--  customer 域集成测试 fixture (PR13 Phase I, 2026-09-24)
--
--  加载入口：test-support::fixture::load_customer_fixture(pool)
--  加载方式：sqlx::raw_sql(include_str!("../../fixtures/customer.sql")).execute(pool).await
--
--  ## 设计原则
--  - 所有 ID 走常量 9_000_000_000_000_000_140+ 区段（physical 不相交）。
--  - bcrypt cost=12 哈希复用 iam.sql 字面值（同一明文 "changeme"）。
--  - 时间列：审计字段用 now()。
--
--  ## ID 段分配（140+）
--  MANAGER_USER_ID  140   fx_customer_manager（MANAGER role，active）
--  MANAGER_ROLE_ID  141   baseline MANAGER role
--
--  ## 不预置 t_customer / t_part
--  customer_api.rs 的 2 个测试均在测试内创建 L1/L2 customer（避免 fixture 占用
--  serial_prefix='A' 与测试现场字面冲突）。baseline 仅提供 MANAGER user + role。
-- ============================================================================

-- ---- MANAGER 用户 ----
INSERT INTO t_user (id, username, password_hash, full_name, is_active, refresh_token_version, version, created_at, updated_at)
VALUES
  (9000000000000000140, 'fx_customer_manager', '$2b$12$KjlHPD7WcAbXC1v6RZxXKOfFHGJJDGS4bxfKGPRnLBmWmqFMvS/4W', 'FX Customer Manager', true, 0, 0, now(), now());

-- ---- MANAGER role ----
INSERT INTO t_user_role (id, user_id, role, scope_type, scope_id, version, created_at, updated_at)
VALUES
  (9000000000000000141, 9000000000000000140, 'MANAGER', NULL, NULL, 0, now(), now());
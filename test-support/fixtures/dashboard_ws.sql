-- ============================================================================
--  dashboard_ws 域集成测试 fixture (PR13 Phase I, 2026-09-24)
--
--  加载入口：test-support::fixture::load_dashboard_ws_fixture(pool)
--  加载方式：sqlx::raw_sql(include_str!("../../fixtures/dashboard_ws.sql")).execute(pool).await
--
--  ## 设计原则
--  - 所有 ID 走常量 9_000_000_000_000_000_150+ 区段（physical 不相交）。
--  - bcrypt cost=12 哈希复用 iam.sql 字面值（同一明文 "changeme"）。
--  - 时间列：审计字段用 now()。
--
--  ## ID 段分配（150+）
--  WS_USER_ID  150   fx_dashboard_ws_user（WS 真实 socket E2E 验签用）
--
--  ## 不预置 t_customer / t_part / t_part_batch / t_shelf
--  dashboard_ws_api.rs 的 7 个测试用例均走 service 层 snapshot 构建或 WS 握手，
--  多数用例现场插数据；fixture 仅提供 baseline user（WS 验签需 token），
--  避免 fixture 占用 t_shelf code 与测试现场字面冲突（snapshot 按 code 查找）。
-- ============================================================================

-- ---- WS 验签用户 ----
INSERT INTO t_user (id, username, password_hash, full_name, is_active, refresh_token_version, version, created_at, updated_at)
VALUES
  (9000000000000000150, 'fx_dashboard_ws_user', '$2b$12$KjlHPD7WcAbXC1v6RZxXKOfFHGJJDGS4bxfKGPRnLBmWmqFMvS/4W', 'FX Dashboard WS User', true, 0, 0, now(), now());
-- ============================================================================
--  _e2e 域集成测试 fixture (PR13 Phase I, 2026-09-24)
--
--  加载入口：test-support::fixture::load__e2e_fixture(pool)
--  加载方式：sqlx::raw_sql(include_str!("../../fixtures/_e2e.sql")).execute(pool).await
--
--  ## 设计原则
--  - 所有 ID 走常量 9_000_000_000_000_000_160+ 区段（physical 不相交）。
--
--  ## ID 段分配（160+）
--  PROCESS_ID  160   fx_e2e_process（hard-delete 引用检测：被 t_outsource_company_process 引用）
--
--  ## 不预置 user / customer / company
--  _e2e_api.rs 的 9 个测试用例均通过 `/_e2e/seed/*` 端点自建数据；
--  fixture 提供 1 条 baseline t_process（被 `hard_delete_outsource_company_referenced_returns_409`
--  测试 INSERT t_outsource_company_process 引用，触发 21205 守卫）。
-- ============================================================================

-- ---- baseline t_process（被 hard-delete 引用检测路径用）----
INSERT INTO t_process (id, code, name, category, sort_order, requires_approval, version, created_at, updated_at)
VALUES
  (9000000000000000160, 'FX_E2E_PROC', 'FX E2E Process', 'OUTSOURCE', 0, false, 0, now(), now());
-- ============================================================================
--  auto_complete 域集成测试 fixture (PR13 Phase I, 2026-09-24)
--
--  加载入口：test-support::fixture::load_auto_complete_fixture(pool)
--  加载方式：sqlx::raw_sql(include_str!("../../fixtures/auto_complete.sql")).execute(pool).await
--
--  ## 设计原则
--  - 所有 ID 走常量 9_000_000_000_000_000_180+ 区段（physical 不相交）。
--  - 时间列：审计字段用 now()。
--
--  ## ID 段分配（180+）
--  L1_CUSTOMER_ID  180   fx_auto_complete_l1（serial_prefix='F'）
--  L2_CUSTOMER_ID  181   fx_auto_complete_l2（parent=180，part 测试用）
--
--  ## 不预置 t_part / t_part_batch / t_part_event
--  auto_complete_api.rs 的 3 个测试用例均通过本地 `seed_delivered_batch`
--  helper 创建专属数据（不同 placed_days_ago），fixture 仅提供 baseline
--  L1/L2 customer，避免 customer fixture 内 prefix='F' 占用与测试现场冲突。
-- ============================================================================

-- ---- L1 客户 ----
INSERT INTO t_customer (id, name, parent_id, serial_prefix, version, created_at, updated_at)
VALUES
  (9000000000000000180, 'FX AutoComplete L1', NULL, 'F', 0, now(), now());

-- ---- L2 客户 ----
INSERT INTO t_customer (id, name, parent_id, serial_prefix, version, created_at, updated_at)
VALUES
  (9000000000000000181, 'FX AutoComplete L2', 9000000000000000180, NULL, 0, now(), now());
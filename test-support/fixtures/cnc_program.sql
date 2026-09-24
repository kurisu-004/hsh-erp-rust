-- ============================================================================
--  cnc_program 域集成测试 fixture (PR13 Phase I, 2026-09-24)
--
--  加载入口：test-support::fixture::load_cnc_program_fixture(pool)
--  加载方式：sqlx::raw_sql(include_str!("../../fixtures/cnc_program.sql")).execute(pool).await
--
--  ## 设计原则
--  - 所有 ID 走常量 9_000_000_000_000_000_170+ 区段（physical 不相交）。
--  - 时间列：审计字段用 now()。
--
--  ## ID 段分配（170+）
--  L1_CUSTOMER_ID  170   fx_cnc_l1（serial_prefix='C'，CNC 测试用）
--  L2_CUSTOMER_ID  171   fx_cnc_l2（parent=170，part 测试用）
--  PART_ID         172   fx_cnc_part（PENDING 状态，upload_cnc_pair 测试用）
--
--  ## 不预置 t_user
--  cnc_program_api.rs 是 service 层单测（不走 HTTP），用
--  `test_current_user(vec![Role::Manager])` 构造 CurrentUser，不依赖 DB user 行；
--  fixture 提供 baseline customer L1/L2 + part（PENDING 状态）。
-- ============================================================================

-- ---- L1 客户 ----
INSERT INTO t_customer (id, name, parent_id, serial_prefix, version, created_at, updated_at)
VALUES
  (9000000000000000170, 'FX CNC L1', NULL, 'C', 0, now(), now());

-- ---- L2 客户 ----
INSERT INTO t_customer (id, name, parent_id, serial_prefix, version, created_at, updated_at)
VALUES
  (9000000000000000171, 'FX CNC L2', 9000000000000000170, NULL, 0, now(), now());

-- ---- baseline PENDING part（upload_cnc_pair 测试用）----
INSERT INTO t_part (id, name, drawing_no, applicant_name, customer_id, request_date, planned_delivery_date, status, version, created_at, updated_at)
VALUES
  (9000000000000000172, 'FX CNC Part', 'D-FX-CNC', 'fx-cnc-tester', 9000000000000000171, CURRENT_DATE, CURRENT_DATE, 'PENDING', 0, now(), now());
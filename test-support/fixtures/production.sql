-- ============================================================================
--  production 域集成测试 fixture (PR13 Phase H, 2026-09-24)
--
--  加载入口：test-support::fixture::load_production_fixture(pool)
--  加载方式：内部先 load_part_fixture() 复用 part 域基线（ID 段 10-49），
--           再 raw_sql(include_str!("../../fixtures/production.sql")) 加载
--           本文件（ID 段 60+）。
--
--  ## 设计原则
--  - 所有 ID 走常量 9_000_000_000_000_000_060+ 区段（process_chain 1-9，
--    part 10-49，delivery 50-52，物理不相交）。
--  - 审计字段 created_at / updated_at 用 now()，业务日期按 fixtures 字面。
--  - 不引 trigger / 不调 hsh_erp_rust 函数 —— SQL 仅 INSERT 静态行。
--  - 无新用户：复用 part 域 MANAGER 用户 fx_part_manager + SHELF_ACCOUNT 用户
--    fx_part_shelf（密码 "changeme"，bcrypt cost=12 哈希已在 part.sql 内预生成）。
--  - 无 t_part / t_part_batch / t_part_process_chain / t_process_chain_step /
--    t_worker 行：各 sub-file 按需 sqlx::query 直插（状态 / code / quantity 由
--    测试现场控制；预置会污染「期望空库」测试断言）。
--
--  ## ID 段分配（60+）
--  PROCESS_A_ID               60   INHOUSE 工序 FX-NA
--  PROCESS_B_ID               61   INHOUSE 工序 FX-NB
--  WORK_TYPE_A_ID             62   工种 FX-WTA
--  WORK_TYPE_B_ID             63   工种 FX-WTB
--  WORK_TYPE_PROCESS_A_ID     64   work_type_A ↔ process_A 映射
--  WORK_TYPE_PROCESS_B_ID     65   work_type_B ↔ process_B 映射
--
--  ## 复用 part 域基线（不在本 SQL 内，由 load_production_fixture 内部先
--     load_part_fixture 加载）
--  CUSTOMER_L1_ID=10, CUSTOMER_L2_ID=11, PROCESS_ID=12, WORK_TYPE_ID=13,
--  INSPECTION_SHELF_ID=14, PRODUCTION_SHELF_ID=15, MANAGER_USER_ID=16,
--  INSPECTOR_USER_ID=17, CLERK_USER_ID=18, SHELF_ACCOUNT_USER_ID=19, ...
-- ============================================================================

-- ---- 工序（INHOUSE 类别，与原 tests/production/process.rs 字面对齐） ----
--  注：code 不能与 part.sql 的 FX-PROC-A 重复（id=12 已用），改用 FX-NA / FX-NB
INSERT INTO t_process (id, code, name, category, sort_order, requires_approval, version, created_at, updated_at)
VALUES
  (9000000000000000060, 'FX-NA', 'FX 工序 NA', 'INHOUSE', 0, false, 0, now(), now()),
  (9000000000000000061, 'FX-NB', 'FX 工序 NB', 'INHOUSE', 0, false, 0, now(), now());

-- ---- 工种（FX-WTA / FX-WTB） ----
--  注：code 不能与 part.sql 的 FX-WT-A 重复（id=13 已用）
INSERT INTO t_work_type (id, code, name, sort_order, max_held_batches, version, created_at, updated_at)
VALUES
  (9000000000000000062, 'FX-WTA', 'FX 工种 A', 0, NULL, 0, now(), now()),
  (9000000000000000063, 'FX-WTB', 'FX 工种 B', 0, NULL, 0, now(), now());

-- ---- 工种 ↔ 工序 映射（work_type_A ↔ process_A；work_type_B ↔ process_B） ----
INSERT INTO t_work_type_process (id, work_type_id, process_id, sort_order, version, created_at, updated_at)
VALUES
  (9000000000000000064, 9000000000000000062, 9000000000000000060, 0, 0, now(), now()),
  (9000000000000000065, 9000000000000000063, 9000000000000000061, 0, 0, now(), now());
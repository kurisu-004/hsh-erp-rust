-- ============================================================================
--  shelf 域集成测试 fixture (PR13 Phase H, 2026-09-24)
--
--  加载入口：test-support::fixture::load_shelf_fixture(pool)
--  加载方式：内部先 load_part_fixture() 复用 part 域基线（ID 段 10-49），
--           再 raw_sql(include_str!("../../fixtures/shelf.sql")) 加载
--           本文件（ID 段 80+）。
--
--  ## 设计原则
--  - 所有 ID 走常量 9_000_000_000_000_000_080+ 区段（process_chain 1-9，
--    part 10-49，delivery 50-52，production 60-69，assembly 70 占位），
--    物理不相交。
--  - 审计字段 created_at / updated_at 用 now()，业务日期按 fixtures 字面。
--  - 不引 trigger / 不调 hsh_erp_rust 函数 —— SQL 仅 INSERT 静态行。
--  - 无新用户：复用 part 域 MANAGER 用户 fx_part_manager（密码 "changeme"，
--    bcrypt cost=12 哈希已在 part.sql 内预生成）。
--
--  ## 不预置 t_customer / t_part / t_part_batch / t_worker
--  shelf 域测试大多走 service 层 / 直插 t_part_batch 路径：
--  - api.rs 的 `insert_part_held_by_shelf` 直插 t_part + t_part_batch（绕开
--    part CRUD，因 part 域自身不在 Task 3 范围内）
--  - api.rs 的 `insert_test_process` 直插 INHOUSE 工序（mapping 端点要校验
--    process_id 存在；测试按需造不同 code / name）
--  - deactivate.rs 的 `insert_l2_customer` + `insert_part` + `insert_batch` +
--    `insert_worker_min` 直插 L1(prefix='F')/L2/part/batch/worker（绕开业务
--    API，按需造不同 L1 prefix / 不同 customer_name / 不同 status）
--  fixture 不预置这些行，预置会污染「期望空库」断言，且 prefix 唯一约束
--  `uq_t_customer_root_prefix` 也要求 deactivate 测试自建 L1。
--
--  ## ID 段分配（80+）
--  SHELF_PROCESS_ID         80   INHOUSE 工序 FX-SHP（shelf 映射目标 process）
--  SHELF_ID                 81   INSPECTION 货架 FX-SH-NEW1
--  SHELF_PROCESS_MAPPING_ID 82   FX-SH-NEW1 ↔ FX-SHP 映射
--
--  ## 复用 part 域基线（不在本 SQL 内，由 load_shelf_fixture 内部先
--     load_part_fixture 加载）
--  CUSTOMER_L1_ID=10, CUSTOMER_L2_ID=11, PROCESS_ID=12, WORK_TYPE_ID=13,
--  INSPECTION_SHELF_ID=14, PRODUCTION_SHELF_ID=15, MANAGER_USER_ID=16, ...
-- ============================================================================

-- ---- 工序（INHOUSE 类别 FX-SHP；不与 part.sql FX-PROC-A / production.sql FX-NA/FB 重复）----
INSERT INTO t_process (id, code, name, category, sort_order, requires_approval, version, created_at, updated_at)
VALUES
  (9000000000000000080, 'FX-SHP', 'FX 货架工序', 'INHOUSE', 0, false, 0, now(), now());

-- ---- 货架（INSPECTION 类别 FX-SH-NEW1；不与 part.sql FX-SH-INSP / FX-SH-PROD 重复）----
INSERT INTO t_shelf (id, code, name, zone, is_active, display_order, version, created_at, updated_at)
VALUES
  (9000000000000000081, 'FX-SH-NEW1', 'FX 新货架1', 'INSPECTION', true, 0, 0, now(), now());

-- ---- 货架 ↔ 工序 映射（FX-SH-NEW1 → FX-SHP）----
INSERT INTO t_shelf_process (id, shelf_id, process_id, sort_order, version, created_at, updated_at)
VALUES
  (9000000000000000082, 9000000000000000081, 9000000000000000080, 0, 0, now(), now());
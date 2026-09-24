-- ============================================================================
--  assembly 域集成测试 fixture (PR13 Phase H, 2026-09-24)
--
--  加载入口：test-support::fixture::load_assembly_fixture(pool)
--  加载方式：内部先 load_part_fixture() 复用 part 域基线（ID 段 10-49），
--           再 raw_sql(include_str!("../../fixtures/assembly.sql")) 加载
--           本文件（ID 段 70+）。
--
--  ## 设计原则
--  - 所有 ID 走常量 9_000_000_000_000_000_070+ 区段（process_chain 1-9，
--    part 10-49，delivery 50-52，production 60-69，物理不相交）。
--  - 审计字段 created_at / updated_at 用 now()，业务日期按 fixtures 字面。
--  - 不引 trigger / 不调 hsh_erp_rust 函数 —— SQL 仅 INSERT 静态行。
--  - 无新用户：复用 part 域 MANAGER 用户 fx_part_manager（密码 "changeme"，
--    bcrypt cost=12 哈希已在 part.sql 内预生成）。
--  - 不预置 t_assembly：tests/assembly/{api,files}.rs 主要走 service 层
--    `AssemblyService::create_assembly` 直接调用（不开 HTTP、不经 JWT、不经
--    Redis），由各测试按需造不同 drawing_no / customer_id / quantity / 带不带
--    PDF 等组合。预置 PENDING 装配体会让「期望空库」断言失败。
--  - 不预置 t_part / t_part_batch：与 assembly 同理，状态机不允许从
--    IN_PROCESS / COMPLETED 等回退 PENDING，预置会污染状态断言。
--
--  ## ID 段分配（70+）
--  CUSTOMER_L1_ID       70   L1 客户（带 serial_prefix='F'，canonical）
--  CUSTOMER_L2_ID       71   L2 子客户（挂在 L1 下，prefix=NULL）
--
--  ## 复用 part 域基线（不在本 SQL 内，由 load_assembly_fixture 内部先
--     load_part_fixture 加载）
--  CUSTOMER_L1_ID=10, CUSTOMER_L2_ID=11, MANAGER_USER_ID=16, ...
--
--  ## t_serial_counter 行
--  单条 (prefix='F', counter=0) —— 18/20 assembly 测试使用 'F' prefix；
--  余下 test_with_x_prefix (list_with_filters_and_l1_expansion) 用 'X'，
--  由该测试本地 `insert_serial_counter(pool, "X", 0)` 自建（ON CONFLICT
--  DO UPDATE 复用）。
-- ============================================================================

-- ---- L1 客户 + L2 子客户（canonical 'F' prefix 配对）----
INSERT INTO t_customer (id, name, parent_id, serial_prefix, version, created_at, updated_at)
VALUES
  (9000000000000000070, 'FX ASM 客户 L1', NULL,                                'F', 0, now(), now()),
  (9000000000000000071, 'FX ASM 客户 L2', 9000000000000000070,                  NULL, 0, now(), now());

-- ---- 序列号计数器（canonical 'F' prefix）----
INSERT INTO t_serial_counter (prefix, counter, version, created_at, updated_at)
VALUES ('F', 0, 0, now(), now());
-- ============================================================================
--  delivery 域集成测试 fixture (PR13 Phase G, 2026-09-23)
--
--  加载入口：test-support::fixture::load_delivery_fixture(pool)
--  加载方式：内部先 load_part_fixture() 复用 part 域基线（ID 段 10-49），
--           再 raw_sql(include_str!("../../fixtures/delivery.sql")) 加载
--           本文件（ID 段 50+）。
--
--  ## 设计原则
--  - 所有 ID 走常量 9_000_000_000_000_000_050+ 区段（part 域占 10-49，
--    process_chain 域占 1-9，物理不相交）。
--  - 不引 trigger / 不调 hsh_erp_rust 函数 —— SQL 仅 INSERT 静态行。
--  - 审计字段 created_at / updated_at 用 now()，业务日期（request_date /
--    planned_delivery_date）按 fixtures 字面（current_date / current_date+30d）。
--  - 无新用户：复用 part 域 MANAGER 用户 fx_part_manager（密码 "changeme"，
--    bcrypt cost=12 哈希已在 part.sql 内预生成）。
--
--  ## ID 段分配（50+）
--  ASSEMBLY_ID                  50   t_assembly（serial_no='FX-ASM-001'，PENDING）
--  DELIVERY_GROUP_ID            51   t_delivery_group（name='FX-Group-1'，L1=10）
--  DELIVERY_GROUP_MEMBER_ID     52   t_delivery_group_member（L2=11 入组）
--
--  ## 不预置 t_delivery_note
--  多数 delivery 域测试用「customer_id 限定 + status 过滤」查单，fixture 预置
--  DRAFT 草稿会让 `total` 计数包含本行，破坏 `list_with_filters_status_and_pagination`
--  等「期望仅 3 条 / N 条」断言。各 sub-file 按需通过 POST /delivery-notes
-- 走业务路径建草稿（PR-C 末统一迁 test-support）。
--
--  ## 复用 part 域基线（不在本 SQL 内，由 load_delivery_fixture 内部先
--     load_part_fixture 加载）
--  CUSTOMER_L1_ID=10, CUSTOMER_L2_ID=11, MANAGER_USER_ID=16, ...
-- ============================================================================

-- ---- 装配件（serial_no 全局活跃唯一 uk_t_assembly_serial_no）----
INSERT INTO t_assembly (id, drawing_no, name, customer_id, request_date, planned_delivery_date, status, serial_no, quantity, version, created_at, updated_at)
VALUES (9000000000000000050, 'FX-D-A001', 'FX 装配件', 9000000000000000010, current_date, current_date + INTERVAL '30 days', 'PENDING', 'FX-ASM-001', 1, 0, now(), now());

-- ---- 送货分组（uq_t_delivery_group_name_active: customer_id+name）----
INSERT INTO t_delivery_group (id, customer_id, name, version, created_at, updated_at)
VALUES (9000000000000000051, 9000000000000000010, 'FX-Group-1', 0, now(), now());

-- ---- 分组成员（uq_t_delivery_group_member_customer_active: L2 全局活跃唯一）----
INSERT INTO t_delivery_group_member (id, group_id, customer_id, created_at)
VALUES (9000000000000000052, 9000000000000000051, 9000000000000000011, now());

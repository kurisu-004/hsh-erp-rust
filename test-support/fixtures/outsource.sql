-- ============================================================================
--  outsource 域集成测试 fixture (PR13 Phase H, 2026-09-24)
--
--  加载入口：test-support::fixture::load_outsource_fixture(pool)
--  加载方式：内部先 load_part_fixture() 复用 part 域基线（ID 段 10-49），
--           再 raw_sql(include_str!("../../fixtures/outsource.sql")) 加载
--           本文件（ID 段 100+）。
--
--  ## 设计原则
--  - 所有 ID 走常量 9_000_000_000_000_000_100+ 区段（process_chain 1-9，
--    part 10-49，delivery 50-52，production 60-69，assembly 70+ / shelf 80+ /
--    statistics 90+，物理不相交）。
--  - 审计字段 created_at / updated_at 用 now()。
--  - 不引 trigger / 不调 hsh_erp_rust 函数 —— SQL 仅 INSERT 静态行。
--  - bcrypt cost=12 哈希复用 part.sql 字面值（同一明文 "changeme"，
--    cost=12 → 同一哈希输出）。
--
--  ## ID 段分配（100+）
--  OUTSOURCE_PROCESS_ID     100   OUTSOURCE 类别工序 FX-OPROC-A
--  OUTSOURCE_COMPANY_ID     101   t_outsource_company FX-OC-001（active baseline）
--  CLERK_USER_ID            103   fx_outsource_clerk 用户
--  CLERK_ROLE_ID            104   CLERK role（无 scope）
--
--  ## 不预置 t_outsource_quote / t_outsource_shipment
--  - 报价单需要 part_id + company_id + process_id 三元组；fixture 不预置
--    t_part（part fixture 也不预置，避免 PENDING/IN_PROCESS 污染「期望空库」
--    断言），故不预置 quote。
--  - shipment 在 send_receive.rs 每个测试现场按 part_id / batch_id / quote_id
--    构造；预置会被绝大多数测试的 raw SQL UPDATE 干扰。
--
--  ## 不预置 part / batch / chain / step
--  outsource 域 22 个场景（company 9 + quote 7 + send_receive 6）每个测试都
--  要按需造不同 customer prefix / 不同 part status / 不同 chain step 的组合；
--  状态机不允许从 OUTSOURCE / IN_PROCESS 回退 PENDING，预置会污染 list /
--  count 等「期望空库」断言。各 sub-file 走本地 helper（insert_l1_customer /
--  insert_part / insert_batch / create_chain_for_part / create_step）直插。
--
--  ## 复用 part 域基线（不在本 SQL 内，由 load_outsource_fixture 内部先
--     load_part_fixture 加载）
--  CUSTOMER_L1_ID=10, CUSTOMER_L2_ID=11, PROCESS_ID=12 (INHOUSE FX-PROC-A),
--  WORK_TYPE_ID=13, INSPECTION_SHELF_ID=14, PRODUCTION_SHELF_ID=15,
--  MANAGER_USER_ID=16 (fx_part_manager), INSPECTOR_USER_ID=17,
--  CLERK_USER_ID=18 (fx_part_clerk), SHELF_ACCOUNT_USER_ID=19, ...
-- ============================================================================

-- ---- OUTSOURCE 类别工序（FX-OPROC-A）----
-- 注：code 不能与 part.sql 的 FX-PROC-A 重复（id=12，category=INHOUSE），
--    也不能与 production.sql 的 FX-NA/FX-NB 重复（id=60/61，INHOUSE）；
--    category 必为 'OUTSOURCE'（migration 003 的 ck_t_process_category 约束）。
INSERT INTO t_process (id, code, name, category, sort_order, requires_approval, version, created_at, updated_at)
VALUES
  (9000000000000000100, 'FX-OPROC-A', 'FX 外协工序 A', 'OUTSOURCE', 0, true, 0, now(), now());

-- ---- 外协公司（FX-OC-001，active baseline）----
-- name 'FX-OC-001' 与各 sub-file 测试自建的不同 name（'Acme 加工厂' /
-- 'SameName' / 'SendCo' / 'RecvCo' 等）不撞；uk_t_outsource_company_name
-- 仅约束 active 同名唯一。
INSERT INTO t_outsource_company (id, name, is_active, version, created_at, updated_at)
VALUES
  (9000000000000000101, 'FX-OC-001', true, 0, now(), now());

-- ---- 用户（CLERK，密码明文 "changeme"，bcrypt cost=12）----
-- bcrypt 哈希与 part.sql 字面值相同（同一明文 + cost → 同输出）。
-- 仅供 quote.rs 的 CLERK 守卫测试（approve_quote_clerk_forbidden_40300）
-- 走 login_token + 测试专用角色；其它 sub-file 用 fx_part_manager 登录。
INSERT INTO t_user (id, username, password_hash, full_name, is_active, refresh_token_version, version, created_at, updated_at)
VALUES
  (9000000000000000103, 'fx_outsource_clerk', '$2b$12$KjlHPD7WcAbXC1v6RZxXKOfFHGJJDGS4bxfKGPRnLBmWmqFMvS/4W', 'FX Outsource Clerk', true, 0, 0, now(), now());

-- ---- 用户角色（CLERK 无 scope）----
INSERT INTO t_user_role (id, user_id, role, scope_type, scope_id, version, created_at, updated_at)
VALUES
  (9000000000000000104, 9000000000000000103, 'CLERK', NULL, NULL, 0, now(), now());

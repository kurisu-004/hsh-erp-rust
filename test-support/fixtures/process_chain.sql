-- ============================================================================
--  process_chain 域集成测试 fixture (PR13 Phase F, 2026-09-23)
--
--  加载入口：test-support::fixture::load_process_chain_fixture(pool)
--  加载方式：sqlx::query(sqlx::AssertSqlSafe(content)).execute(pool).await
--           (整体作为一条 multi-statement SQL 执行)
--
--  ## 设计原则
--  - 所有 ID 走常量 9_000_000_000_000_000_001+ 区段，与运行时雪花 ID
--    (epoch=2020-01-01, instance<=1023, 12 bit seq) 物理不相交。
--  - 预生成 bcrypt 哈希嵌入 SQL，避免每测试现场 hash (~250ms×N)。
--    哈希明文 "changeme"，cost=12（与 hsh_erp_rust::auth::password::hash 同形）。
--  - 时间列：created_at/updated_at 用 now()，request_date/planned_delivery_date
--    用 now()/now()+30 days（与原 tests/production/process_chain.rs 字面一致）。
--  - 不引 trigger / 不调 hsh_erp_rust 函数 —— SQL 仅 INSERT 静态行。
--
--  ## 与原 tests/production/process_chain.rs 的对应
--  - 1 个 customer (L1 prefix='P')          → ProcessChainFixture::CUSTOMER_ID
--  - 2 个 process (PROC-A / PROC-B)         → PROC_A / PROC_B
--  - 1 个 PENDING part                      → PART_PENDING
--  - 1 个 IN_PROCESS part                   → PART_IN_PROCESS
--  - 1 个 MANAGER user + MANAGER role       → MANAGER_USERNAME
--  - 1 个 CLERK user + CLERK role           → CLERK_USERNAME
--  - bcrypt cost=12，hash for "changeme"
-- ============================================================================

-- ---- L1 客户 ---------------------------------------------------------------
INSERT INTO t_customer (id, name, parent_id, serial_prefix, version, created_at, updated_at)
VALUES (9000000000000000001, 'PCH 测试客户', NULL, 'P', 0, now(), now());

-- ---- 工序（INHOUSE 类别） ---------------------------------------------------
INSERT INTO t_process (id, code, name, category, sort_order, requires_approval, version, created_at, updated_at)
VALUES (9000000000000000002, 'PROC-A', '工序A', 'INHOUSE', 0, false, 0, now(), now());

INSERT INTO t_process (id, code, name, category, sort_order, requires_approval, version, created_at, updated_at)
VALUES (9000000000000000003, 'PROC-B', '工序B', 'INHOUSE', 0, false, 0, now(), now());

-- ---- 用户（密码明文 "changeme"，cost=12 bcrypt） ---------------------------
INSERT INTO t_user (id, username, password_hash, full_name, is_active, refresh_token_version, version, created_at, updated_at)
VALUES
    (9000000000000000006, 'fx_manager', '$2b$12$KjlHPD7WcAbXC1v6RZxXKOfFHGJJDGS4bxfKGPRnLBmWmqFMvS/4W', '经理 fx_manager', true, 0, 0, now(), now()),
    (9000000000000000007, 'fx_clerk', '$2b$12$KjlHPD7WcAbXC1v6RZxXKOfFHGJJDGS4bxfKGPRnLBmWmqFMvS/4W', '文员 fx_clerk', true, 0, 0, now(), now());

-- ---- 用户角色（manager / clerk） -------------------------------------------
INSERT INTO t_user_role (id, user_id, role, scope_type, scope_id, version, created_at, updated_at)
VALUES
    (9000000000000000008, 9000000000000000006, 'MANAGER', NULL, NULL, 0, now(), now()),
    (9000000000000000009, 9000000000000000007, 'CLERK',   NULL, NULL, 0, now(), now());

-- ---- 零件：1 个 PENDING + 1 个 IN_PROCESS（IN_PROCESS 触发 20705 守卫） -----
INSERT INTO t_part (id, serial_no, name, drawing_no, applicant_name, request_date, planned_delivery_date, status, is_urgent, customer_id, quantity, unit_price, total_price, version, created_at, updated_at)
VALUES
    (9000000000000000004, 'P-PEND', 'PENDING 件', 'D-PEND', 'PENDING 件', now(), now() + INTERVAL '30 days', 'PENDING',     false, 9000000000000000001, 1, 0, 0, 0, now(), now()),
    (9000000000000000005, 'P-INP',  'IN_PROCESS 件', 'D-INP',  'IN_PROCESS 件', now(), now() + INTERVAL '30 days', 'IN_PROCESS', false, 9000000000000000001, 1, 0, 0, 0, now(), now());
-- ============================================================================
--  process_design 域集成测试 fixture (2026-10-05 新增)
--
--  加载入口：test-support::fixture::load_process_design_fixture(pool)
--  加载方式：sqlx::raw_sql(sql).execute(pool).await（整体作为一条 multi-statement
--           SQL 执行）
--
--  ## 设计原则（与 process_chain.sql / production.sql 同款）
--  - 所有 ID 走常量 9_000_000_000_000_000_001+ 区段，与运行时雪花 ID
--    (epoch=2020-01-01, instance<=1023, 12 bit seq) 物理不相交。
--  - 预生成 bcrypt 哈希嵌入 SQL，避免每测试现场 hash (~250ms×N)。
--    哈希明文 "changeme"，cost=12（与 hsh_erp_rust::auth::password::hash 同形，
--    与其余 fixture 逐字同一个哈希字面值）。
--  - 时间列：created_at/updated_at 用 now()，request_date/planned_delivery_date
--    用 now() / now()+30 days。
--  - 不引 trigger / 不调 hsh_erp_rust 函数 —— SQL 仅 INSERT 静态行。
--
--  ## 预置 5 个角色账号（角色守卫场景需要逐个登录）
--  MANAGER / CLERK / INSPECTOR / CNC_PROGRAMMER 四个是本端点白名单角色；
--  SHELF_ACCOUNT 是**越权**角色（scope 到检验架，合法登录但被 service 守卫拒）。
--  故 SHELF_ACCOUNT 行必须带 scope_type='shelf' + scope_id（否则登录阶段就会被
--  拦掉，测不到 service 层的 40300）。
--
--  ## 预置零件行清单（供各场景自造差异之外的基线）
--  PART_BASE_*   —— PENDING 且 serial_no 有值的基线行（正常成员）
--  PART_NO_SERIAL —— PENDING 但 serial_no 为 NULL（手工工单，验 NULLS LAST）
--  PART_CHAINED  —— PENDING 且已挂工艺链（验 process_chain_id 非 null）
--  PART_CHILD    —— PENDING 且 assembly_id 指向 PART_ASSEMBLY 的**子件**
--                    （★ 本域核心回归：子件必须可见，且行上 assembly_id 有值）
--  PART_ASSEMBLY —— PENDING 装配件主表行（子件的父）
--  PART_IN_PROCESS / PART_PROGRAMMING / PART_COMPLETED / PART_CANCELLED
--                  —— 四种非 PENDING 状态行（验状态闸门全部排除）
--  PART_SOFT_DELETED —— PENDING 但 deleted_at 非空（验软删闸门）
--
--  ⚠️ 零件行的 name 刻意用 ASCII（如 'BASE 件' 的英文标记），避免测试断言依赖
--  容器 locale 下的中文 collation；只有排序键 serial_no 是纯 ASCII。
-- ============================================================================

-- ---- L1 客户（t_part.customer_id NOT NULL 指向它） ----------------------------
-- ⚠️ serial_prefix 列宽是 varchar(1)（全局活跃根客户 prefix 唯一索引
-- uq_t_customer_root_prefix），故用单字符 'P'，不能写 'PD'。
INSERT INTO t_customer (id, name, parent_id, serial_prefix, version, created_at, updated_at)
VALUES (9000000000000000001, 'PD 测试客户', NULL, 'P', 0, now(), now());

-- ---- 货架（供 SHELF_ACCOUNT 的 scope_id 使用；仅登录期 scope 校验需要） -------
INSERT INTO t_shelf (id, code, name, zone, is_active, display_order, version, created_at, updated_at)
VALUES (9000000000000000002, 'PD-SH-INSP', 'PD 检验架', 'INSPECTION', true, 0, 0, now(), now());

-- ---- 工艺链（PART_CHAINED 挂它，验 process_chain_id 非 null） ----------------
INSERT INTO t_part_process_chain (id, name, version, created_at, created_by, updated_at, updated_by)
VALUES (9000000000000000003, 'PD 工艺链', 0, now(), 0, now(), 0);

-- ---- 用户（5 个角色各一个；密码明文 "changeme"，cost=12 bcrypt） ---------------
INSERT INTO t_user (id, username, password_hash, full_name, is_active, refresh_token_version, version, created_at, updated_at)
VALUES
    (9000000000000000004, 'fx_pd_manager',      '$2b$12$KjlHPD7WcAbXC1v6RZxXKOfFHGJJDGS4bxfKGPRnLBmWmqFMvS/4W', 'PD Manager',  true, 0, 0, now(), now()),
    (9000000000000000005, 'fx_pd_clerk',        '$2b$12$KjlHPD7WcAbXC1v6RZxXKOfFHGJJDGS4bxfKGPRnLBmWmqFMvS/4W', 'PD Clerk',    true, 0, 0, now(), now()),
    (9000000000000000006, 'fx_pd_inspector',    '$2b$12$KjlHPD7WcAbXC1v6RZxXKOfFHGJJDGS4bxfKGPRnLBmWmqFMvS/4W', 'PD Inspector',true, 0, 0, now(), now()),
    (9000000000000000007, 'fx_pd_cnc',          '$2b$12$KjlHPD7WcAbXC1v6RZxXKOfFHGJJDGS4bxfKGPRnLBmWmqFMvS/4W', 'PD CNC',      true, 0, 0, now(), now()),
    (9000000000000000008, 'fx_pd_shelf',        '$2b$12$KjlHPD7WcAbXC1v6RZxXKOfFHGJJDGS4bxfKGPRnLBmWmqFMvS/4W', 'PD Shelf',    true, 0, 0, now(), now());

-- ---- 用户角色（4 白名单 + 1 越权；SHELF_ACCOUNT scope 到检验架） --------------
INSERT INTO t_user_role (id, user_id, role, scope_type, scope_id, version, created_at, updated_at)
VALUES
    (9000000000000000009, 9000000000000000004, 'MANAGER',       NULL,    NULL,                0, now(), now()),
    (9000000000000000010, 9000000000000000005, 'CLERK',         NULL,    NULL,                0, now(), now()),
    (9000000000000000011, 9000000000000000006, 'INSPECTOR',     NULL,    NULL,                0, now(), now()),
    (9000000000000000012, 9000000000000000007, 'CNC_PROGRAMMER', NULL,   NULL,                0, now(), now()),
    (9000000000000000013, 9000000000000000008, 'SHELF_ACCOUNT', 'shelf', 9000000000000000002, 0, now(), now());

-- ---- 装配件主表行（PART_CHILD 的父） ------------------------------------------
INSERT INTO t_assembly (id, drawing_no, name, applicant_name, customer_id, request_date, planned_delivery_date, is_urgent, status, serial_no, version, created_at, updated_at)
VALUES (9000000000000000014, 'D-PD-ASM', 'PD 装配件', 'PD 装配件', 9000000000000000001, now(), now() + INTERVAL '30 days', false, 'PENDING', 'PD-ASM', 0, now(), now());

-- ---- 零件行（t_part） ----------------------------------------------------------
-- 说明：
--  - serial_no 列宽 varchar(15)，fixture 值均 <= 15 字符；
--  - PENDING 且 deleted_at IS NULL 的行才是本端点的正常成员；
--  - PART_CHILD 的 assembly_id 指向 PART_ASSEMBLY（★ 核心回归行）；
--  - 四种非 PENDING 状态 + 一行软删，专门用于闸门场景。
INSERT INTO t_part (id, serial_no, name, drawing_no, applicant_name, request_date, planned_delivery_date, status, is_urgent, customer_id, quantity, unit_price, total_price, assembly_id, process_chain_id, version, created_at, updated_at, deleted_at)
VALUES
    -- 基线：PENDING + 有序列号
    (9000000000000000015, 'PD-F1001-01', 'PD baseline-01', 'D-PD-01', 'PD', now(), now() + INTERVAL '30 days', 'PENDING', false, 9000000000000000001, 1, 0, 0, NULL, NULL, 0, now(), now(), NULL),
    -- 基线：PENDING + 有序列号（第二个，用于分页 / 排序场景凑行）
    (9000000000000000016, 'PD-F1001-02', 'PD baseline-02', 'D-PD-02', 'PD', now(), now() + INTERVAL '30 days', 'PENDING', false, 9000000000000000001, 1, 0, 0, NULL, NULL, 0, now(), now(), NULL),
    -- PENDING + serial_no 为 NULL（手工工单，验 NULLS LAST）
    (9000000000000000017, NULL,          'PD no-serial',   'D-PD-03', 'PD', now(), now() + INTERVAL '30 days', 'PENDING', false, 9000000000000000001, 1, 0, 0, NULL, NULL, 0, now(), now(), NULL),
    -- PENDING + 已挂工艺链（验 process_chain_id 非 null）
    (9000000000000000018, 'PD-F1001-03', 'PD chained',     'D-PD-04', 'PD', now(), now() + INTERVAL '30 days', 'PENDING', false, 9000000000000000001, 1, 0, 0, NULL, 9000000000000000003, 0, now(), now(), NULL),
    -- ★ 核心回归：装配件子件（PENDING + assembly_id 指向 PART_ASSEMBLY）
    (9000000000000000019, 'PD-F1001-04', 'PD child-part',  'D-PD-05', 'PD', now(), now() + INTERVAL '30 days', 'PENDING', false, 9000000000000000001, 1, 0, 0, 9000000000000000014, NULL, 0, now(), now(), NULL),
    -- 装配件主表行本身（PENDING，无 assembly_id —— 它是「独立行」形态的装配件）
    (9000000000000000020, 'PD-ASM',      'PD assembly',    'D-PD-06', 'PD', now(), now() + INTERVAL '30 days', 'PENDING', false, 9000000000000000001, 1, 0, 0, NULL, NULL, 0, now(), now(), NULL),
    -- 状态闸门反例：IN_PROCESS
    (9000000000000000021, 'PD-F2001-01', 'PD in-process',  'D-PD-07', 'PD', now(), now() + INTERVAL '30 days', 'IN_PROCESS', false, 9000000000000000001, 1, 0, 0, NULL, NULL, 0, now(), now(), NULL),
    -- 状态闸门反例：PROGRAMMING
    (9000000000000000022, 'PD-F2001-02', 'PD programming', 'D-PD-08', 'PD', now(), now() + INTERVAL '30 days', 'PROGRAMMING', false, 9000000000000000001, 1, 0, 0, NULL, NULL, 0, now(), now(), NULL),
    -- 状态闸门反例：COMPLETED
    (9000000000000000023, 'PD-F2001-03', 'PD completed',   'D-PD-09', 'PD', now(), now() + INTERVAL '30 days', 'COMPLETED', false, 9000000000000000001, 1, 0, 0, NULL, NULL, 0, now(), now(), NULL),
    -- 状态闸门反例：CANCELLED
    (9000000000000000024, 'PD-F2001-04', 'PD cancelled',   'D-PD-10', 'PD', now(), now() + INTERVAL '30 days', 'CANCELLED', false, 9000000000000000001, 1, 0, 0, NULL, NULL, 0, now(), now(), NULL),
    -- 软删闸门反例：PENDING 但 deleted_at 非空（★ deleted_at = now()）
    (9000000000000000025, 'PD-F3001-01', 'PD soft-deleted','D-PD-11', 'PD', now(), now() + INTERVAL '30 days', 'PENDING', false, 9000000000000000001, 1, 0, 0, NULL, NULL, 0, now(), now(), now());

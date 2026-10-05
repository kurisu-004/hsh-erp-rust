-- ============================================================================
--  inspection 域集成测试 fixture (2026-10-05 新增)
--
--  加载入口：test-support::fixture::load_inspection_fixture(pool)
--  加载方式：sqlx::raw_sql(sql).execute(pool).await（整体作为一条 multi-statement
--           SQL 执行）
--
--  服务对象：prod::inspection 子模块扫码端点
--           GET /api/v2/prod/inspection/scan/{serial_no}
--           （装配件 → 全部子件 → 全部批次 三层树）
--
--  ## 设计原则（与 process_design.sql / process_chain.sql 同款）
--  - 所有 ID 走常量 9_000_000_000_000_000_201+ 区段（与运行时雪花 ID 物理不相交，
--    也与 process_design 的 001~025 区段不撞）。
--  - 预生成 bcrypt 哈希嵌入 SQL，避免每测试现场 hash (~250ms×N)。
--    哈希明文 "changeme"，cost=12（与其余 fixture 逐字同一个哈希字面值）。
--  - 时间列：审计字段用 now()，业务日期按 fixtures 字面。
--  - 不引 trigger / 不调 hsh_erp_rust 函数 —— SQL 仅 INSERT 静态行。
--
--  ## 预置 5 个角色账号（角色守卫场景需要逐个登录）
--  MANAGER / INSPECTOR 两个是本端点白名单角色；
--  CLERK / CNC_PROGRAMMER / SHELF_ACCOUNT 三个是**越权**角色（合法登录但被
--  service 守卫拒）。故 SHELF_ACCOUNT 行必须带 scope_type='shelf' + scope_id
--  （否则登录阶段就被拦掉，测不到 service 层的 40300）。
--
--  ## 预置零件行清单
--  PART_STANDALONE —— 独立件（assembly_id IS NULL），挂 8 个批次覆盖
--                     **全部**批次状态（扫码树不按状态过滤）
--  PART_CHILD_1     —— 装配件子件 #1（有 INSPECTION 批次，被扫中的就是它）
--  PART_CHILD_2     —— 装配件子件 #2（**无批次**，验「children 仍是全部子件」）
--  PART_CHILD_3     —— 装配件子件 #3（**无批次**，验上一条不是巧合）
--  PART_CHILD_DELETED —— 装配件子件 #4 但 deleted_at 非空（软删闸门）
--
--  ## OCC 锚口径（★ 前端最容易踩的一处）
--  t_part.version 与 t_part_batch.version 是**两列**：前者是整个工单的聚合投影，
--  后者才是 to-ship / to-process / to-inspection 的 OCC 锚。fixture 刻意让
--  PART_STANDALONE.version = 1 而 BATCH_INSPECTION.version = 3，两者不等 ——
--  集成测试据此证明扫码树没把零件版本当批次版本透出。
--
--  ⚠️ 零件行的 name 用 ASCII，避免测试断言依赖容器 locale 下的中文 collation；
--     排序键 serial_no 全部纯 ASCII。
-- ============================================================================

-- ---- 客户（L1 根客户 + L2 子客户；子件挂 L2） -------------------------------
-- ⚠️ serial_prefix 列宽是 varchar(1)（全局活跃根客户 prefix 唯一索引
-- uq_t_customer_root_prefix），故用单字符 'S'，不能写 'SI'。
INSERT INTO t_customer (id, name, parent_id, serial_prefix, version, created_at, updated_at)
VALUES
    (9000000000000000201, 'SI L1 customer', NULL,                    'S', 0, now(), now()),
    (9000000000000000202, 'SI L2 customer', 9000000000000000201, NULL, 0, now(), now());

-- ---- 货架（品检架 + 生产架；也是 SHELF_ACCOUNT 的 scope 锚点） ---------------
INSERT INTO t_shelf (id, code, name, zone, is_active, display_order, version, created_at, updated_at)
VALUES
    (9000000000000000203, 'SI-SH-INSP', 'SI inspection shelf', 'INSPECTION', true, 0, 0, now(), now()),
    (9000000000000000204, 'SI-SH-PROD', 'SI production shelf', 'PRODUCTION', true, 1, 0, now(), now());

-- ---- 工序（INSPECTION 批次 current_process_id 恒 NULL；IN_PROCESS 才取到名） ---
INSERT INTO t_process (id, code, name, category, sort_order, requires_approval, version, created_at, updated_at)
VALUES
    (9000000000000000205, 'SI-PROC-A', 'SI process A', 'INHOUSE', 0, true, 0, now(), now()),
    (9000000000000000206, 'SI-PROC-B', 'SI process B', 'INHOUSE', 1, true, 0, now(), now());

-- ---- 货架 ↔ 工序映射（生产架 ↔ 工序 A；本端点只读不校验，仅作真实数据形态） ---
INSERT INTO t_shelf_process (id, shelf_id, process_id, sort_order, version, created_at, updated_at)
VALUES (9000000000000000207, 9000000000000000204, 9000000000000000205, 0, 0, now(), now());

-- ---- 用户（5 个角色各一个；密码明文 "changeme"，cost=12 bcrypt） ---------------
INSERT INTO t_user (id, username, password_hash, full_name, is_active, refresh_token_version, version, created_at, updated_at)
VALUES
    (9000000000000000208, 'fx_si_manager',   '$2b$12$KjlHPD7WcAbXC1v6RZxXKOfFHGJJDGS4bxfKGPRnLBmWmqFMvS/4W', 'SI Manager',   true, 0, 0, now(), now()),
    (9000000000000000209, 'fx_si_clerk',     '$2b$12$KjlHPD7WcAbXC1v6RZxXKOfFHGJJDGS4bxfKGPRnLBmWmqFMvS/4W', 'SI Clerk',     true, 0, 0, now(), now()),
    (9000000000000000210, 'fx_si_inspector', '$2b$12$KjlHPD7WcAbXC1v6RZxXKOfFHGJJDGS4bxfKGPRnLBmWmqFMvS/4W', 'SI Inspector', true, 0, 0, now(), now()),
    (9000000000000000211, 'fx_si_cnc',       '$2b$12$KjlHPD7WcAbXC1v6RZxXKOfFHGJJDGS4bxfKGPRnLBmWmqFMvS/4W', 'SI CNC',       true, 0, 0, now(), now()),
    (9000000000000000212, 'fx_si_shelf',     '$2b$12$KjlHPD7WcAbXC1v6RZxXKOfFHGJJDGS4bxfKGPRnLBmWmqFMvS/4W', 'SI Shelf',     true, 0, 0, now(), now());

-- ---- 用户角色（2 白名单 + 3 越权；SHELF_ACCOUNT scope 到品检架） --------------
INSERT INTO t_user_role (id, user_id, role, scope_type, scope_id, version, created_at, updated_at)
VALUES
    (9000000000000000213, 9000000000000000208, 'MANAGER',       NULL,    NULL,                 0, now(), now()),
    (9000000000000000214, 9000000000000000209, 'CLERK',         NULL,    NULL,                 0, now(), now()),
    (9000000000000000215, 9000000000000000210, 'INSPECTOR',     NULL,    NULL,                 0, now(), now()),
    (9000000000000000216, 9000000000000000211, 'CNC_PROGRAMMER', NULL,   NULL,                 0, now(), now()),
    (9000000000000000217, 9000000000000000212, 'SHELF_ACCOUNT', 'shelf', 9000000000000000203, 0, now(), now());

-- ---- 装配件主表行（子件的父；带 serial_no 供「扫装配件条码」场景） ----------
INSERT INTO t_assembly (id, drawing_no, name, applicant_name, customer_id, request_date, planned_delivery_date, is_urgent, status, serial_no, version, created_at, updated_at)
VALUES (9000000000000000218, 'D-SI-ASM', 'SI assembly', 'SI', 9000000000000000201, now(), now() + INTERVAL '30 days', false, 'IN_PROCESS', 'SI-ASM', 0, now(), now());

-- ---- 零件行（t_part） --------------------------------------------------------
-- 说明：
--  - serial_no 列宽 varchar(15)，fixture 值均 <= 15 字符；
--  - PART_STANDALONE.version 刻意写 1（与 BATCH_INSPECTION.version=3 不同），
--    用来证明批次 version 来自 t_part_batch.version；
--  - 4 个子件的 serial_no 形如 {asm}-{i:02d}，即 service 排序口径里的「装配序」。
INSERT INTO t_part (id, serial_no, name, drawing_no, applicant_name, request_date, planned_delivery_date, status, is_urgent, customer_id, quantity, unit_price, total_price, assembly_id, process_chain_id, version, created_at, updated_at, deleted_at)
VALUES
    -- 独立件（assembly_id IS NULL）：挂 8 个批次覆盖全部批次状态
    (9000000000000000219, 'SI-S1001', 'SI standalone', 'D-SI-01', 'SI', now(), now() + INTERVAL '30 days', 'INSPECTION', false, 9000000000000000202, 10, 0, 0, NULL, NULL, 1, now(), now(), NULL),
    -- 子件 #1（★ 扫码树核心回归：被扫中的那个）
    (9000000000000000220, 'SI-ASM-01', 'SI child 1',    'D-SI-02', 'SI', now(), now() + INTERVAL '30 days', 'INSPECTION', false, 9000000000000000202, 5, 0, 0, 9000000000000000218, NULL, 4, now(), now(), NULL),
    -- 子件 #2（无批次）：证明 children 是「全部子件」而非「被扫中的那个」
    (9000000000000000221, 'SI-ASM-02', 'SI child 2',    'D-SI-03', 'SI', now(), now() + INTERVAL '30 days', 'PENDING',   false, 9000000000000000202, 5, 0, 0, 9000000000000000218, NULL, 0, now(), now(), NULL),
    -- 子件 #3（无批次）：同上，凑 3 个子件让断言是「恰好这三个」
    (9000000000000000222, 'SI-ASM-03', 'SI child 3',    'D-SI-04', 'SI', now(), now() + INTERVAL '30 days', 'INSPECTION', false, 9000000000000000202, 5, 0, 0, 9000000000000000218, NULL, 0, now(), now(), NULL),
    -- 软删子件 #4（deleted_at 非空）：扫不到、也不出现在装配件树的 children 里
    (9000000000000000223, 'SI-ASM-04', 'SI child deleted', 'D-SI-05', 'SI', now(), now() + INTERVAL '30 days', 'PENDING', false, 9000000000000000202, 5, 0, 0, 9000000000000000218, NULL, 0, now(), now(), now());

-- ---- 批次行（t_part_batch） -------------------------------------------------
-- ⚠️ 两条口径在下面被**写死**：
--  1. INSPECTION / READY_TO_SHIP 批次的 current_process_id 恒 NULL（出池清该列的
--     不变式 ⇒ 扫码树里它们的 process_name 恒 null，这是正确行为不是缺陷）；
--  2. IN_PROCESS 批次带 current_process_id ⇒ process_name 取到工序名。
-- INSPECTION 区货架 holder 走 t_shelf，location 写 'INSPECTION_SHELF'。
INSERT INTO t_part_batch (id, part_id, batch_no, quantity, status, location, current_holder_id, current_process_id, is_repairing, version, created_at, updated_at, deleted_at)
VALUES
    -- 独立件批次 1：INSPECTION 在品检架上（★ version=3 ≠ 零件 version=1）
    (9000000000000000230, 9000000000000000219, 1, 4, 'INSPECTION',     'INSPECTION_SHELF', 9000000000000000203, NULL,                 false, 3, now(), now(), NULL),
    -- 独立件批次 2：PENDING 尚未下发
    (9000000000000000231, 9000000000000000219, 2, 2, 'PENDING',        NULL,                NULL,                 NULL,                 false, 0, now(), now(), NULL),
    -- 独立件批次 3：IN_PROCESS 在生产架上、工序 A（★ process_name 取到 'SI process A'）
    (9000000000000000232, 9000000000000000219, 3, 2, 'IN_PROCESS',     'PRODUCTION_SHELF', 9000000000000000204, 9000000000000000205, false, 1, now(), now(), NULL),
    -- 独立件批次 4：READY_TO_SHIP（进该态的必经 INSPECTION ⇒ current_process_id 恒 NULL）
    (9000000000000000233, 9000000000000000219, 4, 2, 'READY_TO_SHIP',  'OFFICE',            NULL,                 NULL,                 false, 0, now(), now(), NULL),
    -- 独立件批次 5：COMPLETED 终态
    (9000000000000000234, 9000000000000000219, 5, 1, 'COMPLETED',      'OFFICE',            NULL,                 NULL,                 false, 0, now(), now(), NULL),
    -- 独立件批次 6：CANCELLED 终态
    (9000000000000000235, 9000000000000000219, 6, 1, 'CANCELLED',      NULL,                NULL,                 NULL,                 false, 0, now(), now(), NULL),
    -- 独立件批次 7：IN_PROCESS + is_repairing（返修标记透传，工序 B）
    (9000000000000000236, 9000000000000000219, 7, 1, 'IN_PROCESS',     'PRODUCTION_SHELF', 9000000000000000204, 9000000000000000206, true,  2, now(), now(), NULL),
    -- 独立件批次 8：软删批次（软删闸门：不出现在扫码树里）
    (9000000000000000237, 9000000000000000219, 8, 1, 'INSPECTION',     'INSPECTION_SHELF', 9000000000000000203, NULL,                 false, 9, now(), now(), now()),
    -- 子件 #1 的唯一批次：INSPECTION 在品检架上（version=7，与子件 version=4 又不同）
    (9000000000000000240, 9000000000000000220, 1, 5, 'INSPECTION',     'INSPECTION_SHELF', 9000000000000000203, NULL,                 false, 7, now(), now(), NULL);

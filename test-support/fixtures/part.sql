-- ============================================================================
--  part 域集成测试 fixture (PR13 Phase G, 2026-09-23)
--
--  加载入口：test-support::fixture::load_part_fixture(pool)
--  加载方式：sqlx::raw_sql(include_str!("../../fixtures/part.sql")).execute(pool).await
--           (整体作为一条 multi-statement SQL 执行)
--
--  ## 设计原则
--  - 所有 ID 走常量 9_000_000_000_000_000_010+ 区段，与 process_chain (1-9)
--    物理不相交；为 delivery 域预留 50+ 段
--  - 预生成 bcrypt 哈希嵌入 SQL（明文 "changeme", cost=12），与
--    process_chain.sql 共用同一哈希字面值（同一明文 + cost 输出确定）
--  - 时间列：审计字段用 now()，业务日期按 fixtures 字面
--  - 不引 trigger / 不调 hsh_erp_rust 函数 —— SQL 仅 INSERT 静态行
--
--  ## ID 段分配（10-49，留 50+ 给 delivery）
--  CUSTOMER_L1_ID          10   L1 客户（带 serial_prefix='P'）
--  CUSTOMER_L2_ID          11   L2 客户（挂在 L1 下，prefix=NULL）
--  PROCESS_ID              12   INHOUSE 工序 FX-PROC-A
--  WORK_TYPE_ID            13   工种 FX-WT-A
--  INSPECTION_SHELF_ID     14   FX 检验架
--  PRODUCTION_SHELF_ID     15   FX 生产架（已绑到 PROCESS_ID）
--  MANAGER_USER_ID         16   fx_part_manager
--  INSPECTOR_USER_ID       17   fx_part_inspector
--  CLERK_USER_ID           18   fx_part_clerk
--  SHELF_ACCOUNT_USER_ID   19   fx_part_shelf（无角色守卫测试用）
--  MANAGER_ROLE_ID         20
--  INSPECTOR_ROLE_ID       21
--  CLERK_ROLE_ID           22
--  SHELF_ACCOUNT_ROLE_ID   23   scope=INSPECTION_SHELF（合法登录但越权）
--  WORK_TYPE_PROCESS_ID    24   work_type ↔ process 映射
--  SHELF_PROCESS_ID        25   shelf（生产） ↔ process 映射
--
--  ## 注意：fixture 不含 t_part / t_part_batch 行
--  多数 part 域测试需要 status=READY_TO_SHIP / DELIVERED / REPAIRING / COMPLETED
--  等特定状态；状态机不允许从这些状态转回 PENDING。若 fixture 预置 2 行
--  PENDING/IN_PROCESS，会让 list_parts_basic 等「期望空库」测试失败，也会
--  出现在 unfiltered list 中干扰其它断言。因此 fixture 仅预置「不可变共享」
--  行（customer / process / shelf / user / role / 映射），工单 / 批次由各 sub-file
--  按需用 sqlx::query 直插（PR-C 末统一迁 test-support）。
-- ============================================================================

-- ---- L1 客户 + L2 子客户 ----
INSERT INTO t_customer (id, name, parent_id, serial_prefix, version, created_at, updated_at)
VALUES
  (9000000000000000010, 'FX 客户 L1', NULL, 'P', 0, now(), now()),
  (9000000000000000011, 'FX 客户 L2', 9000000000000000010, NULL, 0, now(), now());

-- ---- 工序（INHOUSE 类别，FX-PROC-A）----
INSERT INTO t_process (id, code, name, category, sort_order, requires_approval, version, created_at, updated_at)
VALUES
  (9000000000000000012, 'FX-PROC-A', 'FX 工序 A', 'INHOUSE', 0, false, 0, now(), now());

-- ---- 工种（FX-WT-A）----
INSERT INTO t_work_type (id, code, name, sort_order, version, created_at, updated_at)
VALUES
  (9000000000000000013, 'FX-WT-A', 'FX 工种 A', 0, 0, now(), now());

-- ---- 货架（检验架 + 生产架）----
INSERT INTO t_shelf (id, code, name, zone, is_active, display_order, version, created_at, updated_at)
VALUES
  (9000000000000000014, 'FX-SH-INSP', 'FX 检验架', 'INSPECTION', true, 0, 0, now(), now()),
  (9000000000000000015, 'FX-SH-PROD', 'FX 生产架', 'PRODUCTION', true, 0, 0, now(), now());

-- ---- 货架 ↔ 工序 映射（生产架 → 工序 FX-PROC-A）----
INSERT INTO t_shelf_process (id, shelf_id, process_id, sort_order, version, created_at, updated_at)
VALUES
  (9000000000000000025, 9000000000000000015, 9000000000000000012, 0, 0, now(), now());

-- ---- 工种 ↔ 工序 映射 ----
INSERT INTO t_work_type_process (id, work_type_id, process_id, sort_order, version, created_at, updated_at)
VALUES
  (9000000000000000024, 9000000000000000013, 9000000000000000012, 0, 0, now(), now());

-- ---- 用户（密码明文 "changeme"，cost=12 bcrypt，hash 复用 process_chain.sql 字面值）----
INSERT INTO t_user (id, username, password_hash, full_name, is_active, refresh_token_version, version, created_at, updated_at)
VALUES
  (9000000000000000016, 'fx_part_manager',   '$2b$12$KjlHPD7WcAbXC1v6RZxXKOfFHGJJDGS4bxfKGPRnLBmWmqFMvS/4W', 'FX Manager',   true, 0, 0, now(), now()),
  (9000000000000000017, 'fx_part_inspector', '$2b$12$KjlHPD7WcAbXC1v6RZxXKOfFHGJJDGS4bxfKGPRnLBmWmqFMvS/4W', 'FX Inspector', true, 0, 0, now(), now()),
  (9000000000000000018, 'fx_part_clerk',     '$2b$12$KjlHPD7WcAbXC1v6RZxXKOfFHGJJDGS4bxfKGPRnLBmWmqFMvS/4W', 'FX Clerk',     true, 0, 0, now(), now()),
  (9000000000000000019, 'fx_part_shelf',     '$2b$12$KjlHPD7WcAbXC1v6RZxXKOfFHGJJDGS4bxfKGPRnLBmWmqFMvS/4W', 'FX Shelf',     true, 0, 0, now(), now());

-- ---- 用户角色（MANAGER / INSPECTOR / CLERK 无 scope；SHELF_ACCOUNT scope 到 INSPECTION_SHELF）----
INSERT INTO t_user_role (id, user_id, role, scope_type, scope_id, version, created_at, updated_at)
VALUES
  (9000000000000000020, 9000000000000000016, 'MANAGER',       NULL,     NULL,                          0, now(), now()),
  (9000000000000000021, 9000000000000000017, 'INSPECTOR',     NULL,     NULL,                          0, now(), now()),
  (9000000000000000022, 9000000000000000018, 'CLERK',         NULL,     NULL,                          0, now(), now()),
  (9000000000000000023, 9000000000000000019, 'SHELF_ACCOUNT', 'shelf',  9000000000000000014,           0, now(), now());
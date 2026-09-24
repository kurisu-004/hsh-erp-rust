-- ============================================================================
--  applicant 域集成测试 fixture (PR13 Phase I, 2026-09-24)
--
--  加载入口：test-support::fixture::load_applicant_fixture(pool)
--  加载方式：sqlx::raw_sql(include_str!("../../fixtures/applicant.sql")).execute(pool).await
--
--  ## 设计原则
--  - 所有 ID 走常量 9_000_000_000_000_000_130+ 区段（process_chain 1-9 /
--    part 10-49 / delivery 50-52 / production 60-69 / assembly 70+ /
--    shelf 80+ / statistics 90+ / outsource 100-104 / iam 110-118 /
--    user_repo 120-122，物理不相交）。
--  - bcrypt cost=12 哈希复用 iam.sql 字面值（同一明文 "changeme"）。
--  - 时间列：审计字段用 now()。
--  - 不引 trigger / 不调 hsh_erp_rust 函数 —— SQL 仅 INSERT 静态行。
--
--  ## ID 段分配（130+）
--  MANAGER_USER_ID  130   fx_applicant_manager（MANAGER role，active）
--  MANAGER_ROLE_ID  131   baseline MANAGER role
--  L1_CUSTOMER_ID   132   fx_applicant_l1（serial_prefix='A'，供 customer_id 引用）
--  L2_CUSTOMER_ID   133   fx_applicant_l2（parent=132，L2 校验路径用）
--  SAMPLE_APPLICANT_ID 134   fx_applicant_baseline（applicant 行；happy path 引用）
--  PART_REF_ID      135   fx_part_referencing_applicant（t_part 引用 134）
--
--  ## 独立加载（不复用其它域基线）
--  applicant 测试不需要 process / work_type / shelf fixture；
--  本 fixture 覆盖 t_user / t_user_role / t_customer / t_applicant / t_part 五张表共 6 行。
-- ============================================================================

-- ---- MANAGER 用户（密码明文 "changeme"，bcrypt cost=12）----
INSERT INTO t_user (id, username, password_hash, full_name, is_active, refresh_token_version, version, created_at, updated_at)
VALUES
  (9000000000000000130, 'fx_applicant_manager', '$2b$12$KjlHPD7WcAbXC1v6RZxXKOfFHGJJDGS4bxfKGPRnLBmWmqFMvS/4W', 'FX Applicant Manager', true, 0, 0, now(), now());

-- ---- MANAGER role（无 scope）----
INSERT INTO t_user_role (id, user_id, role, scope_type, scope_id, version, created_at, updated_at)
VALUES
  (9000000000000000131, 9000000000000000130, 'MANAGER', NULL, NULL, 0, now(), now());

-- ---- L1 客户（serial_prefix='A'，applicant 校验要求 prefix=A-Z 单字符）----
INSERT INTO t_customer (id, name, parent_id, serial_prefix, version, created_at, updated_at)
VALUES
  (9000000000000000132, 'FX Applicant L1', NULL, 'A', 0, now(), now());

-- ---- L2 客户（parent=132，L2 校验路径用）----
INSERT INTO t_customer (id, name, parent_id, serial_prefix, version, created_at, updated_at)
VALUES
  (9000000000000000133, 'FX Applicant L2', 9000000000000000132, NULL, 0, now(), now());

-- ---- baseline applicant 行（happy path 引用，applicant_name='fx-baseline-applicant'）----
INSERT INTO t_applicant (id, name, customer_id, version, created_at, updated_at)
VALUES
  (9000000000000000134, 'fx-baseline-applicant', 9000000000000000132, 0, now(), now());

-- ---- baseline t_part 行（引用 applicant_id=134 + customer_id=132）----
INSERT INTO t_part (id, name, drawing_no, applicant_name, customer_id, request_date, planned_delivery_date, status, version, created_at, updated_at)
VALUES
  (9000000000000000135, 'fx-part-ref-applicant', 'D-FX-APP', 'fx-baseline-applicant', 9000000000000000132, CURRENT_DATE, CURRENT_DATE, 'PENDING', 0, now(), now());
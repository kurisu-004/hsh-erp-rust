-- ============================================================================
--  user_repo 域集成测试 fixture (PR13 Phase I, 2026-09-24)
--
--  加载入口：test-support::fixture::load_user_repo_fixture(pool)
--  加载方式：sqlx::raw_sql(include_str!("../../fixtures/user_repo.sql")).execute(pool).await
--
--  ## 设计原则
--  - 所有 ID 走常量 9_000_000_000_000_000_120+ 区段（process_chain 1-9 /
--    part 10-49 / delivery 50-52 / production 60-69 / assembly 70+ /
--    shelf 80+ / statistics 90+ / outsource 100-104 / iam 110-118，
--    物理不相交）。
--  - bcrypt cost=12 哈希复用 part.sql 字面值（同一明文 "changeme",
--    cost=12 → 同一哈希输出）。
--  - 时间列：审计字段用 now()。
--  - 不引 trigger / 不调 hsh_erp_rust 函数 —— SQL 仅 INSERT 静态行。
--
--  ## ID 段分配（120+）
--  BASELINE_USER_ID  120   fx_user_repo_baseline（密码 "changeme"，active）
--  BASELINE_ROLE_ID  121   baseline MANAGER role（属于 baseline user，无 scope）
--  BASELINE_MENU_ID  122   baseline t_menu 行（不挂 t_role_menu）
--
--  ## 独立加载（不依赖 iam / part 域基线）
--  user_repo 测试不需要 customer / process / work_type / part fixture；
--  本 fixture 只覆盖 t_user / t_user_role / t_menu 三张表共 3 行。
--  load_user_repo_fixture 内部不调 load_iam_fixture / load_part_fixture。
--
--  ## 不预置 t_shelf
--  role.rs 内 ShelfRepo 测试用 seed_shelf 自建不同 code，不预置。
--
--  ## 不预置 t_role_menu
--  MenuRepo 测试只测 list_active_for_roles DISTINCT / 过滤 / 排序逻辑；
--  baseline menu 不挂 t_role_menu；测试现场 link_role_menu 自建关系。
-- ============================================================================

-- ---- baseline 用户（密码明文 "changeme"，bcrypt cost=12）----
-- 仅作为「已知可用 ID 起点」供需要 baseline 的测试选用；多数测试仍走
-- 本地 seed_user / seed_role 等 helper 创建专属测试数据。
INSERT INTO t_user (id, username, password_hash, full_name, is_active, refresh_token_version, version, created_at, updated_at)
VALUES
  (9000000000000000120, 'fx_user_repo_baseline', '$2b$12$KjlHPD7WcAbXC1v6RZxXKOfFHGJJDGS4bxfKGPRnLBmWmqFMvS/4W', 'FX UserRepo Baseline', true, 0, 0, now(), now());

-- ---- baseline 用户角色（MANAGER 无 scope）----
-- t_user_role 的 UNIQUE(user_id, role, scope_type, scope_id) 是 NON-partial
--（迁移 001），baseline user 已持有 (manager, NULL, NULL)；测试若给 baseline
-- user 加 MANAGER 无 scope 角色会被约束拒绝（设计上 baseline user 应保持
-- 「单一 MANAGER 角色」不变量）。
INSERT INTO t_user_role (id, user_id, role, scope_type, scope_id, version, created_at, updated_at)
VALUES
  (9000000000000000121, 9000000000000000120, 'MANAGER', NULL, NULL, 0, now(), now());

-- ---- baseline 菜单（不挂 t_role_menu）----
INSERT INTO t_menu (id, parent_id, code, title, sort_order, is_active, version, created_at, updated_at)
VALUES
  (9000000000000000122, NULL, 'fx-user-repo-baseline', 'FX UserRepo Baseline', 0, true, 0, now(), now());
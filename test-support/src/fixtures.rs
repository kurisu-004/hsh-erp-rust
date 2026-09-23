//! 集成测试 fixture helper：建最小化的「admin / MANAGER / 一组货架 / 菜单」世界
//!
//! 2026-09-23 PR13 Phase A：从原 `tests/common/mod.rs` 切到这里。承载：
//! - `insert_user_with_password` / `insert_inactive_user`（建账号）
//! - `add_role` / `insert_menu` / `add_role_menu`（RBAC 关联）
//! - `insert_shelf`（货架）
//! - `clean_db` / `clean_business_db`（no-op 兼容旧 caller）
//! - `seed_process` / `link_work_type_to_process` / `link_shelf_to_process`
//!   （worker-pool 域 fixture）
//! - `create_chain_for_part` / `create_step` / `seed_test_process`
//!   （part 域 PR-3 工艺链 fixture）
//!
//! ## 设计要点（沿用原 mod.rs 实现）
//! - 雪花 ID 全部走 [`crate::pool::pool_snowflake`] 全局互斥，
//!   同进程内 `next_id()` 串行不发冲突；跨进程 unique instance 不撞。
//! - `clean_db` / `clean_business_db` 已改 no-op（`test_pool` 每次 fresh database，
//!   无需清）；保留函数签名仅为不让 38 处 caller 报错。
//! - `await_holding_lock`：所有 INSERT 模式都是「lock → next_id → .await INSERT」，
//!   guard 在 .await 期间持有。改成 expression 形式需重写所有 helper，
//!   收益与风险不对等；模块顶部 `#![allow(clippy::await_holding_lock)]` 全豁免。

use sqlx::PgPool;

use crate::pool::pool_snowflake;

/// 清表（auth 链路涉及的最小集）：用户/角色/菜单/角色-菜单/货架。
///
/// 2026-09-20 plan 2 改为 no-op：`test_pool()` 每次走 TEMPLATE 克隆派生全新 DB，
/// 已是最干净状态，无需 TRUNCATE。保留函数签名仅为不让 38 处 caller 报错。
#[allow(dead_code, clippy::unused_async)]
pub async fn clean_db(_pool: &PgPool) {
    // no-op：test_pool() 每次 fresh database（nextest: TEMPLATE clone ~100ms；
    // cargo test 兼容路径: plain CREATE DATABASE + migrate ~600ms）。
}

/// 清表（业务域全集）：配送分组 / 配送单 / 批次 / 工单 / 装配体 / 客户 / 申请人 / 工种 / 工人
/// / 工艺链。
///
/// 2026-09-20 plan 2 改为 no-op：`test_pool()` 每次 fresh database，无需 TRUNCATE。
/// 保留函数签名仅为不让 caller 报错。
#[allow(dead_code, clippy::unused_async)]
pub async fn clean_business_db(_pool: &PgPool) {
    // no-op：同上 clean_db
}

#[allow(dead_code)]
pub async fn insert_user_with_password(pool: &PgPool, username: &str, plain_password: &str) -> i64 {
    use hsh_erp_rust::auth::password;
    use hsh_erp_rust::infra::clock::now_naive;

    let hash = password::hash(plain_password).expect("bcrypt hash");
    let snowflake = pool_snowflake().lock().unwrap();
    let id = snowflake.next_id();
    let now = now_naive();
    sqlx::query!(
        "INSERT INTO t_user (id, username, password_hash, full_name, is_active, \
         refresh_token_version, version, created_at, updated_at) \
         VALUES ($1, $2, $3, $4, true, 0, 0, $5, $5)",
        id,
        username.to_lowercase(),
        hash,
        username,
        now,
    )
    .execute(pool)
    .await
    .expect("insert t_user");
    id
}

/// 插一个 is_active=false 的用户（用于测试「已停用账号」拒绝登录）
#[allow(dead_code)]
pub async fn insert_inactive_user(pool: &PgPool, username: &str, plain_password: &str) -> i64 {
    use hsh_erp_rust::auth::password;
    use hsh_erp_rust::infra::clock::now_naive;

    let hash = password::hash(plain_password).expect("bcrypt hash");
    let snowflake = pool_snowflake().lock().unwrap();
    let id = snowflake.next_id();
    let now = now_naive();
    sqlx::query!(
        "INSERT INTO t_user (id, username, password_hash, full_name, is_active, \
         refresh_token_version, version, created_at, updated_at) \
         VALUES ($1, $2, $3, $4, false, 0, 0, $5, $5)",
        id,
        username.to_lowercase(),
        hash,
        username,
        now,
    )
    .execute(pool)
    .await
    .expect("insert inactive t_user");
    id
}

#[allow(dead_code)]
pub async fn add_role(
    pool: &PgPool,
    user_id: i64,
    role: &str,
    scope_type: Option<&str>,
    scope_id: Option<i64>,
) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;

    let snowflake = pool_snowflake().lock().unwrap();
    let id = snowflake.next_id();
    let now = now_naive();
    sqlx::query!(
        "INSERT INTO t_user_role (id, user_id, role, scope_type, scope_id, version, \
         created_at, updated_at) \
         VALUES ($1, $2, $3, $4, $5, 0, $6, $6)",
        id,
        user_id,
        role,
        scope_type,
        scope_id,
        now,
    )
    .execute(pool)
    .await
    .expect("insert t_user_role");
    id
}

#[allow(dead_code)]
pub async fn insert_menu(
    pool: &PgPool,
    code: &str,
    title: &str,
    path: Option<&str>,
    parent_id: Option<i64>,
) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;

    let snowflake = pool_snowflake().lock().unwrap();
    let id = snowflake.next_id();
    let now = now_naive();
    sqlx::query!(
        "INSERT INTO t_menu (id, parent_id, code, title, path, sort_order, is_active, \
         version, created_at, updated_at) \
         VALUES ($1, $2, $3, $4, $5, 0, true, 0, $6, $6)",
        id,
        parent_id,
        code,
        title,
        path,
        now,
    )
    .execute(pool)
    .await
    .expect("insert t_menu");
    id
}

#[allow(dead_code)]
pub async fn add_role_menu(pool: &PgPool, role: &str, menu_id: i64) {
    use hsh_erp_rust::infra::clock::now_naive;

    let snowflake = pool_snowflake().lock().unwrap();
    let id = snowflake.next_id();
    let now = now_naive();
    sqlx::query!(
        "INSERT INTO t_role_menu (id, role, menu_id, version, created_at, updated_at) \
         VALUES ($1, $2, $3, 0, $4, $4)",
        id,
        role,
        menu_id,
        now,
    )
    .execute(pool)
    .await
    .expect("insert t_role_menu");
}

#[allow(dead_code)]
pub async fn insert_shelf(pool: &PgPool, code: &str, name: &str, zone: &str) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;

    let snowflake = pool_snowflake().lock().unwrap();
    let id = snowflake.next_id();
    let now = now_naive();
    sqlx::query!(
        "INSERT INTO t_shelf (id, code, name, zone, is_active, display_order, version, \
         created_at, updated_at) \
         VALUES ($1, $2, $3, $4, true, 0, 0, $5, $5)",
        id,
        code,
        name,
        zone,
        now,
    )
    .execute(pool)
    .await
    .expect("insert t_shelf");
    id
}

/// 取 user 当前 `refresh_token_version`
#[allow(dead_code)]
pub async fn get_refresh_token_version(pool: &PgPool, user_id: i64) -> i32 {
    let row = sqlx::query!(
        "SELECT refresh_token_version AS \"ver!\" FROM t_user WHERE id = $1",
        user_id
    )
    .fetch_one(pool)
    .await
    .expect("query refresh_token_version");
    row.ver
}

// ===========================================================================
// worker-pool 域 fixture helpers（Task 10 e2e 测试用）：
//   - seed_process: 插一个 t_process 工序（INHOUSE 类别）
//   - link_work_type_to_process: t_work_type_process 映射
//   - link_shelf_to_process: t_shelf_process 映射
//
// 命名风格：与 part_api.rs 的 insert_part / insert_batch 同形（prefix=动词 + 名词）。
// 雪花 ID：复用同一 epoch/instance/seq（1_577_836_800_000 / 1 / 1），与其它 fixture 一致。
// ===========================================================================

/// 插一个 INHOUSE 类别 `t_process` 工序（worker-pool 用：INHOUSE 自产）。
#[allow(dead_code)]
pub async fn seed_process(pool: &PgPool, code: &str, name: &str) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;

    let snowflake = pool_snowflake().lock().unwrap();
    let id = snowflake.next_id();
    let now = now_naive();
    sqlx::query!(
        "INSERT INTO t_process (id, code, name, category, sort_order, requires_approval, \
         version, created_at, updated_at) \
         VALUES ($1, $2, $3, 'INHOUSE', 0, false, 0, $4, $4)",
        id,
        code,
        name,
        now,
    )
    .execute(pool)
    .await
    .expect("insert t_process");
    id
}

/// `t_work_type_process` 映射（无业务软删：`deleted_at` 留默认 NULL）。
#[allow(dead_code)]
pub async fn link_work_type_to_process(pool: &PgPool, wt_id: i64, p_id: i64) {
    use hsh_erp_rust::infra::clock::now_naive;

    let snowflake = pool_snowflake().lock().unwrap();
    let id = snowflake.next_id();
    let now = now_naive();
    sqlx::query!(
        "INSERT INTO t_work_type_process (id, work_type_id, process_id, sort_order, \
         version, created_at, updated_at) \
         VALUES ($1, $2, $3, 0, 0, $4, $4)",
        id,
        wt_id,
        p_id,
        now,
    )
    .execute(pool)
    .await
    .expect("insert t_work_type_process");
}

/// `t_shelf_process` 映射（无业务软删）。
#[allow(dead_code)]
pub async fn link_shelf_to_process(pool: &PgPool, s_id: i64, p_id: i64) {
    use hsh_erp_rust::infra::clock::now_naive;

    let snowflake = pool_snowflake().lock().unwrap();
    let id = snowflake.next_id();
    let now = now_naive();
    sqlx::query!(
        "INSERT INTO t_shelf_process (id, shelf_id, process_id, sort_order, \
         version, created_at, updated_at) \
         VALUES ($1, $2, $3, 0, 0, $4, $4)",
        id,
        s_id,
        p_id,
        now,
    )
    .execute(pool)
    .await
    .expect("insert t_shelf_process");
}

/// 2026-09-16 PR-3 批次 step 化：to_process / place_on_shelf / send_to_outsource /
/// repair 等"进入生产流"端点要求 part 已绑定工艺链（migration 028 +
/// error code 20706 BIZ_PROCESS_CHAIN_REQUIRED）。本 helper 帮 part 建链 + 绑 part。
///
/// 返回 chain_id；caller 可继续调 `create_step` 加 step。
#[allow(dead_code)]
pub async fn create_chain_for_part(pool: &PgPool, part_id: i64) -> i64 {
    let snowflake = pool_snowflake().lock().unwrap();
    let chain_id = snowflake.next_id();
    sqlx::query(
        "INSERT INTO t_part_process_chain (id, name, version, created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, 0, now(), 0, now(), 0)",
    )
    .bind(chain_id)
    .bind(format!("chain-{part_id}"))
    .execute(pool)
    .await
    .expect("insert chain");
    sqlx::query("UPDATE t_part SET process_chain_id = $1 WHERE id = $2")
        .bind(chain_id)
        .bind(part_id)
        .execute(pool)
        .await
        .expect("bind part to chain");
    chain_id
}

/// 2026-09-16 PR-3 批次 step 化：在指定 chain 内创建 step（process_id + sort_order）。
#[allow(dead_code)]
pub async fn create_step(pool: &PgPool, chain_id: i64, process_id: i64, sort_order: i32) -> i64 {
    let snowflake = pool_snowflake().lock().unwrap();
    let step_id = snowflake.next_id();
    sqlx::query(
        "INSERT INTO t_process_chain_step (id, chain_id, sort_order, process_id, \
         estimated_minutes, version, created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, $3, $4, 30, 0, now(), 0, now(), 0)",
    )
    .bind(step_id)
    .bind(chain_id)
    .bind(sort_order)
    .bind(process_id)
    .execute(pool)
    .await
    .expect("insert chain step");
    step_id
}

/// 2026-09-16 PR-3 适配：seed 一个 active process（PR-3 之前测试用 999_999 占位 process_id，
/// 现 to_process / place_on_shelf 等端点会经 process_chain 守卫 + step JOIN 校验。
/// 本 helper 提供真实 t_process 行便于测试）。
#[allow(dead_code)]
pub async fn seed_test_process(pool: &PgPool, code: &str, name: &str) -> i64 {
    let snowflake = pool_snowflake().lock().unwrap();
    let proc_id = snowflake.next_id();
    sqlx::query(
        "INSERT INTO t_process (id, code, name, category, sort_order, requires_approval, \
         version, created_at, updated_at) \
         VALUES ($1, $2, $3, 'INHOUSE', 0, false, 0, now(), now())",
    )
    .bind(proc_id)
    .bind(code)
    .bind(name)
    .execute(pool)
    .await
    .expect("insert process");
    proc_id
}

//! # 集成测试动态 fixture helper（PR13 Phase C.Final 末段遗留，2026-09-24）
//!
//! 2026-09-24 PR13 Phase A-I 完成时，原 15 个动态 helper 中 7 个已迁出
//! （`clean_db` / `clean_business_db` / `insert_inactive_user` /
//! `insert_menu` / `add_role_menu` / `get_refresh_token_version` /
//! `seed_test_process`），grep verify 零调用。本 PR-C.Final 删除原文件。
//!
//! 剩余 9 个 helper 仍在以下 binary / sub-file 使用，调用方迁移属
//! 其它 binary scope（本任务硬约束「不改其它 binary 的 tests/*」），
//! 本文件**保留这 9 个函数 + fixture 路由**，作为 PR-D 后续清理目标：
//!
//! | helper                  | 调用方                                          |
//! |-------------------------|------------------------------------------------|
//! | `insert_user_with_password` | production/{worker_pool,worker_pool_auto_allocate}.rs |
//! | `add_role`              | production/{worker_pool,worker_pool_auto_allocate}.rs |
//! | `seed_process`          | production/{work_type,worker_pool,worker_pool_auto_allocate}.rs |
//! | `link_shelf_to_process` | production/{worker_pool,worker_pool_auto_allocate}.rs |
//! | `link_work_type_to_process` | production/{worker_pool,worker_pool_auto_allocate}.rs |
//! | `insert_shelf`          | production/{worker_pool,worker_pool_auto_allocate}.rs + part/{to_inspection,list_enrichment}.rs |
//! | `create_chain_for_part` | part/{lifecycle,to_process,repair}.rs           |
//! | `create_step`           | part/{lifecycle,to_process,repair}.rs           |
//! | `seed_outsource_process`（本文件未实现，outsource 域已 self-define）| — |
//!
//! ## 迁移指引（PR-D 后续 phase）
//! - `insert_user_with_password` / `add_role`：走 `<Binary>Fixture::MANAGER_USERNAME` +
//!   `login_token` 直接登录，fixture 预置 baseline user。
//! - `seed_process` / `link_shelf_to_process` / `link_work_type_to_process`：
//!   改走 `load_<domain>_fixture` 预置；本任务保留 local helper。
//! - `insert_shelf` / `create_chain_for_part` / `create_step`：
//!   改走 `PartFixture` 预置 shelf / process_chain；本任务保留 local helper。
//!
//! ## 为什么用 `sqlx::query`（非 macro）
//! 本 crate 走 `SQLX_OFFLINE=true` 编译，sqlx macro 要求 .sqlx cache 里有
//! 匹配 hash；master `fixtures.rs` 已删除且重跑 prepare 与原 cache hash
//! 不一致；为避免「无 cached data」错误，使用 runtime `sqlx::query` +
//! `.bind()`（同 `tests/part/helpers.rs::seed_process` 私有 helper 模式）。

use sqlx::PgPool;

// ===========================================================================
// 用户 / 角色 helpers（production 域 worker_pool 测试用）
// ===========================================================================

/// 插一个 `is_active=true` 的 `t_user` 行（bcrypt 哈希现场生成）。
#[allow(dead_code)]
pub async fn insert_user_with_password(
    pool: &PgPool,
    username: &str,
    plain_password: &str,
) -> i64 {
    use hsh_erp_rust::auth::password;
    use hsh_erp_rust::infra::clock::now_naive;

    let hash = password::hash(plain_password).expect("bcrypt hash");
    let snowflake = crate::pool::pool_snowflake()
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let id = snowflake.next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_user (id, username, password_hash, full_name, is_active, \
         refresh_token_version, version, created_at, updated_at) \
         VALUES ($1, $2, $3, $4, true, 0, 0, $5, $5)",
    )
    .bind(id)
    .bind(username.to_lowercase())
    .bind(hash)
    .bind(username)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_user");
    id
}

/// 插一个 `t_user_role` 行（user_id + role + scope）。
#[allow(dead_code)]
pub async fn add_role(
    pool: &PgPool,
    user_id: i64,
    role: &str,
    scope_type: Option<&str>,
    scope_id: Option<i64>,
) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;

    let snowflake = crate::pool::pool_snowflake()
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let id = snowflake.next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_user_role (id, user_id, role, scope_type, scope_id, version, \
         created_at, updated_at) \
         VALUES ($1, $2, $3, $4, $5, 0, $6, $6)",
    )
    .bind(id)
    .bind(user_id)
    .bind(role)
    .bind(scope_type)
    .bind(scope_id)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_user_role");
    id
}

// ===========================================================================
// 工序 / 货架 / 工种映射 helpers（worker-pool / part 域用）
// ===========================================================================

/// 插一个 `t_process` 工序（INHOUSE 类别）。
#[allow(dead_code)]
pub async fn seed_process(pool: &PgPool, code: &str, name: &str) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;

    let snowflake = crate::pool::pool_snowflake()
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let id = snowflake.next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_process (id, code, name, category, sort_order, requires_approval, \
         version, created_at, updated_at) \
         VALUES ($1, $2, $3, 'INHOUSE', 0, false, 0, $4, $4)",
    )
    .bind(id)
    .bind(code)
    .bind(name)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_process");
    id
}

/// `t_work_type_process` 映射。
#[allow(dead_code)]
pub async fn link_work_type_to_process(pool: &PgPool, wt_id: i64, p_id: i64) {
    use hsh_erp_rust::infra::clock::now_naive;

    let snowflake = crate::pool::pool_snowflake()
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let id = snowflake.next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_work_type_process (id, work_type_id, process_id, sort_order, \
         version, created_at, updated_at) \
         VALUES ($1, $2, $3, 0, 0, $4, $4)",
    )
    .bind(id)
    .bind(wt_id)
    .bind(p_id)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_work_type_process");
}

/// `t_shelf_process` 映射。
#[allow(dead_code)]
pub async fn link_shelf_to_process(pool: &PgPool, s_id: i64, p_id: i64) {
    use hsh_erp_rust::infra::clock::now_naive;

    let snowflake = crate::pool::pool_snowflake()
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let id = snowflake.next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_shelf_process (id, shelf_id, process_id, sort_order, \
         version, created_at, updated_at) \
         VALUES ($1, $2, $3, 0, 0, $4, $4)",
    )
    .bind(id)
    .bind(s_id)
    .bind(p_id)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_shelf_process");
}

/// 插一个 t_shelf 行（code / name / zone）。
#[allow(dead_code)]
pub async fn insert_shelf(pool: &PgPool, code: &str, name: &str, zone: &str) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;

    let snowflake = crate::pool::pool_snowflake()
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let id = snowflake.next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_shelf (id, code, name, zone, is_active, display_order, version, \
         created_at, updated_at) \
         VALUES ($1, $2, $3, $4, true, 0, 0, $5, $5)",
    )
    .bind(id)
    .bind(code)
    .bind(name)
    .bind(zone)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_shelf");
    id
}

// ===========================================================================
// 工艺链 helpers（part 域 lifecycle / to_process / repair 测试用）
// ===========================================================================

/// 为指定 part 建一个最小工艺链（t_part_process_chain），并把 part.process_chain_id 绑回。
///
/// 返回 chain_id。
#[allow(dead_code)]
pub async fn create_chain_for_part(pool: &PgPool, part_id: i64) -> i64 {
    let snowflake = crate::pool::pool_snowflake()
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let chain_id = snowflake.next_id();
    sqlx::query(
        "INSERT INTO t_part_process_chain (id, name, version, created_at, created_by, \
         updated_at, updated_by) \
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

/// 在指定 chain 内创建 step（process_id + sort_order）。
#[allow(dead_code)]
pub async fn create_step(pool: &PgPool, chain_id: i64, process_id: i64, sort_order: i32) -> i64 {
    let snowflake = crate::pool::pool_snowflake()
        .lock()
        .unwrap_or_else(|p| p.into_inner());
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
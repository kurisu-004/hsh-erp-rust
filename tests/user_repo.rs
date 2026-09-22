//! user 域 repo 集成测试
//!
//! 总计 47 例：覆盖 iam/repo/sql.rs 17 个固有静态方法（happy path + error path）。
//!
//! 本文件为集成测试，承载 4 域 repo 测试用例；按域拆分会增加 fixture 复用成本，本仓库
//! 约定集成测试可豁免 1000 行上限。
//!
//! ## 测试并行注意
//! 进程级 test_pool 每次 fresh database（plan 2 2026-09-20），DB 间 schema
//! 完全独立，无需 Mutex 串行化（每个用例 fresh database，无 fixture 覆盖）。
//!
//! ## 事务迁移（2026-09-21）
//! 原 2 例 SqlxUoW commit / drop 语义测试已删除——`uow.rs` 全家删除后 UoW 不再存在，
//! 事务由 handler 层 `state.pool.begin()` 管；handler 层语义回归改由
//! `tests/iam_api.rs`（HTTP 契约测试，强回归网）承担。本文件保留 SQL/repo 层 47 例。
//!
//! 复用 `tests/common/mod.rs` 的 fixture（ensure_database_exists / test_pool / clean_db
//! / insert_user_with_password 等），与既有 tests/* 风格一致。

#[path = "common/mod.rs"]
mod common;

use chrono::NaiveDateTime;
use sqlx::PgPool;

use common::{ensure_database_exists, test_pool};

use hsh_erp_rust::infra::clock::now_naive;
use hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator;
// 2026-09-19 IAM 域合并：原 `user::repo` 重定向到 `iam::repo`，方法零 diff。
// 2026-09-22 重构 #2：sql.rs 拆为 sql/{user,user_role,menu,shelf}.rs free fn；
// 原 `user_sql::xxx` → `sql::user::xxx`（`UserInsert` / `UserPartialUpdate` 等入参 DTO 仍从
// `repo` re-export 取，与 handler 层 `state.pool.begin()` + `&mut *tx` 路径同构。
use hsh_erp_rust::modules::iam::repo::{
    UserInsert, UserPartialUpdate, UserRoleInsert,
};
use hsh_erp_rust::modules::iam::repo::sql::{
    menu as menu_sql, shelf as shelf_sql, user as user_sql, user_role as user_role_sql,
};

// ===========================================================================
// 全局串行化互斥：所有用例共享同一 DB。
// ===========================================================================

/// 进程级共享雪花生成器：每次 `SnowflakeIdGenerator::new(...)` 都把 sequence 重置为 0，
/// 同一毫秒内多次 seed 会撞 ID。共享同一生成器才能保证每个用例内多 ID 唯一。
/// 复用 `tests/common/mod.rs::pool_snowflake()` 的实例 + epoch + instance 配置。
fn snowflake() -> &'static std::sync::Mutex<SnowflakeIdGenerator> {
    common::pool_snowflake()
}

/// 用例开头固定两步：建库 → 连池（test_pool 每次 fresh database，无残留，无需清表）。
async fn setup() -> PgPool {
    ensure_database_exists().await;
    test_pool().await
}

// ===========================================================================
// UserRepo 测试 (24 例，覆盖 10 个固有方法)
// ===========================================================================

/// 直接 `INSERT` 一个最小可用的 user 行（不经过 `user_sql::create_user`）。
/// 便于 get_by_id / get_by_username 等读路径测试 seed 数据。
async fn seed_user(pool: &PgPool, username: &str, is_active: bool) -> i64 {
    use hsh_erp_rust::auth::password;
    let hash = password::hash("seed-password").expect("bcrypt");
    let id = snowflake().lock().unwrap().next_id();
    let now = now_naive();
    sqlx::query!(
        "INSERT INTO t_user (id, username, password_hash, full_name, is_active, \
         refresh_token_version, version, created_at, updated_at) \
         VALUES ($1, $2, $3, $4, $5, 0, 0, $6, $6)",
        id,
        username.to_lowercase(),
        hash,
        username,
        is_active,
        now,
    )
    .execute(pool)
    .await
    .expect("seed t_user");
    id
}

/// `user_sql::get_by_id`：命中（活跃用户）
#[tokio::test]
async fn get_by_id_returns_user_when_active() {
    let pool = setup().await;
    let id = seed_user(&pool, "alice", true).await;

    let u = user_sql::get_user_by_id(&pool, id)
        .await
        .expect("query")
        .expect("user must exist");
    assert_eq!(u.id, id);
    assert_eq!(u.username, "alice");
    assert!(u.is_active);
    assert!(u.deleted_at.is_none());
}

/// `user_sql::get_by_id`：不存在的 id 返回 None
#[tokio::test]
async fn get_by_id_returns_none_for_missing_id() {
    let pool = setup().await;
    let u = user_sql::get_user_by_id(&pool, 999_999_999_999)
        .await
        .expect("query");
    assert!(u.is_none(), "不存在的 id 应返回 None");
}

/// `user_sql::get_by_id`：软删除的用户被过滤
#[tokio::test]
async fn get_by_id_excludes_soft_deleted() {
    let pool = setup().await;
    let id = seed_user(&pool, "ghost", true).await;
    sqlx::query!(
        "UPDATE t_user SET deleted_at = $2 WHERE id = $1",
        id,
        now_naive()
    )
    .execute(&pool)
    .await
    .expect("soft delete");

    let u = user_sql::get_user_by_id(&pool, id).await.expect("query");
    assert!(u.is_none(), "软删用户应被排除");
}

/// `user_sql::get_by_username`：命中（按 lowercase 查询）
#[tokio::test]
async fn get_by_username_returns_active_user() {
    let pool = setup().await;
    let _id = seed_user(&pool, "Bob", true).await;
    // 调用方需 trim().to_lowercase()，repo 不做归一
    let u = user_sql::get_user_by_username(&pool, "bob")
        .await
        .expect("query")
        .expect("user must exist");
    // seed_user 已 to_lowercase，DB 内统一存小写（与既有 insert_user_with_password 一致）
    assert_eq!(u.username, "bob");
}

/// `user_sql::get_by_username`：repo 不做大小写归一
#[tokio::test]
async fn get_by_username_is_case_sensitive_in_repo() {
    let pool = setup().await;
    let _id = seed_user(&pool, "Bob", true).await;
    // seed_user 已 lowercase 存 "bob"，所以 query("BOB") 找不到
    let u = user_sql::get_user_by_username(&pool, "BOB")
        .await
        .expect("query");
    assert!(u.is_none(), "repo 不做归一，BOB != bob");
    // 但 query("bob") 能命中
    let u = user_sql::get_user_by_username(&pool, "bob")
        .await
        .expect("query");
    assert!(u.is_some(), "小写精确匹配应命中");
}

/// `user_sql::get_by_username`：不存在的 username 返回 None
#[tokio::test]
async fn get_by_username_returns_none_for_missing() {
    let pool = setup().await;
    let u = user_sql::get_user_by_username(&pool, "no-such-user")
        .await
        .expect("query");
    assert!(u.is_none());
}

/// `user_sql::list_with_filters`：空表
#[tokio::test]
async fn list_with_filters_empty_returns_empty() {
    let pool = setup().await;
    let rows = user_sql::list_users_with_filters(&pool, None, None, 50, 0)
        .await
        .expect("query");
    assert!(rows.is_empty());
}

/// `user_sql::list_with_filters`：username_like 模糊匹配
#[tokio::test]
async fn list_with_filters_username_like_filters() {
    let pool = setup().await;
    let _ = seed_user(&pool, "alice", true).await;
    let _ = seed_user(&pool, "alex", true).await;
    let _ = seed_user(&pool, "bob", true).await;

    let rows = user_sql::list_users_with_filters(&pool, Some("al"), None, 50, 0)
        .await
        .expect("query");
    let names: Vec<_> = rows.iter().map(|u| u.username.as_str()).collect();
    assert_eq!(rows.len(), 2);
    assert!(names.contains(&"alice") && names.contains(&"alex"));
    assert!(!names.contains(&"bob"));
}

/// `user_sql::list_with_filters`：is_active 过滤
#[tokio::test]
async fn list_with_filters_is_active_filters() {
    let pool = setup().await;
    let _ = seed_user(&pool, "active1", true).await;
    let _ = seed_user(&pool, "active2", true).await;
    let _ = seed_user(&pool, "inactive1", false).await;

    let rows = user_sql::list_users_with_filters(&pool, None, Some(true), 50, 0)
        .await
        .expect("query");
    assert_eq!(rows.len(), 2);
    let rows_inactive = user_sql::list_users_with_filters(&pool, None, Some(false), 50, 0)
        .await
        .expect("query");
    assert_eq!(rows_inactive.len(), 1);
    assert_eq!(rows_inactive[0].username, "inactive1");
}

/// `user_sql::list_with_filters`：limit + offset 分页
#[tokio::test]
async fn list_with_filters_pagination() {
    let pool = setup().await;
    for i in 0..5 {
        let _ = seed_user(&pool, &format!("user-{i:02}"), true).await;
    }
    let page1 = user_sql::list_users_with_filters(&pool, None, None, 2, 0)
        .await
        .expect("query");
    let page2 = user_sql::list_users_with_filters(&pool, None, None, 2, 2)
        .await
        .expect("query");
    let page3 = user_sql::list_users_with_filters(&pool, None, None, 2, 4)
        .await
        .expect("query");
    assert_eq!(page1.len(), 2);
    assert_eq!(page2.len(), 2);
    assert_eq!(page3.len(), 1);
}

/// `user_sql::list_with_filters`：按 created_at DESC, id DESC 排序
#[tokio::test]
async fn list_with_filters_orders_by_created_at_desc() {
    let pool = setup().await;
    let _ = seed_user(&pool, "first", true).await;
    // 等一毫秒确保 created_at 不同
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    let _ = seed_user(&pool, "second", true).await;

    let rows = user_sql::list_users_with_filters(&pool, None, None, 50, 0)
        .await
        .expect("query");
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].username, "second", "更新的应在前面");
    assert_eq!(rows[1].username, "first");
}

/// `user_sql::count_with_filters`：只数活跃
#[tokio::test]
async fn count_with_filters_counts_active_only() {
    let pool = setup().await;
    let _ = seed_user(&pool, "u1", true).await;
    let _ = seed_user(&pool, "u2", true).await;
    let _ = seed_user(&pool, "u3", false).await;

    let total = user_sql::count_users_with_filters(&pool, None, None)
        .await
        .expect("count");
    assert_eq!(total, 3);
    let active = user_sql::count_users_with_filters(&pool, None, Some(true))
        .await
        .expect("count");
    assert_eq!(active, 2);
}

/// `user_sql::count_with_filters`：username_like 过滤
#[tokio::test]
async fn count_with_filters_with_username_filter() {
    let pool = setup().await;
    let _ = seed_user(&pool, "alpha-1", true).await;
    let _ = seed_user(&pool, "alpha-2", true).await;
    let _ = seed_user(&pool, "beta-1", true).await;

    let count = user_sql::count_users_with_filters(&pool, Some("alpha"), None)
        .await
        .expect("count");
    assert_eq!(count, 2);
}

/// `user_sql::create_user`：INSERT 新用户，再 get_by_id 应拿到
#[tokio::test]
async fn create_inserts_new_user() {
    let pool = setup().await;
    let id = snowflake().lock().unwrap().next_id();
    let insert = UserInsert {
        id,
        username: "newuser".to_string(),
        password_hash: "hash".to_string(),
        full_name: "New User".to_string(),
        phone: Some("13800138000".to_string()),
        is_active: true,
        created_at: now_naive(),
        created_by: None,
    };
    user_sql::create_user(&pool, &insert).await.expect("create");

    let u = user_sql::get_user_by_id(&pool, id)
        .await
        .expect("query")
        .expect("hit");
    assert_eq!(u.username, "newuser");
    assert_eq!(u.full_name, "New User");
    assert_eq!(u.refresh_token_version, 0);
    assert_eq!(u.version, 0);
}

/// `user_sql::create_user`：username 撞唯一索引 → sqlx::Error
#[tokio::test]
async fn create_returns_error_on_duplicate_username() {
    let pool = setup().await;
    let _ = seed_user(&pool, "dup", true).await;

    let id = snowflake().lock().unwrap().next_id();
    let insert = UserInsert {
        id,
        username: "dup".to_string(),
        password_hash: "h".to_string(),
        full_name: "Dup".to_string(),
        phone: None,
        is_active: true,
        created_at: now_naive(),
        created_by: None,
    };
    let res = user_sql::create_user(&pool, &insert).await;
    assert!(res.is_err(), "重复 username 应失败");
}

/// `user_sql::update_user_partial`：更新 full_name
#[tokio::test]
async fn update_partial_updates_full_name() {
    let pool = setup().await;
    let id = seed_user(&pool, "alice", true).await;
    let affected = user_sql::update_user_partial(
        &pool,
        id,
        0, // version
        &UserPartialUpdate {
            full_name: Some("Alice New"),
            set_phone: false,
            phone: None,
            password_hash: None,
            is_active: None,
            when: now_naive(),
            updated_by: None,
        },
    )
    .await
    .expect("update");
    assert_eq!(affected, 1);
    let u = user_sql::get_user_by_id(&pool, id)
        .await
        .expect("query")
        .expect("hit");
    assert_eq!(u.full_name, "Alice New");
    assert_eq!(u.version, 1);
}

/// `user_sql::update_user_partial`：set_phone=true 清空 phone
#[tokio::test]
async fn update_partial_set_phone_flag_clears_phone() {
    let pool = setup().await;
    let id = seed_user(&pool, "alice", true).await;
    sqlx::query!(
        "UPDATE t_user SET phone = $2 WHERE id = $1",
        id,
        Some("13800138000")
    )
    .execute(&pool)
    .await
    .expect("seed phone");

    let affected = user_sql::update_user_partial(
        &pool,
        id,
        0,
        &UserPartialUpdate {
            full_name: None,
            set_phone: true, // 明确清空
            phone: None,     // None + set_phone=true → phone=NULL
            password_hash: None,
            is_active: None,
            when: now_naive(),
            updated_by: None,
        },
    )
    .await
    .expect("update");
    assert_eq!(affected, 1);
    let u = user_sql::get_user_by_id(&pool, id)
        .await
        .expect("query")
        .expect("hit");
    assert!(
        u.phone.is_none(),
        "set_phone=true + phone=None 应清空 phone"
    );
}

/// `user_sql::update_user_partial`：set_phone=false 保留 phone
#[tokio::test]
async fn update_partial_no_set_phone_keeps_existing() {
    let pool = setup().await;
    let id = seed_user(&pool, "alice", true).await;
    sqlx::query!(
        "UPDATE t_user SET phone = $2 WHERE id = $1",
        id,
        Some("13800138000")
    )
    .execute(&pool)
    .await
    .expect("seed phone");

    let affected = user_sql::update_user_partial(
        &pool,
        id,
        0,
        &UserPartialUpdate {
            full_name: None,
            set_phone: false, // 不动 phone
            phone: Some("13900139000"), // 即使传 phone 也不更新
            password_hash: None,
            is_active: None,
            when: now_naive(),
            updated_by: None,
        },
    )
    .await
    .expect("update");
    assert_eq!(affected, 1);
    let u = user_sql::get_user_by_id(&pool, id)
        .await
        .expect("query")
        .expect("hit");
    assert_eq!(u.phone.as_deref(), Some("13800138000"));
}

/// `user_sql::update_user_partial`：version 不匹配 → 0 行
#[tokio::test]
async fn update_partial_zero_rows_for_version_conflict() {
    let pool = setup().await;
    let id = seed_user(&pool, "alice", true).await;
    let affected = user_sql::update_user_partial(
        &pool,
        id,
        99, // 错误 version
        &UserPartialUpdate {
            full_name: Some("X"),
            set_phone: false,
            phone: None,
            password_hash: None,
            is_active: None,
            when: now_naive(),
            updated_by: None,
        },
    )
    .await
    .expect("update");
    assert_eq!(affected, 0, "version 不匹配应返回 0 行");
}

/// `user_sql::soft_delete_user`：软删成功 + 后续 get_by_id 不可见
#[tokio::test]
async fn soft_delete_sets_deleted_at_and_is_active_false() {
    let pool = setup().await;
    let id = seed_user(&pool, "alice", true).await;
    let affected = user_sql::soft_delete_user(&pool, id, 0, now_naive(), None)
        .await
        .expect("soft_delete");
    assert_eq!(affected, 1);

    // get_by_id 走 deleted_at IS NULL 过滤 → 已软删用户看不到
    let u = user_sql::get_user_by_id(&pool, id).await.expect("query");
    assert!(u.is_none(), "软删后 get_by_id 应返回 None");
    // 但 list_with_filters(include_deleted=false) 也过滤；用 raw SQL 验证 deleted_at 已置
    let raw = sqlx::query!(
        "SELECT deleted_at AS \"deleted_at!\", is_active AS \"is_active!\" FROM t_user WHERE id = $1",
        id
    )
    .fetch_one(&pool)
    .await
    .expect("raw query");
    // "deleted_at!" = NOT NULL;软删后必设值
    let _ = raw.deleted_at;
    assert!(!raw.is_active);
}

/// `user_sql::soft_delete_user`：version 不匹配 → 0 行
#[tokio::test]
async fn soft_delete_zero_rows_for_version_conflict() {
    let pool = setup().await;
    let id = seed_user(&pool, "alice", true).await;
    let affected = user_sql::soft_delete_user(&pool, id, 99, now_naive(), None)
        .await
        .expect("soft_delete");
    assert_eq!(affected, 0);
}

/// `user_sql::touch_login`：刷新 last_login_at
#[tokio::test]
async fn touch_login_updates_last_login_at() {
    let pool = setup().await;
    let id = seed_user(&pool, "alice", true).await;
    let when: NaiveDateTime = now_naive() + chrono::Duration::hours(1);
    user_sql::touch_user_last_login_at(&pool, id, when)
        .await
        .expect("touch_login");

    let raw = sqlx::query!(
        "SELECT last_login_at AS \"last_login_at?\" FROM t_user WHERE id = $1",
        id
    )
    .fetch_one(&pool)
    .await
    .expect("raw query");
    assert_eq!(raw.last_login_at, Some(when));
}

/// `user_sql::increment_refresh_token_version`：轮转成功
#[tokio::test]
async fn increment_refresh_token_version_rotates_token_version() {
    let pool = setup().await;
    let id = seed_user(&pool, "alice", true).await;
    let affected = user_sql::increment_user_refresh_token_version(&pool, id, 0, now_naive(), None)
        .await
        .expect("increment");
    assert_eq!(affected, 1);

    let raw = sqlx::query!(
        "SELECT refresh_token_version AS \"rv!\" FROM t_user WHERE id = $1",
        id
    )
    .fetch_one(&pool)
    .await
    .expect("raw query");
    assert_eq!(raw.rv, 1);
}

/// `user_sql::increment_refresh_token_version`：version 不匹配 → 0 行
#[tokio::test]
async fn increment_refresh_token_version_zero_rows_for_version_conflict() {
    let pool = setup().await;
    let id = seed_user(&pool, "alice", true).await;
    let affected = user_sql::increment_user_refresh_token_version(&pool, id, 99, now_naive(), None)
        .await
        .expect("increment");
    assert_eq!(affected, 0);
}

/// `user_sql::update_password_and_rotate`：同时改密 + 轮转
#[tokio::test]
async fn update_password_and_rotate_updates_hash_and_rotates() {
    let pool = setup().await;
    let id = seed_user(&pool, "alice", true).await;
    let affected =
        user_sql::update_user_password_and_rotate(&pool, id, 0, "new-hash", now_naive(), None)
            .await
            .expect("update");
    assert_eq!(affected, 1);

    let raw = sqlx::query!(
        "SELECT password_hash AS \"ph!\", refresh_token_version AS \"rv!\", version AS \"v!\" \
         FROM t_user WHERE id = $1",
        id
    )
    .fetch_one(&pool)
    .await
    .expect("raw query");
    assert_eq!(raw.ph, "new-hash");
    assert_eq!(raw.rv, 1);
    assert_eq!(raw.v, 1);
}

/// `user_sql::update_password_and_rotate`：version 不匹配 → 0 行
#[tokio::test]
async fn update_password_and_rotate_zero_rows_for_version_conflict() {
    let pool = setup().await;
    let id = seed_user(&pool, "alice", true).await;
    let affected = user_sql::update_user_password_and_rotate(
        &pool,
        id,
        99, // 错误 version
        "new-hash",
        now_naive(),
        None,
    )
    .await
    .expect("update");
    assert_eq!(affected, 0);
}

// ===========================================================================
// UserRoleRepo 测试 (11 例，覆盖 5 个固有方法)
// ===========================================================================

/// seed 一个 user_role（直插 SQL，绕开 UserRoleRepo）
async fn seed_role(
    pool: &PgPool,
    user_id: i64,
    role: &str,
    scope_type: Option<&str>,
    scope_id: Option<i64>,
) -> i64 {
    let id = snowflake().lock().unwrap().next_id();
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
    .expect("seed t_user_role");
    id
}

/// `user_role_sql::list_user_roles_by_user_id`：返回该用户全部活跃角色
#[tokio::test]
async fn list_user_roles_by_user_id_returns_active_roles() {
    let pool = setup().await;
    let uid = seed_user(&pool, "alice", true).await;
    let _ = seed_role(&pool, uid, "MANAGER", None, None).await;
    let _ = seed_role(&pool, uid, "CLERK", None, None).await;

    let rows = user_role_sql::list_user_roles_by_user_id(&pool, uid).await.expect("list");
    assert_eq!(rows.len(), 2);
    let roles: Vec<_> = rows.iter().map(|r| r.role.as_str()).collect();
    assert!(roles.contains(&"MANAGER") && roles.contains(&"CLERK"));
}

/// `user_role_sql::list_user_roles_by_user_id`：无角色用户返回空
#[tokio::test]
async fn list_user_roles_by_user_id_returns_empty_when_no_roles() {
    let pool = setup().await;
    let uid = seed_user(&pool, "lonely", true).await;
    let rows = user_role_sql::list_user_roles_by_user_id(&pool, uid).await.expect("list");
    assert!(rows.is_empty());
}

/// `user_role_sql::list_user_roles_by_user_id`：LEFT JOIN t_shelf 带出 shelf_code/shelf_name
#[tokio::test]
async fn list_user_roles_by_user_id_includes_shelf_code_and_name() {
    let pool = setup().await;
    let uid = seed_user(&pool, "shelfie", true).await;

    let shelf_id = snowflake().lock().unwrap().next_id();
    sqlx::query!(
        "INSERT INTO t_shelf (id, code, name, zone, is_active, display_order, version, \
         created_at, updated_at) \
         VALUES ($1, 'S-001', 'Shelf One', 'PRODUCTION', true, 0, 0, now(), now())",
        shelf_id,
    )
    .execute(&pool)
    .await
    .expect("seed t_shelf");

    let _ = seed_role(&pool, uid, "SHELF_ACCOUNT", Some("shelf"), Some(shelf_id)).await;

    let rows = user_role_sql::list_user_roles_by_user_id(&pool, uid).await.expect("list");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].shelf_code.as_deref(), Some("S-001"));
    assert_eq!(rows[0].shelf_name.as_deref(), Some("Shelf One"));
}

/// `user_role_sql::get_user_role_by_id`：命中
#[tokio::test]
async fn get_by_id_returns_role() {
    let pool = setup().await;
    let uid = seed_user(&pool, "alice", true).await;
    let rid = seed_role(&pool, uid, "MANAGER", None, None).await;

    let r = user_role_sql::get_user_role_by_id(&pool, rid)
        .await
        .expect("query")
        .expect("hit");
    assert_eq!(r.id, rid);
    assert_eq!(r.role, "MANAGER");
}

/// `user_role_sql::get_user_role_by_id`：不存在的 id
#[tokio::test]
async fn get_by_id_returns_none_for_missing() {
    let pool = setup().await;
    let r = user_role_sql::get_user_role_by_id(&pool, 999_999_999_999)
        .await
        .expect("query");
    assert!(r.is_none());
}

/// `user_role_sql::has_user_role_with_scope`：已存在重复
#[tokio::test]
async fn exists_same_scope_returns_true_for_dup() {
    let pool = setup().await;
    let uid = seed_user(&pool, "alice", true).await;
    let _ = seed_role(&pool, uid, "MANAGER", None, None).await;
    let dup = user_role_sql::has_user_role_with_scope(&pool, uid, "MANAGER", None, None)
        .await
        .expect("query");
    assert!(dup);
}

/// `user_role_sql::has_user_role_with_scope`：不重复
#[tokio::test]
async fn exists_same_scope_returns_false_when_no_dup() {
    let pool = setup().await;
    let uid = seed_user(&pool, "alice", true).await;
    let dup = user_role_sql::has_user_role_with_scope(&pool, uid, "MANAGER", None, None)
        .await
        .expect("query");
    assert!(!dup);
}

/// `user_role_sql::has_user_role_with_scope`：用 IS NOT DISTINCT FROM 处理 NULL
/// （(NULL, NULL) = (NULL, NULL) 在 SQL 里是 NULL，依赖 `=` 会漏判）
#[tokio::test]
async fn exists_same_scope_handles_null_scope_via_is_not_distinct_from() {
    let pool = setup().await;
    let uid = seed_user(&pool, "alice", true).await;
    // seed 一个 (None, None) 的 MANAGER
    let _ = seed_role(&pool, uid, "MANAGER", None, None).await;

    // 再用 (None, None) 查重——用 IS NOT DISTINCT FROM 才能命中
    let dup = user_role_sql::has_user_role_with_scope(&pool, uid, "MANAGER", None, None)
        .await
        .expect("query");
    assert!(dup, "IS NOT DISTINCT FROM 应让 NULL=NULL 视为 equal");
}

/// `user_role_sql::create_user_role`：INSERT 成功
#[tokio::test]
async fn create_inserts_new_role() {
    let pool = setup().await;
    let uid = seed_user(&pool, "alice", true).await;
    let rid = snowflake().lock().unwrap().next_id();
    let insert = UserRoleInsert {
        id: rid,
        user_id: uid,
        role: "INSPECTOR".to_string(),
        scope_type: None,
        scope_id: None,
        created_at: now_naive(),
        created_by: Some(uid),
    };
    user_role_sql::create_user_role(&pool, &insert).await.expect("create");

    let r = user_role_sql::get_user_role_by_id(&pool, rid)
        .await
        .expect("query")
        .expect("hit");
    assert_eq!(r.user_id, uid);
    assert_eq!(r.role, "INSPECTOR");
}

/// `user_role_sql::soft_delete_user_role`：软删成功 + 后续 `list_user_roles_by_user_id` 不见
#[tokio::test]
async fn soft_delete_marks_deleted_at() {
    let pool = setup().await;
    let uid = seed_user(&pool, "alice", true).await;
    let rid = seed_role(&pool, uid, "CLERK", None, None).await;
    let affected = user_role_sql::soft_delete_user_role(&pool, rid, 0, now_naive(), None)
        .await
        .expect("soft_delete");
    assert_eq!(affected, 1);

    let rows = user_role_sql::list_user_roles_by_user_id(&pool, uid).await.expect("list");
    assert_eq!(rows.len(), 0, "软删后 list_user_roles_by_user_id 应过滤");
}

/// `user_role_sql::soft_delete_user_role`：version 不匹配 → 0 行
#[tokio::test]
async fn soft_delete_returns_zero_rows_on_version_conflict() {
    let pool = setup().await;
    let uid = seed_user(&pool, "alice", true).await;
    let rid = seed_role(&pool, uid, "CLERK", None, None).await;
    let affected = user_role_sql::soft_delete_user_role(&pool, rid, 99, now_naive(), None)
        .await
        .expect("soft_delete");
    assert_eq!(affected, 0);
}

// ===========================================================================
// MenuRepo 测试 (3 例，覆盖 1 个固有方法)
// ===========================================================================

async fn seed_menu(pool: &PgPool, code: &str, sort_order: i32, is_active: bool) -> i64 {
    let id = snowflake().lock().unwrap().next_id();
    let now = now_naive();
    sqlx::query!(
        "INSERT INTO t_menu (id, parent_id, code, title, sort_order, is_active, \
         version, created_at, updated_at) \
         VALUES ($1, NULL, $2, $3, $4, $5, 0, $6, $6)",
        id,
        code,
        code,
        sort_order,
        is_active,
        now,
    )
    .execute(pool)
    .await
    .expect("seed t_menu");
    id
}

async fn link_role_menu(pool: &PgPool, role: &str, menu_id: i64) {
    let id = snowflake().lock().unwrap().next_id();
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
    .expect("seed t_role_menu");
}

/// `menu_sql::list_active_for_roles`：多个 role 共用同一菜单应去重（DISTINCT）
#[tokio::test]
async fn list_active_for_roles_returns_distinct_menus() {
    let pool = setup().await;
    let m = seed_menu(&pool, "shared-menu", 0, true).await;
    link_role_menu(&pool, "MANAGER", m).await;
    link_role_menu(&pool, "CLERK", m).await;

    let rows = menu_sql::list_active_menus_by_roles(&pool, &["MANAGER".into(), "CLERK".into()])
        .await
        .expect("list");
    assert_eq!(rows.len(), 1, "两个 role 共用应去重");
    assert_eq!(rows[0].code, "shared-menu");
}

/// `menu_sql::list_active_for_roles`：is_active=false 的菜单被排除
#[tokio::test]
async fn list_active_for_roles_excludes_inactive_menus() {
    let pool = setup().await;
    let active = seed_menu(&pool, "active", 0, true).await;
    let inactive = seed_menu(&pool, "inactive", 1, false).await;
    link_role_menu(&pool, "MANAGER", active).await;
    link_role_menu(&pool, "MANAGER", inactive).await;

    let rows = menu_sql::list_active_menus_by_roles(&pool, &["MANAGER".into()])
        .await
        .expect("list");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].code, "active");
}

/// `menu_sql::list_active_for_roles`：按 sort_order, code 排序
#[tokio::test]
async fn list_active_for_roles_ordered_by_sort_order_code() {
    let pool = setup().await;
    let m3 = seed_menu(&pool, "z-sort-3", 30, true).await;
    let m1 = seed_menu(&pool, "a-sort-1", 10, true).await;
    let m2 = seed_menu(&pool, "b-sort-2", 20, true).await;
    for m in [m1, m2, m3] {
        link_role_menu(&pool, "MANAGER", m).await;
    }

    let rows = menu_sql::list_active_menus_by_roles(&pool, &["MANAGER".into()])
        .await
        .expect("list");
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].code, "a-sort-1");
    assert_eq!(rows[1].code, "b-sort-2");
    assert_eq!(rows[2].code, "z-sort-3");
}

// ===========================================================================
// ShelfRepo 测试 (2 例)
// ===========================================================================

async fn seed_shelf(pool: &PgPool, code: &str, zone: &str) -> i64 {
    let id = snowflake().lock().unwrap().next_id();
    let now = now_naive();
    sqlx::query!(
        "INSERT INTO t_shelf (id, code, name, zone, is_active, display_order, version, \
         created_at, updated_at) \
         VALUES ($1, $2, $3, $4, true, 0, 0, $5, $5)",
        id,
        code,
        code,
        zone,
        now,
    )
    .execute(pool)
    .await
    .expect("seed t_shelf");
    id
}

/// `shelf_sql::get_by_id`：命中
#[tokio::test]
async fn get_by_id_returns_shelf() {
    let pool = setup().await;
    let sid = seed_shelf(&pool, "S-001", "PRODUCTION").await;
    let s = shelf_sql::get_shelf_by_id(&pool, sid)
        .await
        .expect("query")
        .expect("hit");
    assert_eq!(s.id, sid);
    assert_eq!(s.code, "S-001");
    assert_eq!(s.zone, "PRODUCTION");
}

/// `shelf_sql::get_by_id`：不存在的 id
#[tokio::test]
async fn shelf_get_by_id_returns_none_for_missing() {
    let pool = setup().await;
    let s = shelf_sql::get_shelf_by_id(&pool, 999_999_999_999)
        .await
        .expect("query");
    assert!(s.is_none());
}

// ===========================================================================
// 多表组合事务 (2 例)：直调 `sql::user_sql::xxx(&mut *tx, ...)` + `pool.begin()` 开 tx，
// 跨多 repo 写，最后 commit。与 handler 层 `state.pool.begin()` + `&mut *tx` 路径同构
// （2026-09-22 删 `PgIamRepo` 转发壳后的事务边界）。
// ===========================================================================

/// 手写 begin/commit：commit 后写入对外可见
#[tokio::test]
#[allow(clippy::explicit_auto_deref)] // `&mut *tx` 是 sqlx 借 `&mut PgConnection` 的标准模式
async fn create_user_then_add_role_then_list_persists_all() {
    let pool = setup().await;

    let mut tx = pool.begin().await.expect("begin");

    // 写 user
    let uid = snowflake().lock().unwrap().next_id();
    user_sql::create_user(&mut *tx, &UserInsert {
        id: uid,
        username: "atomic-user".to_string(),
        password_hash: "h".to_string(),
        full_name: "Atomic User".to_string(),
        phone: None,
        is_active: true,
        created_at: now_naive(),
        created_by: None,
    })
    .await
    .expect("create user");

    // 写 role
    let rid = snowflake().lock().unwrap().next_id();
    user_role_sql::create_user_role(&mut *tx, &UserRoleInsert {
        id: rid,
        user_id: uid,
        role: "MANAGER".to_string(),
        scope_type: None,
        scope_id: None,
        created_at: now_naive(),
        created_by: None,
    })
    .await
    .expect("create role");

    tx.commit().await.expect("commit");

    // 用独立 SQL 查应可见
    let u = user_sql::get_user_by_id(&pool, uid)
        .await
        .expect("query")
        .expect("hit");
    assert_eq!(u.username, "atomic-user");
    let rows = user_role_sql::list_user_roles_by_user_id(&pool, uid).await.expect("list");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].role, "MANAGER");
}

/// 手写 begin/commit：commit 后 user 软删生效（`list_user_roles_by_user_id` 只看 role.deleted_at）
#[tokio::test]
#[allow(clippy::explicit_auto_deref)]
async fn soft_delete_user_then_list_roles_returns_empty() {
    let pool = setup().await;

    // seed 一个 user + role
    let uid = seed_user(&pool, "toclose", true).await;
    let _ = seed_role(&pool, uid, "CLERK", None, None).await;

    let mut tx = pool.begin().await.expect("begin");
    {
        user_sql::soft_delete_user(&mut *tx, uid, 0, now_naive(), None)
            .await
            .expect("soft delete");
    }
    tx.commit().await.expect("commit");

    // 软删后再 list_user_roles_by_user_id —— 角色还在（list_user_roles_by_user_id 不 JOIN t_user），但 user 不可见
    let rows = user_role_sql::list_user_roles_by_user_id(&pool, uid).await.expect("list");
    assert_eq!(
        rows.len(),
        1,
        "list_user_roles_by_user_id 只看 t_user_role.deleted_at，与 user 软删无关"
    );
    let u = user_sql::get_user_by_id(&pool, uid).await.expect("query");
    assert!(u.is_none(), "user 已软删");
}

// ===========================================================================
// 事务边界 (2 例)：手写 begin + commit / drop 行为（替代原 SqlxIamUnitOfWork commit/drop 语义）
// ===========================================================================

/// 事务 commit 后写入持久化（独立连接可查到）
#[tokio::test]
#[allow(clippy::explicit_auto_deref)]
async fn transaction_commit_persists_writes() {
    let pool = setup().await;

    let mut tx = pool.begin().await.expect("begin");
    let uid = snowflake().lock().unwrap().next_id();
    user_sql::create_user(&mut *tx, &UserInsert {
        id: uid,
        username: "committed".to_string(),
        password_hash: "h".to_string(),
        full_name: "Committed".to_string(),
        phone: None,
        is_active: true,
        created_at: now_naive(),
        created_by: None,
    })
    .await
    .expect("create");
    tx.commit().await.expect("commit");

    // 同一 pool（连接）直接查应可见（commit 已让事务落库）
    let u = user_sql::get_user_by_id(&pool, uid)
        .await
        .expect("query")
        .expect("hit after commit");
    assert_eq!(u.username, "committed");
}

/// 事务在 commit 前 drop → 隐式回滚（写入不可见）
#[tokio::test]
#[allow(clippy::explicit_auto_deref)]
async fn transaction_drop_without_commit_rolls_back() {
    let pool = setup().await;

    let mut tx = pool.begin().await.expect("begin");
    let uid = snowflake().lock().unwrap().next_id();
    user_sql::create_user(&mut *tx, &UserInsert {
        id: uid,
        username: "rolledback".to_string(),
        password_hash: "h".to_string(),
        full_name: "RolledBack".to_string(),
        phone: None,
        is_active: true,
        created_at: now_naive(),
        created_by: None,
    })
    .await
    .expect("create");

    // 不调 commit，让 `tx` 在作用域结束时 drop —— sqlx::Transaction 的 Drop 语义 = ROLLBACK
    drop(tx);

    let u = user_sql::get_user_by_id(&pool, uid).await.expect("query");
    assert!(u.is_none(), "drop 未 commit → 隐式回滚 → 数据不可见");
}

// ===========================================================================
// 3 个补充集成测试
// ===========================================================================

/// `user_sql::update_user_partial`：同时改 password_hash（管理员重置密码不踢下线路径）
#[tokio::test]
async fn update_partial_changes_password_hash_without_rotate() {
    let pool = setup().await;
    let id = seed_user(&pool, "alice", true).await;
    let affected = user_sql::update_user_partial(
        &pool,
        id,
        0,
        &UserPartialUpdate {
            full_name: None,
            set_phone: false,
            phone: None,
            password_hash: Some("admin-new-hash"), // 管理员改密，不轮转 refresh_token_version
            is_active: None,
            when: now_naive(),
            updated_by: None,
        },
    )
    .await
    .expect("update");
    assert_eq!(affected, 1);
    let raw = sqlx::query!(
        "SELECT password_hash AS \"ph!\", refresh_token_version AS \"rv!\" FROM t_user WHERE id = $1",
        id
    )
    .fetch_one(&pool)
    .await
    .expect("raw query");
    assert_eq!(raw.ph, "admin-new-hash");
    assert_eq!(raw.rv, 0, "管理员改密不轮转 refresh_token_version");
}

/// `user_sql::update_user_partial`：同时改 is_active=false（管理员停用）
#[tokio::test]
async fn update_partial_changes_is_active() {
    let pool = setup().await;
    let id = seed_user(&pool, "alice", true).await;
    let affected = user_sql::update_user_partial(
        &pool,
        id,
        0,
        &UserPartialUpdate {
            full_name: None,
            set_phone: false,
            phone: None,
            password_hash: None,
            is_active: Some(false), // is_active=false
            when: now_naive(),
            updated_by: None,
        },
    )
    .await
    .expect("update");
    assert_eq!(affected, 1);
    let u = user_sql::get_user_by_id(&pool, id)
        .await
        .expect("query")
        .expect("hit");
    assert!(!u.is_active);
}

/// `user_role_sql::list_user_roles_by_user_id`：过滤软删的角色
#[tokio::test]
async fn list_user_roles_by_user_id_excludes_soft_deleted_roles() {
    let pool = setup().await;
    let uid = seed_user(&pool, "alice", true).await;
    let active_rid = seed_role(&pool, uid, "MANAGER", None, None).await;
    let deleted_rid = seed_role(&pool, uid, "CLERK", None, None).await;
    // 软删第二个
    sqlx::query!(
        "UPDATE t_user_role SET deleted_at = $2 WHERE id = $1",
        deleted_rid,
        now_naive()
    )
    .execute(&pool)
    .await
    .expect("soft delete role");

    let rows = user_role_sql::list_user_roles_by_user_id(&pool, uid).await.expect("list");
    assert_eq!(rows.len(), 1, "软删的角色应被过滤");
    assert_eq!(rows[0].id, active_rid);
}

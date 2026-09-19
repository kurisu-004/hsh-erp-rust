//! user 域 repo 集成测试 + UoW 语义测试
//!
//! 总计 49 例：
//! - 47 例覆盖 user/repo.rs 17 个固有静态方法（happy path + error path）
//! - 2 例 SqlxUoW commit/drop 语义
//!
//! 本文件为集成测试，承载 4 域 repo 测试用例 + UoW 语义测试；按域拆分会增加 fixture
//! 复用成本，本仓库约定集成测试可豁免 1000 行上限。
//!
//! ## 测试并行注意
//! 所有用例共享同一个 `postgres_rust_test` 库。多个 `#[tokio::test]` 并发跑时会相互
//! 覆盖 fixture。用一个进程级 `tokio::sync::Mutex` 在每个用例入口序列化对 DB 的写入。
//!
//! ## UoW 语义测试
//! - `sqlx_uow_commit_persists_writes` —— 通过 SqlxIamUowProvider.begin() 开 tx，
//!   经访问器写数据，commit() 后用独立 pool 查应可见；
//! - `sqlx_uow_drop_without_commit_rolls_back` —— 开 tx 后写数据但直接 drop
//!   （隐式回滚），用独立 pool 查应不可见。
//!
//! 复用 `tests/common/mod.rs` 的 fixture（ensure_database_exists / test_pool / clean_db
//! / insert_user_with_password 等），与既有 tests/* 风格一致。

#[path = "common/mod.rs"]
mod common;

use chrono::NaiveDateTime;
use sqlx::PgPool;
use tokio::sync::Mutex;

use common::{ensure_database_exists, test_pool};

use hsh_erp_rust::infra::clock::now_naive;
use hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator;
// 2026-09-19 IAM 域合并：原 `user::repo` / `user::uow` 重定向到 `iam::repo` / `iam::uow`，
// 类型重命名为 `SqlxIamUowProvider` / `IamUowProvider`（加 Iam 前缀）。
use hsh_erp_rust::modules::iam::repo::{
    MenuRepo, ShelfRepo, UserInsert, UserRepo, UserRoleInsert, UserRoleRepo,
};
use hsh_erp_rust::modules::iam::uow::{IamUowProvider, SqlxIamUowProvider};

// ===========================================================================
// 全局串行化互斥：所有用例共享同一 DB。
// ===========================================================================
static TEST_LOCK: Mutex<()> = Mutex::const_new(());

/// 进程级共享雪花生成器：每次 `SnowflakeIdGenerator::new(...)` 都把 sequence 重置为 0，
/// 同一毫秒内多次 seed 会撞 ID。共享同一生成器才能保证每个用例内多 ID 唯一。
/// 复用 `tests/common/mod.rs::pool_snowflake()` 的实例 + epoch + instance 配置。
fn snowflake() -> &'static std::sync::Mutex<SnowflakeIdGenerator> {
    common::pool_snowflake()
}

/// 用例开头固定三步：拿锁 → 建库 → 连池 + 迁移 → 清表。
///
/// 返回的 `MutexGuard` 必须绑到 `_guard` 一直活到用例结束。
async fn setup<'a>() -> (tokio::sync::MutexGuard<'a, ()>, PgPool) {
    let guard = TEST_LOCK.lock().await;
    ensure_database_exists().await;
    let pool = test_pool().await;
    // 仅清 auth 表，user_repo 测试只触这 5 张表
    sqlx::query(
        "TRUNCATE t_user, t_user_role, t_menu, t_role_menu, t_shelf RESTART IDENTITY CASCADE",
    )
    .execute(&pool)
    .await
    .expect("truncate auth tables");
    (guard, pool)
}

// ===========================================================================
// UserRepo 测试 (24 例，覆盖 10 个固有方法)
// ===========================================================================

/// 直接 `INSERT` 一个最小可用的 user 行（不经过 `UserRepo::create`）。
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

/// `UserRepo::get_by_id`：命中（活跃用户）
#[tokio::test]
async fn get_by_id_returns_user_when_active() {
    let (_g, pool) = setup().await;
    let id = seed_user(&pool, "alice", true).await;

    let u = UserRepo::get_by_id(&pool, id)
        .await
        .expect("query")
        .expect("user must exist");
    assert_eq!(u.id, id);
    assert_eq!(u.username, "alice");
    assert!(u.is_active);
    assert!(u.deleted_at.is_none());
}

/// `UserRepo::get_by_id`：不存在的 id 返回 None
#[tokio::test]
async fn get_by_id_returns_none_for_missing_id() {
    let (_g, pool) = setup().await;
    let u = UserRepo::get_by_id(&pool, 999_999_999_999)
        .await
        .expect("query");
    assert!(u.is_none(), "不存在的 id 应返回 None");
}

/// `UserRepo::get_by_id`：软删除的用户被过滤
#[tokio::test]
async fn get_by_id_excludes_soft_deleted() {
    let (_g, pool) = setup().await;
    let id = seed_user(&pool, "ghost", true).await;
    sqlx::query!(
        "UPDATE t_user SET deleted_at = $2 WHERE id = $1",
        id,
        now_naive()
    )
    .execute(&pool)
    .await
    .expect("soft delete");

    let u = UserRepo::get_by_id(&pool, id).await.expect("query");
    assert!(u.is_none(), "软删用户应被排除");
}

/// `UserRepo::get_by_username`：命中（按 lowercase 查询）
#[tokio::test]
async fn get_by_username_returns_active_user() {
    let (_g, pool) = setup().await;
    let _id = seed_user(&pool, "Bob", true).await;
    // 调用方需 trim().to_lowercase()，repo 不做归一
    let u = UserRepo::get_by_username(&pool, "bob")
        .await
        .expect("query")
        .expect("user must exist");
    // seed_user 已 to_lowercase，DB 内统一存小写（与既有 insert_user_with_password 一致）
    assert_eq!(u.username, "bob");
}

/// `UserRepo::get_by_username`：repo 不做大小写归一
#[tokio::test]
async fn get_by_username_is_case_sensitive_in_repo() {
    let (_g, pool) = setup().await;
    let _id = seed_user(&pool, "Bob", true).await;
    // seed_user 已 lowercase 存 "bob"，所以 query("BOB") 找不到
    let u = UserRepo::get_by_username(&pool, "BOB")
        .await
        .expect("query");
    assert!(u.is_none(), "repo 不做归一，BOB != bob");
    // 但 query("bob") 能命中
    let u = UserRepo::get_by_username(&pool, "bob")
        .await
        .expect("query");
    assert!(u.is_some(), "小写精确匹配应命中");
}

/// `UserRepo::get_by_username`：不存在的 username 返回 None
#[tokio::test]
async fn get_by_username_returns_none_for_missing() {
    let (_g, pool) = setup().await;
    let u = UserRepo::get_by_username(&pool, "no-such-user")
        .await
        .expect("query");
    assert!(u.is_none());
}

/// `UserRepo::list_with_filters`：空表
#[tokio::test]
async fn list_with_filters_empty_returns_empty() {
    let (_g, pool) = setup().await;
    let rows = UserRepo::list_with_filters(&pool, None, None, 50, 0)
        .await
        .expect("query");
    assert!(rows.is_empty());
}

/// `UserRepo::list_with_filters`：username_like 模糊匹配
#[tokio::test]
async fn list_with_filters_username_like_filters() {
    let (_g, pool) = setup().await;
    let _ = seed_user(&pool, "alice", true).await;
    let _ = seed_user(&pool, "alex", true).await;
    let _ = seed_user(&pool, "bob", true).await;

    let rows = UserRepo::list_with_filters(&pool, Some("al"), None, 50, 0)
        .await
        .expect("query");
    let names: Vec<_> = rows.iter().map(|u| u.username.as_str()).collect();
    assert_eq!(rows.len(), 2);
    assert!(names.contains(&"alice") && names.contains(&"alex"));
    assert!(!names.contains(&"bob"));
}

/// `UserRepo::list_with_filters`：is_active 过滤
#[tokio::test]
async fn list_with_filters_is_active_filters() {
    let (_g, pool) = setup().await;
    let _ = seed_user(&pool, "active1", true).await;
    let _ = seed_user(&pool, "active2", true).await;
    let _ = seed_user(&pool, "inactive1", false).await;

    let rows = UserRepo::list_with_filters(&pool, None, Some(true), 50, 0)
        .await
        .expect("query");
    assert_eq!(rows.len(), 2);
    let rows_inactive = UserRepo::list_with_filters(&pool, None, Some(false), 50, 0)
        .await
        .expect("query");
    assert_eq!(rows_inactive.len(), 1);
    assert_eq!(rows_inactive[0].username, "inactive1");
}

/// `UserRepo::list_with_filters`：limit + offset 分页
#[tokio::test]
async fn list_with_filters_pagination() {
    let (_g, pool) = setup().await;
    for i in 0..5 {
        let _ = seed_user(&pool, &format!("user-{i:02}"), true).await;
    }
    let page1 = UserRepo::list_with_filters(&pool, None, None, 2, 0)
        .await
        .expect("query");
    let page2 = UserRepo::list_with_filters(&pool, None, None, 2, 2)
        .await
        .expect("query");
    let page3 = UserRepo::list_with_filters(&pool, None, None, 2, 4)
        .await
        .expect("query");
    assert_eq!(page1.len(), 2);
    assert_eq!(page2.len(), 2);
    assert_eq!(page3.len(), 1);
}

/// `UserRepo::list_with_filters`：按 created_at DESC, id DESC 排序
#[tokio::test]
async fn list_with_filters_orders_by_created_at_desc() {
    let (_g, pool) = setup().await;
    let _ = seed_user(&pool, "first", true).await;
    // 等一毫秒确保 created_at 不同
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    let _ = seed_user(&pool, "second", true).await;

    let rows = UserRepo::list_with_filters(&pool, None, None, 50, 0)
        .await
        .expect("query");
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].username, "second", "更新的应在前面");
    assert_eq!(rows[1].username, "first");
}

/// `UserRepo::count_with_filters`：只数活跃
#[tokio::test]
async fn count_with_filters_counts_active_only() {
    let (_g, pool) = setup().await;
    let _ = seed_user(&pool, "u1", true).await;
    let _ = seed_user(&pool, "u2", true).await;
    let _ = seed_user(&pool, "u3", false).await;

    let total = UserRepo::count_with_filters(&pool, None, None)
        .await
        .expect("count");
    assert_eq!(total, 3);
    let active = UserRepo::count_with_filters(&pool, None, Some(true))
        .await
        .expect("count");
    assert_eq!(active, 2);
}

/// `UserRepo::count_with_filters`：username_like 过滤
#[tokio::test]
async fn count_with_filters_with_username_filter() {
    let (_g, pool) = setup().await;
    let _ = seed_user(&pool, "alpha-1", true).await;
    let _ = seed_user(&pool, "alpha-2", true).await;
    let _ = seed_user(&pool, "beta-1", true).await;

    let count = UserRepo::count_with_filters(&pool, Some("alpha"), None)
        .await
        .expect("count");
    assert_eq!(count, 2);
}

/// `UserRepo::create`：INSERT 新用户，再 get_by_id 应拿到
#[tokio::test]
async fn create_inserts_new_user() {
    let (_g, pool) = setup().await;
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
    UserRepo::create(&pool, &insert).await.expect("create");

    let u = UserRepo::get_by_id(&pool, id)
        .await
        .expect("query")
        .expect("hit");
    assert_eq!(u.username, "newuser");
    assert_eq!(u.full_name, "New User");
    assert_eq!(u.refresh_token_version, 0);
    assert_eq!(u.version, 0);
}

/// `UserRepo::create`：username 撞唯一索引 → sqlx::Error
#[tokio::test]
async fn create_returns_error_on_duplicate_username() {
    let (_g, pool) = setup().await;
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
    let res = UserRepo::create(&pool, &insert).await;
    assert!(res.is_err(), "重复 username 应失败");
}

/// `UserRepo::update_partial`：更新 full_name
#[tokio::test]
async fn update_partial_updates_full_name() {
    let (_g, pool) = setup().await;
    let id = seed_user(&pool, "alice", true).await;
    let affected = UserRepo::update_partial(
        &pool,
        id,
        0, // version
        Some("Alice New"),
        false, // set_phone
        None,  // phone
        None,  // password_hash
        None,  // is_active
        now_naive(),
        None, // updated_by
    )
    .await
    .expect("update");
    assert_eq!(affected, 1);
    let u = UserRepo::get_by_id(&pool, id)
        .await
        .expect("query")
        .expect("hit");
    assert_eq!(u.full_name, "Alice New");
    assert_eq!(u.version, 1);
}

/// `UserRepo::update_partial`：set_phone=true 清空 phone
#[tokio::test]
async fn update_partial_set_phone_flag_clears_phone() {
    let (_g, pool) = setup().await;
    let id = seed_user(&pool, "alice", true).await;
    sqlx::query!(
        "UPDATE t_user SET phone = $2 WHERE id = $1",
        id,
        Some("13800138000")
    )
    .execute(&pool)
    .await
    .expect("seed phone");

    let affected = UserRepo::update_partial(
        &pool,
        id,
        0,
        None, // full_name
        true, // set_phone：明确清空
        None, // phone: None + set_phone=true → phone=NULL
        None, // password_hash
        None, // is_active
        now_naive(),
        None,
    )
    .await
    .expect("update");
    assert_eq!(affected, 1);
    let u = UserRepo::get_by_id(&pool, id)
        .await
        .expect("query")
        .expect("hit");
    assert!(
        u.phone.is_none(),
        "set_phone=true + phone=None 应清空 phone"
    );
}

/// `UserRepo::update_partial`：set_phone=false 保留 phone
#[tokio::test]
async fn update_partial_no_set_phone_keeps_existing() {
    let (_g, pool) = setup().await;
    let id = seed_user(&pool, "alice", true).await;
    sqlx::query!(
        "UPDATE t_user SET phone = $2 WHERE id = $1",
        id,
        Some("13800138000")
    )
    .execute(&pool)
    .await
    .expect("seed phone");

    let affected = UserRepo::update_partial(
        &pool,
        id,
        0,
        None,
        false,               // set_phone=false：不动 phone
        Some("13900139000"), // 即使传 phone 也不更新
        None,
        None,
        now_naive(),
        None,
    )
    .await
    .expect("update");
    assert_eq!(affected, 1);
    let u = UserRepo::get_by_id(&pool, id)
        .await
        .expect("query")
        .expect("hit");
    assert_eq!(u.phone.as_deref(), Some("13800138000"));
}

/// `UserRepo::update_partial`：version 不匹配 → 0 行
#[tokio::test]
async fn update_partial_zero_rows_for_version_conflict() {
    let (_g, pool) = setup().await;
    let id = seed_user(&pool, "alice", true).await;
    let affected = UserRepo::update_partial(
        &pool,
        id,
        99, // 错误 version
        Some("X"),
        false,
        None,
        None,
        None,
        now_naive(),
        None,
    )
    .await
    .expect("update");
    assert_eq!(affected, 0, "version 不匹配应返回 0 行");
}

/// `UserRepo::soft_delete`：软删成功 + 后续 get_by_id 不可见
#[tokio::test]
async fn soft_delete_sets_deleted_at_and_is_active_false() {
    let (_g, pool) = setup().await;
    let id = seed_user(&pool, "alice", true).await;
    let affected = UserRepo::soft_delete(&pool, id, 0, now_naive(), None)
        .await
        .expect("soft_delete");
    assert_eq!(affected, 1);

    // get_by_id 走 deleted_at IS NULL 过滤 → 已软删用户看不到
    let u = UserRepo::get_by_id(&pool, id).await.expect("query");
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

/// `UserRepo::soft_delete`：version 不匹配 → 0 行
#[tokio::test]
async fn soft_delete_zero_rows_for_version_conflict() {
    let (_g, pool) = setup().await;
    let id = seed_user(&pool, "alice", true).await;
    let affected = UserRepo::soft_delete(&pool, id, 99, now_naive(), None)
        .await
        .expect("soft_delete");
    assert_eq!(affected, 0);
}

/// `UserRepo::touch_login`：刷新 last_login_at
#[tokio::test]
async fn touch_login_updates_last_login_at() {
    let (_g, pool) = setup().await;
    let id = seed_user(&pool, "alice", true).await;
    let when: NaiveDateTime = now_naive() + chrono::Duration::hours(1);
    UserRepo::touch_login(&pool, id, when)
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

/// `UserRepo::increment_refresh_token_version`：轮转成功
#[tokio::test]
async fn increment_refresh_token_version_rotates_token_version() {
    let (_g, pool) = setup().await;
    let id = seed_user(&pool, "alice", true).await;
    let affected = UserRepo::increment_refresh_token_version(&pool, id, 0, now_naive(), None)
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

/// `UserRepo::increment_refresh_token_version`：version 不匹配 → 0 行
#[tokio::test]
async fn increment_refresh_token_version_zero_rows_for_version_conflict() {
    let (_g, pool) = setup().await;
    let id = seed_user(&pool, "alice", true).await;
    let affected = UserRepo::increment_refresh_token_version(&pool, id, 99, now_naive(), None)
        .await
        .expect("increment");
    assert_eq!(affected, 0);
}

/// `UserRepo::update_password_and_rotate`：同时改密 + 轮转
#[tokio::test]
async fn update_password_and_rotate_updates_hash_and_rotates() {
    let (_g, pool) = setup().await;
    let id = seed_user(&pool, "alice", true).await;
    let affected =
        UserRepo::update_password_and_rotate(&pool, id, 0, "new-hash", now_naive(), None)
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

/// `UserRepo::update_password_and_rotate`：version 不匹配 → 0 行
#[tokio::test]
async fn update_password_and_rotate_zero_rows_for_version_conflict() {
    let (_g, pool) = setup().await;
    let id = seed_user(&pool, "alice", true).await;
    let affected = UserRepo::update_password_and_rotate(
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

/// `UserRoleRepo::list_by_user`：返回该用户全部活跃角色
#[tokio::test]
async fn list_by_user_returns_active_roles() {
    let (_g, pool) = setup().await;
    let uid = seed_user(&pool, "alice", true).await;
    let _ = seed_role(&pool, uid, "MANAGER", None, None).await;
    let _ = seed_role(&pool, uid, "CLERK", None, None).await;

    let rows = UserRoleRepo::list_by_user(&pool, uid).await.expect("list");
    assert_eq!(rows.len(), 2);
    let roles: Vec<_> = rows.iter().map(|r| r.role.as_str()).collect();
    assert!(roles.contains(&"MANAGER") && roles.contains(&"CLERK"));
}

/// `UserRoleRepo::list_by_user`：无角色用户返回空
#[tokio::test]
async fn list_by_user_returns_empty_when_no_roles() {
    let (_g, pool) = setup().await;
    let uid = seed_user(&pool, "lonely", true).await;
    let rows = UserRoleRepo::list_by_user(&pool, uid).await.expect("list");
    assert!(rows.is_empty());
}

/// `UserRoleRepo::list_by_user`：LEFT JOIN t_shelf 带出 shelf_code/shelf_name
#[tokio::test]
async fn list_by_user_includes_shelf_code_and_name() {
    let (_g, pool) = setup().await;
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

    let rows = UserRoleRepo::list_by_user(&pool, uid).await.expect("list");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].shelf_code.as_deref(), Some("S-001"));
    assert_eq!(rows[0].shelf_name.as_deref(), Some("Shelf One"));
}

/// `UserRoleRepo::get_by_id`：命中
#[tokio::test]
async fn get_by_id_returns_role() {
    let (_g, pool) = setup().await;
    let uid = seed_user(&pool, "alice", true).await;
    let rid = seed_role(&pool, uid, "MANAGER", None, None).await;

    let r = UserRoleRepo::get_by_id(&pool, rid)
        .await
        .expect("query")
        .expect("hit");
    assert_eq!(r.id, rid);
    assert_eq!(r.role, "MANAGER");
}

/// `UserRoleRepo::get_by_id`：不存在的 id
#[tokio::test]
async fn get_by_id_returns_none_for_missing() {
    let (_g, pool) = setup().await;
    let r = UserRoleRepo::get_by_id(&pool, 999_999_999_999)
        .await
        .expect("query");
    assert!(r.is_none());
}

/// `UserRoleRepo::exists_same_scope`：已存在重复
#[tokio::test]
async fn exists_same_scope_returns_true_for_dup() {
    let (_g, pool) = setup().await;
    let uid = seed_user(&pool, "alice", true).await;
    let _ = seed_role(&pool, uid, "MANAGER", None, None).await;
    let dup = UserRoleRepo::exists_same_scope(&pool, uid, "MANAGER", None, None)
        .await
        .expect("query");
    assert!(dup);
}

/// `UserRoleRepo::exists_same_scope`：不重复
#[tokio::test]
async fn exists_same_scope_returns_false_when_no_dup() {
    let (_g, pool) = setup().await;
    let uid = seed_user(&pool, "alice", true).await;
    let dup = UserRoleRepo::exists_same_scope(&pool, uid, "MANAGER", None, None)
        .await
        .expect("query");
    assert!(!dup);
}

/// `UserRoleRepo::exists_same_scope`：用 IS NOT DISTINCT FROM 处理 NULL
/// （(NULL, NULL) = (NULL, NULL) 在 SQL 里是 NULL，依赖 `=` 会漏判）
#[tokio::test]
async fn exists_same_scope_handles_null_scope_via_is_not_distinct_from() {
    let (_g, pool) = setup().await;
    let uid = seed_user(&pool, "alice", true).await;
    // seed 一个 (None, None) 的 MANAGER
    let _ = seed_role(&pool, uid, "MANAGER", None, None).await;

    // 再用 (None, None) 查重——用 IS NOT DISTINCT FROM 才能命中
    let dup = UserRoleRepo::exists_same_scope(&pool, uid, "MANAGER", None, None)
        .await
        .expect("query");
    assert!(dup, "IS NOT DISTINCT FROM 应让 NULL=NULL 视为 equal");
}

/// `UserRoleRepo::create`：INSERT 成功
#[tokio::test]
async fn create_inserts_new_role() {
    let (_g, pool) = setup().await;
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
    UserRoleRepo::create(&pool, &insert).await.expect("create");

    let r = UserRoleRepo::get_by_id(&pool, rid)
        .await
        .expect("query")
        .expect("hit");
    assert_eq!(r.user_id, uid);
    assert_eq!(r.role, "INSPECTOR");
}

/// `UserRoleRepo::soft_delete`：软删成功 + 后续 list_by_user 不见
#[tokio::test]
async fn soft_delete_marks_deleted_at() {
    let (_g, pool) = setup().await;
    let uid = seed_user(&pool, "alice", true).await;
    let rid = seed_role(&pool, uid, "CLERK", None, None).await;
    let affected = UserRoleRepo::soft_delete(&pool, rid, 0, now_naive(), None)
        .await
        .expect("soft_delete");
    assert_eq!(affected, 1);

    let rows = UserRoleRepo::list_by_user(&pool, uid).await.expect("list");
    assert_eq!(rows.len(), 0, "软删后 list_by_user 应过滤");
}

/// `UserRoleRepo::soft_delete`：version 不匹配 → 0 行
#[tokio::test]
async fn soft_delete_returns_zero_rows_on_version_conflict() {
    let (_g, pool) = setup().await;
    let uid = seed_user(&pool, "alice", true).await;
    let rid = seed_role(&pool, uid, "CLERK", None, None).await;
    let affected = UserRoleRepo::soft_delete(&pool, rid, 99, now_naive(), None)
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

/// `MenuRepo::list_active_for_roles`：多个 role 共用同一菜单应去重（DISTINCT）
#[tokio::test]
async fn list_active_for_roles_returns_distinct_menus() {
    let (_g, pool) = setup().await;
    let m = seed_menu(&pool, "shared-menu", 0, true).await;
    link_role_menu(&pool, "MANAGER", m).await;
    link_role_menu(&pool, "CLERK", m).await;

    let rows = MenuRepo::list_active_for_roles(&pool, &["MANAGER".into(), "CLERK".into()])
        .await
        .expect("list");
    assert_eq!(rows.len(), 1, "两个 role 共用应去重");
    assert_eq!(rows[0].code, "shared-menu");
}

/// `MenuRepo::list_active_for_roles`：is_active=false 的菜单被排除
#[tokio::test]
async fn list_active_for_roles_excludes_inactive_menus() {
    let (_g, pool) = setup().await;
    let active = seed_menu(&pool, "active", 0, true).await;
    let inactive = seed_menu(&pool, "inactive", 1, false).await;
    link_role_menu(&pool, "MANAGER", active).await;
    link_role_menu(&pool, "MANAGER", inactive).await;

    let rows = MenuRepo::list_active_for_roles(&pool, &["MANAGER".into()])
        .await
        .expect("list");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].code, "active");
}

/// `MenuRepo::list_active_for_roles`：按 sort_order, code 排序
#[tokio::test]
async fn list_active_for_roles_ordered_by_sort_order_code() {
    let (_g, pool) = setup().await;
    let m3 = seed_menu(&pool, "z-sort-3", 30, true).await;
    let m1 = seed_menu(&pool, "a-sort-1", 10, true).await;
    let m2 = seed_menu(&pool, "b-sort-2", 20, true).await;
    for m in [m1, m2, m3] {
        link_role_menu(&pool, "MANAGER", m).await;
    }

    let rows = MenuRepo::list_active_for_roles(&pool, &["MANAGER".into()])
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

/// `ShelfRepo::get_by_id`：命中
#[tokio::test]
async fn get_by_id_returns_shelf() {
    let (_g, pool) = setup().await;
    let sid = seed_shelf(&pool, "S-001", "PRODUCTION").await;
    let s = ShelfRepo::get_by_id(&pool, sid)
        .await
        .expect("query")
        .expect("hit");
    assert_eq!(s.id, sid);
    assert_eq!(s.code, "S-001");
    assert_eq!(s.zone, "PRODUCTION");
}

/// `ShelfRepo::get_by_id`：不存在的 id
#[tokio::test]
async fn shelf_get_by_id_returns_none_for_missing() {
    let (_g, pool) = setup().await;
    let s = ShelfRepo::get_by_id(&pool, 999_999_999_999)
        .await
        .expect("query");
    assert!(s.is_none());
}

// ===========================================================================
// 多表组合事务 (2 例)：经 SqlxIamUowProvider 开 tx，跨多 repo 写，最后 commit。
// ===========================================================================

/// `SqlxIamUowProvider`：commit 后写入对外可见
#[tokio::test]
async fn create_user_then_add_role_then_list_persists_all() {
    let (_g, pool) = setup().await;

    let provider = SqlxIamUowProvider::new(pool.clone());
    let mut uow = provider.begin().await.expect("begin");

    // 写 user
    let uid = snowflake().lock().unwrap().next_id();
    uow.user_repo()
        .create(&UserInsert {
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
    uow.user_role_repo()
        .create(&UserRoleInsert {
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

    // commit（消费 Box<Self>）
    uow.commit().await.expect("commit");

    // 用独立 SQL 查应可见
    let u = UserRepo::get_by_id(&pool, uid)
        .await
        .expect("query")
        .expect("hit");
    assert_eq!(u.username, "atomic-user");
    let rows = UserRoleRepo::list_by_user(&pool, uid).await.expect("list");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].role, "MANAGER");
}

/// `SqlxIamUowProvider`：commit 后不存在的 user 不能通过 list_by_user 看到软删 + role 关联
#[tokio::test]
async fn soft_delete_user_then_list_roles_returns_empty() {
    let (_g, pool) = setup().await;

    // seed 一个 user + role
    let uid = seed_user(&pool, "toclose", true).await;
    let _ = seed_role(&pool, uid, "CLERK", None, None).await;

    let provider = SqlxIamUowProvider::new(pool.clone());
    let mut uow = provider.begin().await.expect("begin");

    uow.user_repo()
        .soft_delete(uid, 0, now_naive(), None)
        .await
        .expect("soft delete");

    uow.commit().await.expect("commit");

    // 软删后再 list_by_user —— 角色还在（list_by_user 不 JOIN t_user），但 user 不可见
    let rows = UserRoleRepo::list_by_user(&pool, uid).await.expect("list");
    assert_eq!(
        rows.len(),
        1,
        "list_by_user 只看 t_user_role.deleted_at，与 user 软删无关"
    );
    let u = UserRepo::get_by_id(&pool, uid).await.expect("query");
    assert!(u.is_none(), "user 已软删");
}

// ===========================================================================
// UoW 语义 (2 例)：commit / drop 行为
// ===========================================================================

/// `SqlxIamUnitOfWork::commit()` 后写入持久化（独立连接可查到）
#[tokio::test]
async fn sqlx_uow_commit_persists_writes() {
    let (_g, pool) = setup().await;

    let provider = SqlxIamUowProvider::new(pool.clone());
    let mut uow = provider.begin().await.expect("begin");
    let uid = snowflake().lock().unwrap().next_id();
    uow.user_repo()
        .create(&UserInsert {
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
    uow.commit().await.expect("commit");

    // 同一 pool（连接）直接查应可见（commit 已让事务落库）
    let u = UserRepo::get_by_id(&pool, uid)
        .await
        .expect("query")
        .expect("hit after commit");
    assert_eq!(u.username, "committed");
}

/// `SqlxIamUnitOfWork` 在不 commit 时 drop → 隐式回滚（写入不可见）
#[tokio::test]
async fn sqlx_uow_drop_without_commit_rolls_back() {
    let (_g, pool) = setup().await;

    let provider = SqlxIamUowProvider::new(pool.clone());
    let mut uow = provider.begin().await.expect("begin");
    let uid = snowflake().lock().unwrap().next_id();
    uow.user_repo()
        .create(&UserInsert {
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
    // 不调 commit，直接 drop —— sqlx::Transaction 的 Drop 语义 = ROLLBACK
    drop(uow);

    let u = UserRepo::get_by_id(&pool, uid).await.expect("query");
    assert!(u.is_none(), "drop 未 commit → 隐式回滚 → 数据不可见");
}

// ===========================================================================
// 3 个补充集成测试（计数对齐 47 + 2 = 49）
// ===========================================================================

/// `UserRepo::update_partial`：同时改 password_hash（管理员重置密码不踢下线路径）
#[tokio::test]
async fn update_partial_changes_password_hash_without_rotate() {
    let (_g, pool) = setup().await;
    let id = seed_user(&pool, "alice", true).await;
    let affected = UserRepo::update_partial(
        &pool,
        id,
        0,
        None,
        false,
        None,
        Some("admin-new-hash"), // password_hash：管理员改密，不轮转 refresh_token_version
        None,
        now_naive(),
        None,
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

/// `UserRepo::update_partial`：同时改 is_active=false（管理员停用）
#[tokio::test]
async fn update_partial_changes_is_active() {
    let (_g, pool) = setup().await;
    let id = seed_user(&pool, "alice", true).await;
    let affected = UserRepo::update_partial(
        &pool,
        id,
        0,
        None,
        false,
        None,
        None,
        Some(false), // is_active=false
        now_naive(),
        None,
    )
    .await
    .expect("update");
    assert_eq!(affected, 1);
    let u = UserRepo::get_by_id(&pool, id)
        .await
        .expect("query")
        .expect("hit");
    assert!(!u.is_active);
}

/// `UserRoleRepo::list_by_user`：过滤软删的角色
#[tokio::test]
async fn list_by_user_excludes_soft_deleted_roles() {
    let (_g, pool) = setup().await;
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

    let rows = UserRoleRepo::list_by_user(&pool, uid).await.expect("list");
    assert_eq!(rows.len(), 1, "软删的角色应被过滤");
    assert_eq!(rows[0].id, active_rid);
}

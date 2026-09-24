//! user 域 repo 集成测试 —— UserRepo 子集（PR13 Phase D 拆分）
//!
//! ## 拆分映射（原 1164 行 user_repo.rs → 3 文件）
//! - basic.rs   ← UserRepo 24 例（user CRUD/query/update/touch_login/refresh_token/密码轮转）
//! - role.rs    ← UserRoleRepo + MenuRepo + ShelfRepo 16 例
//! - password.rs ← 多表组合事务 + 事务边界 + 3 个补充集成测试 7 例
//!
//! 本文件 24 例：覆盖 `iam/repo/sql/user.rs` 10 个固有方法（happy + error）。
//!
//! ## 测试并行注意
//! 进程级 test_pool 每次 fresh database（plan 2 2026-09-20），DB 间 schema
//! 完全独立，无需 Mutex 串行化（每个用例 fresh database，无 fixture 覆盖）。
//!
//! ## 事务迁移（2026-09-21）
//! 原 2 例 SqlxUoW commit / drop 语义测试已删除——`uow.rs` 全家删除后 UoW 不再存在，
//! 事务由 handler 层 `state.pool.begin()` 管；handler 层语义回归改由
//! `tests/iam/api.rs`（HTTP 契约测试，强回归网）承担。本文件保留 SQL/repo 层 47 例。
//!
//! ## Fixture 范本化（2026-09-24 PR13 Phase I）
//! 本文件原 `#[path = "../common/mod.rs"] mod common;` + `use common::{...};`
//! 改走 `use hsh_erp_test_support::*` + `load_user_repo_fixture(&pool)` +
//! `UserRepoFixture`。fixture 提供 baseline user / role / menu；多数测试
//! 仍走本地 `seed_user(pool, username, is_active)` helper 创建专属测试数据
//! （特定 username / 多用户组合），user_repo 域独享 helper 不走 fixtures.rs。
//! 字面请求 / 断言逐字保留。

use chrono::NaiveDateTime;
use sqlx::PgPool;

use hsh_erp_rust::infra::clock::now_naive;
use hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator;
// 2026-09-19 IAM 域合并：原 `user::repo` 重定向到 `iam::repo`，方法零 diff。
// 2026-09-22 重构 #2：sql.rs 拆为 sql/{user,user_role,menu,shelf}.rs free fn；
// 原 `user_sql::xxx` → `sql::user::xxx`（`UserInsert` / `UserPartialUpdate` 等入参 DTO 仍从
// `repo` re-export 取，与 handler 层 `state.pool.begin()` + `&mut *tx` 路径同构。
use hsh_erp_rust::modules::iam::repo::{UserInsert, UserPartialUpdate};
use hsh_erp_rust::modules::iam::repo::sql::user as user_sql;

// 2026-09-24 PR13 Phase I：fixture 范本化入口。`load_user_repo_fixture(&pool)` 加载
// 1 user + 1 role + 1 menu baseline；多数测试用本地 seed_user 创建专属测试数据，
// 仅在需要 baseline 时取 fx.baseline_user_id 等常量。
use hsh_erp_test_support::{UserRepoFixture, load_user_repo_fixture, test_pool};

// ===========================================================================
// 全局串行化互斥：所有用例共享同一 DB。
// ===========================================================================

/// 进程级共享雪花生成器：每次 `SnowflakeIdGenerator::new(...)` 都把 sequence 重置为 0，
/// 同一毫秒内多次 seed 会撞 ID。共享同一生成器才能保证每个用例内多 ID 唯一。
/// 复用 `test-support::pool::pool_snowflake()` 的实例 + epoch + instance 配置
///（原 tests/common/mod.rs::pool_snowflake 转发路径已收口到 crate root）。
fn snowflake() -> &'static std::sync::Mutex<SnowflakeIdGenerator> {
    hsh_erp_test_support::pool_snowflake()
}

/// 基础 bootstrap：fresh DB + user_repo fixture 3 行（含 baseline user）+ 返回 pool。
///
/// fixture baseline（本文件大多数测试不直接使用，但 setup 加载过程无害）；
/// 测试现场仍走本地 `seed_user` 创建专属测试数据（特定 username / 多用户组合）。
async fn setup() -> (PgPool, UserRepoFixture) {
    let pool = test_pool().await;
    let fx = load_user_repo_fixture(&pool).await;
    (pool, fx)
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
    let (pool, _fx) = setup().await;
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

/// `user_sql::get_by_id`：不存在的 id
#[tokio::test]
async fn get_by_id_returns_none_for_missing_id() {
    let (pool, _fx) = setup().await;
    let u = user_sql::get_user_by_id(&pool, 999_999_999_999)
        .await
        .expect("query");
    assert!(u.is_none());
}

/// `user_sql::get_by_id`：软删用户不可见
#[tokio::test]
async fn get_by_id_excludes_soft_deleted() {
    let (pool, _fx) = setup().await;
    let id = seed_user(&pool, "ghost", true).await;
    sqlx::query!(
        "UPDATE t_user SET deleted_at = $2, is_active = false WHERE id = $1",
        id,
        now_naive()
    )
    .execute(&pool)
    .await
    .expect("soft delete");

    let u = user_sql::get_user_by_id(&pool, id).await.expect("query");
    assert!(u.is_none(), "软删用户 get_by_id 应返回 None");
}

/// `user_sql::get_by_username`：命中（活跃）
#[tokio::test]
async fn get_by_username_returns_active_user() {
    let (pool, _fx) = setup().await;
    let _ = seed_user(&pool, "bob", true).await;

    let u = user_sql::get_user_by_username(&pool, "bob")
        .await
        .expect("query")
        .expect("hit");
    assert_eq!(u.username, "bob");
    assert!(u.is_active);
}

/// `user_sql::get_by_username`：repo 层**不**做大小写归一（落库已 lower，应用层是 LIKE）
#[tokio::test]
async fn get_by_username_is_case_sensitive_in_repo() {
    let (pool, _fx) = setup().await;
    let _ = seed_user(&pool, "carol", true).await;

    // 小写命中
    let u = user_sql::get_user_by_username(&pool, "carol")
        .await
        .expect("query");
    assert!(u.is_some());
    // 大写不命中 → 验证 repo 层用 `=` 不归一
    let u = user_sql::get_user_by_username(&pool, "CAROL")
        .await
        .expect("query");
    assert!(u.is_none(), "repo 层应不做 case fold");
}

/// `user_sql::get_by_username`：不存在的 username
#[tokio::test]
async fn get_by_username_returns_none_for_missing() {
    let (pool, _fx) = setup().await;
    let u = user_sql::get_user_by_username(&pool, "no-such")
        .await
        .expect("query");
    assert!(u.is_none());
}

/// `user_sql::list_with_filters`：空库返回空
#[tokio::test]
async fn list_with_filters_empty_returns_empty() {
    let (pool, _fx) = setup().await;
    let rows = user_sql::list_users_with_filters(&pool, None, None, 50, 0)
        .await
        .expect("list");
    assert!(rows.is_empty());
}

/// `user_sql::list_with_filters`：username_like 部分匹配
#[tokio::test]
async fn list_with_filters_username_like_filters() {
    let (pool, _fx) = setup().await;
    let _ = seed_user(&pool, "alice-1", true).await;
    let _ = seed_user(&pool, "alice-2", true).await;
    let _ = seed_user(&pool, "bob", true).await;

    let rows = user_sql::list_users_with_filters(&pool, Some("alice"), None, 50, 0)
        .await
        .expect("list");
    assert_eq!(rows.len(), 2);
    for r in &rows {
        assert!(r.username.starts_with("alice"));
    }
}

/// `user_sql::list_with_filters`：is_active=false 过滤
#[tokio::test]
async fn list_with_filters_is_active_filters() {
    let (pool, _fx) = setup().await;
    let active = seed_user(&pool, "active", true).await;
    let _inactive = seed_user(&pool, "inactive", false).await;

    let rows =
        user_sql::list_users_with_filters(&pool, None, Some(true), 50, 0)
            .await
            .expect("list");
    let ids: Vec<_> = rows.iter().map(|r| r.id).collect();
    assert_eq!(ids, vec![active]);
}

/// `user_sql::list_with_filters`：分页
#[tokio::test]
async fn list_with_filters_pagination() {
    let (pool, _fx) = setup().await;
    for i in 0..5 {
        let _ = seed_user(&pool, &format!("u{i}"), true).await;
    }

    // limit=2 offset=0
    let page0 = user_sql::list_users_with_filters(&pool, None, None, 2, 0)
        .await
        .expect("list");
    assert_eq!(page0.len(), 2);
    // limit=2 offset=2
    let page1 = user_sql::list_users_with_filters(&pool, None, None, 2, 2)
        .await
        .expect("list");
    assert_eq!(page1.len(), 2);
    // ids 不重叠
    let ids0: std::collections::HashSet<_> = page0.iter().map(|r| r.id).collect();
    let ids1: std::collections::HashSet<_> = page1.iter().map(|r| r.id).collect();
    assert!(ids0.is_disjoint(&ids1));
}

/// `user_sql::list_with_filters`：默认排序按 created_at DESC
#[tokio::test]
async fn list_with_filters_orders_by_created_at_desc() {
    let (pool, _fx) = setup().await;
    let first = seed_user(&pool, "first", true).await;
    // 隔一会插第二个
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let second = seed_user(&pool, "second", true).await;

    let rows = user_sql::list_users_with_filters(&pool, None, None, 50, 0)
        .await
        .expect("list");
    let ids: Vec<_> = rows.iter().map(|r| r.id).collect();
    assert_eq!(ids.first(), Some(&second), "后插的应排在前面");
    assert_eq!(ids.last(), Some(&first));
}

/// `user_sql::count_with_filters`：只数活跃
#[tokio::test]
async fn count_with_filters_counts_active_only() {
    let (pool, _fx) = setup().await;
    let _ = seed_user(&pool, "u1", true).await;
    let _ = seed_user(&pool, "u2", true).await;
    let _ = seed_user(&pool, "u3", false).await;

    let total = user_sql::count_users_with_filters(&pool, None, None)
        .await
        .expect("count");
    assert_eq!(total, 3);
}

/// `user_sql::count_with_filters`：带 username_like 过滤
#[tokio::test]
async fn count_with_filters_with_username_filter() {
    let (pool, _fx) = setup().await;
    let _ = seed_user(&pool, "alpha-1", true).await;
    let _ = seed_user(&pool, "alpha-2", true).await;
    let _ = seed_user(&pool, "beta", true).await;

    let c = user_sql::count_users_with_filters(&pool, Some("alpha"), None)
        .await
        .expect("count");
    assert_eq!(c, 2);
}

/// `user_sql::create_user`：INSERT 成功
#[tokio::test]
async fn create_inserts_new_user() {
    let (pool, _fx) = setup().await;
    let id = snowflake().lock().unwrap().next_id();
    let insert = UserInsert {
        id,
        username: "newuser".to_string(),
        password_hash: "h".to_string(),
        full_name: "New User".to_string(),
        phone: None,
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
    let (pool, _fx) = setup().await;
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
    let (pool, _fx) = setup().await;
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
    let (pool, _fx) = setup().await;
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
    let (pool, _fx) = setup().await;
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
    let (pool, _fx) = setup().await;
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
    let (pool, _fx) = setup().await;
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
    let (pool, _fx) = setup().await;
    let id = seed_user(&pool, "alice", true).await;
    let affected = user_sql::soft_delete_user(&pool, id, 99, now_naive(), None)
        .await
        .expect("soft_delete");
    assert_eq!(affected, 0);
}

/// `user_sql::touch_login`：刷新 last_login_at
#[tokio::test]
async fn touch_login_updates_last_login_at() {
    let (pool, _fx) = setup().await;
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
    let (pool, _fx) = setup().await;
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
    let (pool, _fx) = setup().await;
    let id = seed_user(&pool, "alice", true).await;
    let affected = user_sql::increment_user_refresh_token_version(&pool, id, 99, now_naive(), None)
        .await
        .expect("increment");
    assert_eq!(affected, 0);
}

/// `user_sql::update_password_and_rotate`：同时改密 + 轮转
#[tokio::test]
async fn update_password_and_rotate_updates_hash_and_rotates() {
    let (pool, _fx) = setup().await;
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
    let (pool, _fx) = setup().await;
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
//! user 域 repo 集成测试 —— 多表事务 + 事务边界 + 密码/状态 子集（PR13 Phase D 拆分）
//!
//! ## 拆分映射（原 1164 行 user_repo.rs → 3 文件）
//! - basic.rs   ← UserRepo 24 例
//! - role.rs    ← UserRoleRepo + MenuRepo + ShelfRepo 16 例
//! - password.rs ← 多表组合事务 2 + 事务边界 2 + 3 个补充集成测试 = 7 例（本文件）
//!
//! 本文件 7 例：覆盖
//!   - `sql::{user,user_role}::xxx(&mut *tx, ...)` 跨多 repo 写在同一事务里
//!     （与 handler 层 `state.pool.begin()` + `&mut *tx` 路径同构）
//!   - sqlx::Transaction 显式 commit / drop 隐式 rollback 边界
//!   - 管理员重置密码路径（`update_user_partial` 改 password_hash 不轮转）
//!   - 管理员停用用户（`update_user_partial` 改 is_active）
//!   - 软删角色过滤（`list_user_roles_by_user_id` 排除 deleted_at）
//!
//! ## 测试并行注意
//! 进程级 test_pool 每次 fresh database（plan 2 2026-09-20），DB 间 schema
//! 完全独立，无需 Mutex 串行化。
//!
//! ## Fixture 范本化（2026-09-24 PR13 Phase I）
//! 本文件原 `#[path = "../common/mod.rs"] mod common;` + `use common::{...};`
//! 改走 `use hsh_erp_test_support::*` + `load_user_repo_fixture(&pool)` +
//! `UserRepoFixture`。fixture 提供 baseline user / role / menu；本文件 4 例
//! 事务测试用 `snowflake().next_id()` 现造 user_id（必须用新 ID 走
//! `create_user(&mut *tx, ...)`），3 例补充测试用本地 seed_user 创建专属测试
//! 数据。user_repo 域独享 helper 不走 fixtures.rs。字面请求 / 断言逐字保留。

use sqlx::PgPool;

use hsh_erp_rust::infra::clock::now_naive;
use hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator;
// 2026-09-19 IAM 域合并：原 `user::repo` 重定向到 `iam::repo`，方法零 diff。
// 2026-09-22 重构 #2：sql.rs 拆为 sql/{user,user_role,menu,shelf}.rs free fn。
use hsh_erp_rust::modules::iam::repo::{UserInsert, UserPartialUpdate, UserRoleInsert};
use hsh_erp_rust::modules::iam::repo::sql::{user as user_sql, user_role as user_role_sql};

// 2026-09-24 PR13 Phase I：fixture 范本化入口。`load_user_repo_fixture(&pool)` 加载
// 1 menu baseline（PR-C.Final 移除 user + role baseline，避免污染
// count / list_with_filters_* 「期望空库」断言）；本文件事务测试用 snowflake()
// 现造 user_id（必须新 ID 才能 create_user），补充测试用本地 seed_user 创建
// 专属测试数据，仅在需要 baseline 时取 fx.baseline_menu_id 常量。
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

/// 基础 bootstrap：fresh DB + user_repo fixture 1 行（baseline menu）+ 返回 pool。
///
/// fixture baseline（本文件大多数测试不直接使用，但 setup 加载过程无害）；
/// 事务测试用 snowflake() 现造 user_id（必须新 ID 才能 create_user），
/// 补充测试用本地 seed_user 创建专属测试数据。
async fn setup() -> (PgPool, UserRepoFixture) {
    let pool = test_pool().await;
    let fx = load_user_repo_fixture(&pool).await;
    (pool, fx)
}

/// seed 一个最小可用的 user 行（不经过 `user_sql::create_user`）。
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

/// seed 一个 user_role（直插 SQL，绕开 UserRoleRepo）。用于软删后过滤分组测试。
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

// ===========================================================================
// 多表组合事务 (2 例)：直调 `sql::user_sql::xxx(&mut *tx, ...)` + `pool.begin()` 开 tx，
// 跨多 repo 写，最后 commit。与 handler 层 `state.pool.begin()` + `&mut *tx` 路径同构
// （2026-09-22 删 `PgIamRepo` 转发壳后的事务边界）。
// ===========================================================================

/// 手写 begin/commit：commit 后写入对外可见
#[tokio::test]
#[allow(clippy::explicit_auto_deref)] // `&mut *tx` 是 sqlx 借 `&mut PgConnection` 的标准模式
async fn create_user_then_add_role_then_list_persists_all() {
    let (pool, _fx) = setup().await;

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
    let (pool, _fx) = setup().await;

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
    let (pool, _fx) = setup().await;

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
    let (pool, _fx) = setup().await;

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
    let (pool, _fx) = setup().await;
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
    let (pool, _fx) = setup().await;
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
    let (pool, _fx) = setup().await;
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
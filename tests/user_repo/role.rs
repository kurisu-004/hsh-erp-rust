//! user 域 repo 集成测试 —— UserRoleRepo / MenuRepo / ShelfRepo 子集（PR13 Phase D 拆分）
//!
//! ## 拆分映射（原 1164 行 user_repo.rs → 3 文件）
//! - basic.rs   ← UserRepo 24 例
//! - role.rs    ← UserRoleRepo 11 + MenuRepo 3 + ShelfRepo 2 = 16 例（本文件）
//! - password.rs ← 多表组合事务 + 事务边界 + 3 个补充集成测试 7 例
//!
//! 本文件 16 例：覆盖 `iam/repo/sql/{user_role,menu,shelf}.rs` 共 7 个固有方法。
//!
//! ## 测试并行注意
//! 进程级 test_pool 每次 fresh database（plan 2 2026-09-20），DB 间 schema
//! 完全独立，无需 Mutex 串行化。
//!
//! ## Fixture 范本化（2026-09-24 PR13 Phase I）
//! 本文件原 `#[path = "../common/mod.rs"] mod common;` + `use common::{...};`
//! 改走 `use hsh_erp_test_support::*` + `load_user_repo_fixture(&pool)` +
//! `UserRepoFixture`。fixture 提供 baseline user / role / menu；本文件大部分
//! 测试仍走本地 `seed_user` / `seed_role` / `seed_menu` / `link_role_menu` /
//! `seed_shelf` helper 创建专属测试数据（特定 username / role 组合 / 多角色
//! / 不同 menu code / 不同 shelf code），user_repo 域独享 helper 不走
//! fixtures.rs。字面请求 / 断言逐字保留。

use sqlx::PgPool;

use hsh_erp_rust::infra::clock::now_naive;
use hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator;
// 2026-09-19 IAM 域合并：原 `user::repo` 重定向到 `iam::repo`，方法零 diff。
// 2026-09-22 重构 #2：sql.rs 拆为 sql/{user,user_role,menu,shelf}.rs free fn；
// 原 `user_role_sql::xxx` → `sql::user_role::xxx`（`UserRoleInsert` 等入参 DTO 仍从
// `repo` re-export 取，与 handler 层 `state.pool.begin()` + `&mut *tx` 路径同构。
use hsh_erp_rust::modules::iam::repo::UserRoleInsert;
use hsh_erp_rust::modules::iam::repo::sql::{
    menu as menu_sql, shelf as shelf_sql, user_role as user_role_sql,
};

// 2026-09-24 PR13 Phase I：fixture 范本化入口。`load_user_repo_fixture(&pool)` 加载
// 1 user + 1 role + 1 menu baseline；本文件大部分测试用本地 seed_user / seed_role
// 等 helper 创建专属测试数据（不同 username / 不同 role 组合），仅在需要 baseline
// 时取 fx.baseline_user_id 等常量。
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

/// 基础 bootstrap：fresh DB + user_repo fixture 3 行（含 baseline user / role / menu）
/// + 返回 pool。
///
/// fixture baseline（本文件大多数测试不直接使用，但 setup 加载过程无害）；
/// 测试现场仍走本地 `seed_user` / `seed_role` 等 helper 创建专属测试数据
///（不同 username / role 组合 / menu code / shelf code）。
async fn setup() -> (PgPool, UserRepoFixture) {
    let pool = test_pool().await;
    let fx = load_user_repo_fixture(&pool).await;
    (pool, fx)
}

/// seed 一个最小可用的 user 行（不经过 `user_sql::create_user`）。本文件大部分
/// role/menu/shelf 测试需要先有 user 才能挂角色，故保留本 fixture。
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
    let (pool, _fx) = setup().await;
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
    let (pool, _fx) = setup().await;
    let uid = seed_user(&pool, "lonely", true).await;
    let rows = user_role_sql::list_user_roles_by_user_id(&pool, uid).await.expect("list");
    assert!(rows.is_empty());
}

/// `user_role_sql::list_user_roles_by_user_id`：LEFT JOIN t_shelf 带出 shelf_code/shelf_name
#[tokio::test]
async fn list_user_roles_by_user_id_includes_shelf_code_and_name() {
    let (pool, _fx) = setup().await;
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
    let (pool, _fx) = setup().await;
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
    let (pool, _fx) = setup().await;
    let r = user_role_sql::get_user_role_by_id(&pool, 999_999_999_999)
        .await
        .expect("query");
    assert!(r.is_none());
}

/// `user_role_sql::has_user_role_with_scope`：已存在重复
#[tokio::test]
async fn exists_same_scope_returns_true_for_dup() {
    let (pool, _fx) = setup().await;
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
    let (pool, _fx) = setup().await;
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
    let (pool, _fx) = setup().await;
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
    let (pool, _fx) = setup().await;
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
    let (pool, _fx) = setup().await;
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
    let (pool, _fx) = setup().await;
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
    let (pool, _fx) = setup().await;
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
    let (pool, _fx) = setup().await;
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
    let (pool, _fx) = setup().await;
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
    let (pool, _fx) = setup().await;
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
    let (pool, _fx) = setup().await;
    let s = shelf_sql::get_shelf_by_id(&pool, 999_999_999_999)
        .await
        .expect("query");
    assert!(s.is_none());
}
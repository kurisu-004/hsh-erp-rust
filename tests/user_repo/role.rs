//! user 域 repo 集成测试 —— UserRoleRepo / MenuRepo / ShelfRepo 子集（PR13 Phase D 拆分）
//!
//! ## 拆分映射（原 1164 行 user_repo.rs → 3 文件）
//! - basic.rs   ← UserRepo 24 例
//! - role.rs    ← UserRoleRepo 16 + MenuRepo 4 + ShelfRepo 2 = 22 例（本文件）
//! - password.rs ← 多表组合事务 + 事务边界 + 3 个补充集成测试 7 例
//!
//! 本文件 22 例：覆盖 `iam/repo/sql/{user_role,menu,shelf}.rs` 共 7 个固有方法。
//! 22 例**已含**针对 `seeds/menu.sql` 的那条自锁断言（`MenuRepo` 4 例之一，不碰
//! DB），不是 22 之外再加的第 23 条。
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
// 1 menu baseline（PR-C.Final 移除 user + role baseline，避免污染
// count / list_with_filters_* 「期望空库」断言）；本文件大部分测试用本地
// seed_user / seed_role 等 helper 创建专属测试数据（不同 username / 不同
// role 组合），仅在需要 baseline 时取 fx.baseline_menu_id 常量。
use hsh_erp_test_support::{UserRepoFixture, load_user_repo_fixture, test_pool};

// ===========================================================================
// 全局串行化互斥：所有用例共享同一 DB。
// ===========================================================================

/// 进程级共享雪花生成器：每次 `SnowflakeIdGenerator::new(...)` 都把 sequence 重置为 0，
/// 同一毫秒内多次 seed 会撞 ID。共享同一生成器才能保证每个用例内多 ID 唯一。
/// 复用 `test-support::pool::pool_snowflake()` 的实例 + epoch + instance 配置
///（原 tests/common/mod.rs::pool_snowflake 转发路径已收口到 crate root）。
///
/// 2026-10-09：`pool_snowflake()` 内层包了 `Arc`（全进程唯一 generator 对象，
/// 见 `test-support/src/pool.rs`），故本转发壳的返回类型同步改一层 `Arc`；
/// 调用侧 `.lock()?.next_id()` 靠 `Deref` 不受影响。
fn snowflake() -> &'static std::sync::Mutex<std::sync::Arc<SnowflakeIdGenerator>> {
    hsh_erp_test_support::pool_snowflake()
}

/// 基础 bootstrap：fresh DB + user_repo fixture 1 行（baseline menu）+ 返回 pool。
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

    let rows = user_role_sql::list_user_roles_by_user_id(&pool, uid)
        .await
        .expect("list");
    assert_eq!(rows.len(), 2);
    let roles: Vec<_> = rows.iter().map(|r| r.role.as_str()).collect();
    assert!(roles.contains(&"MANAGER") && roles.contains(&"CLERK"));
}

/// `user_role_sql::list_user_roles_by_user_id`：无角色用户返回空
#[tokio::test]
async fn list_user_roles_by_user_id_returns_empty_when_no_roles() {
    let (pool, _fx) = setup().await;
    let uid = seed_user(&pool, "lonely", true).await;
    let rows = user_role_sql::list_user_roles_by_user_id(&pool, uid)
        .await
        .expect("list");
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

    let rows = user_role_sql::list_user_roles_by_user_id(&pool, uid)
        .await
        .expect("list");
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
    user_role_sql::create_user_role(&pool, &insert)
        .await
        .expect("create");

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

    let rows = user_role_sql::list_user_roles_by_user_id(&pool, uid)
        .await
        .expect("list");
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
// 批量角色查询（2026-10-10 新增，消解 list_users 的 N+1）
// ===========================================================================

/// `list_user_roles_by_user_ids`：多个 user_id 一次取回，按 user_id 可分组
#[tokio::test]
async fn list_user_roles_by_user_ids_returns_roles_of_multiple_users() {
    let (pool, _fx) = setup().await;
    let u1 = seed_user(&pool, "alice", true).await;
    let u2 = seed_user(&pool, "bob", true).await;
    let _ = seed_role(&pool, u1, "MANAGER", None, None).await;
    let _ = seed_role(&pool, u1, "CLERK", None, None).await;
    let _ = seed_role(&pool, u2, "INSPECTOR", None, None).await;
    // u3 有角色但不在查询集合里 → 不该被返回
    let u3 = seed_user(&pool, "carol", true).await;
    let _ = seed_role(&pool, u3, "CLERK", None, None).await;

    let rows = user_role_sql::list_user_roles_by_user_ids(&pool, &[u1, u2])
        .await
        .expect("list");
    assert_eq!(rows.len(), 3);
    assert!(rows.iter().all(|r| r.user_id == u1 || r.user_id == u2));
    let u1_roles: Vec<_> = rows
        .iter()
        .filter(|r| r.user_id == u1)
        .map(|r| r.role.as_str())
        .collect();
    assert_eq!(u1_roles.len(), 2, "alice 应有 2 个角色：{u1_roles:?}");
}

/// 单个 user_id 与批量版逐字等价（两条路径的投影必须一致）
#[tokio::test]
async fn list_user_roles_by_user_ids_with_single_id_matches_single_id_query() {
    let (pool, _fx) = setup().await;
    let uid = seed_user(&pool, "alice", true).await;
    let _ = seed_role(&pool, uid, "MANAGER", None, None).await;
    let _ = seed_role(&pool, uid, "SHELF_ACCOUNT", Some("shelf"), Some(1)).await;

    let batch = user_role_sql::list_user_roles_by_user_ids(&pool, &[uid])
        .await
        .expect("batch");
    let single = user_role_sql::list_user_roles_by_user_id(&pool, uid)
        .await
        .expect("single");
    assert_eq!(batch.len(), single.len());
    for (b, s) in batch.iter().zip(single.iter()) {
        assert_eq!(b.id, s.id);
        assert_eq!(b.role, s.role);
        assert_eq!(b.scope_type, s.scope_type);
        assert_eq!(b.scope_id, s.scope_id);
        assert_eq!(b.version, s.version);
        assert_eq!(b.shelf_code, s.shelf_code);
        assert_eq!(b.shelf_name, s.shelf_name);
    }
}

/// 空数组 → 返空 Vec（不发 SQL；`= ANY('{}')` 虽能命中 0 行，但白跑一次往返）
#[tokio::test]
async fn list_user_roles_by_user_ids_with_empty_slice_returns_empty() {
    let (pool, _fx) = setup().await;
    let uid = seed_user(&pool, "alice", true).await;
    let _ = seed_role(&pool, uid, "MANAGER", None, None).await;

    let rows = user_role_sql::list_user_roles_by_user_ids(&pool, &[])
        .await
        .expect("空数组不应报错");
    assert!(rows.is_empty(), "空 user_ids 应返空集合");
}

/// 软删行被排除
#[tokio::test]
async fn list_user_roles_by_user_ids_excludes_soft_deleted() {
    let (pool, _fx) = setup().await;
    let uid = seed_user(&pool, "alice", true).await;
    let keep = seed_role(&pool, uid, "MANAGER", None, None).await;
    let drop = seed_role(&pool, uid, "CLERK", None, None).await;
    user_role_sql::soft_delete_user_role(&pool, drop, 0, now_naive(), None)
        .await
        .expect("soft_delete");

    let rows = user_role_sql::list_user_roles_by_user_ids(&pool, &[uid])
        .await
        .expect("list");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, keep);
}

/// LEFT JOIN t_shelf 带出 shelf_code / shelf_name（与单账号版同形）
#[tokio::test]
async fn list_user_roles_by_user_ids_includes_shelf_code_and_name() {
    let (pool, _fx) = setup().await;
    let uid = seed_user(&pool, "shelfie", true).await;

    let shelf_id = snowflake().lock().unwrap().next_id();
    sqlx::query!(
        "INSERT INTO t_shelf (id, code, name, zone, is_active, display_order, version, \
         created_at, updated_at) \
         VALUES ($1, 'S-BATCH', 'Shelf Batch', 'INSPECTION', true, 0, 0, now(), now())",
        shelf_id,
    )
    .execute(&pool)
    .await
    .expect("seed t_shelf");

    let _ = seed_role(&pool, uid, "SHELF_ACCOUNT", Some("shelf"), Some(shelf_id)).await;
    // 非货架角色不应被 JOIN 出的货架字段污染
    let _ = seed_role(&pool, uid, "MANAGER", None, None).await;

    let rows = user_role_sql::list_user_roles_by_user_ids(&pool, &[uid])
        .await
        .expect("list");
    assert_eq!(rows.len(), 2);
    let shelf_row = rows
        .iter()
        .find(|r| r.role == "SHELF_ACCOUNT")
        .expect("SHELF_ACCOUNT 行");
    assert_eq!(shelf_row.shelf_code.as_deref(), Some("S-BATCH"));
    assert_eq!(shelf_row.shelf_name.as_deref(), Some("Shelf Batch"));
    let mgr_row = rows
        .iter()
        .find(|r| r.role == "MANAGER")
        .expect("MANAGER 行");
    assert!(mgr_row.shelf_code.is_none());
    assert!(mgr_row.shelf_name.is_none());
}

// ===========================================================================
// MenuRepo 测试 (4 例，覆盖 1 个固有方法 + 1 条自锁断言)
// ===========================================================================

/// 合成菜单角色（主）：`scripts/test_nextest.sh` 建共享 template DB 时会 apply
/// `seeds/menu.sql`，那份 seed 给 MANAGER / CLERK / INSPECTOR / CNC_PROGRAMMER /
/// SHELF_ACCOUNT 五个真实角色授了若干菜单。断言 `t_role_menu` / `t_menu`
/// 精确条数或内容时若用这五个角色反查，会把共享 seed 的菜单一起数进来
/// （`list_active_menus_by_roles` 因此变红）。故 MenuRepo 的 3 条用例一律走
/// 本常量这种 seed 永不授予的合成角色。
///
/// `t_role_menu.role` 是 `varchar(20)`，无 DB ENUM、无 `t_role` 表，
/// 任意不超过 20 字符的字符串都合法。
const SYNTHETIC_MENU_ROLE: &str = "TEST_MENU_ROLE";

/// 合成菜单角色（辅，供 DISTINCT 用例挂同一菜单的第二条授权行）：
/// `uk_t_role_menu_role_menu` 是 `(role, menu_id) WHERE deleted_at IS NULL` 上的
/// UNIQUE 索引，同一菜单挂两条授权必须用**两个不同**的 role 串，
/// 且两条都必须是合成角色（否则又会把 seed 菜单数进来）。
const SYNTHETIC_MENU_ROLE_B: &str = "TEST_MENU_ROLE_B";

/// 自锁：`seeds/menu.sql` 不得出现上面两个合成角色串。
///
/// 这条断言护的是「合成角色」这个前提本身：若有人把 `TEST_MENU_ROLE` 写进 seed 的
/// 授权白名单，下面的 3 条用例会以「精确条数」失败（可见的红）；但更隐蔽的形态是
/// 同一菜单既被 seed 授予又被测试自建、断言写成 `>=`，红不起来而覆盖已被稀释。
/// 本断言让前提被破坏时**当场**红在 seed 文本上。
///
/// ⚠️ **边界（2026-10-10 登记）**：本断言只读 `seeds/menu.sql` 这一份**文本**。
/// ① 若日后 template DB 改灌别的 seed 文件、或新增别的含授权的 seed，则
/// 「模板库只带这一份 seed」这个前提已破，而本断言仍会绿 —— 届时的失效形态是
/// 下方 3 条用例的精确条数被数错，不会红在 seed 文本上。改 template DB 的灌库
/// 清单（`scripts/test_nextest.sh`）时必须回来补这份前提的护栏。
/// ② 反方向偏严、无漏报：`contains` 是子串匹配，`TEST_MENU_ROLE_X` 这类**包含**
/// 合成串的写法（`SYNTHETIC_MENU_ROLE_B` 就是这种形态）也会被算作命中而误报。
#[test]
fn menu_seed_never_grants_synthetic_menu_roles() {
    // 与生产启动钩子 `src/infra/seed.rs`、测试 template DB 同一份 SQL
    const MENU_SEED_SQL: &str = include_str!("../../seeds/menu.sql");

    for role in [SYNTHETIC_MENU_ROLE, SYNTHETIC_MENU_ROLE_B] {
        assert!(
            !MENU_SEED_SQL.contains(role),
            "seeds/menu.sql 出现了合成角色 {role}：它已不再是 seed 永不授予的角色，\
             下面的 MenuRepo 用例会退化成「把 seed 菜单一起数进来」"
        );
    }
}

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
///
/// 用合成角色而非真实角色：真实角色带进 template DB 的 seed 菜单，精确条数断言会被
/// 稀释。见 `SYNTHETIC_MENU_ROLE` 与 `menu_seed_never_grants_synthetic_menu_roles`。
#[tokio::test]
async fn list_active_for_roles_returns_distinct_menus() {
    let (pool, _fx) = setup().await;
    let m = seed_menu(&pool, "shared-menu", 0, true).await;
    link_role_menu(&pool, SYNTHETIC_MENU_ROLE, m).await;
    link_role_menu(&pool, SYNTHETIC_MENU_ROLE_B, m).await;

    let rows = menu_sql::list_active_menus_by_roles(
        &pool,
        &[SYNTHETIC_MENU_ROLE.into(), SYNTHETIC_MENU_ROLE_B.into()],
    )
    .await
    .expect("list");
    assert_eq!(rows.len(), 1, "两个 role 共用应去重");
    assert_eq!(rows[0].code, "shared-menu");
}

/// `menu_sql::list_active_for_roles`：is_active=false 的菜单被排除
///
/// 用合成角色而非真实角色：真实角色带进 template DB 的 seed 菜单，精确条数断言会被
/// 稀释。见 `SYNTHETIC_MENU_ROLE` 与 `menu_seed_never_grants_synthetic_menu_roles`。
#[tokio::test]
async fn list_active_for_roles_excludes_inactive_menus() {
    let (pool, _fx) = setup().await;
    let active = seed_menu(&pool, "active", 0, true).await;
    let inactive = seed_menu(&pool, "inactive", 1, false).await;
    link_role_menu(&pool, SYNTHETIC_MENU_ROLE, active).await;
    link_role_menu(&pool, SYNTHETIC_MENU_ROLE, inactive).await;

    let rows = menu_sql::list_active_menus_by_roles(&pool, &[SYNTHETIC_MENU_ROLE.into()])
        .await
        .expect("list");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].code, "active");
}

/// `menu_sql::list_active_for_roles`：按 sort_order, code 排序
///
/// 用合成角色而非真实角色：真实角色带进 template DB 的 seed 菜单，精确条数断言会被
/// 稀释。见 `SYNTHETIC_MENU_ROLE` 与 `menu_seed_never_grants_synthetic_menu_roles`。
#[tokio::test]
async fn list_active_for_roles_ordered_by_sort_order_code() {
    let (pool, _fx) = setup().await;
    let m3 = seed_menu(&pool, "z-sort-3", 30, true).await;
    let m1 = seed_menu(&pool, "a-sort-1", 10, true).await;
    let m2 = seed_menu(&pool, "b-sort-2", 20, true).await;
    for m in [m1, m2, m3] {
        link_role_menu(&pool, SYNTHETIC_MENU_ROLE, m).await;
    }

    let rows = menu_sql::list_active_menus_by_roles(&pool, &[SYNTHETIC_MENU_ROLE.into()])
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

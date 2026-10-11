//! 初始管理员 seed 集成测试（2026-09-26 新增）
//!
//! 覆盖 `seeds/admin.sql` + `BOOTSTRAP_ADMIN_ENABLED` env 门控 + 启动钩子
//! `src/infra/seed.rs::run_seeds(pool, bootstrap_admin_enabled)` 的 5 个核心场景：
//!
//! 1. **`bootstrap_admin_seed_disabled_returns_401`** —— `BOOTSTRAP_ADMIN_ENABLED=false`
//!    默认行为：admin 不存在，`POST /iam/login` 走 40101 BIZ_AUTH_INVALID（用户不存在）
//! 2. **`bootstrap_admin_seed_enabled_login_success`** —— 手动执行 seed SQL 后
//!    admin 存在，`POST /iam/login admin/changeme` 返回 200 + bearer token
//! 3. **`bootstrap_admin_seed_idempotent_preserves_manual_hash`** —— admin 已存在
//!    且被手动改过 password_hash 后，重跑 seed ON CONFLICT DO NOTHING 不会被覆盖
//! 4. **`bootstrap_admin_seed_coexists_with_iam_fixture`** —— admin seed 与既有
//!    iam fixture 不冲突，admin + fx_iam_manager 共存
//! 5. **`bootstrap_admin_seed_disabled_keeps_rbac_roles`**（2026-10-14 新增）——
//!    `BOOTSTRAP_ADMIN_ENABLED=false` 时 `t_role` 仍有 6 行系统角色：RBAC 基线
//!    seed 与 admin seed 解耦（前者无条件加载，后者门控）
//!
//! ## admin 的角色是 ADMIN 元角色（2026-10-14 起）
//!
//! 旧版 admin.sql 给的是 MANAGER；现行版给 `ADMIN`（代码级通配全部菜单与权限，
//! 角色行由 `seeds/rbac.sql` 无条件保证）。本文件断言随之改口径。
//!
//! ## 测试策略
//!
//! 不依赖 `src/main.rs` 启动钩子（避免测试间并行导致 fresh-database 里 admin
//! 行意外泄露到其它用例），**测试内直接用 `sqlx::raw_sql(include_str!(...))` 触发**
//! admin seed —— 与生产路径等价（同样 SQL 文件 + 同样 raw_sql 执行），但启动
//! 时序可控、不污染 fresh db 的默认状态。场景 5 直接调 `run_seeds(false)`，验的是
//! 「门控关掉时 RBAC 基线照样加载」这条组合语义。
//!
//! ## Fixture 复用
//!
//! - `hsh_erp_test_support::login_token` —— 走 `/iam/login` 拿完整 bearer token
//! - `hsh_erp_test_support::json_request` / `send` —— HTTP helper
//! - `hsh_erp_test_support::test_pool` / `test_state` / `test_app` —— 启动最小化 app stack
//! - `hsh_erp_test_support::load_iam_fixture` —— 共存场景加载现有 fixture

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;

use hsh_erp_test_support::{
    IamFixture, json_request, load_iam_fixture, login_token, send as ts_send, test_app, test_pool,
    test_state,
};

const ADMIN_USERNAME: &str = "admin";
const ADMIN_PASSWORD: &str = "changeme";
const ADMIN_USER_ID: i64 = 900_000_000_000_000_001;
const ADMIN_ROLE_ID: i64 = 900_000_000_000_000_002;

/// 编译期嵌入 `seeds/admin.sql`（与生产启动钩子同一份文件；本测试直接走
/// `sqlx::raw_sql` 不依赖 main.rs，避免 fresh-database 顺序耦合）。
const ADMIN_SEED_SQL: &str = include_str!("../../seeds/admin.sql");

async fn send(app: axum::Router, req: Request<Body>) -> (StatusCode, Value) {
    ts_send(app, req).await
}

/// 共享 bootstrap：fresh DB + state + app（不预置 admin，admin 由各场景 SQL 控制）。
async fn bootstrap() -> (PgPool, axum::Router) {
    let pool = test_pool().await;
    let state = test_state(pool.clone()).await;
    let app = test_app(state);
    (pool, app)
}

// ===========================================================================
// 场景 1：BOOTSTRAP_ADMIN_ENABLED=false 默认行为（admin 不存在 → 40101）
// ===========================================================================
#[tokio::test]
async fn bootstrap_admin_seed_disabled_returns_401() {
    let (pool, app) = bootstrap().await;
    // 默认 fresh DB 不含 admin（生产环境 BOOTSTRAP_ADMIN_ENABLED 默认 false 时同此状态）。
    let admin_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM t_user WHERE username = $1 AND deleted_at IS NULL",
    )
    .bind(ADMIN_USERNAME)
    .fetch_one(&pool)
    .await
    .expect("count admin");
    assert_eq!(
        admin_count, 0,
        "fresh DB 不应包含 admin 用户（BOOTSTRAP_ADMIN_ENABLED 默认 false）"
    );

    // POST /iam/login admin/changeme → 40101 BIZ_AUTH_INVALID（用户不存在分支）
    let (status, body) = send(
        app,
        json_request(
            "POST",
            "/iam/login",
            Some(json!({"username": ADMIN_USERNAME, "password": ADMIN_PASSWORD})),
            None,
        ),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "未启用 admin 应 401 body={body}"
    );
    assert_eq!(
        body.get("code").and_then(|v| v.as_i64()),
        Some(40101),
        "登录失败用户不存在分支 = BIZ_AUTH_INVALID body={body}"
    );
}

// ===========================================================================
// 场景 2：手动跑 seed → admin 存在 → 登录 200 + token
// ===========================================================================
#[tokio::test]
async fn bootstrap_admin_seed_enabled_login_success() {
    let (pool, app) = bootstrap().await;

    // 模拟生产路径：seed::run_seeds(bootstrap_admin_enabled=true) 内部就是 raw_sql(admin.sql).
    sqlx::raw_sql(ADMIN_SEED_SQL)
        .execute(&pool)
        .await
        .expect("apply admin seed");

    // 断言 admin 行已落库
    let admin_exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM t_user WHERE id = $1 AND deleted_at IS NULL)",
    )
    .bind(ADMIN_USER_ID)
    .fetch_one(&pool)
    .await
    .expect("exists admin");
    assert!(
        admin_exists,
        "seed 后 admin 行必须存在（id={ADMIN_USER_ID}）"
    );

    // 断言 ADMIN role 已落库（2026-10-14 起 admin 是 ADMIN 元角色，不再是 MANAGER）
    let role_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM t_user_role \
         WHERE user_id = $1 AND role = 'ADMIN' AND scope_type IS NULL AND deleted_at IS NULL",
    )
    .bind(ADMIN_USER_ID)
    .fetch_one(&pool)
    .await
    .expect("count role");
    assert_eq!(role_count, 1, "admin 必须拥有 1 条 ADMIN role");

    // 复用 test-support helper 登录拿 token（内部已断言 200 + data.token 存在）。
    let token = login_token(&app, ADMIN_USERNAME, ADMIN_PASSWORD).await;
    assert!(!token.is_empty(), "bearer token 必须非空");

    // 二次完整断言：响应信封结构 + data.token + data.refresh_token
    let (status, body) = send(
        app,
        json_request(
            "POST",
            "/iam/login",
            Some(json!({"username": ADMIN_USERNAME, "password": ADMIN_PASSWORD})),
            None,
        ),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "再次登录 admin/changeme 应 200 body={body}"
    );
    assert_eq!(body.get("code").and_then(|v| v.as_i64()), Some(0));
    let data = body.get("data").expect("data");
    assert!(
        data.get("token").and_then(|v| v.as_str()).is_some(),
        "应返回 access token field"
    );
    assert!(
        data.get("refresh_token").and_then(|v| v.as_str()).is_some(),
        "应返回 refresh_token field"
    );
}

// ===========================================================================
// 场景 3：seed 幂等性 —— 手工改 hash 后重跑 seed，hash 不被覆盖
// ===========================================================================
#[tokio::test]
async fn bootstrap_admin_seed_idempotent_preserves_manual_hash() {
    let (pool, _app) = bootstrap().await;

    // 1. 首次 seed：admin 用默认 changeme 哈希
    sqlx::raw_sql(ADMIN_SEED_SQL)
        .execute(&pool)
        .await
        .expect("first admin seed apply");

    // 2. 模拟 ops 改密：把 admin 的 password_hash 替换成明显不同的字面值（合规
    //    bcrypt cost=12 shape 是 $2b$12$ + 53 chars；故意写成「hash-rotated」
    //    后缀以肉眼可识别 —— 测试只关心"ON CONFLICT 不覆盖"，不关心真能登）。
    let manual_hash = format!(
        "$2b$12${}hash-rotated-by-admin-after-first-login",
        "0".repeat(22)
    );
    let updated = sqlx::query(
        "UPDATE t_user SET password_hash = $1, updated_at = now(), version = version + 1 \
         WHERE id = $2 AND deleted_at IS NULL",
    )
    .bind(&manual_hash)
    .bind(ADMIN_USER_ID)
    .execute(&pool)
    .await
    .expect("manual password rotate");
    assert_eq!(updated.rows_affected(), 1, "应命中 1 行 admin 改密");

    // 3. 再次 seed（生产对应 BOOTSTRAP_ADMIN_ENABLED=true 重启）
    sqlx::raw_sql(ADMIN_SEED_SQL)
        .execute(&pool)
        .await
        .expect("second admin seed re-apply");

    // 4. 断言 hash **未被** seed 字面值覆盖（ON CONFLICT DO NOTHING 生效）
    let current_hash: String =
        sqlx::query_scalar("SELECT password_hash FROM t_user WHERE id = $1 AND deleted_at IS NULL")
            .bind(ADMIN_USER_ID)
            .fetch_one(&pool)
            .await
            .expect("read password_hash");
    assert_eq!(
        current_hash, manual_hash,
        "ON CONFLICT (username) WHERE deleted_at IS NULL DO NOTHING 必须保留手工改过的 hash \
         （不能被反向覆盖回 seed 字面值）"
    );

    // 5. 顺带断言：role 行也未被重复写入（ON CONFLICT DO NOTHING 同样生效）
    let role_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM t_user_role WHERE user_id = $1 AND role = 'ADMIN' \
         AND scope_type IS NULL",
    )
    .bind(ADMIN_USER_ID)
    .fetch_one(&pool)
    .await
    .expect("count role after second seed");
    assert_eq!(
        role_count, 1,
        "t_user_role 重跑 seed 不能产生重复（ON CONFLICT 兜底）"
    );
}

// ===========================================================================
// 场景 4（边界）：admin seed 与既有 iam fixture 不冲突
// ===========================================================================
#[tokio::test]
async fn bootstrap_admin_seed_coexists_with_iam_fixture() {
    let (pool, app) = bootstrap().await;

    // 先灌 admin seed
    sqlx::raw_sql(ADMIN_SEED_SQL)
        .execute(&pool)
        .await
        .expect("admin seed apply");

    // 再灌 iam fixture（5 用户 + 2 角色 + 2 货架；与 admin seed 无 username 冲突）
    let _fx = load_iam_fixture(&pool).await;

    // admin 与 fixture MANAGER 用户（fx_iam_manager）双双存在
    let total: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM t_user WHERE username IN ('admin', 'fx_iam_manager') \
         AND deleted_at IS NULL",
    )
    .fetch_one(&pool)
    .await
    .expect("count both users");
    assert_eq!(total, 2, "admin + fx_iam_manager 应并存");

    // admin 仍能登录（fixture 没把它软删/覆盖）
    let _token = login_token(&app, ADMIN_USERNAME, ADMIN_PASSWORD).await;

    // 额外 sanity：fixture MANAGER 用户（仍是 fx_iam_manager / changeme）也能登录
    let _fx_token = login_token(&app, IamFixture::MANAGER_USERNAME, IamFixture::PASSWORD).await;
}

// ===========================================================================
// 场景 5（2026-10-14 新增）：BOOTSTRAP_ADMIN_ENABLED=false 不阻断 RBAC 基线
// ===========================================================================
// RBAC 基线 seed（seeds/rbac.sql，6 个内置角色 + 权限点 + 授权矩阵）与 admin seed
// 是两件事：前者无条件加载（业务端点与角色面板的判定基线），后者由门控开关。
// 这条用例直接调生产入口 `run_seeds(&pool, false)`，验的就是这个组合语义。
#[tokio::test]
async fn bootstrap_admin_seed_disabled_keeps_rbac_roles() {
    let (pool, _app) = bootstrap().await;

    // 模拟生产启动：BOOTSTRAP_ADMIN_ENABLED=false
    hsh_erp_rust::infra::seed::run_seeds(&pool, false)
        .await
        .expect("run_seeds(false) 应成功");

    // ① admin 账号仍不存在（门控关掉的是账号 seed）
    let admin_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM t_user WHERE username = $1 AND deleted_at IS NULL",
    )
    .bind(ADMIN_USERNAME)
    .fetch_one(&pool)
    .await
    .expect("count admin");
    assert_eq!(
        admin_count, 0,
        "BOOTSTRAP_ADMIN_ENABLED=false 不应创建 admin 用户"
    );

    // ② 6 个内置角色一条不少、全部 is_system + is_active
    let role_codes: Vec<String> = sqlx::query_scalar(
        "SELECT code FROM t_role WHERE is_system AND is_active AND deleted_at IS NULL \
         ORDER BY code",
    )
    .fetch_all(&pool)
    .await
    .expect("查询 t_role 内置角色");
    assert_eq!(
        role_codes,
        vec![
            "ADMIN",
            "CLERK",
            "CNC_PROGRAMMER",
            "INSPECTOR",
            "MANAGER",
            "SHELF_ACCOUNT"
        ],
        "关掉 admin 门控后 6 个内置角色仍必须存在（RBAC seed 无条件加载）"
    );
    let total: i64 = sqlx::query_scalar("SELECT count(*) FROM t_role WHERE deleted_at IS NULL")
        .fetch_one(&pool)
        .await
        .expect("count t_role");
    assert_eq!(total, 6, "fresh 库的 t_role 恰 6 行（尚未建自定义角色）");

    // ③ 幂等：重跑一次结果不变（ON CONFLICT code WHERE deleted_at IS NULL）
    hsh_erp_rust::infra::seed::run_seeds(&pool, false)
        .await
        .expect("run_seeds(false) 重跑应成功");
    let total_again: i64 =
        sqlx::query_scalar("SELECT count(*) FROM t_role WHERE deleted_at IS NULL")
            .fetch_one(&pool)
            .await
            .expect("count t_role after rerun");
    assert_eq!(total_again, 6, "重跑 RBAC seed 不得插出重复角色");
}

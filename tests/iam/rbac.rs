//! iam 域 RBAC 子模块端到端集成测试（2026-10-14 新增，BE-7）
//!
//! 覆盖 `src/modules/iam/rbac/` 的 8 条端点 + 用户域角色授予 / 撤销的联动语义。
//! 18 条用例（名字固定，便于机械核对）：
//!
//! | # | 用例 | 钉住什么 |
//! |---|---|---|
//! | 1 | `roles_list_returns_admin_first_with_user_count` | `GET /roles` 排序（is_system DESC, code ASC）与 user_count 口径 |
//! | 2 | `rbac_endpoints_require_manager` | 8 条端点全部 MANAGER-only，CLERK 一律 40300 |
//! | 3 | `grants_roundtrip_replaces_menus_and_permissions` | grants 整包替换（未提交的授权被软删） |
//! | 4 | `grants_on_admin_role_is_rejected` | ADMIN grants 读全量、写 400（元角色） |
//! | 5 | `admin_sees_all_menus_and_permissions` | ADMIN 元角色的菜单通配（/iam/me 与 /iam/menus 同树）+ grants 权限全量 |
//! | 6 | `non_admin_sees_only_granted_menus` | 普通角色菜单 = t_role_menu 授权集，看不到 roles_list |
//! | 7 | `custom_role_lifecycle_create_update_deactivate_delete` | 自定义角色 CRUD + 系统角色不可删 / 不可带 code / ADMIN+MANAGER 不可停用 + 角色码重复 400 |
//! | 8 | `rename_custom_role_code_cascades_to_user_role` | 改 code 三表级联 + bump t_user_role.version；仅改名称不踢、改 code 才踢 |
//! | 9 | `deactivating_last_admin_user_is_rejected` | 端点 11 / 端点 9 停用最后 ADMIN 用户均 400；有替补时放行 |
//! | 10 | `revoking_admin_role_from_last_admin_is_rejected` | 撤销最后 ADMIN 400 且不踢人；有替补时撤销并踢 |
//! | 11 | `grants_change_kicks_holders_sessions` | 改 grants 后持有人旧 token 40105 |
//! | 12 | `user_role_grant_kicks_target_session` | 授予 / 撤销用户角色后目标旧 token 40105 |
//! | 13 | `custom_role_grant_does_not_trigger_scope_validation` | 自定义角色无 scope 可授予；未知 code 400 |
//! | 14 | `permission_matrix_matches_legacy_roles` | 六身份 × 3 权限点的放行矩阵逐格等于迁移前角色守卫集合 |
//! | 15 | `panel_menu_tree_is_full_for_manager` | `GET /menus` 对非 ADMIN 的 MANAGER 也返全量 active 菜单树（配置面板用） |
//! | 16 | `custom_role_menu_grant_visible_after_login` | 给自定义角色授菜单 → 目标登录后 /iam/me 可见（raw role code 进菜单装载） |
//! | 17 | `deactivated_role_is_filtered_from_next_login` | 停用即降权：停用踢持有人、重登后该角色的菜单消失 |
//! | 18 | `grants_rejects_duplicate_codes` | grants 入参含重复码 → 400（不静默去重） |
//!
//! ## 测试基建
//! - fresh DB（`test_pool()`，从灌过 seeds/menu.sql + seeds/rbac.sql 的 template 克隆）；
//! - `load_iam_fixture`：5 用户 / 2 角色行 / 2 货架（密码明文 changeme）；
//! - 需要 ADMIN 持有人时**直插 t_user_role**（fixture 无 ADMIN 用户）；动态测试行
//!   （如测试用权限点）走 `shared_test_snowflake()` 取号，不另起 generator；
//! - 踢下线的行为判据：旧 token 再打 `/iam/me` → 40105 SESSION_REVOKED。
//!
//! ⚠️ 本文件不硬编码种子菜单的具体条数（改 seeds/menu.sql 会让数字过期）——
//! 「ADMIN 看到全量」的期望值一律现场查库推导。

use std::collections::BTreeSet;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;

use hsh_erp_test_support::{
    IamFixture, json_request, load_iam_fixture, login_token, send as ts_send, test_app, test_pool,
    test_state,
};

// ===========================================================================
// Bootstrap / HTTP helpers
// ===========================================================================

async fn send(app: axum::Router, req: Request<Body>) -> (StatusCode, Value) {
    ts_send(app, req).await
}

/// fresh DB + iam fixture + app（不登录）。
async fn bootstrap() -> (PgPool, axum::Router, IamFixture) {
    let pool = test_pool().await;
    let fx = load_iam_fixture(&pool).await;
    let app = test_app(test_state(pool.clone()).await);
    (pool, app, fx)
}

/// bootstrap + `fx_iam_manager` 登录。
async fn bootstrap_as_manager() -> (PgPool, axum::Router, String, IamFixture) {
    let (pool, app, fx) = bootstrap().await;
    let token = login_token(&app, IamFixture::MANAGER_USERNAME, IamFixture::PASSWORD).await;
    (pool, app, token, fx)
}

async fn get(app: &axum::Router, uri: &str, token: &str) -> (StatusCode, Value) {
    send(app.clone(), json_request("GET", uri, None, Some(token))).await
}

async fn post(app: &axum::Router, uri: &str, body: Value, token: &str) -> (StatusCode, Value) {
    send(
        app.clone(),
        json_request("POST", uri, Some(body), Some(token)),
    )
    .await
}

/// 用 token 打 `/iam/me`，只关心状态码（踢下线后应为 401）。
async fn me_status(app: &axum::Router, token: &str) -> StatusCode {
    get(app, "/iam/me", token).await.0
}

// ===========================================================================
// 取号 / 直插 helper（fixture 不含的测试行）
// ===========================================================================

/// 直插一条 `t_user_role`（无 scope），用于给 fixture 用户加 ADMIN / 角色。
async fn insert_user_role(pool: &PgPool, user_id: i64, role_code: &str) {
    let id = hsh_erp_test_support::shared_test_snowflake().next_id();
    let affected = sqlx::query(
        "INSERT INTO t_user_role \
           (id, user_id, role, scope_type, scope_id, version, created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, $3, NULL, NULL, 0, now(), 0, now(), 0)",
    )
    .bind(id)
    .bind(user_id)
    .bind(role_code)
    .execute(pool)
    .await
    .expect("直插 t_user_role");
    assert_eq!(affected.rows_affected(), 1, "直插 t_user_role 应命中 1 行");
}

/// 直插一条启用中的权限点，用于 grants 整包替换用例（不依赖 BE-8 的种子权限点）。
async fn insert_permission(pool: &PgPool, code: &str, domain: &str) {
    let id = hsh_erp_test_support::shared_test_snowflake().next_id();
    sqlx::query(
        "INSERT INTO t_permission \
           (id, code, name, domain, is_active, version, created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, $3, $4, TRUE, 0, now(), 0, now(), 0)",
    )
    .bind(id)
    .bind(code)
    .bind(format!("测试权限 {code}"))
    .bind(domain)
    .execute(pool)
    .await
    .expect("直插 t_permission");
}

/// 按 code 读角色的 `(id, version)`（未软删）。
async fn role_row(pool: &PgPool, code: &str) -> (i64, i32) {
    sqlx::query_as::<_, (i64, i32)>(
        "SELECT id, version FROM t_role WHERE code = $1 AND deleted_at IS NULL",
    )
    .bind(code)
    .fetch_one(pool)
    .await
    .unwrap_or_else(|e| panic!("角色 {code} 不存在：{e}"))
}

/// 按 `(user_id, role)` 读未软删的 `t_user_role` 行 `(id, version)`。
async fn user_role_row(pool: &PgPool, user_id: i64, role_code: &str) -> (i64, i32) {
    sqlx::query_as::<_, (i64, i32)>(
        "SELECT id, version FROM t_user_role \
         WHERE user_id = $1 AND role = $2 AND deleted_at IS NULL",
    )
    .bind(user_id)
    .bind(role_code)
    .fetch_one(pool)
    .await
    .unwrap_or_else(|e| panic!("用户 {user_id} 的 {role_code} 角色行不存在：{e}"))
}

/// 用户的 `version`（OCC 锚点）。
async fn user_version(pool: &PgPool, user_id: i64) -> i32 {
    sqlx::query_scalar::<_, i32>("SELECT version FROM t_user WHERE id = $1")
        .bind(user_id)
        .fetch_one(pool)
        .await
        .expect("读 t_user.version")
}

/// 某角色经 `t_role_menu` 可见的启用菜单码（与 `/iam/me` 的过滤条件一致）。
async fn live_menu_codes_for_role(pool: &PgPool, role_code: &str) -> BTreeSet<String> {
    sqlx::query_scalar::<_, String>(
        "SELECT DISTINCT m.code FROM t_menu m \
         JOIN t_role_menu rm ON rm.menu_id = m.id \
         WHERE rm.role = $1 AND rm.deleted_at IS NULL \
           AND m.is_active = TRUE AND m.deleted_at IS NULL",
    )
    .bind(role_code)
    .fetch_all(pool)
    .await
    .expect("查角色可见菜单码")
    .into_iter()
    .collect()
}

/// 全部启用中的菜单码（ADMIN 通配的期望值）。
async fn all_active_menu_codes(pool: &PgPool) -> BTreeSet<String> {
    sqlx::query_scalar::<_, String>(
        "SELECT code FROM t_menu WHERE is_active = TRUE AND deleted_at IS NULL",
    )
    .fetch_all(pool)
    .await
    .expect("查全量启用菜单码")
    .into_iter()
    .collect()
}

/// 全部启用中的权限码。
async fn all_active_permission_codes(pool: &PgPool) -> BTreeSet<String> {
    sqlx::query_scalar::<_, String>(
        "SELECT code FROM t_permission WHERE is_active = TRUE AND deleted_at IS NULL ORDER BY code",
    )
    .fetch_all(pool)
    .await
    .expect("查全量启用权限码")
    .into_iter()
    .collect()
}

/// 一份 JSON 字符串数组 → 集合。
fn str_set(v: &Value) -> BTreeSet<String> {
    v.as_array()
        .unwrap_or_else(|| panic!("期望 JSON 数组，实得 {v}"))
        .iter()
        .map(|s| s.as_str().expect("数组元素应为 string").to_string())
        .collect()
}

/// 递归收菜单树里的 code。
fn collect_menu_codes(tree: &Value, out: &mut BTreeSet<String>) {
    let arr = tree
        .as_array()
        .unwrap_or_else(|| panic!("期望菜单树数组，实得 {tree}"));
    for node in arr {
        out.insert(
            node["code"]
                .as_str()
                .unwrap_or_else(|| panic!("菜单节点缺 code：{node}"))
                .to_string(),
        );
        collect_menu_codes(&node["children"], out);
    }
}

/// `GET /iam/me` → `data.menus` 的 code 集合。
async fn me_menu_codes(app: &axum::Router, token: &str) -> BTreeSet<String> {
    let (status, env) = get(app, "/iam/me", token).await;
    assert_eq!(status, StatusCode::OK, "/iam/me 应 200：{env}");
    let mut out = BTreeSet::new();
    collect_menu_codes(&env["data"]["menus"], &mut out);
    out
}

/// 通过 API 建自定义角色，返回 `id`（JSON string 里的数字）。
async fn create_role(app: &axum::Router, token: &str, code: &str, name: &str) -> i64 {
    let (status, env) = post(
        app,
        "/iam/roles",
        json!({ "code": code, "name": name }),
        token,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "创建角色应 201：{env}");
    env["data"]["id"]
        .as_str()
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or_else(|| panic!("角色 id 应为数字字符串：{env}"))
}

// ===========================================================================
// ① roles_list_returns_admin_first_with_user_count
// ===========================================================================
#[tokio::test]
async fn roles_list_returns_admin_first_with_user_count() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    // fixture 无 ADMIN 持有人；给 target 挂上以断言 ADMIN 的 user_count。
    insert_user_role(&pool, fx.target_user_id, "ADMIN").await;

    let (status, env) = get(&app, "/iam/roles", &token).await;
    assert_eq!(status, StatusCode::OK, "GET /roles 应 200：{env}");

    let roles = env["data"].as_array().expect("data 应为数组");
    let codes: Vec<&str> = roles.iter().map(|r| r["code"].as_str().unwrap()).collect();
    assert_eq!(
        codes[0], "ADMIN",
        "is_system DESC, code ASC 排序下 ADMIN 必须置顶：{codes:?}"
    );
    for expected in [
        "ADMIN",
        "CLERK",
        "CNC_PROGRAMMER",
        "INSPECTOR",
        "MANAGER",
        "SHELF_ACCOUNT",
    ] {
        assert!(
            codes.contains(&expected),
            "种子角色 {expected} 应出现在列表：{codes:?}"
        );
    }
    for r in roles {
        assert_eq!(r["is_system"], true, "种子角色全部 is_system=true：{r}");
        assert_eq!(r["is_active"], true, "种子角色全部 is_active=true：{r}");
    }

    // user_count：MANAGER 1（fx_iam_manager）、ADMIN 1（刚直插的 target）
    let count_of = |code: &str| -> i64 {
        roles
            .iter()
            .find(|r| r["code"].as_str() == Some(code))
            .unwrap_or_else(|| panic!("列表缺角色 {code}"))["user_count"]
            .as_i64()
            .unwrap()
    };
    assert_eq!(count_of("MANAGER"), 1, "MANAGER 的在册用户数应为 1");
    assert_eq!(count_of("ADMIN"), 1, "ADMIN 的在册用户数应为 1");
    assert_eq!(count_of("SHELF_ACCOUNT"), 0, "无人持有的角色 user_count=0");
}

// ===========================================================================
// ② rbac_endpoints_require_manager
// ===========================================================================
#[tokio::test]
async fn rbac_endpoints_require_manager() {
    let (_pool, app, _manager_token, _fx) = bootstrap_as_manager().await;
    let clerk = login_token(&app, IamFixture::CLERK_USERNAME, IamFixture::PASSWORD).await;

    // 读端点：roles / permissions / menus
    for uri in ["/iam/roles", "/iam/permissions", "/iam/menus"] {
        let (status, env) = get(&app, uri, &clerk).await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "CLERK 调 {uri} 应 403：{env}"
        );
        assert_eq!(env["code"], 40300, "应为 FORBIDDEN：{env}");
    }

    // 写端点：建角色（body 合法，确保拒绝来自角色守卫而非 JSON 提取器）
    let (status, env) = post(
        &app,
        "/iam/roles",
        json!({ "code": "CLERK_PROBE", "name": "越权探针" }),
        &clerk,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "CLERK 建角色应 403：{env}");
    assert_eq!(env["code"], 40300, "应为 FORBIDDEN：{env}");

    // 反向 sanity：MANAGER（非 ADMIN）可访问 —— 守卫是 MANAGER 而非 ADMIN-only
    let manager = login_token(&app, IamFixture::MANAGER_USERNAME, IamFixture::PASSWORD).await;
    let (status, _) = get(&app, "/iam/roles", &manager).await;
    assert_eq!(status, StatusCode::OK, "MANAGER 应能访问角色列表");
}

// ===========================================================================
// ③ grants_roundtrip_replaces_menus_and_permissions
// ===========================================================================
#[tokio::test]
async fn grants_roundtrip_replaces_menus_and_permissions() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    // 测试用权限点：本用例在 BE-8 种子权限点存在之前 / 之后都要能跑，故自备。
    insert_permission(&pool, "rbac_test.probe", "rbac_test").await;

    let role_id = create_role(&app, &token, "QC_LEAD", "品检组长").await;
    let grants_uri = format!("/iam/roles/{role_id}/grants");

    // 写入：菜单 + 权限点各一条
    let (status, env) = post(
        &app,
        &grants_uri,
        json!({
            "menu_codes": ["home", "parts_list"],
            "permission_codes": ["rbac_test.probe"],
            "version": 0,
        }),
        &token,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "grants 提交应 200：{env}");
    assert_eq!("QC_LEAD", env["data"]["role_code"]);
    assert_eq!(1, env["data"]["version"].as_i64().unwrap(), "OCC 版本 +1");
    assert_eq!(
        str_set(&env["data"]["menu_codes"]),
        BTreeSet::from(["home".to_string(), "parts_list".to_string()])
    );
    assert_eq!(
        str_set(&env["data"]["permission_codes"]),
        BTreeSet::from(["rbac_test.probe".to_string()])
    );

    // 读回：与提交一致
    let (status, env) = get(&app, &grants_uri, &token).await;
    assert_eq!(status, StatusCode::OK, "GET grants 应 200：{env}");
    assert_eq!(
        str_set(&env["data"]["menu_codes"]),
        BTreeSet::from(["home".to_string(), "parts_list".to_string()])
    );
    assert_eq!(
        str_set(&env["data"]["permission_codes"]),
        BTreeSet::from(["rbac_test.probe".to_string()])
    );

    // 整包替换：只留 home、权限清空（parts_list / probe 被软删，不是「不动」）
    let (status, env) = post(
        &app,
        &grants_uri,
        json!({ "menu_codes": ["home"], "permission_codes": [], "version": 1 }),
        &token,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "整包替换应 200：{env}");
    assert_eq!(
        str_set(&env["data"]["menu_codes"]),
        BTreeSet::from(["home".to_string()])
    );
    assert!(
        env["data"]["permission_codes"]
            .as_array()
            .unwrap()
            .is_empty()
    );

    // 库侧：live 行恰为整包后的集合，被替换掉的授权已软删
    let live_menus: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM t_role_menu WHERE role = 'QC_LEAD' AND deleted_at IS NULL",
    )
    .fetch_one(&pool)
    .await
    .expect("count live t_role_menu");
    assert_eq!(live_menus, 1, "整包替换后只剩 home 一条 live 授权");
    let live_perms: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM t_role_permission WHERE role_code = 'QC_LEAD' AND deleted_at IS NULL",
    )
    .fetch_one(&pool)
    .await
    .expect("count live t_role_permission");
    assert_eq!(live_perms, 0, "权限清空后不应有 live 授权行");

    // 不存在的权限码 → 422（AppError::validation 的请求形态校验）
    let (status, _) = post(
        &app,
        &grants_uri,
        json!({ "menu_codes": ["home"], "permission_codes": ["nope.nope"], "version": 2 }),
        &token,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "未知权限码应被入参校验拒绝"
    );
}

// ===========================================================================
// ④ grants_on_admin_role_is_rejected
// ===========================================================================
#[tokio::test]
async fn grants_on_admin_role_is_rejected() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let (admin_id, admin_version) = role_row(&pool, "ADMIN").await;
    let grants_uri = format!("/iam/roles/{admin_id}/grants");

    // 读：全量 active 菜单 / 权限码（面板要能整页渲染）
    let (status, env) = get(&app, &grants_uri, &token).await;
    assert_eq!(status, StatusCode::OK, "ADMIN grants 读应 200：{env}");
    assert_eq!(env["data"]["role_code"], "ADMIN");
    assert_eq!(env["data"]["is_system"], true);
    let menu_codes = str_set(&env["data"]["menu_codes"]);
    assert_eq!(
        menu_codes,
        all_active_menu_codes(&pool).await,
        "ADMIN 的 grants 菜单应为全量 active 菜单"
    );
    assert!(
        menu_codes.contains("roles_list"),
        "全量菜单里应含新菜单 roles_list：{menu_codes:?}"
    );
    assert_eq!(
        str_set(&env["data"]["permission_codes"]),
        all_active_permission_codes(&pool).await,
        "ADMIN 的 grants 权限应为全量 active 权限点"
    );

    // 写：一律 400（元角色，代码级通配不靠数据）
    let (status, env) = post(
        &app,
        &grants_uri,
        json!({ "menu_codes": [], "permission_codes": [], "version": admin_version }),
        &token,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "ADMIN grants 写应 400：{env}"
    );
    assert_eq!(env["code"], 40000, "策略拒绝用 40000 BAD_REQUEST：{env}");

    // 库侧：ADMIN 名下不得出现任何授权行
    let admin_menu_rows: i64 =
        sqlx::query_scalar("SELECT count(*) FROM t_role_menu WHERE role = 'ADMIN'")
            .fetch_one(&pool)
            .await
            .expect("count ADMIN t_role_menu");
    assert_eq!(admin_menu_rows, 0, "ADMIN 不进 t_role_menu");
    let admin_perm_rows: i64 =
        sqlx::query_scalar("SELECT count(*) FROM t_role_permission WHERE role_code = 'ADMIN'")
            .fetch_one(&pool)
            .await
            .expect("count ADMIN t_role_permission");
    assert_eq!(admin_perm_rows, 0, "ADMIN 不进 t_role_permission");
}

// ===========================================================================
// ⑤ admin_sees_all_menus_and_permissions
// ===========================================================================
#[tokio::test]
async fn admin_sees_all_menus_and_permissions() {
    let (pool, app, fx) = bootstrap().await;
    // target 无角色；直插 ADMIN 后登录 —— ADMIN 是元角色，不依赖任何授权行。
    insert_user_role(&pool, fx.target_user_id, "ADMIN").await;
    let admin = login_token(&app, IamFixture::TARGET_USERNAME, IamFixture::PASSWORD).await;

    let all_menus = all_active_menu_codes(&pool).await;
    assert!(
        all_menus.contains("roles_list") && all_menus.contains("home"),
        "前提：template 菜单基线应含 roles_list / home"
    );

    // `/iam/me`：ADMIN 的登录态菜单树 = 全量 active 菜单（不是空树）
    let me_codes = me_menu_codes(&app, &admin).await;
    assert_eq!(
        me_codes, all_menus,
        "ADMIN 的 /iam/me 菜单应为全量 active 菜单"
    );
    // roles 出参是原始大写字符串数组，ADMIN 必须原样出现
    let (status, env) = get(&app, "/iam/me", &admin).await;
    assert_eq!(status, StatusCode::OK, "/iam/me 应 200：{env}");
    assert!(
        str_set(&env["data"]["roles"]).contains("ADMIN"),
        "ADMIN 应出现在 roles 原始字符串数组里：{env}"
    );

    // `GET /iam/menus`（面板渲染用）与 /iam/me 同树
    let (status, env) = get(&app, "/iam/menus", &admin).await;
    assert_eq!(status, StatusCode::OK, "GET /iam/menus 应 200：{env}");
    let mut panel_codes = BTreeSet::new();
    collect_menu_codes(&env["data"], &mut panel_codes);
    assert_eq!(
        panel_codes, all_menus,
        "GET /iam/menus 与 /iam/me 的 menus 必须为同一棵树"
    );

    // 权限点：ADMIN grants 读侧返回全量 active 权限码（BE-8 起非空）
    let (admin_id, _) = role_row(&pool, "ADMIN").await;
    let (status, env) = get(&app, &format!("/iam/roles/{admin_id}/grants"), &admin).await;
    assert_eq!(status, StatusCode::OK, "ADMIN grants 应 200：{env}");
    assert_eq!(
        str_set(&env["data"]["permission_codes"]),
        all_active_permission_codes(&pool).await
    );
}

// ===========================================================================
// ⑥ non_admin_sees_only_granted_menus
// ===========================================================================
#[tokio::test]
async fn non_admin_sees_only_granted_menus() {
    let (pool, app, _fx) = bootstrap().await;
    let clerk = login_token(&app, IamFixture::CLERK_USERNAME, IamFixture::PASSWORD).await;

    let me_codes = me_menu_codes(&app, &clerk).await;
    let granted = live_menu_codes_for_role(&pool, "CLERK").await;
    assert_eq!(
        me_codes, granted,
        "非 ADMIN 的菜单必须恰为其 t_role_menu 授权集"
    );
    assert!(me_codes.contains("home"), "CLERK 应能看到首页");
    assert!(
        !me_codes.contains("roles_list"),
        "CLERK 未获 roles_list 授权，不应看到角色面板：{me_codes:?}"
    );
}

// ===========================================================================
// ⑦ custom_role_lifecycle_create_update_deactivate_delete
// ===========================================================================
#[tokio::test]
async fn custom_role_lifecycle_create_update_deactivate_delete() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;

    // 创建
    let role_id = create_role(&app, &token, "QC_LEAD", "品检组长").await;
    let update_uri = format!("/iam/roles/{role_id}/update");
    let deactivate_uri = format!("/iam/roles/{role_id}/deactivate");

    // 改信息（version 0 → 1）
    let (status, env) = post(
        &app,
        &update_uri,
        json!({ "name": "品检组长（改）", "description": "自定义角色", "version": 0 }),
        &token,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "更新角色应 200：{env}");
    assert_eq!(env["data"]["name"], "品检组长（改）");
    assert_eq!(env["data"]["version"], 1);

    // OCC：旧 version 提交 → 409
    let (status, env) = post(
        &app,
        &update_uri,
        json!({ "name": "过期版本", "version": 0 }),
        &token,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "过期 version 应 409：{env}");
    assert_eq!(env["code"], 40901, "应为 VERSION_CONFLICT：{env}");

    // 重复 code：create → 400（计划口径 400，不是 422；事务回滚不动任何行）
    let (status, env) = post(
        &app,
        "/iam/roles",
        json!({ "code": "qc_lead", "name": "重复角色" }),
        &token,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "重复 code 创建应 400：{env}"
    );
    assert_eq!(env["code"], 40000, "策略拒绝用 40000：{env}");

    // 重复 code：update 改成已有角色 code → 同样 400（与 create 同口径预检）
    let (status, env) = post(
        &app,
        &update_uri,
        json!({ "code": "CLERK", "name": "品检组长（改）", "version": 1 }),
        &token,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "改名撞已有 code 应 400：{env}"
    );
    assert_eq!(env["code"], 40000, "策略拒绝用 40000：{env}");

    // 系统角色不可删（delete=true 一律 400，与 version 无关）
    let (admin_id, admin_version) = role_row(&pool, "ADMIN").await;
    let (status, env) = post(
        &app,
        &format!("/iam/roles/{admin_id}/deactivate"),
        json!({ "version": 0, "delete": true }),
        &token,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "系统角色不可删：{env}");
    assert_eq!(env["code"], 40000);

    // 系统角色 code 不可改 → 400
    let (status, env) = post(
        &app,
        &format!("/iam/roles/{admin_id}/update"),
        json!({ "code": "ADMIN_2", "name": "超级管理员", "version": admin_version }),
        &token,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "系统角色 code 不可改：{env}"
    );
    assert_eq!(env["code"], 40000);

    // 系统角色带**与现值相同**的 code 也一律 400（严格口径：不接受 code 入参）
    let (status, env) = post(
        &app,
        &format!("/iam/roles/{admin_id}/update"),
        json!({ "code": "ADMIN", "name": "超级管理员", "version": admin_version }),
        &token,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "系统角色带相同 code 也应 400：{env}"
    );
    assert_eq!(env["code"], 40000);

    // 停用 ADMIN 角色本身一律 400（不做计数：停用生效后全员的元角色都会失效；
    // fresh 库无 ADMIN 持有人，旧实现只会因计数 0 命中护栏 —— 本断言改为
    // 覆盖「固定拒绝」路径）
    let (status, env) = post(
        &app,
        &format!("/iam/roles/{admin_id}/deactivate"),
        json!({ "version": admin_version, "delete": false }),
        &token,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "ADMIN 角色不可停用应 400：{env}"
    );
    assert_eq!(env["code"], 40000);

    // 停用 MANAGER 角色本身一律 400（2026-10-14：它是面板守卫角色，停用会把面板
    // 对所有 MANAGER 关死，而当前没有启用端点能恢复；与 ADMIN 合并为固定拒绝）
    let (manager_id, manager_version) = role_row(&pool, "MANAGER").await;
    let (status, env) = post(
        &app,
        &format!("/iam/roles/{manager_id}/deactivate"),
        json!({ "version": manager_version, "delete": false }),
        &token,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "MANAGER 角色不可停用应 400：{env}"
    );
    assert_eq!(env["code"], 40000);

    // 停用（delete=false；version 1 → 2）
    let (status, env) = post(
        &app,
        &deactivate_uri,
        json!({ "version": 1, "delete": false }),
        &token,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "停用角色应 200：{env}");
    assert_eq!(env["data"]["affected_user_count"], 0);

    let (status, env) = get(&app, "/iam/roles", &token).await;
    assert_eq!(status, StatusCode::OK);
    let role = env["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["code"] == "QC_LEAD")
        .expect("停用后的角色仍在列表里");
    assert_eq!(role["is_active"], false, "停用后 is_active=false");
    assert_eq!(role["is_system"], false, "自定义角色 is_system=false");

    // 停用角色不可被新授予（400）
    let (status, env) = post(
        &app,
        &format!("/iam/users/{}/roles", fx.target_user_id),
        json!({ "role": "QC_LEAD" }),
        &token,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "停用角色不可授予：{env}");
    assert_eq!(env["code"], 40000);

    // 软删（delete=true；version 2 → 3）
    let (status, env) = post(
        &app,
        &deactivate_uri,
        json!({ "version": 2, "delete": true }),
        &token,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "删除自定义角色应 200：{env}");
    assert_eq!(env["data"]["affected_user_count"], 0);

    // 列表不再可见；grants 读 404
    let (status, env) = get(&app, "/iam/roles", &token).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        !env["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["code"] == "QC_LEAD"),
        "软删后角色不应再出现在列表：{env}"
    );
    let (status, env) = get(&app, &format!("/iam/roles/{role_id}/grants"), &token).await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "软删角色的 grants 应 404：{env}"
    );
}

// ===========================================================================
// ⑧ rename_custom_role_code_cascades_to_user_role
// ===========================================================================
#[tokio::test]
async fn rename_custom_role_code_cascades_to_user_role() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    insert_permission(&pool, "rbac_test.probe", "rbac_test").await;

    let role_id = create_role(&app, &token, "QC_LEAD", "品检组长").await;
    // 给角色配上菜单 + 权限点，验证三张关联表都被级联
    let (status, _env) = post(
        &app,
        &format!("/iam/roles/{role_id}/grants"),
        json!({ "menu_codes": ["home"], "permission_codes": ["rbac_test.probe"], "version": 0 }),
        &token,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    // 授予 fixture target：自定义角色 + 一个内置角色。
    // ⚠️ 必须搭一个内置角色：login 对「枚举认不出任何角色」的用户返 20606
    //    NO_ROLE（`roles.is_empty()` 检查，见 session.rs），纯自定义角色的
    //    账号当前无法登录 —— 这是现行的组合限制，不是本用例的断言对象。
    let (status, env) = post(
        &app,
        &format!("/iam/users/{}/roles", fx.target_user_id),
        json!({ "role": "QC_LEAD" }),
        &token,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "授予自定义角色应 201：{env}");
    let (status, _) = post(
        &app,
        &format!("/iam/users/{}/roles", fx.target_user_id),
        json!({ "role": "CLERK" }),
        &token,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "授予内置角色应 201");

    // 改 code（version 1 → 2）
    let (status, env) = post(
        &app,
        &format!("/iam/roles/{role_id}/update"),
        json!({ "code": "qc_lead_2", "name": "品检组长", "version": 1 }),
        &token,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "改 code 应 200：{env}");
    assert_eq!(env["data"]["code"], "QC_LEAD_2", "code 归一化为大写");

    // 级联三表：旧 code 无 live 行、新 code 行数正确
    let old_user_rows: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM t_user_role WHERE role = 'QC_LEAD' AND deleted_at IS NULL",
    )
    .fetch_one(&pool)
    .await
    .expect("count old t_user_role");
    assert_eq!(old_user_rows, 0, "旧 code 不应残留 live t_user_role 行");
    let new_user_rows: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM t_user_role WHERE role = 'QC_LEAD_2' AND deleted_at IS NULL",
    )
    .fetch_one(&pool)
    .await
    .expect("count new t_user_role");
    assert_eq!(new_user_rows, 1, "持有行的 role 应级联为新 code");

    let new_menu_rows: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM t_role_menu WHERE role = 'QC_LEAD_2' AND deleted_at IS NULL",
    )
    .fetch_one(&pool)
    .await
    .expect("count new t_role_menu");
    assert_eq!(new_menu_rows, 1, "t_role_menu 的 role 应级联为新 code");
    let new_perm_rows: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM t_role_permission WHERE role_code = 'QC_LEAD_2' AND deleted_at IS NULL",
    )
    .fetch_one(&pool)
    .await
    .expect("count new t_role_permission");
    assert_eq!(
        new_perm_rows, 1,
        "t_role_permission 的 role_code 应级联为新 code"
    );

    // m9：级联改名 bump `t_user_role.version`（该行的 OCC 锚点，remove_role 收它）
    let (_, renamed_row_version) = user_role_row(&pool, fx.target_user_id, "QC_LEAD_2").await;
    assert_eq!(
        renamed_row_version, 1,
        "级联改名应把持有行 version 从 0 推到 1"
    );

    // M4：仅改名称 / 描述**不踢**持有者
    let target_token = login_token(&app, IamFixture::TARGET_USERNAME, IamFixture::PASSWORD).await;
    let (status, env) = post(
        &app,
        &format!("/iam/roles/{role_id}/update"),
        json!({ "name": "品检组长（再改）", "version": 2 }),
        &token,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "仅改名称应 200：{env}");
    assert_eq!(
        me_status(&app, &target_token).await,
        StatusCode::OK,
        "仅改名称 / 描述不得踢持有者（session 缓存按 code 组装）"
    );

    // 再改 code（version 3 → 4）：这次必须踢
    let (status, env) = post(
        &app,
        &format!("/iam/roles/{role_id}/update"),
        json!({ "code": "QC_LEAD_3", "name": "品检组长（再改）", "version": 3 }),
        &token,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "再改 code 应 200：{env}");
    assert_eq!(
        me_status(&app, &target_token).await,
        StatusCode::UNAUTHORIZED,
        "改 code 后持有者旧 token 应 40105"
    );
}

// ===========================================================================
// ⑨ deactivating_last_admin_user_is_rejected
// ===========================================================================
#[tokio::test]
async fn deactivating_last_admin_user_is_rejected() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    // 唯一 active 管理员 = target
    insert_user_role(&pool, fx.target_user_id, "ADMIN").await;
    let deactivate_uri = format!("/iam/users/{}/deactivate", fx.target_user_id);
    let target_version = user_version(&pool, fx.target_user_id).await;

    let (status, env) = post(
        &app,
        &deactivate_uri,
        json!({ "version": target_version }),
        &token,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "停用最后一个管理员应 400：{env}"
    );
    assert_eq!(env["code"], 40000);

    // 未被停用：行仍 active、ADMIN 授权仍 live
    let still_active: bool = sqlx::query_scalar("SELECT is_active FROM t_user WHERE id = $1")
        .bind(fx.target_user_id)
        .fetch_one(&pool)
        .await
        .expect("读 target.is_active");
    assert!(still_active, "护栏拒绝后账号必须保持 active");
    let (_, _) = user_role_row(&pool, fx.target_user_id, "ADMIN").await;

    // 端点 9 `update` 也能置 `is_active=false`：同一降权必须被同一护栏拦住
    let (status, env) = post(
        &app,
        &format!("/iam/users/{}/update", fx.target_user_id),
        json!({ "version": target_version, "is_active": false }),
        &token,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "update 置停用同样不得绕过最后一个管理员护栏：{env}"
    );
    assert_eq!(env["code"], 40000);
    let still_active: bool = sqlx::query_scalar("SELECT is_active FROM t_user WHERE id = $1")
        .bind(fx.target_user_id)
        .fetch_one(&pool)
        .await
        .expect("读 target.is_active");
    assert!(still_active, "update 护栏拒绝后账号必须保持 active");

    // 有替补管理员时放行：再给 clerk 挂 ADMIN
    insert_user_role(&pool, fx.clerk_user_id, "ADMIN").await;
    let (status, env) = post(
        &app,
        &deactivate_uri,
        json!({ "version": target_version }),
        &token,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "有替补管理员时停用应放行：{env}");
    assert_eq!(env["data"]["is_active"], false);
}

// ===========================================================================
// ⑩ revoking_admin_role_from_last_admin_is_rejected
// ===========================================================================
#[tokio::test]
async fn revoking_admin_role_from_last_admin_is_rejected() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    insert_user_role(&pool, fx.target_user_id, "ADMIN").await;

    // target 登录（ADMIN 元角色可登录且菜单全量）
    let target_token = login_token(&app, IamFixture::TARGET_USERNAME, IamFixture::PASSWORD).await;
    assert_eq!(me_status(&app, &target_token).await, StatusCode::OK);

    let (row_id, row_version) = user_role_row(&pool, fx.target_user_id, "ADMIN").await;
    let remove_uri = format!("/iam/users/{}/roles/{}/remove", fx.target_user_id, row_id);

    // 撤销最后一个 ADMIN → 400；且**未踢人**（旧 token 仍可用）
    let (status, env) = post(&app, &remove_uri, json!({ "version": row_version }), &token).await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "撤销最后 ADMIN 应 400：{env}"
    );
    assert_eq!(env["code"], 40000);
    assert_eq!(
        me_status(&app, &target_token).await,
        StatusCode::OK,
        "护栏拒绝路径不得调 delete_all_user_sessions（旧 token 应仍然有效）"
    );
    let (_, _) = user_role_row(&pool, fx.target_user_id, "ADMIN").await;

    // 有替补管理员时撤销成功 → 才踢目标
    insert_user_role(&pool, fx.clerk_user_id, "ADMIN").await;
    let (status, env) = post(&app, &remove_uri, json!({ "version": row_version }), &token).await;
    assert_eq!(status, StatusCode::OK, "有替补管理员时撤销应放行：{env}");
    assert_eq!(
        me_status(&app, &target_token).await,
        StatusCode::UNAUTHORIZED,
        "撤销成功后目标旧 token 应 40105"
    );
}

// ===========================================================================
// ⑪ grants_change_kicks_holders_sessions
// ===========================================================================
#[tokio::test]
async fn grants_change_kicks_holders_sessions() {
    let (_pool, app, token, fx) = bootstrap_as_manager().await;

    let role_id = create_role(&app, &token, "QC_LEAD", "品检组长").await;
    let grants_uri = format!("/iam/roles/{role_id}/grants");
    let (status, _) = post(
        &app,
        &grants_uri,
        json!({ "menu_codes": ["home"], "permission_codes": [], "version": 0 }),
        &token,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = post(
        &app,
        &format!("/iam/users/{}/roles", fx.target_user_id),
        json!({ "role": "QC_LEAD" }),
        &token,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    // 搭配内置角色：login 对纯自定义角色用户返 20606（见 ⑧ 的说明）
    let (status, _) = post(
        &app,
        &format!("/iam/users/{}/roles", fx.target_user_id),
        json!({ "role": "CLERK" }),
        &token,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // target 建立 session
    let target_token = login_token(&app, IamFixture::TARGET_USERNAME, IamFixture::PASSWORD).await;
    assert_eq!(me_status(&app, &target_token).await, StatusCode::OK);

    // 改该角色的 grants（version 1 → 2）→ 全部持有人被踢
    let (status, env) = post(
        &app,
        &grants_uri,
        json!({ "menu_codes": ["home", "parts_list"], "permission_codes": [], "version": 1 }),
        &token,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "改 grants 应 200：{env}");
    assert_eq!(
        me_status(&app, &target_token).await,
        StatusCode::UNAUTHORIZED,
        "改 grants 后持有人旧 token 应 40105"
    );
}

// ===========================================================================
// ⑫ user_role_grant_kicks_target_session
// ===========================================================================
#[tokio::test]
async fn user_role_grant_kicks_target_session() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let roles_uri = format!("/iam/users/{}/roles", fx.target_user_id);

    // ① 先授 CLERK 让 target 能登录
    let (status, env) = post(&app, &roles_uri, json!({ "role": "CLERK" }), &token).await;
    assert_eq!(status, StatusCode::CREATED, "授予 CLERK 应 201：{env}");
    let target_token = login_token(&app, IamFixture::TARGET_USERNAME, IamFixture::PASSWORD).await;
    assert_eq!(me_status(&app, &target_token).await, StatusCode::OK);

    // ② 再授 MANAGER → 该次授予必须踢掉 target 的旧 session
    let (status, env) = post(&app, &roles_uri, json!({ "role": "MANAGER" }), &token).await;
    assert_eq!(status, StatusCode::CREATED, "授予 MANAGER 应 201：{env}");
    assert_eq!(
        me_status(&app, &target_token).await,
        StatusCode::UNAUTHORIZED,
        "授予后目标旧 token 应 40105"
    );

    // ③ 重新登录，撤销 MANAGER → 同样踢
    let target_token2 = login_token(&app, IamFixture::TARGET_USERNAME, IamFixture::PASSWORD).await;
    assert_eq!(me_status(&app, &target_token2).await, StatusCode::OK);
    let (row_id, row_version) = user_role_row(&pool, fx.target_user_id, "MANAGER").await;
    let (status, env) = post(
        &app,
        &format!("/iam/users/{}/roles/{}/remove", fx.target_user_id, row_id),
        json!({ "version": row_version }),
        &token,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "撤销角色应 200：{env}");
    assert_eq!(
        me_status(&app, &target_token2).await,
        StatusCode::UNAUTHORIZED,
        "撤销后目标旧 token 应 40105"
    );
}

// ===========================================================================
// ⑬ custom_role_grant_does_not_trigger_scope_validation
// ===========================================================================
#[tokio::test]
async fn custom_role_grant_does_not_trigger_scope_validation() {
    let (_pool, app, token, fx) = bootstrap_as_manager().await;
    create_role(&app, &token, "QC_LEAD", "品检组长").await;
    let roles_uri = format!("/iam/users/{}/roles", fx.target_user_id);

    // 自定义角色 scope 留空（null / 省略）即可授予 —— 不得套用 SHELF_ACCOUNT 的必填规则
    let (status, env) = post(&app, &roles_uri, json!({ "role": "QC_LEAD" }), &token).await;
    assert_eq!(status, StatusCode::CREATED, "自定义角色授予应 201：{env}");
    assert_eq!(env["data"]["role"], "QC_LEAD");
    assert_eq!(env["data"]["scope_type"], Value::Null);
    assert_eq!(env["data"]["scope_id"], Value::Null);

    // 未知 code → 400（t_role 存在性校验接管了原枚举 parse 的职责）
    let (status, env) = post(&app, &roles_uri, json!({ "role": "NO_SUCH_ROLE" }), &token).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "未知角色码应 400：{env}");
    assert_eq!(env["code"], 40000, "应为 BAD_REQUEST：{env}");
}

// ===========================================================================
// ⑭ permission_matrix_matches_legacy_roles（BE-8，2026-10-14）
// ===========================================================================
/// 六身份 × 3 个权限点的放行矩阵，**逐格等于迁移前**的角色守卫集合。
///
/// 迁移前集合的来源（照代码，不猜）：
/// - `part.drawing.print`   ← `part::handler::print` 原 `require_any_role`
///   （MANAGER / CLERK / INSPECTOR / CNC_PROGRAMMER）；同点另接
///   `part_file::service::get_file_with_url`（`GET /part-files/{id}/url` 及
///   cnc alias，原白名单逐格相同）
/// - `queue.dispatch`       ← `dispatch_batch` 原 `require_any_role`（MANAGER / CLERK）
/// - `delivery_note.print`  ← 前端 `canPrint`（MANAGER / CLERK / INSPECTOR；后端打印
///   端点已下线，本点只登记矩阵）
///
/// 三层断言：
/// 1. 数据层：`t_role_permission` 的 live 行逐角色精确等于矩阵（不多不少），
///    ADMIN 无行、SHELF_ACCOUNT 三点全无；
/// 2. 判定层：按角色构造 `CurrentUser`（非 ADMIN 的 permissions 取自 DB），逐点跑
///    `require_permission`，放行结果恰好等于矩阵 —— ADMIN 刻意给**空** permissions
///    也必须全放行（元角色通配在代码里，不靠数据）；
/// 3. 端点层（不在本用例内重复）：`part.drawing.print` 的打印端由
///    `tests/print_forward.rs` 的角色闸门用例覆盖、`/url` 端由
///    `tests/part/file.rs` 的 `get_url_requires_permission_point` /
///    `rbac_shelf_account_get_file_url_still_forbidden` 覆盖，
///    `queue.dispatch` 由 `tests/production/queue_dispatch.rs` 的 Inspector 403
///    用例覆盖。
#[tokio::test]
async fn permission_matrix_matches_legacy_roles() {
    use hsh_erp_rust::auth::rbac::{CurrentUser, Role, parse_role_string};

    const POINTS: [&str; 3] = [
        "part.drawing.print",
        "queue.dispatch",
        "delivery_note.print",
    ];
    /// (身份, 期望放行集合)。ADMIN 由通配全放行；SHELF_ACCOUNT 三点全不授。
    const MATRIX: [(&str, &[&str]); 6] = [
        ("ADMIN", &POINTS),
        (
            "MANAGER",
            &[
                "part.drawing.print",
                "queue.dispatch",
                "delivery_note.print",
            ],
        ),
        (
            "CLERK",
            &[
                "part.drawing.print",
                "queue.dispatch",
                "delivery_note.print",
            ],
        ),
        ("INSPECTOR", &["part.drawing.print", "delivery_note.print"]),
        ("CNC_PROGRAMMER", &["part.drawing.print"]),
        ("SHELF_ACCOUNT", &[]),
    ];

    let pool = test_pool().await;

    // 权限点本体必须已由 seeds/rbac.sql 注册（模板库）
    for code in POINTS {
        let active: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM t_permission \
             WHERE code = $1 AND is_active AND deleted_at IS NULL)",
        )
        .bind(code)
        .fetch_one(&pool)
        .await
        .expect("查 t_permission");
        assert!(active, "权限点 {code} 应已注册且启用");
    }

    for (role, expected) in MATRIX {
        // ① 数据层：该角色的 live 授权 == 期望集合
        let granted: BTreeSet<String> = sqlx::query_scalar::<_, String>(
            "SELECT rp.permission_code FROM t_role_permission rp \
             JOIN t_permission p ON p.code = rp.permission_code \
             WHERE rp.role_code = $1 AND rp.deleted_at IS NULL \
               AND p.is_active AND p.deleted_at IS NULL",
        )
        .bind(role)
        .fetch_all(&pool)
        .await
        .expect("查 t_role_permission")
        .into_iter()
        .collect();
        let expected_set: BTreeSet<String> = expected.iter().map(|s| s.to_string()).collect();
        // 数据层：ADMIN 不写行（元角色通配在代码里），其余角色逐格等于矩阵
        let db_expected: BTreeSet<String> = if role == "ADMIN" {
            BTreeSet::new()
        } else {
            expected_set.clone()
        };
        assert_eq!(
            granted, db_expected,
            "角色 {role} 的权限矩阵与迁移前集合不符（多授权 / 少授权都算回归）"
        );

        // ② 判定层：permissions 取自 DB → 逐点放行结果 == 期望
        let roles = if role == "ADMIN" {
            vec![Role::Admin]
        } else {
            vec![parse_role_string(role).expect("内置角色应可枚举化")]
        };
        let user = CurrentUser {
            id: 1,
            username: role.to_string(),
            roles,
            shelf_ids: vec![],
            shelf_wildcard: false,
            // ADMIN 刻意空：通配不靠数据（若实现改成逐点查数据本断言会红）
            permissions: granted.into_iter().collect(),
        };
        for point in POINTS {
            assert_eq!(
                user.require_permission(point).is_ok(),
                expected.contains(&point),
                "身份 {role} 对权限点 {point} 的放行结果与迁移前不符"
            );
        }
    }
}

// ===========================================================================
// ⑮ panel_menu_tree_is_full_for_manager（M1，2026-10-14）
// ===========================================================================
/// `GET /iam/menus` 是**配置界面**用树：MANAGER（非 ADMIN）也必须拿到**全量**
/// active 菜单 —— 否则「只授给某角色的菜单」（如只授 SHELF_ACCOUNT 的
/// `scan_badge`）在面板里看不见，管理员无法给任何角色勾选 / 收回。
#[tokio::test]
async fn panel_menu_tree_is_full_for_manager() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let all_menus = all_active_menu_codes(&pool).await;
    assert!(
        all_menus.contains("roles_list"),
        "前提：template 菜单基线应含 roles_list"
    );

    let (status, env) = get(&app, "/iam/menus", &token).await;
    assert_eq!(status, StatusCode::OK, "GET /iam/menus 应 200：{env}");
    let mut codes = BTreeSet::new();
    collect_menu_codes(&env["data"], &mut codes);
    assert_eq!(
        codes, all_menus,
        "MANAGER 的配置树必须是全量 active 菜单（不得按调用者已授权集合收窄）"
    );
}

// ===========================================================================
// ⑯ custom_role_menu_grant_visible_after_login（M2，2026-10-14）
// ===========================================================================
/// 给**自定义角色**（`parse_role_string` 认不出的 code）授一个菜单 → 持有该
/// 角色的用户登录后 `/iam/me` 必须能看见。
///
/// 回归点：菜单装载过去用枚举化的 `roles`，自定义角色的 `t_role_menu` 行
/// 永远不会进 `/iam/me`（且零报错）。探针用 `roles_list`（CLERK 未被授予，
/// 见 ⑥），保证它出现在菜单树里只能来自自定义角色的授权。
#[tokio::test]
async fn custom_role_menu_grant_visible_after_login() {
    let (_pool, app, token, fx) = bootstrap_as_manager().await;

    let role_id = create_role(&app, &token, "QC_LEAD", "品检组长").await;
    let (status, env) = post(
        &app,
        &format!("/iam/roles/{role_id}/grants"),
        json!({ "menu_codes": ["roles_list"], "permission_codes": [], "version": 0 }),
        &token,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "自定义角色 grants 应 200：{env}");

    // 搭配 CLERK：login 要求至少一个枚举可识别的角色（见 iam.md 已知偏差）
    for role in ["CLERK", "QC_LEAD"] {
        let (status, env) = post(
            &app,
            &format!("/iam/users/{}/roles", fx.target_user_id),
            json!({ "role": role }),
            &token,
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "授予 {role} 应 201：{env}");
    }

    let target = login_token(&app, IamFixture::TARGET_USERNAME, IamFixture::PASSWORD).await;
    let codes = me_menu_codes(&app, &target).await;
    assert!(
        codes.contains("roles_list"),
        "自定义角色被勾选的菜单必须在 /iam/me 里出现：{codes:?}"
    );
}

// ===========================================================================
// ⑰ deactivated_role_is_filtered_from_next_login（M3，2026-10-14）
// ===========================================================================
/// **停用即降权**：停用角色会踢在册持有人；持有者重新登录后该角色被整体滤掉
/// （其菜单 / 权限 / 货架 scope 一并失效），其它启用角色不受影响。
#[tokio::test]
async fn deactivated_role_is_filtered_from_next_login() {
    let (_pool, app, token, fx) = bootstrap_as_manager().await;

    let role_id = create_role(&app, &token, "QC_LEAD", "品检组长").await;
    let (status, env) = post(
        &app,
        &format!("/iam/roles/{role_id}/grants"),
        json!({ "menu_codes": ["roles_list"], "permission_codes": [], "version": 0 }),
        &token,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "自定义角色 grants 应 200：{env}");
    for role in ["CLERK", "QC_LEAD"] {
        let (status, _) = post(
            &app,
            &format!("/iam/users/{}/roles", fx.target_user_id),
            json!({ "role": role }),
            &token,
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
    }

    let target = login_token(&app, IamFixture::TARGET_USERNAME, IamFixture::PASSWORD).await;
    assert!(
        me_menu_codes(&app, &target).await.contains("roles_list"),
        "前提：停用前自定义角色的菜单可见"
    );

    // 停用 QC_LEAD（version 1 → 2）：必须踢在册持有人，且不删授权行
    let (status, env) = post(
        &app,
        &format!("/iam/roles/{role_id}/deactivate"),
        json!({ "version": 1, "delete": false }),
        &token,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "停用角色应 200：{env}");
    assert_eq!(env["data"]["affected_user_count"], 0, "停用不删授权行");
    assert_eq!(
        me_status(&app, &target).await,
        StatusCode::UNAUTHORIZED,
        "停用后持有者旧 token 应 40105"
    );

    // 重登：QC_LEAD 被滤掉 ⇒ 它的菜单不再出现；CLERK 仍能登录且菜单不受影响
    let target_after = login_token(&app, IamFixture::TARGET_USERNAME, IamFixture::PASSWORD).await;
    let codes = me_menu_codes(&app, &target_after).await;
    assert!(
        !codes.contains("roles_list"),
        "停用角色的菜单不得再下发：{codes:?}"
    );
    assert!(
        codes.contains("home"),
        "启用中角色的菜单不受影响：{codes:?}"
    );
}

// ===========================================================================
// ⑱ grants_rejects_duplicate_codes（m8，2026-10-14）
// ===========================================================================
/// grants 入参里的重复码**显式拒绝**（400），不静默去重 —— 与错误文案 /
/// 「整包 = 集合」语义一致。
#[tokio::test]
async fn grants_rejects_duplicate_codes() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    insert_permission(&pool, "rbac_test.probe", "rbac_test").await;

    let role_id = create_role(&app, &token, "QC_LEAD", "品检组长").await;
    let grants_uri = format!("/iam/roles/{role_id}/grants");

    // 菜单重复
    let (status, env) = post(
        &app,
        &grants_uri,
        json!({ "menu_codes": ["home", "home"], "permission_codes": [], "version": 0 }),
        &token,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "重复菜单码应 400：{env}");
    assert_eq!(env["code"], 40000, "策略拒绝用 40000：{env}");

    // 权限点重复（用**存在**的权限码，证明拒绝来自重复检查而非存在性检查）
    let (status, env) = post(
        &app,
        &grants_uri,
        json!({ "menu_codes": [], "permission_codes": ["rbac_test.probe", "rbac_test.probe"], "version": 0 }),
        &token,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "重复权限码应 400：{env}");
    assert_eq!(env["code"], 40000, "策略拒绝用 40000：{env}");
}

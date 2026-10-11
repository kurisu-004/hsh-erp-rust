//! outsource 域权限点闸门（2026-10-15 第二批新增）
//!
//! 本批迁点（矩阵见 `seeds/rbac.sql` 第 2 节）：
//! - `outsource.view`（M,C,I）：看板 snapshot / 工序详情读、报价读、公司读；
//!   ⚠️ 公司读自原 `require_any_role(M,C,CNC,I)` **有意收紧去 CNC**（用户定稿）；
//! - `outsource.quote.edit`（M,C）：报价建 / 可报价零件 / 提交 / 删；
//! - `outsource.quote.approve`（M）：approve / reject（既有
//!   `quote.rs::approve_quote_clerk_forbidden_40300` 继续覆盖 Clerk 越权）；
//! - `outsource.company.manage`（M,C）：公司建 / 改 / 删；
//! - `outsource.move`（M,C,I）：`POST /outsource-queue/move`。
//!
//! 覆盖（4 个用例）：
//! 1. `company_read_tightened_for_cnc` —— CNC 公司读 403（收紧）；Inspector 正向 200；
//! 2. `quote_read_forbidden_for_shelf_account` —— SA 报价读 / 公司读 403；
//!    Inspector 报价读正向 200；
//! 3. `quote_edit_forbidden_for_inspector` —— Inspector 可报价零件（quote.edit）403；
//!    Clerk 正向 200；
//! 4. `move_and_board_forbidden_for_cnc` —— CNC 看板读 / move 403；SA move 403
//!    （SA 看板读的负向已有 `pool.rs::snapshot_forbidden_for_shelf_account`）。
//!
//! fixture：outsource fixture 内部先载 part fixture ⇒ 已有 MANAGER / CLERK /
//! INSPECTOR / SHELF_ACCOUNT 四身份；CNC 现场建（bcrypt 现场生成）。

#![allow(clippy::await_holding_lock)]

use axum::http::StatusCode;
use serde_json::{Value, json};
use sqlx::PgPool;

use hsh_erp_test_support::{
    OutsourceFixture, PartFixture, json_request, load_outsource_fixture, login_token, pool_snowflake,
    send, test_app, test_pool, test_state,
};

/// fresh DB + outsource fixture + app（不登录）。
async fn bootstrap() -> (PgPool, axum::Router, OutsourceFixture) {
    let pool = test_pool().await;
    let fx = load_outsource_fixture(&pool).await;
    let app = test_app(test_state(pool.clone()).await);
    (pool, app, fx)
}

/// 现场建一个 CNC_PROGRAMMER 用户（bcrypt 现场生成）。
async fn insert_cnc_user(pool: &PgPool, username: &str) {
    use hsh_erp_rust::auth::password;
    use hsh_erp_rust::infra::clock::now_naive;

    let snowflake = pool_snowflake().lock().unwrap_or_else(|p| p.into_inner());
    let user_id = snowflake.next_id();
    let role_id = snowflake.next_id();
    drop(snowflake);
    let now = now_naive();
    let hash = password::hash("changeme").expect("bcrypt");
    sqlx::query(
        "INSERT INTO t_user (id, username, password_hash, full_name, is_active, \
         refresh_token_version, version, created_at, updated_at) \
         VALUES ($1, $2, $3, $4, true, 0, 0, $5, $5)",
    )
    .bind(user_id)
    .bind(username.to_lowercase())
    .bind(hash)
    .bind(username)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_user");
    sqlx::query(
        "INSERT INTO t_user_role (id, user_id, role, scope_type, scope_id, version, \
         created_at, updated_at) \
         VALUES ($1, $2, 'CNC_PROGRAMMER', NULL, NULL, 0, $3, $3)",
    )
    .bind(role_id)
    .bind(user_id)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_user_role");
}

async fn get(app: &axum::Router, uri: &str, token: &str) -> (StatusCode, Value) {
    send(app.clone(), json_request("GET", uri, None, Some(token))).await
}

fn assert_forbidden(s: StatusCode, env: &Value, label: &str) {
    assert_eq!(s, StatusCode::FORBIDDEN, "{label} 应 403: {env}");
    assert_eq!(env["code"], 40300, "{label} 应为 FORBIDDEN: {env}");
}

/// 公司读收紧去 CNC（M,C,CNC,I → M,C,I）：CNC ⇒ 403；Inspector 正向 200。
#[tokio::test]
async fn company_read_tightened_for_cnc() {
    let (pool, app, _fx) = bootstrap().await;
    insert_cnc_user(&pool, "fx_os_cnc1").await;
    let cnc = login_token(&app, "fx_os_cnc1", "changeme").await;
    let inspector = login_token(&app, PartFixture::INSPECTOR_USERNAME, OutsourceFixture::PASSWORD)
        .await;

    let (s, env) = get(&app, "/outsource-companies", &cnc).await;
    assert_forbidden(s, &env, "CNC 公司列表");

    let (s, env) = get(&app, "/outsource-companies", &inspector).await;
    assert_eq!(s, StatusCode::OK, "Inspector 公司列表应 200: {env}");
    assert!(
        env["data"]["items"].as_array().is_some(),
        "items 必须是数组: {env}"
    );
}

/// 报价读（outsource.view 的 M,C,I）：SA 403；Inspector 正向 200。
#[tokio::test]
async fn quote_read_forbidden_for_shelf_account() {
    let (_pool, app, _fx) = bootstrap().await;
    let sa = login_token(
        &app,
        PartFixture::SHELF_ACCOUNT_USERNAME,
        OutsourceFixture::PASSWORD,
    )
    .await;
    let inspector = login_token(&app, PartFixture::INSPECTOR_USERNAME, OutsourceFixture::PASSWORD)
        .await;

    let (s, env) = get(&app, "/outsource-quotes", &sa).await;
    assert_forbidden(s, &env, "SA 报价列表");

    let (s, env) = get(&app, "/outsource-quotes/quotable-parts", &sa).await;
    assert_forbidden(s, &env, "SA 可报价零件");

    let (s, env) = get(&app, "/outsource-quotes", &inspector).await;
    assert_eq!(s, StatusCode::OK, "Inspector 报价列表应 200: {env}");
}

/// 报价工作流辅助读（`quote.edit` 的 M,C）：Inspector 403；Clerk 正向 200。
///
/// `GET /outsource-quotes/quotable-parts` 归 `outsource.quote.edit`（原 M,C 集合
/// 逐格保留，见 seeds 注释）—— 若它被误归 `outsource.view`（M,C,I），
/// Inspector 会拿到 200，本用例立刻红。
#[tokio::test]
async fn quote_edit_forbidden_for_inspector() {
    let (_pool, app, fx) = bootstrap().await;
    let inspector = login_token(&app, PartFixture::INSPECTOR_USERNAME, OutsourceFixture::PASSWORD)
        .await;
    let clerk = login_token(
        &app,
        &fx.clerk_username,
        OutsourceFixture::PASSWORD,
    )
    .await;

    let (s, env) = get(&app, "/outsource-quotes/quotable-parts", &inspector).await;
    assert_forbidden(s, &env, "Inspector 可报价零件");

    let (s, env) = get(&app, "/outsource-quotes/quotable-parts", &clerk).await;
    assert_eq!(s, StatusCode::OK, "Clerk 可报价零件应 200: {env}");
}

/// 外协移动 + 看板读：CNC 两侧 403；SA move 403（SA 看板读负向已在
/// `pool.rs::snapshot_forbidden_for_shelf_account` 覆盖）。
///
/// move 的 body 用「不存在的批次 + 合法形状」：闸门在 service 首行，先于批次查找。
#[tokio::test]
async fn move_and_board_forbidden_for_cnc() {
    let (pool, app, _fx) = bootstrap().await;
    insert_cnc_user(&pool, "fx_os_cnc2").await;
    let cnc = login_token(&app, "fx_os_cnc2", "changeme").await;
    let sa = login_token(
        &app,
        PartFixture::SHELF_ACCOUNT_USERNAME,
        OutsourceFixture::PASSWORD,
    )
    .await;

    let (s, env) = get(&app, "/outsource-queue/snapshot", &cnc).await;
    assert_forbidden(s, &env, "CNC 看板 snapshot");

    let move_body = json!({
        "batch_id": "9000000000000099999",
        "version": 0,
        "from": { "kind": "OUTSOURCE_COMPANY", "company_id": "123" },
        "to": { "kind": "PRODUCTION_SHELF", "shelf_id": "456" },
    });
    let (s, env) = send(
        app.clone(),
        json_request(
            "POST",
            "/outsource-queue/move",
            Some(move_body.clone()),
            Some(&cnc),
        ),
    )
    .await;
    assert_forbidden(s, &env, "CNC move");

    let (s, env) = send(
        app,
        json_request("POST", "/outsource-queue/move", Some(move_body), Some(&sa)),
    )
    .await;
    assert_forbidden(s, &env, "SA move");
}

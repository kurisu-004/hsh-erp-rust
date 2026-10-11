//! delivery_note 三条读端点的权限点闸门（2026-10-15 新增）
//!
//! `delivery_note.view` 落地后：列表 `GET /com/delivery/note` 自原
//! `require_any_role(M,C,I,CNC)` **收紧去 CNC**；详情 `GET /com/delivery/note/{id}`
//! 与批量详情 `GET /com/delivery/note/batch-detail` 为原**零守卫**读、在 handler
//! 首行补闸（守卫留 handler 的理由见 `handler/crud.rs` 文件头）。
//!
//! 覆盖（2 个用例）：
//! 1. `note_reads_permission_matrix`：三条读 × 五身份 —— MANAGER / CLERK /
//!    INSPECTOR 放行（详情按「放行但资源不存在 ⇒ 404 / 21401」判定）、
//!    CNC_PROGRAMMER 与 SHELF_ACCOUNT ⇒ 40300（CNC 收紧、SA 本就不在集合）；
//! 2. `note_reads_reject_before_argument_validation`：无权限身份即使传坏
//!    `ids`，也必须先 40300 —— 钉住「闸门在 handler 首行、先于参数解析」。
//!
//! 扫码入单（`POST /com/delivery/note/scan`，service 层 M,C,I 守卫）不在本文件
//! 重复覆盖，其越权用例见 `scan.rs` 既有用例；本批未改其守卫。

#![allow(clippy::await_holding_lock)]

use axum::http::StatusCode;
use serde_json::Value;
use sqlx::PgPool;

use hsh_erp_test_support::{
    DeliveryFixture, PartFixture, json_request, load_delivery_fixture, login_token, pool_snowflake,
    send, test_app, test_pool, test_state,
};

/// 现场建一个指定角色的用户（bcrypt 现场生成）；供 fixture 未预置的 CNC 身份用。
///
/// 与 `tests/production/queue.rs::insert_user_with_password + add_role` 同形。
async fn insert_user_with_role(pool: &PgPool, username: &str, role: &str) {
    use hsh_erp_rust::auth::password;
    use hsh_erp_rust::infra::clock::now_naive;

    let snowflake = pool_snowflake().lock().unwrap_or_else(|p| p.into_inner());
    let user_id = snowflake.next_id();
    let role_id = snowflake.next_id();
    let hash = password::hash("changeme").expect("bcrypt");
    let now = now_naive();
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
         VALUES ($1, $2, $3, NULL, NULL, 0, $4, $4)",
    )
    .bind(role_id)
    .bind(user_id)
    .bind(role)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_user_role");
}

async fn get(app: &axum::Router, uri: &str, token: &str) -> (StatusCode, Value) {
    send(app.clone(), json_request("GET", uri, None, Some(token))).await
}

/// 五身份 × 三条读端点的闸门矩阵。
///
/// 判定口径：
/// - 放行角色：list ⇒ 200；detail 用一个不存在的 id ⇒ **404 / 21401**（放行到
///   service、资源不存在）；batch-detail `?ids=1` ⇒ 200（缺失 id 静默跳过）；
/// - 拒绝角色（CNC / SA）：三条一律 403 / 40300。
#[tokio::test]
async fn note_reads_permission_matrix() {
    let pool = test_pool().await;
    let fx = load_delivery_fixture(&pool).await;
    let app = test_app(test_state(pool.clone()).await);

    // 五个身份：fixture 预置 M / C / I / SA 用户，CNC 现场建。
    insert_user_with_role(&pool, "fx_dn_view_cnc", "CNC_PROGRAMMER").await;
    let manager = login_token(&app, &fx.part_manager_username, DeliveryFixture::PASSWORD).await;
    let clerk = login_token(&app, PartFixture::CLERK_USERNAME, DeliveryFixture::PASSWORD).await;
    let inspector =
        login_token(&app, PartFixture::INSPECTOR_USERNAME, DeliveryFixture::PASSWORD).await;
    let shelf =
        login_token(&app, PartFixture::SHELF_ACCOUNT_USERNAME, DeliveryFixture::PASSWORD).await;
    let cnc = login_token(&app, "fx_dn_view_cnc", "changeme").await;

    // 不存在但格式合法的 note id（固定值，落在 fixture 常量段之外）。
    const MISSING_NOTE: i64 = 9_000_000_000_000_099_001;

    // ---- 放行角色 ----
    for (label, token) in [
        ("MANAGER", &manager),
        ("CLERK", &clerk),
        ("INSPECTOR", &inspector),
    ] {
        let (s, env) = get(&app, "/com/delivery/note", token).await;
        assert_eq!(s, StatusCode::OK, "{label} list 应 200: {env}");

        let (s, env) = get(&app, &format!("/com/delivery/note/{MISSING_NOTE}"), token).await;
        assert_eq!(
            s,
            StatusCode::NOT_FOUND,
            "{label} detail 应放行到 service（不存在的 id ⇒ 404 / 21401）: {env}"
        );
        assert_eq!(env["code"], 21401, "{label} detail 应 21401: {env}");

        let (s, env) = get(&app, "/com/delivery/note/batch-detail?ids=1", token).await;
        assert_eq!(s, StatusCode::OK, "{label} batch-detail 应 200: {env}");
    }

    // ---- 拒绝角色：CNC 收紧去、SA 本就不在集合 ----
    for (label, token) in [("CNC_PROGRAMMER", &cnc), ("SHELF_ACCOUNT", &shelf)] {
        let (s, env) = get(&app, "/com/delivery/note", token).await;
        assert_eq!(s, StatusCode::FORBIDDEN, "{label} list 应 403: {env}");
        assert_eq!(env["code"], 40300, "{label} list 应 40300: {env}");

        let (s, env) = get(&app, &format!("/com/delivery/note/{MISSING_NOTE}"), token).await;
        assert_eq!(s, StatusCode::FORBIDDEN, "{label} detail 应 403: {env}");
        assert_eq!(env["code"], 40300, "{label} detail 应 40300: {env}");

        let (s, env) = get(&app, "/com/delivery/note/batch-detail?ids=1", token).await;
        assert_eq!(s, StatusCode::FORBIDDEN, "{label} batch-detail 应 403: {env}");
        assert_eq!(env["code"], 40300, "{label} batch-detail 应 40300: {env}");
    }
}

/// 无权限身份的坏入参也必须先 40300 —— 钉住「闸门在 handler 首行、先于参数解析」。
///
/// batch-detail 对坏 ids 是 20104；若有人把守卫挪到参数解析之后，本用例会拿到
/// 20104 而不是 40300。
#[tokio::test]
async fn note_reads_reject_before_argument_validation() {
    let pool = test_pool().await;
    let _fx = load_delivery_fixture(&pool).await;
    let app = test_app(test_state(pool.clone()).await);

    insert_user_with_role(&pool, "fx_dn_view_cnc2", "CNC_PROGRAMMER").await;
    let cnc = login_token(&app, "fx_dn_view_cnc2", "changeme").await;

    let (s, env) = get(&app, "/com/delivery/note/batch-detail?ids=abc", &cnc).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "坏 ids 也应先 403: {env}");
    assert_eq!(env["code"], 40300, "应先 40300 而非 20104: {env}");

    // 对照：MANAGER 传坏 ids ⇒ 20104（证明参数校验本身可达、不是被权限短路掩盖）。
    let manager = login_token(&app, PartFixture::MANAGER_USERNAME, DeliveryFixture::PASSWORD).await;
    let (s, env) = get(&app, "/com/delivery/note/batch-detail?ids=abc", &manager).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "MANAGER 坏 ids 应 400: {env}");
    assert_eq!(env["code"], 20104, "应为 20104: {env}");
}

//! `POST /api/v2/cron/wecom/webhooks/{id}/test`（测试连通）契约。
//!
//! ★ 两条核心回归：
//! 1. **`/test` 不写任何日志表** —— `t_wecom_daily_log` 行数在调用前后必须**相等**。
//!    这是「管理员点一下看通不通」的诊断动作，不是���次推送；混进流水会让
//!    「今天推了几次」这个统计口径失真。
//! 2. **必须解析响应体的 `errcode`** —— 造一个**格式合法但 key 无效**的地址，真打
//!    `qyapi.weixin.qq.com`，企微返 **HTTP 200 + `errcode != 0`** ⇒ 端点必须返
//!    `21802`（HTTP 502）。若实现只看 HTTP 状态，这条会红（它会 200 成功）。
//!
//! ## ⚠️ 这条用例**需要外网**
//! 出网不通（DNS 失败 / 连接超时 / 代理拦截）时**结论完全相同**：`classify` 之外的所有
//! 失败路径也都归到 `21802`。所以用例在「能连上」与「连不上」两种环境下都稳定为红/绿
//! 的正确期望，不会间歇失败。**成功路径（企微返 `errcode=0`）无法在 CI 里覆盖** ——
//! 那需要一个真实可用的群机器人地址，登记在 `docs/api/wecom.md` §8.3。

use axum::http::StatusCode;
use serde_json::json;

use crate::{
    assert_biz_code, assert_no_key_leak, bootstrap, create_body, create_webhook, id_of,
    post_as_manager, unique_name,
};

/// 数一行 `t_wecom_daily_log`。
async fn log_count(pool: &sqlx::PgPool) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM t_wecom_daily_log")
        .fetch_one(pool)
        .await
        .expect("数 t_wecom_daily_log")
}

/// ★ `/test` **不写任何日志**。
///
/// 本用例走的是「key 无效 ⇒ 21802」的失败路径 —— 失败比成功更该被记账（真实推送失败
/// 都要落流水），所以用失败路径来钉「/test 例外」这条更硬。
#[tokio::test]
async fn probe_writes_no_log_rows() {
    let ctx = bootstrap().await;
    let created = create_webhook(&ctx, &unique_name("探针无流水")).await;
    let id = id_of(&created);

    let before = log_count(&ctx.pool).await;
    let (status, env) =
        post_as_manager(&ctx, &format!("/cron/wecom/webhooks/{id}/test"), json!({})).await;
    // 地址合法 ⇒ 请求真的发出去了 ⇒ 必然是 21802（key 无效）
    assert_biz_code(status, &env, 21802, StatusCode::BAD_GATEWAY);
    let after = log_count(&ctx.pool).await;
    assert_eq!(
        before, after,
        "/test 不得写任何 t_wecom_daily_log 行：{before} → {after}"
    );
}

/// ★ key 无效 ⇒ 企微返 HTTP 200 + `errcode != 0` ⇒ 端点必须返 `21802`。
///
/// **这条就是「不解析 errcode 就是静默成功」的守门用例**：把 `sender::classify` 里
/// `errcode` 解析整段去掉，本用例会立刻红（端点会 200 成功）。
#[tokio::test]
async fn probe_with_invalid_key_returns_21802_and_hides_the_key() {
    let ctx = bootstrap().await;
    let key = "invalid-key-for-test-do-not-use-000000000000";
    let url = format!("https://qyapi.weixin.qq.com/cgi-bin/webhook/send?key={key}");
    let (status, env) = post_as_manager(
        &ctx,
        "/cron/wecom/webhooks",
        create_body(&unique_name("无效key探针"), &url),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{env}");
    let id = id_of(&env["data"]);

    let (status, env) =
        post_as_manager(&ctx, &format!("/cron/wecom/webhooks/{id}/test"), json!({})).await;
    assert_biz_code(status, &env, 21802, StatusCode::BAD_GATEWAY);

    // ⚠️ 失败文案里**一个字都不许有 key 或地址**（它会原样回给浏览器、也进日志）
    let raw = env.to_string();
    assert_no_key_leak(&raw, key);
    assert!(!raw.contains("qyapi.weixin.qq.com"), "{raw}");
}

/// `/test` 的 body 是**空对象 `{}`**，⚠️ **不含 `version`**（它不改那一行 ⇒ 没有 OCC）。
///
/// 发 `{}` 必须 200/21802（进到了 handler），发 `{"version":0}` 也一样能进 —— 这条
/// 钉的是「**不**因为缺 `version` 而 422」。
#[tokio::test]
async fn probe_body_is_empty_object_without_version() {
    let ctx = bootstrap().await;
    let created = create_webhook(&ctx, &unique_name("空body探针")).await;
    let id = id_of(&created);

    let (status, raw) = hsh_erp_test_support::send_raw(
        ctx.app.clone(),
        hsh_erp_test_support::json_request(
            "POST",
            &format!("/cron/wecom/webhooks/{id}/test"),
            Some(json!({})),
            Some(&ctx.manager_token),
        ),
    )
    .await;
    // 不是 422（那是 `version` 必填的端点才有的形态）
    assert_ne!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "/test 的 body 是空对象，不该因缺 version 而 422：{raw}"
    );
    // 成功路径走不通（key 无效），但**成功信封的形状**必须是 `R<()>`：data 为 null。
    let v: serde_json::Value = serde_json::from_str(&raw).expect("失败也是 R<T> 信封");
    assert_eq!(v["code"], 21802, "{v}");
    assert!(
        v["data"].is_null(),
        "/test 的响应 data 必须是 null（R<()>）：{v}"
    );
}

/// 探针**不改那一行**（不涨 version、不动 `updated_at`）。
#[tokio::test]
async fn probe_does_not_mutate_the_row() {
    let ctx = bootstrap().await;
    let created = create_webhook(&ctx, &unique_name("探针不改行")).await;
    let id = id_of(&created);
    let before = created["version"].as_i64().unwrap();

    let _ = post_as_manager(&ctx, &format!("/cron/wecom/webhooks/{id}/test"), json!({})).await;

    let (_, env) = crate::get_as_manager(&ctx, "/cron/wecom/webhooks").await;
    let row = env["data"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["id"] == json!(id.to_string()))
        .expect("行仍在");
    assert_eq!(
        row["version"].as_i64().unwrap(),
        before,
        "/test 不得涨 version：{row}"
    );
}

/// 探针也只认 MANAGER（CLERK ⇒ 403，**不得**真发那条请求）。
#[tokio::test]
async fn probe_rejects_non_manager() {
    let ctx = bootstrap().await;
    let created = create_webhook(&ctx, &unique_name("探针越权")).await;
    let id = id_of(&created);

    let before = log_count(&ctx.pool).await;
    let (status, env) =
        crate::post_as_clerk(&ctx, &format!("/cron/wecom/webhooks/{id}/test"), json!({})).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{env}");
    assert_eq!(
        log_count(&ctx.pool).await,
        before,
        "越权请求不得产生任何副作用"
    );
}

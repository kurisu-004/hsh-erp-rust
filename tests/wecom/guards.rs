//! 写端点的**失败矩阵**：OCC（422 纯文本 / 40901）、21807 重名、21803 非法地址、
//! 21806 不存在或已软删、21808 被每日总结配置引用禁删、软删后同名可再建。
//!
//! ★ 核心回归：**被 `t_wecom_daily_config` 引用的 webhook 禁止删除**（21808 + 409）——
//! 否则删完之后每日总结配置的 `webhook_id` 会指向一条死行，页面看着正常、
//! 直到定时任务真推送才以 21801 暴露。

use axum::http::StatusCode;
use serde_json::json;

use crate::{
    assert_biz_code, assert_no_key_leak, bootstrap, create_body, create_webhook, get_as_manager,
    id_of, post_as_manager, post_raw, reference_from_daily_config, stored_url_cipher, unique_name,
    unique_url,
};

// ===========================================================================
//  1. OCC —— 缺 version 是 HTTP 422 纯文本，不是业务信封
// ===========================================================================

/// ⚠️ 缺 `version` ⇒ **HTTP 422 + 纯文本 body**，**没有 `code` 字段**。
///
/// 前端必须按 HTTP 状态码处理这一类失败，不能指望读 `code`。
#[tokio::test]
async fn update_without_version_returns_422_plain_text() {
    let ctx = bootstrap().await;
    let created = create_webhook(&ctx, &unique_name("缺版本")).await;
    let id = id_of(&created);

    let (status, raw) = post_raw(
        &ctx,
        "POST",
        &format!("/cron/wecom/webhooks/{id}"),
        json!({ "name": "不该生效" }),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{raw}");
    assert!(
        !raw.contains("\"code\""),
        "422 是纯文本、不是业务信封：{raw}"
    );
    assert!(raw.contains("version"), "422 文案应点名缺失字段：{raw}");
}

/// `delete` / `set-default` 同样必收 `version`（软删也吃 OCC）。
#[tokio::test]
async fn delete_and_set_default_require_version() {
    let ctx = bootstrap().await;
    let created = create_webhook(&ctx, &unique_name("缺版本2")).await;
    let id = id_of(&created);

    for suffix in ["delete", "set-default"] {
        let (status, raw) = post_raw(
            &ctx,
            "POST",
            &format!("/cron/wecom/webhooks/{id}/{suffix}"),
            json!({}),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{suffix}: {raw}");
        assert!(!raw.contains("\"code\""), "{suffix} 422 不是信封：{raw}");
    }
}

/// 陈旧 `version` ⇒ `40901` + HTTP 409。
#[tokio::test]
async fn stale_version_returns_40901_on_every_write_endpoint() {
    let ctx = bootstrap().await;
    let created = create_webhook(&ctx, &unique_name("陈旧版本")).await;
    let id = id_of(&created);
    let stale = created["version"].as_i64().unwrap();

    // 先改一次把 version 推进
    let (status, env) = post_as_manager(
        &ctx,
        &format!("/cron/wecom/webhooks/{id}"),
        json!({ "version": stale, "name": "推进版本" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{env}");
    let fresh = env["data"]["version"].as_i64().unwrap();
    assert!(fresh > stale, "version 必须自增");

    // 三个写端点都用旧 version ⇒ 全都 40901
    let (status, env) = post_as_manager(
        &ctx,
        &format!("/cron/wecom/webhooks/{id}"),
        json!({ "version": stale, "name": "陈旧" }),
    )
    .await;
    assert_biz_code(status, &env, 40901, StatusCode::CONFLICT);

    let (status, env) = post_as_manager(
        &ctx,
        &format!("/cron/wecom/webhooks/{id}/delete"),
        json!({ "version": stale }),
    )
    .await;
    assert_biz_code(status, &env, 40901, StatusCode::CONFLICT);

    let (status, env) = post_as_manager(
        &ctx,
        &format!("/cron/wecom/webhooks/{id}/set-default"),
        json!({ "version": stale }),
    )
    .await;
    assert_biz_code(status, &env, 40901, StatusCode::CONFLICT);
}

// ===========================================================================
//  2. 21807 —— 同名（未软删）唯一
// ===========================================================================

/// 同名新建 ⇒ `21807` + 409（**不是** 40901：40901 只表达 OCC）。
#[tokio::test]
async fn duplicate_name_returns_21807() {
    let ctx = bootstrap().await;
    let name = unique_name("重名");
    let _first = create_webhook(&ctx, &name).await;

    let (status, env) = post_as_manager(
        &ctx,
        "/cron/wecom/webhooks",
        create_body(&name, &unique_url()),
    )
    .await;
    assert_biz_code(status, &env, 21807, StatusCode::CONFLICT);
}

/// 改名撞同名未软删行 ⇒ `21807` + 409，且**被拒的那一行一个字节都没动**。
#[tokio::test]
async fn rename_to_a_live_duplicate_returns_21807_and_keeps_row_intact() {
    let ctx = bootstrap().await;
    let taken = unique_name("已被占");
    let created = create_webhook(&ctx, &unique_name("要改名")).await;
    let id = id_of(&created);
    let _taken = create_webhook(&ctx, &taken).await;
    let sealed_before = stored_url_cipher(&ctx.pool, id).await;

    let (status, env) = post_as_manager(
        &ctx,
        &format!("/cron/wecom/webhooks/{id}"),
        json!({ "version": created["version"], "name": taken }),
    )
    .await;
    assert_biz_code(status, &env, 21807, StatusCode::CONFLICT);
    assert_eq!(
        stored_url_cipher(&ctx.pool, id).await,
        sealed_before,
        "被拒的改名不得动密文"
    );
    let (_, env) = get_as_manager(&ctx, "/cron/wecom/webhooks").await;
    let row = env["data"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["id"] == json!(id.to_string()))
        .expect("行仍在");
    assert_ne!(row["name"], taken, "名字没被改掉");
}

// ===========================================================================
//  3. 21803 —— 地址非法
// ===========================================================================

/// 非法地址 ⇒ `21803` + 400，且**报错文案不回显地址**。
#[tokio::test]
async fn invalid_url_returns_21803_without_echoing_it() {
    let ctx = bootstrap().await;
    for bad in [
        "https://evil.example.com/hook?key=693a-secret-value",
        "http://qyapi.weixin.qq.com/cgi-bin/webhook/send?key=693a-secret-value",
        "https://qyapi.weixin.qq.com/cgi-bin/webhook/send",
        "   ",
    ] {
        let (status, env) = post_as_manager(
            &ctx,
            "/cron/wecom/webhooks",
            create_body(&unique_name("非法地址"), bad),
        )
        .await;
        assert_biz_code(status, &env, 21803, StatusCode::BAD_REQUEST);
        assert_no_key_leak(&env.to_string(), "693a-secret");
    }
    // 名字超长 ⇒ 21804（⚠️ 不是 21803：语义分层）
    let (status, env) = post_as_manager(
        &ctx,
        "/cron/wecom/webhooks",
        create_body(&"名".repeat(51), &unique_url()),
    )
    .await;
    assert_biz_code(status, &env, 21804, StatusCode::BAD_REQUEST);
}

// ===========================================================================
//  4. 21806 —— 不存在或已软删
// ===========================================================================

/// 对不存在的 id 做任何写操作 ⇒ `21806` + 404。
#[tokio::test]
async fn unknown_id_returns_21806_on_every_write_endpoint() {
    let ctx = bootstrap().await;
    let ghost = crate::ghost_id();

    let (status, env) = post_as_manager(
        &ctx,
        &format!("/cron/wecom/webhooks/{ghost}"),
        json!({ "version": 0, "name": "幽灵" }),
    )
    .await;
    assert_biz_code(status, &env, 21806, StatusCode::NOT_FOUND);

    let (status, env) = post_as_manager(
        &ctx,
        &format!("/cron/wecom/webhooks/{ghost}/delete"),
        json!({ "version": 0 }),
    )
    .await;
    assert_biz_code(status, &env, 21806, StatusCode::NOT_FOUND);

    let (status, env) = post_as_manager(
        &ctx,
        &format!("/cron/wecom/webhooks/{ghost}/set-default"),
        json!({ "version": 0 }),
    )
    .await;
    assert_biz_code(status, &env, 21806, StatusCode::NOT_FOUND);

    let (status, env) = post_as_manager(
        &ctx,
        &format!("/cron/wecom/webhooks/{ghost}/test"),
        json!({}),
    )
    .await;
    assert_biz_code(status, &env, 21806, StatusCode::NOT_FOUND);
}

/// **已软删**的 webhook 对所有写端点等同于「不存在」⇒ 21806。
#[tokio::test]
async fn soft_deleted_id_returns_21806() {
    let ctx = bootstrap().await;
    let created = create_webhook(&ctx, &unique_name("已删")).await;
    let id = id_of(&created);
    let (status, env) = post_as_manager(
        &ctx,
        &format!("/cron/wecom/webhooks/{id}/delete"),
        json!({ "version": created["version"] }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{env}");

    let (status, env) = post_as_manager(
        &ctx,
        &format!("/cron/wecom/webhooks/{id}"),
        json!({ "version": created["version"], "name": "复活" }),
    )
    .await;
    assert_biz_code(status, &env, 21806, StatusCode::NOT_FOUND);
}

// ===========================================================================
//  5. ★ 21808 —— 被每日总结配置引用 ⇒ 禁删
// ===========================================================================

/// ★ 被 `t_wecom_daily_config` 引用 ⇒ `21808` + 409，行**仍在**库里、列表里仍可见。
#[tokio::test]
async fn delete_referenced_webhook_returns_21808() {
    let ctx = bootstrap().await;
    let created = create_webhook(&ctx, &unique_name("被引用")).await;
    let id = id_of(&created);
    reference_from_daily_config(&ctx.pool, id).await;

    let (status, env) = post_as_manager(
        &ctx,
        &format!("/cron/wecom/webhooks/{id}/delete"),
        json!({ "version": created["version"] }),
    )
    .await;
    assert_biz_code(status, &env, 21808, StatusCode::CONFLICT);

    // 行必须还在（拒删，不是「删了再说」）
    let sealed = stored_url_cipher(&ctx.pool, id).await;
    assert!(!sealed.is_empty(), "被引用的 webhook 不得被软删");
    let (_, env) = get_as_manager(&ctx, "/cron/wecom/webhooks").await;
    assert!(
        env["data"]["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["id"] == json!(id.to_string())),
        "被引用的 webhook 必须仍在列表：{env}"
    );
}

/// **已软删**的每日总结配置**不**算引用（软删行既不推送也不被配置页读到，拿它当占用
/// 会让管理员删不掉一个实际上谁也没在用的 webhook）。
#[tokio::test]
async fn delete_allowed_when_only_soft_deleted_config_references_it() {
    let ctx = bootstrap().await;
    let created = create_webhook(&ctx, &unique_name("软删配置")).await;
    let id = id_of(&created);
    reference_from_daily_config(&ctx.pool, id).await;
    sqlx::query("UPDATE t_wecom_daily_config SET deleted_at = now() WHERE deleted_at IS NULL")
        .execute(&ctx.pool)
        .await
        .expect("软删每日总结配置");

    let (status, env) = post_as_manager(
        &ctx,
        &format!("/cron/wecom/webhooks/{id}/delete"),
        json!({ "version": created["version"] }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{env}");
}

// ===========================================================================
//  6. 软删后同名可再建（两个唯一索引都以 deleted_at IS NULL 为前提）
// ===========================================================================

#[tokio::test]
async fn name_can_be_reused_after_soft_delete() {
    let ctx = bootstrap().await;
    let name = unique_name("可复用名");
    let first = create_webhook(&ctx, &name).await;
    let id = id_of(&first);
    let (status, env) = post_as_manager(
        &ctx,
        &format!("/cron/wecom/webhooks/{id}/delete"),
        json!({ "version": first["version"] }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{env}");

    let second = create_webhook(&ctx, &name).await;
    assert_ne!(id_of(&second), id, "同名重建应是一条新行");
}

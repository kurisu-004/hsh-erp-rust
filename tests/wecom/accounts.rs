//! `/api/v2/cron/wecom` 的 `GET /webhooks`、`POST /webhooks`、
//! `POST /webhooks/{id}`、`POST /webhooks/{id}/set-default` 契约。
//!
//! ★ 核心回归：**地址加密入库 + 出参里一个字都不回显明文**（只给 `url_masked`）。

use axum::http::StatusCode;
use serde_json::json;

use crate::{
    Ctx, assert_no_key_leak, bootstrap, create_body, create_webhook, get_as_manager, id_of,
    open_with_test_aad, post_as_clerk, post_as_manager, post_raw, stored_name_and_version,
    stored_url_cipher, unique_name, unique_url,
};

// ===========================================================================
//  1. POST /webhooks —— 建 webhook（密文入库 + 不回显）
// ===========================================================================

/// ★ 建 webhook：地址**加密入库**（库里不是明文），出参**只给掩码**。
#[tokio::test]
async fn create_encrypts_url_and_never_echoes_it() {
    let ctx = bootstrap().await;
    let name = unique_name("加密入库");
    let url = unique_url();
    let key = url
        .split("key=")
        .nth(1)
        .expect("造出来的地址一定带 key=")
        .to_string();

    let (status, env) =
        post_as_manager(&ctx, "/cron/wecom/webhooks", create_body(&name, &url)).await;
    assert_eq!(status, StatusCode::OK, "{env}");
    let out = env["data"].clone();

    // 出参：只有掩码，明文与 key 中段一个字都不出现
    let raw = out.to_string();
    assert!(!raw.contains(&url), "出参回显了地址明文：{raw}");
    assert_no_key_leak(&raw, &key[..10.min(key.len())]);
    assert!(
        out["url_masked"]
            .as_str()
            .expect("必须有 url_masked")
            .contains("***"),
        "url_masked 必须是掩码形态：{raw}"
    );

    // 出参的公开信息齐（前端要显示的）
    assert_eq!(out["name"], name);
    // ⚠️ 新建**不**设默认（置默认是独立端点的事）
    assert_eq!(out["is_default"], false);
    assert_eq!(out["version"], 0);
    // id 是 JSON string
    assert!(out["id"].is_string(), "id 必须是 JSON string：{raw}");

    // 库里确实是密文，且能用测试主密钥 + 本域 AAD 解回明文
    let sealed = stored_url_cipher(&ctx.pool, id_of(&out)).await;
    assert_ne!(sealed, url, "库里不能是明文");
    assert_eq!(open_with_test_aad(&sealed), url);
}

/// 列表同样不回显明文，且**默认 webhook 排最前**。
#[tokio::test]
async fn list_webhooks_hides_plaintext_and_orders_default_first() {
    let ctx = bootstrap().await;
    let a = create_webhook(&ctx, &unique_name("甲机器人")).await;
    let b = create_webhook(&ctx, &unique_name("乙机器人")).await;

    // 置 b 为默认
    let (status, env) = post_as_manager(
        &ctx,
        &format!("/cron/wecom/webhooks/{}/set-default", id_of(&b)),
        json!({ "version": b["version"] }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{env}");
    assert_eq!(env["data"]["is_default"], true, "{env}");

    let (_, env) = get_as_manager(&ctx, "/cron/wecom/webhooks").await;
    assert_eq!(env["code"], 0, "{env}");
    let items = env["data"]["items"].as_array().expect("items 是数组");
    assert_eq!(items[0]["id"], json!(id_of(&b).to_string()), "{env}");
    assert_eq!(items[0]["is_default"], true, "{env}");
    assert_eq!(items[1]["id"], json!(id_of(&a).to_string()), "{env}");

    // ⚠️ **整条列表一个字都不含地址明文 / 密文**
    let raw = env.to_string();
    assert!(!raw.contains("encrypted_url"), "出参出现了密文字段：{raw}");
    for it in items {
        let masked = it["url_masked"].as_str().expect("url_masked 是字符串");
        assert!(masked.contains("***"), "url_masked 未掩码：{masked}");
        assert!(
            masked.ends_with("***") || masked.contains("***"),
            "{masked}"
        );
    }
}

/// 列表只认 MANAGER（CLERK ⇒ 403）。
#[tokio::test]
async fn list_webhooks_rejects_non_manager() {
    let ctx = bootstrap().await;
    let (status, env) = hsh_erp_test_support::send(
        ctx.app.clone(),
        hsh_erp_test_support::json_request(
            "GET",
            "/cron/wecom/webhooks",
            None,
            Some(&ctx.clerk_token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{env}");
}

/// 建档也只认 MANAGER。
#[tokio::test]
async fn create_webhook_rejects_non_manager() {
    let ctx = bootstrap().await;
    let (status, env) = post_as_clerk(
        &ctx,
        "/cron/wecom/webhooks",
        create_body(&unique_name("越权"), &unique_url()),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{env}");
}

// ===========================================================================
//  2. POST /webhooks/{id} —— 全量覆写
// ===========================================================================

/// 改档（**给了新地址**）⇒ 密文被换掉，出参掩码跟着变。
#[tokio::test]
async fn update_with_new_url_replaces_ciphertext() {
    let ctx = bootstrap().await;
    let created = create_webhook(&ctx, &unique_name("改地址")).await;
    let id = id_of(&created);
    let new_url = unique_url();
    let new_key = new_url.split("key=").nth(1).unwrap();

    let (status, env) = post_as_manager(
        &ctx,
        &format!("/cron/wecom/webhooks/{id}"),
        json!({
            "version": created["version"],
            "name": "改过的名字",
            "webhook_url": new_url,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{env}");
    assert_eq!(env["data"]["name"], "改过的名字");
    assert_eq!(env["data"]["version"], 1, "覆写后 version 自增：{env}");
    let raw = env.to_string();
    assert!(!raw.contains(&new_url), "出参回显了新地址明文：{raw}");
    assert_no_key_leak(&raw, &new_key[..10]);

    let sealed = stored_url_cipher(&ctx.pool, id).await;
    assert_eq!(open_with_test_aad(&sealed), new_url);
}

/// ★ 改档**省略** `webhook_url` ⇒ 沿用旧密文（不报错、不换密文）。
///
/// 这条是前端编辑弹窗的既定交互：管理员改个名称时前端拿不到地址明文（服务端从不回显），
/// 强制必填等于逼前端每次把「不知道的东西」原样传回来。
#[tokio::test]
async fn update_omitting_url_keeps_old_ciphertext() {
    let ctx = bootstrap().await;
    let created = create_webhook(&ctx, &unique_name("沿用密文")).await;
    let id = id_of(&created);
    let before = stored_url_cipher(&ctx.pool, id).await;

    let (status, env) = post_as_manager(
        &ctx,
        &format!("/cron/wecom/webhooks/{id}"),
        json!({ "version": created["version"], "name": "只改名字" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{env}");
    assert_eq!(env["data"]["name"], "只改名字");

    let after = stored_url_cipher(&ctx.pool, id).await;
    assert_eq!(
        before, after,
        "省略 webhook_url 必须原样沿用旧密文（不得重新 seal —— 那会换掉密文却无人察觉）"
    );
    // 出参的掩码也必须与之前一致（说明解出来的是同一个地址）
    assert_eq!(env["data"]["url_masked"], created["url_masked"]);
}

/// 空串同样表示「沿用」（前端输入框清空后提交常带空白）。
#[tokio::test]
async fn update_with_blank_url_keeps_old_ciphertext() {
    let ctx = bootstrap().await;
    let created = create_webhook(&ctx, &unique_name("空串沿用")).await;
    let id = id_of(&created);
    let before = stored_url_cipher(&ctx.pool, id).await;

    let (status, env) = post_as_manager(
        &ctx,
        &format!("/cron/wecom/webhooks/{id}"),
        json!({ "version": created["version"], "name": "空串也算沿用", "webhook_url": "   " }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{env}");
    assert_eq!(
        stored_url_cipher(&ctx.pool, id).await,
        before,
        "纯空白 URL 必须沿用旧密文"
    );
}

/// 改档给了**非法**地址 ⇒ `21803`，且**旧密文一个字节都没动**。
#[tokio::test]
async fn update_with_invalid_url_returns_21803_and_keeps_row_intact() {
    let ctx = bootstrap().await;
    let created = create_webhook(&ctx, &unique_name("非法地址")).await;
    let id = id_of(&created);
    let before_sealed = stored_url_cipher(&ctx.pool, id).await;
    let before = stored_name_and_version(&ctx.pool, id).await;

    let (status, env) = post_as_manager(
        &ctx,
        &format!("/cron/wecom/webhooks/{id}"),
        json!({
            "version": created["version"],
            "name": "不该生效",
            "webhook_url": "https://evil.example.com/hook?key=693a-secret",
        }),
    )
    .await;
    crate::assert_biz_code(status, &env, 21803, StatusCode::BAD_REQUEST);
    assert_no_key_leak(&env.to_string(), "693a-secret");

    assert_eq!(
        stored_name_and_version(&ctx.pool, id).await,
        before,
        "被拒的覆写不得动 name / version"
    );
    assert_eq!(
        stored_url_cipher(&ctx.pool, id).await,
        before_sealed,
        "被拒的覆写不得动密文"
    );
}

// ===========================================================================
//  3. set-default —— 唯一性 + 先降后升
// ===========================================================================

/// 置默认**先降旧、再升新**，且库终态恰好一个默认。
#[tokio::test]
async fn set_default_demotes_the_previous_default() {
    let ctx = bootstrap().await;
    let a = create_webhook(&ctx, &unique_name("旧默认")).await;
    let b = create_webhook(&ctx, &unique_name("新默认")).await;

    for target in [&a, &b] {
        let (status, env) = post_as_manager(
            &ctx,
            &format!("/cron/wecom/webhooks/{}/set-default", id_of(target)),
            json!({ "version": target["version"] }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{env}");
        assert_eq!(env["data"]["is_default"], true, "{env}");
    }

    let defaults = count_defaults(&ctx).await;
    assert_eq!(defaults, 1, "库里终态必须恰好一个默认：{defaults}");
    let (_, env) = get_as_manager(&ctx, "/cron/wecom/webhooks").await;
    let items = env["data"]["items"].as_array().expect("items 是数组");
    assert_eq!(
        items[0]["id"],
        json!(id_of(&b).to_string()),
        "默认排最前：{env}"
    );
}

/// 软删默认 webhook 后库里「零个默认」是**合法状态**（不自动转给别的）。
#[tokio::test]
async fn deleting_the_default_leaves_no_default() {
    let ctx = bootstrap().await;
    let a = create_webhook(&ctx, &unique_name("被删默认")).await;
    let b = create_webhook(&ctx, &unique_name("留下的")).await;
    let (status, env) = post_as_manager(
        &ctx,
        &format!("/cron/wecom/webhooks/{}/set-default", id_of(&a)),
        json!({ "version": a["version"] }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{env}");
    let a_id = id_of(&a);
    let (_, env) = get_as_manager(&ctx, "/cron/wecom/webhooks").await;
    let cur = version_of(&env, a_id);
    let (status, env) = post_as_manager(
        &ctx,
        &format!("/cron/wecom/webhooks/{a_id}/delete"),
        json!({ "version": cur }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{env}");

    let defaults = count_defaults(&ctx).await;
    assert_eq!(defaults, 0, "删掉默认后应为「零个默认」，实测 {defaults}");
    let (_, env) = get_as_manager(&ctx, "/cron/wecom/webhooks").await;
    let items = env["data"]["items"].as_array().expect("items 是数组");
    assert_eq!(items.len(), 1, "只剩留下的那个：{env}");
    assert_eq!(items[0]["id"], json!(id_of(&b).to_string()));
}

// ===========================================================================
//  4. 动词纪律
// ===========================================================================

/// ⚠️ `PUT` / `PATCH` / `DELETE` **不被路由**（本仓 HTTP 方法只有 GET / POST，
/// 登记在 `docs/api/wecom.md` §6）。将来有人加了 `DELETE /webhooks/{id}` 时会当场红。
#[tokio::test]
async fn put_patch_delete_verbs_are_not_routed() {
    let ctx = bootstrap().await;
    let created = create_webhook(&ctx, &unique_name("动词纪律")).await;
    let id = id_of(&created);

    for verb in ["PUT", "PATCH", "DELETE"] {
        let (status, raw) =
            post_raw(&ctx, verb, &format!("/cron/wecom/webhooks/{id}"), json!({})).await;
        assert_eq!(
            status,
            StatusCode::METHOD_NOT_ALLOWED,
            "{verb} /cron/wecom/webhooks/{{id}} 不该被路由：{raw}"
        );
    }
}

// ===========================================================================
//  辅助
// ===========================================================================

/// 从列表信封里取出某 id 当前的 `version`（子文件共用）。
pub(crate) fn version_of(env: &serde_json::Value, id: i64) -> i64 {
    env["data"]["items"]
        .as_array()
        .expect("items 是数组")
        .iter()
        .find(|i| i["id"] == json!(id.to_string()))
        .unwrap_or_else(|| panic!("列表里找不到 {id}：{env}"))["version"]
        .as_i64()
        .expect("version 是数字")
}

/// 库里未软删的默认行数。
pub(crate) async fn count_defaults(ctx: &Ctx) -> i64 {
    sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM t_wecom_webhook WHERE deleted_at IS NULL AND is_default",
    )
    .fetch_one(&ctx.pool)
    .await
    .expect("数默认 webhook")
}

/// 辅助：`ctx` 参数在本文件被多处使用时不至于被误判为未用。
#[allow(dead_code)]
fn _ctx_type_check(_c: &Ctx) {}

//! mail_digest 发送流水端点（`GET /logs`）集成测试
//!
//! 覆盖 4 组场景：
//! 1. `logs_paginates_with_limit_and_offset_and_reports_total`
//! 2. `logs_clamps_out_of_range_pagination_params`
//! 3. `log_snapshots_recipients_at_send_time`（★ 配置改了也不改历史行）
//! 4. `log_records_config_version_at_send_time`
//!
//! ## 为什么要钉「快照」语义
//! `t_mail_digest_log` 存的是**逗号分隔的收件人快照**与 `config_version` 快照，
//! **不 join** 配置表。理由：配置改了之后仍要看「当时发给谁、用的是哪版配置」，
//! 否则「上周那封邮件为什么发给了 A 而不是 B」永远查不清 —— 而这正是本域最常见
//! 的运维问题（有人改了收件人列表，回头查历史）。

use axum::http::StatusCode;
use serde_json::{Value, json};

use hsh_erp_test_support::{json_request, send};

use crate::{Ctx, bootstrap_with_mail, config_body, insert_log_row, unreachable_smtp};

/// `GET /logs?query` → `(status, 信封)`。
async fn get_logs(ctx: &Ctx, query: &str) -> (StatusCode, Value) {
    send(
        ctx.app.clone(),
        json_request(
            "GET",
            &format!("/mail-digest/logs{query}"),
            None,
            Some(&ctx.manager_token),
        ),
    )
    .await
}

/// `POST /config`（全量覆写）→ `(status, 信封)`。
async fn save_config(ctx: &Ctx, body: Value) -> (StatusCode, Value) {
    send(
        ctx.app.clone(),
        json_request(
            "POST",
            "/mail-digest/config",
            Some(body),
            Some(&ctx.manager_token),
        ),
    )
    .await
}

// ===========================================================================
//  1. 分页
// ===========================================================================

/// `limit` / `offset` 生效，且 `total` **不受** `limit` 截断影响。
///
/// 排序是 `sent_at DESC, id DESC`（同刻按 id 兜底，保证分页顺序确定）。
#[tokio::test]
async fn logs_paginates_with_limit_and_offset_and_reports_total() {
    let ctx = bootstrap_with_mail(unreachable_smtp()).await;
    let base = hsh_erp_rust::infra::clock::now_naive();
    // 三个不同发送时刻：old < mid < new
    insert_log_row(&ctx.pool, 1, "MANUAL", "old@example.com", base).await;
    insert_log_row(
        &ctx.pool,
        1,
        "SCHEDULED",
        "mid@example.com",
        base + chrono::Duration::minutes(1),
    )
    .await;
    insert_log_row(
        &ctx.pool,
        1,
        "MANUAL",
        "new@example.com",
        base + chrono::Duration::minutes(2),
    )
    .await;

    // 首页 2 条
    let (status, env) = get_logs(&ctx, "?limit=2").await;
    assert_eq!(status, StatusCode::OK, "{env}");
    assert_eq!(env["data"]["total"], 3, "total 不受 limit 截断：{env}");
    assert_eq!(env["data"]["limit"], 2);
    assert_eq!(env["data"]["offset"], 0);
    let items = env["data"]["items"].as_array().expect("items 是数组");
    assert_eq!(items.len(), 2, "{env}");
    // 倒序：最新在前
    assert_eq!(
        items[0]["to_recipients"],
        json!(["new@example.com"]),
        "{env}"
    );
    assert_eq!(
        items[1]["to_recipients"],
        json!(["mid@example.com"]),
        "{env}"
    );

    // 第二页 1 条（offset 生效）
    let (status, env) = get_logs(&ctx, "?limit=2&offset=2").await;
    assert_eq!(status, StatusCode::OK, "{env}");
    let items = env["data"]["items"].as_array().expect("items 是数组");
    assert_eq!(items.len(), 1, "{env}");
    assert_eq!(
        items[0]["to_recipients"],
        json!(["old@example.com"]),
        "{env}"
    );
    assert_eq!(env["data"]["total"], 3, "{env}");

    // 缺省 limit = 20
    let (_, env) = get_logs(&ctx, "").await;
    assert_eq!(env["data"]["limit"], 20, "缺省单页 20 条：{env}");
}

/// 越界分页参数被 clamp：`limit∈[1,200]`、`offset>=0`。
#[tokio::test]
async fn logs_clamps_out_of_range_pagination_params() {
    let ctx = bootstrap_with_mail(unreachable_smtp()).await;

    let (_, env) = get_logs(&ctx, "?limit=99999").await;
    assert_eq!(env["data"]["limit"], 200, "超上限 clamp 到 200：{env}");

    let (_, env) = get_logs(&ctx, "?limit=0").await;
    assert_eq!(
        env["data"]["limit"], 1,
        "下界 clamp 到 1（0 会让 SQL LIMIT 报错）：{env}"
    );

    let (_, env) = get_logs(&ctx, "?limit=-5").await;
    assert_eq!(env["data"]["limit"], 1, "{env}");

    let (_, env) = get_logs(&ctx, "?offset=-7").await;
    assert_eq!(env["data"]["offset"], 0, "负 offset clamp 到 0：{env}");
}

// ===========================================================================
//  2. 收件人快照
// ===========================================================================

/// ★ 发完信后改配置收件人，**历史流水的 `to_recipients` 仍是旧值**。
///
/// 用真实失败发送造流水（`deliver` 的落库路径），而不是手工插行 ——
/// 这样才验到「快照来自配置」而不是「DB 列恰好没被更新」。
#[tokio::test]
async fn log_snapshots_recipients_at_send_time() {
    let ctx = bootstrap_with_mail(unreachable_smtp()).await;

    let mut cfg = config_body(0);
    cfg["recipients"] = json!(["old@example.com"]);
    let (status, saved) = save_config(&ctx, cfg).await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    let v = saved["data"]["version"].as_i64().expect("version 是数字") as i32;

    // 真发一次（必失败 ⇒ 落一行流水）
    let (status, env) = send(
        ctx.app.clone(),
        json_request(
            "POST",
            "/mail-digest/send",
            Some(json!({})),
            Some(&ctx.manager_token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{env}");

    // 改配置收件人（按刚读回的版本提交）
    let mut cfg2 = config_body(v);
    cfg2["recipients"] = json!(["new@example.com"]);
    let (status, env2) = save_config(&ctx, cfg2).await;
    assert_eq!(status, StatusCode::OK, "{env2}");

    // 历史流水仍是旧收件人
    let (status, env3) = get_logs(&ctx, "").await;
    assert_eq!(status, StatusCode::OK, "{env3}");
    assert_eq!(env3["data"]["total"], 1, "{env3}");
    assert_eq!(
        env3["data"]["items"][0]["to_recipients"],
        json!(["old@example.com"]),
        "流水必须存快照，不能反映后来的配置改动：{env3}"
    );

    // 配置侧确实已经改了（否则本用例等于什么都没验）
    let (_, env4) = send(
        ctx.app.clone(),
        json_request("GET", "/mail-digest/config", None, Some(&ctx.manager_token)),
    )
    .await;
    assert_eq!(
        env4["data"]["recipients"],
        json!(["new@example.com"]),
        "{env4}"
    );
}

// ===========================================================================
//  3. config_version 快照
// ===========================================================================

/// ★ 流水里的 `config_version` = **发送当时**的配置版本，改配置后不追认。
#[tokio::test]
async fn log_records_config_version_at_send_time() {
    let ctx = bootstrap_with_mail(unreachable_smtp()).await;

    // v0：首次初始化（INSERT，version=0）
    let (_, env) = save_config(&ctx, config_body(0)).await;
    assert_eq!(env["data"]["version"], 0, "{env}");

    // 发一次 → 流水应记 version=0
    let (status, _) = send(
        ctx.app.clone(),
        json_request(
            "POST",
            "/mail-digest/send",
            Some(json!({})),
            Some(&ctx.manager_token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);

    // v1：再存一次（version 0 → 1）
    let (_, env2) = save_config(&ctx, config_body(0)).await;
    assert_eq!(env2["data"]["version"], 1, "{env2}");

    let (_, env3) = get_logs(&ctx, "").await;
    assert_eq!(env3["data"]["total"], 1, "{env3}");
    assert_eq!(
        env3["data"]["items"][0]["config_version"], 0,
        "流水记的是发送当时的版本，不追认后续改动：{env3}"
    );
}

// ===========================================================================
//  4. 角色守卫
// ===========================================================================

/// CLERK 读流水 ⇒ 403。
#[tokio::test]
async fn clerk_cannot_read_logs_returns_403() {
    let ctx = bootstrap_with_mail(unreachable_smtp()).await;
    let (status, env) = send(
        ctx.app.clone(),
        json_request("GET", "/mail-digest/logs", None, Some(&ctx.clerk_token)),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{env}");
}

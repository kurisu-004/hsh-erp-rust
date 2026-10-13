//! mail_digest 发送端点（`POST /send`）集成测试
//!
//! 本文件覆盖 `/send` 的**全部失败路径**与「失败也落流水」这条核心不变量：
//! 1. `send_without_smtp_config_returns_21701_and_writes_no_log`
//! 2. `send_to_unreachable_smtp_returns_21702_and_records_failed_log`（★ 失败落流水）
//! 3. `send_with_header_injection_recipient_returns_21703_and_writes_no_log`
//! 4. `send_never_reports_success_when_smtp_unreachable`（不出现假 SUCCESS 流水）
//!
//! ## ⚠️ 覆盖空洞：`/send` 的**成功路径**无集成测试（2026-10-13 登记）
//! 测试环境没有可达的 SMTP 服务器，而本仓刻意**不**引入 lettre 的 mock transport
//! （那会给生产依赖树加一层只为测试存在的构造）。故「`status='SUCCESS'` 的流水行」
//! 与「真实投递成功」**未被任何自动化覆盖**。当前替代覆盖：
//! - 渲染 + 统计 SQL：`preview.rs` 全部用例（同一段 `template::render` + 同一对
//!   `repo::list_overdue` / `list_upcoming`）；
//! - 落流水：下面的「必失败」路径（`deliver` 里落流水在调 lettre **之后**、
//!   **无论成败**，两分支共用同一段代码）；
//! - SMTP 映射表（TLS 三档 → lettre 变体）与收件人解析：`mailer.rs` 的 lib 单测。
//!
//! 要补齐需要一个本地假 SMTP（如 `smtp-sink`）或进程内 mock transport，
//! 登记为后续增强，**不要**为了它在本域偷偷加 `#[cfg(test)]` 的生产代码分支。

use axum::http::StatusCode;
use serde_json::{Value, json};

use hsh_erp_test_support::{json_request, send};

use crate::{
    Ctx, assert_biz_code, bootstrap, bootstrap_with_sender_account, config_body, insert_part,
};

/// `POST /send` → `(status, 信封)`。
async fn send_mail(ctx: &Ctx, body: Value) -> (StatusCode, Value) {
    send(
        ctx.app.clone(),
        json_request(
            "POST",
            "/mail-digest/send",
            Some(body),
            Some(&ctx.manager_token),
        ),
    )
    .await
}

/// `GET /logs` → `(status, 信封)`。
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

/// 一份能进到 `deliver` 的合法收件人覆盖体。
fn ok_body() -> Value {
    json!({ "recipients": ["ops@example.com"] })
}

// ===========================================================================
//  1. 未配置 SMTP → 21701（503），且不落流水
// ===========================================================================

/// 未选发信邮箱（`sender_account_id` 为 NULL / 指向已删邮箱 / 授权码解不开）
/// ⇒ `21701` + **HTTP 503**。
///
/// ⚠️ 语义是「服务未就绪」而不是「调用方填错了」—— 与 `BIZ_WX_NOT_CONFIGURED`
/// (40109 → 503) 同款；**不得**兜底成 400，否则运维会以为是自己的参数问题。
/// 且这条路径**在渲染之前**就返回 ⇒ 不得产生任何流水行。
/// （2026-10-16 起判据从 env 换成 DB，见 `main.rs::bootstrap` 的 doc。）
#[tokio::test]
async fn send_without_smtp_config_returns_21701_and_writes_no_log() {
    let ctx = bootstrap().await;
    insert_part(
        &ctx.pool,
        ctx.customer_id,
        "SEND-1",
        "发送件",
        "IN_PROCESS",
        Some(crate::today() - chrono::Duration::days(1)),
        None,
        1,
        None,
        None,
    )
    .await;

    let (status, env) = send_mail(&ctx, ok_body()).await;
    assert_biz_code(status, &env, 21701, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        crate::count_logs(&ctx.pool).await,
        0,
        "未选定发信邮箱是**前置**拒绝，不得落流水：{env}"
    );
}

// ===========================================================================
//  2. SMTP 连不通 → 21702（502）+ 一行 FAILED 流水
// ===========================================================================

/// ★ `mailer::send` 在 SMTP 层失败（零网络 IO：`From` 头解析失败，见
/// `main.rs::TEST_ACCOUNT_NAME`）⇒ `21702` + HTTP 502，
/// **并且必须留下一行 `status='FAILED'` + `error_message` 的流水**。
///
/// 这是本域最重要的一条不变量：**落流水在发送之后、且不分成败**。
/// 只记成功会让「定时任务每天都在失败」完全不可观测 —— 而这恰是定时任务最常见的
/// 故障形态（授权码过期 / 收件人被拒 / TLS 模式错配）。
///
/// ⚠️ **造法在本轮改过（2026-10-16）**：原实现把 SMTP 指向 `127.0.0.1:1`
/// （端口 1 无监听 ⇒ connect 立刻 refused）。host/port 现在由**编译期预设表**固定
/// （网易 163 / QQ 都是 465 隐式 TLS），env 那套已整轮删除 ⇒ 那种「指定一个连不通
/// 的主机」的造法**不再可复现**。改用 `From` 头解析失败：`mailer::send` 的
/// `build_transport` 之后、`transport.send` 之前就 `Err`，网络 IO 为零，
/// 后续的「记 FAILED 流水 → 返 `21702`」两段代码**逐字未变**。
#[tokio::test]
async fn send_to_unreachable_smtp_returns_21702_and_records_failed_log() {
    let ctx = bootstrap_with_sender_account().await;
    insert_part(
        &ctx.pool,
        ctx.customer_id,
        "SEND-2",
        "发送件",
        "IN_PROCESS",
        Some(crate::today() - chrono::Duration::days(1)),
        None,
        1,
        None,
        None,
    )
    .await;

    let (status, env) = send_mail(&ctx, ok_body()).await;
    assert_biz_code(status, &env, 21702, StatusCode::BAD_GATEWAY);
    // 错误文本里应带失败原因（排障第一眼要看的那一样）
    let msg = env["message"].as_str().unwrap_or_default();
    assert!(
        msg.contains("发件人地址无法解析"),
        "错误信息应含失败原因：{env}"
    );
    assert!(msg.contains("log_id="), "应给出流水 id 作为排障指针：{env}");

    // 流水：1 行 FAILED + error_message 非空 + 计数与渲染一致
    let (status, env) = get_logs(&ctx, "").await;
    assert_eq!(status, StatusCode::OK, "{env}");
    assert_eq!(env["data"]["total"], 1, "必落 1 行流水：{env}");
    let item = &env["data"]["items"][0];
    assert_eq!(item["status"], "FAILED", "{env}");
    assert_eq!(item["trigger_source"], "MANUAL", "手动触发来源：{env}");
    assert_eq!(item["to_recipients"], json!(["ops@example.com"]), "{env}");
    assert_eq!(item["overdue_count"], 1, "{env}");
    let err_msg = item["error_message"].as_str().unwrap_or_default();
    assert!(!err_msg.is_empty(), "FAILED 行必须带 error_message：{env}");
    assert!(
        !err_msg.contains("dummy-password"),
        "流水里绝不能出现 SMTP 口令：{env}"
    );
    assert!(
        !err_msg.starts_with("[21702]"),
        "流水存裸文本，不带 AppError 的 `[21702] ` 前缀（否则会与 HTTP 层重复）：{env}"
    );
    assert!(
        item["html_bytes"].as_i64().unwrap() > 0,
        "应记录正文字节数：{env}"
    );
}

/// 发信失败时**不得**出现 `SUCCESS` 流水行（否则「失败被记成成功」不可排查）。
#[tokio::test]
async fn send_never_reports_success_when_sending_fails() {
    let ctx = bootstrap_with_sender_account().await;
    for _ in 0..2 {
        let (status, _) = send_mail(&ctx, ok_body()).await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
    }
    let (_, env) = get_logs(&ctx, "").await;
    assert_eq!(env["data"]["total"], 2, "两次失败 ⇒ 两行流水：{env}");
    for item in env["data"]["items"].as_array().expect("items 是数组") {
        assert_eq!(item["status"], "FAILED", "{item}");
    }
}

// ===========================================================================
//  3. 校验失败 → 21703，且不落流水
// ===========================================================================

/// 收件人含 CR/LF ⇒ `21703`，且**渲染前就拒** ⇒ 不产生任何流水行。
///
/// 对照第 2 条：SMTP 层失败**要**落流水（失败诊断需要），入参校验失败**不**落
/// （那不是一次「发送尝试」，落进去会污染「今天发过几封」这个数）。
#[tokio::test]
async fn send_with_header_injection_recipient_returns_21703_and_writes_no_log() {
    let ctx = bootstrap_with_sender_account().await;
    let (status, env) = send_mail(
        &ctx,
        json!({ "recipients": ["ops@example.com\r\nBcc: victim@x.com"] }),
    )
    .await;
    assert_biz_code(status, &env, 21703, StatusCode::BAD_REQUEST);
    assert_eq!(
        crate::count_logs(&ctx.pool).await,
        0,
        "校验失败发生在 deliver 之前，不得落流水：{env}"
    );
}

/// 配置组合非法（如两节都关）⇒ `21704`，同样不落流水。
#[tokio::test]
async fn send_with_invalid_config_combination_returns_21704_and_writes_no_log() {
    let ctx = bootstrap_with_sender_account().await;
    let (status, env) = send_mail(
        &ctx,
        json!({ "include_overdue": false, "include_upcoming": false }),
    )
    .await;
    assert_biz_code(status, &env, 21704, StatusCode::BAD_REQUEST);
    assert_eq!(crate::count_logs(&ctx.pool).await, 0, "{env}");
}

// ===========================================================================
//  4. 角色守卫
// ===========================================================================

/// CLERK 调 `/send` ⇒ 403（端点全 MANAGER 独占；越权者**发不出信**）。
#[tokio::test]
async fn clerk_cannot_send_mail_returns_403() {
    let ctx = bootstrap_with_sender_account().await;
    let (status, env) = send(
        ctx.app.clone(),
        json_request(
            "POST",
            "/mail-digest/send",
            Some(ok_body()),
            Some(&ctx.clerk_token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{env}");
    assert_eq!(
        crate::count_logs(&ctx.pool).await,
        0,
        "越权请求不该产生任何副作用"
    );
}

// ===========================================================================
//  5. 保存配置会触发调度热重载（不 500）
// ===========================================================================

/// `POST /config` 成功后 handler 调 `task::mail_digest::reschedule`。
/// 本用例钉的是「热重载**永不**把保存接口搞挂」：未选发信邮箱时热重载走
/// 「摘旧 job、不挂新 job」的分支，保存接口照常 200。
#[tokio::test]
async fn save_config_survives_scheduler_reschedule() {
    let ctx = bootstrap().await;
    let (status, env) = send(
        ctx.app.clone(),
        json_request(
            "POST",
            "/mail-digest/config",
            Some(config_body(0)),
            Some(&ctx.manager_token),
        ),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "热重载失败不得让保存接口 500：{env}"
    );
    assert_eq!(env["data"]["frequency"], "DAILY", "{env}");
}
// ===========================================================================
//  6. 定时路径的准备阶段失败**也**落流水（2026-10-13 review 第 1 轮 M8）
// ===========================================================================

/// ★ `SCHEDULED` 触发时**准备阶段**失败（配置解析失败 / 收件人为空），必须留下
/// 一行 `status='FAILED'` 的流水。
///
/// 修复前这条路径直接 `return`，`deliver` 根本没被调用 ⇒ `t_mail_digest_log` 里查不到
/// 任何东西，「定时任务每天都在失败」只剩一行 `tracing::warn!` —— 而容器日志恰恰是
/// 最容易被轮转掉、最难按时间线回看的那一份。
///
/// 本用例直调 `service::send_scheduled`（cron job 闭包就是调它，没有 HTTP 端点），
/// 并**绕过 API 直连 UPDATE** 把 `frequency` 改成脏值来制造「准备阶段失败」——
/// 走 API 是改不出脏值的（写侧会拒），这正是「真实脏数据」的样子。
#[tokio::test]
async fn scheduled_prepare_failure_still_writes_failed_log() {
    let ctx = bootstrap_with_sender_account().await;

    // 先存一份合法配置，再绕过写侧把 frequency 改成非法值（写侧永远不会产生这种行）
    let (status, env) = send(
        ctx.app.clone(),
        json_request(
            "POST",
            "/mail-digest/config",
            Some(config_body(0)),
            Some(&ctx.manager_token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "前置：合法配置应保存成功：{env}");
    sqlx::query("UPDATE t_mail_digest_config SET frequency = 'EVERY_2_HOURS'")
        .execute(&ctx.pool)
        .await
        .expect("把 frequency 改成脏值");

    assert_eq!(
        crate::count_logs(&ctx.pool).await,
        0,
        "前置：此刻还没有任何流水"
    );

    // cron job 闭包调的就是这个函数
    hsh_erp_rust::modules::mail_digest::service::send_scheduled(&ctx.state).await;

    // ★ 核心断言：准备阶段失败**也**落了一行 FAILED 流水
    assert_eq!(
        crate::count_logs(&ctx.pool).await,
        1,
        "定时准备阶段失败必须落流水，否则「每天都在失败」不可观测"
    );
    let (status, env) = get_logs(&ctx, "").await;
    assert_eq!(status, StatusCode::OK, "{env}");
    let item = &env["data"]["items"][0];
    assert_eq!(item["status"], "FAILED", "{item}");
    assert_eq!(item["trigger_source"], "SCHEDULED", "{item}");
    assert_eq!(item["html_bytes"], 0, "没渲染 ⇒ 无正文字节：{item}");
    assert_eq!(item["overdue_count"], 0, "{item}");
    assert_eq!(item["upcoming_count"], 0, "{item}");
    assert_eq!(item["subject"], "", "没渲染 ⇒ 无主题快照：{item}");
    let msg = item["error_message"].as_str().unwrap_or_default();
    assert!(
        msg.contains("EVERY_2_HOURS"),
        "error_message 应带上原始失败原因：{msg}"
    );
}

/// 准备阶段的第二种失败：**收件人为空**。同样必须落流水。
///
/// 与上一条的区别：`current_normalized` 成功（配置合法）、失败点在 `deliver` 之前
/// 的收件人非空检查 —— 这正是「配置页允许先存空收件人」的代价，定时路径下没有调用方
/// 看得见错误码，流水是唯一的痕迹。
#[tokio::test]
async fn scheduled_empty_recipients_still_writes_failed_log() {
    let ctx = bootstrap_with_sender_account().await;

    let (status, env) = send(
        ctx.app.clone(),
        json_request(
            "POST",
            "/mail-digest/config",
            Some(config_body(0)),
            Some(&ctx.manager_token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{env}");

    // 配置页**允许**存空收件人（编辑中途的正常形态，§8.3「实现与注释不符」第 1 条）
    let (_, env) = crate::config::get_config(&ctx).await;
    let version = env["data"]["version"].as_i64().unwrap() as i32;
    let mut body = config_body(version);
    body["recipients"] = json!([]);
    let (status, env) = crate::config::save_config(&ctx, body).await;
    assert_eq!(status, StatusCode::OK, "配置页允许空收件人：{env}");

    hsh_erp_rust::modules::mail_digest::service::send_scheduled(&ctx.state).await;

    assert_eq!(
        crate::count_logs(&ctx.pool).await,
        1,
        "定时发现收件人为空也必须落流水"
    );
    let (status, env) = get_logs(&ctx, "").await;
    assert_eq!(status, StatusCode::OK, "{env}");
    let item = &env["data"]["items"][0];
    assert_eq!(item["status"], "FAILED", "{item}");
    assert_eq!(item["trigger_source"], "SCHEDULED", "{item}");
    let msg = item["error_message"].as_str().unwrap_or_default();
    assert!(msg.contains("收件人为空"), "{msg}");
}

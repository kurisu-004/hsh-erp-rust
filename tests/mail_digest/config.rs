//! mail_digest 配置端点（`GET /config` + `POST /config`）集成测试
//!
//! 覆盖 12 组场景（每条钉一个契约）：
//! 1. `config_empty_table_returns_defaults_with_zero_version` —— 表空返**默认值 +
//!    `version=0` + `id="0"`**（不是 404）：配置页要在「还没配过」时能打开。
//! 2. `config_save_without_version_field_returns_422_plain_text` —— OCC 锚点缺失
//!    ⇒ axum `Json` 提取器返 **HTTP 422 纯文本**，不是业务信封（CLAUDE.md §8）。
//! 3. `config_save_with_stale_version_returns_409` —— 标准 OCC 冲突。
//! 4. `weekly_without_weekday_returns_21704` / `monthly_without_day_of_month_returns_21704`
//! 5. `monthly_day_of_month_31_returns_21704`（上限 28，刻意避开短月）
//! 6. `daily_with_weekday_field_is_cleared_not_rejected` —— ⚠️ 实际契约与
//!    「多填即非法」的直觉相反，见文末「本文件钉住的两处偏差」。
//! 7. `recipient_without_at_returns_21703` / `recipient_with_crlf_returns_21703`
//!    （SMTP **header 注入**面：入库前拒，不能指望 lettre 兜底）
//! 8. `config_accepts_empty_recipients_but_send_rejects_it` —— ⚠️ 见文末偏差登记。
//! 9. `upcoming_days_999_is_clamped_to_60` —— 越界值被 clamp，不是拒绝。
//! 10. `unknown_visible_column_returns_21704`（不静默丢弃）。
//! 11. `save_twice_bumps_version_by_one`
//! 12. `clerk_cannot_read_config_returns_403`（6 端点全 MANAGER 独占）

use axum::http::StatusCode;
use serde_json::json;

use hsh_erp_test_support::{json_request, send, send_raw};

use crate::{Ctx, assert_biz_code, bootstrap, config_body, today};

/// `POST /config` → 返回 `(status, 信封)`。
async fn save_config(ctx: &Ctx, body: serde_json::Value) -> (StatusCode, serde_json::Value) {
    send(
        ctx.app.clone(),
        json_request("POST", "/mail-digest/config", Some(body), Some(&ctx.manager_token)),
    )
    .await
}

/// `GET /config` → 返回 `(status, 信封)`。
async fn get_config(ctx: &Ctx) -> (StatusCode, serde_json::Value) {
    send(
        ctx.app.clone(),
        json_request("GET", "/mail-digest/config", None, Some(&ctx.manager_token)),
    )
    .await
}

// ===========================================================================
//  1. 表空时读配置
// ===========================================================================

/// 表里无行时 `GET /config` 返**全部默认值**而非 404，且 `version=0` / `id="0"`。
///
/// `recipients` 缺省是**空数组**（刻意不塞占位地址，否则运维会以为配好了）。
#[tokio::test]
async fn config_empty_table_returns_defaults_with_zero_version() {
    let ctx = bootstrap().await;
    let (status, env) = get_config(&ctx).await;

    assert_eq!(status, StatusCode::OK, "{env}");
    let d = &env["data"];
    assert_eq!(d["version"], 0, "表空时 version 必须是 0：{d}");
    assert_eq!(d["id"], "0", "表空时 id 必须是 JSON string \"0\"：{d}");
    assert_eq!(d["enabled"], false);
    assert_eq!(d["frequency"], "DAILY");
    assert_eq!(d["send_hour"], 8);
    assert_eq!(d["send_minute"], 0);
    assert_eq!(d["upcoming_days"], 14);
    assert_eq!(d["include_overdue"], true);
    assert_eq!(d["include_upcoming"], true);
    assert_eq!(d["recipients"], json!([]), "缺省收件人是空数组而非占位地址");
    // 时间戳在「尚未创建」时给固定空串（不是会跳动的假时间）
    assert_eq!(d["created_at"], "");
    assert_eq!(d["updated_at"], "");
    // SMTP 未配置时 smtp_configured=false（前端据此灰掉「立即发送」）
    assert_eq!(d["smtp_configured"], false);
    // 缺省列白名单 = 8 列（row_type / is_urgent / order_no 不在缺省集里）
    assert_eq!(
        d["visible_columns"],
        json!([
            "serial_no",
            "drawing_no",
            "name",
            "quantity",
            "customer_name",
            "status",
            "system_delivery_date",
            "remaining_days"
        ])
    );
}

// ===========================================================================
//  2. 缺 version → 422 纯文本
// ===========================================================================

/// body 缺 `version` ⇒ **HTTP 422 纯文本**（axum `Json` 提取器），**不是**业务信封。
///
/// 断言点：body 不是 JSON（解析必失败）⇒ 说明前端拿不到 `{code, message, data}`，
/// 不能按业务码分支处理，只能当成「请求体不合法」。
#[tokio::test]
async fn config_save_without_version_field_returns_422_plain_text() {
    let ctx = bootstrap().await;
    let mut body = config_body(0);
    body.as_object_mut().expect("config_body 是对象").remove("version");

    let (status, raw) = send_raw(
        ctx.app.clone(),
        json_request(
            "POST",
            "/mail-digest/config",
            Some(body),
            Some(&ctx.manager_token),
        ),
    )
    .await;

    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "body={raw}");
    assert!(
        serde_json::from_str::<serde_json::Value>(&raw).is_err(),
        "422 必须是**纯文本**（不是 R 信封），实际 body={raw}"
    );
    assert!(!raw.contains("\"code\""), "422 body 不该带业务 code：{raw}");
}

// ===========================================================================
//  3. 过期 version → 409 / 40901
// ===========================================================================

/// `version` 与库中不符 ⇒ HTTP 409 + `40901 VERSION_CONFLICT`。
#[tokio::test]
async fn config_save_with_stale_version_returns_409() {
    let ctx = bootstrap().await;
    // 先建一行（首次初始化写 version=0）
    let (st, env) = save_config(&ctx, config_body(0)).await;
    assert_eq!(st, StatusCode::OK, "首次初始化应成功：{env}");
    assert_eq!(env["data"]["version"], 0);

    // 拿一个一定对不上的版本号
    let (status, env) = save_config(&ctx, config_body(99)).await;
    assert_biz_code(status, &env, 40901, StatusCode::CONFLICT);
    // 冲突后配置行内容不受影响（仍是首轮那份）
    let (_, env) = get_config(&ctx).await;
    assert_eq!(env["data"]["version"], 0, "冲突不应改动配置行：{env}");
}

// ===========================================================================
//  4. 频率专属字段缺失
// ===========================================================================

/// `WEEKLY` 缺 `weekday` ⇒ `21704 BIZ_MAIL_CONFIG_INVALID`（400）。
#[tokio::test]
async fn weekly_without_weekday_returns_21704() {
    let ctx = bootstrap().await;
    let mut body = config_body(0);
    body["frequency"] = json!("WEEKLY");
    let (status, env) = save_config(&ctx, body).await;
    assert_biz_code(status, &env, 21704, StatusCode::BAD_REQUEST);
}

/// `MONTHLY` 缺 `day_of_month` ⇒ `21704`。
#[tokio::test]
async fn monthly_without_day_of_month_returns_21704() {
    let ctx = bootstrap().await;
    let mut body = config_body(0);
    body["frequency"] = json!("MONTHLY");
    let (status, env) = save_config(&ctx, body).await;
    assert_biz_code(status, &env, 21704, StatusCode::BAD_REQUEST);
}

/// `MONTHLY` + `day_of_month=31` ⇒ `21704`（**上限 28**：刻意避开 2 月与 30/31 日短月）。
#[tokio::test]
async fn monthly_day_of_month_31_returns_21704() {
    let ctx = bootstrap().await;
    let mut body = config_body(0);
    body["frequency"] = json!("MONTHLY");
    body["day_of_month"] = json!(31);
    let (status, env) = save_config(&ctx, body).await;
    assert_biz_code(status, &env, 21704, StatusCode::BAD_REQUEST);

    // 边界 28 必须能存（只钉上界，不误伤合法值）
    let mut ok_body = config_body(0);
    ok_body["frequency"] = json!("MONTHLY");
    ok_body["day_of_month"] = json!(28);
    let (status, env) = save_config(&ctx, ok_body).await;
    assert_eq!(status, StatusCode::OK, "day_of_month=28 是合法边界：{env}");
    assert_eq!(env["data"]["day_of_month"], 28);
}

// ===========================================================================
//  5. 非专属频率多填专属字段
// ===========================================================================

/// ⚠️ **实际契约：`DAILY` / `MANUAL_ONLY` 之下多填 `weekday` 不报 21704，而是被
/// **清空**（落库为 NULL）。**
///
/// 理由（`service::validate_save_request`）：`weekday` 对这些频率不参与 cron 拼装，
/// 留着是「脏值」——读侧会照单全收地拼 cron。写侧选择主动清 NULL 而不是拒，
/// 代价是「多填了字段」这件事**不**在写侧暴露给前端。
/// ⇒ 见 `docs/api/mail_digest.md` §8.3「已知偏差登记」第 1 条。
#[tokio::test]
async fn daily_with_weekday_field_is_cleared_not_rejected() {
    let ctx = bootstrap().await;
    let mut body = config_body(0);
    body["frequency"] = json!("DAILY");
    body["weekday"] = json!(3);

    let (status, env) = save_config(&ctx, body).await;
    assert_eq!(status, StatusCode::OK, "DAILY + weekday 应被清空而非拒：{env}");
    assert_eq!(env["data"]["weekday"], serde_json::Value::Null, "{env}");

    // 复读确认真的落库成 NULL（不是响应层临时抹掉）
    let (_, env) = get_config(&ctx).await;
    assert_eq!(env["data"]["weekday"], serde_json::Value::Null, "{env}");

    // MANUAL_ONLY 同款（weekday / day_of_month 被清空）
    let mut body2 = config_body(env["data"]["version"].as_i64().unwrap() as i32);
    body2["frequency"] = json!("MANUAL_ONLY");
    body2["weekday"] = json!(5);
    body2["day_of_month"] = json!(9);
    let (status, env2) = save_config(&ctx, body2).await;
    assert_eq!(status, StatusCode::OK, "MANUAL_ONLY + weekday 应被清空：{env2}");
    assert_eq!(env2["data"]["weekday"], serde_json::Value::Null);
    assert_eq!(env2["data"]["day_of_month"], serde_json::Value::Null);
}

/// ⚠️ **实际契约：`MANUAL_ONLY` 之下 `send_hour` / `send_minute` 既不清空、也不校验范围。**
///
/// migration 的列注释写的是「`MANUAL_ONLY` → 四个字段全部不参与，写侧一律清 NULL」，
/// 但写侧只清 `weekday` / `day_of_month`；`send_hour` / `send_minute` 是**原样透传**，
/// 且 `validate_time` 对 `MANUAL_ONLY` 直接早退 ⇒ 连 `0..23` 都不查（能存进 99）。
/// 危害有限（`MANUAL_ONLY` 永不产生 cron，这两个字段不会被读），但文档与列注释
/// 需按实现订正。⇒ `docs/api/mail_digest.md` §8.3 偏差 4。
#[tokio::test]
async fn manual_only_keeps_send_time_unchecked() {
    let ctx = bootstrap().await;
    let mut body = config_body(0);
    body["frequency"] = json!("MANUAL_ONLY");
    body["send_hour"] = json!(99);
    body["send_minute"] = json!(88);

    let (status, env) = save_config(&ctx, body).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "MANUAL_ONLY 下时刻字段既不清空也不校验（实际行为）：{env}"
    );
    assert_eq!(env["data"]["send_hour"], 99, "{env}");
    assert_eq!(env["data"]["send_minute"], 88, "{env}");

    // 对照：DAILY 下同样越界的值会被 21704 拒（校验按频率生效）
    let mut daily = config_body(env["data"]["version"].as_i64().unwrap() as i32);
    daily["frequency"] = json!("DAILY");
    daily["send_hour"] = json!(99);
    let (status, env2) = save_config(&ctx, daily).await;
    assert_biz_code(status, &env2, 21704, StatusCode::BAD_REQUEST);
}

// ===========================================================================
//  6. 收件人校验
// ===========================================================================

/// 邮箱不含 `@` ⇒ `21703 BIZ_MAIL_RECIPIENT_INVALID`（400）。
#[tokio::test]
async fn recipient_without_at_returns_21703() {
    let ctx = bootstrap().await;
    let mut body = config_body(0);
    body["recipients"] = json!(["not-an-email"]);
    let (status, env) = save_config(&ctx, body).await;
    assert_biz_code(status, &env, 21703, StatusCode::BAD_REQUEST);
}

/// ⚠️ 地址含 `\r\n` ⇒ `21703`：**SMTP header 注入**面。
///
/// 攻击者把 `a@b.com\r\nBcc: victim@x.com` 塞进收件人，就能让本系统替他发信给任意
/// 第三方。必须在**入库前**拒掉，不能指望 lettre 兜底（lettre 确实也会拒，但那时
/// 已经进库了）。⇒ 本用例钉的是「写侧第一道闸就拦住」。
#[tokio::test]
async fn recipient_with_crlf_returns_21703() {
    let ctx = bootstrap().await;
    let mut body = config_body(0);
    body["recipients"] = json!(["ops@example.com\r\nBcc: victim@x.com"]);
    let (status, env) = save_config(&ctx, body).await;
    assert_biz_code(status, &env, 21703, StatusCode::BAD_REQUEST);

    // 抄送同样受检（不能只拦 To）
    let mut body2 = config_body(0);
    body2["cc_recipients"] = json!(["cc@example.com\nX-Injected: 1"]);
    let (status, env2) = save_config(&ctx, body2).await;
    assert_biz_code(status, &env2, 21703, StatusCode::BAD_REQUEST);
}

// ===========================================================================
//  7. 空收件人
// ===========================================================================

/// ⚠️ **实际契约：`POST /config` 接受空 `recipients`（落库为空数组），
/// 而 `POST /send` 才拒（21704）。**
///
/// 即「收件人非空」这条业务不变量**只**在发送路径上生效，配置页可以先存一份
/// 空配置（编辑中途的正常形态）。`service::validate_save_request` 的模块 doc 与
/// `NormalizedConfig::default` 的注释都写着「空数组会被 POST /config 校验当场拦下」，
/// 与实现不符 ⇒ 见 `docs/api/mail_digest.md` §8.3「已知偏差登记」第 2 条。
/// 本用例把**实际**行为两侧都钉死，让这条偏差在测试层可见而不是只存在于文档里。
#[tokio::test]
async fn config_accepts_empty_recipients_but_send_rejects_it() {
    // ⚠️ 必须用「已配置 SMTP」的上下文：`deliver` 里**先**判 SMTP 就绪（21701）
    // 再判收件人非空（21704），SMTP 没配时根本走不到第二条。
    let ctx = crate::bootstrap_with_mail(crate::unreachable_smtp()).await;
    let mut body = config_body(0);
    body["recipients"] = json!([]);

    let (status, env) = save_config(&ctx, body).await;
    assert_eq!(status, StatusCode::OK, "配置页允许先存空收件人：{env}");
    assert_eq!(env["data"]["recipients"], json!([]));

    // 发送路径才是真正的闸：空收件人 → 21704，且**不**产生流水
    let (status, env) = send(
        ctx.app.clone(),
        json_request("POST", "/mail-digest/send", Some(json!({})), Some(&ctx.manager_token)),
    )
    .await;
    assert_biz_code(status, &env, 21704, StatusCode::BAD_REQUEST);
    assert_eq!(crate::count_logs(&ctx.pool).await, 0, "校验失败不得落流水：{env}");
}

// ===========================================================================
//  8. upcoming_days clamp
// ===========================================================================

/// `upcoming_days=999` ⇒ **被 clamp 到 60**（不是拒绝、不是报错）。
#[tokio::test]
async fn upcoming_days_999_is_clamped_to_60() {
    let ctx = bootstrap().await;
    let mut body = config_body(0);
    body["upcoming_days"] = json!(999);
    let (status, env) = save_config(&ctx, body).await;
    assert_eq!(status, StatusCode::OK, "{env}");
    assert_eq!(env["data"]["upcoming_days"], 60, "999 应被 clamp 到 60：{env}");

    // 下界同理（0 → 1）
    let mut body2 = config_body(0);
    body2["upcoming_days"] = json!(0);
    let (status, env2) = save_config(&ctx, body2).await;
    assert_eq!(status, StatusCode::OK, "{env2}");
    assert_eq!(env2["data"]["upcoming_days"], 1, "0 应被 clamp 到 1：{env2}");
}

// ===========================================================================
//  9. 列白名单
// ===========================================================================

/// `visible_columns` 含未知列 ⇒ `21704`（**不静默丢弃**：静默丢弃会让人以为列生效）。
#[tokio::test]
async fn unknown_visible_column_returns_21704() {
    let ctx = bootstrap().await;
    let mut body = config_body(0);
    body["visible_columns"] = json!(["drawing_no", "no_such_column"]);
    let (status, env) = save_config(&ctx, body).await;
    assert_biz_code(status, &env, 21704, StatusCode::BAD_REQUEST);
    // 错误 message 应带合法列名清单（运维照着改，不用猜）
    let msg = env["message"].as_str().unwrap_or_default();
    assert!(msg.contains("no_such_column"), "{env}");
    assert!(msg.contains("drawing_no"), "错误信息应列合法值：{env}");
}

// ===========================================================================
//  10. version 自增
// ===========================================================================

/// 连续两次保存：`version` 0 → 1，且响应回填的是**提交后**的新版本。
///
/// 钉的是「响应里的 version 是读回的新值」——前端拿提交前的 version 做下一次提交
/// 会立刻撞一次假的 OCC 冲突。
#[tokio::test]
async fn save_twice_bumps_version_by_one() {
    let ctx = bootstrap().await;
    let (st, env1) = save_config(&ctx, config_body(0)).await;
    assert_eq!(st, StatusCode::OK, "{env1}");
    let v1 = env1["data"]["version"].as_i64().expect("version 是数字");
    assert_eq!(v1, 0, "首次初始化落 version=0（INSERT 而非 UPDATE）");

    let (st, env2) = save_config(&ctx, config_body(v1 as i32)).await;
    assert_eq!(st, StatusCode::OK, "{env2}");
    assert_eq!(env2["data"]["version"].as_i64(), Some(v1 + 1), "{env2}");

    let (_, env3) = get_config(&ctx).await;
    assert_eq!(env3["data"]["version"].as_i64(), Some(v1 + 1), "{env3}");
    // id 在首次 INSERT 后固定为真雪花（JSON string）
    assert_ne!(env3["data"]["id"], "0", "保存后 id 应是真雪花：{env3}");
}

// ===========================================================================
//  11. 角色守卫
// ===========================================================================

/// CLERK 调配置端点 ⇒ HTTP 403（6 端点全 MANAGER 独占）。
#[tokio::test]
async fn clerk_cannot_read_config_returns_403() {
    let ctx = bootstrap().await;
    let (status, env) = send(
        ctx.app.clone(),
        json_request("GET", "/mail-digest/config", None, Some(&ctx.clerk_token)),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{env}");

    // 写端点同样拒绝
    let (status, env) = send(
        ctx.app.clone(),
        json_request(
            "POST",
            "/mail-digest/config",
            Some(config_body(0)),
            Some(&ctx.clerk_token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{env}");
}

// ===========================================================================
//  12. 时间基准自检（防「用例里写死绝对日期」复发）
// ===========================================================================

/// 本域全部断言围绕 today±N 天；该用例把基准钉在**运行时的今天**上，
/// 任何人把某条用例的日期换成字面量时，至少这里会提示基准是活的。
#[tokio::test]
async fn date_window_is_relative_to_runtime_today() {
    let ctx = bootstrap().await;
    let t = today();
    assert_eq!(
        t,
        now_naive_date(),
        "today() 必须与 infra::clock::now_naive() 同源（Asia/Shanghai）"
    );
    // 建一个 today-3 的逾期工单，验证它落在逾期节
    let id = crate::insert_part(
        &ctx.pool,
        ctx.customer_id,
        "DWG-REL",
        "相对日期回归件",
        "IN_PROCESS",
        Some(t - chrono::Duration::days(3)),
        None,
        1,
        None,
        None,
    )
    .await;
    let _ = id;
    let (status, env) = send(
        ctx.app.clone(),
        json_request("POST", "/mail-digest/preview", Some(json!({})), Some(&ctx.manager_token)),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{env}");
    assert_eq!(env["data"]["overdue_count"], 1, "today-3 的件应在逾期节：{env}");
}

/// `now_naive().date()` 的本地别名（避免在断言里混用两套时间来源）。
fn now_naive_date() -> chrono::NaiveDate {
    hsh_erp_rust::infra::clock::now_naive().date()
}
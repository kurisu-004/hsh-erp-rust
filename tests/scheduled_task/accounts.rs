//! `/api/v2/mail` 的 `GET /meta`、`GET /accounts`、`POST /accounts`、
//! `POST /accounts/{id}`、`POST /accounts/{id}/set-default` 契约。
//!
//! ★ 核心回归：**授权码加密入库 + 出参里一个字都不回显**（掩码也不回显）。

use axum::http::StatusCode;
use serde_json::{Value, json};

use crate::{
    Ctx, assert_biz_code, bootstrap, create_account, create_body, get_as_manager, id_of,
    open_with_test_key, post_as_clerk, post_as_manager, post_raw, stored_name_and_version,
    stored_secret, unique_name,
};

// ===========================================================================
//  1. GET /meta —— 预设表
// ===========================================================================

/// `GET /meta` 返回两条预设，host / port / tls_mode 逐字钉死。
///
/// 这三项是**前端下拉里会显示出来**的（host），也是发信时的连接参数与 TLS SNI，
/// 属对外契约 —— lib 单测 `preset_smoke_parameters_are_pinned` 守同一组字面量，
/// 这里守的是「真的把它们送到了前端」。
#[tokio::test]
async fn meta_lists_both_presets_with_smoke_parameters() {
    let ctx = bootstrap().await;
    let (status, env) = get_as_manager(&ctx, "/mail/meta").await;
    assert_eq!(status, StatusCode::OK, "{env}");

    let presets = env["data"]["presets"].as_array().expect("presets 是数组");
    assert_eq!(presets.len(), 2, "只做网易 163 与 QQ 两家：{env}");
    assert_eq!(presets[0]["value"], "NET_EASE_163");
    assert_eq!(presets[0]["label"], "网易 163");
    assert_eq!(presets[0]["host"], "smtp.163.com");
    assert_eq!(presets[0]["port"], 465);
    assert_eq!(presets[0]["tls_mode"], "WRAPPED");
    assert_eq!(presets[1]["value"], "TENCENT_QQ");
    assert_eq!(presets[1]["host"], "smtp.qq.com");
    assert_eq!(presets[1]["port"], 465);
    assert_eq!(presets[1]["tls_mode"], "WRAPPED");

    // ⚠️ 出参里**一个字都不能有** secret / mask / password
    let raw = env.to_string();
    assert!(!raw.to_lowercase().contains("secret"), "{raw}");
    assert!(!raw.to_lowercase().contains("mask"), "{raw}");
}

/// `GET /meta` 只认 MANAGER（CLERK ⇒ 403）。
#[tokio::test]
async fn meta_rejects_non_manager() {
    let ctx = bootstrap().await;
    let (status, env) = hsh_erp_test_support::send(
        ctx.app.clone(),
        hsh_erp_test_support::json_request("GET", "/mail/meta", None, Some(&ctx.clerk_token)),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{env}");
}

// ===========================================================================
//  2. POST /accounts —— 建邮箱（密文入库 + 不回显）
// ===========================================================================

/// ★ 建邮箱：授权码**加密入库**（库里不是明文），出参里**一个字都不回显**。
#[tokio::test]
async fn create_encrypts_secret_and_never_echoes_it() {
    let ctx = bootstrap().await;
    let name = unique_name("加密入库");
    let account = create_account(&ctx, &name).await;

    // 出参：没有 secret 任何形式的影子
    let raw = account.to_string();
    assert!(
        !raw.contains("abcdef1234567890"),
        "出参回显了授权码明文：{raw}"
    );
    assert!(
        !raw.to_lowercase().contains("secret"),
        "出参出现 secret 字段：{raw}"
    );
    assert!(
        !raw.to_lowercase().contains("mask"),
        "出参出现掩码字段：{raw}"
    );

    // 出参的公开信息齐（前端要显示的）
    assert_eq!(account["name"], name);
    assert_eq!(account["preset"], "NET_EASE_163");
    assert_eq!(account["preset_label"], "网易 163");
    assert_eq!(account["account"], "ops@163.com");
    assert_eq!(account["host"], "smtp.163.com");
    assert_eq!(account["port"], 465);
    assert_eq!(account["tls_mode"], "WRAPPED");
    // ⚠️ 新建**不**设默认（置默认是独立端点的事）
    assert_eq!(account["is_default"], false);
    // id 是 JSON string
    assert!(account["id"].is_string(), "id 必须是 JSON string：{raw}");

    // 库里确实是密文，且能用测试主密钥解回明文
    let sealed = stored_secret(&ctx.pool, id_of(&account)).await;
    assert_ne!(sealed, "abcdef1234567890", "库里不能是明文");
    assert_eq!(open_with_test_key(&sealed), "abcdef1234567890");
}

/// 列表同样不回显密文，且**默认邮箱排最前**。
#[tokio::test]
async fn list_accounts_hides_secret_and_orders_default_first() {
    let ctx = bootstrap().await;
    let a = create_account(&ctx, &unique_name("甲邮箱")).await;
    let b = create_account(&ctx, &unique_name("乙邮箱")).await;

    // 置 b 为默认
    let (status, env) = post_as_manager(
        &ctx,
        &format!("/mail/accounts/{}/set-default", id_of(&b)),
        json!({ "version": b["version"] }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{env}");
    assert_eq!(env["data"]["is_default"], true, "{env}");

    let (status, env) = get_as_manager(&ctx, "/mail/accounts").await;
    assert_eq!(status, StatusCode::OK, "{env}");
    let items = env["data"]["items"].as_array().expect("items 是数组");
    assert_eq!(items.len(), 2, "{env}");
    assert_eq!(items[0]["id"], b["id"], "默认邮箱必须排最前：{env}");
    assert_eq!(items[1]["id"], a["id"], "{env}");

    let raw = env.to_string();
    assert!(!raw.to_lowercase().contains("secret"), "{raw}");
    assert!(!raw.to_lowercase().contains("mask"), "{raw}");
    assert!(!raw.contains("abcdef1234567890"), "{raw}");
}

/// 名称重复 ⇒ `21707` + HTTP 409（**不是** 40901 —— 40901 只表达 OCC）。
#[tokio::test]
async fn duplicate_name_returns_21707() {
    let ctx = bootstrap().await;
    let name = unique_name("重名");
    create_account(&ctx, &name).await;

    let (status, env) = post_as_manager(&ctx, "/mail/accounts", create_body(&name)).await;
    assert_biz_code(status, &env, 21707, StatusCode::CONFLICT);
}

/// 未知预设 ⇒ `21704` + HTTP 400，且错误信息列出合法值。
#[tokio::test]
async fn unknown_preset_returns_21704_with_legal_values() {
    let ctx = bootstrap().await;
    let mut body = create_body(&unique_name("坏预设"));
    body["preset"] = json!("OUTLOOK");
    let (status, env) = post_as_manager(&ctx, "/mail/accounts", body).await;
    assert_biz_code(status, &env, 21704, StatusCode::BAD_REQUEST);
    let msg = env["message"].as_str().unwrap_or_default();
    assert!(
        msg.contains("NET_EASE_163") && msg.contains("TENCENT_QQ"),
        "{env}"
    );
}

/// 授权码为空 ⇒ `21704`（不存在「先建邮箱、以后再补授权码」）。
#[tokio::test]
async fn empty_secret_returns_21704() {
    let ctx = bootstrap().await;
    let mut body = create_body(&unique_name("空授权码"));
    body["secret"] = json!("   ");
    let (status, env) = post_as_manager(&ctx, "/mail/accounts", body).await;
    assert_biz_code(status, &env, 21704, StatusCode::BAD_REQUEST);
}

/// 建邮箱是 MANAGER 独占。
#[tokio::test]
async fn create_rejects_non_manager() {
    let ctx = bootstrap().await;
    let (status, env) =
        post_as_clerk(&ctx, "/mail/accounts", create_body(&unique_name("越权"))).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{env}");
}

// ===========================================================================
//  3. POST /accounts/{id} —— 覆写 + OCC + 授权码「沿用」
// ===========================================================================

/// `secret` 缺省 ⇒ **沿用旧密文**（管理员改个名称时前端拿不到明文）。
#[tokio::test]
async fn update_without_secret_keeps_old_ciphertext() {
    let ctx = bootstrap().await;
    let name = unique_name("沿用密文");
    let created = create_account(&ctx, &name).await;
    let sealed_before = stored_secret(&ctx.pool, id_of(&created)).await;

    let (status, env) = post_as_manager(
        &ctx,
        &format!("/mail/accounts/{}", id_of(&created)),
        json!({
            "version": created["version"],
            "name": format!("{name}-改名"),
            "preset": "TENCENT_QQ",
            "account": "ops@qq.com",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{env}");
    assert_eq!(env["data"]["name"], format!("{name}-改名"));
    assert_eq!(env["data"]["preset"], "TENCENT_QQ");
    assert_eq!(env["data"]["host"], "smtp.qq.com", "换预设后 host 跟着换");
    assert_eq!(
        env["data"]["version"].as_i64().unwrap(),
        created["version"].as_i64().unwrap() + 1
    );

    // 密文逐字未变
    let sealed_after = stored_secret(&ctx.pool, id_of(&created)).await;
    assert_eq!(sealed_before, sealed_after, "没传 secret 时不得重写密文");
    assert_eq!(open_with_test_key(&sealed_after), "abcdef1234567890");
}

/// `secret` 是空串 ⇒ 同样沿用（前端唯一能表达「我没改它」的方式）。
#[tokio::test]
async fn update_with_blank_secret_keeps_old_ciphertext() {
    let ctx = bootstrap().await;
    let created = create_account(&ctx, &unique_name("空串沿用")).await;
    let sealed_before = stored_secret(&ctx.pool, id_of(&created)).await;

    let (status, env) = post_as_manager(
        &ctx,
        &format!("/mail/accounts/{}", id_of(&created)),
        json!({
            "version": created["version"],
            "name": "改名-2",
            "preset": "NET_EASE_163",
            "account": "ops@163.com",
            "secret": "   ",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{env}");
    let sealed_after = stored_secret(&ctx.pool, id_of(&created)).await;
    assert_eq!(sealed_before, sealed_after, "空 secret 不得重写密文");
}

/// 给了新 `secret` ⇒ 密文换新，且能解回新明文。
#[tokio::test]
async fn update_with_new_secret_reencrypts() {
    let ctx = bootstrap().await;
    let created = create_account(&ctx, &unique_name("换授权码")).await;
    let sealed_before = stored_secret(&ctx.pool, id_of(&created)).await;

    let (status, env) = post_as_manager(
        &ctx,
        &format!("/mail/accounts/{}", id_of(&created)),
        json!({
            "version": created["version"],
            "name": "改名-3",
            "preset": "NET_EASE_163",
            "account": "ops@163.com",
            "secret": "zzzzzz9999999999",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{env}");
    let sealed_after = stored_secret(&ctx.pool, id_of(&created)).await;
    assert_ne!(sealed_before, sealed_after, "给了新 secret 必须重写密文");
    assert_eq!(open_with_test_key(&sealed_after), "zzzzzz9999999999");
}

/// ★ 改名撞上另一个**存活**邮箱的名字 ⇒ `21707` + 409，**不是** 500，也不是 40901。
///
/// 这条钉的是「写侧的 `is_unique_violation` 映射**在 update 路径也在**」：漏了它，
/// sqlx 的 `Database(23505)` 会一路冒到 `AppError::Database` ⇒ 前端只看到一句
/// 「数据库错误」，连「重名了」这个可执行结论都拿不到（`docs/api/mail.md` §2.4 承诺
/// 的是 21707）。回归 `service.rs::update_account` 里那个 `Err(e) if
/// is_unique_violation(&e)` 分支。
#[tokio::test]
async fn update_name_to_a_live_duplicate_returns_21707_and_keeps_row_intact() {
    let ctx = bootstrap().await;
    let taken = unique_name("已被占");
    create_account(&ctx, &taken).await;
    let mine = create_account(&ctx, &unique_name("要改名")).await;
    let sealed_before = stored_secret(&ctx.pool, id_of(&mine)).await;
    let (name_before, ver_before) = stored_name_and_version(&ctx.pool, id_of(&mine)).await;

    let (status, env) = post_as_manager(
        &ctx,
        &format!("/mail/accounts/{}", id_of(&mine)),
        json!({
            "version": mine["version"],
            "name": taken,          // ← 撞 uk_t_mail_account_name_alive
            "preset": "NET_EASE_163",
            "account": "ops@163.com",
        }),
    )
    .await;
    assert_biz_code(status, &env, 21707, StatusCode::CONFLICT);

    // 失败的那次写**一个字都不能改**：名字、版本、密文都原样
    let (name_after, ver_after) = stored_name_and_version(&ctx.pool, id_of(&mine)).await;
    assert_eq!(name_before, name_after, "被拒的改名不该落到库里");
    assert_eq!(ver_before, ver_after, "被拒的写不该推进 version");
    assert_eq!(
        stored_secret(&ctx.pool, id_of(&mine)).await,
        sealed_before,
        "被拒的写不该重写密文"
    );
}

/// ⚠️ 非空 `secret` 里的**控制字符** ⇒ `21704`，且密文逐字未变。
///
/// 改档是绕过「建档时那道闸」的旁路：CR/LF 会进 SMTP `AUTH` 命令行（凭据注入面），
/// 而 `validate_secret` 的 doc 就是这么写的。只有 create 校验 = 留了个后门。
#[tokio::test]
async fn update_secret_with_control_chars_returns_21704_and_keeps_ciphertext() {
    let ctx = bootstrap().await;
    let created = create_account(&ctx, &unique_name("控制字符")).await;
    let sealed_before = stored_secret(&ctx.pool, id_of(&created)).await;
    let (_, ver_before) = stored_name_and_version(&ctx.pool, id_of(&created)).await;

    let (status, env) = post_as_manager(
        &ctx,
        &format!("/mail/accounts/{}", id_of(&created)),
        json!({
            "version": created["version"],
            "name": created["name"],
            "preset": "NET_EASE_163",
            "account": "ops@163.com",
            "secret": "abcdef\r\nX-Evil: 1",   // ← 控制字符
        }),
    )
    .await;
    assert_biz_code(status, &env, 21704, StatusCode::BAD_REQUEST);

    let sealed_after = stored_secret(&ctx.pool, id_of(&created)).await;
    assert_eq!(sealed_before, sealed_after, "被拒的授权码不得落库");
    assert_eq!(open_with_test_key(&sealed_after), "abcdef1234567890");
    assert_eq!(
        stored_name_and_version(&ctx.pool, id_of(&created)).await.1,
        ver_before,
        "被拒的写不该推进 version"
    );
}

/// ⚠️ 非空 `secret` 超长（201 字 > `SECRET_MAX_CHARS` = 200）⇒ `21704`。
///
/// 这一条与控制字符是**同一个缺口的两端**：`SECRET_MAX_CHARS` 此前只在 create 生效，
/// 于是「建一个短码、再改成超长码」能把任意长度的串塞进 `encrypted_secret` 列。
#[tokio::test]
async fn update_secret_over_200_chars_returns_21704() {
    let ctx = bootstrap().await;
    let created = create_account(&ctx, &unique_name("超长授权码")).await;
    let sealed_before = stored_secret(&ctx.pool, id_of(&created)).await;

    let (status, env) = post_as_manager(
        &ctx,
        &format!("/mail/accounts/{}", id_of(&created)),
        json!({
            "version": created["version"],
            "name": created["name"],
            "preset": "NET_EASE_163",
            "account": "ops@163.com",
            "secret": "x".repeat(201),
        }),
    )
    .await;
    assert_biz_code(status, &env, 21704, StatusCode::BAD_REQUEST);
    assert_eq!(
        stored_secret(&ctx.pool, id_of(&created)).await,
        sealed_before,
        "被拒的超长授权码不得落库"
    );
}

/// OCC：body **缺** `version` ⇒ HTTP **422 纯文本**（axum `Json` 提取器，
/// 不是业务信封 —— 前端拿不到 `code` 字段）。
#[tokio::test]
async fn update_without_version_returns_422_plain_text() {
    let ctx = bootstrap().await;
    let created = create_account(&ctx, &unique_name("缺版本")).await;

    let (status, raw) = post_raw(
        &ctx,
        "POST",
        &format!("/mail/accounts/{}", id_of(&created)),
        json!({ "name": "x", "preset": "NET_EASE_163", "account": "a@163.com" }),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{raw}");
    assert!(
        !raw.contains("\"code\""),
        "422 是纯文本，**不是**业务信封（不该出现 code 字段）：{raw}"
    );
    assert!(
        raw.contains("version"),
        "422 的纯文本应说清缺哪个字段：{raw}"
    );
}

/// OCC：版本不对 ⇒ `40901` + HTTP 409。
#[tokio::test]
async fn update_with_stale_version_returns_40901() {
    let ctx = bootstrap().await;
    let created = create_account(&ctx, &unique_name("过期版本")).await;

    let (status, env) = post_as_manager(
        &ctx,
        &format!("/mail/accounts/{}", id_of(&created)),
        json!({
            "version": created["version"].as_i64().unwrap() + 99,
            "name": "改名-4",
            "preset": "NET_EASE_163",
            "account": "ops@163.com",
        }),
    )
    .await;
    assert_biz_code(status, &env, 40901, StatusCode::CONFLICT);
}

/// 改不存在的邮箱 ⇒ `21706` + HTTP 404。
#[tokio::test]
async fn update_missing_account_returns_21706() {
    let ctx = bootstrap().await;
    let (status, env) = post_as_manager(
        &ctx,
        "/mail/accounts/999999999999999",
        json!({
            "version": 0, "name": "x",
            "preset": "NET_EASE_163", "account": "a@163.com"
        }),
    )
    .await;
    assert_biz_code(status, &env, 21706, StatusCode::NOT_FOUND);
}

// ===========================================================================
//  4. POST /accounts/{id}/set-default —— 唯一默认
// ===========================================================================

/// 「至多一个默认」由唯一索引保证：`set-default` 第二个邮箱后，**第一个降级**。
#[tokio::test]
async fn set_default_demotes_the_previous_default() {
    let ctx = bootstrap().await;
    let a = create_account(&ctx, &unique_name("默认甲")).await;
    let b = create_account(&ctx, &unique_name("默认乙")).await;

    for acct in [&a, &b] {
        let (status, env) = post_as_manager(
            &ctx,
            &format!("/mail/accounts/{}/set-default", id_of(acct)),
            json!({ "version": acct["version"] }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{env}");
    }

    let (_, env) = get_as_manager(&ctx, "/mail/accounts").await;
    let items = env["data"]["items"].as_array().expect("items 是数组");
    let defaults: Vec<&Value> = items
        .iter()
        .filter(|i| i["is_default"] == json!(true))
        .collect();
    assert_eq!(defaults.len(), 1, "有且仅有一个默认邮箱：{env}");
    assert_eq!(defaults[0]["id"], b["id"], "后设的那个才是默认：{env}");
}

/// `set-default` 也吃 OCC（目标行的 `version` 必填）。
#[tokio::test]
async fn set_default_requires_version() {
    let ctx = bootstrap().await;
    let created = create_account(&ctx, &unique_name("默认缺版本")).await;

    let (status, raw) = post_raw(
        &ctx,
        "POST",
        &format!("/mail/accounts/{}/set-default", id_of(&created)),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{raw}");
    assert!(!raw.contains("\"code\""), "422 是纯文本：{raw}");
}

/// ⚠️ **两个管理员并发对不同邮箱点置默认 ⇒ 绝不返 500**。
///
/// 两个事务各自看不见对方未提交的结果，后到者的 `promote_to_default` 会撞
/// `uk_t_mail_account_one_default`（23505）—— 事务消不掉这个窗口（见
/// `service::set_default_account` 的 doc 与 `docs/api/mail.md` §4.3）。故断言的是
/// **允许的结果集合**：两个都成功（串行执行时本该如此）**或** 一个成功、另一个
/// `40901`；**任何 5xx 都是回归**（那一支原来会变成「数据库错误」500）。
#[tokio::test]
async fn concurrent_set_default_on_two_accounts_never_returns_500() {
    let ctx = bootstrap().await;
    let a = create_account(&ctx, &unique_name("并发甲")).await;
    let b = create_account(&ctx, &unique_name("并发乙")).await;

    let path_a = format!("/mail/accounts/{}/set-default", id_of(&a));
    let path_b = format!("/mail/accounts/{}/set-default", id_of(&b));
    let (ra, rb) = tokio::join!(
        post_as_manager(&ctx, &path_a, json!({ "version": a["version"] })),
        post_as_manager(&ctx, &path_b, json!({ "version": b["version"] })),
    );

    let mut ok_count = 0;
    for (who, (status, env)) in [("甲", ra), ("乙", rb)] {
        match status {
            StatusCode::OK => ok_count += 1,
            StatusCode::CONFLICT => assert_biz_code(status, &env, 40901, StatusCode::CONFLICT),
            other => panic!("{who}：只允许 200 或 40901（绝不能是 5xx），实际 {other}：{env}"),
        }
    }
    assert_eq!(
        ok_count, 1,
        "两次并发置默认只可能有一个赢家（40901 分支之外的兜底）"
    );

    // 终态：唯一索引兜底 ⇒ 至多一个默认，且确实有一个
    let (status, env) = get_as_manager(&ctx, "/mail/accounts").await;
    assert_eq!(status, StatusCode::OK, "{env}");
    let items = env["data"]["items"].as_array().expect("items 是数组");
    let defaults = items
        .iter()
        .filter(|i| i["is_default"] == json!(true))
        .count();
    assert_eq!(defaults, 1, "并发之后仍须有且仅有一个默认邮箱：{env}");
}

// ===========================================================================
//  路由纪律
// ===========================================================================

/// 全仓零 PUT/PATCH/DELETE ⇒ 改 / 删只走 POST + action 子路径。
///
/// 这条钉的是「**将来有人加 `DELETE /accounts/{id}` 时会当场红**」——
/// 那会让前端同时面对两套语义（DELETE 与 `/delete`）而不知该用哪个。
#[tokio::test]
async fn put_and_delete_verbs_are_not_routed() {
    let ctx = bootstrap().await;
    let created = create_account(&ctx, &unique_name("动词纪律")).await;
    let id = id_of(&created);

    for verb in ["PUT", "PATCH", "DELETE"] {
        let (status, raw) = post_raw(&ctx, verb, &format!("/mail/accounts/{id}"), json!({})).await;
        assert_eq!(
            status,
            StatusCode::METHOD_NOT_ALLOWED,
            "{verb} /mail/accounts/{{id}} 不该被路由（登记在 docs/api/mail.md §6）：{raw}"
        );
    }
}

/// 辅助：让 `ctx` 参数在本文件被多处使用时不被误判为未用。
#[allow(dead_code)]
fn _ctx_type_check(_c: &Ctx) {}

#[allow(dead_code)]
fn _unused_value(_v: &Value) {}

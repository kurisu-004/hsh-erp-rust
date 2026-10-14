//! wecom 域集成测试（2026-10-18 新建，与新域同批）
//!
//! 覆盖 `/api/v2/cron/wecom` 的 6 个端点的 HTTP 契约 + 三条硬约束：
//! - **地址加密入库 + 明文永不出参**（出参只有 `url_masked`）；
//! - **OCC**（缺 `version` → 422 纯文本；版本不对 → 40901）；
//! - **被引用禁删**（`t_wecom_daily_config.webhook_id` 指向它 → 21808）。
//!
//! ## 拆分映射（3 个 sub-file）
//! - `accounts.rs` —— 端点 1/2/3/6：列表排序、CRUD、置默认的唯一性、动词纪律
//! - `guards.rs`  —— 端点 4/5 的失败矩阵（422 / 40901 / 21807 / 21803 / 21806 / 21808）
//! - `probe.rs`  —— 端点 6（`/test`）：**不写任何日志** + 真发网络拿到 `errcode != 0`
//!
//! ## ⚠️ 测试主密钥必须写进进程 env（`tests/wecom/main.rs::ensure_mail_secret_env`）
//! `infra::crypto::seal_with_aad` / `open_with_aad` **每次重读** env `MAIL_ACCOUNT_SECRET`
//! （签名不带 state，见该模块 doc 的取舍段），所以集成测试进程必须自己设一个。
//! ⚠️ 与 `tests/scheduled_task` / `tests/mail_digest` **各设各的**（不同 binary = 不同进程，
//! 不共享 env）。
//!
//! ## 前置条件
//! - PG：`scripts/test_nextest.sh` 自带 session 容器与 per-test fresh database。
//! - **Redis**：端点全走 `/iam/login` 拿 token ⇒ 本 binary 需要 Redis
//!   （与全仓其它 HTTP 契约测试同款前提）。
//! - **外网**：`probe.rs` 有一条用例会真打 `qyapi.weixin.qq.com`（拿到 `errcode != 0`）。

#![allow(dead_code, clippy::await_holding_lock, unused_imports)]

mod accounts;
mod guards;
mod probe;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::http::StatusCode;
use serde_json::{Value, json};
use sqlx::PgPool;

use hsh_erp_rust::infra::clock::now_naive;
use hsh_erp_rust::infra::config::MAIL_ACCOUNT_SECRET_ENV;
use hsh_erp_rust::infra::crypto;
use hsh_erp_test_support::{
    PartFixture, load_part_fixture, login_token, shared_test_snowflake, test_app, test_pool,
    test_state,
};

/// 测试主密钥（`MailAccountSecret::default()` 的 `[0x5A; 32]` 同值，
/// 但**显式写出来**而不是依赖那个 `Default` —— 让「测试用的就是这一把」是显式事实）。
pub(crate) const TEST_MAIL_SECRET: [u8; 32] = [0x5Au8; 32];

/// 一份**合法**的 webhook 地址前缀（key 由用例拼，保证各用例互不相同 ⇒ 探针互不串扰）。
const URL_PREFIX: &str = "https://qyapi.weixin.qq.com/cgi-bin/webhook/send?key=";

// ===========================================================================
//  测试上下文
// ===========================================================================

/// 一个用例的公共上下文：fresh database + part fixture + MANAGER / CLERK 两个 token。
pub(crate) struct Ctx {
    pub pool: PgPool,
    pub app: axum::Router,
    pub state: Arc<hsh_erp_rust::state::AppState>,
    pub manager_token: String,
    /// CLERK token 只用于钉越权用例（本域 6 端点全部 MANAGER 独占）。
    pub clerk_token: String,
}

/// 标准上下文。
pub(crate) async fn bootstrap() -> Ctx {
    ensure_mail_secret_env();
    let pool = test_pool().await;
    let fx = load_part_fixture(&pool).await;
    let state = test_state(pool.clone()).await;
    let app = test_app(state.clone());
    let manager_token = login_token(&app, &fx.manager_username, PartFixture::PASSWORD).await;
    let clerk_token = login_token(&app, &fx.clerk_username, PartFixture::PASSWORD).await;
    Ctx {
        pool,
        app,
        state,
        manager_token,
        clerk_token,
    }
}

// ===========================================================================
//  测试主密钥进 env
// ===========================================================================

/// 把测试主密钥写进进程 env（`crypto::seal_with_aad` / `open_with_aad` 每次重读 env）。
///
/// ⚠️ `std::env::set_var` 在 edition 2024 是 unsafe，且**不能**与同进程的并发 env 读写
/// 竞争 ⇒ 用 `OnceLock::get_or_init` 串成「第一个调用者写、其余调用者等它写完」。
/// 本 binary 里所有需要加解密路径的用例都先过 `bootstrap()`，而 `bootstrap()` 第一行就是
/// 本函数 ⇒ 不存在绕开它的读方。仓内同款手法见 `tests/cos_opendal_api.rs` 的 `ENV_LOCK`
/// 与 `tests/scheduled_task/main.rs`。
pub(crate) fn ensure_mail_secret_env() {
    static ONCE: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    ONCE.get_or_init(|| {
        let hex: String = TEST_MAIL_SECRET
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        // SAFETY: 本 binary 内所有读该 env 的路径都先过本函数（由 `OnceLock` 串行）。
        unsafe { std::env::set_var(MAIL_ACCOUNT_SECRET_ENV, hex) };
    });
}

// ===========================================================================
//  名字 / 地址去重计数器
// ===========================================================================

/// 用例级唯一 webhook 名（同一用例里建多个时避免撞 `uk_t_wecom_webhook_name_alive`）。
static NAME_SEQ: AtomicU64 = AtomicU64::new(0);

/// 造一个本进程内唯一的 webhook 名。
pub(crate) fn unique_name(prefix: &str) -> String {
    let n = NAME_SEQ.fetch_add(1, Ordering::Relaxed);
    let tail = shared_test_snowflake().next_id();
    format!("{prefix}-{n}-{tail}")
}

/// 造一个**合法且唯一**的 webhook 地址（key 用雪花 id 尾巴 ⇒ 各用例互不相同）。
pub(crate) fn unique_url() -> String {
    format!("{URL_PREFIX}test-{}", shared_test_snowflake().next_id())
}

/// 造一个**库里绝不会存在**的 id（雪花 id 尾巴，用例拿它当幽灵行）。
pub(crate) fn ghost_id() -> i64 {
    shared_test_snowflake().next_id()
}

// ===========================================================================
//  请求 helper
// ===========================================================================

/// `GET {path}`（带 MANAGER token）。
pub(crate) async fn get_as_manager(ctx: &Ctx, path: &str) -> (StatusCode, Value) {
    hsh_erp_test_support::send(
        ctx.app.clone(),
        hsh_erp_test_support::json_request("GET", path, None, Some(&ctx.manager_token)),
    )
    .await
}

/// `POST {path}`（带 MANAGER token）。
pub(crate) async fn post_as_manager(ctx: &Ctx, path: &str, body: Value) -> (StatusCode, Value) {
    hsh_erp_test_support::send(
        ctx.app.clone(),
        hsh_erp_test_support::json_request("POST", path, Some(body), Some(&ctx.manager_token)),
    )
    .await
}

/// `POST {path}`（带 CLERK token —— 越权用例）。
pub(crate) async fn post_as_clerk(ctx: &Ctx, path: &str, body: Value) -> (StatusCode, Value) {
    hsh_erp_test_support::send(
        ctx.app.clone(),
        hsh_erp_test_support::json_request("POST", path, Some(body), Some(&ctx.clerk_token)),
    )
    .await
}

/// 一份**合法**的 `POST /webhooks` 请求体。
pub(crate) fn create_body(name: &str, url: &str) -> Value {
    json!({ "name": name, "webhook_url": url })
}

/// 建一个 webhook 并返回它的 JSON 出参。
pub(crate) async fn create_webhook(ctx: &Ctx, name: &str) -> Value {
    let (status, env) = post_as_manager(
        ctx,
        "/cron/wecom/webhooks",
        create_body(name, &unique_url()),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "建 webhook 应成功：{env}");
    env["data"].clone()
}

/// 建一个 webhook，返回它的 `id`（JSON string → i64）。
pub(crate) fn id_of(v: &Value) -> i64 {
    v["id"]
        .as_str()
        .expect("id 必须是 JSON string（雪花防精度截断）")
        .parse()
        .expect("id 必须是数字串")
}

/// 库里该 webhook 的 `encrypted_url`（用于「确实加密入库、且不是明文」断言）。
pub(crate) async fn stored_url_cipher(pool: &PgPool, id: i64) -> String {
    sqlx::query_scalar::<_, String>(
        "SELECT encrypted_url FROM t_wecom_webhook WHERE id = $1 AND deleted_at IS NULL",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .expect("读 t_wecom_webhook.encrypted_url")
}

/// 库里该 webhook 的 `name` 与 `version`（用于断言「被拒的那次写**没有动过那一行**」）。
///
/// ⚠️ 读的是**未软删**行（`deleted_at IS NULL`）：行若被软删掉，`fetch_one` 直接 panic，
/// 那本身也是一条断言（「写侧校验失败不该软删任何东西」）。
pub(crate) async fn stored_name_and_version(pool: &PgPool, id: i64) -> (String, i32) {
    let row = sqlx::query(
        "SELECT name, version FROM t_wecom_webhook WHERE id = $1 AND deleted_at IS NULL",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .expect("读 t_wecom_webhook.name / version");
    (
        sqlx::Row::get(&row, "name"),
        sqlx::Row::get(&row, "version"),
    )
}

/// 用测试主密钥 + 本域 AAD 解密某个 webhook 的密文（验证「入库的确实是本密钥下的密文」）。
pub(crate) fn open_with_test_aad(sealed: &str) -> String {
    crypto::open_with_key_aad(
        &TEST_MAIL_SECRET,
        sealed,
        b"t_wecom_webhook.url.v1",
        "webhook 地址",
    )
    .expect("用测试主密钥 + 本域 AAD 必须能解开")
}

/// 让每日总结配置指向某个 webhook（造「被引用」场景用）。
///
/// ⚠️ **直接 SQL 插一行**：内容域（`wecom_daily`）下一批才落地，此刻还没有它的 service。
/// 只填 `webhook_id`，其余列全走 DDL 里的 DEFAULT。
/// ⚠️ `uk_t_wecom_daily_config_alive` 保证「至多一行未软删配置」，故本 helper 幂等：
/// 先删掉可能已存在的行再插，保证「同一用例里连造两次引用」不会撞 23505。
pub(crate) async fn reference_from_daily_config(pool: &PgPool, webhook_id: i64) {
    sqlx::query("DELETE FROM t_wecom_daily_config WHERE deleted_at IS NULL")
        .execute(pool)
        .await
        .expect("清理旧的每日总结配置行");
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_wecom_daily_config (id, version, enabled, frequency, webhook_id, \
         created_at, created_by, updated_at, updated_by) \
         VALUES ($1, 0, true, 'DAILY', $2, $3, 0, $3, 0)",
    )
    .bind(shared_test_snowflake().next_id())
    .bind(webhook_id)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_wecom_daily_config");
}

/// `POST {path}`（MANAGER），返回**原始文本**（不走 JSON 解析）。
///
/// 用于断言 axum 提取器层的**纯文本** 4xx（body 缺 `version` → HTTP 422、
/// 未注册的方法 → HTTP 405）：那两类响应不是 `R<T>` 信封，`send` 会在 JSON 解析处 panic。
pub(crate) async fn post_raw(
    ctx: &Ctx,
    method: &str,
    path: &str,
    body: Value,
) -> (StatusCode, String) {
    hsh_erp_test_support::send_raw(
        ctx.app.clone(),
        hsh_erp_test_support::json_request(method, path, Some(body), Some(&ctx.manager_token)),
    )
    .await
}

/// 断言信封里的业务码 + HTTP 状态。
pub(crate) fn assert_biz_code(
    status: StatusCode,
    env: &Value,
    want_code: i32,
    want_http: StatusCode,
) {
    assert_eq!(
        status, want_http,
        "HTTP 状态不符；信封={env}（want_http={want_http} want_code={want_code}）"
    );
    assert_eq!(
        env.get("code").and_then(|c| c.as_i64()),
        Some(want_code as i64),
        "业务码不符：{env}（want {want_code}）"
    );
}

/// 断言一段文本里**不含** webhook 地址的 key 段（脱敏纪律）。
pub(crate) fn assert_no_key_leak(text: &str, key_fragment: &str) {
    assert!(
        !text.contains(key_fragment),
        "响应体泄漏了 webhook key 片段 {key_fragment}：{text}"
    );
    assert!(
        !text.contains("qyapi.weixin.qq.com/cgi-bin/webhook/send?key=693a"),
        "响应体出现了未掩码的地址：{text}"
    );
}

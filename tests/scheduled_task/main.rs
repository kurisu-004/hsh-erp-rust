//! scheduled_task 域集成测试（2026-10-16 新建，与新域同批）
//!
//! 覆盖 `/api/v2/mail` 的 6 个端点的 HTTP 契约 + 三条硬约束：
//! - **密文不回显**（列表与新建/修改的出参里都没有 `secret` 任何形式的影子）；
//! - **OCC**（缺 `version` → 422 纯文本；版本不对 → 40901）；
//! - **被引用禁删**（`t_mail_digest_config.sender_account_id` 指向它 → 21708）。
//!
//! ## 拆分映射（2 个 sub-file）
//! - `accounts.rs` —— 端点 1/2/3/4/6：预设表、列表、CRUD、默认邮箱的唯一性
//! - `delete.rs`   —— 端点 5：软删 + 被引用禁删 + 软删后同名可再建
//!
//! ## ⚠️ 测试主密钥必须写进进程 env（`tests/scheduled_task/main.rs::ensure_mail_secret_env`）
//! `infra::crypto::seal` / `open` **每次重读** env `MAIL_ACCOUNT_SECRET`
//! （签名不带 state，见该模块 doc 的取舍段），所以集成测试进程必须自己设一个。
//!
//! ## 前置条件
//! - PG：`scripts/test_nextest.sh` 自带 session 容器与 per-test fresh database。
//! - **Redis**：端点全走 `/iam/login` 拿 token ⇒ 本 binary 需要 Redis
//!   （与全仓其它 HTTP 契约测试同款前提）。

#![allow(dead_code, clippy::await_holding_lock, unused_imports)]

mod accounts;
mod delete;

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

/// 把测试主密钥写进进程 env（`crypto::seal` / `open` 每次重读 env）。
///
/// ⚠️ `std::env::set_var` 在 edition 2024 是 unsafe，且**不能**与同进程的并发
/// env 读写竞争 ⇒ 用 `OnceLock::get_or_init` 串成「第一个调用者写、其余调用者等它
/// 写完」。本 binary 里所有需要加解密路径的用例都先过 `bootstrap()`，而
/// `bootstrap()` 第一行就是本函数 ⇒ 不存在绕开它的读方。仓内同款手法见
/// `tests/cos_opendal_api.rs` 的 `ENV_LOCK`。
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
//  名字去重计数器
// ===========================================================================

/// 用例级唯一邮箱名（同一用例里建多个邮箱时避免撞 `uk_t_mail_account_name_alive`）。
static NAME_SEQ: AtomicU64 = AtomicU64::new(0);

/// 造一个本进程内唯一的邮箱名。
pub(crate) fn unique_name(prefix: &str) -> String {
    let n = NAME_SEQ.fetch_add(1, Ordering::Relaxed);
    // 用雪花 id 尾巴而非纯自增序号：并行跑多个用例时自增序号在**不同 database**
    // 里也会重复 —— 而 name 唯一索引是**每库一份**，所以其实不会撞。用雪花尾巴
    // 是为了顺带保证「跨库 dump 回来」时也不撞。
    let tail = shared_test_snowflake().next_id();
    format!("{prefix}-{n}-{tail}")
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

/// 一份**合法**的 `POST /accounts` 请求体。
pub(crate) fn create_body(name: &str) -> Value {
    json!({
        "name": name,
        "preset": "NET_EASE_163",
        "account": "ops@163.com",
        "secret": "abcdef1234567890",
    })
}

/// 建一个邮箱并返回它的 JSON 出参。
pub(crate) async fn create_account(ctx: &Ctx, name: &str) -> Value {
    let (status, env) = post_as_manager(ctx, "/mail/accounts", create_body(name)).await;
    assert_eq!(status, StatusCode::OK, "建邮箱应成功：{env}");
    env["data"].clone()
}

/// 建一个邮箱，返回它的 `id`（JSON string → i64）。
pub(crate) fn id_of(v: &Value) -> i64 {
    v["id"]
        .as_str()
        .expect("id 必须是 JSON string（雪花防精度截断）")
        .parse()
        .expect("id 必须是数字串")
}

/// 库里该邮箱的 `encrypted_secret`（用于「确实加密入库、且不是明文」断言）。
pub(crate) async fn stored_secret(pool: &PgPool, id: i64) -> String {
    sqlx::query_scalar::<_, String>(
        "SELECT encrypted_secret FROM t_mail_account WHERE id = $1 AND deleted_at IS NULL",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .expect("读 t_mail_account.encrypted_secret")
}

/// 库里该邮箱的 `name` 与 `version`（用于断言「被拒的那次写**没有动过那一行**」）。
///
/// ⚠️ 读的是**未软删**行（`deleted_at IS NULL`）：行若被软删掉，`fetch_one` 直接 panic，
/// 那本身也是一条断言（「写侧校验失败不该软删任何东西」）。
pub(crate) async fn stored_name_and_version(pool: &PgPool, id: i64) -> (String, i32) {
    let row = sqlx::query(
        "SELECT name, version FROM t_mail_account WHERE id = $1 AND deleted_at IS NULL",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .expect("读 t_mail_account.name / version");
    (
        sqlx::Row::get(&row, "name"),
        sqlx::Row::get(&row, "version"),
    )
}

/// 用给定密钥解密某个邮箱的密文（验证「入库的确实是本密钥下的 AES-GCM 密文」）。
pub(crate) fn open_with_test_key(sealed: &str) -> String {
    crypto::open_with(&TEST_MAIL_SECRET, sealed).expect("用测试主密钥必须能解开")
}

/// 让 mail_digest 配置指向某个邮箱（造「被引用」场景用）。
pub(crate) async fn reference_from_digest_config(pool: &PgPool, account_id: i64) {
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_mail_digest_config (id, version, enabled, frequency, send_hour, \
         send_minute, recipients, cc_recipients, include_overdue, include_upcoming, \
         upcoming_days, visible_columns, sender_account_id, created_at, created_by, \
         updated_at, updated_by) \
         VALUES ($1, 0, true, 'DAILY', 8, 0, '{}', '{}', true, true, 14, \
         '{serial_no,drawing_no}', $2, $3, 0, $3, 0)",
    )
    .bind(shared_test_snowflake().next_id())
    .bind(account_id)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_mail_digest_config");
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

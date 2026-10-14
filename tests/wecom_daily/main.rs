//! wecom_daily 域集成测试（2026-10-18 新建，与新域同批）
//!
//! 覆盖 `/api/v2/cron/wecom-daily` 的 5 个端点（6 条路由）的 HTTP 契约 + 统计 SQL
//! 口径 + 分片与流水语义。
//!
//! ## 拆分映射（6 个 sub-file）
//! - `config.rs`  —— 端点 1/2：读单例 + 全量覆写（422 / 40901 / 21804 / 21806）
//! - `preview.rs` —— 端点 3：窗口半开含今天、节开关、分片、**绝不落流水**
//! - `send.rs`    —— 端点 4：`/send` 的失败路径 + 「失败也落流水」不变量
//! - `logs.rs`    —— 端点 5：分页 + 快照列
//! - `n_plus_one_guard.rs` —— **源码级**护栏：`service::fetch_rows` 的循环体内禁
//!   `repo::` 调用 + 顶层恰好 6 个调用点
//! - `timeout_guard.rs` —— **跨域**纯函数护栏：`/send` 必须命中
//!   `middleware::timeout::is_long_timeout_path` 长档（否则超时会把已推送的分片变成
//!   「零流水」），`/preview` 必须不命中
//!
//! ## ⚠️ 覆盖空洞（2026-10-18 登记）
//! **`/send` 的成功路径（`status='SUCCESS'` 的流水行）无集成测试**：测试环境没有
//! 真实可达的群机器人地址，而本仓**刻意不引入 webhook 的 mock transport**（那会为了
//! 一条 happy-path 给生产依赖树加一层测试专用构造）。替代覆盖：`/preview` 覆盖
//! 渲染与分片、失败路径覆盖落流水、`sender::classify` 的 lib 单测覆盖 errcode 解析。
//! 登记见 `docs/api/wecom-daily.md` §8.3。
//!
//! ## ⚠️ 测试主密钥必须写进进程 env（`ensure_mail_secret_env`）
//! `wecom::repo` / `crypto::open_with_aad` **每次重读** env `MAIL_ACCOUNT_SECRET`
//! （签名不带 state），而本 binary 的用例要造**可解的** webhook 密文 ⇒ 必须自己设一个。
//! ⚠️ 与 `tests/wecom` / `tests/scheduled_task` / `tests/mail_digest` **各设各的**
//! （不同 binary = 不同进程，不共享 env）。
//!
//! ## 前置条件
//! - PG：`scripts/test_nextest.sh` 自带 session 容器与 per-test fresh database。
//! - **Redis**：端点全走 `/iam/login` 拿 token ⇒ 本 binary 需要 Redis。
//! - **外网**：`send.rs` 的失败路径会真打 `qyapi.weixin.qq.com`（拿到 `errcode != 0`）。

#![allow(dead_code, clippy::await_holding_lock, unused_imports)]

mod config;
mod logs;
mod n_plus_one_guard;
mod preview;
mod send;
mod timeout_guard;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::http::StatusCode;
use chrono::{NaiveDate, NaiveDateTime};
use serde_json::{Value, json};
use sqlx::PgPool;

use hsh_erp_rust::infra::clock::now_naive;
use hsh_erp_rust::infra::config::MAIL_ACCOUNT_SECRET_ENV;
use hsh_erp_test_support::{
    PartFixture, load_part_fixture, login_token, shared_test_snowflake, test_app, test_pool,
    test_state,
};

/// 测试主密钥（显式写出来而不是依赖 `Default` —— 让「测试用的就是这一把」是显式事实）。
pub(crate) const TEST_MAIL_SECRET: [u8; 32] = [0x5Au8; 32];

/// 一份**合法**的 webhook 地址前缀（key 由用例拼，保证各用例互不相同）。
const URL_PREFIX: &str = "https://qyapi.weixin.qq.com/cgi-bin/webhook/send?key=";

/// 本域 6 条路由的挂载前缀。
pub(crate) const BASE: &str = "/cron/wecom-daily";

// ===========================================================================
//  测试上下文
// ===========================================================================

/// 一个用例的公共上下文：fresh database + fixture + MANAGER / CLERK 两个 token
/// + 一个**未软删**的 webhook 通道。
pub(crate) struct Ctx {
    pub pool: PgPool,
    pub app: axum::Router,
    /// `AppState` 本体：唯一需要**直调 service** 的用例用（`send_scheduled` 没有
    /// HTTP 端点，只能像 cron job 那样直接调）。其余用例走 `app`。
    pub state: Arc<hsh_erp_rust::state::AppState>,
    pub manager_token: String,
    /// CLERK token 只用于钉越权用例（本域全部 MANAGER 独占）。
    pub clerk_token: String,
    pub customer_id: i64,
    /// 本用例造出来的 webhook id（**可解**，因为用测试主密钥加密）。
    pub webhook_id: i64,
}

/// 标准上下文：已建好一条**可解**的 webhook 通道（配置表仍是空的，`GET /config`
/// 返全默认）。
pub(crate) async fn bootstrap() -> Ctx {
    ensure_mail_secret_env();
    let pool = test_pool().await;
    let fx = load_part_fixture(&pool).await;
    let state = test_state(pool.clone()).await;
    let app = test_app(state.clone());
    let manager_token = login_token(&app, &fx.manager_username, PartFixture::PASSWORD).await;
    let clerk_token = login_token(&app, &fx.clerk_username, PartFixture::PASSWORD).await;
    let customer_id = insert_customer(&pool, "日报测试客户", 'M').await;
    let webhook_id = insert_webhook(&pool, &unique_name("日报通道"), &unique_url()).await;
    Ctx {
        pool,
        app,
        state,
        manager_token,
        clerk_token,
        customer_id,
        webhook_id,
    }
}

// ===========================================================================
//  测试主密钥进 env
// ===========================================================================

/// 把测试主密钥写进进程 env。
///
/// ⚠️ `std::env::set_var` 在 edition 2024 是 unsafe，且**不能**与同进程的并发 env 读写
/// 竞争 ⇒ 用 `OnceLock::get_or_init` 串成「第一个调用者写、其余调用者等它写完」。
/// 本 binary 里所有需要加解密路径的用例都先过 `bootstrap()`，而 `bootstrap()` 第一行
/// 就是本函数 ⇒ 不存在绕开它的读方。
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

/// 用测试主密钥 + **wecom 域的 AAD** 加密（与生产写侧逐字同一条路径）。
pub(crate) fn seal_with_test_aad(plain: &str) -> String {
    hsh_erp_rust::infra::crypto::seal_with_aad(plain, b"t_wecom_webhook.url.v1")
        .expect("用测试主密钥 + 通道域 AAD 加密必须成功（env 已在 ensure_mail_secret_env 里设好）")
}

/// 用测试主密钥 + 通道域 AAD 解密（验证「入库的确实是本密钥下的密文」）。
pub(crate) fn open_with_test_aad(sealed: &str) -> String {
    hsh_erp_rust::infra::crypto::open_with_key_aad(
        &TEST_MAIL_SECRET,
        sealed,
        b"t_wecom_webhook.url.v1",
        "webhook 地址",
    )
    .expect("用测试主密钥 + 通道域 AAD 必须能解开")
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

/// 造一个**指向不可达 key** 的地址（企业微信会回 `errcode != 0` ⇒ `21802`）。
///
/// ⚠️ 这条路径**真发网络**（打 `qyapi.weixin.qq.com`）：它不 mock 传输层，而是用一条
/// 语法合法但 key 不存在的地址让上游**如实**报失败 —— 这正是 `send.rs` 要验的
/// 「失败也落流水」。
pub(crate) fn unreachable_url() -> String {
    format!("{URL_PREFIX}definitely-not-a-real-key-000000000000")
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

/// `GET {path}`（带 CLERK token —— 越权用例）。
pub(crate) async fn get_as_clerk(ctx: &Ctx, path: &str) -> (StatusCode, Value) {
    hsh_erp_test_support::send(
        ctx.app.clone(),
        hsh_erp_test_support::json_request("GET", path, None, Some(&ctx.clerk_token)),
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

// ===========================================================================
//  时间基准（**禁止**在用例里出现 `2026-10-18` 这类字面量）
// ===========================================================================

/// 「今天」（Asia/Shanghai，naive）。本域全部断言围绕 today±N 天。
pub(crate) fn today() -> NaiveDate {
    now_naive().date()
}

/// 今天某个钟点（造「今天的事件」用）。
pub(crate) fn today_at(hour: u32, minute: u32) -> NaiveDateTime {
    now_naive()
        .date()
        .and_hms_opt(hour, minute, 0)
        .expect("时分合法")
}

/// 昨天某个钟点（用窗口下界之外的时刻造「窗口外」事件）。
pub(crate) fn yesterday_at(hour: u32) -> NaiveDateTime {
    use chrono::Duration;
    (now_naive().date() - Duration::days(1))
        .and_hms_opt(hour, 0, 0)
        .expect("时分合法")
}

// ===========================================================================
//  数据 fixture helper（直插，域独享）
// ===========================================================================

/// 插一个一级客户（`serial_prefix` 必填且只能是大写单字母）。
pub(crate) async fn insert_customer(pool: &PgPool, name: &str, prefix: char) -> i64 {
    let id = shared_test_snowflake().next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_customer (id, name, parent_id, serial_prefix, version, \
         created_at, updated_at) VALUES ($1, $2, NULL, $3, 0, $4, $4)",
    )
    .bind(id)
    .bind(name)
    .bind(prefix.to_string())
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_customer");
    id
}

/// 插一个 `t_wecom_webhook` 行（地址用**测试主密钥 + 通道域 AAD** 加密 ⇒ 可解）。
pub(crate) async fn insert_webhook(pool: &PgPool, name: &str, url: &str) -> i64 {
    let id = shared_test_snowflake().next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_wecom_webhook (id, version, name, encrypted_url, is_default, \
         created_at, created_by, updated_at, updated_by) \
         VALUES ($1, 0, $2, $3, false, $4, 0, $4, 0)",
    )
    .bind(id)
    .bind(name)
    .bind(seal_with_test_aad(url))
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_wecom_webhook");
    id
}

/// 造一个 `t_part` 工单（`assembly_id = None` 即散件）。
#[allow(clippy::too_many_arguments)]
pub(crate) async fn insert_part(
    pool: &PgPool,
    customer_id: i64,
    drawing_no: &str,
    status: &str,
    system_date: Option<NaiveDate>,
) -> i64 {
    let id = shared_test_snowflake().next_id();
    let now = now_naive();
    let req = today();
    sqlx::query(
        "INSERT INTO t_part (id, serial_no, name, drawing_no, applicant_name, quantity, \
         unit_price, total_price, request_date, planned_delivery_date, system_delivery_date, \
         status, is_urgent, customer_id, assembly_id, order_no, version, created_at, updated_at) \
         VALUES ($1, NULL, $2, $3, 'tester', 1, 0, 0, $4, $4, $5, $6, false, $7, NULL, NULL, 0, $8, $8)",
    )
    .bind(id)
    .bind(format!("件{drawing_no}"))
    .bind(drawing_no)
    .bind(req)
    .bind(system_date)
    .bind(status)
    .bind(customer_id)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_part");
    id
}

/// 造一个装配件工单（交期 / 状态谓词与散件同款，进第三节的两支 `UNION ALL`）。
pub(crate) async fn insert_assembly(
    pool: &PgPool,
    customer_id: i64,
    drawing_no: &str,
    status: &str,
    system_date: Option<NaiveDate>,
) -> i64 {
    let id = shared_test_snowflake().next_id();
    let now = now_naive();
    let req = today();
    sqlx::query(
        "INSERT INTO t_assembly (id, serial_no, drawing_no, name, applicant_name, quantity, \
         unit_price, total_price, request_date, planned_delivery_date, system_delivery_date, \
         status, is_urgent, customer_id, version, created_at, updated_at) \
         VALUES ($1, NULL, $2, $3, 'tester', 1, 0, 0, $4, $4, $5, $6, false, $7, 0, $8, $8)",
    )
    .bind(id)
    .bind(drawing_no)
    .bind(format!("装{drawing_no}"))
    .bind(req)
    .bind(system_date)
    .bind(status)
    .bind(customer_id)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_assembly");
    id
}

/// 造一个批次（`status` 决定第三节的「已交量守卫」是否命中：`DELIVERED` / `COMPLETED`）。
pub(crate) async fn insert_batch(pool: &PgPool, part_id: i64, status: &str) -> i64 {
    insert_batch_full(pool, part_id, status, None).await
}

pub(crate) async fn insert_batch_full(
    pool: &PgPool,
    part_id: i64,
    status: &str,
    delivery_note_id: Option<i64>,
) -> i64 {
    let id = shared_test_snowflake().next_id();
    let now = now_naive();
    // ⚠️ `uq_t_part_batch_part_no` 是 `(part_id, batch_no)` 唯一 ⇒ batch_no 必须**按 part
    // 自增**，不能一律写 1（同一用例给同一工单造多个批次时会撞唯一索引）。
    let batch_no: i32 = sqlx::query_scalar(
        "SELECT COALESCE(MAX(batch_no), 0) + 1 FROM t_part_batch WHERE part_id = $1",
    )
    .bind(part_id)
    .fetch_one(pool)
    .await
    .expect("取 batch_no");
    sqlx::query(
        "INSERT INTO t_part_batch (id, part_id, batch_no, quantity, status, \
         delivery_note_id, is_repairing, version, created_at, updated_at) \
         VALUES ($1, $2, $3, 1, $4, $5, false, 0, $6, $6)",
    )
    .bind(id)
    .bind(part_id)
    .bind(batch_no)
    .bind(status)
    .bind(delivery_note_id)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_part_batch");
    id
}

/// 造一个工人（`badge_code` 唯一，日报第四节按它做同名兜底排序）。
pub(crate) async fn insert_worker(pool: &PgPool, name: &str) -> i64 {
    let id = shared_test_snowflake().next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_worker (id, badge_code, name, is_active, version, created_at, updated_at) \
         VALUES ($1, $2, $3, true, 0, $4, $4)",
    )
    .bind(id)
    .bind(format!("W-{id}"))
    .bind(name)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_worker");
    id
}

/// 造一条 `t_part_event`（**报工口径的核心 fixture**）。
///
/// ⚠️ `worker_id` 是 `2026-10-12` 才写进事件表的那一列（迁移 `20261011120000`），
/// 本域第一节 / 第四节的 INNER `JOIN t_worker` 依赖它 ⇒ 用例里报工事件**必须**带
/// `worker_id`，否则既不进第一节也不进第四节（见 `repo.rs` 的「`worker_id` 的历史
/// 空洞」一节）。
pub(crate) async fn insert_event(
    pool: &PgPool,
    part_id: i64,
    worker_id: Option<i64>,
    event_type: &str,
    quantity: Option<i32>,
    batch_id: Option<i64>,
    at: NaiveDateTime,
) -> i64 {
    let id = shared_test_snowflake().next_id();
    sqlx::query(
        "INSERT INTO t_part_event (id, part_id, worker_id, event_type, quantity, batch_id, \
         created_at, created_by) VALUES ($1, $2, $3, $4, $5, $6, $7, 0)",
    )
    .bind(id)
    .bind(part_id)
    .bind(worker_id)
    .bind(event_type)
    .bind(quantity)
    .bind(batch_id)
    .bind(at)
    .execute(pool)
    .await
    .expect("insert t_part_event");
    id
}

/// 造一条送货单（`picked_up_at` 是第二节的唯一判据）。
pub(crate) async fn insert_delivery_note(
    pool: &PgPool,
    customer_id: i64,
    status: &str,
    picked_up_at: Option<NaiveDateTime>,
) -> i64 {
    let id = shared_test_snowflake().next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_delivery_note (id, delivery_note_no, customer_id, status, picked_up_at, \
         version, created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, $3, $4, $5, 0, $6, 0, $6, 0)",
    )
    // ⚠️ `delivery_note_no` 列是 `varchar(16)`（格式 `DN-YYYYMMDD-NNNN`）⇒ 用短号段，
    // 不能直接塞 19 位雪花。
    .bind(id)
    .bind(format!(
        "DN{:013}",
        shared_test_snowflake().next_id() % 10_000_000_000_000
    ))
    .bind(customer_id)
    .bind(status)
    .bind(picked_up_at)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_delivery_note");
    id
}

/// 直插一行推送流水（`logs.rs` 分页用；发送路径本身用真发造行）。
#[allow(clippy::too_many_arguments)]
pub(crate) async fn insert_log_row(
    pool: &PgPool,
    config_version: i32,
    trigger_source: &str,
    status: &str,
    webhook_name: &str,
    chunk_count: i16,
    chunk_sent: i16,
    sent_at: NaiveDateTime,
) -> i64 {
    let id = shared_test_snowflake().next_id();
    sqlx::query(
        "INSERT INTO t_wecom_daily_log (id, config_version, trigger_source, webhook_name, \
         webhook_masked, report_date, markdown_bytes, chunk_count, chunk_sent, \
         overview_batch_count, overview_quantity, note_count, note_part_count, \
         overdue_count, upcoming_count, worker_row_total, status, error_message, \
         sent_at, created_at) \
         VALUES ($1, $2, $3, $4, $5, $6, 100, $7, $8, 1, 2, 3, 4, 5, 6, $9, $10, $11, $12, $12)",
    )
    .bind(id)
    .bind(config_version)
    .bind(trigger_source)
    .bind(webhook_name)
    .bind("https://qyapi.weixin.qq.com/cgi-bin/webhook/send?key=693a***faaa")
    .bind(sent_at.date())
    .bind(chunk_count)
    .bind(chunk_sent)
    .bind(worker_row_total_of(chunk_sent))
    .bind(status)
    .bind(if status == "FAILED" {
        Some("第 1/2 条失败：测试注入的失败原因")
    } else {
        None
    })
    .bind(sent_at)
    .execute(pool)
    .await
    .expect("insert t_wecom_daily_log");
    id
}

/// 让 `worker_row_total` 随 `chunk_sent` 变化（避免它成为常量掩盖断言）。
fn worker_row_total_of(chunk_sent: i16) -> i32 {
    chunk_sent as i32 * 3
}

/// `t_wecom_daily_log` 当前行数（用于「**不得**产生 log 行」这类否定断言）。
pub(crate) async fn count_logs(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM t_wecom_daily_log")
        .fetch_one(pool)
        .await
        .expect("count t_wecom_daily_log")
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

/// 一份**合法**的 `POST /config` 请求体（`version` 由用例指定）。
pub(crate) fn config_body(version: i32, webhook_id: i64) -> Value {
    json!({
        "version": version,
        "enabled": true,
        "frequency": "DAILY",
        "send_hour": 8,
        "send_minute": 0,
        "weekday": null,
        "day_of_month": null,
        "webhook_id": webhook_id.to_string(),
        "upcoming_days": 3,
        "worker_metric": "QUANTITY",
        "worker_top_n": 15,
        "max_chunks": 3,
        "include_overview": true,
        "include_delivery": true,
        "include_orders": true,
        "include_workers": true,
        "subject_template": null
    })
}

/// JSON string 形态的雪花 id → i64。
pub(crate) fn id_str(v: &Value) -> i64 {
    v.as_str()
        .expect("雪花 id 必须是 JSON string")
        .parse()
        .expect("id 必须是数字串")
}

/// `data.xxx` 取值的便捷读取。
pub(crate) fn data(env: &Value) -> &Value {
    env.get("data").unwrap_or(env)
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

/// 把响应里的 `chunks` 读成 `Vec<String>`。
pub(crate) fn chunks_of(preview: &Value) -> Vec<String> {
    data(preview)["chunks"]
        .as_array()
        .expect("chunks 必须是数组")
        .iter()
        .map(|v| v.as_str().expect("chunk 必须是字符串").to_string())
        .collect()
}

/// 取某天窗口内的事件条数（辅助断言用）。
pub(crate) async fn count_events_today(pool: &PgPool, event_type: &str) -> i64 {
    sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM t_part_event \
         WHERE event_type = $1 \
           AND created_at >= $2::date::timestamp \
           AND created_at < ($2::date + INTERVAL '1 day')::timestamp",
    )
    .bind(event_type)
    .bind(today())
    .fetch_one(pool)
    .await
    .expect("count t_part_event")
}

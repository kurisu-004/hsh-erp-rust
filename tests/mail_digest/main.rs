//! mail_digest 域集成测试（2026-10-13 新建，域同批新建）
//!
//! 本 binary 与 `src/modules/mail_digest/` 同批落地，覆盖 6 个端点的
//! HTTP 契约 + 统计 SQL 口径 + 发送流水语义。
//!
//! ## 拆分映射（6 个 sub-file）
//! - `config.rs`  —— 端点 1/2：读单例 + 全量覆写（OCC / 422 / 409）+ 校验矩阵
//! - `batches.rs` —— 2026-10-16 新增：工单行下的批次子行 + 位置列 + 空数组入参
//! - `preview.rs` —— 端点 3：渲染（工单级行单位 / 已交量守卫 / 节开关 / 列白名单 / XSS）
//! - `send.rs`    —— 端点 4：`/send` 的三条失败路径 + 「失败也落流水」不变量
//! - `logs.rs`    —— 端点 6：分页 + 收件人快照 + `config_version` 快照
//! - `n_plus_one_guard.rs` —— **源码级**护栏：`service::fetch_rows` 的循环体内禁
//!   `repo::` 调用（需求 4「禁止出现 N+1」的回归防线；手段与局限见该文件头注释）
//!
//! ## ⚠️ 覆盖空洞（2026-10-13 登记）
//! **`/send` 的成功路径（`status='SUCCESS'` 的流水行）无集成测试**：测试环境没有
//! 可达的 SMTP 服务器，而本仓刻意**不引入 lettre 的 mock transport**（那会为了
//! 一条 happy-path 给生产依赖树加一层测试专用构造）。故成功路径只由
//! 「渲染 → 调 lettre → 落流水」这条链的前两段（`/preview` 覆盖渲染、
//! 「必失败」路径覆盖落流水）间接保证。**要补它需要一个真实 SMTP 或
//! `smtp-sink` 之类的本地假服务器**，登记为后续增强，不在当前测试面里。
//! lib 单测侧另有 `template.rs` / `schedule.rs` / `mailer.rs` 的纯函数与映射表覆盖。
//!
//! ## 前置条件
//! - PG：`scripts/test_nextest.sh` 自带 session 容器与 per-test fresh database。
//! - **Redis**：6 个端点全走 `/iam/login` 拿 token ⇒ 本 binary 需要 Redis
//!   （与全仓其它 HTTP 契约测试同款前提）。默认连 `redis-test` 容器（6380），
//!   跨 worktree 用 `TEST_REDIS_URL=redis://localhost:<port>/0` 覆盖。

#![allow(dead_code, clippy::await_holding_lock, unused_imports)]

mod batches;
mod config;
mod logs;
mod n_plus_one_guard;
mod preview;
mod send;

use std::sync::Arc;

use axum::http::StatusCode;
use chrono::NaiveDate;
use serde_json::{Value, json};
use sqlx::PgPool;

use hsh_erp_rust::infra::clock::now_naive;
use hsh_erp_rust::infra::config::MAIL_ACCOUNT_SECRET_ENV;
use hsh_erp_rust::infra::crypto;
use hsh_erp_test_support::{
    PartFixture, load_part_fixture, login_token, shared_test_snowflake, test_app, test_pool,
    test_state,
};

// ===========================================================================
//  测试上下文
// ===========================================================================

/// 一个用例的公共上下文：fresh database + fixture + MANAGER / CLERK 两个 token。
///
/// `manager_token` / `clerk_token` 分别来自 `part` fixture 的 MANAGER / CLERK
/// 用户（同款 bcrypt 预置哈希，明文 `PartFixture::PASSWORD`）。本域 6 端点
/// **全部 MANAGER 独占**，CLERK token 只用于钉越权用例。
pub(crate) struct Ctx {
    pub pool: PgPool,
    pub app: axum::Router,
    /// `AppState` 本体：唯一需要**直调 service** 的用例用（`send_scheduled` 没有
    /// HTTP 端点，只能像 cron job 那样直接调）。其余用例走 `app`。
    pub state: Arc<hsh_erp_rust::state::AppState>,
    pub manager_token: String,
    pub clerk_token: String,
    pub customer_id: i64,
    /// `bootstrap_with_sender_account()` 建出来的发信邮箱 id；`bootstrap()` 为 `None`。
    ///
    /// 供需要把 `sender_account_id` 填进 `POST /config` 请求体的用例使用。
    pub sender_account_id: Option<i64>,
}

/// 默认（**未选**发信邮箱）上下文：绝大多数用例用它。
///
/// 等价于旧 env 方案里的「SMTP 未配置」形态 ⇒ `/send` 返 `21701` + HTTP 503。
pub(crate) async fn bootstrap() -> Ctx {
    bootstrap_inner(None).await
}

/// 「已选定可用发信邮箱」的上下文（2026-10-16 起取代 `bootstrap_with_mail`）。
///
/// 建两行：
/// 1. `t_mail_account` 一行（授权码用 [`crypto::seal_with`] + [`TEST_MAIL_SECRET`]
///    加密，与 [`ensure_mail_secret_env`] 设进 env 的那把是同一把）；
/// 2. `t_mail_digest_config` 一行，`sender_account_id` 指向上面那行
///    —— 这样 `/send` 与 cron 的准备阶段都能进到 `deliver`。
///
/// ⚠️ **邮箱名带 `<` / `>` 是刻意的**（`TEST_ACCOUNT_NAME`）：它让
/// `mailer::send` 构造 `From` 头时 `Mailbox` 解析**必然失败**，从而在**零网络 IO**
/// 的前提下触发「SMTP 层失败 ⇒ `21702` + 一行 FAILED 流水」这条不变量。
/// 旧实现靠 env 指 `127.0.0.1:1` 达到同样效果，而 host/port 现在由**编译期预设表**
/// 固定（网易 163 / QQ 都是 465），env 已整轮删除 ⇒ 那条确定性造法不可复现。
/// 两条失败路径在 `deliver` 里共用**完全相同**的一段代码
/// （`mailer::send` 返回 `Err` → 记 FAILED 流水 → 返 `21702`），
/// 详见 `docs/api/mail_digest.md` §8.3「覆盖空洞」。
pub(crate) async fn bootstrap_with_sender_account() -> Ctx {
    ensure_mail_secret_env();
    let pool = test_pool().await;
    let fx = load_part_fixture(&pool).await;
    let state = test_state(pool.clone()).await;
    let account_id = insert_mail_account(&pool, TEST_ACCOUNT_NAME, "sender@163.com").await;
    insert_digest_config(&pool, account_id).await;
    let app = test_app(state.clone());
    let manager_token = login_token(&app, &fx.manager_username, PartFixture::PASSWORD).await;
    let clerk_token = login_token(&app, &fx.clerk_username, PartFixture::PASSWORD).await;
    let customer_id = insert_customer(&pool, "汇总邮件测试客户").await;
    Ctx {
        pool,
        app,
        state,
        manager_token,
        clerk_token,
        customer_id,
        sender_account_id: Some(account_id),
    }
}

async fn bootstrap_inner(sender_account_id: Option<i64>) -> Ctx {
    let pool = test_pool().await;
    let fx = load_part_fixture(&pool).await;
    let state = test_state(pool.clone()).await;
    let app = test_app(state.clone());
    let manager_token = login_token(&app, &fx.manager_username, PartFixture::PASSWORD).await;
    let clerk_token = login_token(&app, &fx.clerk_username, PartFixture::PASSWORD).await;
    let customer_id = insert_customer(&pool, "汇总邮件测试客户").await;
    Ctx {
        pool,
        app,
        state,
        manager_token,
        clerk_token,
        customer_id,
        sender_account_id,
    }
}

// ===========================================================================
//  发信邮箱 fixture（2026-10-16 取代 env 版 `unreachable_smtp`）
// ===========================================================================

/// 测试用主密钥（与 `MailAccountSecret::default()` 的 `[0x5A; 32]` 同值）。
pub(crate) const TEST_MAIL_SECRET: [u8; 32] = [0x5Au8; 32];

/// ⚠️ **含 `<` / `>`**：让 lettre 的 `Mailbox` 解析必然失败 ⇒ 零网络 IO 的
/// 「SMTP 层失败」。见 [`bootstrap_with_sender_account`] 的 doc。
pub(crate) const TEST_ACCOUNT_NAME: &str = "测试<发件人>";

/// 把测试主密钥写进进程 env（`crypto::seal` / `open` 每次重读 env）。
///
/// ⚠️ `std::env::set_var` 在 edition 2024 是 unsafe，且**不能**与同进程的并发
/// 读写竞争 ⇒ 用 `OnceLock::get_or_init` 串成「第一个调用者写、其余调用者等它写完」
/// （本 binary 里所有用到密文的路径都先过本函数）。仓内同款手法见
/// `tests/cos_opendal_api.rs` 的 `ENV_LOCK`。
pub(crate) fn ensure_mail_secret_env() {
    static ONCE: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    ONCE.get_or_init(|| {
        let hex: String = TEST_MAIL_SECRET
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        // SAFETY: 本 binary 内所有需要该 env 的代码路径都先过本函数（由
        // `OnceLock` 串行），不存在与其它线程的 env 读写并发。
        unsafe { std::env::set_var(MAIL_ACCOUNT_SECRET_ENV, hex) };
    });
}

/// 插一行 `t_mail_account`（授权码已加密）。
pub(crate) async fn insert_mail_account(pool: &PgPool, name: &str, account: &str) -> i64 {
    let id = shared_test_snowflake().next_id();
    let now = now_naive();
    let secret =
        crypto::seal_with(&TEST_MAIL_SECRET, "dummy-secret").expect("用测试主密钥加密必成功");
    sqlx::query(
        "INSERT INTO t_mail_account (id, version, name, preset, account, encrypted_secret, \
         is_default, created_at, created_by, updated_at, updated_by) \
         VALUES ($1, 0, $2, 'NET_EASE_163', $3, $4, true, $5, 0, $5, 0)",
    )
    .bind(id)
    .bind(name)
    .bind(account)
    .bind(secret)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_mail_account");
    id
}

/// 插一行 `t_mail_digest_config`，`sender_account_id` 指向给定邮箱。
///
/// 直插 SQL 而不走 `POST /config`：这批用例要的是「上下文里有可用通道」这个**前置**，
/// 而走 API 还得先算 `version`、再逐个填必填字段，会让每个用例多三行样板。
/// 值本身仍按 DTO 的缺省写（`DAILY` / 两节都开 / 10 列），与 `config_body()` 一致。
/// ⚠️ 这份字面量与 `VisibleColumn::DEFAULT_SET` + 迁移里的列 DEFAULT 是**三条副本**，
/// 改缺省列时三处必须一起改。
pub(crate) async fn insert_digest_config(pool: &PgPool, sender_account_id: i64) {
    let id = shared_test_snowflake().next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_mail_digest_config (id, version, enabled, frequency, send_hour, \
         send_minute, recipients, cc_recipients, include_overdue, include_upcoming, \
         upcoming_days, visible_columns, sender_account_id, created_at, created_by, \
         updated_at, updated_by) \
         VALUES ($1, 0, true, 'DAILY', 8, 0, '{}', '{}', true, true, 14, \
         '{serial_no,drawing_no,name,batch_no,quantity,customer_name,status,location,system_delivery_date,remaining_days}', \
         $2, $3, 0, $3, 0)",
    )
    .bind(id)
    .bind(sender_account_id)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_mail_digest_config");
}

// ===========================================================================
//  时间基准（**禁止**在用例里出现 `2026-10-13` 这类字面量）
// ===========================================================================

/// 「今天」（Asia/Shanghai，naive）。本域全部断言围绕 today±N 天，
/// 写死绝对日期会在每月 1 号再次变红（CLAUDE.md「测试数据两条硬约定」第 2 条）。
pub(crate) fn today() -> NaiveDate {
    now_naive().date()
}

// ===========================================================================
//  数据 fixture helper（直插，域独享）
// ===========================================================================

/// 插一个一级客户（`serial_prefix` 必填且只能是大写单字母；一级客户的 prefix
/// 有唯一索引 `uq_t_customer_root_prefix`，同一用例内插第二个要换 prefix）。
pub(crate) async fn insert_customer(pool: &PgPool, name: &str) -> i64 {
    insert_root_customer(pool, name, 'M').await
}

/// 同 [`insert_customer`]，但显式指定一级客户的 `serial_prefix`。
pub(crate) async fn insert_root_customer(pool: &PgPool, name: &str, prefix: char) -> i64 {
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

/// 造一个 `t_part` 工单（`assembly_id = None` 即散件）。
#[allow(clippy::too_many_arguments)]
pub(crate) async fn insert_part(
    pool: &PgPool,
    customer_id: i64,
    drawing_no: &str,
    name: &str,
    status: &str,
    system_date: Option<NaiveDate>,
    assembly_id: Option<i64>,
    quantity: i32,
    serial_no: Option<&str>,
    order_no: Option<&str>,
) -> i64 {
    let id = shared_test_snowflake().next_id();
    let now = now_naive();
    let req = today();
    sqlx::query(
        "INSERT INTO t_part (id, serial_no, name, drawing_no, applicant_name, quantity, \
         unit_price, total_price, request_date, planned_delivery_date, system_delivery_date, \
         status, is_urgent, customer_id, assembly_id, order_no, version, created_at, updated_at) \
         VALUES ($1, $2, $3, $4, 'tester', $5, 0, 0, $6, $6, $7, $8, false, $9, $10, $11, 0, $12, $12)",
    )
    .bind(id)
    .bind(serial_no)
    .bind(name)
    .bind(drawing_no)
    .bind(quantity)
    .bind(req)
    .bind(system_date)
    .bind(status)
    .bind(customer_id)
    .bind(assembly_id)
    .bind(order_no)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_part");
    id
}

/// 造一个 `t_assembly` 装配件（`t_part.assembly_id` 指向它即成子件）。
pub(crate) async fn insert_assembly(
    pool: &PgPool,
    customer_id: i64,
    drawing_no: &str,
    name: &str,
    status: &str,
    system_date: Option<NaiveDate>,
    quantity: i32,
) -> i64 {
    let id = shared_test_snowflake().next_id();
    let now = now_naive();
    let req = today();
    sqlx::query(
        "INSERT INTO t_assembly (id, serial_no, drawing_no, name, applicant_name, quantity, \
         unit_price, total_price, request_date, planned_delivery_date, system_delivery_date, \
         status, is_urgent, customer_id, version, created_at, updated_at) \
         VALUES ($1, NULL, $2, $3, 'tester', $4, 0, 0, $5, $5, $6, $7, false, $8, 0, $9, $9)",
    )
    .bind(id)
    .bind(drawing_no)
    .bind(name)
    .bind(quantity)
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

/// 造一个批次（`status` 决定「已交量守卫」是否命中：`DELIVERED` / `COMPLETED`）。
pub(crate) async fn insert_batch(pool: &PgPool, part_id: i64, status: &str) -> i64 {
    insert_batch_full(pool, part_id, 1, 1, status, None, None, false).await
}

/// 造一个批次的**完整形态**（位置 / 多态持有人 / 返修标记都可指定）。
///
/// `location` 取 `t_part_batch.location` 的 5 个字面量之一（`OFFICE` /
/// `PRODUCTION_SHELF` / `WORKER` / `INSPECTION_SHELF` / `OUTSOURCE_COMPANY`），
/// `holder_id` 是**多态** holder（货架 / 工人 / 外协厂的 id，取决于 `location`）。
#[allow(clippy::too_many_arguments)]
pub(crate) async fn insert_batch_full(
    pool: &PgPool,
    part_id: i64,
    batch_no: i32,
    quantity: i32,
    status: &str,
    location: Option<&str>,
    holder_id: Option<i64>,
    is_repairing: bool,
) -> i64 {
    let id = shared_test_snowflake().next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_part_batch (id, part_id, batch_no, quantity, status, location, \
         current_holder_id, is_repairing, version, created_at, updated_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, 0, $9, $9)",
    )
    .bind(id)
    .bind(part_id)
    .bind(batch_no)
    .bind(quantity)
    .bind(status)
    .bind(location)
    .bind(holder_id)
    .bind(is_repairing)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_part_batch");
    id
}

/// 造一个工人（`t_worker.name` = 邮件「位置」列在 `WORKER` 档的取值）。
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

/// 造一个货架（邮件「位置」列在两种货架档取的是 **`code`** 不是 `name`）。
pub(crate) async fn insert_shelf(pool: &PgPool, code: &str) -> i64 {
    let id = shared_test_snowflake().next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_shelf (id, code, name, zone, is_active, version, display_order, \
         created_at, updated_at) VALUES ($1, $2, $3, 'PRODUCTION', true, 0, 0, $4, $4)",
    )
    .bind(id)
    .bind(code)
    // `name` 与 `code` 同源不同形：正因为「位置」列取的是 `code`，这里必须让两者
    // **不可混淆**（取到 `name` 的用例会红），故 `name` 加一个 `货架` 前缀。
    .bind(format!("货架{code}"))
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_shelf");
    id
}

/// 造一个外协公司（邮件「位置」列在 `OUTSOURCE_COMPANY` 档的取值）。
pub(crate) async fn insert_outsource_company(pool: &PgPool, name: &str) -> i64 {
    let id = shared_test_snowflake().next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_outsource_company (id, name, is_active, version, created_at, updated_at) \
         VALUES ($1, $2, true, 0, $3, $3)",
    )
    .bind(id)
    .bind(name)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_outsource_company");
    id
}

/// `t_mail_digest_log` 当前行数（用于「**不得**产生 log 行」这类否定断言）。
pub(crate) async fn count_logs(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM t_mail_digest_log")
        .fetch_one(pool)
        .await
        .expect("count t_mail_digest_log")
}

/// 直插一行发送流水（`logs.rs` 分页用；发送路径本身用真发信造行）。
pub(crate) async fn insert_log_row(
    pool: &PgPool,
    config_version: i32,
    trigger_source: &str,
    to_recipients: &str,
    sent_at: chrono::NaiveDateTime,
) -> i64 {
    let id = shared_test_snowflake().next_id();
    sqlx::query(
        "INSERT INTO t_mail_digest_log (id, config_version, trigger_source, to_recipients, \
         cc_recipients, subject, overdue_count, upcoming_count, html_bytes, status, \
         error_message, sent_at, created_at) \
         VALUES ($1, $2, $3, $4, '', '交期汇总', 1, 1, 1024, 'SUCCESS', NULL, $5, $5)",
    )
    .bind(id)
    .bind(config_version)
    .bind(trigger_source)
    .bind(to_recipients)
    .bind(sent_at)
    .execute(pool)
    .await
    .expect("insert t_mail_digest_log");
    id
}

// ===========================================================================
//  请求体 / 请求 helper
// ===========================================================================

/// 一份**合法**的 `POST /config` 请求体（各用例只覆盖自己要改的键）。
///
/// `frequency` / `recipients` / `visible_columns` 三项在 DTO 上**无
/// `#[serde(default)]`**（= 必填），`version` 亦然。
pub(crate) fn config_body(version: i32) -> Value {
    json!({
        "version": version,
        "enabled": false,
        "frequency": "DAILY",
        "send_hour": 8,
        "send_minute": 0,
        "recipients": ["ops@example.com"],
        "include_overdue": true,
        "include_upcoming": true,
        "upcoming_days": 14,
        "visible_columns": [
            "serial_no", "drawing_no", "name", "quantity",
            "customer_name", "status", "system_delivery_date", "remaining_days"
        ]
    })
}

/// 一份**带发信邮箱**的 `POST /config` 请求体。
///
/// `sender_account_id` 是 **JSON string**（与出参对称；`deserialize_i64_opt` 只收
/// 字符串或 `null`，收数字会 422）。
pub(crate) fn config_body_with_sender(version: i32, sender_account_id: i64) -> Value {
    let mut body = config_body(version);
    body["sender_account_id"] = json!(sender_account_id.to_string());
    body
}

/// 断言信封里的业务码。
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

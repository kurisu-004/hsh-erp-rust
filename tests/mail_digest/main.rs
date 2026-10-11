//! mail_digest 域集成测试（2026-10-13 新建，域同批新建）
//!
//! 本 binary 与 `src/modules/mail_digest/` 同批落地，覆盖 6 个端点的
//! HTTP 契约 + 统计 SQL 口径 + 发送流水语义。
//!
//! ## 拆分映射（4 个 sub-file）
//! - `config.rs`  —— 端点 1/2：读单例 + 全量覆写（OCC / 422 / 409）+ 校验矩阵
//! - `preview.rs` —— 端点 3：渲染（工单级行单位 / 已交量守卫 / 节开关 / 列白名单 / XSS）
//! - `send.rs`    —— 端点 4：`/send` 的三条失败路径 + 「失败也落流水」不变量
//! - `logs.rs`    —— 端点 6：分页 + 收件人快照 + `config_version` 快照
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

mod config;
mod logs;
mod preview;
mod send;

use std::sync::Arc;

use axum::http::StatusCode;
use chrono::NaiveDate;
use serde_json::{Value, json};
use sqlx::PgPool;

use hsh_erp_rust::infra::clock::now_naive;
use hsh_erp_rust::infra::config::{AppConfig, MailConfig};
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
}

/// 默认（SMTP **未**配置）上下文：绝大多数用例用它。
pub(crate) async fn bootstrap() -> Ctx {
    bootstrap_with_mail(MailConfig::default()).await
}

/// 指定 SMTP 配置的上下文。
///
/// ⚠️ `MailConfig` 走 env、不进 DB，集成测试**没有** env 通道，只能在
/// `AppState.config` 这一层替换（手法同 `test_state_with_wecom`：
/// `Arc::get_mut` 拿唯一 state Arc → 整体替换 `config` Arc）。
pub(crate) async fn bootstrap_with_mail(mail: MailConfig) -> Ctx {
    let pool = test_pool().await;
    let fx = load_part_fixture(&pool).await;
    let mut state = test_state(pool.clone()).await;
    {
        let inner = Arc::get_mut(&mut state).expect("state Arc 必须 unique");
        let old_cfg = (*inner.config).clone();
        inner.config = Arc::new(AppConfig { mail, ..old_cfg });
    }
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
    }
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
    let id = shared_test_snowflake().next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_part_batch (id, part_id, batch_no, quantity, status, version, \
         created_at, updated_at) VALUES ($1, $2, 1, 1, $3, 0, $4, $4)",
    )
    .bind(id)
    .bind(part_id)
    .bind(status)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_part_batch");
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

/// 指向 `127.0.0.1:1` 的「必然连不通」SMTP 配置（端口 1 无监听 ⇒ connect 立刻 refused）。
pub(crate) fn unreachable_smtp() -> MailConfig {
    MailConfig {
        host: "127.0.0.1".to_string(),
        port: 1,
        username: "sender@example.com".to_string(),
        password: "dummy-password".to_string(),
        from_address: "sender@example.com".to_string(),
        from_name: "测试发件人".to_string(),
        tls_mode: hsh_erp_rust::infra::config::MailTlsModeKind::None,
        digest_enabled: true,
    }
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

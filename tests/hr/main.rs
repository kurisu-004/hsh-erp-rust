//! `hr`（人事排班）域集成测试（**2026-10-19 新建 binary**，与新域同批）
//!
//! 覆盖 `/api/v2/hr` 的 10 个端点的 HTTP 契约 + 本域三条硬约束：
//! - **推导规则**（覆盖 > 全局 > 星期兜底）跨三张表，单元测试覆盖不到，全部落在
//!   `calendar.rs`；
//! - **时段校验 V1–V6**（`21903` 与 `21908` 是两个不同码）落在 `shifts.rs`；
//! - **权限与动词纪律**（CLERK 只读、`PUT`/`PATCH`/`DELETE` 不被路由）落在 `guards.rs`。
//!
//! ## ⚠️ 入口**必须是 `main.rs`**
//!
//! cargo 1.98.1 **不识别** `tests/<dir>/mod.rs`，只识别 `tests/<dir>/main.rs`
//! （binary 名 = `<dir>`，即 `hr`）。
//!
//! ## 拆分映射（3 个 sub-file）
//! - `shifts.rs`   —— 端点 4/5/6/7：班次 CRUD + OCC + 时段 V1–V6 的失败矩阵
//! - `calendar.rs` —— 端点 1/2/3/8/9/10：**推导规则 6 条**（本域业务心脏）
//! - `guards.rs`   —— 端点 1–10 的权限矩阵 + HTTP 动词纪律
//!
//! ## 前置条件
//! - PG：`scripts/test_nextest.sh` 自带 session 容器与 per-test fresh database，
//!   并按序灌 `seeds/menu.sql` + `seeds/rbac.sql`（⇒ `hr.view` / `hr.schedule.manage`
//!   两个权限点与 MANAGER / CLERK 的授权基线都在库里）。
//! - **Redis**：走 `/iam/login` 拿 token ⇒ 本 binary 需要 Redis（同全仓其它 HTTP
//!   契约测试同款前提）。
//!
//! ## fixture 口径
//!
//! 全部基建取自 `hsh_erp_test_support::*`，本 binary **不重复声明**任何一条
//! `send` / `json_request` / `login_token` / `test_pool` / `test_state`；取号一律走
//! 仓库登记的唯一源 `shared_test_snowflake()`。
//!
//! ## ⚠️ 本域**不需要** `MAIL_ACCOUNT_SECRET`
//!
//! `tests/wecom` 那种 `ensure_mail_secret_env` 是因为 `wecom::webhook` 要对地址做
//! 加解密。`hr` 域**零凭据列**，故本 binary 不设那个 env（设了也只是空跑）。
//! 但**仍要 CLERK token** —— 它是「只读越权」一整组用例的命脉。
//!
//! ## 测试日期用**固定远期日**而不是 `now()`
//!
//! 推导规则要断言「周六 / 周日给 `REST_DAY`、工作日给 `WORKDAY`」，用 `now()` 推导
//! 星期会让用例随日历漂移。基准日 [`BASE`] 是**已核过的周一**，[`date(offset)`] 在它
//! 上加天数 ⇒ 星期完全确定。远期（2030）是为了不与任何真实业务数据重叠
//! （每个用例是 fresh database，其实不重叠，但固定日期让失败可复现）。

#![allow(dead_code, clippy::await_holding_lock, unused_imports)]

mod calendar;
mod guards;
mod shifts;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::http::StatusCode;
use chrono::{Datelike, NaiveDate, Weekday};
use serde_json::{Value, json};
use sqlx::PgPool;

use hsh_erp_test_support::{
    PartFixture, load_part_fixture, login_token, shared_test_snowflake, test_app, test_pool,
    test_state,
};

// ===========================================================================
//  测试上下文
// ===========================================================================

/// 一个用例的公共上下文：fresh database + fixture + MANAGER / CLERK 两个 token。
pub(crate) struct Ctx {
    pub pool: PgPool,
    pub app: axum::Router,
    pub state: Arc<hsh_erp_rust::state::AppState>,
    pub manager_token: String,
    /// CLERK token 用于钉「只读」矩阵：读端点 200、**全部**写端点 40300。
    pub clerk_token: String,
}

/// 标准上下文。
pub(crate) async fn bootstrap() -> Ctx {
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
//  日期助手（推导规则用例的确定性基准）
// ===========================================================================

/// 基准日 = **2030-03-04，周一**（推导规则的「工作日」代表）。
pub(crate) const BASE: &str = "2030-03-04";

/// 基准日 + `offset` 天，返回 ISO 日期串（`2030-03-04` 形态，喂给入参 / URL）。
pub(crate) fn date(offset: i64) -> String {
    let base = NaiveDate::from_ymd_opt(2030, 3, 4).expect("基准日必合法");
    (base + chrono::Duration::days(offset))
        .format("%Y-%m-%d")
        .to_string()
}

/// 基准日 + `offset` 天的星期（用例里显式断言它，避免「以为那天是周六」）。
pub(crate) fn weekday_of(offset: i64) -> Weekday {
    let base = NaiveDate::from_ymd_opt(2030, 3, 4).expect("基准日必合法");
    (base + chrono::Duration::days(offset)).weekday()
}

// ===========================================================================
//  名字 / 班次编码去重计数器
// ===========================================================================

/// 用例级计数器（同一进程内多个用例各自造数据，避免撞 `uk_t_hr_shift_code`）。
static CODE_SEQ: AtomicU64 = AtomicU64::new(0);

/// 造一个本进程内唯一的班次编码（`varchar(30)`，前缀刻意留短）。
pub(crate) fn unique_code(prefix: &str) -> String {
    let n = CODE_SEQ.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}-{n}-{}", shared_test_snowflake().next_id())
}

/// 造一个**库里绝不会存在**的班次 id（幽灵行用例用）。
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

/// `GET {path}`（带 CLERK token —— 只读矩阵用例）。
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

/// 断言信封里的业务码 + HTTP 状态（双断言，缺一不可）。
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

// ===========================================================================
//  班次 / 日历 helper
// ===========================================================================

/// 段数组糖：`[("22:00","26:00"), …]`，`seq` 按序自动编号。
pub(crate) fn segs(pairs: &[(&str, &str)]) -> Value {
    Value::Array(
        pairs
            .iter()
            .enumerate()
            .map(|(i, (s, e))| json!({ "seq": i as u8 + 1, "start": s, "end": e }))
            .collect(),
    )
}

/// 建班次的最小请求体（`segments` 必填，无它就是 axum 的 422 纯文本）。
pub(crate) fn create_body(code: &str, pairs: &[(&str, &str)]) -> Value {
    json!({
        "code": code,
        "name": "测试班次",
        "segments": segs(pairs),
    })
}

/// 建一个班次并返回它的出参 `data`（失败即 panic —— 造数据是前置，不是被测对象）。
pub(crate) async fn create_shift(ctx: &Ctx, code: &str, pairs: &[(&str, &str)]) -> Value {
    let (status, env) = post_as_manager(ctx, "/hr/shifts", create_body(code, pairs)).await;
    assert_eq!(status, StatusCode::OK, "建班次应成功：{env}");
    env["data"].clone()
}

/// 出参里的雪花 `id`（**JSON string**，防 JS 精度截断）。
pub(crate) fn id_of(v: &Value) -> i64 {
    v["id"]
        .as_str()
        .expect("id 必须是 JSON string（雪花防精度截断）")
        .parse()
        .expect("id 必须是数字串")
}

/// 某班次 id 的行内 `version`（OCC 锚）。
pub(crate) fn version_of(v: &Value) -> i32 {
    v["version"].as_i64().expect("version 是数字") as i32
}

/// 班次区间日历的路径。
pub(crate) fn cal_path(shift_id: i64, from: &str, to: &str) -> String {
    format!("/hr/shifts/{shift_id}/calendar?from={from}&to={to}")
}

/// 全局日历区间读的路径。
pub(crate) fn global_cal_path(from: &str, to: &str) -> String {
    format!("/hr/calendar/days?from={from}&to={to}")
}

/// 写一条全局日历显式行（端点 2），失败即 panic。
pub(crate) async fn write_global_day(ctx: &Ctx, day: &str, day_type: &str, name: Option<&str>) {
    let mut item = json!({ "cal_date": day, "day_type": day_type });
    if let Some(n) = name {
        item["name"] = json!(n);
    }
    let (status, env) = post_as_manager(ctx, "/hr/calendar/days", json!({ "items": [item] })).await;
    assert_eq!(status, StatusCode::OK, "写全局日历行应成功：{env}");
}

/// 写一条班次覆盖行（端点 9），失败即 panic。
pub(crate) async fn write_override_day(ctx: &Ctx, shift_id: i64, day: &str, day_type: &str) {
    let (status, env) = post_as_manager(
        ctx,
        &format!("/hr/shifts/{shift_id}/calendar/days"),
        json!({ "items": [{ "cal_date": day, "day_type": day_type }] }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "写班次覆盖行应成功：{env}");
}

/// 停用班次（端点 7），失败即 panic。
pub(crate) async fn deactivate_shift(ctx: &Ctx, shift_id: i64, version: i32) {
    let (status, env) = post_as_manager(
        ctx,
        &format!("/hr/shifts/{shift_id}/deactivate"),
        json!({ "version": version }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "停用班次应成功：{env}");
}

/// 读某班次区间日历的 `items` 数组（失败即 panic）。
pub(crate) async fn shift_calendar_items(
    ctx: &Ctx,
    shift_id: i64,
    from: &str,
    to: &str,
) -> Vec<Value> {
    let (status, env) = get_as_manager(ctx, &cal_path(shift_id, from, to)).await;
    assert_eq!(status, StatusCode::OK, "读班次日历应成功：{env}");
    env["data"]["items"]
        .as_array()
        .unwrap_or_else(|| panic!("items 是数组：{env}"))
        .clone()
}

/// 读全局日历区间，逐日抽出 `cal_date` 的 map（`derived` / `day_type` 断言用）。
pub(crate) async fn global_calendar_map(ctx: &Ctx, from: &str, to: &str) -> Vec<Value> {
    let (status, env) = get_as_manager(ctx, &global_cal_path(from, to)).await;
    assert_eq!(status, StatusCode::OK, "读全局日历应成功：{env}");
    env["data"]["items"]
        .as_array()
        .unwrap_or_else(|| panic!("items 是数组：{env}"))
        .clone()
}

/// 某班次在某区间的**未软删**覆盖行条数（停用不级联那条断言用）。
pub(crate) async fn live_override_count(pool: &PgPool, shift_id: i64) -> i64 {
    sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM t_hr_shift_calendar WHERE shift_id = $1 AND deleted_at IS NULL",
    )
    .bind(shift_id)
    .fetch_one(pool)
    .await
    .expect("数 t_hr_shift_calendar 活行")
}

//! `hr`（人事排班）域 HTTP handler：10 个端点 + 路由表（**2026-10-19 新建**）。
//!
//! 端点（挂在 `/api/v2/hr`，由 [`router`] 桥接）：
//!
//! | # | Method | Path | 权限点 | 说明 |
//! |---|---|---|---|---|
//! | 1 | `GET` | `/calendar/days?from&to` | `hr.view` | 全局日历区间读 |
//! | 2 | `POST` | `/calendar/days` | `hr.schedule.manage` | 批量 upsert 全局日历显式行 |
//! | 3 | `POST` | `/calendar/days/delete` | `hr.schedule.manage` | 删显式行 → 回退推导 |
//! | 4 | `GET` | `/shifts` | `hr.view` | 班次列表（含派生列） |
//! | 5 | `POST` | `/shifts` | `hr.schedule.manage` | 建班次 |
//! | 6 | `POST` | `/shifts/{id}` | `hr.schedule.manage` | 改班次（部分覆盖 + OCC） |
//! | 7 | `POST` | `/shifts/{id}/deactivate` | `hr.schedule.manage` | 停用（软下架） |
//! | 8 | `GET` | `/shifts/{id}/calendar?from&to` | `hr.view` | 班次区间日历 |
//! | 9 | `POST` | `/shifts/{id}/calendar/days` | `hr.schedule.manage` | 批量 upsert 覆盖行 |
//! | 10 | `POST` | `/shifts/{id}/calendar/days/delete` | `hr.schedule.manage` | 清覆盖 → 回退全局 |
//!
//! ## 零 PUT / PATCH / DELETE
//!
//! 与 `wecom` / `serial` 域同款：`/calendar/days/delete` 与
//! `/shifts/{id}/calendar/days/delete` 都是 **`POST`** 动词 + 路径里的 `/delete` 段，
//! 不是 HTTP `DELETE`。范本：`.route("/calendar/days/delete", post(delete_calendar_days))` ——
//! handler 函数名带 `delete_` 前缀是安全的（路由比对器判的是 `delete(`，不是 `delete_`）。
//!
//! ## 权限守卫：**不在本文件**（判在 service 第一行）
//!
//! 10 个 handler 都不写 `require_permission` —— 逐字在 `hr::service` 的每个用例第一行
//! （[`crate::modules::hr::service`] 模块 doc 的「权限守卫」段）。范式同
//! `prod::shelf_process` / `prod::work_type` / `prod::inspection`。
//!
//! ## ⚠️ 事务边界：**读端点不开事务、写端点开**
//!
//! | 端点 | 事务 | 理由 |
//! |---|---|---|
//! | 3 条 `GET` | **不开**，`pool.acquire()` | 纯只读。为一次预览开事务会让 PG 的只读快照把连接按请求时长占住，且没有任何写要提交 |
//! | 7 条 `POST` | `pool.begin()` → service → `tx.commit()` | 一批 upsert / 删行必须**全成功或全回滚**：一批日历写一半进去，管理员看到的是一张既不是旧版也不是新版的日历 |
//!
//! ⚠️ 无权账号会**先开一次事务**才被 service 的权限闸拒掉（守卫在 service 而非
//! handler），代价只是短连接占用，零写入、不落锁。取舍理由见 `service` 模块 doc。
//!
//! ## ⚠️ 零 WS 广播（有意取舍，理由见 `service` 模块 doc 的「零 WS 广播」段）
//!
//! ## ⚠️ matchit 注册顺序：**本域路由表内无冲突**
//!
//! 本域唯一的 `Path<i64>` catch-all 在 `/shifts/{id}`（**2 段**）。同段数的其它路由
//! 只有 `/calendar/days`（2 段、全静态）—— 两者第 1 段就分道（`calendar` vs `shifts`），
//! **不存在**「同段数下静态段与 catch-all 抢同一格」的情形（CLAUDE.md §9 那条硬约束
//! 讲的是 `/counts` 与 `/quotable-parts` 那种**第 2 段**的冲突）。
//! `/shifts/{id}/deactivate` 与 `/shifts/{id}/calendar` 同为 3 段，但第 3 段都是静态
//! 段（`deactivate` / `calendar`）且互不为前缀 ⇒ 安全。
//!
//! ## 权威路由清单：[`ROUTES`]（lib 单测逐条比对 `router()` 源码）
//!
//! 比对器复用 `files::route_guard::routes_in_router_source`（共用比对器，**不**另抄一份），
//! 范式同 `serial::handler` / `wecom::handler`。

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::routing::{get, post};
use axum::{Json, Router};

use crate::auth::rbac::CurrentUser;
use crate::modules::hr::dto::{
    CalendarDaysDeleteRequest, CalendarDaysUpsertRequest, CalendarRangeQuery, CreateShiftRequest,
    DeactivateShiftRequest, ShiftCalendarRangeQuery, ShiftDaysDeleteRequest,
    ShiftDaysUpsertRequest, ShiftListQuery, UpdateShiftRequest,
};
use crate::modules::hr::service;
use crate::modules::hr::vo::{
    CalendarDaysOut, CalendarDaysWriteOut, ShiftCalendarOut, ShiftDaysWriteOut, ShiftListOut,
    ShiftOut,
};
use crate::shared::error::AppError;
use crate::shared::response::R;
use crate::state::AppState;

// ============================================================================
// 端点 1–3：全局日历
// ============================================================================

/// `GET /api/v2/hr/calendar/days?from&to` —— 全局日历区间读。
///
/// 区间内**逐日一行，一天都不省**：显式行返回真实 `day_type` / `name`
/// （`derived = false`），无显式行的那天按星期推导（周六周日 = `REST_DAY`，
/// 否则 `WORKDAY`）并标 `derived = true`。
///
/// - 权限点 `hr.view`（service 内守卫）。
/// - 区间校验：`from <= to` 且跨度 ≤ 366 天，否则 `21904`。
/// - 读端点不开事务、不广播 WS。
pub async fn list_calendar_days(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Query(query): Query<CalendarRangeQuery>,
) -> Result<Json<R<CalendarDaysOut>>, AppError> {
    let mut conn = state.pool.acquire().await?;
    let out = service::list_calendar_days(&mut conn, &current, &query).await?;
    Ok(Json(R::ok(out)))
}

/// `POST /api/v2/hr/calendar/days` —— 批量 upsert 全局日历显式行。
///
/// 入参 `{"items":[{"cal_date":"2026-10-01","day_type":"STATUTORY_HOLIDAY","name":"国庆节"}]}`。
/// ⚠️ 日历**只存例外**：常态工作日 / 常态周末不需要写行，读端点会推导。
/// ⚠️ `CUSTOM_LEAVE` 必须带 `name`（否则 `21907`）；未知 `day_type` → `21905`。
/// ⚠️ 事务：整批全成功或全回滚。
pub async fn upsert_calendar_days(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Json(req): Json<CalendarDaysUpsertRequest>,
) -> Result<Json<R<CalendarDaysWriteOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = service::upsert_calendar_days(&mut tx, &state.snowflake, &current, &req).await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// `POST /api/v2/hr/calendar/days/delete` —— 删显式行，那几天回退到按星期推导。
///
/// 入参二选一：`{"dates":["2026-10-01", …]}` 或 `{"from":"2026-10-01","to":"2026-10-07"}`。
/// ⚠️ `affected = 0` 是**合法成功**（那几天本来就没有显式行）。
pub async fn delete_calendar_days(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Json(req): Json<CalendarDaysDeleteRequest>,
) -> Result<Json<R<CalendarDaysWriteOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = service::delete_calendar_days(&mut tx, &current, &req).await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

// ============================================================================
// 端点 4–7：班次
// ============================================================================

/// `GET /api/v2/hr/shifts?include_inactive` —— 班次列表。
///
/// 出参含三个**后端算好**的派生列：`cross_day` / `overflow_minutes` /
/// `daily_minutes`。前端**不重复实现**推导。
/// ⚠️ `include_inactive` 缺省 `false`（停用项默认不出现在列表里）。
pub async fn list_shifts(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Query(query): Query<ShiftListQuery>,
) -> Result<Json<R<ShiftListOut>>, AppError> {
    let mut conn = state.pool.acquire().await?;
    let out = service::list_shifts(&mut conn, &current, &query).await?;
    Ok(Json(R::ok(out)))
}

/// `POST /api/v2/hr/shifts` —— 建班次。
///
/// ⚠️ `segments` **必填**（缺了 → axum `Json` 提取器返 **HTTP 422 纯文本**）：
/// 没有时段的班次没有意义。
/// ⚠️ `code` 重复 → `21902`（HTTP 409，**不是** `40901` —— 后者只表达 OCC）。
pub async fn create_shift(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Json(req): Json<CreateShiftRequest>,
) -> Result<Json<R<ShiftOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = service::create_shift(&mut tx, &state.snowflake, &current, &req).await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// `POST /api/v2/hr/shifts/{id}` —— 改班次（**部分覆盖** + 标准 OCC）。
///
/// ⚠️ `version` **必填且无 `#[serde(default)]`**（CLAUDE.md §8）：缺失 → axum `Json`
/// 提取器返 **HTTP 422 纯文本**；陈旧 → `40901`。
/// ⚠️ `segments` 是**单列覆盖**（传了整列替换，不做段级 merge）。
/// ⚠️ 对已停用班次做本操作 → `21906`（HTTP 409）。
pub async fn update_shift(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
    Json(req): Json<UpdateShiftRequest>,
) -> Result<Json<R<ShiftOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = service::update_shift(&mut tx, &current, id, &req).await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// `POST /api/v2/hr/shifts/{id}/deactivate` —— 停用（软下架）。
///
/// ⚠️ body **必含 `version`**（同 `POST /shifts/{id}`）。
/// ⚠️ **不软删** `t_hr_shift` 行、**不级联**清 `t_hr_shift_calendar` 的覆盖行 ——
/// 历史考勤还要复现当年那天的口径。
/// ⚠️ 重复停用 → `21906`（幂等拒绝）。
pub async fn deactivate_shift(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
    Json(req): Json<DeactivateShiftRequest>,
) -> Result<Json<R<ShiftOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = service::deactivate_shift(&mut tx, &current, id, &req).await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

// ============================================================================
// 端点 8–10：班次日历
// ============================================================================

/// `GET /api/v2/hr/shifts/{id}/calendar?from&to` —— 班次的工作日历区间读。
///
/// 每一天回传 `global_day_type`（全局日历说的，已含按星期推导）与
/// `effective_day_type`（覆盖行 / 全局行 / 星期推导三级推导的结果）、`is_workday`、
/// `is_override`。**前端不重复实现推导**。
/// ⚠️ 班次不存在 / 已软删 → `21901`（HTTP 404）；**停用**的班次照常可读
/// （停用只挡写操作）。
pub async fn list_shift_calendar(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
    Query(query): Query<ShiftCalendarRangeQuery>,
) -> Result<Json<R<ShiftCalendarOut>>, AppError> {
    let mut conn = state.pool.acquire().await?;
    let out = service::list_shift_calendar(&mut conn, &current, id, &query).await?;
    Ok(Json(R::ok(out)))
}

/// `POST /api/v2/hr/shifts/{id}/calendar/days` —— 批量 upsert 班次覆盖行。
///
/// 入参 `{"items":[{"cal_date":"2026-10-01","day_type":"MAKEUP_WORKDAY"}]}`。
/// ⚠️ 覆盖行**没有** `name` 字段（本域表也没这列），要写说明走 `remark`。
/// ⚠️ 对已停用班次写 → `21906`。
pub async fn upsert_shift_days(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
    Json(req): Json<ShiftDaysUpsertRequest>,
) -> Result<Json<R<ShiftDaysWriteOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = service::upsert_shift_days(&mut tx, &state.snowflake, &current, id, &req).await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// `POST /api/v2/hr/shifts/{id}/calendar/days/delete` —— 清覆盖，回退到全局日历。
///
/// 入参二选一：`{"dates":[…]}` 或 `{"from","to"}`。
/// ⚠️ 清完**不需要**补一条「与全局相同」的覆盖行 —— 覆盖行是**例外表**，删掉就是
/// 「这天按全局来」。
pub async fn delete_shift_days(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(id): Path<i64>,
    Json(req): Json<ShiftDaysDeleteRequest>,
) -> Result<Json<R<ShiftDaysWriteOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = service::delete_shift_days(&mut tx, &current, id, &req).await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

// ============================================================================
// 路由表
// ============================================================================

/// 本域权威路由清单（**10 条**，由 `mod tests::routes_declared_in_router` 逐条比对
/// [`router`] 源码 —— 加 / 删端点忘登记就当场红）。
///
/// 比对器复用**共用**的 `files::route_guard::routes_in_router_source`，不另抄一份。
pub const ROUTES: &[&str] = &[
    "GET /calendar/days",
    "POST /calendar/days",
    "POST /calendar/days/delete",
    "GET /shifts",
    "POST /shifts",
    "POST /shifts/{id}",
    "POST /shifts/{id}/deactivate",
    "GET /shifts/{id}/calendar",
    "POST /shifts/{id}/calendar/days",
    "POST /shifts/{id}/calendar/days/delete",
];

/// `hr` 域 axum 子路由（不含公共前缀；由 [`crate::modules::hr::router`]
/// 桥接到 `/hr`）。
pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route(
            "/calendar/days",
            get(list_calendar_days).post(upsert_calendar_days),
        )
        .route("/calendar/days/delete", post(delete_calendar_days))
        .route("/shifts", get(list_shifts).post(create_shift))
        .route("/shifts/{id}", post(update_shift))
        .route("/shifts/{id}/deactivate", post(deactivate_shift))
        .route("/shifts/{id}/calendar", get(list_shift_calendar))
        .route("/shifts/{id}/calendar/days", post(upsert_shift_days))
        .route("/shifts/{id}/calendar/days/delete", post(delete_shift_days))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::Router;

    use super::{ROUTES, router};
    use crate::modules::files::route_guard::routes_in_router_source;
    use crate::state::AppState;

    /// `ROUTES` 必须与 `router()` 源码**逐条**一致（不多、不少、同序）。
    ///
    /// ⚠️ 复用 `files::route_guard::routes_in_router_source`（**不复制一份**）：那份是
    /// files 域三个 router 共用的比对器，`#[cfg(test)] pub(crate)` 在 crate 内跨模块
    /// 可达。抄一份会让「解析规则」变成两份可漂移的散文。
    #[test]
    fn routes_declared_in_router() {
        let actual = routes_in_router_source(include_str!("handler.rs"), "router");
        assert_eq!(
            actual.len(),
            ROUTES.len(),
            "`router()` 注册了 {} 条路由，ROUTES 只列了 {} 条（新增端点忘了登记？）\n\
             router(): {:?}\nROUTES: {:?}",
            actual.len(),
            ROUTES.len(),
            actual,
            ROUTES,
        );
        for (got, want) in actual.iter().zip(ROUTES.iter()) {
            assert_eq!(got, want, "`router()` 与 ROUTES 的条目不一致");
        }
    }

    /// 本域 10 条路由、无重复，且**零 PUT / PATCH / DELETE**。
    #[test]
    fn routes_have_no_duplicates() {
        let mut seen = std::collections::BTreeSet::new();
        for r in ROUTES.iter() {
            assert!(seen.insert(*r), "ROUTES 里 `{r}` 重复了");
        }
        for r in ROUTES.iter() {
            assert!(
                r.starts_with("GET ") || r.starts_with("POST "),
                "本域零 PUT/PATCH/DELETE，实测到：`{r}`"
            );
        }
        assert_eq!(
            ROUTES.len(),
            10,
            "本域端点数是 10；改端点必须同步改这里与 mod.rs 端点表"
        );
    }

    /// `router()` 本身能构造出来（挡掉「写错了 axum API 签名」这类编译期之外的坑）。
    #[test]
    fn router_builds() {
        let r: Router<Arc<AppState>> = router();
        drop(r);
    }
}

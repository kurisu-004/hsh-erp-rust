//! wx::production 子模块 service 层 —— 业务逻辑（ZST + 静态方法）
//!
//! 2026-10-11 新增。承接旧 `src/modules/wx/batches.rs` + `worker.rs` 的白名单校验
//! / 分页逻辑，**并新增** `tab ↔ DB` 映射表与 `t_user.worker_id` 解链。
//!
//! ## 本层是「2 类 tab 口径」的唯一拥有者（与 `wx::part_list` 同构）
//! | 前端 tab 值 | DB 状态集（过滤谓词） | `counts` 归桶 |
//! |---|---|---|
//! | `in_progress` | `['IN_PROCESS']` | `status='IN_PROCESS'`（+ `updated_at` 落当月） |
//! | `done` | `['DELIVERED','COMPLETED']`（**两个**） | `status IN (...)` + 当月存在 `DELIVERED` 事件 |
//!
//! ⚠️ **不**复用 `part::statemachine::PartStatus` 做校验 —— 那正是本次重构要消灭的
//! 跨域复用（`wx::mod.rs` 的模块 doc 有登记）。
//!
//! ## ★ `resolve_period` 从 `wx/mod.rs` 搬来（2026-10-11）
//! 旧位置是 `wx::resolve_period`（`pub(crate)`，被 `batches.rs` + `worker.rs`
//! 共用）。B3 把这两个 handler 一起搬进本域后，**wx 域只剩本域一个消费者**，故
//! 降级成本模块**私有**函数（连同它的 3 个单测）。`wx/mod.rs` 里那份已删除。
//!
//! ## ★ 未绑定工人（`t_user.worker_id IS NULL`）的形态
//! `worker: null` + `stats: { batchCount: 0, workHours: 0.0 }`，**HTTP 仍 200**。
//! 绝大多数系统账号（admin / 系统管理员 / `hmi-*` 等非工人账号）没有对应工人，
//! 回填脚本 `scripts/sql/20261011_backfill_t_user_worker_id.sql` 需人工确认后
//! 执行 ⇒ 在那之前绝大多数账号都会命中这一支。**不报错**是刻意的：把它做成
//! 401/403 会让非工人账号连**批次列表都看不了**。
//!
//! ⚠️ 「未绑定」与「已绑定但当月零工作量」在响应里**不可区分**（都是零值 /
//! `worker` 有值 vs `null`——其实可区分：`worker` 是否为 `null` 就是标志位）。
//! 若将来要区分，得在 `t_user` 上再加一个「已绑定」标记位，本轮不做。

use sqlx::PgConnection;

use crate::shared::error::AppError;

use super::dto::ProductionQuery;
use super::model::ProductionBatchRow;
use super::repo::ProductionRepo;
use super::vo::{
    BatchCountsOut, ProductionBatchCardOut, ProductionHomeOut, ProductionPageOut, WorkerOut,
    WorkerStatsOut,
};

/// 缺省每页条数（小程序首屏一次 10 张卡片）。
const DEFAULT_PAGE_SIZE: i64 = 10;

/// 每页条数上限（`clamp` 上界；防单个请求被拉成全表扫描）。
const MAX_PAGE_SIZE: i64 = 50;

/// `wx::production` service（ZST + 静态方法，与 `wx::part_list` /
/// `prod::process_design` 范本一致）。
pub struct ProductionService;

impl ProductionService {
    /// `GET /api/v2/wx/production` 业务逻辑：工人 + 统计 + 2 tab 角标 + 第 1 页卡片。
    ///
    /// 查询顺序：**先解链拿工人与统计**（未绑定直接给零值，省掉统计查询），再取
    /// 角标，最后取卡片。⚠️ 角标 `counts` 是**全局口径**（不带 `?tab=` 过滤）——
    /// 小程序两个 tab 的角标是固定的，切 tab 时不变。
    pub async fn home(
        conn: &mut PgConnection,
        user_id: i64,
        q: &ProductionQuery,
    ) -> Result<ProductionHomeOut, AppError> {
        // 0. period 归一化（缺省 → 当前月；非法 → 40001）：工人统计、角标、
        //    卡片三处共用同一个值，故**先**校验一次再往下传。
        let period = resolve_period(q.period.as_deref())?;

        // 1. 工人 + 当月统计（`t_user.worker_id → t_worker`；未绑定 → (null, 零值)）
        let (worker, stats) = worker_and_stats(conn, user_id, &period).await?;

        // 2. 角标：恒不依赖 `?tab=`，故不会因非法 tab 而单独失败
        let counts = ProductionRepo::batch_counts_by_period(conn, &period).await?;

        // 3. 卡片：与 `/page` 端点共用同一条查询路径
        let (list, has_more) = list_cards(conn, q, &period).await?;

        Ok(ProductionHomeOut {
            worker,
            stats,
            counts: BatchCountsOut {
                in_progress: counts.in_progress,
                done: counts.done,
            },
            list,
            has_more,
        })
    }

    /// `GET /api/v2/wx/production/page` 业务逻辑：纯增量（**不含** worker /
    /// stats / counts）。
    pub async fn page(
        conn: &mut PgConnection,
        q: &ProductionQuery,
    ) -> Result<ProductionPageOut, AppError> {
        let period = resolve_period(q.period.as_deref())?;
        let (list, has_more) = list_cards(conn, q, &period).await?;
        Ok(ProductionPageOut { list, has_more })
    }
}

// =============================================================================
//  工人 / 统计
// =============================================================================

/// 当前登录账号 → 工人 + 当月工作量统计（★ 2026-10-11 修掉的既有 bug）。
///
/// 旧实现把 `CurrentUser.id`（`t_user.id`）直接当 `t_part_event.worker_id`
/// （语义是 `t_worker.id`）查 —— 两表之间没有任何映射，实测对任何真实用户恒返
/// `batch_count: 0`。本函数按 B1 新增的 `t_user.worker_id` 列解链后再统计。
///
/// 返回 `(None, 零值)`（**不报错**）的情形：`t_user.worker_id IS NULL` /
/// 指向的工人不存在 / 工人已软删。详见模块 doc 的「未绑定工人」段。
async fn worker_and_stats(
    conn: &mut PgConnection,
    user_id: i64,
    period: &str,
) -> Result<(Option<WorkerOut>, WorkerStatsOut), AppError> {
    let Some(row) = ProductionRepo::find_worker_by_user(&mut *conn, user_id).await? else {
        return Ok((None, zero_stats()));
    };

    // 只有真的绑定了工人时才打统计查询（未绑定账号绝大多数，一次查询都省）
    let stats_row =
        ProductionRepo::worker_stats_by_period(&mut *conn, row.worker_id, period).await?;

    Ok((
        Some(WorkerOut {
            name: row.name,
            // ⚠️ `t_work_type` 可能不存在（工人未分配工种）或已软删 ⇒ NULL。
            // 归一成**空串**而不是 null：前端模板直接 `{{worker.workType}}`
            // 渲染，空串渲染成空白、null 渲染成字面量 `null`。见 vo.rs 与
            // docs/api/wx.md §8.7。
            work_type: row.work_type_name.unwrap_or_default(),
            // `t_worker` 无头像列 ⇒ 恒 None（前端组件有 `|| ''` 兜底走占位图标）
            avatar: None,
        }),
        WorkerStatsOut {
            batch_count: stats_row.batch_count,
            work_hours: stats_row.qty_sum as f64, // 件数累计作为 work_hours 估算
        },
    ))
}

/// 未绑定工人时的零值统计（**不**用 `Default` 派生，避免将来加字段时静默补 0）。
fn zero_stats() -> WorkerStatsOut {
    WorkerStatsOut {
        batch_count: 0,
        work_hours: 0.0,
    }
}

// =============================================================================
//  列表（两个端点共用）
// =============================================================================

/// 两个端点共用的**唯一**列表查询路径（★ 结构上保证「`?page=2` 两端点结果一致」）。
///
/// 步骤：`tab` 归一化 → 分页参数 clamp → 取 `size + 1` 条 → `has_more` 判定 →
/// 截断到 `size` → row→vo 投影。
///
/// ⚠️ `period` 由调用方**先**解析好传进来（[`ProductionService::home`] 解析一次
/// 给工人统计 / 角标 / 卡片三处共用），本函数不再重复解析。
async fn list_cards(
    conn: &mut PgConnection,
    q: &ProductionQuery,
    period: &str,
) -> Result<(Vec<ProductionBatchCardOut>, bool), AppError> {
    // 1. `?tab=` 白名单归一化（非法 → 40001 / HTTP 422）
    let statuses = tab_to_db_statuses(q.tab.as_str())?;

    // 2. 分页参数：page 缺省 1 / max(1)；size 缺省 10 / clamp(1, 50)
    let page = q.page.unwrap_or(1).max(1);
    let size = q.size.unwrap_or(DEFAULT_PAGE_SIZE).clamp(1, MAX_PAGE_SIZE);
    let offset = (page - 1) * size;

    // 3. 取 `size + 1` 条：超出的那一条就是「还有下一页」的证据。
    //    ⚠️ 不要为了拿 has_more 再打一条 COUNT —— 前端不读 total。
    let rows = ProductionRepo::list_batches(&mut *conn, statuses, period, size + 1, offset).await?;
    let has_more = rows.len() as i64 > size;

    // 4. 截断 + 投影
    let list = rows
        .into_iter()
        .take(size as usize)
        .map(row_to_card)
        .collect();

    Ok((list, has_more))
}

// =============================================================================
//  tab ↔ DB 映射
// =============================================================================

/// 前端 tab 值 → DB 状态集。
///
/// - `in_progress` → `["IN_PROCESS"]`
/// - `done` → `["DELIVERED", "COMPLETED"]`（**两个**）
/// - 其余（DB 原值 `IN_PROCESS` / 注入串 `; DROP TABLE t_part_batch`）→ **40001 /
///   HTTP 422**
///
/// ⚠️ 返回 `&'static [&'static str]`（不是 `Vec<String>`）：映射表是编译期常量，
/// 非法值**在拼进 SQL 之前**就被拒，SQL 侧不存在注入面（参数仍是绑定变量）。
fn tab_to_db_statuses(tab: &str) -> Result<&'static [&'static str], AppError> {
    match tab {
        "in_progress" => Ok(&["IN_PROCESS"]),
        "done" => Ok(&["DELIVERED", "COMPLETED"]),
        other => Err(AppError::validation(format!(
            "tab {other:?} 不在白名单（in_progress / done）"
        ))),
    }
}

/// DB 状态 → 前端 2 类 tab 值（折叠）。
///
/// ⚠️ 与 `wx::part_list` 的 4 类折叠**不同**：本域只有 2 个 tab，且 `?tab=` 白名单
/// 与本函数**一一对应** —— 能在列表里出现的 DB 状态必然落在某个 tab 集合内，
/// 不存在 `part_list` 那种「只计入 counts 却不在任何 tab 里」的静默兜底。
/// 即便如此仍保留 `_ => "in_progress"` 臂：SQL 的 `ANY($1)` 只按 tab 白名单过滤，
/// 未来若新增 tab 而忘了改这里，兜底至少不会让小程序渲染出未知字符串。
fn display_status(db_status: &str) -> &'static str {
    match db_status {
        "DELIVERED" | "COMPLETED" => "done",
        _ => "in_progress",
    }
}

/// `PartRow` → 卡片 VO。
fn row_to_card(r: ProductionBatchRow) -> ProductionBatchCardOut {
    ProductionBatchCardOut {
        id: r.id,
        serial_no: r.serial_no,
        name: r.name,
        code: r.drawing_no,
        // `planned_delivery_date` 是 NOT NULL 的 `date` 列；显式格式化（不靠
        // chrono 的 serde 行为）以钉死 `YYYY-MM-DD` 三段式。
        due_date: r.planned_delivery_date.format("%Y-%m-%d").to_string(),
        batch_no: r.batch_no,
        // ⚠️ 本批次量（`t_part_batch.quantity`），**不是** `wx::part_list` 卡片里
        // 那个同名 `batchQty`（那里取 `t_part.quantity`）。既有口径，本次不改，
        // 登记在 docs/api/wx.md §8.8。
        batch_qty: r.quantity,
        status: display_status(&r.status).to_string(),
        assigned_to: r.assigned_to,
        // ⚠️ Option 直传：无事件时是 null 而不是 0（前端 `!= null` 守门）
        work_hours: r.work_hours,
        finished_date: r.finished_date.map(|d| d.format("%Y-%m-%d").to_string()),
    }
}

// =============================================================================
//  period 归一化
// =============================================================================

/// 把可选 `period`（YYYY-MM）归一化：`None` → 当前月；`Some(s)` → 严格校验。
///
/// 2026-10-11 从 `wx/mod.rs`（原 `pub(crate)`，`batches` / `worker` 两个 handler
/// 共用）搬进本模块并**降级为私有** —— B3 之后 wx 域只剩本域一个消费者。
///
/// 设计：服务端 fallback 到当前月是为了让 mini-program 端不必每次拼 query 字符
/// 串；同时支持前端显式传 period（历史月份视图）。
///
/// 校验规则：
/// - 长度必须 7（`YYYY-MM`）
/// - 第 5 字节必须是 `-`
/// - 月份 ∈ `01..=12`
///
/// 2026-09-28 review #1 的历史：曾从 batches / worker 各抽一份副本抽到 wx 模块共享，
/// 本次反向收敛回本域单副本（三个用例也一并搬来，见文件底部）。
fn resolve_period(raw: Option<&str>) -> Result<String, AppError> {
    match raw {
        None => Ok(chrono::Local::now().format("%Y-%m").to_string()),
        Some(s) => {
            if s.len() != 7 || s.as_bytes()[4] != b'-' {
                return Err(AppError::validation(format!(
                    "period {s:?} 格式非法（要求 YYYY-MM）"
                )));
            }
            let month: u32 = s[5..7]
                .parse()
                .map_err(|_| AppError::validation(format!("period {s:?} 月份非法")))?;
            if !(1..=12).contains(&month) {
                return Err(AppError::validation(format!("period {s:?} 月份非法")));
            }
            Ok(s.to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // tab 白名单（非法值必须在拼 SQL 前被拒）
    // -----------------------------------------------------------------------

    #[test]
    fn two_tab_values_map_to_expected_db_status_sets() {
        assert_eq!(
            tab_to_db_statuses("in_progress").unwrap(),
            &["IN_PROCESS"][..]
        );
        // ★ done 是**两个**状态
        assert_eq!(
            tab_to_db_statuses("done").unwrap(),
            &["DELIVERED", "COMPLETED"][..]
        );
    }

    #[test]
    fn non_whitelisted_tab_is_validation_error() {
        // DB 原值不被接受（前端传的是 tab 值）
        for bad in ["IN_PROCESS", "DELIVERED", "COMPLETED", "GARBAGE", ""] {
            let e = tab_to_db_statuses(bad).unwrap_err();
            assert_eq!(e.code(), 40001, "{bad:?} 应是 40001");
        }
        // 注入串
        let e = tab_to_db_statuses("; DROP TABLE t_part_batch").unwrap_err();
        assert_eq!(e.code(), 40001);
    }

    /// 折叠方向与过滤表**必须**互为逆映射（否则 `list[].status` 会与 tab 打架）。
    #[test]
    fn display_status_is_inverse_of_the_filter_table() {
        for tab in ["in_progress", "done"] {
            let set = tab_to_db_statuses(tab).unwrap();
            for db in set {
                assert_eq!(
                    display_status(db),
                    tab,
                    "[{tab}] 状态 {db} 折叠后必须 == tab 名"
                );
            }
        }
        assert_eq!(display_status("DELIVERED"), "done");
        assert_eq!(display_status("COMPLETED"), "done");
        assert_eq!(display_status("IN_PROCESS"), "in_progress");
    }

    // -----------------------------------------------------------------------
    // period 归一化（3 个用例随函数一起从 wx/mod.rs 搬来）
    // -----------------------------------------------------------------------

    #[test]
    fn resolve_period_defaults_to_current_month() {
        let p = resolve_period(None).unwrap();
        assert_eq!(p.len(), 7);
        assert_eq!(p.as_bytes()[4], b'-');
    }

    #[test]
    fn resolve_period_accepts_valid() {
        assert_eq!(resolve_period(Some("2026-09")).unwrap(), "2026-09");
        assert_eq!(resolve_period(Some("2025-12")).unwrap(), "2025-12");
    }

    #[test]
    fn resolve_period_rejects_invalid() {
        assert!(resolve_period(Some("2026-9")).is_err()); // 月份 1 位
        assert!(resolve_period(Some("2026/09")).is_err()); // 分隔符错
        assert!(resolve_period(Some("2026-13")).is_err()); // 月份 13
        assert!(resolve_period(Some("2026-00")).is_err()); // 月份 0
        assert!(resolve_period(Some("26-09")).is_err()); // 年份 2 位
    }

    // -----------------------------------------------------------------------
    // row → vo 投影
    // -----------------------------------------------------------------------

    fn row() -> ProductionBatchRow {
        ProductionBatchRow {
            id: 2256,
            serial_no: Some("F2256-01".into()),
            name: "法兰盘 DN80".into(),
            drawing_no: "FL-DN80-A2".into(),
            batch_no: 3,
            quantity: 7,
            status: "DELIVERED".into(),
            assigned_to: Some("李伟".into()),
            planned_delivery_date: chrono::NaiveDate::from_ymd_opt(2026, 10, 20).unwrap(),
            finished_date: chrono::NaiveDate::from_ymd_opt(2026, 10, 22),
            work_hours: Some(12.5),
        }
    }

    #[test]
    fn row_projection_keeps_dates_as_yyyy_mm_dd_strings() {
        let v = serde_json::to_value(row_to_card(row())).expect("serialize");
        assert_eq!(v["id"], serde_json::json!("2256"));
        assert_eq!(v["code"], serde_json::json!("FL-DN80-A2"));
        assert_eq!(v["dueDate"], serde_json::json!("2026-10-20"));
        assert_eq!(v["finishedDate"], serde_json::json!("2026-10-22"));
        assert_eq!(v["batchNo"], serde_json::json!(3));
        assert_eq!(v["status"], serde_json::json!("done"));
    }

    /// 无工时 / 无完成日的行必须投影成 `null`（前端 `!= null` 守门）。
    #[test]
    fn row_projection_keeps_missing_work_hours_as_null() {
        let mut r = row();
        r.work_hours = None;
        r.finished_date = None;
        r.assigned_to = None;
        let v = serde_json::to_value(row_to_card(r)).expect("serialize");
        assert_eq!(v["workHours"], serde_json::Value::Null);
        assert_eq!(v["finishedDate"], serde_json::Value::Null);
        assert_eq!(v["assignedTo"], serde_json::Value::Null);
    }
}

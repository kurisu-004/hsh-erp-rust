//! wx::part_list 子模块 service 层 —— 业务逻辑（ZST + 静态方法）
//!
//! 2026-10-11 新增。承接旧 `src/modules/wx/repo.rs::map_counts_by_status` 与
//! `parts.rs::list` 的归桶 / 校验逻辑，并**新增** tab↔DB 映射表。
//!
//! ## 本层是「4 类 tab 口径」的唯一拥有者（★ 本次重构的核心）
//! 两张表必须由**同一个**映射定义驱动，否则角标与列表必然对不上（这正是旧实现的
//! bug：角标 198、列表 126）：
//!
//! | 前端 tab 值 | DB 状态集（过滤谓词） | 归桶（counts） |
//! |---|---|---|
//! | `pendingProduction` | `['PENDING']` | `PENDING` → `pendingProduction` |
//! | `inProduction` | `['IN_PROCESS']` | `IN_PROCESS` → `inProduction` |
//! | `pendingInspection` | `['INSPECTION']` | `INSPECTION` → `pendingInspection` |
//! | `delivered` | `['READY_TO_SHIP','DELIVERED']` | `READY_TO_SHIP` / `DELIVERED` → `delivered` |
//!
//! `['PENDING']` / `['IN_PROCESS']` / `['INSPECTION']` 三处各写两遍字面量
//! （`status_to_db_statuses` 的返回 + `map_counts_by_status` 的 match 臂），故
//! 两张表放在**同一个 `mod tests`** 里用一组断言同时钉死（见文件底部）。
//!
//! ## ⚠️ `REPAIRING` 不在表里（别加回来）
//! `REPAIRING` 已于 2026-10-01 降级为 `t_part_batch.is_repairing` 标记列
//! （migration 005/006），DB 层不再产生该 status，返修中的工单 status 就是
//! `IN_PROCESS` ⇒ 自动计入 `in_production`。把返修拆出去单列 tab 会让「生产中」
//! 少算正在返修的量。
//!
//! ## `hasMore` 算法（★ 不多打 count 查询）
//! 「**取 `size + 1` 条，看是否超出**」。前端两张页面都从未读取 `total`，所以响应
//! 里**没有** `total` 字段，也就没有为了算 `has_more` 而额外打一条
//! `SELECT COUNT(*)` 的理由。

use sqlx::PgConnection;

use crate::shared::error::AppError;

use super::dto::PartListQuery;
use super::model::PartListRow;
use super::repo::PartListRepo;
use super::vo::{
    BatchCard, PartCardOut, PartCountsOut, PartListHomeOut, PartListPageOut, WorkOrderCard,
};

/// 缺省每页条数（小程序首屏一次 10 张卡片，实测响应 ~1.5KB JSON）。
const DEFAULT_PAGE_SIZE: i64 = 10;

/// 每页条数上限（`clamp` 上界；防单个请求被拉成全表扫描）。
const MAX_PAGE_SIZE: i64 = 50;

/// `wx::part_list` service（ZST + 静态方法，与 `prod::process_design` 范本一致）。
pub struct PartListService;

impl PartListService {
    /// `GET /api/v2/wx/part-list` 业务逻辑：4 tab 角标 + 第 1 页卡片。
    ///
    /// ⚠️ `counts` 是**全局**口径（不带 `?status=` 过滤）：小程序 4 个 tab 的角标
    /// 是固定的，不会随当前选中的 tab 变。若把过滤套到 counts 上，切 tab 时角标会
    /// 集体塌成 0。
    pub async fn home(
        conn: &mut PgConnection,
        q: &PartListQuery,
    ) -> Result<PartListHomeOut, AppError> {
        // 角标：先算（恒不依赖 ?status=，故不会因非法 status 而单独失败）
        let raw = PartListRepo::counts_by_status(&mut *conn).await?;
        let counts = map_counts_by_status(raw);

        // 卡片：与 `/page` 端点共用同一条查询路径
        let (list, has_more) = list_cards(conn, q).await?;

        Ok(PartListHomeOut {
            counts,
            list,
            has_more,
        })
    }

    /// `GET /api/v2/wx/part-list/page` 业务逻辑：纯增量（**不含** counts）。
    pub async fn page(
        conn: &mut PgConnection,
        q: &PartListQuery,
    ) -> Result<PartListPageOut, AppError> {
        let (list, has_more) = list_cards(conn, q).await?;
        Ok(PartListPageOut { list, has_more })
    }
}

/// 两个端点共用的**唯一**列表查询路径（★ 结构上保证「?page=2 两端点结果一致」）。
///
/// 步骤：`status` 归一化 → 分页参数 clamp → 取 `size + 1` 条 → `has_more` 判定 →
/// 截断到 `size` → row→vo 投影。
async fn list_cards(
    conn: &mut PgConnection,
    q: &PartListQuery,
) -> Result<(Vec<PartCardOut>, bool), AppError> {
    // 1. `?status=` 白名单归一化（非法 → 40001 / HTTP 422）
    let statuses = status_to_db_statuses(q.status.as_deref())?;

    // 2. 分页参数：page 缺省 1 / max(1)；size 缺省 10 / clamp(1, 50)
    let page = q.page.unwrap_or(1).max(1);
    let size = q.size.unwrap_or(DEFAULT_PAGE_SIZE).clamp(1, MAX_PAGE_SIZE);
    let offset = (page - 1) * size;

    // 3. 取 `size + 1` 条：超出的那一条就是「还有下一页」的证据。
    //    ⚠️ 不要为了拿 has_more 再打一条 COUNT —— 前端不读 total。
    let rows = PartListRepo::list_parts(&mut *conn, statuses, size + 1, offset).await?;
    let has_more = rows.len() as i64 > size;

    // 4. 截断 + 投影
    let list = rows
        .into_iter()
        .take(size as usize)
        .map(row_to_card)
        .collect();

    Ok((list, has_more))
}

/// 前端 tab 值 → DB 状态集。
///
/// - `None` / `Some("all")` → `None`（不过滤）
/// - 4 个 tab 值 → 对应状态集（`delivered` 是**两个**状态）
/// - 其余（含 DB 原值 `PENDING` / 注入串 `; DROP TABLE t_part`）→ **40001 / HTTP 422**
///
/// ⚠️ 返回 `&'static [&'static str]`（不是 `Vec<String>`）：映射表是编译期常量，
/// 非法值**在拼进 SQL 之前**就被拒，SQL 侧不存在注入面（参数仍是绑定变量）。
pub(crate) fn status_to_db_statuses(
    tab: Option<&str>,
) -> Result<Option<&'static [&'static str]>, AppError> {
    match tab {
        None => Ok(None),
        Some("all") => Ok(None),
        Some("pendingProduction") => Ok(Some(&["PENDING"])),
        Some("inProduction") => Ok(Some(&["IN_PROCESS"])),
        Some("pendingInspection") => Ok(Some(&["INSPECTION"])),
        Some("delivered") => Ok(Some(&["READY_TO_SHIP", "DELIVERED"])),
        Some(other) => Err(AppError::validation(format!(
            "status {other:?} 不在白名单（all / pendingProduction / inProduction / \
             pendingInspection / delivered）"
        ))),
    }
}

/// `Vec<(DB status, count)>` → 4 个 tab 计数 + `all`（自旧 `map_counts_by_status`
/// 逐字搬来，唯一差异见下）。
///
/// ⚠️ **`delivered` 是两个状态**（`READY_TO_SHIP` + `DELIVERED`）：与
/// `status_to_db_statuses` 的 `delivered` 臂同源。本地库实测 72 + 126 = 198。
///
/// ⚠️ `REPAIRING` 已被 2026-10-01 的 migration 005/006 降级为
/// `t_part_batch.is_repairing` 标记列，`t_part.status` 不再产生该值 —— 返修中的
/// 工单自动由 `IN_PROCESS` 臂计入 `in_production`（**别加回 `REPAIRING` 分支**）。
///
/// `PROGRAMMING` / `OUTSOURCE` / `COMPLETED` / `CANCELLED` 只进 `all`（无对应 tab）。
fn map_counts_by_status(rows: Vec<(String, i64)>) -> PartCountsOut {
    let mut out = PartCountsOut {
        all: 0,
        pending_production: 0,
        in_production: 0,
        pending_inspection: 0,
        delivered: 0,
    };
    for (s, c) in rows {
        out.all += c;
        match s.as_str() {
            "PENDING" => out.pending_production += c,
            "IN_PROCESS" => out.in_production += c,
            "INSPECTION" => out.pending_inspection += c,
            "READY_TO_SHIP" | "DELIVERED" => out.delivered += c,
            // PROGRAMMING / OUTSOURCE / COMPLETED / CANCELLED 仅计入 all
            _ => {}
        }
    }
    out
}

/// DB 状态 → 前端 4 类 tab 值（**静默兜底**）。
///
/// ⚠️ `PROGRAMMING` / `OUTSOURCE` / `COMPLETED` / `CANCELLED` 以及任何未来新增的
/// 状态都落到 `"pendingProduction"` —— 与旧前端 `services/parts.ts::mapStatus` 的
/// catch-all 分支（`return 'pendingProduction'`）逐字对齐，避免小程序渲染行为突变。
///
/// ⚠️ **静默**是有登记的偏差（`docs/api/wx.md` §8）：这类工单只计入 `counts.all`，
/// 却不会出现在任何单个 tab 的列表里（列表按 DB 状态集过滤，它们不在任何集合内）。
/// 要彻底消除只能给前端加第 5 个 tab —— 那是产品决议，不是本轮的事。
fn display_status(db_status: &str) -> &'static str {
    match db_status {
        "PENDING" => "pendingProduction",
        "IN_PROCESS" => "inProduction",
        "INSPECTION" => "pendingInspection",
        "READY_TO_SHIP" | "DELIVERED" => "delivered",
        _ => "pendingProduction", // 静默兜底，见上方 doc
    }
}

/// DB 行 → 卡片 VO。
///
/// ⚠️ `kind` 判定口径**本次不改**：`assembly_id.is_some() → batch`，否则
/// `workOrder`。即装配件的子件按批次卡片呈现（沿自旧 `repo.rs::row_to_wx_part`）。
fn row_to_card(r: PartListRow) -> PartCardOut {
    // `planned_delivery_date` 是 NOT NULL 的 `date` 列；显式格式化（不靠 chrono
    // 的 serde 行为）以钉死 `YYYY-MM-DD` 三段式。
    let due_date = r.planned_delivery_date.format("%Y-%m-%d").to_string();

    if r.assembly_id.is_some() {
        PartCardOut::Batch(BatchCard {
            id: r.id,
            serial_no: r.serial_no,
            name: r.name,
            code: r.drawing_no,
            due_date,
            batch_no: r.current_batch_no,
            // ⚠️ 前端 `BatchPartCard.batchQty` 对应 `t_part.quantity`（工单总件数），
            // **不是**当前批次的 `t_part_batch.quantity` —— 与旧前端映射层一致。
            batch_qty: r.quantity,
        })
    } else {
        PartCardOut::WorkOrder(WorkOrderCard {
            id: r.id,
            serial_no: r.serial_no,
            name: r.name,
            code: r.drawing_no,
            due_date,
            customer: r.customer_name,
            delivered_qty: r.delivered_qty,
            total_qty: r.quantity,
            status: display_status(&r.status).to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // tab 白名单（非法值必须在拼 SQL 前被拒）
    // -----------------------------------------------------------------------

    #[test]
    fn status_absent_and_all_both_mean_no_filter() {
        assert_eq!(status_to_db_statuses(None).unwrap(), None);
        assert_eq!(status_to_db_statuses(Some("all")).unwrap(), None);
    }

    #[test]
    fn four_tab_values_map_to_expected_db_status_sets() {
        assert_eq!(
            status_to_db_statuses(Some("pendingProduction")).unwrap(),
            Some(&["PENDING"][..])
        );
        assert_eq!(
            status_to_db_statuses(Some("inProduction")).unwrap(),
            Some(&["IN_PROCESS"][..])
        );
        assert_eq!(
            status_to_db_statuses(Some("pendingInspection")).unwrap(),
            Some(&["INSPECTION"][..])
        );
        // ★ delivered 是**两个**状态（角标 198 vs 列表 126 的老 bug 就出在这里）
        assert_eq!(
            status_to_db_statuses(Some("delivered")).unwrap(),
            Some(&["READY_TO_SHIP", "DELIVERED"][..])
        );
    }

    #[test]
    fn non_whitelisted_status_is_validation_error() {
        // DB 原值不被接受（前端传的是 tab 值）
        for bad in ["PENDING", "IN_PROCESS", "READY_TO_SHIP", "DELIVERED"] {
            let e = status_to_db_statuses(Some(bad)).unwrap_err();
            assert_eq!(e.code(), 40001, "{bad} 应是 40001");
        }
        // 注入串
        let e = status_to_db_statuses(Some("; DROP TABLE t_part")).unwrap_err();
        assert_eq!(e.code(), 40001);
    }

    // -----------------------------------------------------------------------
    // counts 归桶（与上面的过滤表必须**逐条一致**）
    // -----------------------------------------------------------------------

    #[test]
    fn counts_buckets_match_the_filter_table() {
        let out = map_counts_by_status(vec![
            ("PENDING".into(), 7),
            ("IN_PROCESS".into(), 11),
            ("INSPECTION".into(), 13),
            ("READY_TO_SHIP".into(), 72),
            ("DELIVERED".into(), 126),
            // 无 tab 的 4 类：只进 all
            ("PROGRAMMING".into(), 17),
            ("OUTSOURCE".into(), 19),
            ("COMPLETED".into(), 23),
            ("CANCELLED".into(), 29),
        ]);

        assert_eq!(out.all, 7 + 11 + 13 + 72 + 126 + 17 + 19 + 23 + 29);
        assert_eq!(out.pending_production, 7);
        assert_eq!(out.in_production, 11);
        assert_eq!(out.pending_inspection, 13);
        // ★ 与 delivered 的过滤集合同口径：72 + 126
        assert_eq!(out.delivered, 198);

        // 交叉断言：每个 tab 的 counts 恰好等于「按该 tab 过滤后 rows 的条数」——
        // 这是本次修复的核心不变量（旧实现两边口径不同）。
        let raw = vec![
            ("PENDING".to_string(), 7i64),
            ("IN_PROCESS".to_string(), 11),
            ("INSPECTION".to_string(), 13),
            ("READY_TO_SHIP".to_string(), 72),
            ("DELIVERED".to_string(), 126),
            ("PROGRAMMING".to_string(), 17),
            ("OUTSOURCE".to_string(), 19),
            ("COMPLETED".to_string(), 23),
            ("CANCELLED".to_string(), 29),
        ];
        for tab in [
            "pendingProduction",
            "inProduction",
            "pendingInspection",
            "delivered",
        ] {
            let set = status_to_db_statuses(Some(tab))
                .unwrap()
                .expect("tab 有状态集");
            let filtered: i64 = raw
                .iter()
                .filter(|(s, _)| set.contains(&s.as_str()))
                .map(|(_, c)| *c)
                .sum();
            let bucket = match tab {
                "pendingProduction" => out.pending_production,
                "inProduction" => out.in_production,
                "pendingInspection" => out.pending_inspection,
                _ => out.delivered,
            };
            assert_eq!(
                bucket, filtered,
                "[{tab}] counts 桶与过滤集合必须同口径（否则角标与列表对不上）"
            );
        }
    }

    #[test]
    fn repairing_falls_into_in_production_bucket() {
        // 2026-10-01 起 DB 不再产生 REPAIRING；返修工单 status 就是 IN_PROCESS。
        // 这里显式断言「若真出现 REPAIRING 字面，也只进 all」——它**不是**新分支。
        let out = map_counts_by_status(vec![("IN_PROCESS".into(), 5), ("REPAIRING".into(), 3)]);
        assert_eq!(
            out.in_production, 5,
            "REPAIRING 不得单列桶（已降级为标记列）"
        );
        assert_eq!(out.all, 8);
    }

    #[test]
    fn empty_counts_are_all_zero() {
        let out = map_counts_by_status(Vec::new());
        assert_eq!(out.all, 0);
        assert_eq!(out.pending_production, 0);
        assert_eq!(out.in_production, 0);
        assert_eq!(out.pending_inspection, 0);
        assert_eq!(out.delivered, 0);
    }

    // -----------------------------------------------------------------------
    // DB 状态 → 前端 tab 值（静默兜底）
    // -----------------------------------------------------------------------

    #[test]
    fn display_status_maps_four_known_states() {
        assert_eq!(display_status("PENDING"), "pendingProduction");
        assert_eq!(display_status("IN_PROCESS"), "inProduction");
        assert_eq!(display_status("INSPECTION"), "pendingInspection");
        assert_eq!(display_status("READY_TO_SHIP"), "delivered");
        assert_eq!(display_status("DELIVERED"), "delivered");
    }

    #[test]
    fn display_status_silently_falls_back_to_pending_production() {
        // 与旧前端 mapStatus 的 catch-all 逐字对齐
        for db in [
            "PROGRAMMING",
            "OUTSOURCE",
            "COMPLETED",
            "CANCELLED",
            "GARBAGE",
        ] {
            assert_eq!(
                display_status(db),
                "pendingProduction",
                "{db} 应静默兜底成 pendingProduction"
            );
        }
    }

    // -----------------------------------------------------------------------
    // row → vo 投影
    // -----------------------------------------------------------------------

    fn row(assembly_id: Option<i64>) -> PartListRow {
        PartListRow {
            id: 1,
            serial_no: Some("F1".into()),
            name: "n".into(),
            drawing_no: "D-1".into(),
            quantity: 8,
            status: "IN_PROCESS".into(),
            planned_delivery_date: chrono::NaiveDate::from_ymd_opt(2026, 8, 4).unwrap(),
            customer_name: Some("六厂".into()),
            assembly_id,
            current_batch_id: Some(99),
            current_batch_no: Some(2),
            delivered_qty: 3,
        }
    }

    #[test]
    fn assembly_child_row_becomes_batch_card() {
        let v = serde_json::to_value(row_to_card(row(Some(555)))).unwrap();
        assert_eq!(v["kind"], "batch");
        assert_eq!(v["batchNo"], 2);
        assert_eq!(v["batchQty"], 8, "batchQty 是 t_part.quantity，不是批次量");
        assert_eq!(v["dueDate"], "2026-08-04");
        // current_batch_id 不进 VO
        assert!(!v.as_object().unwrap().contains_key("current_batch_id"));
    }

    #[test]
    fn standalone_row_becomes_work_order_card() {
        let v = serde_json::to_value(row_to_card(row(None))).unwrap();
        assert_eq!(v["kind"], "workOrder");
        assert_eq!(v["status"], "inProduction");
        assert_eq!(v["deliveredQty"], 3);
        assert_eq!(v["totalQty"], 8);
        assert_eq!(v["customer"], "六厂");
        assert_eq!(v["code"], "D-1");
        assert!(!v.as_object().unwrap().contains_key("current_batch_id"));
    }
}

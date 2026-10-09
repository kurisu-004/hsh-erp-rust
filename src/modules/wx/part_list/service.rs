//! wx::part_list 子模块 service 层 —— 业务逻辑（ZST + 静态方法）
//!
//! 2026-10-11 新增。承接旧 `src/modules/wx/repo.rs::map_counts_by_status` 与
//! `parts.rs::list` 的归桶 / 校验逻辑，并**新增** tab↔DB 映射表。
//!
//! 2026-10-12：**接上日期筛选**（`?date=YYYY-MM-DD`）。此前小程序 `date-nav-bar`
//! 的日期是**纯装饰**的 —— `selectedDate` 既不进 `queryKey` 也不进 `queryFn`。
//! 日期谓词打 `p.system_delivery_date`（**不是** `planned_delivery_date`）。
//!
//! ## 本层是「7 类 tab 口径」的唯一拥有者（★ 本次重构的核心）
//! 两张表必须由**同一个**映射定义驱动，否则角标与列表必然对不上（这正是旧实现的
//! bug：角标 198、列表 126）：
//!
//! | 前端 tab 值 | DB 状态集（过滤谓词） | 日期谓词 | 归桶（counts） |
//! |---|---|---|---|
//! | `all` / 缺省 | 6 状态白名单 | `= $date` | 6 状态之和 |
//! | `pendingProduction` | `['PENDING']` | `= $date` | `PENDING` |
//! | `inProduction` | `['IN_PROCESS']` | `= $date` | `IN_PROCESS` |
//! | `outsource` | `['OUTSOURCE']` | `= $date` | `OUTSOURCE` |
//! | `inspecting` | `['INSPECTION','READY_TO_SHIP']` | `= $date` | `INSPECTION` + `READY_TO_SHIP` |
//! | `delivered` | `['DELIVERED']` | `= $date` | `DELIVERED` |
//! | `noSystemDate` | 6 状态白名单 | `IS NULL`（**忽略** `$date`） | `is_null = true` 的行 |
//!
//! `['PENDING']` / `['IN_PROCESS']` / `['OUTSOURCE']` / `['INSPECTION', …]` 四处
//! 各写两遍字面量（`status_to_db_statuses` 的返回 + `map_counts_by_status` 的
//! match 臂），故两张表放在**同一个 `mod tests`** 里用一组交叉断言同时钉死
//! （见文件底部）。
//!
//! ## ★ `all` / 缺省**不再是「不过滤」**（2026-10-12 语义变更）
//! 旧实现 `None` / `all` → `Option::None`（SQL 里 `$1::text[] IS NULL` ⇒ 无谓词），
//! 于是 `PROGRAMMING`（CNC 编程）/ `COMPLETED` / `CANCELLED` 会漏进列表和角标。
//! 现在一律落到 6 状态白名单。⚠️ 这**排除**上述 3 个状态（产品已明确确认）。
//! 「在零件一览页看到已完成的工单」本身就不是用户要的语义。
//!
//! ## ★ counts 不变量
//! `all == pendingProduction + inProduction + outsource + inspecting + delivered`
//! 成立的前提是 6 个 DB 状态与 5 个 dated tab **严格一一归属、无重叠无遗漏**。
//! `noSystemDate` 是第 7 个桶、是「NULL 日期」的横切口径，**不进**这条等式。
//! lib 单测 `counts_buckets_match_the_filter_table` + 集成测试
//! `counts_all_equals_sum_of_five_dated_tabs` 共同钉死。
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

use chrono::NaiveDate;
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

/// ★ 「一览页可见」的 6 状态白名单（2026-10-12 新增，原先是「不过滤」）。
///
/// `all` / 缺省 / `noSystemDate` 三个 tab 共用它；其余 5 个 tab 是它的子集。
/// **不含** `PROGRAMMING` / `COMPLETED` / `CANCELLED` —— 这三个状态对「零件
/// 一览」页没有意义（CNC 编程走 `prod::cnc_program` 页、完结 / 取消工单不该在
/// 生产待办里占位）。产品已明确确认本次排除。
pub(crate) const LISTED_STATUSES: &[&str] = &[
    "PENDING",
    "IN_PROCESS",
    "OUTSOURCE",
    "INSPECTION",
    "READY_TO_SHIP",
    "DELIVERED",
];

/// tab 值归一化后的筛选条件：**状态集 + 是否忽略日期**。
///
/// 为什么不是裸的 `Option<&[&str]>`：`noSystemDate` tab 的状态集与 `all` 完全相同
/// （都是 6 状态白名单），两者**只差日期谓词**。把「是否忽略日期」硬塞进状态集里
/// （比如塞个哨兵值）会让 SQL 片段无从选择；做成结构体后，「状态集」与「日期口径」
/// 两个维度各自独立可断言。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PartFilter {
    /// DB 状态集（**永不为空**：2026-10-12 起没有「不过滤」这条路径）
    pub statuses: &'static [&'static str],
    /// `true` = 用 `system_delivery_date IS NULL` 谓词并**忽略** `?date=`
    /// （`noSystemDate` tab）
    pub ignore_date: bool,
}

impl PartFilter {
    /// 走 `system_delivery_date = ?date` 谓词（其余 6 个 tab 共用）。
    const fn dated(statuses: &'static [&'static str]) -> Self {
        Self {
            statuses,
            ignore_date: false,
        }
    }

    /// 走 `system_delivery_date IS NULL` 谓词，忽略 `?date`。
    const fn undated() -> Self {
        Self {
            statuses: LISTED_STATUSES,
            ignore_date: true,
        }
    }
}

/// `wx::part_list` service（ZST + 静态方法，与 `prod::process_design` 范本一致）。
pub struct PartListService;

impl PartListService {
    /// `GET /api/v2/wx/part-list` 业务逻辑：7 tab 角标 + 第 1 页卡片。
    ///
    /// ⚠️ `counts` **不随 `?status=` 变**（小程序 7 个 tab 的角标是固定的，切 tab
    /// 不会让角标集体塌成 0），但**随 `?date=` 变** —— 日期导航条一切，7 个数字
    /// 整体换一批，那是日期作用域而非 tab 作用域。详见 `docs/api/wx.md` §8.5。
    pub async fn home(
        conn: &mut PgConnection,
        q: &PartListQuery,
    ) -> Result<PartListHomeOut, AppError> {
        // 角标恒按 6 状态白名单统计（不受 ?status= 影响），但带 ?date= 作用域。
        // 先算（恒不依赖 ?status=，故不会因非法 status 而单独失败）
        let raw = PartListRepo::counts_by_status(&mut *conn, LISTED_STATUSES, q.date).await?;
        // noSystemDate 桶与日期无关，走独立的标量查询（见 repo.rs 的取舍说明）
        let no_system_date = PartListRepo::count_null_date(&mut *conn, LISTED_STATUSES).await?;
        let counts = map_counts_by_status(raw, q.date, no_system_date);

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
    let filter = status_to_db_statuses(q.status.as_deref())?;

    // 2. 分页参数：page 缺省 1 / max(1)；size 缺省 10 / clamp(1, 50)
    let page = q.page.unwrap_or(1).max(1);
    let size = q.size.unwrap_or(DEFAULT_PAGE_SIZE).clamp(1, MAX_PAGE_SIZE);
    let offset = (page - 1) * size;

    // 3. 取 `size + 1` 条：超出的那一条就是「还有下一页」的证据。
    //    ⚠️ 不要为了拿 has_more 再打一条 COUNT —— 前端不读 total。
    let rows = PartListRepo::list_parts(
        &mut *conn,
        filter.statuses,
        q.date,
        filter.ignore_date,
        size + 1,
        offset,
    )
    .await?;
    let has_more = rows.len() as i64 > size;

    // 4. 截断 + 投影
    let list = rows
        .into_iter()
        .take(size as usize)
        .map(row_to_card)
        .collect();

    Ok((list, has_more))
}

/// 前端 tab 值 → DB 状态集 + 是否忽略日期。
///
/// - `None` / `Some("all")` → 6 状态白名单（⚠️ **不再是「不过滤」**，见模块 doc）
/// - `noSystemDate` → 同一个 6 状态白名单，但 `ignore_date = true`
/// - 其余 5 个 tab → 对应状态集（`inspecting` 是**两个**状态）
/// - 其余（含 DB 原值 `PENDING` / 注入串 `; DROP TABLE t_part`）→ **40001 / HTTP 422**
///
/// ⚠️ 非法值**在拼进 SQL 之前**就被拒（映射表是编译期常量，SQL 侧参数仍走 bind）
/// ⇒ 注入面为 0。
pub(crate) fn status_to_db_statuses(tab: Option<&str>) -> Result<PartFilter, AppError> {
    match tab {
        None | Some("all") => Ok(PartFilter::dated(LISTED_STATUSES)),
        Some("pendingProduction") => Ok(PartFilter::dated(&["PENDING"])),
        Some("inProduction") => Ok(PartFilter::dated(&["IN_PROCESS"])),
        Some("outsource") => Ok(PartFilter::dated(&["OUTSOURCE"])),
        Some("inspecting") => Ok(PartFilter::dated(&["INSPECTION", "READY_TO_SHIP"])),
        Some("delivered") => Ok(PartFilter::dated(&["DELIVERED"])),
        Some("noSystemDate") => Ok(PartFilter::undated()),
        Some(other) => Err(AppError::validation(format!(
            "status {other:?} 不在白名单（all / pendingProduction / inProduction / \
             outsource / inspecting / delivered / noSystemDate）"
        ))),
    }
}

/// repo 的 `(status, is_null, cnt)` 分组行 → 7 个 tab 计数。
///
/// - `rows`：[`PartListRepo::counts_by_status`] 的返回值（含 `is_null` 标记）
/// - `date`：仅用于判断「`= $date` 有没有作用域」
/// - `no_system_date`：[`PartListRepo::count_null_date`] 的标量结果（与日期无关）
///
/// ⚠️ **`is_null` 行的归桶取决于 `date`**：
/// - `date = Some(_)`：repo 的 SQL 已用 `system_delivery_date = $2` 把 NULL 行滤掉，
///   `is_null = true` 的行**一条都不会出现**（下面的 `continue` 是纯防御）
/// - `date = None`：「`= $date`」退化为无谓词，NULL 行同样是「当日全部」的一部分 ⇒
///   进 dated 桶。**这是刻意的**：否则不传 `?date` 时 `counts.all` 会比
///   `?status=all` 列表的行数少一截，正是 §3.3 记的那个「角标 ≠ 列表」的裂缝。
///
/// ⚠️ `no_system_date` **恒取标量查询的结果**，不从 `rows` 里累加：那两条查询的
/// 日期口径不同（标量恒 `IS NULL`；分组查询在 `?date` 有值时压根看不到 NULL 行），
/// 混用会随 `?date` 变。
///
/// ⚠️ `PROGRAMMING` / `COMPLETED` / `CANCELLED` / `REPAIRING` 等：白名单外，SQL 层
/// 就查不到；真出现（将来新增状态）也只走 `_ => {}`，不进任何桶。
fn map_counts_by_status(
    rows: Vec<(String, bool, i64)>,
    date: Option<NaiveDate>,
    no_system_date: i64,
) -> PartCountsOut {
    let mut out = PartCountsOut {
        all: 0,
        pending_production: 0,
        in_production: 0,
        outsource: 0,
        inspecting: 0,
        delivered: 0,
        no_system_date,
    };
    // 「= $date」无作用域（?date 缺省）时，is_null 行也算「当日全部」
    let dated_scope_is_unbounded = date.is_none();

    for (s, is_null, c) in rows {
        if is_null && !dated_scope_is_unbounded {
            continue;
        }
        out.all += c;
        match s.as_str() {
            "PENDING" => out.pending_production += c,
            "IN_PROCESS" => out.in_production += c,
            "OUTSOURCE" => out.outsource += c,
            // ★ inspecting 是**两个**状态（品检中 + 待发运）
            "INSPECTION" | "READY_TO_SHIP" => out.inspecting += c,
            "DELIVERED" => out.delivered += c,
            // 白名单外的状态（SQL 层已挡掉，这里是纯防御）
            _ => {}
        }
    }
    out
}

/// DB 状态 → 前端 tab 值。
///
/// ⚠️ `inspecting` 是**两个** DB 状态（`INSPECTION` + `READY_TO_SHIP`）——
/// 与 [`status_to_db_statuses`] 的 `inspecting` 臂同源。
///
/// ⚠️ catch-all 仍返 `"pendingProduction"`，与旧前端 `services/parts.ts::mapStatus`
/// 的 catch-all 分支（`return 'pendingProduction'`）逐字对齐。**但 2026-10-12 起它
/// 已不可达**：过滤谓词就是 6 状态白名单，`PROGRAMMING` / `COMPLETED` /
/// `CANCELLED`（以及任何未来新增状态）根本进不了列表。留着只作纯防御 ——
/// 「将来 DB 新增状态时卡片不至于渲染成空白」。
fn display_status(db_status: &str) -> &'static str {
    match db_status {
        "PENDING" => "pendingProduction",
        "IN_PROCESS" => "inProduction",
        "OUTSOURCE" => "outsource",
        "INSPECTION" | "READY_TO_SHIP" => "inspecting",
        "DELIVERED" => "delivered",
        // 纯防御：6 状态白名单后已不可达，见上方 doc
        _ => "pendingProduction",
    }
}

/// DB 行 → 卡片 VO。
///
/// ⚠️ `kind` 判定口径**本次不改**：`assembly_id.is_some() → batch`，否则
/// `workOrder`。即装配件的子件按批次卡片呈现（沿自旧 `repo.rs::row_to_wx_part`）。
fn row_to_card(r: PartListRow) -> PartCardOut {
    // `system_delivery_date` 可空（无交期工单）：显式格式化（不靠 chrono 的
    // serde 行为）以钉死 `YYYY-MM-DD` 三段式，NULL 保持 None → JSON `null`。
    let due_date = r
        .system_delivery_date
        .map(|d| d.format("%Y-%m-%d").to_string());

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

    /// ⚠️ 2026-10-12 **语义变更**：缺省 / `all` 从「不过滤」变成 6 状态白名单。
    #[test]
    fn status_absent_and_all_both_mean_the_six_status_whitelist() {
        for tab in [None, Some("all")] {
            let f = status_to_db_statuses(tab).unwrap();
            assert_eq!(f.statuses, LISTED_STATUSES, "[{tab:?}] 应是 6 状态白名单");
            assert!(!f.ignore_date, "[{tab:?}] 应走 `= $date` 谓词");
        }
        // ★ 白名单恰是 6 个，且**不含** PROGRAMMING / COMPLETED / CANCELLED
        assert_eq!(LISTED_STATUSES.len(), 6);
        for banned in ["PROGRAMMING", "COMPLETED", "CANCELLED", "REPAIRING"] {
            assert!(
                !LISTED_STATUSES.contains(&banned),
                "{banned} 不该在一览页白名单里"
            );
        }
    }

    #[test]
    fn tab_values_map_to_expected_db_status_sets() {
        for (tab, want) in [
            ("pendingProduction", vec!["PENDING"]),
            ("inProduction", vec!["IN_PROCESS"]),
            ("outsource", vec!["OUTSOURCE"]),
            ("inspecting", vec!["INSPECTION", "READY_TO_SHIP"]),
            ("delivered", vec!["DELIVERED"]),
        ] {
            let f = status_to_db_statuses(Some(tab)).unwrap();
            assert_eq!(f.statuses, want.as_slice(), "[{tab}] 状态集不对");
            assert!(!f.ignore_date, "[{tab}] 应走 `= $date` 谓词");
        }
    }

    /// `noSystemDate`：状态集与 `all` **完全相同**，只差 `ignore_date`。
    #[test]
    fn no_system_date_shares_the_status_set_with_all_but_ignores_date() {
        let all = status_to_db_statuses(Some("all")).unwrap();
        let ns = status_to_db_statuses(Some("noSystemDate")).unwrap();
        assert_eq!(ns.statuses, all.statuses, "两个 tab 的状态集必须一致");
        assert!(!all.ignore_date);
        assert!(ns.ignore_date, "noSystemDate 必须忽略 ?date");
    }

    #[test]
    fn non_whitelisted_status_is_validation_error() {
        // DB 原值不被接受（前端传的是 tab 值）
        for bad in [
            "PENDING",
            "IN_PROCESS",
            "OUTSOURCE",
            "INSPECTION",
            "READY_TO_SHIP",
            "DELIVERED",
            // 旧 tab 值已随本次改版作废（品检 tab 更名 + 新增外协 tab）
            "pendingInspection",
            "",
        ] {
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

    fn d(status: &str, cnt: i64) -> (String, bool, i64) {
        (status.to_string(), false, cnt)
    }

    fn n(status: &str, cnt: i64) -> (String, bool, i64) {
        (status.to_string(), true, cnt)
    }

    fn date() -> Option<NaiveDate> {
        Some(NaiveDate::from_ymd_opt(2026, 8, 4).unwrap())
    }

    /// 核心不变量 + 每个 tab 的 counts 桶 == 按该 tab 状态集过滤后的行数。
    #[test]
    fn counts_buckets_match_the_filter_table() {
        let out = map_counts_by_status(
            vec![
                d("PENDING", 7),
                d("IN_PROCESS", 11),
                d("OUTSOURCE", 19),
                d("INSPECTION", 13),
                d("READY_TO_SHIP", 72),
                d("DELIVERED", 126),
            ],
            date(),
            42,
        );

        assert_eq!(out.pending_production, 7);
        assert_eq!(out.in_production, 11);
        assert_eq!(out.outsource, 19);
        // ★ inspecting 是两个状态：13 + 72
        assert_eq!(out.inspecting, 85);
        assert_eq!(out.delivered, 126);
        assert_eq!(out.all, 7 + 11 + 19 + 13 + 72 + 126);

        // ★★ 本次新增的不变量（必须钉死）
        assert_eq!(
            out.all,
            out.pending_production
                + out.in_production
                + out.outsource
                + out.inspecting
                + out.delivered,
            "all 必须恒等于 5 个 dated tab 之和（6 状态与 5 个 tab 严格一一归属）"
        );
        // noSystemDate 是第 7 桶，**不进**上面那条等式
        assert_eq!(out.no_system_date, 42);

        // 交叉断言：每个 dated tab 的 counts 桶 == 「按该 tab 状态集过滤后的行数」
        let raw = [
            d("PENDING", 7),
            d("IN_PROCESS", 11),
            d("OUTSOURCE", 19),
            d("INSPECTION", 13),
            d("READY_TO_SHIP", 72),
            d("DELIVERED", 126),
        ];
        for tab in [
            "pendingProduction",
            "inProduction",
            "outsource",
            "inspecting",
            "delivered",
        ] {
            let set = status_to_db_statuses(Some(tab)).unwrap().statuses;
            let filtered: i64 = raw
                .iter()
                .filter(|(s, _, _)| set.contains(&s.as_str()))
                .map(|(_, _, c)| *c)
                .sum();
            let bucket = match tab {
                "pendingProduction" => out.pending_production,
                "inProduction" => out.in_production,
                "outsource" => out.outsource,
                "inspecting" => out.inspecting,
                _ => out.delivered,
            };
            assert_eq!(
                bucket, filtered,
                "[{tab}] counts 桶与过滤集合必须同口径（否则角标与列表对不上）"
            );
        }
    }

    /// ★ `?date` 有值时，`is_null` 行不计入 dated 桶（SQL 层已滤掉，这里钉死
    /// 防御分支）；`?date` 缺省时它们计入 —— 否则「不传 date」会出现
    /// `counts.all` < `?status=all` 列表行数。
    #[test]
    fn null_date_rows_join_dated_buckets_only_when_date_is_absent() {
        let rows = vec![d("PENDING", 7), n("PENDING", 5), n("DELIVERED", 3)];

        // ?date 有值 → 只有 = $date 的行（SQL 侧已滤，这里模拟其输出）
        let bounded = map_counts_by_status(rows.clone(), date(), 8);
        assert_eq!(bounded.all, 7);
        assert_eq!(bounded.pending_production, 7);
        assert_eq!(
            bounded.no_system_date, 8,
            "noSystemDate 恒取标量、不随 date 变"
        );

        // ?date 缺省 → 「= $date」退化为无谓词，NULL 行也是「全部」的一部分
        let unbounded = map_counts_by_status(rows, None, 8);
        assert_eq!(unbounded.all, 15, "不传 date 时 all 必须含 NULL 日期行");
        assert_eq!(unbounded.pending_production, 12);
        assert_eq!(unbounded.delivered, 3);
        assert_eq!(unbounded.no_system_date, 8);
    }

    #[test]
    fn repairing_falls_into_in_production_bucket() {
        // 2026-10-01 起 DB 不再产生 REPAIRING；返修工单 status 就是 IN_PROCESS。
        // 这里显式断言「若真出现 REPAIRING 字面，也只进 all」——它**不是**新分支。
        let out = map_counts_by_status(vec![d("IN_PROCESS", 5), d("REPAIRING", 3)], date(), 0);
        assert_eq!(
            out.in_production, 5,
            "REPAIRING 不得单列桶（已降级为标记列）"
        );
        assert_eq!(out.all, 8);
    }

    #[test]
    fn empty_counts_are_all_zero() {
        let out = map_counts_by_status(Vec::new(), date(), 0);
        assert_eq!(out.all, 0);
        assert_eq!(out.pending_production, 0);
        assert_eq!(out.in_production, 0);
        assert_eq!(out.outsource, 0);
        assert_eq!(out.inspecting, 0);
        assert_eq!(out.delivered, 0);
        assert_eq!(out.no_system_date, 0);
    }

    // -----------------------------------------------------------------------
    // DB 状态 → 前端 tab 值
    // -----------------------------------------------------------------------

    #[test]
    fn display_status_maps_the_six_whitelisted_states() {
        assert_eq!(display_status("PENDING"), "pendingProduction");
        assert_eq!(display_status("IN_PROCESS"), "inProduction");
        assert_eq!(display_status("OUTSOURCE"), "outsource");
        // ★ inspecting 是两个状态
        assert_eq!(display_status("INSPECTION"), "inspecting");
        assert_eq!(display_status("READY_TO_SHIP"), "inspecting");
        assert_eq!(display_status("DELIVERED"), "delivered");
    }

    #[test]
    fn display_status_falls_back_to_pending_production_for_unlisted_states() {
        // 与旧前端 mapStatus 的 catch-all 逐字对齐；6 状态白名单后已不可达，纯防御
        for db in [
            "PROGRAMMING",
            "COMPLETED",
            "CANCELLED",
            "REPAIRING",
            "GARBAGE",
        ] {
            assert_eq!(
                display_status(db),
                "pendingProduction",
                "{db} 应兜底成 pendingProduction"
            );
        }
    }

    /// `display_status` 必须是 [`status_to_db_statuses`] 的**逆映射**：
    /// 每个 tab 的状态集里每一项折出来的值都 == tab 名。否则卡片上的 `status`
    /// 会与用户点的 tab 不符。
    #[test]
    fn display_status_is_inverse_of_the_filter_table() {
        for tab in [
            "pendingProduction",
            "inProduction",
            "outsource",
            "inspecting",
            "delivered",
        ] {
            let set = status_to_db_statuses(Some(tab)).unwrap().statuses;
            for db in set {
                assert_eq!(
                    display_status(db),
                    tab,
                    "[{tab}] 状态 {db} 折叠后必须 == tab 名（否则卡片与 tab 不符）"
                );
            }
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
            system_delivery_date: NaiveDate::from_ymd_opt(2026, 8, 4),
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

    /// `system_delivery_date IS NULL` ⇒ `dueDate` 是 JSON `null`（不是空串、
    /// 不是缺键）。`noSystemDate` tab 的每一行都会走到这里。
    #[test]
    fn null_system_delivery_date_projects_to_json_null_due_date() {
        let mut r = row(None);
        r.system_delivery_date = None;
        let v = serde_json::to_value(row_to_card(r)).unwrap();
        assert_eq!(v["dueDate"], serde_json::Value::Null);
        assert!(
            v.as_object().unwrap().contains_key("dueDate"),
            "键必须在位（值 null）"
        );

        let mut r = row(Some(555));
        r.system_delivery_date = None;
        let v = serde_json::to_value(row_to_card(r)).unwrap();
        assert_eq!(v["dueDate"], serde_json::Value::Null);
    }
}

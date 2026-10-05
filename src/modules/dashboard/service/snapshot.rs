//! dashboard 域 snapshot 装配 service（2026-09-22 Group E 重构）
//!
//! 对应 Python myERP/service/dashboard.py 的 `build_snapshot_with_workers`：
//! 大屏实时推送的完整快照，包含生产货架分组 + 品检区扁平 + 工人持有件 +
//! 未来 N 天交付分桶（N 来自 `?upcoming_days=` / WS 路径默认 14；2026-09-30 同步）。
//!
//! ## 设计要点（2026-09-22 Group E 重构后）
//! - 4 次 trait call 拉全量数据（`snapshot_counters` / `snapshot_top_parts` /
//!   `snapshot_recent_batches` / `snapshot_workers`），service 仅做装配 +
//!   DTO 转换（`part_to_item` / 货架分组 / 时间格式化）
//! - `repo/sql.rs::DashboardRepo` ZST 提供 4 个聚合静态方法，每个方法内部已做完
//!   「多表 JOIN + 防 N+1」（批量查名字、批量查客户路径、批量查 PICKED_UP 时间）
//! - `top_n` 默认 1000：远高于合理在持量，仅作防爆兜底
//! - 形参 `days` / `basis` 语义见 `build_snapshot_with_workers` 函数 doc +
//!   `dto.rs::DeliveryBasis`（A.4 核实：两天形参与本文件同日 PR 8325350 / 9534e88 引入，
//!   时间戳保留指向权威点）
//! - service 不持 repo / pool——handler 借 `&mut *tx` 喂给 trait 即可
//!
//! ## 事务分层
//! 事务移交 handler：service 方法 `<R: DashboardRepoTrait>(&self, mut repo: R, ...)`
//! by-value。生产 `R = &mut PgConnection`，handler `pool.begin()` + `tx.commit()`
//! 包外。dashboard 两个端点均为 handler 三形态 ①（snapshot 拉一次即结束）。
//!
//! ## 数据结构
//! `DashboardSnapshot` / `OnProductionShelfGroup` / `DashboardItem` /
//! `UpcomingDeliveryBucket` 在 `dto.rs`（2026-09-22 从 service.rs 平移）。

use std::collections::HashMap;

use crate::modules::dashboard::dto::DeliveryBasis;
use crate::modules::dashboard::repo::{BatchLite, DashboardRepoTrait, PartLite};
use crate::modules::dashboard::vo::{DashboardItem, DashboardSnapshot, OnProductionShelfGroup};
use crate::shared::analytics::shelf_grouping::group_by_shelf;

/// 快照 top_n 默认值（远高于合理在持量，仅作防爆兜底）
pub const DASHBOARD_TOP_N: i64 = 1000;

/// 未来交付分桶默认天数（2026-09-30 新增，对齐前端 dashboard 视图横轴默认宽度）
pub const DASHBOARD_DEFAULT_DAYS: i64 = 14;

/// 未来交付分桶最大天数（防御恶意大数 / 拼写错把日期塞成 10000）
pub const DASHBOARD_MAX_DAYS: i64 = 60;
/// 未来交付分桶最小天数（防御 0 / 负数 / 拼写错）
pub const DASHBOARD_MIN_DAYS: i64 = 1;

/// 大屏 snapshot 装配 service（2026-09-22 Group E 重构）
///
/// unit struct（无字段依赖；事务移交 handler；handler 借连接传入 repo）。
/// 直接 `Arc<DashboardService>` 存 `AppState`；方法签名收 `mut repo: R`
///（by-value；生产 `R = &mut PgConnection`，单测 `R = MockDashboardRepoTrait`），
/// 单测用 `MockDashboardRepoTrait` 直接注入。
pub struct DashboardService;

impl Default for DashboardService {
    fn default() -> Self {
        Self
    }
}

impl DashboardService {
    /// 构造（空 struct，无字段；保留供未来切到 `Arc<DashboardService>` 时使用）。
    /// 当前 AppState 装线直接 `Arc::new(DashboardService)` 也行（unit struct 无字段）。
    pub fn new() -> Self {
        Self
    }

    /// 异步构建一次完整快照。
    ///
    /// 返回 `{on_production_shelves, on_inspection_shelves, in_process,
    ///         upcoming_delivery, ts}`，JSON shape 与 v1 Python 一致。
    ///
    /// 流程：4 次 trait call 拉全量数据 → 装配成 4 个 DTO 子结构 → 包装进
    /// `DashboardSnapshot`。service 内零 SQL——所有 SQL 在 `repo/sql.rs`。
    ///
    /// 形参（2026-09-30 新增 days 形参）：
    /// - `top_n`：产线 + 工人持有 IN_PROCESS 批次每 holder 的 top-N 截流；None →
    ///   `DASHBOARD_TOP_N`（1000，防爆兜底）
    /// - `days`：未来 N 天交付分桶天数；None → `DASHBOARD_DEFAULT_DAYS`（14，与
    ///   前端 dashboard 视图横轴默认宽度对齐）；clamp(1, 60) 防御恶意大数 /
    ///   拼写错（0 / 负数 / 巨大日期）
    /// - `basis`（2026-10-04 新增）：`upcoming_delivery[]` 分桶的交期口径；None →
    ///   `DeliveryBasis::Planned`（计划交期）。默认值在此 `unwrap_or_default()`
    ///   收敛成唯一决策点，repo 层收确定值不做兜底。
    pub async fn build_snapshot_with_workers<R: DashboardRepoTrait>(
        &self,
        mut repo: R,
        top_n: Option<i64>,
        days: Option<i64>,
        basis: Option<DeliveryBasis>,
    ) -> Result<DashboardSnapshot, sqlx::Error> {
        let top_n = top_n.unwrap_or(DASHBOARD_TOP_N);
        // 2026-09-30 新增：原写死 7 改为 query-driven；None → 14 默认值
        // （沿前端 dashboard 视图横轴默认宽度）；clamp(1, 60) 防御恶意大数 / 拼写错。
        let days = days
            .unwrap_or(DASHBOARD_DEFAULT_DAYS)
            .clamp(DASHBOARD_MIN_DAYS, DASHBOARD_MAX_DAYS);
        // 2026-10-04 新增：口径默认值在此收口；WS 路径恒传 None → Planned。
        let basis = basis.unwrap_or_default();

        // 1) 4 次聚合 trait call
        let upcoming = repo.snapshot_counters(days, basis).await?;
        let top = repo.snapshot_top_parts(top_n).await?;
        let recent = repo.snapshot_recent_batches(top_n).await?;

        // 2) 工人 id 列表（用于 worker name 查表）
        let worker_ids: Vec<i64> = recent
            .worker_pairs
            .iter()
            .filter_map(|(b, _)| b.holder_id)
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();
        let worker_name_map = repo.snapshot_workers(&worker_ids).await?;

        // 3) 产线架按 current_holder_id 分桶限流（PR3 抽离：纯聚合走 shared::analytics）
        let rows_by_shelf = group_by_shelf(top.on_prod_pairs, |b: &BatchLite| b.holder_id);
        let prod_groups: Vec<OnProductionShelfGroup> = top
            .active_prod_shelves
            .into_iter()
            .map(|(shelf_id, shelf_code, shelf_name)| {
                let shelf_rows = rows_by_shelf.get(&shelf_id).cloned().unwrap_or_default();
                let total_count = shelf_rows.len();
                let items: Vec<DashboardItem> = shelf_rows
                    .into_iter()
                    .take(10)
                    .map(|(b, p)| {
                        let mut item = part_to_item(&p, &b, &top.cust_map, &top.process_map);
                        item.shelf_code = Some(shelf_code.clone());
                        item.current_holder_kind = Some("shelf".into());
                        item
                    })
                    .collect();
                OnProductionShelfGroup {
                    shelf_id: shelf_id.to_string(),
                    shelf_code,
                    shelf_name,
                    total_count,
                    items,
                }
            })
            .collect();

        // 4) 品检区扁平
        let insp_items: Vec<DashboardItem> = top
            .on_insp_pairs
            .into_iter()
            .map(|(b, p)| {
                let mut item = part_to_item(&p, &b, &top.cust_map, &top.process_map);
                item.current_holder_kind = Some("shelf".into());
                item
            })
            .collect();

        // 5) 工人持有（含 PICKED_UP 时间戳 + 工人姓名）
        let worker_items: Vec<DashboardItem> = recent
            .worker_pairs
            .into_iter()
            .map(|(b, p)| {
                let mut item = part_to_item(&p, &b, &recent.cust_map, &top.process_map);
                item.current_holder_kind = Some("worker".into());
                item.worker_name = b
                    .holder_id
                    .and_then(|wid| worker_name_map.get(&wid).cloned());
                if let Some(bid) = recent.picked_at_map.get(&b.batch_id) {
                    item.picked_up_at = Some(bid.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string());
                }
                item
            })
            .collect();

        let ts = chrono::Local::now()
            .format("%Y-%m-%dT%H:%M:%S%.3f%:z")
            .to_string();

        Ok(DashboardSnapshot {
            on_production_shelves: prod_groups,
            on_inspection_shelves: insp_items,
            in_process: worker_items,
            upcoming_delivery: upcoming,
            ts,
        })
    }
}

#[allow(clippy::too_many_arguments)]
fn part_to_item(
    p: &PartLite,
    b: &BatchLite,
    cust_map: &HashMap<i64, (Option<String>, String)>,
    process_map: &HashMap<i64, String>,
) -> DashboardItem {
    let (cust_name, cust_path) = cust_map
        .get(&p.customer_id)
        .cloned()
        .unwrap_or((None, String::new()));
    let np_name = b
        .next_process_id
        .and_then(|np| process_map.get(&np).cloned());
    DashboardItem {
        id: p.part_id.to_string(),
        batch_id: Some(b.batch_id.to_string()),
        // 2026-09-15 review 修：从 BatchLite.batch_no 取值（之前硬编 None 丢数据）
        batch_no: b.batch_no,
        serial_no: p.serial_no.clone(),
        name: p.name.clone(),
        drawing_no: p.drawing_no.clone(),
        quantity: p.quantity,
        is_urgent: p.is_urgent,
        planned_delivery_date: p
            .planned_delivery_date
            .map(|d| d.format("%Y-%m-%d").to_string()),
        picked_up_at: None,
        current_holder_id: b.holder_id.map(|h| h.to_string()),
        current_holder_kind: None,
        shelf_code: None,
        customer_id: Some(p.customer_id.to_string()),
        customer_name: cust_name,
        customer_path: Some(cust_path),
        next_process_id: b.next_process_id.map(|np| np.to_string()),
        next_process_name: np_name,
        worker_name: None,
    }
}

//! dashboard 域 snapshot 装配 service
//!
//! ## 设计要点
//! - 4 次 trait call 拉全量数据（`snapshot_overdue` / `snapshot_in_inspection_count` /
//!   `snapshot_recent_batches` + `snapshot_workers` / `snapshot_system_delivery_orders`），
//!   service 只做装配
//! - `repo/sql.rs::DashboardRepo` ZST 提供聚合静态方法；交期三方法在
//!   `repo/delivery.rs::DeliveryRepo`
//! - service 不持 repo / pool——handler 借 `&mut *tx` 喂给 trait 即可
//!
//! ## 事务分层
//! 事务移交 handler：service 方法 `<R: DashboardRepoTrait>(&self, repo: R)` by-value。
//! 生产 `R = &mut PgConnection`，handler `pool.begin()` + `tx.commit()` 包外。
//!
//! ## 数据结构
//! VO 全在 `vo/` 下；SQL 行精简（`BatchLite` / `PartLite`）在 `repo/sql.rs`。

use crate::infra::clock::now_naive;
use crate::modules::dashboard::repo::{BatchLite, DashboardRepoTrait, PartLite};
use crate::modules::dashboard::vo::{DashboardSnapshot, WorkerHeldBatch};

/// 大屏 snapshot 装配 service
///
/// unit struct（无字段依赖；事务移交 handler；handler 借连接传入 repo）。
/// 直接 `Arc<DashboardService>` 存 `AppState`；方法签名收 `repo: R` by-value。
pub struct DashboardService;

impl Default for DashboardService {
    fn default() -> Self {
        Self
    }
}

impl DashboardService {
    /// 构造（空 struct，无字段）。
    pub fn new() -> Self {
        Self
    }

    /// 异步构建一次完整快照（HTTP 首取与 WS 握手首帧共用）。
    ///
    /// 返回 `{overdue_count, in_inspection_count, in_process,
    /// system_delivery_orders, ts}`。交期分桶已拆到独立端点
    /// （`GET /api/v2/dashboard/upcoming-delivery`），不再内嵌在本快照里。
    ///
    /// service 内零 SQL——所有 SQL 在 `repo/sql.rs` / `repo/delivery.rs`。
    pub async fn build_snapshot<R: DashboardRepoTrait>(
        &self,
        mut repo: R,
    ) -> Result<DashboardSnapshot, sqlx::Error> {
        // 「今天」只取一次，同一个值喂给逾期 / 面板两个查询：两侧窗口边界必须一致，
        // 否则同一条工单可能同时落进逾期数与面板。
        let today = now_naive().date();

        let overdue_count = repo.snapshot_overdue(today).await?;
        let in_inspection_count = repo.snapshot_in_inspection_count().await?;
        let system_delivery_orders = repo.snapshot_system_delivery_orders(today).await?;

        let recent = repo.snapshot_recent_batches().await?;

        // 工人 id 列表（用于 worker name 查表）
        let worker_ids: Vec<i64> = recent
            .worker_pairs
            .iter()
            .filter_map(|(b, _)| b.holder_id)
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();
        let worker_name_map = repo.snapshot_workers(&worker_ids).await?;

        let in_process: Vec<WorkerHeldBatch> = recent
            .worker_pairs
            .into_iter()
            .map(|(b, p)| batch_to_worker_held(&p, &b, &worker_name_map))
            .collect();

        let ts = crate::infra::clock::now_shanghai_iso();

        Ok(DashboardSnapshot {
            overdue_count,
            in_inspection_count,
            in_process,
            system_delivery_orders,
            ts,
        })
    }
}

fn batch_to_worker_held(
    p: &PartLite,
    b: &BatchLite,
    worker_name_map: &std::collections::HashMap<i64, String>,
) -> WorkerHeldBatch {
    WorkerHeldBatch {
        id: p.part_id.to_string(),
        batch_id: Some(b.batch_id.to_string()),
        serial_no: p.serial_no.clone(),
        quantity: p.quantity,
        is_urgent: p.is_urgent,
        current_holder_id: b.holder_id.map(|h| h.to_string()),
        worker_name: b
            .holder_id
            .and_then(|wid| worker_name_map.get(&wid).cloned()),
    }
}

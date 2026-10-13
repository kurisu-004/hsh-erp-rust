//! `SCAN_BANNER`（业务事件 banner）的共享构造与广播。
//!
//! 2026-10-13 新增。前端要按「姓名 + 动作 + 序列号 + 数量 + 系统交期」拼一行
//! banner 文案（形如 `张三领取 F1234 x 20 (10-27)`，交期可空则不显示），需要一个
//! 专属 VO 承载这些字段。
//!
//! ## 为什么是**独立事件**而不是给既有事件加字段
//! 1. `WORKER_SCAN_RETURNED` / `WORKER_SCAN_INSPECTED` 的 payload 是
//!    `serde_json::to_value(&scan_out)` **整 VO 直转**，给 `WorkerScanCoreOut` 加
//!    展示字段会**同时改掉 HTTP 响应体**，把纯展示需求扩散到扫码主链路的 wire
//!    契约上；
//! 2. 既有事件刻意只带 id（「不要指望事件带全量快照」），为 banner 破例会让同一个
//!    `{type:'event'}` 信封下并存两套 payload 契约 —— 本仓已因「同名事件两个
//!    payload 形状」吃过一次 review MAJOR。
//!
//! 本函数是**唯一构造点**，两个产出点（`pick_up` / `worker_scan`）都走它。
//!
//! ## 本事件**不进**前端失效白名单
//! banner 是纯展示信号：不改变任何看板数据的有效性，消费方应当只把它渲染成一行
//! 文案、**不要**拿它去 invalidate 任何 query。故本仓不把它登记进
//! `docs/api/dashboard.md` §7 的「前端 `AFFECTS_DASHBOARD` 白名单」对应关系里。

use crate::infra::ws_hub::WsEvent;
use crate::modules::part::repo::PartRepoTrait;
use crate::modules::prod::scan::vo::ScanBannerItem;
use crate::modules::prod::worker::repo::WorkerRepo;
use crate::state::AppState;

/// 补查零件 / 工人并广播一条 `SCAN_BANNER`（commit 之后调用）。
///
/// - `action`：三选一 —— `"PICKED_UP"` / `"RETURNED"` / `"INSPECTED"`。
/// - `worker_id` / `part_id`：产出点已知的两个主键。
/// - `quantity`：本次动作涉及的数量（产出点的既有出参，不另查）。
///
/// ## 补查走 `state.pool` 而不是复用事务
/// 三个产出点的 `tx.commit()` 都**早于**本调用，事务已耗尽，`&mut *tx` 借不出可
/// 用的连接。因此这里自行 `pool.acquire()`。
///
/// ## 两条查询都取 `include_deleted = true`
/// banner 描述的是**刚刚被操作过**的那一行：零件与工人都还在，只是可能被软删
/// （工人软删后仍可能有在途批次）。此时查不到行 = banner 恒缺，与业务事实矛盾。
///
/// ## 查询失败**静默跳过**而不是抛错
/// 这是一条纯展示事件，而调用点已经 `commit()` 成功。把查不到行 / 查询失败升级成
/// 错误，等于让一个展示事件把一笔已经落库成功的业务请求变成 500 —— 代价与收益
/// 完全不成比例。故失败一律 `tracing::warn!` 后 `return`。
pub async fn broadcast_scan_banner(
    state: &AppState,
    action: &str,
    worker_id: i64,
    part_id: i64,
    quantity: i32,
) {
    let mut conn = match state.pool.acquire().await {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(
                action,
                part_id,
                worker_id,
                quantity,
                error = %e,
                "SCAN_BANNER：连接池取连接失败，跳过本次 banner 广播"
            );
            return;
        }
    };
    // `PartRepoTrait` 的 impl 写在 `&mut PgConnection` 上（不是 `PgConnection`），
    // 故这里必须有一个 `&mut PgConnection` 形参的绑定来借用它。
    let mut repo = &mut *conn;
    let part = match repo.get_by_id(part_id, true).await {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(
                action, part_id, error = %e, "SCAN_BANNER：查零件失败，跳过本次 banner 广播"
            );
            return;
        }
    };
    let Some(part) = part else {
        tracing::warn!(
            action,
            part_id,
            "SCAN_BANNER：零件查不到（含软删），跳过本次 banner 广播"
        );
        return;
    };
    let worker = match WorkerRepo::get_by_id(&mut *conn, worker_id, true).await {
        Ok(w) => w,
        Err(e) => {
            tracing::warn!(
                action, worker_id, error = %e, "SCAN_BANNER：查工人失败，跳过本次 banner 广播"
            );
            return;
        }
    };
    let Some(worker) = worker else {
        tracing::warn!(
            action,
            worker_id,
            "SCAN_BANNER：工人查不到（含软删），跳过本次 banner 广播"
        );
        return;
    };
    let item = ScanBannerItem {
        action: action.to_string(),
        worker_name: worker.name,
        serial_no: part.serial_no,
        drawing_no: part.drawing_no,
        quantity,
        system_delivery_date: part.system_delivery_date,
    };
    state.ws_hub.broadcast(WsEvent::DashboardEvent {
        kind: "SCAN_BANNER".into(),
        payload: serde_json::to_value(&item).unwrap_or_default(),
    });
}

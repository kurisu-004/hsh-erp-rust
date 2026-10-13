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
//! `docs/api/dashboard.md` §7 里那句「前端 `AFFECTS_DASHBOARD` 白名单与之人工对应
//! （关系是子集）」—— §7 提到本事件时只登记 payload 形状，不声称它影响大屏。

use crate::infra::ws_hub::WsEvent;
use crate::modules::part::repo::PartRepoTrait;
use crate::modules::prod::scan::vo::ScanBannerItem;
use crate::modules::prod::worker::repo::WorkerRepo;
use crate::state::AppState;

/// `worker-scan` 的响应 `event_type` → banner `action` 字面量。
///
/// 2026-10-13 新增。抽成独立函数是为了让「按**响应**派生、不是按请求派生」这条纪律
/// 有一个**被 lib 单测钉住**的落点：内联映射的两条臂之间没有任何护栏，改成按
/// `req.event_type` 派生会把链尾自动送检说成「放回」，且既有响应侧断言
/// （`tests/production/scan_worker_scan.rs` 钉「请求 `RETURNED` / 响应
/// `WORKER_SCAN_INSPECTED`」）**完全拦不住**。
///
/// ⚠️ **入参必须是 `WorkerScanCoreOut::event_type`（响应）的值**，不是
/// `WorkerScanRequest::event_type`（请求）的值：链尾自动送检会让「请求 `RETURNED` /
/// 响应 `WORKER_SCAN_INSPECTED`」成立，按请求派生就会把自动送检说成放回。调用点是
/// 一行 `match banner::banner_action(scan_out.event_type.as_str())` —— 形参位置写着
/// `scan_out` 就是刻意让这件事在源码上一眼可见。
///
/// 未知值返 `None`（调用点 warn 后不发）。返回类型**不含** `pick-up` 的
/// `"PICKED_UP"`：那条产出点的 `action` 是常量、不经本映射，故
/// `banner_action("PICKED_UP") == None` 由下方单测钉住。
pub fn banner_action(event_type: &str) -> Option<&'static str> {
    match event_type {
        "WORKER_SCAN_RETURNED" => Some("RETURNED"),
        "WORKER_SCAN_INSPECTED" => Some("INSPECTED"),
        _ => None,
    }
}

/// 补查零件 / 工人并广播一条 `SCAN_BANNER`（commit 之后调用）。
///
/// - `action`：三选一 —— `"PICKED_UP"` / `"RETURNED"` / `"INSPECTED"`。
/// - `worker_id` / `part_id`：产出点已知的两个主键。
/// - `quantity`：本次动作涉及的数量（产出点的既有出参，不另查）。
///
/// ## 零订阅者时**直接早退**，不取连接、不查库
/// `WsHub::broadcast` 在零订阅者时 `send` 失败即丢弃 ⇒ 查到的展示数据没有去处，
/// 白花 1 次 `pool.acquire()` + 2 条主键查询。`worker-scan` / `pick-up` 是报工台最热的
/// 写路径，而 `acquire_timeout = 5s`（`infra/db.rs`）⇒ 池耗尽时早退让这笔**已经
/// commit 成功**的写请求零额外等待，而不是白等 5s 再降级成一条 warn。
///
/// ⚠️ 口径：本早退**保守**、不会丢帧。`conn_count()` 记的是「已认证握手」的连接表
/// （`handle_socket` 入口即 `register_conn`，早于 `run_socket` 里的 `subscribe()`），
/// 它 **≥** 真正订阅广播的连接数 ⇒ 只可能「查了但广播被丢」，不可能「有人订阅却早退」。
///
/// ## 补查走 `state.pool` 而不是复用事务
/// 两个产出点的 `tx.commit()` 都**早于**本调用，事务已耗尽，`&mut *tx` 借不出可
/// 用的连接。因此这里自行 `pool.acquire()`。
///
/// ## 两条查询都取 `include_deleted = true`（防御性取值）
/// 当前两个产出点的**写侧**都硬拦软删（`worker_scan` 侧 `get_by_badge_code(.., false)` /
/// `get_by_serial(.., false)`，`pick_up` 侧 `AND deleted_at IS NULL`）⇒ 软删工人根本
/// 进不了扫码台，所以「软删了还扫得到」不是可达路径。取 `true` 只是**不让展示事件因
/// 软删竞态丢失**（写侧校验通过、广播补查之前那一行被软删）—— 属于「查不到就少一行
/// banner」的防御，不是「允许操作软删行」的业务口径。
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
    if state.ws_hub.conn_count() == 0 {
        return;
    }
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
    // `unwrap_or_default()` 在这里是安全的：本 VO 全部字段都是 `String` / `i32` /
    // `Option<String>` / `Option<NaiveDate>`，serde_json 序列化它们**不会失败**（没有
    // 自定义 Serialize、没有 map key、没有浮点 NaN/Inf），故这条兜底分支实际不可达 ——
    // 真走到也只是广播一条 `payload: null`，与全仓既有广播写法一致。
    state.ws_hub.broadcast(WsEvent::DashboardEvent {
        kind: "SCAN_BANNER".into(),
        payload: serde_json::to_value(&item).unwrap_or_default(),
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 钉住 `worker-scan` 两条映射：banner 的 `action` 必须按**响应**的
    /// `scan_out.event_type` 派生（链尾自动送检 → `"INSPECTED"`，不是 `"RETURNED"`）。
    #[test]
    fn banner_action_maps_worker_scan_response_event_types() {
        assert_eq!(banner_action("WORKER_SCAN_RETURNED"), Some("RETURNED"));
        assert_eq!(
            banner_action("WORKER_SCAN_INSPECTED"),
            Some("INSPECTED"),
            "链尾自动送检时响应是 WORKER_SCAN_INSPECTED，action 必须是 INSPECTED"
        );
    }

    /// 未知值返 `None`（调用点 warn 后不发），且**不得**把请求侧的
    /// `"RETURNED"` / `"INSPECTED"` 当成合法输入 —— 那正是「按 req 派生」的 bug 形态，
    /// 本函数只吃 `WORKER_SCAN_*` 响应字面量。
    #[test]
    fn banner_action_rejects_non_worker_scan_event_types() {
        assert_eq!(banner_action("PICKED_UP"), None);
        assert_eq!(banner_action("RETURNED"), None);
        assert_eq!(banner_action("INSPECTED"), None);
        assert_eq!(banner_action(""), None);
    }
}

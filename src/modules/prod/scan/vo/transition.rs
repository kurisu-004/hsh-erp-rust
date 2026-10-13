//! prod::scan 报工台**写路径**出参 VO
//!
//! 2026-10-10 自 `prod::batch::vo` 搬来：`worker-scan` 端点连同其 DTO / service
//! 一起迁入 `prod::scan`（旧路径 `POST /api/v2/prod/batches/worker-scan` 已下线，
//! **无 alias**，新路径 `POST /api/v2/prod/scan/worker-scan`）。
//!
//! 响应形状逐字不变：仍是 `R<WorkerScanOut>`，`scan` + `refill` 两段。
//!
//! 2026-10-11：worker-scan 支持部分数量后，`scan.batch_id` 的取值从「工人手上那批」
//! 变成「**本次实际被处理的那一批**」（拆批场景 = 拆出来的新批次），并新增一个
//! **不进 JSON** 的内部管道字段 `split`（与 `work_type_id` / `badge_code` 同类）。
//!
//! 2026-10-13：再新增第 4 个内部管道字段 `operated_quantity`（本次实际处理量，
//! 供 handler 在 commit 之后组 `SCAN_BANNER`），同样是 `#[serde(skip)]`。⇒ **内部
//! 管道字段共 4 个**：`work_type_id` / `badge_code` / `split` / `operated_quantity`。
//! JSON 形状本身仍是那 6 个键。

use serde::Serialize;

use crate::modules::prod::queue::vo::worker::RefillResult;
use crate::shared::types::{serialize_i64, serialize_i64_opt};

/// worker-scan 核心出参（不含 refill）。
///
/// handler 会把 `scan + refill` 一起装到 [`WorkerScanOut`] 返回；
/// `WorkerScanCoreOut` 是 service 层直接产出的最小投影（与 queue 域的
/// `RefillResult` 解耦，便于 service 层单测）。
///
/// 本 struct 共 **4 个内部管道字段**，全部 `#[serde(skip)]`、全部不进 JSON：
/// - `work_type_id` / `badge_code`：handler 用它把 `worker_scan_event` 已经 fetch 过的
///   worker 信息透传给同事务的 `QueueService::refill_for_worker_with_work_type`，
///   避免重复 `WorkerRepo::get_by_id` 查询（refill 写 `TAKEN_FROM_POOL` 事件日志还要
///   `badge_code`）；
/// - `split`：部分数量自动拆批信息，供 handler 在 commit 之后补发 `PART_BATCH_SPLIT`；
/// - `operated_quantity`（2026-10-13 新增）：本次实际处理量，**只供** handler 在
///   commit 之后组 `SCAN_BANNER`，别处零消费。
///
/// 逐字段登记（含「哪一个不进 JSON」）见 `docs/api/scan.md` §2.5。
#[derive(Debug, Clone, Serialize)]
pub struct WorkerScanCoreOut {
    #[serde(serialize_with = "serialize_i64")]
    pub worker_id: i64,
    #[serde(serialize_with = "serialize_i64")]
    pub part_id: i64,
    /// **本次实际被处理的那个批次**：拆批场景（`quantity` 落在 `0 < q <
    /// batch.quantity`）下是拆出来的**新批次**，整批场景下是工人手上那批。
    /// 消费方（前端「已处理哪一件」的回显与后继动作锚点）据此刷新。
    #[serde(serialize_with = "serialize_i64")]
    pub batch_id: i64,
    /// ⚠️ **可能与请求的 `event_type` 不同**：批次在工序链上是最后一道时，
    /// 放回（`RETURNED`）被服务端改判为送检，这里返
    /// `"WORKER_SCAN_INSPECTED"`。消费方（前端文案 + WS 广播）都按**本字段**分支。
    pub event_type: String,
    /// 父装配件 id（仅当送检路径触发父 status 变更时 Some）
    #[serde(serialize_with = "serialize_i64_opt")]
    pub synced_assembly_id: Option<i64>,
    /// 内部：透传给 refill，refill 不再 fetch worker。
    #[serde(skip)]
    pub work_type_id: i64,
    /// 内部：refill 写 `TAKEN_FROM_POOL` 事件日志需要 badge_code。
    #[serde(skip)]
    pub badge_code: String,
    /// 内部：部分数量自动拆批信息（整批路径为 `None`），供 handler 在 commit
    /// 之后补发 `PART_BATCH_SPLIT`。**不进 JSON** —— 与本 struct 的另三个内部管道
    /// 字段同类，「响应体形状不变」是本端点的既有契约。
    #[serde(skip)]
    pub split: Option<WorkerScanSplitInfo>,
    /// 内部：本次**实际**处理的数量（整批 = `batch.quantity`、部分数量 = 拆走量），
    /// 供 handler 在 commit 之后组装 `SCAN_BANNER`。
    ///
    /// 2026-10-13 新增。⚠️ **`#[serde(skip)]` 是硬要求**：handler 把本 struct
    /// `serde_json::to_value` 整份转成 `WORKER_SCAN_RETURNED` /
    /// `WORKER_SCAN_INSPECTED` 的 WS payload，加一个不 skip 的字段就等于给
    /// **既有 wire 契约**（`docs/api/scan.md` §2.5 与前端 Zod schema 依赖的那份）
    /// 塞进一个纯展示字段 —— 展示需求不该扩散到扫码主链路的响应体上。
    #[serde(skip)]
    pub operated_quantity: i32,
}

/// 业务事件 banner 的专属出参 VO（`SCAN_BANNER` 事件的 payload）。
///
/// 2026-10-13 新增：前端要按「姓名 + 动作 + 序列号 + 数量 + 系统交期」拼一行
/// banner 文案（形如 `张三领取 F1234 x 20 (10-27)`，交期可空则不显示）。
///
/// ## 为什么是**独立事件**而不是给既有事件加字段
/// 1. `WORKER_SCAN_*` 的 payload 是整份 `WorkerScanCoreOut` 的序列化，给它加
///    展示字段会**同时改掉 HTTP 响应体**，把纯展示需求扩散到扫码主链路上；
/// 2. 既有事件刻意只带 id（「不要指望事件带全量快照」），为 banner 破例会让
///    同一个 `{type:'event'}` 信封下并存两套 payload 契约。
///
/// 本 VO 只有 **2 个产出点**、单一 payload 形状，与「同名事件两个 payload」的
/// 历史教训正面相反。完整契约见 `docs/api/dashboard.md` §7 与
/// `docs/api/scan.md` §4。
#[derive(Debug, Clone, Serialize)]
pub struct ScanBannerItem {
    /// 动作字面量，三选一：`"PICKED_UP"`（领取）/ `"RETURNED"`（放回）/
    /// `"INSPECTED"`（送检）。由产出点传入，见
    /// [`crate::modules::prod::scan::handler::banner`]。
    pub action: String,
    /// `t_worker.name`（`张三` 这样的显示名，不是 `badge_code`）。
    pub worker_name: String,
    /// `t_part.serial_no`，**可空** —— 终态工单被 migration
    /// `20261001000200_007_serial_release.sql` 置 NULL，且手工工单本就没有序列号。
    /// 消费方在它为 null 时回退读 [`Self::drawing_no`]。
    pub serial_no: Option<String>,
    /// `t_part.drawing_no`（**必填**）：`serial_no` 缺失时的兜底显示值。
    pub drawing_no: String,
    /// 本次动作涉及的数量（领取量 / 放回量 / 送检量）。
    pub quantity: i32,
    /// `t_part.system_delivery_date`（baseline 可空）。走 chrono 默认 serde ⇒
    /// JSON 形状恒为 `"YYYY-MM-DD"` 字符串，**无交期时是 `null`**（不是空串、
    /// 不是缺键）—— 与 `prod::inspection` 的同名出参逐字同款。
    pub system_delivery_date: Option<chrono::NaiveDate>,
}

/// 部分数量（`WorkerScanRequest::quantity`）触发自动拆批时带回 service 的拆批事实。
///
/// 2026-10-11 新增，字段与 pick-up 侧的
/// [`crate::modules::prod::scan::service::pickup::PickUpSplitInfo`] 同构，两处共用
/// `PART_BATCH_SPLIT` 事件名与 payload 字段名。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkerScanSplitInfo {
    /// 被扣减的源批次 id（余量留在这里，仍在工人手上）。
    pub source_batch_id: i64,
    /// 拆出来、被本次流转处理的那一批的雪花 id（= 响应的 `scan.batch_id`）。
    pub new_batch_id: i64,
    /// 拆给本次操作的数量（= 新批次 quantity = 事件日志的 `quantity`）。
    pub quantity: i32,
}

/// worker-scan 端点出参：`scan` + 同事务 refill 结果。
#[derive(Debug, Clone, Serialize)]
pub struct WorkerScanOut {
    pub scan: WorkerScanCoreOut,
    pub refill: RefillResult,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 构造一条 banner payload 的样板（两条用例只差两个可空字段）。
    fn item(serial_no: Option<&str>, sdd: Option<&str>) -> ScanBannerItem {
        ScanBannerItem {
            action: "PICKED_UP".into(),
            worker_name: "张三".into(),
            serial_no: serial_no.map(str::to_string),
            drawing_no: "DWG-BN-1".into(),
            quantity: 20,
            system_delivery_date: sdd
                .map(|s| chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()),
        }
    }

    /// 前端的 banner 文案与 Zod schema 是照这份 JSON 写的，故键集合与形状必须
    /// 钉死：恰好 6 个键、日期恒为 `"YYYY-MM-DD"` 字符串、两个可空字段在无值时
    /// 是 JSON `null`（缺键 / 空串都会让前端兜底出怪值）。
    #[test]
    fn scan_banner_payload_has_exactly_six_keys_and_iso_date() {
        let v = serde_json::to_value(item(Some("F1234"), Some("2026-10-27"))).unwrap();
        assert_eq!(
            v,
            json!({
                "action": "PICKED_UP",
                "worker_name": "张三",
                "serial_no": "F1234",
                "drawing_no": "DWG-BN-1",
                "quantity": 20,
                "system_delivery_date": "2026-10-27",
            }),
            "banner payload 的键集合与 JSON 形状就是前端契约，逐字对齐"
        );
        assert_eq!(v.as_object().unwrap().len(), 6, "恰好 6 个键");
    }

    #[test]
    fn scan_banner_nullable_fields_serialize_as_json_null_not_missing_key() {
        let v = serde_json::to_value(item(None, None)).unwrap();
        let o = v.as_object().unwrap();
        assert_eq!(o.len(), 6, "两个字段为 null 时键数不变");
        assert_eq!(v["serial_no"], serde_json::Value::Null);
        assert_eq!(v["system_delivery_date"], serde_json::Value::Null);
        assert_eq!(
            v["drawing_no"], "DWG-BN-1",
            "serial_no 缺失时 drawing_no 仍是兜底显示位、必须有值"
        );
    }

    /// 回归：`operated_quantity` 加了之后，`WORKER_SCAN_*` / `PART_BATCH_SPLIT`
    /// 等既有事件的 payload 是 `serde_json::to_value(&scan_out)` 整份直转 ——
    /// 一旦这个字段没 skip，扫码主链路的 wire 契约就被塞进了一个纯展示字段
    /// （前端 Zod schema 会因此红）。内部管道字段同理。
    #[test]
    fn worker_scan_core_out_keeps_existing_json_keys() {
        let out = WorkerScanCoreOut {
            worker_id: 1,
            part_id: 2,
            batch_id: 3,
            event_type: "WORKER_SCAN_RETURNED".into(),
            synced_assembly_id: None,
            work_type_id: 4,
            badge_code: "B-001".into(),
            split: None,
            operated_quantity: 20,
        };
        let v = serde_json::to_value(&out).unwrap();
        let mut keys: Vec<&String> = v.as_object().unwrap().keys().collect();
        keys.sort();
        assert_eq!(
            keys,
            vec![
                "batch_id",
                "event_type",
                "part_id",
                "synced_assembly_id",
                "worker_id"
            ],
            "内部管道字段（含新增的 operated_quantity）不得进 JSON"
        );
    }
}

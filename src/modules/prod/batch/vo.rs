//! prod::batch 子模块 VO —— 出参（handler 响应序列化层）
//!
//! 仅 `Serialize` 不 `Deserialize`（禁止出现在 axum extractor 反序列化侧）。
//!
//! i64 一律走 `serialize_i64` → JSON string（雪花 ID > 2^53，JS `Number`
//! 会丢精度，参见 `shared::types` 模块 doc）。
//!
//! ## 分组
//! - **流转与生命周期**（`transition*` / `lifecycle` / `shelf` / `programming` /
//!   `repair` / `batch_ops` / `scan`）：`ToXxxOut` / `BatchToXxxOut` /
//!   `BatchOpFailure`
//! - **集合读**（`repair.rs`）：`InspectionBatchListOut`（仅 repair / repairing
//!   两条共用）
//!
//! 2026-10-07：待品检队列的出参（分页列表 + 13 字段行项）随端点迁往
//! `prod::inspection` —— 该页面的两个数据源现同域。
//!
//! 2026-10-08：下发流的 7 类出参（`PendingBatchItem` / `PendingBatchListOut` /
//! `DispatchResult` / `DispatchSuccessItem` / `DispatchFailureItem` /
//! `AutoDispatchItem` / `AutoDispatchResult`）**连同本文件里的旧副本**一并迁往
//! `prod::queue::vo::queue` —— 它们的唯一消费方是队列页的下发动作。
//!
//! 2026-10-09：拆批端点提升为顶层共用端点 `POST /api/v2/batches/split`（三个
//! 消费方共用），出参由 `R<i64>` 裸数字换成 [`BatchSplitOut`]（全 ID 字符串）。
//!
//! 2026-10-10：报工台的 worker-scan 出参（`WorkerScanCoreOut` / `WorkerScanOut`）
//! 随端点迁往 `prod::scan::vo::transition`。pick-up 端点也一并迁出，但其 HTTP
//! 响应体是 `PartOut`（与 to-XXX 三流共用），本 VO 的形状不变。
//!
use chrono::{NaiveDate, NaiveDateTime};
use serde::Serialize;

use crate::modules::part::vo::PartOut;
use crate::shared::types::{serialize_i64, serialize_i64_opt};

// ===== 集合读（返修：repair / repairing 两条共用）=====

/// `GET /prod/batches/repair` / `repairing` 列表行：批次 + 工单 + 客户 + holder/
/// process/delivery_note 名称（一次性 JOIN 解析，不在 service 做 N+1）。
///
/// **仅这 2 条端点共用**：待品检队列读（`GET /api/v2/prod/inspection/queue`，2026-10-07
/// 起在 `prod::inspection` 域）用的是 13 字段精简 VO —— 待品检页只渲染 7 个数据列，
/// 共用本 VO 会让 15 个字段成为无用负载。
///
/// 字段命名沿用 v1 `PartOut`/`PartBatchOut` 约定（`batch_id` 即 `t_part_batch.id`，
/// `version` 即乐观锁版本号）。前端用 `batch_id + version` 直接拼
/// `POST /prod/batches/{batch_id}/to-ship` 或 `to-inspection` 的请求体。
///
/// 2026-09-16 PR-2 瘦身（migration 027）：删 `has_been_repaired` 字段
/// （t_part_batch 列已删；返修事实由 t_part_event REPAIR_STARTED 事件追溯）。
///
/// 2026-09-16 PR-3 批次 step 化（migration 028）：
/// - 删 `placed_at`（t_part_batch 列已删，不再统计生产时间）
/// - 新增 `current_process_step_id`：逻辑 FK → t_process_chain_step.id
///   （批次的**链内位置指针**，随工序推进；NULL = 批次尚未进入生产流或 part 无链）
/// - `next_process_id` / `next_process_name` 字段保留，由 repo JOIN step 派生
///   （保持 DTO 兼容，不破坏前端）
///
/// 2026-09-30（review 第 3 轮 M3）：本 VO 的 `next_process_id` 保持**从
/// `current_process_step_id` 经 step JOIN 派生**，不改直读新列
/// `current_process_id`。后者是**池归属权威列**，只服务 5 条工序池 SQL + rollup；
/// INSPECTION 批次按出池不变式该列恒为 NULL，直读会让本 VO 的工序字段恒 null。
#[derive(Debug, Clone, Serialize)]
pub struct InspectionBatchListItemOut {
    // ===== 批次字段 =====
    #[serde(serialize_with = "serialize_i64")]
    pub batch_id: i64,
    pub batch_no: i32,
    pub quantity: i32,
    pub status: String, // 必为 "INSPECTION"
    /// 2026-10-01 review 第 1 轮 M5 新增（migration 005，**BREAKING**）：
    /// `REPAIRING` 已从 `PartStatus` 降级为 `t_part_batch.is_repairing` 标记列，
    /// 本 VO 的 `status` 因此**恒为** `IN_PROCESS`（`GET /prod/batches/repairing`
    /// 的过滤判据已是 `is_repairing = true`）。改造前前端靠
    /// `status === 'REPAIRING'` 标「返修中」，现在任何端点都拿不到该值 ——
    /// 除非读本字段。
    pub is_repairing: bool,
    pub location: Option<String>,
    pub version: i32,
    /// 逻辑 FK → t_process_chain_step.id（2026-09-16 PR-3；替代 next_process_id 列）
    #[serde(serialize_with = "serialize_i64_opt")]
    pub current_process_step_id: Option<i64>,
    #[serde(serialize_with = "serialize_i64_opt")]
    pub parent_batch_id: Option<i64>,

    // ===== holder 解析（COALESCE 三表）=====
    #[serde(serialize_with = "serialize_i64_opt")]
    pub current_holder_id: Option<i64>,
    pub holder_name: Option<String>,
    /// 由 `current_process_step_id` 经 `LEFT JOIN t_process_chain_step` 取
    /// `s.process_id` 派生；保留字段名以兼容前端契约。
    ///
    /// 2026-09-30 一度改直读 `t_part_batch.current_process_id`（migration 004），
    /// **2026-09-30 review 第 3 轮 M3 已回退**（follow-up 于 2026-09-30 补齐
    /// repair 两端点的漏网项）。原因：送检 = 出池，该列被置 NULL。
    ///
    /// 本字段由 2 个端点共用（`InspectionBatchListItemOut`，别名
    /// `RepairBatchesOut`），**2 个都不是工序池端点**：
    /// - `GET /prod/batches/repair`（`DELIVERED`，2026-09-30 follow-up 补齐）
    /// - `GET /prod/batches/repairing`（`is_repairing = true`，2026-10-01 由
    ///   `status='REPAIRING'` 改为标记列过滤）
    ///
    /// DELIVERED 端点的 `current_process_id` 按「DELIVERED 必经 INSPECTION」
    /// 不变式同样恒 NULL（进 INSPECTION 的写点都把该列清成 NULL）⇒ 直读得不到
    /// 正确值，故该查询走 step 派生。
    /// 返修中端点：2026-10-01 起 `mark_batch_repairing` **不再把批次
    /// 翻出 IN_PROCESS**（status 保持不变），故该列对返修批次不再有「残留
    /// 陈旧值」问题 —— 但本 VO 仍统一走 step 派生，理由是「展示类列表一律走
    /// step 派生」这条分工（见 `prod/batch/model.rs` 模块 doc 的读取方清单），
    /// 不因单个端点的判据变化而分叉。
    /// `current_process_step_id` 在送检期间被刻意保留
    /// （`mark_batch_inspected` 不写它），正是「INSPECTION 期间显示批次走到
    /// 工艺链第几步」这条产品需求的数据来源。
    ///
    /// 2026-09-27 part 域前后端字段对齐：`/parts` 响应已对该字段加
    /// `#[serde(skip)]` 仅隐藏（DB 列保留、rollup 派生链路不变）。repair 域
    /// **不做 skip**，字段照常序列化。
    #[serde(serialize_with = "serialize_i64_opt")]
    pub next_process_id: Option<i64>,
    pub next_process_name: Option<String>,

    // ===== delivery_note 解析 =====
    #[serde(serialize_with = "serialize_i64_opt")]
    pub delivery_note_id: Option<i64>,
    pub delivery_note_no: Option<String>,

    // ===== 工单字段（JOIN t_part）=====
    #[serde(serialize_with = "serialize_i64")]
    pub part_id: i64,
    pub serial_no: Option<String>,
    pub drawing_no: String,
    pub name: String,
    pub order_no: Option<String>,
    pub planned_delivery_date: NaiveDate,
    pub is_urgent: bool,
    pub part_version: i32,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,

    // ===== 客户解析（JOIN t_customer + 自连 L1）=====
    #[serde(serialize_with = "serialize_i64")]
    pub customer_id: i64,
    pub customer_name: Option<String>,
    pub l1_customer_name: Option<String>,
}

/// `GET /api/v2/prod/batches/repair` / `repairing` 出参（分页）：返修批次列表。
///
/// **仅这 2 条端点**：待品检队列读（`GET /api/v2/prod/inspection/queue`）有自己的
/// 分页 VO，两条端点与它只有过滤判据不同。
#[derive(Debug, Clone, Serialize)]
pub struct InspectionBatchListOut {
    pub items: Vec<InspectionBatchListItemOut>,
    #[serde(serialize_with = "serialize_i64")]
    pub total: i64,
    #[serde(serialize_with = "serialize_i64")]
    pub limit: i64,
    #[serde(serialize_with = "serialize_i64")]
    pub offset: i64,
}

/// `GET /api/v2/prod/batches/repair` / `repairing` 出参别名。
pub type RepairBatchesOut = InspectionBatchListOut;

/// 单件 / 批量 to-XXX 端点的统一出参 shape。
///
/// `part`：操作后 part 的最新 [`PartOut`] 投影（含 OCC 更新后的 `version`）。
/// `new_batch_id`：仅当 `quantity < target.quantity` 走拆批分支时为
///   `Some(remainder_id)`（拆批后**剩余批次**的 id，留在源状态待后续操作）；
///   整批操作时为 `None`（序列化为 JSON `null`），前端拿到非 null 时应刷新批次列表。
///   用 `serialize_i64_opt` 把 Some 序列化为 JSON 字符串、None 序列化为 `null`，
///   跟 [`PartOut`] 的雪花 id 序列化契约对齐。
/// `synced_assembly_id`：仅当本 part 由 inspection 流触发父装配件 status 翻转时
///   为 `Some(assembly_id)`（handler 据此发 `ASSEMBLY_UPDATED` WS 广播）；
///   无父装配件或父未变更时为 `None`。
#[derive(Debug, Clone, Serialize)]
pub struct ToXxxOut {
    pub part: PartOut,
    #[serde(serialize_with = "serialize_i64_opt")]
    pub new_batch_id: Option<i64>,
    /// 父装配件 id（仅当本 part 由 inspection 流触发父 status 变更时 Some）
    #[serde(serialize_with = "serialize_i64_opt")]
    pub synced_assembly_id: Option<i64>,
}

/// Per-item 失败明细（item 级别错误，非整批失败）。
///
/// `batch_id`：按 batch 定位失败 item（批量 item 不含 `part_id`，服务从
///   `BatchOpItem::batch_id` 反查后回填；无法 parse 的串落到 `40001` 失败，
///   不进入本结构）。`i64` 而非 `String` 是因为 service 已 parse 过一次，
///   用 `serialize_i64` 序列化为 JSON 字符串与前端 batch_id 字段类型对称。
/// `code` 透传 service 层错误码（20103 / 20104 / 20109 / 20111 / 20511 / 20512 / 40901）；
/// `message` 透传 service 层错误文案（前端可作 toast）。
#[derive(Debug, Clone, Serialize)]
pub struct BatchOpFailure {
    #[serde(serialize_with = "serialize_i64")]
    pub batch_id: i64,
    pub code: i32,
    pub message: String,
}

/// 批量端点统一出参（`batch-to-ship` / `batch-to-inspection` 共用）。
///
/// `submitted`：成功并完成状态流转的 item（含 `PartOut` 最小投影 + 拆批后的
///   `new_batch_id`）；`failed`：item 级别错误（共享 [`BatchOpFailure`]）。
/// `submitted` 与 `failed` 互斥，单 item 不会同时出现在两侧。
#[derive(Debug, Clone, Serialize)]
pub struct BatchToXxxOut {
    pub submitted: Vec<ToXxxOut>,
    pub failed: Vec<BatchOpFailure>,
}

// ===== 共用顶层端点（/api/v2/batches）=====

/// `POST /api/v2/batches/split` 出参。**全 ID 字符串**。
///
/// 2026-10-09 新增。旧端点 `POST /api/v2/prod/batches/{batch_id}/split` 出参是
/// `R<i64>` 裸数字 —— 雪花 ID ≈ 9.0e18 远超 JS 的 `Number.MAX_SAFE_INTEGER`
/// (2^53-1 ≈ 9.007e15)，浏览器侧解析即丢精度。这正是本 VO 存在的成因，
/// 见本文件末尾 `batch_split_out_ids_serialize_as_strings` 回归单测。
///
/// `source_version` 是**源批次**（被扣减的那个）拆批后的 version，前端对源批次
/// 继续操作时必须带这个值；新批次的 version 从 0 起、由 `t_part_batch.version`
/// 的 DB 默认给出，本 VO 不重复返回。
#[derive(Debug, Clone, Serialize)]
pub struct BatchSplitOut {
    #[serde(serialize_with = "serialize_i64")]
    pub batch_id: i64,
    #[serde(serialize_with = "serialize_i64")]
    pub new_batch_id: i64,
    #[serde(serialize_with = "serialize_i64")]
    pub part_id: i64,
    /// **实际拆走量**（= 新批次的 quantity），**不是**源批次余量。
    /// 源批次余量 = 调用前的源批次 quantity − 本字段，**不在出参里**。
    pub quantity: i32,
    /// 源批次 version（拆批后 = 请求的 version + 1）
    pub source_version: i32,
}

#[cfg(test)]
mod tests {
    use super::BatchSplitOut;

    /// 2026-10-09 新增：三个雪花 ID 必须序列化成 JSON 字符串，不得出现裸数字。
    ///
    /// 这条测试存在的理由是旧出参 `R<i64>`：裸数字在 JS 侧落到
    /// `Number`（IEEE754 双精度，2^53-1 之上即失真），本仓雪花 ID ≈ 9.0e18
    /// 比那个上限大三个数量级，前端拿到的 `new_batch_id` 会与库里那一行对不上。
    #[test]
    fn batch_split_out_ids_serialize_as_strings() {
        let value = serde_json::to_value(BatchSplitOut {
            batch_id: 1_590_000_000_000_000_001,
            new_batch_id: 1_590_000_000_000_000_002,
            part_id: 1_590_000_000_000_000_003,
            quantity: 3,
            source_version: 4,
        })
        .expect("BatchSplitOut 应可序列化");
        for (field, raw) in [
            ("batch_id", "1590000000000000001"),
            ("new_batch_id", "1590000000000000002"),
            ("part_id", "1590000000000000003"),
        ] {
            assert_eq!(
                value[field],
                serde_json::Value::String(raw.to_string()),
                "`{field}` 必须是 JSON 字符串（裸数字会被 JS 截断精度）"
            );
        }
        // 非 ID 字段保持原生类型，别被 serde 顺手字符串化
        assert_eq!(value["quantity"], serde_json::json!(3));
        assert_eq!(value["source_version"], serde_json::json!(4));
    }
}

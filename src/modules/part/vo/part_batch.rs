//! part 域 batch 详情 / 列表 / 子域相关出参 VO（2026-09-22 PR4 重构）

use serde::Serialize;

use crate::modules::part::batch::model::PartBatchScanRow;
use crate::modules::part::service::crud::TPartScanRow;
use crate::shared::types::{serialize_i64, serialize_i64_opt};

/// `POST /parts/batch` per-item 失败明细。
///
/// `part_id`：`Some(id)` = INSERT 成功但 detail lookup 失败；
///            `None` = INSERT 本身失败。
#[derive(Debug, Clone, Serialize)]
pub struct PartBatchCreateFailure {
    #[serde(serialize_with = "serialize_i64_opt")]
    pub part_id: Option<i64>,
    pub code: i32,
    pub message: String,
    pub item_index: usize,
}

/// `POST /parts/batch` 出参：`created` 与 `failed` 互斥。
///
/// 2026-09-16 M2-B review 第 1 轮：`cleanup_tmp_keys` 新增字段。
/// - 含义：本批次成功 INSERT 后、需要 commit 后异步清理的 tmp 对象 key 列表
///   （client 已直传到 COS tmp 区，已被 service 端 head+copy 到 CAS key）。
/// - 用途：handler 在 `tx.commit()` 之后 `tokio::spawn` 批量 `cos.delete_object(&key)`
///   兜底，避免 commit 失败却已触发 COS 删除产生孤儿。
/// - 前端不需要该字段（`#[serde(default)]` 兜空，前端忽略）；后端用 `out.cleanup_tmp_keys`。
/// - legacy（无 binding）路径该列表为空，前端 / 集成测试无需关注。
#[derive(Debug, Clone, Serialize)]
pub struct PartBatchCreateOut {
    pub created: Vec<crate::modules::part::vo::PartDetailOut>,
    pub failed: Vec<PartBatchCreateFailure>,
    /// commit 后由 handler spawn 异步清理的 tmp 对象 key 列表。
    #[serde(default)]
    pub cleanup_tmp_keys: Vec<String>,
}

/// `GET /parts/by-serial/{serial_no}/part-batches` 出参：工单窄字段。
/// 字段严格来自 `t_part`（仅 8 列 + id），不复用 `PartDetailOut` 的 28 列 flatten。
#[derive(Debug, Clone, Serialize)]
pub struct PartScanInfoOut {
    #[serde(serialize_with = "serialize_i64")]
    pub id: i64,
    pub drawing_no: String, // b 图号
    pub name: String,       // 名称
    pub quantity: i32,      // 数量
    #[serde(serialize_with = "serialize_i64")]
    pub customer_id: i64, // 客户（仅 FK，不冗余 customer_name）
    pub system_delivery_date: Option<chrono::NaiveDate>, // 系统交期
    pub is_urgent: bool,    // 是否加急
    pub order_no: Option<String>, // 订单号
    pub note: Option<String>, // 备注
}

/// `GET /parts/by-serial/{serial_no}/part-batches` 出参：单批次窄字段。
/// `holder_name` 由 service 层经 repo `list_active_by_part_id_with_holder` 解析。
#[derive(Debug, Clone, Serialize)]
pub struct PartBatchScanOut {
    #[serde(serialize_with = "serialize_i64")]
    pub id: i64,
    pub quantity: i32,
    pub status: String,              // PartBatchStatus 字符串形态
    pub holder_name: Option<String>, // 当前持有人/货架名称（解析自 t_shelf/t_user/t_worker）
    pub version: i32,                // 乐观锁版本号（前端 to-ship 用）
}

/// Scan context 完整出参：工单 + 全部未删批次（按 batch_no 升序）。
#[derive(Debug, Clone, Serialize)]
pub struct PartScanContextOut {
    pub part: PartScanInfoOut,
    pub batches: Vec<PartBatchScanOut>,
}

/// `PartScanInfoOut::from(TPartScanRow)`：service 内私有窄字段 FromRow
/// (`src/modules/part/service/crud.rs::TPartScanRow`) → DTO 字段对拷。
impl From<TPartScanRow> for PartScanInfoOut {
    fn from(p: TPartScanRow) -> Self {
        Self {
            id: p.id,
            drawing_no: p.drawing_no,
            name: p.name,
            quantity: p.quantity,
            customer_id: p.customer_id,
            system_delivery_date: p.system_delivery_date,
            is_urgent: p.is_urgent,
            order_no: p.order_no,
            note: p.note,
        }
    }
}

/// `PartBatchScanOut::from(PartBatchScanRow)`：repo 解析出的批次窄字段 → DTO。
impl From<PartBatchScanRow> for PartBatchScanOut {
    fn from(p: PartBatchScanRow) -> Self {
        Self {
            id: p.id,
            quantity: p.quantity,
            status: p.status,
            holder_name: p.holder_name,
            version: p.version,
        }
    }
}

/// `POST /parts/match-by-excel-items` 出参：单行匹配结果（part_id 或 null）。
#[derive(Debug, Clone, Serialize)]
pub struct MatchByExcelItemResult {
    #[serde(default)]
    pub drawing_no: Option<String>,
    #[serde(default)]
    pub serial_no: Option<String>,
    #[serde(serialize_with = "serialize_i64_opt")]
    pub part_id: Option<i64>,
    pub status: String, // "MATCHED" / "NOT_FOUND" / "AMBIGUOUS"
    #[serde(default)]
    pub message: Option<String>,
}

/// `POST /parts/batch-update-order-info` 出参：成功 N，失败列表。
#[derive(Debug, Clone, Serialize)]
pub struct BatchUpdateOrderInfoOut {
    pub updated: i64,
    pub failed: Vec<BatchUpdateOrderInfoFailure>,
}

#[derive(Debug, Clone, Serialize)]
pub struct BatchUpdateOrderInfoFailure {
    #[serde(serialize_with = "serialize_i64")]
    pub part_id: i64,
    pub code: i32,
    pub message: String,
}
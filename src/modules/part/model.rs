//! part 域数据模型
//!
//! 对应 Python myERP/model/part.py。包含：
//! - sqlx `FromRow` 行结构（含 version 乐观锁、deleted_at 软删、created/updated 审计字段）
//!
//! Phase P1（送货分组）只需 FromRow 行结构以承载 sqlx 反序列化；
//! 域枚举（PartStatus / PartLocation / PartEventType 等）的 Rust enum 等到
//! part 域业务实现阶段再补，避免越权改动本域。
//!
//! Phase PR-CRUD 增量：
//! - TPart 25 列完整投影（含 NUMERIC 金额列 `unit_price` / `total_price`）
//! - TPartEvent / NewPartEvent：保持不变（已对齐 migration 010）
//!
//! 2026-09-27 part 域前后端字段对齐：
//! - TPart 加 `unit_price` / `total_price`（NUMERIC(12,2) / NUMERIC(14,2) NOT NULL
//!   DEFAULT 0）—— 通过 `rust_decimal::Decimal` + `serde-with-str` 序列化为
//!   JSON string，避免 JS `Number` 浮点丢精度
//! - TPart 序列化包含 `next_process_id`（用于 detail 端点 PartDetailOut），
//!   但 `PartListItem` 改显式列字段后不再含此字段（2026-09-27 review 第 1 轮
//!   修复：列表不返 + 详情仍返）。statemachine rollup / batch repo 派生链路
//!   完全不变

use chrono::NaiveDateTime;
use rust_decimal::Decimal;
use serde::Serialize;

use crate::shared::types::{serialize_i64, serialize_i64_opt};

/// `t_part` 完整行投影（Phase PR-CRUD 2026-08-25；2026-09-16 PR-2 瘦身至 23 列；
/// 2026-09-27 加金额列至 25 列）。
///
/// 25 列：含 NUMERIC 金额列 `unit_price` / `total_price`（NOT NULL DEFAULT 0，
/// 通过 `Decimal` + `serde-with-str` 序列化为 JSON string）。
///
/// 2026-09-16 PR-2 瘦身（migration 027）：删除 6 个批次依附列 ——
/// `actual_delivery_date` / `location` / `current_holder_id` / `placed_at` /
/// `delivery_note_id` / `has_been_repaired`。这些信息的真相源在 `t_part_batch`
/// 同名列（返修标除外，已整体废弃）；实际交付日期由 `t_part_event` 的
/// DELIVERED 事件派生。列表页位置 / 持有人展示由 service 层按
/// 「min-progress 活跃批次」派生（见 `PartListItem.location` / `holder_name`）。
///
/// `next_process_id` 保留：作 rollup 读缓存（PR-2 不动）；响应中序列化仍
/// 暴露给 `PartDetailOut`（detail 端点），但 `PartListItem` 改显式列字段后
/// 不再含此字段（2026-09-27 review 第 1 轮修复）。DB 列与 batch repo rollup
/// 派生链路完全不变。
///
/// 2026-09-16 增量（migration 026 FK 翻转）：+`process_chain_id`，
/// 前端「工序制定」列表按它是否为 NULL 区分已制定 / 未制定。
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct TPart {
    #[serde(serialize_with = "serialize_i64")]
    pub id: i64,
    pub serial_no: Option<String>,
    pub name: String,
    pub drawing_no: String,
    pub applicant_name: String,
    pub quantity: i32,
    pub request_date: chrono::NaiveDate,
    pub planned_delivery_date: chrono::NaiveDate,
    #[serde(serialize_with = "serialize_i64")]
    pub customer_id: i64,
    #[serde(serialize_with = "serialize_i64_opt")]
    pub assembly_id: Option<i64>,
    pub status: String,
    pub is_urgent: bool,
    /// 2026-09-27 review 第 1 轮修复：撤销 `#[serde(skip)]`，恢复序列化。
    /// detail 端点（`PartDetailOut`）依赖 TPart flatten → 仍需此字段；list
    /// 端点（`PartListItem`）改显式列字段后不含此字段。DB 列保留；
    /// statemachine rollup / batch repo 派生链路完全不变。
    #[serde(serialize_with = "serialize_i64_opt")]
    pub next_process_id: Option<i64>,
    pub order_no: Option<String>,
    pub system_delivery_date: Option<chrono::NaiveDate>,
    pub note: Option<String>,
    /// 2026-09-27 新增：单价（NUMERIC(12,2) NOT NULL DEFAULT 0）。`Decimal` +
    /// `serde-with-str` 自动序列化为 JSON string。
    #[serde(with = "rust_decimal::serde::str")]
    pub unit_price: Decimal,
    /// 2026-09-27 新增：总价（NUMERIC(14,2) NOT NULL DEFAULT 0）。`Decimal` +
    /// `serde-with-str` 自动序列化为 JSON string。
    #[serde(with = "rust_decimal::serde::str")]
    pub total_price: Decimal,
    pub version: i32,
    pub created_at: chrono::NaiveDateTime,
    #[serde(serialize_with = "serialize_i64_opt")]
    pub created_by: Option<i64>,
    pub updated_at: chrono::NaiveDateTime,
    #[serde(serialize_with = "serialize_i64_opt")]
    pub updated_by: Option<i64>,
    pub deleted_at: Option<chrono::NaiveDateTime>,
    /// 逻辑 FK → `t_part_process_chain.id`（migration 026 FK 翻转，2026-09-16）。
    /// `None` = 未制定工艺链；活跃 part 间 1:1（`uq_t_part_process_chain`）。
    #[serde(serialize_with = "serialize_i64_opt")]
    pub process_chain_id: Option<i64>,
}

/// `t_part` 行（to_ship 流专用最小投影）
///
/// 仅含 to_ship 路径与 `PartOut` 响应必需列。本结构服务于批量送检接口，
/// 重点暴露 `status` / `version` / `quantity` / `order_no` 等本流程必需字段。
/// 完整业务字段（`applicant_name` / `unit_price` / `total_price` 等）由
/// `TPart` 承载（`PartDetailOut` 用），本最小投影保持精简。
///
/// 2026-09-16 PR-2 瘦身（migration 027）：删 `actual_delivery_date` /
/// `current_holder_id`（t_part 列已删）。原「IN_PROCESS 组合校验」的 holder
/// 启发式改由 service 层读 min-progress 活跃批次的 `current_holder_id`
///（见 `inspection_core.rs::to_inspection_core`）。
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct TPartInspected {
    pub id: i64,
    pub serial_no: Option<String>,
    pub name: String,
    pub drawing_no: String,
    pub status: String,
    pub version: i32,
    pub quantity: i32,
    pub order_no: Option<String>,
    pub updated_at: NaiveDateTime,
    pub updated_by: Option<i64>,
}

/// `t_part_event` 行（`FromRow` 投影；实际写入走 `NewPartEvent<'a>` builder）。
///
/// 列对齐 migration 010（含 0018/0020 增列）。
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct TPartEvent {
    pub id: i64,
    pub part_id: i64,
    pub worker_id: Option<i64>,
    pub event_type: String,
    pub from_status: Option<String>,
    pub to_status: Option<String>,
    pub drawing_code: Option<String>,
    pub badge_code: Option<String>,
    pub note: Option<String>,
    pub created_at: NaiveDateTime,
    pub created_by: Option<i64>,
    pub outsource_company_id: Option<i64>,
    pub batch_id: Option<i64>,
    pub quantity: Option<i32>,
}

/// 新事件 builder —— 由 service 层在事务内构造，repo 负责 INSERT。
///
/// `id` 由调用方用 `SnowflakeIdGenerator::next_id()` 预生成；
/// `created_at` 走 DB 默认 `now()`（保持与其它写入路径一致）。
/// 其余可选字段在不需要时传 `None`。
pub struct NewPartEvent<'a> {
    pub id: i64,
    pub part_id: i64,
    /// 事件类型，如 `"STATUS_CHANGED"` / `"BATCH_PASSED"` 等。
    pub event_type: &'a str,
    pub from_status: Option<&'a str>,
    pub to_status: Option<&'a str>,
    pub batch_id: Option<i64>,
    pub quantity: Option<i32>,
    pub drawing_code: Option<&'a str>,
    pub badge_code: Option<&'a str>,
    pub note: Option<&'a str>,
    pub created_by: Option<i64>,
}

/// `t_part` rollup 派生列投影（PR-B2 `sync_from_batch_change` 用）。
///
/// 2 列：status / next_process_id。无 `Serialize`（service 内短暂使用）。
///
/// 2026-09-16 PR-2 瘦身（migration 027）：`location` / `current_holder_id` /
/// `placed_at` 列已从 t_part 删除，rollup 只物化 status + next_process_id
///（读缓存）；`version` 不参与 target==当前 比较，一并移出投影。
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct TPartRollupState {
    pub status: String,
    pub next_process_id: Option<i64>,
}

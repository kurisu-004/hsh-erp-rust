//! part 域数据模型
//!
//! 对应 Python myERP/model/part.py。包含：
//! - sqlx `FromRow` 行结构（含 version 乐观锁、deleted_at 软删、created/updated 审计字段）
//!
//! Phase P1（送货分组）只需 FromRow 行结构以承载 sqlx 反序列化；
//! 域枚举（PartStatus / PartLocation / PartEventType 等）的 Rust enum 等到
//! part 域业务实现阶段再补，避免越权改动本域。
//!
//! 完整列（含金额 / 数量 / holder / 日期 / note / has_been_repaired 等）待
//! part 域业务实施时再补全 —— 当前 `Cargo.toml` 还未挂 `rust_decimal` feature，
//! 而 `unit_price` / `total_price` 是 NUMERIC，缺 feature 时 sqlx 编译期拒收。

use chrono::{NaiveDate, NaiveDateTime};

/// `t_part` 行（Phase P1 投影）
///
/// 仅含 delivery_note / delivery_group 当前会用到的列：
/// 标识 + 序列号 + 客户 + 装配件 + 状态 + 送货单 + 乐观锁 + 软删。
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct TPart {
    pub id: i64,
    pub serial_no: Option<String>,
    pub name: String,
    pub drawing_no: String,
    pub customer_id: i64,
    pub assembly_id: Option<i64>,
    pub status: String,
    pub version: i32,
    pub created_at: NaiveDateTime,
    pub created_by: Option<i64>,
    pub updated_at: NaiveDateTime,
    pub updated_by: Option<i64>,
    pub deleted_at: Option<NaiveDateTime>,
    pub delivery_note_id: Option<i64>,
}

/// `t_part` 行（pass_inspection 流专用最小投影）
///
/// 仅含 pass_inspection 路径与 `PartOut` 响应必需列。完整业务字段
///（`applicant_name` / `unit_price` / `total_price` 等）待
/// `rust_decimal` 上线、part 域业务实施时再补全。
///
/// 与 `TPart`（Phase P1 投影）字段集不同：本结构服务于批量送检接口，
/// 重点暴露 `status` / `version` / `quantity` / `actual_delivery_date` /
/// `order_no` / `current_holder_id` 等本流程必需字段。
///
/// Phase F2（scan-inspect）：`current_holder_id` 用于「IN_PROCESS 组合校验」
/// 启发式（命中 `t_shelf` → shelf；否则 → worker）。
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
    pub actual_delivery_date: Option<NaiveDate>,
    pub current_holder_id: Option<i64>,
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

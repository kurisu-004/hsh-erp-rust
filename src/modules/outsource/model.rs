//! outsource 域数据模型（Phase 2 2026-09-13）
//!
//! 对应 Python myERP/model/outsource.py，包含：
//! - sqlx `FromRow` 行结构（含 version 乐观锁、deleted_at 软删、created/updated 审计字段）
//! - 域枚举（DB 用 varchar，应用层用 enum 校验）
//!
//! 与 part / customer 等域模型对齐：审计字段统一 `version` + 5 个时间戳列（AuditMixin）。

use chrono::NaiveDateTime;
use rust_decimal::Decimal;
use sqlx::FromRow;

/// t_outsource_company 行结构（DB schema 见 migration 004）。
#[derive(Debug, Clone, FromRow)]
pub struct TOutsourceCompany {
    pub id: i64,
    pub name: String,
    pub contact_name: Option<String>,
    pub contact_phone: Option<String>,
    pub address: Option<String>,
    pub is_active: bool,
    pub version: i32,
    pub created_at: NaiveDateTime,
    pub created_by: Option<i64>,
    pub updated_at: NaiveDateTime,
    pub updated_by: Option<i64>,
    pub deleted_at: Option<NaiveDateTime>,
}

/// t_outsource_company_process 行结构（junction：company ↔ process）。
#[derive(Debug, Clone, FromRow)]
pub struct TOutsourceCompanyProcess {
    pub id: i64,
    pub outsource_company_id: i64,
    pub process_id: i64,
    pub sort_order: i32,
    pub version: i32,
    pub created_at: NaiveDateTime,
    pub created_by: Option<i64>,
    pub updated_at: NaiveDateTime,
    pub updated_by: Option<i64>,
    pub deleted_at: Option<NaiveDateTime>,
}

/// t_outsource_quote 行结构。
#[derive(Debug, Clone, FromRow)]
pub struct TOutsourceQuote {
    pub id: i64,
    pub part_id: i64,
    pub outsource_company_id: i64,
    pub process_id: i64,
    /// 单件单价（CNY），DIRECT 自动创建时为 0。
    pub price: Decimal,
    pub note: Option<String>,
    pub status: String,
    pub submitted_at: Option<NaiveDateTime>,
    pub reviewed_at: Option<NaiveDateTime>,
    pub review_note: Option<String>,
    pub version: i32,
    pub created_at: NaiveDateTime,
    pub created_by: Option<i64>,
    pub updated_at: NaiveDateTime,
    pub updated_by: Option<i64>,
    pub deleted_at: Option<NaiveDateTime>,
}

/// t_outsource_quote_event 行结构（append-only）。
#[derive(Debug, Clone, FromRow)]
pub struct TOutsourceQuoteEvent {
    pub id: i64,
    pub quote_id: i64,
    pub event_type: String,
    pub from_status: Option<String>,
    pub to_status: Option<String>,
    pub note: Option<String>,
    pub created_by: Option<i64>,
    pub created_at: NaiveDateTime,
}

/// t_outsource_shipment 行结构。
#[derive(Debug, Clone, FromRow)]
pub struct TOutsourceShipment {
    pub id: i64,
    pub quote_id: i64,
    pub part_id: i64,
    pub batch_id: Option<i64>,
    pub outsource_company_id: i64,
    pub process_id: i64,
    pub quantity: i32,
    pub unit_price: Decimal,
    pub status: String,
    pub sent_at: NaiveDateTime,
    pub received_at: Option<NaiveDateTime>,
    pub is_billed: bool,
    pub version: i32,
    pub created_at: NaiveDateTime,
    pub created_by: Option<i64>,
    pub updated_at: NaiveDateTime,
    pub updated_by: Option<i64>,
    pub deleted_at: Option<NaiveDateTime>,
}

/// NewOutsourceCompanyInsert：service → repo 入参。
#[derive(Debug, Clone)]
pub struct NewOutsourceCompany {
    pub id: i64,
    pub name: String,
    pub contact_name: Option<String>,
    pub contact_phone: Option<String>,
    pub address: Option<String>,
    pub is_active: bool,
    pub created_by: i64,
}

/// NewOutsourceCompanyProcessInsert：junction 行入参。
#[derive(Debug, Clone)]
pub struct NewOutsourceCompanyProcess {
    pub id: i64,
    pub outsource_company_id: i64,
    pub process_id: i64,
    pub sort_order: i32,
    pub created_by: i64,
}

/// NewOutsourceQuoteInsert：service → repo 入参。
#[derive(Debug, Clone)]
pub struct NewOutsourceQuote {
    pub id: i64,
    pub part_id: i64,
    pub outsource_company_id: i64,
    pub process_id: i64,
    pub price: Decimal,
    pub note: Option<String>,
    pub created_by: i64,
}

/// NewOutsourceQuoteEventInsert：service → repo 入参。
#[derive(Debug, Clone)]
pub struct NewOutsourceQuoteEvent {
    pub id: i64,
    pub quote_id: i64,
    pub event_type: String,
    pub from_status: Option<String>,
    pub to_status: Option<String>,
    pub note: Option<String>,
    pub created_by: i64,
}

/// NewOutsourceShipmentInsert：service → repo 入参。
#[derive(Debug, Clone)]
pub struct NewOutsourceShipment {
    pub id: i64,
    pub quote_id: i64,
    pub part_id: i64,
    pub batch_id: Option<i64>,
    pub outsource_company_id: i64,
    pub process_id: i64,
    pub quantity: i32,
    pub unit_price: Decimal,
    pub created_by: i64,
}

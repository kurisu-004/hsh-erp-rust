//! assembly 域端点响应 VO

use chrono::{NaiveDate, NaiveDateTime};
use rust_decimal::Decimal;
use serde::Serialize;

use crate::shared::types::{serialize_i64, serialize_i64_opt};

/// 2026-09-16 PR-2 瘦身（migration 027）：t_assembly 删 `actual_delivery_date`
/// 列，DTO `AssemblyOut` 同步删该字段。实际交付日期由子件批次的
/// t_part_event DELIVERED 事件派生（前端按需另调 statistics 端点）。
///
/// 2026-09-17 PR-4 与 DDL 对齐：`request_date` / `planned_delivery_date`
/// 改为非 `Option<>`（与 `t_assembly` DDL NOT NULL 一致，model.rs 同步）。
/// 2026-09-22 PR4：迁移到 vo/，仅 Serialize。
#[derive(Debug, Clone, Serialize)]
pub struct AssemblyOut {
    #[serde(serialize_with = "serialize_i64")]
    pub id: i64,
    pub drawing_no: String,
    pub name: String,
    pub applicant_name: Option<String>,
    #[serde(serialize_with = "serialize_i64")]
    pub customer_id: i64,
    pub request_date: NaiveDate,
    pub planned_delivery_date: NaiveDate,
    pub is_urgent: bool,
    pub status: String,
    pub version: i32,
    pub serial_no: Option<String>,
    pub quantity: i32,
    pub unit_price: Option<Decimal>,
    pub total_price: Option<Decimal>,
    pub order_no: Option<String>,
    pub system_delivery_date: Option<NaiveDate>,
    pub note: Option<String>,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
}

/// 列表项：`#[serde(flatten)]` 把 `assembly` 字段的所有 key 平铺到顶层，
/// 再追加 `customer_name` / `parent_customer_name`。前端拿到的 JSON 形状与单条
/// `AssemblyOut` 几乎一致（仅多两个 customer 名字段）。
///
/// 2026-09-22 PR4：迁移到 vo/，仅 Serialize。
#[derive(Debug, Clone, Serialize)]
pub struct AssemblyListItem {
    #[serde(flatten)]
    pub assembly: AssemblyOut,
    pub customer_name: Option<String>,
    pub parent_customer_name: Option<String>,
}

/// 列表出参（items + total + limit + offset）。字段顺序对齐 Python
/// `schema/assembly.py::AssemblyListOut`，前端翻页需要 limit/offset 回显，
/// 故不复用 `shared::response::Page<T>`（只含 total + items）。
///
/// 2026-09-22 PR4：迁移到 vo/。
#[derive(Debug, Clone, Serialize)]
pub struct AssemblyListOut {
    pub items: Vec<AssemblyListItem>,
    pub total: i64,
    pub limit: i64,
    pub offset: i64,
}

/// 子件出参。`applicant_name` 在 t_part 是 NOT NULL VARCHAR(50)；其余可空。
///
/// §3.4（2026-09-11）— 子件继承自父件 / update 级联后的 6 个共享信息字段，
/// 由 service 层从 t_part 行透传。
///
/// 2026-09-14 Phase 3（deferred #7）— `current_batch_id` 子件当前激活批次 id；
/// 子件无活跃批次（如刚被 CANCELLED）时为 `None`。
///
/// 2026-09-22 PR4：迁移到 vo/。
#[derive(Debug, Clone, Serialize)]
pub struct AssemblyChildOut {
    #[serde(serialize_with = "serialize_i64")]
    pub id: i64,
    pub serial_no: Option<String>,
    pub name: String,
    pub drawing_no: Option<String>,
    pub status: String,
    pub version: i32,
    pub quantity: i32,
    pub planned_delivery_date: Option<NaiveDate>,
    pub applicant_name: String,
    pub request_date: NaiveDate,
    pub order_no: Option<String>,
    pub system_delivery_date: Option<NaiveDate>,
    pub is_urgent: bool,
    pub note: Option<String>,
    #[serde(serialize_with = "serialize_i64_opt")]
    pub current_batch_id: Option<i64>,
}

/// 关联文件出参（详情 / 上传响应）。不含下载 URL，前端用
/// `GET /api/v2/part-files/{id}/url` 单独取。
///
/// 2026-09-22 PR4：迁移到 vo/。
#[derive(Debug, Clone, Serialize)]
pub struct AssemblyFileRef {
    #[serde(serialize_with = "serialize_i64")]
    pub id: i64,
    pub original_filename: String,
    pub page_count: Option<i32>,
}

/// 详情出参：`#[serde(flatten)]` 把 `assembly` 字段所有 key 平铺到顶层，
/// 再追加 `children` / `files` 两个数组。
///
/// 2026-09-22 PR4：迁移到 vo/。
#[derive(Debug, Clone, Serialize)]
pub struct AssemblyDetail {
    #[serde(flatten)]
    pub assembly: AssemblyOut,
    pub children: Vec<AssemblyChildOut>,
    pub files: Vec<AssemblyFileRef>,
}

/// 创建结果出参：父件 + 新派生的子件列表（POST 201 响应专用）。
///
/// 2026-09-22 PR4：迁移到 vo/。
#[derive(Debug, Clone, Serialize)]
pub struct AssemblyCreateResult {
    pub assembly: AssemblyOut,
    pub created_children: Vec<AssemblyChildOut>,
}

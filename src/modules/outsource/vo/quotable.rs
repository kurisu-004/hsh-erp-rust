//! outsource 域 `quotable-parts` 端点响应 VO
//!
//! 2026-10-03 新增：外协报价 picker 的读侧契约。一行 = 一个
//! （零件 × OUTSOURCE 工序）组合，同一组合只出一行（见 repo SQL 的
//! `DISTINCT ON (p.id, pr.id)`）。
//!
//! ## 为什么单独一套 VO 而不是复用 `PartListItem`
//! `PartListItem` 是 part 域通用列表投影，**刻意不声明 `next_process_id`**
//! （2026-09-27 决策：part list 响应不暴露派生工序）。而报价 picker 必须拿到
//! `next_process_id` 才能自动填工序 —— 复用一个「字段缺省」的 VO 只会把
//! 前端逼进临时 cast。本 VO 显式声明该字段。
//!
//! 本文件在 `vo/mod.rs` 的 re-export 里是 `quotable`；与 `quote.rs`（报价
//! lifecycle 出参）刻意分文件，避免 2 个语义不同的读模型混在一个文件里。

use serde::Serialize;

use crate::shared::types::serialize_i64;

/// `GET /outsource-quotes/quotable-parts` 分页信封。
#[derive(Debug, Clone, Serialize)]
pub struct QuotablePartListOut {
    pub items: Vec<QuotablePartOut>,
    pub total: i64,
    pub limit: i64,
    pub offset: i64,
}

/// 可建外协报价的（零件 × OUTSOURCE 工序）组合行。
#[derive(Debug, Clone, Serialize)]
pub struct QuotablePartOut {
    /// `t_part.id`。
    #[serde(serialize_with = "serialize_i64")]
    pub id: i64,
    pub serial_no: Option<String>,
    pub drawing_no: String,
    pub name: String,
    pub is_urgent: bool,
    /// `t_part.unit_price` 的 Decimal 字符串（零件下单单价，供与报价对比）。
    pub unit_price: String,
    #[serde(serialize_with = "serialize_i64")]
    pub customer_id: i64,
    pub customer_name: Option<String>,
    /// L1（`t_customer.parent_id`）名称。
    pub l1_customer_name: Option<String>,
    /// 客户路径：有 L1 拼 `L1 / L2`，否则仅 L2 名，缺客户为 `null`。
    pub customer_path: Option<String>,
    /// 批次所在货架（绑定了该 OUTSOURCE 工序的货架）。
    #[serde(serialize_with = "serialize_i64")]
    pub shelf_id: i64,
    pub shelf_code: String,
    /// 该 OUTSOURCE 工序（**前端靠这个字段自动填报价工序**）。
    #[serde(serialize_with = "serialize_i64")]
    pub next_process_id: i64,
    pub next_process_name: String,
}

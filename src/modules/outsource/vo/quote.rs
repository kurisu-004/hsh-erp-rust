//! outsource 域 quote 端点响应 VO（2026-09-22 PR4 重构）

use chrono::NaiveDateTime;
use serde::Serialize;

use crate::shared::types::serialize_i64;

/// 外协报价详情出参（含 part / company / process / customer 名称补全）。
#[derive(Debug, Clone, Serialize)]
pub struct OutsourceQuoteOut {
    #[serde(serialize_with = "serialize_i64")]
    pub id: i64,
    pub version: i32,
    #[serde(serialize_with = "serialize_i64")]
    pub part_id: i64,
    #[serde(serialize_with = "serialize_i64")]
    pub outsource_company_id: i64,
    #[serde(serialize_with = "serialize_i64")]
    pub process_id: i64,
    pub price: String,
    pub note: Option<String>,
    pub status: String,
    pub submitted_at: Option<NaiveDateTime>,
    pub reviewed_at: Option<NaiveDateTime>,
    pub review_note: Option<String>,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
    // 展示用补全字段（service 拼装）
    pub part_serial_no: Option<String>,
    pub part_drawing_no: Option<String>,
    pub part_name: Option<String>,
    pub outsource_company_name: Option<String>,
    pub process_code: Option<String>,
    pub process_name: Option<String>,
    /// 客户路径（第 2 层到叶子用 `" / "` 拼接，**不含**根；本节点即根时 = 根自己的名字）。
    ///
    /// 2026-10-17 由 `Option<String>` 改为**必填** `String`，且数据源由「service 层把
    /// L1 名 / L2 名两段拼起来」改成 [`crate::shared::customer::paths_of`] 的
    /// `CustomerPath::path`：客户树放开任意深度后两段拼接既不是全路径、也不是产品口径。
    /// 客户行缺失 / 已软删时为空串（前端按「空 = 无客户信息」展示）。
    pub customer_path: String,
    pub part_unit_price: Option<String>,
    pub is_urgent: bool,
}

/// 报价列表出参。
#[derive(Debug, Clone, Serialize)]
pub struct OutsourceQuoteListOut {
    pub items: Vec<OutsourceQuoteOut>,
    pub total: i64,
    pub limit: i64,
    pub offset: i64,
}

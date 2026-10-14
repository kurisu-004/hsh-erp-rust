//! outsource 域 `quotable-parts` 端点响应 VO
//!
//! 2026-10-03 新增：外协报价 picker 的读侧契约。同日简化为**一行 = 一个还没下发
//! 的零件**（该零件存在 `PENDING` 批次），见 `repo/sql.rs::OutsourceQuotableRepo`
//! 的头注释。
//!
//! ## 为什么单独一套 VO 而不是复用 `PartListItem`
//! 2026-09-27 决策：`PartListItem` 刻意不声明任何派生工序字段（part list 响应不
//! 暴露派生工序），而本 VO 要带 `unit_price` / `is_urgent` / 客户路径供 picker 列表
//! 直接渲染。2026-10-03 简化行粒度后本 VO 与 `PartListItem` 的差异只剩这几个字段，
//! 仍不值得为它引入一层临时 cast。
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

/// 可建外协报价的一个零件行。
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
    /// 本节点单段客户名
    pub customer_name: Option<String>,
    /// **根**（L1 集团）客户名 —— 2026-10-17 起由 `shared::customer::paths_of` 补齐，
    /// 不再是「上跳一层」的直接父
    pub l1_customer_name: Option<String>,
    /// 客户路径（第 2 层到叶子用 `" / "` 拼接，**不含**根；本节点即根时 = 根自己的名字）。
    ///
    /// 2026-10-17 由 `Option<String>` 改为**必填** `String`，且数据源由「service 层把
    /// L1 名 / L2 名两段拼起来」改成 [`crate::shared::customer::paths_of`] 的
    /// `CustomerPath::path`：客户树放开任意深度后两段拼接既不是全路径、也不是产品口径。
    /// 客户行缺失 / 已软删时为空串（前端按「空 = 无客户信息」展示）。
    pub customer_path: String,
}

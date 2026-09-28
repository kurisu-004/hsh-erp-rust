//! 微信小程序 BFF 模块（2026-09-28 新增）VO 响应层
//!
//! 这些 DTO 是为 mini-program 卡片视图量身定制的「瘦」结构，**不**复用
//! `/api/v2/part/*` / `/api/v2/iam/*` 端点的全字段响应：
//!
//! - 没有 `version` / `created_at` / `updated_at` / `created_by` 等审计字段
//! - 没有 children 嵌套 / 元数据冗余
//! - 雪花 ID 仍走 `serialize_i64` 序列化为 JSON string（与全栈契约一致）
//!
//! 设计目标：单个 HTTP 响应 < 4KB，便于微信小程序首屏秒开；list 端点
//! 默认 page=1 size=10（不开放大页，避免一次拉太多批次）。

use chrono::NaiveDate;
use serde::Serialize;

use crate::modules::iam::vo::CurrentUserOut;
use crate::modules::part::statemachine::PartStatus;
use crate::shared::types::{serialize_i64, serialize_i64_opt};

// =============================================================================
// Status 枚举（mini-program 视图层语义）
// =============================================================================

/// 批次状态字符串形态（t_part_batch.status 取值：PENDING / IN_PROCESS /
/// INSPECTION / READY_TO_SHIP / DELIVERED / REPAIRING / OUTSOURCE /
/// COMPLETED / CANCELLED）。
///
/// 暂用 `String` 不抽 enum：与 part/vo/part_batch.rs 同形，handler / DTO 不做
/// 反序列化（只 SQL → DTO 单向），无需 typed enum。如未来 mini-program 改严
/// 字面量类型校验，再引入枚举一并收紧。
pub type BatchStatus = String;

/// 工单类型（用于 mini-program 卡片分组：按 part 自身的 kind 维度）。
///
/// 当前 mini-program 不持久化该字段（前端从 drawing_no 前缀推断）；
/// 但后端响应里加这个字段便于「组装件子件 vs 普通工单」分类展示。
#[allow(non_camel_case_types)]
#[derive(Debug, Clone, Copy, Serialize)]
pub enum WxPartKind {
    #[serde(rename = "workOrder")]
    WorkOrder,
    #[serde(rename = "batch")]
    Batch,
}

// =============================================================================
// Counts / 计数
// =============================================================================

/// 工单状态计数（mini-program 首页 4 个 tab + 全部）。
///
/// 设计取舍：4 个 tab（pending_production / in_production / pending_inspection /
/// delivered）按"用户视角"映射（工单创建 → 待投产 → 生产中 → 待检 → 出货）；
/// `PROGRAMMING` / `OUTSOURCE` / `REPAIRING` / `COMPLETED` / `CANCELLED` 仅计
/// 入 `all`，不单独 tab 化。
///
/// `delivered` = `READY_TO_SHIP + DELIVERED`（"已完工可出货"二合一），与
/// 现有 dashboard 域 `item.shelf_code` 判定的"待出"口径一致。
#[derive(Debug, Clone, Serialize)]
pub struct CountsByStatus {
    pub all: i64,
    pub pending_production: i64,
    pub in_production: i64,
    pub pending_inspection: i64,
    pub delivered: i64,
}

/// 批次计数（mini-program 批次页 2 个 tab：进行中 / 已完工）。
#[derive(Debug, Clone, Serialize)]
pub struct BatchCounts {
    pub in_progress: i64,
    pub done: i64,
}

/// 当月工人工作统计。
///
/// `batch_count` = 该工人在该月发生过事件的不同 batch_id 数；
/// `work_hours` = 关联 quantity 求和（mini-program 用作工作量估算，未做
/// 严格的工时统计——DB schema 无 work_hours 列；参见
/// `src/modules/statistics/repo/sql.rs` 的同形聚合）。
#[derive(Debug, Clone, Serialize)]
pub struct MonthlyStats {
    pub batch_count: i64,
    pub work_hours: f64,
}

// =============================================================================
// 首页聚合
// =============================================================================

/// mini-program 首页聚合响应（一次 HTTP 拉全部卡片 + 计数 + 用户视图）。
#[derive(Debug, Clone, Serialize)]
pub struct HomeDashboard {
    pub me: CurrentUserOut,
    pub part_counts: CountsByStatus,
    pub batch_counts: BatchCounts,
    pub today_picked: i64,
    pub today_delivered: i64,
}

// =============================================================================
// 工单 / 批次 卡片视图
// =============================================================================

/// 工单卡片列表行（mini-program「工单列表」用）。
///
/// 字段比 `part::vo::PartListItem` 少：无 `version` / `applicant_name` /
/// `unit_price` / `total_price` / `process_chain_id` / 审计字段；保留的
/// 字段全为卡片必展示项或筛选条件。
#[derive(Debug, Clone, Serialize)]
pub struct WxPartSummary {
    #[serde(serialize_with = "serialize_i64")]
    pub id: i64,
    pub serial_no: Option<String>,
    pub name: String,
    pub drawing_no: String,
    pub quantity: i32,
    pub status: PartStatus,
    pub is_urgent: bool,
    pub planned_delivery_date: NaiveDate,
    pub customer_name: Option<String>,
    /// 当前批次 id（mini-program 跳转批次详情用）。
    #[serde(serialize_with = "serialize_i64_opt")]
    pub current_batch_id: Option<i64>,
    pub current_batch_no: Option<i32>,
    /// 卡片底部"当前持有方"展示文字（货架 code / 工人姓名 / 外协公司名 /
    /// 其它），由后端按 `batch.location` 分桶 JOIN 解析。
    pub current_holder_label: Option<String>,
    pub kind: WxPartKind,
}

/// 批次卡片列表行（mini-program「批次列表」用）。
///
/// 比 `part::vo::PartBatchListItemOut` 少：无 `version` / `parent_batch_id` /
/// `delivery_note_id`。
///
/// 多：`assigned_to`（mini-program 工人在做谁手里视图） + `work_hours`（当前
/// 批次累计）+ `finished_date` / `due_date` / `drawing_url`（草稿卡片渲染）。
#[derive(Debug, Clone, Serialize)]
pub struct WxBatchSummary {
    #[serde(serialize_with = "serialize_i64")]
    pub id: i64,
    #[serde(serialize_with = "serialize_i64")]
    pub part_id: i64,
    pub serial_no: Option<String>,
    pub name: String,
    pub drawing_no: String,
    pub batch_no: i32,
    pub quantity: i32,
    pub status: BatchStatus,
    pub assigned_to: Option<String>,
    pub work_hours: Option<f64>,
    pub finished_date: Option<NaiveDate>,
    pub due_date: Option<NaiveDate>,
    pub drawing_url: Option<String>,
}

// =============================================================================
// 分页
// =============================================================================

/// 列表响应统一外壳：items + total + page + size + has_more。
///
/// mini-program 端要做"加载更多"分页，自带 `page` / `size` 字段便于客户端
/// 校验分页完整性；`has_more` 由后端按 `total > page * size` 计算，前端
/// `useInfiniteQuery` 直接据此决定是否触发下一页（不再自己算 total/page/size
/// 比值）。
#[derive(Debug, Clone, Serialize)]
pub struct WxPage<T> {
    pub items: Vec<T>,
    pub total: i64,
    pub page: i64,
    pub size: i64,
    pub has_more: bool,
}

impl<T> WxPage<T> {
    /// 构造分页响应，自动计算 `has_more = total > page * size`。
    ///
    /// 调用方只需传 `(items, total, page, size)`，无需手动算 has_more，避免
    /// 多处漏算 / 算错（2026-09-28 review #1 修复：前端 useInfiniteQuery 依赖）。
    pub fn new(items: Vec<T>, total: i64, page: i64, size: i64) -> Self {
        let has_more = total > page * size;
        Self {
            items,
            total,
            page,
            size,
            has_more,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn has_more_true_when_more_pages_exist() {
        let p: WxPage<i32> = WxPage::new(vec![1, 2, 3], 25, 1, 10);
        assert!(p.has_more);
    }

    #[test]
    fn has_more_false_on_last_page() {
        let p: WxPage<i32> = WxPage::new(vec![1, 2, 3], 13, 2, 10);
        assert!(!p.has_more);
    }

    #[test]
    fn has_more_false_when_empty() {
        let p: WxPage<i32> = WxPage::new(vec![], 0, 1, 10);
        assert!(!p.has_more);
    }
}

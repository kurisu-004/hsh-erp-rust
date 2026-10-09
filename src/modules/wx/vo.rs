//! 微信小程序 BFF 模块响应层 —— **B3 过渡期：只剩 batch / worker**
//!
//! 2026-10-11 重构：part 相关的 VO（`CountsByStatus` / `WxPartSummary` /
//! `WxPartKind`）与 login 用的 `CurrentUserOut` 跨域复用**已全部删除**：
//!
//! | 已删 | 新位置 |
//! |---|---|
//! | `CountsByStatus` | [`super::part_list::vo::PartCountsOut`]（camelCase） |
//! | `WxPartSummary` / `WxPartKind` | [`super::part_list::vo::PartCardOut`] 判别联合（camelCase） |
//! | `HomeDashboard` | **删除**（`/wx/dashboard/home` 整域下线） |
//! | `iam::vo::CurrentUserOut` 的跨域复用 | **删除**（login 改用自己的 `WxLoginOut`，dashboard 已下线） |
//!
//! ⚠️ **B3（2026-10-11 之后接手）会把本文件剩余内容整体搬进
//! `wx::production::vo`**（配合 `/wx/batches/*` + `/wx/worker/*` →
//! `/wx/production/*` 的 URL 硬切）。
//!
//! ## 这些 DTO 是为 mini-program 卡片视图量身定制的「瘦」结构，**不**复用
//! `/api/v2/part/*` / `/api/v2/iam/*` 端点的全字段响应：
//!
//! - 没有 `version` / `created_at` / `updated_at` / `created_by` 等审计字段
//! - 没有 children 嵌套 / 元数据冗余
//! - 雪花 ID 仍走 `serialize_i64` 序列化为 JSON string（与全栈契约一致）
//!
//! ⚠️ **字段名仍是 snake_case**：`wx::part_list` 的卡片 VO 已切成 camelCase
//! （逐字对齐前端卡片模型），但 `batches` / `worker` 的 VO 本轮**刻意不动** —— 它们
//! 的前端映射层（`services/production.ts`）逐字读 `serial_no` / `batch_no` 等键，
//! 改名即打断契约。B3 若要统一口径，请把前端映射层一并纳入改动范围。

use chrono::NaiveDate;
use serde::Serialize;

use crate::shared::types::serialize_i64;

// =============================================================================
// Status 枚举（mini-program 视图层语义）
// =============================================================================

/// 批次状态字符串形态（t_part_batch.status 取值：PENDING / IN_PROCESS /
/// INSPECTION / READY_TO_SHIP / DELIVERED / OUTSOURCE /
/// COMPLETED / CANCELLED）。
///
/// 2026-10-01：删掉 `REPAIRING` —— REPAIRING 已降级为
/// `t_part_batch.is_repairing` 标记列（migration 005/006），DB 层不再产生该
/// status，返修中的批次 status 就是 `IN_PROCESS`。
///
/// 暂用 `String` 不抽 enum：与 part/vo/part_batch.rs 同形，handler / DTO 不做
/// 反序列化（只 SQL → DTO 单向），无需 typed enum。如未来 mini-program 改严
/// 字面量类型校验，再引入枚举一并收紧。
pub type BatchStatus = String;

// =============================================================================
// Counts / 计数
// =============================================================================

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
// 批次 卡片视图
// =============================================================================

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
///
/// ⚠️ 本外壳**只服务** `/wx/batches/*`（B3 目标态：移进 `wx::production::vo`）。
/// `/wx/part-list/*` **不用**它 —— 那个域的响应只有 `list` + `hasMore`，没有
/// `total` / `page` / `size`（前端不读）。
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

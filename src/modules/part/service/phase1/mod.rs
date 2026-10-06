//! part 域 Phase 1 只读端点 + 批量创建增强的业务逻辑。
//!
//! 2026-10-02：Phase 1 的**写端点**（上架 / 召回 / 编程出口 / 外协收发 / 返修闭环 /
//! 拆批 / 取消 / pick-up / 扫码 / worker-scan）与它们共用的 9 个状态机守卫自由函数
//! 迁往 `crate::modules::prod::batch::service`（它们的操作对象是批次）。本模块
//! 只剩读端点与「一次操作一批 part」的批量创建增强。
//!
//! 2026-10-03：`outsource.rs`（外协系列只读）**整文件删除** —— 它服务的
//! `/parts/outsource-in-flight` / `/parts/outsource-sendable` 两条路由返回通用
//! `PartListItem`，与前端外协域字段需求不匹配。取代者迁往 outsource 域。
//!
//! - `events.rs` 1.6 事件历史 + 位置树 + 1.8 批量创建增强（list_events /
//!   location_tree / batch_with_pdfs / batch_update_order_info）
//! - `excel_match.rs` 采购订单 Excel 匹配（2026-10-06 新增，从 `events.rs` 迁出；
//!   含分档决策纯函数 `resolve_match_tier` + 匹配索引 `ExcelMatchIndex`）
//! - `lifecycle_helpers.rs` 批次列表（list_batches）
//! - `work_type.rs` 工种维度只读（list_by_work_type / list_pickable_by_work_type /
//!   list_by_worker）
//!
//! 状态机扩展见 `part/statemachine.rs`（2026-09-29 缩至 19 个合法迁移）。
//! 错误码全部沿用 `shared/error.rs::code` 已声明常量（201xx / 205xx）。

#![allow(deprecated, clippy::too_many_arguments, clippy::type_complexity)]

pub mod events;
pub mod excel_match;
pub mod lifecycle_helpers;
pub mod work_type;

/// 外协公司精简投影（outsource 域 Phase 2 stub 期间绕过 OutsourceCompanyRepo）。
struct OutsourceLite {
    id: i64,
    name: String,
    is_active: bool,
}

// ===== Row helpers for non-macro sqlx queries =====
// These structs implement `sqlx::FromRow` manually so we can use runtime
// `sqlx::query_as::<_, Row>(...)` instead of `sqlx::query_as!` (which requires
// compile-time DB access via .sqlx cache).

#[derive(sqlx::FromRow)]
#[allow(dead_code)] // current_process_step_id: 通过 service 层需要，但本 struct 仅 DTO 转换使用
struct BatchListRow {
    id: i64,
    /// 2026-09-30 新增（来自 b.part_id，对齐 PartBatchListItemOut 新字段）
    part_id: i64,
    batch_no: i32,
    quantity: i32,
    status: String,
    /// 2026-10-01 review 第 1 轮 M5 新增（migration 005）：返修标记随列表投出
    is_repairing: bool,
    location: Option<String>,
    version: i32,
    /// 2026-09-30 新增（来自 b.created_at，对齐 PartBatchListItemOut 新字段）
    created_at: chrono::NaiveDateTime,
    /// 2026-09-30 新增（来自 b.updated_at，对齐 PartBatchListItemOut 新字段）
    updated_at: chrono::NaiveDateTime,
    current_process_step_id: Option<i64>,
    parent_batch_id: Option<i64>,
    current_holder_id: Option<i64>,
    /// 2026-09-30 重命名（原 `holder_name`）—— 与 SQL alias `current_holder_display` 对齐
    current_holder_display: Option<String>,
    next_process_id: Option<i64>,
    /// 2026-09-30 新增（来自 LEFT JOIN t_process p2）
    next_process_name: Option<String>,
    /// 2026-09-30 新增（来自 LEFT JOIN t_delivery_note dn）
    delivery_note_no: Option<String>,
    delivery_note_id: Option<i64>,
}

#[derive(sqlx::FromRow)]
struct EventListRow {
    id: i64,
    event_type: String,
    from_status: Option<String>,
    to_status: Option<String>,
    batch_id: Option<i64>,
    quantity: Option<i32>,
    drawing_code: Option<String>,
    badge_code: Option<String>,
    note: Option<String>,
    created_at: chrono::NaiveDateTime,
    created_by: Option<i64>,
}

#[derive(sqlx::FromRow)]
struct HolderCountRow {
    holder_id: i64,
    n: i64,
}

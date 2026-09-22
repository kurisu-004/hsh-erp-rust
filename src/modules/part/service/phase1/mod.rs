//! part 域 Phase 1（2026-09-13）业务逻辑：补齐 14 端点。
//!
//! 2026-09-22 D-6 重构：原 `phase1.rs`（3084 行超限）按业务动作拆为子模块：
//! - `lifecycle_helpers` 1.1 上架 / 召回 + 1.5 批次列表（place_on_shelf /
//!   recall_to_pending / list_pending_programming / list_batches）
//! - `programming` 1.2 CNC 编程流转（send_to_programming /
//!   release_from_programming / recall_to_programming）
//! - `outsource` 1.3 外协流转（send_to_outsource / receive_from_outsource /
//!   receive_from_outsource_to_inspection / list_outsource_in_flight /
//!   list_outsource_sendable）
//! - `repair` 1.4 返修闭环（complete_repair / repair_dispatch /
//!   list_repair_batches / list_repairing_batches + 共享 list_batches_with_status）
//! - `batch_ops` 1.5 批次拆分 / 取消（split_batch / cancel_batch）
//! - `scan` 1.7 扫码检 / 司机扫码（scan_inspect / scan_deliver_part）
//! - `events` 1.6 事件历史 + 位置树 + 1.8 批量创建增强（list_events /
//!   location_tree / batch_update_order_info / match_by_excel_items /
//!   batch_with_pdfs）
//! - `work_type` Phase 2 (2026-09-13) 领取链路（pick_up / list_by_work_type /
//!   list_pickable_by_work_type / list_by_worker）
//!
//! - 1.1 上架 / 召回（`place_on_shelf` / `recall_to_pending`）
//! - 1.2 CNC 编程流转（`send_to_programming` / `release_from_programming` /
//!   `recall_to_programming` / `pending_programming`）
//! - 1.3 外协流转（`send_to_outsource` / `receive_from_outsource` /
//!   `receive_from_outsource_to_inspection` / `outsource_in_flight` /
//!   `outsource_sendable`）
//! - 1.4 返修闭环（`complete_repair` / `repair_dispatch` /
//!   `repair_batches` / `repairing_batches`）
//! - 1.5 批次拆分 / 取消（`split_batch` / `cancel_batch` / `list_batches`）
//! - 1.6 事件历史 + 位置树（`list_events` / `location_tree`）
//! - 1.7 扫码检 / 司机扫码（`scan_inspect` / `scan_deliver_part`）
//! - 1.8 批量创建增强（`batch_with_pdfs` / `match_by_excel_items` /
//!   `batch_update_order_info`）
//!
//! 状态机扩展见 `part/statemachine.rs`（共 21 个合法迁移）。
//! 错误码全部沿用 `shared/error.rs::code` 已声明常量（201xx / 205xx）。
//!
//! ## 批次守恒不变量
//! 拆分时 `Σ(未删批次.quantity) = t_part.quantity` 必须保持；
//! 由 `PartBatchRepo::split_batch` 强制（同一事务内连发 max+1 / INSERT / UPDATE 三条 SQL，
//! OCC 守源批次），handler 层再加 `BIZ_PART_BATCH_INVALID_QUANTITY` 防御性校验。

#![allow(deprecated, clippy::too_many_arguments, clippy::type_complexity)]

use sqlx::{PgConnection, PgExecutor};

use crate::modules::part::statemachine::PartStatus;
use crate::modules::shelf::repo::ShelfRepo;
use crate::shared::error::{AppError, code};

pub mod batch_ops;
pub mod events;
pub mod lifecycle_helpers;
pub mod outsource;
pub mod programming;
pub mod repair;
pub mod scan;
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
    batch_no: i32,
    quantity: i32,
    status: String,
    location: Option<String>,
    version: i32,
    current_process_step_id: Option<i64>,
    parent_batch_id: Option<i64>,
    current_holder_id: Option<i64>,
    holder_name: Option<String>,
    next_process_id: Option<i64>,
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
struct InspectionRepairRow {
    batch_id: i64,
    part_id: i64,
    batch_no: i32,
    quantity: i32,
    status: String,
    location: Option<String>,
    version: i32,
    current_process_step_id: Option<i64>,
    parent_batch_id: Option<i64>,
    current_holder_id: Option<i64>,
    holder_name: Option<String>,
    next_process_id: Option<i64>,
    next_process_name: Option<String>,
    delivery_note_id: Option<i64>,
    delivery_note_no: Option<String>,
    serial_no: Option<String>,
    drawing_no: String,
    name: String,
    order_no: Option<String>,
    planned_delivery_date: chrono::NaiveDate,
    is_urgent: bool,
    part_version: i32,
    created_at: chrono::NaiveDateTime,
    updated_at: chrono::NaiveDateTime,
    customer_id: i64,
    customer_name: Option<String>,
    l1_customer_name: Option<String>,
}

#[derive(sqlx::FromRow)]
struct HolderCountRow {
    holder_id: i64,
    n: i64,
}

/// 状态机迁移守卫 + 错误码映射（在 phase1.rs 内复用）：
/// - 起点状态非法 → 20103 `BIZ_INVALID_TRANSITION`
/// - 起点已是终态 → 20115 `BIZ_PART_ALREADY_CANCELLED`（仅 cancel 路径）
#[inline]
fn ensure_transition(from: PartStatus, to: PartStatus, ctx: &str) -> Result<(), AppError> {
    if from == PartStatus::CANCELLED {
        return Err(AppError::biz(
            code::BIZ_PART_ALREADY_CANCELLED,
            format!("{ctx}: 工单已 CANCELLED"),
        ));
    }
    if !from.can_transition_to(to) {
        return Err(AppError::biz(
            code::BIZ_INVALID_TRANSITION,
            format!("{ctx}: {} → {} 不允许", from.as_str(), to.as_str()),
        ));
    }
    Ok(())
}

/// 校验 batch 属于 part + 锚定 `version`（OCC）。
#[inline]
fn validate_batch_ownership(
    batch_part_id: i64,
    batch_id: i64,
    expected_part_id: i64,
    expected_version: i32,
    actual_version: i32,
) -> Result<(), AppError> {
    if batch_part_id != expected_part_id {
        return Err(AppError::biz(
            code::BIZ_PART_BATCH_NOT_FOUND,
            format!("batch {batch_id} 不属于 part {expected_part_id}"),
        ));
    }
    if batch_version_mismatch(batch_id, expected_version, actual_version) {
        return Err(AppError::biz(
            code::VERSION_CONFLICT,
            format!("batch {batch_id} 版本冲突（期望 {expected_version}，实际 {actual_version}）"),
        ));
    }
    Ok(())
}

#[inline]
fn batch_version_mismatch(_batch_id: i64, expected: i32, actual: i32) -> bool {
    expected != actual
}

/// 校验 shelf 存在 + active + zone 一致。
async fn validate_shelf_zone(
    conn: &mut PgConnection,
    shelf_id: i64,
    expected_zone: &str,
) -> Result<(), AppError> {
    let shelf = ShelfRepo::get_by_id(&mut *conn, shelf_id)
        .await?
        .ok_or_else(|| {
            AppError::biz(
                code::BIZ_SHELF_NOT_FOUND,
                format!("shelf {shelf_id} 不存在"),
            )
        })?;
    if !shelf.is_active {
        return Err(AppError::biz(
            code::BIZ_SHELF_INACTIVE,
            format!("shelf {} (id={}) 已停用", shelf.code, shelf.id),
        ));
    }
    if shelf.zone != expected_zone {
        return Err(AppError::biz(
            code::BIZ_INVALID_VALUE,
            format!(
                "shelf {} (id={}) zone={} 不等于 {expected_zone}",
                shelf.code, shelf.id, shelf.zone
            ),
        ));
    }
    Ok(())
}

/// 校验 shelf ↔ process 映射（`_assert_shelf_maps_process`）：必须存在
/// `t_shelf_process` 映射行，否则 20507 `BIZ_SHELF_PROCESS_NOT_MAPPED`。
async fn assert_shelf_maps_process(
    conn: &mut PgConnection,
    shelf_id: i64,
    process_id: i64,
) -> Result<(), AppError> {
    let exists: Option<(i64,)> = sqlx::query_as(
        "SELECT id FROM t_shelf_process WHERE shelf_id = $1 AND process_id = $2 \
         AND deleted_at IS NULL LIMIT 1",
    )
    .bind(shelf_id)
    .bind(process_id)
    .fetch_optional(&mut *conn)
    .await?;
    if exists.is_none() {
        return Err(AppError::biz(
            code::BIZ_SHELF_PROCESS_NOT_MAPPED,
            format!("shelf {shelf_id} 未映射 process {process_id}"),
        ));
    }
    Ok(())
}

/// 2026-09-16 PR-3 批次 step 化：
/// - 删 `placed_at` 列写入（COALESCE(placed_at, now()) 已无意义）
/// - `next_process_id: Option<i64>` → `current_process_step_id: Option<i64>`
///   （写入 t_part_batch.current_process_step_id 新列）
/// - `new_next_process_id` 参数改名为 `new_current_process_step_id`
///   （DTO / worker / frontend 仍传 process_id，由 caller 在调本函数前
///   经 `ProcessChainRepo::resolve_step_id_by_process` 解析）
#[allow(clippy::too_many_arguments)]
async fn mark_batch_with_status_and_meta<'e, E: PgExecutor<'e>>(
    executor: E,
    batch_id: i64,
    expected_version: i32,
    new_status: &str,
    new_location: Option<&str>,
    new_holder_id: Option<i64>,
    new_current_process_step_id: Option<i64>,
    updated_by: i64,
) -> Result<u64, sqlx::Error> {
    let r = sqlx::query(
        "UPDATE t_part_batch SET status = $3, location = $4, current_holder_id = $5, \
         current_process_step_id = $6, \
         version = version + 1, updated_at = now(), updated_by = $7 \
         WHERE id = $1 AND version = $2 AND status NOT IN ('CANCELLED', 'COMPLETED') \
         AND deleted_at IS NULL",
    )
    .bind(batch_id)
    .bind(expected_version)
    .bind(new_status)
    .bind(new_location)
    .bind(new_holder_id)
    .bind(new_current_process_step_id)
    .bind(updated_by)
    .execute(executor)
    .await?;
    Ok(r.rows_affected())
}

/// mark_batch 的轻量版本（不写 location/holder/process；用于状态机迁移但保持原 holder 的场景，如 CANCELLED）。
async fn mark_batch_status_only<'e, E: PgExecutor<'e>>(
    executor: E,
    batch_id: i64,
    expected_version: i32,
    new_status: &str,
    updated_by: i64,
) -> Result<u64, sqlx::Error> {
    let r = sqlx::query(
        "UPDATE t_part_batch SET status = $3, version = version + 1, \
         updated_at = now(), updated_by = $4 \
         WHERE id = $1 AND version = $2 AND deleted_at IS NULL",
    )
    .bind(batch_id)
    .bind(expected_version)
    .bind(new_status)
    .bind(updated_by)
    .execute(executor)
    .await?;
    Ok(r.rows_affected())
}

/// mark_batch 给 PROGRAMMING/OUTSOURCE 等特殊 location 转换用。
///
/// 2026-09-16 PR-3：删 placed_at 写入（列已删）；PENDING/PROGRAMMING 起点
/// batch 的 current_process_step_id 通常为 NULL（不在生产流），由 caller
/// 在调本函数前决定。
#[allow(clippy::too_many_arguments)]
async fn mark_batch_for_programming<'e, E: PgExecutor<'e>>(
    executor: E,
    batch_id: i64,
    expected_version: i32,
    new_status: &str,
    new_location: &str,
    new_holder_id: Option<i64>,
    updated_by: i64,
) -> Result<u64, sqlx::Error> {
    let r = sqlx::query(
        "UPDATE t_part_batch SET status = $3, location = $4, current_holder_id = $5, \
         version = version + 1, updated_at = now(), updated_by = $6 \
         WHERE id = $1 AND version = $2 AND status NOT IN ('CANCELLED', 'COMPLETED') \
         AND deleted_at IS NULL",
    )
    .bind(batch_id)
    .bind(expected_version)
    .bind(new_status)
    .bind(new_location)
    .bind(new_holder_id)
    .bind(updated_by)
    .execute(executor)
    .await?;
    Ok(r.rows_affected())
}

/// 2026-09-16 PR-3 批次 step 化：part 进入生产流（place_on_shelf /
/// release_from_programming / send_to_outsource）前必须已制定工艺链。
///
/// 守卫：
/// - `process_chain_id IS NULL` → `BIZ_PROCESS_CHAIN_REQUIRED` 409 「请先制定工序链」
/// - chain 已软删（防御）→ 同样 `BIZ_PROCESS_CHAIN_REQUIRED`
///
/// 返回：chain_id（已校验非空）。caller 继续用 `process_id` 经
/// `ProcessChainRepo::resolve_step_id_by_process` 解析为 step_id。
async fn require_process_chain(
    conn: &mut PgConnection,
    part_id: i64,
) -> Result<i64, AppError> {
    let row: Option<(Option<i64>,)> =
        sqlx::query_as("SELECT process_chain_id FROM t_part WHERE id = $1 AND deleted_at IS NULL")
            .bind(part_id)
            .fetch_optional(&mut *conn)
            .await?;
    let chain_id_opt = row
        .ok_or_else(|| AppError::biz(code::BIZ_PART_NOT_FOUND, format!("part {part_id} 不存在")))?
        .0;
    chain_id_opt.ok_or_else(|| {
        AppError::biz(
            code::BIZ_PROCESS_CHAIN_REQUIRED,
            "请先制定工序链（part 未绑定 process_chain）",
        )
    })
}

// ===== unit tests for state-machine helpers =====

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modules::part::statemachine::PartStatus;

    #[test]
    fn ensure_transition_allows_known() {
        // 已知的合法迁移应通过
        ensure_transition(PartStatus::PENDING, PartStatus::IN_PROCESS, "test").unwrap();
        ensure_transition(PartStatus::IN_PROCESS, PartStatus::PENDING, "test").unwrap();
        ensure_transition(PartStatus::PROGRAMMING, PartStatus::PENDING, "test").unwrap();
        ensure_transition(PartStatus::PROGRAMMING, PartStatus::IN_PROCESS, "test").unwrap();
        ensure_transition(PartStatus::REPAIRING, PartStatus::IN_PROCESS, "test").unwrap();
        ensure_transition(PartStatus::OUTSOURCE, PartStatus::IN_PROCESS, "test").unwrap();
        ensure_transition(PartStatus::INSPECTION, PartStatus::REPAIRING, "test").unwrap();
    }

    #[test]
    fn ensure_transition_rejects_unknown() {
        // 非法迁移
        let r = ensure_transition(PartStatus::DELIVERED, PartStatus::READY_TO_SHIP, "test");
        assert!(r.is_err());
        let r = ensure_transition(PartStatus::COMPLETED, PartStatus::CANCELLED, "test");
        assert!(r.is_err());
    }

    #[test]
    fn ensure_transition_rejects_already_cancelled() {
        // CANCELLED 是终态；任何迁移拒绝
        let r = ensure_transition(PartStatus::CANCELLED, PartStatus::PENDING, "test");
        assert!(r.is_err());
        let code = r.unwrap_err().code();
        assert_eq!(code, code::BIZ_PART_ALREADY_CANCELLED);
    }

    #[test]
    fn batch_version_mismatch_detects_correctly() {
        assert!(batch_version_mismatch(1, 0, 1));
        assert!(!batch_version_mismatch(1, 1, 1));
    }
}
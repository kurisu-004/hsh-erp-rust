//! part 域 Phase 1（2026-09-13）业务逻辑：补齐 14 端点。
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

use crate::auth::rbac::{CurrentUser, Role};
use crate::infra::snowflake::SnowflakeIdGenerator;
use crate::modules::customer::repo::CustomerRepo;
use crate::modules::part::dto::{
    InspectionBatchListItemOut, InspectionBatchListOut, InspectionBatchListQuery,
};
use crate::modules::part::model::NewPartEvent;
use crate::modules::part::repo::part::{PartListFilters, PartUpdate};
use crate::modules::part::repo::PartRepo;
use crate::modules::part::statemachine::PartStatus;
use crate::modules::part_batch::repo::PartBatchRepo;
use crate::modules::shelf::repo::ShelfRepo;
use crate::modules::worker::repo::WorkerRepo;
use crate::shared::error::{code, AppError};

use super::super::dto_crud::{
    BatchUpdateOrderInfoFailure, BatchUpdateOrderInfoOut, BatchUpdateOrderInfoRequest,
    BatchWithPdfsRequest, ByWorkTypeQuery, CancelBatchRequest,
    CompleteRepairRequest, LocationTreeNodeOut, LocationTreeOut, MatchByExcelItemResult,
    MatchByExcelItemsRequest, PartBatchListItemOut, PartEventOut, PickUpRequest,
    PlaceOnShelfRequest, ReceiveFromOutsourceToInspectionRequest, RecallToPendingRequest,
    RecallToProgrammingRequest, RepairDispatchRequest, ScanDeliverPartRequest,
    ScanInspectRequest, SendToOutsourceRequest, SendToProgrammingRequest, SplitBatchRequest,
};
use super::super::dto_crud::PartListOut;
use super::PartService;

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
struct BatchListRow {
    id: i64,
    batch_no: i32,
    quantity: i32,
    status: String,
    location: Option<String>,
    version: i32,
    placed_at: Option<chrono::NaiveDateTime>,
    has_been_repaired: bool,
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
    placed_at: Option<chrono::NaiveDateTime>,
    has_been_repaired: bool,
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
            format!(
                "batch {batch_id} 版本冲突（期望 {expected_version}，实际 {actual_version}）"
            ),
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
    let shelf = ShelfRepo::get_by_id(&mut *conn, shelf_id).await?.ok_or_else(|| {
        AppError::biz(code::BIZ_SHELF_NOT_FOUND, format!("shelf {shelf_id} 不存在"))
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

/// 在 batch 上把状态机 + OCC 走完整（`UPDATE ... WHERE version=$expected`）。
/// 返回影响行数（0 行由 caller 决定错误码）。
#[allow(clippy::too_many_arguments)]
async fn mark_batch_with_status_and_meta<'e, E: PgExecutor<'e>>(
    executor: E,
    batch_id: i64,
    expected_version: i32,
    new_status: &str,
    new_location: Option<&str>,
    new_holder_id: Option<i64>,
    new_next_process_id: Option<i64>,
    updated_by: i64,
) -> Result<u64, sqlx::Error> {
    let r = sqlx::query(
        "UPDATE t_part_batch SET status = $3, location = $4, current_holder_id = $5, \
         next_process_id = $6, placed_at = COALESCE(placed_at, now()), \
         version = version + 1, updated_at = now(), updated_by = $7 \
         WHERE id = $1 AND version = $2 AND status NOT IN ('CANCELLED', 'COMPLETED') \
         AND deleted_at IS NULL",
    )
    .bind(batch_id)
    .bind(expected_version)
    .bind(new_status)
    .bind(new_location)
    .bind(new_holder_id)
    .bind(new_next_process_id)
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
         placed_at = now(), version = version + 1, updated_at = now(), updated_by = $6 \
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

impl PartService {
    // ===== 1.1 上架 / 召回 =====

    /// `POST /parts/{id}/place-on-shelf`：PENDING → IN_PROCESS（PRODUCTION_SHELF）。
    pub async fn place_on_shelf(
        conn: &mut PgConnection,
        snowflake: &SnowflakeIdGenerator,
        part_id: i64,
        req: PlaceOnShelfRequest,
        current: &CurrentUser,
    ) -> Result<crate::modules::part::dto::PartOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk])?;
        let part = PartRepo::get_part_inspected(&mut *conn, part_id)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_PART_NOT_FOUND, "part 不存在"))?;
        let batch = PartRepo::find_batch_by_id(&mut *conn, req.batch_id)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_PART_BATCH_NOT_FOUND, "batch 不存在"))?;
        validate_batch_ownership(batch.part_id, batch.id, part_id, req.version, batch.version)?;
        let from = PartStatus::from_str(&batch.status)
            .ok_or_else(|| AppError::biz(code::BIZ_INVALID_VALUE, "batch.status 非法"))?;
        ensure_transition(from, PartStatus::IN_PROCESS, "place-on-shelf")?;
        // shelf 校验
        validate_shelf_zone(conn, req.shelf_id, "PRODUCTION").await?;
        // shelf ↔ process 映射
        assert_shelf_maps_process(conn, req.shelf_id, req.next_process_id).await?;
        // 翻状态
        let n = mark_batch_with_status_and_meta(
            &mut *conn,
            batch.id,
            req.version,
            "IN_PROCESS",
            Some("PRODUCTION_SHELF"),
            Some(req.shelf_id),
            Some(req.next_process_id),
            current.id,
        )
        .await?;
        if n == 0 {
            return Err(AppError::biz(code::VERSION_CONFLICT, "batch 版本冲突"));
        }
        // rollup
        let _ = Self::sync_from_batch_change(conn, part_id, current).await?;
        // 事件日志
        PartRepo::insert_part_event(
            &mut *conn,
            NewPartEvent {
                id: snowflake.next_id(),
                part_id,
                event_type: "PLACED_ON_SHELF",
                from_status: Some(from.as_str()),
                to_status: Some("IN_PROCESS"),
                batch_id: Some(batch.id),
                quantity: Some(batch.quantity),
                drawing_code: Some(&part.drawing_no),
                badge_code: None,
                note: req.note.as_deref(),
                created_by: Some(current.id),
            },
        )
        .await?;
        let fresh = PartRepo::get_part_inspected(&mut *conn, part_id)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_PART_NOT_FOUND, "place-on-shelf 后查不到"))?;
        Ok(crate::modules::part::dto::PartOut::from(fresh))
    }

    /// `POST /parts/{id}/recall-to-pending`：ON_SHELF / PROGRAMMING → PENDING。
    pub async fn recall_to_pending(
        conn: &mut PgConnection,
        snowflake: &SnowflakeIdGenerator,
        part_id: i64,
        req: RecallToPendingRequest,
        current: &CurrentUser,
    ) -> Result<crate::modules::part::dto::PartOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk])?;
        let part = PartRepo::get_part_inspected(&mut *conn, part_id)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_PART_NOT_FOUND, "part 不存在"))?;
        let batch = PartRepo::find_batch_by_id(&mut *conn, req.batch_id)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_PART_BATCH_NOT_FOUND, "batch 不存在"))?;
        validate_batch_ownership(batch.part_id, batch.id, part_id, req.version, batch.version)?;
        let from = PartStatus::from_str(&batch.status)
            .ok_or_else(|| AppError::biz(code::BIZ_INVALID_VALUE, "batch.status 非法"))?;
        ensure_transition(from, PartStatus::PENDING, "recall-to-pending")?;
        // 额外 service 守：IN_PROCESS 时必须有 location=PRODUCTION_SHELF（与 Python 一致）
        if from == PartStatus::IN_PROCESS && batch.location.as_deref() != Some("PRODUCTION_SHELF") {
            return Err(AppError::biz(
                code::BIZ_INVALID_TRANSITION,
                "recall-to-pending: IN_PROCESS 批次必须在 PRODUCTION_SHELF 上",
            ));
        }
        // 翻状态
        let n = mark_batch_with_status_and_meta(
            &mut *conn,
            batch.id,
            req.version,
            "PENDING",
            None,
            None,
            None,
            current.id,
        )
        .await?;
        if n == 0 {
            return Err(AppError::biz(code::VERSION_CONFLICT, "batch 版本冲突"));
        }
        let _ = Self::sync_from_batch_change(conn, part_id, current).await?;
        PartRepo::insert_part_event(
            &mut *conn,
            NewPartEvent {
                id: snowflake.next_id(),
                part_id,
                event_type: "RECALLED",
                from_status: Some(from.as_str()),
                to_status: Some("PENDING"),
                batch_id: Some(batch.id),
                quantity: Some(batch.quantity),
                drawing_code: Some(&part.drawing_no),
                badge_code: None,
                note: req.note.as_deref(),
                created_by: Some(current.id),
            },
        )
        .await?;
        let fresh = PartRepo::get_part_inspected(&mut *conn, part_id)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_PART_NOT_FOUND, "recall 后查不到"))?;
        Ok(crate::modules::part::dto::PartOut::from(fresh))
    }

    // ===== 1.2 CNC 编程流转 =====

    /// `POST /parts/{id}/send-to-programming`：PENDING → PROGRAMMING（OFFICE）。
    pub async fn send_to_programming(
        conn: &mut PgConnection,
        snowflake: &SnowflakeIdGenerator,
        part_id: i64,
        req: SendToProgrammingRequest,
        current: &CurrentUser,
    ) -> Result<crate::modules::part::dto::PartOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk])?;
        let part = PartRepo::get_part_inspected(&mut *conn, part_id)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_PART_NOT_FOUND, "part 不存在"))?;
        let batch = PartRepo::find_batch_by_id(&mut *conn, req.batch_id)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_PART_BATCH_NOT_FOUND, "batch 不存在"))?;
        validate_batch_ownership(batch.part_id, batch.id, part_id, req.version, batch.version)?;
        let from = PartStatus::from_str(&batch.status)
            .ok_or_else(|| AppError::biz(code::BIZ_INVALID_VALUE, "batch.status 非法"))?;
        ensure_transition(from, PartStatus::PROGRAMMING, "send-to-programming")?;
        let n = mark_batch_for_programming(
            &mut *conn,
            batch.id,
            req.version,
            "PROGRAMMING",
            "OFFICE",
            None,
            current.id,
        )
        .await?;
        if n == 0 {
            return Err(AppError::biz(code::VERSION_CONFLICT, "batch 版本冲突"));
        }
        let _ = Self::sync_from_batch_change(conn, part_id, current).await?;
        PartRepo::insert_part_event(
            &mut *conn,
            NewPartEvent {
                id: snowflake.next_id(),
                part_id,
                event_type: "SENT_TO_PROGRAMMING",
                from_status: Some(from.as_str()),
                to_status: Some("PROGRAMMING"),
                batch_id: Some(batch.id),
                quantity: Some(batch.quantity),
                drawing_code: Some(&part.drawing_no),
                badge_code: None,
                note: req.note.as_deref(),
                created_by: Some(current.id),
            },
        )
        .await?;
        let fresh = PartRepo::get_part_inspected(&mut *conn, part_id)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_PART_NOT_FOUND, "send-to-programming 后查不到"))?;
        Ok(crate::modules::part::dto::PartOut::from(fresh))
    }

    /// `POST /parts/{id}/release-from-programming`：PROGRAMMING → IN_PROCESS（PRODUCTION_SHELF）。
    pub async fn release_from_programming(
        conn: &mut PgConnection,
        snowflake: &SnowflakeIdGenerator,
        part_id: i64,
        req: PlaceOnShelfRequest,
        current: &CurrentUser,
    ) -> Result<crate::modules::part::dto::PartOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::CncProgrammer])?;
        let part = PartRepo::get_part_inspected(&mut *conn, part_id)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_PART_NOT_FOUND, "part 不存在"))?;
        let batch = PartRepo::find_batch_by_id(&mut *conn, req.batch_id)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_PART_BATCH_NOT_FOUND, "batch 不存在"))?;
        validate_batch_ownership(batch.part_id, batch.id, part_id, req.version, batch.version)?;
        let from = PartStatus::from_str(&batch.status)
            .ok_or_else(|| AppError::biz(code::BIZ_INVALID_VALUE, "batch.status 非法"))?;
        ensure_transition(from, PartStatus::IN_PROCESS, "release-from-programming")?;
        if from != PartStatus::PROGRAMMING {
            return Err(AppError::biz(
                code::BIZ_INVALID_TRANSITION,
                "release-from-programming: 源状态必须是 PROGRAMMING",
            ));
        }
        validate_shelf_zone(conn, req.shelf_id, "PRODUCTION").await?;
        assert_shelf_maps_process(conn, req.shelf_id, req.next_process_id).await?;
        let n = mark_batch_with_status_and_meta(
            &mut *conn,
            batch.id,
            req.version,
            "IN_PROCESS",
            Some("PRODUCTION_SHELF"),
            Some(req.shelf_id),
            Some(req.next_process_id),
            current.id,
        )
        .await?;
        if n == 0 {
            return Err(AppError::biz(code::VERSION_CONFLICT, "batch 版本冲突"));
        }
        let _ = Self::sync_from_batch_change(conn, part_id, current).await?;
        PartRepo::insert_part_event(
            &mut *conn,
            NewPartEvent {
                id: snowflake.next_id(),
                part_id,
                event_type: "CNC_RELEASED",
                from_status: Some("PROGRAMMING"),
                to_status: Some("IN_PROCESS"),
                batch_id: Some(batch.id),
                quantity: Some(batch.quantity),
                drawing_code: Some(&part.drawing_no),
                badge_code: None,
                note: req.note.as_deref(),
                created_by: Some(current.id),
            },
        )
        .await?;
        let fresh = PartRepo::get_part_inspected(&mut *conn, part_id)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_PART_NOT_FOUND, "release 后查不到"))?;
        Ok(crate::modules::part::dto::PartOut::from(fresh))
    }

    /// `POST /parts/{id}/recall-to-programming`：IN_PROCESS+PRODUCTION_SHELF → PROGRAMMING。
    pub async fn recall_to_programming(
        conn: &mut PgConnection,
        snowflake: &SnowflakeIdGenerator,
        part_id: i64,
        req: RecallToProgrammingRequest,
        current: &CurrentUser,
    ) -> Result<crate::modules::part::dto::PartOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::CncProgrammer])?;
        let part = PartRepo::get_part_inspected(&mut *conn, part_id)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_PART_NOT_FOUND, "part 不存在"))?;
        let batch = PartRepo::find_batch_by_id(&mut *conn, req.batch_id)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_PART_BATCH_NOT_FOUND, "batch 不存在"))?;
        validate_batch_ownership(batch.part_id, batch.id, part_id, req.version, batch.version)?;
        let from = PartStatus::from_str(&batch.status)
            .ok_or_else(|| AppError::biz(code::BIZ_INVALID_VALUE, "batch.status 非法"))?;
        ensure_transition(from, PartStatus::PROGRAMMING, "recall-to-programming")?;
        if from == PartStatus::IN_PROCESS && batch.location.as_deref() != Some("PRODUCTION_SHELF") {
            return Err(AppError::biz(
                code::BIZ_INVALID_TRANSITION,
                "recall-to-programming: IN_PROCESS 批次必须在 PRODUCTION_SHELF 上",
            ));
        }
        let n = mark_batch_for_programming(
            &mut *conn,
            batch.id,
            req.version,
            "PROGRAMMING",
            "OFFICE",
            None,
            current.id,
        )
        .await?;
        if n == 0 {
            return Err(AppError::biz(code::VERSION_CONFLICT, "batch 版本冲突"));
        }
        let _ = Self::sync_from_batch_change(conn, part_id, current).await?;
        PartRepo::insert_part_event(
            &mut *conn,
            NewPartEvent {
                id: snowflake.next_id(),
                part_id,
                event_type: "RECALLED",
                from_status: Some(from.as_str()),
                to_status: Some("PROGRAMMING"),
                batch_id: Some(batch.id),
                quantity: Some(batch.quantity),
                drawing_code: Some(&part.drawing_no),
                badge_code: None,
                note: req.note.as_deref(),
                created_by: Some(current.id),
            },
        )
        .await?;
        let fresh = PartRepo::get_part_inspected(&mut *conn, part_id)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_PART_NOT_FOUND, "recall-to-programming 后查不到"))?;
        Ok(crate::modules::part::dto::PartOut::from(fresh))
    }

    /// `GET /parts/pending-programming`：status=PROGRAMMING 一览（复用 PartListOut）。
    pub async fn list_pending_programming(
        conn: &mut PgConnection,
        query: &super::super::dto_crud::PartListQuery,
        current: &CurrentUser,
    ) -> Result<PartListOut, AppError> {
        current.require_any_role(&[
            Role::Manager,
            Role::Clerk,
            Role::CncProgrammer,
            Role::Inspector,
        ])?;
        let limit = query.limit.unwrap_or(50).clamp(1, 500);
        let offset = query.offset.unwrap_or(0).max(0);
        // 强制 status=PROGRAMMING
        let f = PartListFilters {
            customer_ids: &[],
            status: Some("PROGRAMMING"),
            statuses: &[],
            is_urgent: query.is_urgent,
            keyword: Some(query.keyword.as_deref().unwrap_or("")),
            sort_by: match query.sort_by.as_deref().unwrap_or("PLANNED_DELIVERY_DATE") {
                "CREATED_AT" => "created_at",
                "UPDATED_AT" => "updated_at",
                "PLANNED_DELIVERY_DATE" => "planned_delivery_date",
                "REQUEST_DATE" => "request_date",
                "SERIAL_NO" => "serial_no",
                "DRAWING_NO" => "drawing_no",
                "NAME" => "name",
                _ => "planned_delivery_date",
            },
            sort_dir: query.sort_dir.as_deref().unwrap_or("ASC"),
            limit,
            offset,
            include_deleted: false,
        };
        let items = PartRepo::list_with_filters(&mut *conn, &f).await?;
        let total = PartRepo::count_with_filters(&mut *conn, &f).await?;
        // 直接转 PartListItem
        let list_items: Vec<super::super::dto_crud::PartListItem> = items
            .into_iter()
            .map(|p| super::super::dto_crud::PartListItem {
                part: p,
                customer_name: None,
                l1_customer_name: None,
            })
            .collect();
        Ok(PartListOut {
            items: list_items,
            total,
            limit,
            offset,
        })
    }

    // ===== 1.3 外协流转 =====

    /// `POST /parts/{id}/send-to-outsource`：PENDING / IN_PROCESS+PRODUCTION_SHELF → OUTSOURCE。
    ///
    /// Phase 2（2026-09-13）扩展：
    /// - 同事务 INSERT t_outsource_shipment（status=OUTSOURCING，quantity=batch.quantity）
    /// - 若 `quote_id` 提供：校验 APPROVED 状态，写 `t_outsource_quote_event` `SENT`
    /// - `direct=true` 时 stub 返回 501 NOT_IMPLEMENTED（follow-up）
    pub async fn send_to_outsource(
        conn: &mut PgConnection,
        snowflake: &SnowflakeIdGenerator,
        part_id: i64,
        req: SendToOutsourceRequest,
        current: &CurrentUser,
    ) -> Result<crate::modules::part::dto::PartOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk, Role::Inspector])?;
        // DIRECT 模式 stub（Phase 2 占位；follow-up：免审批自动建 quote + shipment）
        if req.direct.unwrap_or(false) {
            return Err(AppError::biz(
                code::INTERNAL,
                "DIRECT 模式发外协尚未实现（Phase 2 stub；follow-up: 自动建 APPROVED quote 占位）",
            ));
        }
        let part = PartRepo::get_part_inspected(&mut *conn, part_id)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_PART_NOT_FOUND, "part 不存在"))?;
        let batch = PartRepo::find_batch_by_id(&mut *conn, req.batch_id)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_PART_BATCH_NOT_FOUND, "batch 不存在"))?;
        validate_batch_ownership(batch.part_id, batch.id, part_id, req.version, batch.version)?;
        let from = PartStatus::from_str(&batch.status)
            .ok_or_else(|| AppError::biz(code::BIZ_INVALID_VALUE, "batch.status 非法"))?;
        ensure_transition(from, PartStatus::OUTSOURCE, "send-to-outsource")?;
        // service 守：IN_PROCESS 时必须有 location=PRODUCTION_SHELF
        if from == PartStatus::IN_PROCESS && batch.location.as_deref() != Some("PRODUCTION_SHELF") {
            return Err(AppError::biz(
                code::BIZ_INVALID_TRANSITION,
                "send-to-outsource: IN_PROCESS 批次必须在 PRODUCTION_SHELF 上",
            ));
        }
        // 校验 outsource 公司存在 + 启用
        let company_row: Option<(bool,)> = sqlx::query_as(
            "SELECT is_active FROM t_outsource_company WHERE id = $1 AND deleted_at IS NULL",
        )
        .bind(req.outsource_company_id)
        .fetch_optional(&mut *conn)
        .await?;
        let is_active = company_row.ok_or_else(|| {
            AppError::biz(
                code::BIZ_OUTSOURCE_COMPANY_NOT_FOUND,
                format!("outsource_company {} 不存在", req.outsource_company_id),
            )
        })?;
        if !is_active.0 {
            return Err(AppError::biz(
                code::BIZ_OUTSOURCE_COMPANY_IN_USE,
                format!("outsource_company {} 已停用", req.outsource_company_id),
            ));
        }
        // 校验 process 存在
        let proc: Option<(i64,)> = sqlx::query_as("SELECT id FROM t_process WHERE id = $1 AND deleted_at IS NULL")
            .bind(req.process_id)
            .fetch_optional(&mut *conn)
            .await?;
        if proc.is_none() {
            return Err(AppError::biz(
                code::BIZ_PROCESS_NOT_FOUND,
                format!("process {} 不存在", req.process_id),
            ));
        }
        // Phase 2：quote_id 可选；若提供必须 APPROVED 状态
        let (quote_id_opt, unit_price): (Option<i64>, Option<rust_decimal::Decimal>) = if let Some(qid) = req.quote_id {
            let row: Option<(String, rust_decimal::Decimal, i64, i64, i64)> = sqlx::query_as(
                "SELECT status, price, part_id, outsource_company_id, process_id \
                 FROM t_outsource_quote \
                 WHERE id = $1 AND deleted_at IS NULL",
            )
            .bind(qid)
            .fetch_optional(&mut *conn)
            .await?;
            let (status, price, q_part_id, q_company_id, q_process_id) = row.ok_or_else(|| {
                AppError::biz(
                    code::BIZ_OUTSOURCE_QUOTE_NOT_FOUND,
                    format!("quote {qid} 不存在"),
                )
            })?;
            if status != "APPROVED" {
                return Err(AppError::biz(
                    code::BIZ_OUTSOURCE_QUOTE_NOT_APPROVED,
                    format!(
                        "quote {qid} 当前 {status}，非 APPROVED 不可发送"
                    ),
                ));
            }
            if q_part_id != part_id || q_company_id != req.outsource_company_id || q_process_id != req.process_id {
                return Err(AppError::biz(
                    code::BIZ_OUTSOURCE_QUOTE_INVALID_TRANSITION,
                    format!(
                        "quote {qid} 与 send_to_outsource 参数不一致（part/company/process）"
                    ),
                ));
            }
            (Some(qid), Some(price))
        } else {
            (None, None)
        };
        let n = mark_batch_with_status_and_meta(
            &mut *conn,
            batch.id,
            req.version,
            "OUTSOURCE",
            Some("OUTSOURCE_COMPANY"),
            Some(req.outsource_company_id),
            Some(req.process_id),
            current.id,
        )
        .await?;
        if n == 0 {
            return Err(AppError::biz(code::VERSION_CONFLICT, "batch 版本冲突"));
        }
        // Phase 2：同事务 INSERT t_outsource_shipment（status=OUTSOURCING）
        //   quantity = batch.quantity（与 Python send_to_outsource 一致：整批发送）
        //   unit_price = quote.price（若有）；DIRECT 模式暂 0 占位（但 Phase 2 DIRECT stub 不入此分支）
        let shipment_unit_price = unit_price.unwrap_or_else(|| rust_decimal::Decimal::new(0, 0));
        let shipment_id = snowflake.next_id();
        let inserted_shipment = sqlx::query(
            "INSERT INTO t_outsource_shipment \
             (id, quote_id, part_id, batch_id, outsource_company_id, process_id, \
              quantity, unit_price, status, sent_at, created_by, updated_by) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, 'OUTSOURCING', now(), $9, $9) \
             ON CONFLICT DO NOTHING",
        )
        .bind(shipment_id)
        .bind(quote_id_opt)
        .bind(part_id)
        .bind(batch.id)
        .bind(req.outsource_company_id)
        .bind(req.process_id)
        .bind(batch.quantity)
        .bind(shipment_unit_price)
        .bind(current.id)
        .execute(&mut *conn)
        .await?;
        // 若唯一索引 uq_t_outsource_shipment_open_batch 撞了（同一批次已有开口 shipment），
        // 0 行；回滚思路：拒 + BIZ_OUTSOURCE_SHIPMENT_INVALID_TRANSITION
        if inserted_shipment.rows_affected() == 0 {
            return Err(AppError::biz(
                code::BIZ_OUTSOURCE_SHIPMENT_INVALID_TRANSITION,
                format!("batch {} 已有开口 shipment；不可重复发送", batch.id),
            ));
        }
        // 若提供了 quote，写 SENT 事件（仅审计，不改 quote.status）
        if let Some(qid) = quote_id_opt {
            sqlx::query(
                "INSERT INTO t_outsource_quote_event \
                 (id, quote_id, event_type, from_status, to_status, note, created_by) \
                 VALUES ($1, $2, 'SENT', 'APPROVED', 'APPROVED', $3, $4)",
            )
            .bind(snowflake.next_id())
            .bind(qid)
            .bind(req.note.as_deref())
            .bind(current.id)
            .execute(&mut *conn)
            .await?;
        }
        let _ = Self::sync_from_batch_change(conn, part_id, current).await?;
        PartRepo::insert_part_event(
            &mut *conn,
            NewPartEvent {
                id: snowflake.next_id(),
                part_id,
                event_type: "SENT_TO_OUTSOURCE",
                from_status: Some(from.as_str()),
                to_status: Some("OUTSOURCE"),
                batch_id: Some(batch.id),
                quantity: Some(batch.quantity),
                drawing_code: Some(&part.drawing_no),
                badge_code: None,
                note: req.note.as_deref(),
                created_by: Some(current.id),
            },
        )
        .await?;
        let fresh = PartRepo::get_part_inspected(&mut *conn, part_id)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_PART_NOT_FOUND, "send-to-outsource 后查不到"))?;
        Ok(crate::modules::part::dto::PartOut::from(fresh))
    }

    /// `POST /parts/{id}/receive-from-outsource`：OUTSOURCE → IN_PROCESS（PRODUCTION_SHELF）。
    ///
    /// Phase 2（2026-09-13）扩展：同事务把批次开口 shipment 标 RECEIVED + 写 RECEIVED 事件。
    pub async fn receive_from_outsource(
        conn: &mut PgConnection,
        snowflake: &SnowflakeIdGenerator,
        part_id: i64,
        req: PlaceOnShelfRequest,
        current: &CurrentUser,
    ) -> Result<crate::modules::part::dto::PartOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk, Role::Inspector])?;
        let part = PartRepo::get_part_inspected(&mut *conn, part_id)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_PART_NOT_FOUND, "part 不存在"))?;
        let batch = PartRepo::find_batch_by_id(&mut *conn, req.batch_id)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_PART_BATCH_NOT_FOUND, "batch 不存在"))?;
        validate_batch_ownership(batch.part_id, batch.id, part_id, req.version, batch.version)?;
        let from = PartStatus::from_str(&batch.status)
            .ok_or_else(|| AppError::biz(code::BIZ_INVALID_VALUE, "batch.status 非法"))?;
        ensure_transition(from, PartStatus::IN_PROCESS, "receive-from-outsource")?;
        if from != PartStatus::OUTSOURCE {
            return Err(AppError::biz(
                code::BIZ_INVALID_TRANSITION,
                "receive-from-outsource: 源状态必须是 OUTSOURCE",
            ));
        }
        validate_shelf_zone(conn, req.shelf_id, "PRODUCTION").await?;
        assert_shelf_maps_process(conn, req.shelf_id, req.next_process_id).await?;
        let n = mark_batch_with_status_and_meta(
            &mut *conn,
            batch.id,
            req.version,
            "IN_PROCESS",
            Some("PRODUCTION_SHELF"),
            Some(req.shelf_id),
            Some(req.next_process_id),
            current.id,
        )
        .await?;
        if n == 0 {
            return Err(AppError::biz(code::VERSION_CONFLICT, "batch 版本冲突"));
        }
        // Phase 2：把批次对应开口 shipment 标 RECEIVED + 写 quote event RECEIVED
        let shipment_row: Option<(i64, i64, i32)> = sqlx::query_as(
            "SELECT id, quote_id, version FROM t_outsource_shipment \
             WHERE batch_id = $1 AND deleted_at IS NULL AND status = 'OUTSOURCING'",
        )
        .bind(batch.id)
        .fetch_optional(&mut *conn)
        .await?;
        if let Some((sid, qid, sver)) = shipment_row {
            let marked = sqlx::query(
                "UPDATE t_outsource_shipment SET status = 'RECEIVED', received_at = now(), \
                 version = version + 1, updated_at = now(), updated_by = $2 \
                 WHERE id = $1 AND version = $3 AND deleted_at IS NULL",
            )
            .bind(sid)
            .bind(current.id)
            .bind(sver)
            .execute(&mut *conn)
            .await?;
            if marked.rows_affected() == 0 {
                return Err(AppError::biz(
                    code::VERSION_CONFLICT,
                    format!("shipment {sid} 版本冲突"),
                ));
            }
            // quote_id 可空（旧 shipment 兼容）；非空时写 RECEIVED 事件
            if qid != 0 {
                sqlx::query(
                    "INSERT INTO t_outsource_quote_event \
                     (id, quote_id, event_type, from_status, to_status, note, created_by) \
                     VALUES ($1, $2, 'RECEIVED', 'APPROVED', 'APPROVED', $3, $4)",
                )
                .bind(snowflake.next_id())
                .bind(qid)
                .bind(req.note.as_deref())
                .bind(current.id)
                .execute(&mut *conn)
                .await?;
            }
        }
        let _ = Self::sync_from_batch_change(conn, part_id, current).await?;
        PartRepo::insert_part_event(
            &mut *conn,
            NewPartEvent {
                id: snowflake.next_id(),
                part_id,
                event_type: "RECEIVED_FROM_OUTSOURCE",
                from_status: Some("OUTSOURCE"),
                to_status: Some("IN_PROCESS"),
                batch_id: Some(batch.id),
                quantity: Some(batch.quantity),
                drawing_code: Some(&part.drawing_no),
                badge_code: None,
                note: req.note.as_deref(),
                created_by: Some(current.id),
            },
        )
        .await?;
        let fresh = PartRepo::get_part_inspected(&mut *conn, part_id)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_PART_NOT_FOUND, "receive 后查不到"))?;
        Ok(crate::modules::part::dto::PartOut::from(fresh))
    }

    /// `POST /parts/{id}/receive-from-outsource-to-inspection`：OUTSOURCE → INSPECTION。
    ///
    /// Phase 2（2026-09-13）扩展：同事务把批次对应开口 shipment 标 RECEIVED（与
    /// `receive_from_outsource` 同样的"整批接收"语义）。
    pub async fn receive_from_outsource_to_inspection(
        conn: &mut PgConnection,
        snowflake: &SnowflakeIdGenerator,
        part_id: i64,
        req: ReceiveFromOutsourceToInspectionRequest,
        current: &CurrentUser,
    ) -> Result<crate::modules::part::dto::PartOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk, Role::Inspector])?;
        let part = PartRepo::get_part_inspected(&mut *conn, part_id)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_PART_NOT_FOUND, "part 不存在"))?;
        let batch = PartRepo::find_batch_by_id(&mut *conn, req.batch_id)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_PART_BATCH_NOT_FOUND, "batch 不存在"))?;
        validate_batch_ownership(batch.part_id, batch.id, part_id, req.version, batch.version)?;
        let from = PartStatus::from_str(&batch.status)
            .ok_or_else(|| AppError::biz(code::BIZ_INVALID_VALUE, "batch.status 非法"))?;
        ensure_transition(from, PartStatus::INSPECTION, "receive-from-outsource-to-inspection")?;
        if from != PartStatus::OUTSOURCE {
            return Err(AppError::biz(
                code::BIZ_INVALID_TRANSITION,
                "receive-from-outsource-to-inspection: 源状态必须是 OUTSOURCE",
            ));
        }
        validate_shelf_zone(conn, req.shelf_id, "INSPECTION").await?;
        let n = mark_batch_with_status_and_meta(
            &mut *conn,
            batch.id,
            req.version,
            "INSPECTION",
            Some("INSPECTION_SHELF"),
            Some(req.shelf_id),
            None,
            current.id,
        )
        .await?;
        if n == 0 {
            return Err(AppError::biz(code::VERSION_CONFLICT, "batch 版本冲突"));
        }
        // Phase 2：标 shipment RECEIVED + 写 quote event RECEIVED
        let shipment_row: Option<(i64, i64, i32)> = sqlx::query_as(
            "SELECT id, quote_id, version FROM t_outsource_shipment \
             WHERE batch_id = $1 AND deleted_at IS NULL AND status = 'OUTSOURCING'",
        )
        .bind(batch.id)
        .fetch_optional(&mut *conn)
        .await?;
        if let Some((sid, qid, sver)) = shipment_row {
            let marked = sqlx::query(
                "UPDATE t_outsource_shipment SET status = 'RECEIVED', received_at = now(), \
                 version = version + 1, updated_at = now(), updated_by = $2 \
                 WHERE id = $1 AND version = $3 AND deleted_at IS NULL",
            )
            .bind(sid)
            .bind(current.id)
            .bind(sver)
            .execute(&mut *conn)
            .await?;
            if marked.rows_affected() == 0 {
                return Err(AppError::biz(
                    code::VERSION_CONFLICT,
                    format!("shipment {sid} 版本冲突"),
                ));
            }
            if qid != 0 {
                sqlx::query(
                    "INSERT INTO t_outsource_quote_event \
                     (id, quote_id, event_type, from_status, to_status, note, created_by) \
                     VALUES ($1, $2, 'RECEIVED', 'APPROVED', 'APPROVED', $3, $4)",
                )
                .bind(snowflake.next_id())
                .bind(qid)
                .bind(req.note.as_deref())
                .bind(current.id)
                .execute(&mut *conn)
                .await?;
            }
        }
        let _ = Self::sync_from_batch_change(conn, part_id, current).await?;
        PartRepo::insert_part_event(
            &mut *conn,
            NewPartEvent {
                id: snowflake.next_id(),
                part_id,
                event_type: "RECEIVED_FROM_OUTSOURCE_INSPECTED",
                from_status: Some("OUTSOURCE"),
                to_status: Some("INSPECTION"),
                batch_id: Some(batch.id),
                quantity: Some(batch.quantity),
                drawing_code: Some(&part.drawing_no),
                badge_code: None,
                note: req.note.as_deref(),
                created_by: Some(current.id),
            },
        )
        .await?;
        let fresh = PartRepo::get_part_inspected(&mut *conn, part_id)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_PART_NOT_FOUND, "receive 后查不到"))?;
        Ok(crate::modules::part::dto::PartOut::from(fresh))
    }

    // ===== 1.4 返修闭环 =====

    /// `POST /parts/{id}/complete-repair`：REPAIRING → IN_PROCESS（落回生产架）
    /// 或 REPAIRING → INSPECTION（送检区）。
    pub async fn complete_repair(
        conn: &mut PgConnection,
        snowflake: &SnowflakeIdGenerator,
        part_id: i64,
        req: CompleteRepairRequest,
        current: &CurrentUser,
    ) -> Result<crate::modules::part::dto::PartOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk, Role::Inspector])?;
        let part = PartRepo::get_part_inspected(&mut *conn, part_id)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_PART_NOT_FOUND, "part 不存在"))?;
        let batch = PartRepo::find_batch_by_id(&mut *conn, req.batch_id)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_PART_BATCH_NOT_FOUND, "batch 不存在"))?;
        validate_batch_ownership(batch.part_id, batch.id, part_id, req.version, batch.version)?;
        let from = PartStatus::from_str(&batch.status)
            .ok_or_else(|| AppError::biz(code::BIZ_INVALID_VALUE, "batch.status 非法"))?;
        if from != PartStatus::REPAIRING {
            return Err(AppError::biz(
                code::BIZ_PART_REPAIR_NOT_TRIGGERED,
                "complete-repair: 源状态必须是 REPAIRING",
            ));
        }
        // shelf 区决定目标状态
        let shelf = ShelfRepo::get_by_id(&mut *conn, req.shelf_id)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_SHELF_NOT_FOUND, "shelf 不存在"))?;
        if !shelf.is_active {
            return Err(AppError::biz(code::BIZ_SHELF_INACTIVE, "shelf 已停用"));
        }
        let (new_status, new_location, next_proc_id) = match shelf.zone.as_str() {
            "PRODUCTION" => {
                let np = req.next_process_id.ok_or_else(|| {
                    AppError::biz(code::BIZ_INVALID_VALUE, "PRODUCTION 区需要 next_process_id")
                })?;
                assert_shelf_maps_process(conn, req.shelf_id, np).await?;
                ("IN_PROCESS", Some("PRODUCTION_SHELF"), Some(np))
            }
            "INSPECTION" => {
                // carried next_process_id（如果 caller 传了）不写回；service 不再校验
                ("INSPECTION", Some("INSPECTION_SHELF"), None)
            }
            other => {
                return Err(AppError::biz(
                    code::BIZ_INVALID_VALUE,
                    format!("complete-repair 不允许 zone={other}"),
                ));
            }
        };
        let n = mark_batch_with_status_and_meta(
            &mut *conn,
            batch.id,
            req.version,
            new_status,
            new_location,
            Some(req.shelf_id),
            next_proc_id,
            current.id,
        )
        .await?;
        if n == 0 {
            return Err(AppError::biz(code::VERSION_CONFLICT, "batch 版本冲突"));
        }
        let _ = Self::sync_from_batch_change(conn, part_id, current).await?;
        PartRepo::insert_part_event(
            &mut *conn,
            NewPartEvent {
                id: snowflake.next_id(),
                part_id,
                event_type: "REPAIR_COMPLETED",
                from_status: Some("REPAIRING"),
                to_status: Some(new_status),
                batch_id: Some(batch.id),
                quantity: Some(batch.quantity),
                drawing_code: Some(&part.drawing_no),
                badge_code: None,
                note: req.note.as_deref(),
                created_by: Some(current.id),
            },
        )
        .await?;
        let fresh = PartRepo::get_part_inspected(&mut *conn, part_id)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_PART_NOT_FOUND, "complete-repair 后查不到"))?;
        Ok(crate::modules::part::dto::PartOut::from(fresh))
    }

    /// `POST /parts/{id}/repair-dispatch`：一步式返修下发。
    ///
    /// 入口：IN_PROCESS / INSPECTION / READY_TO_SHIP；目标状态由 shelf.zone 决定
    /// （PRODUCTION → IN_PROCESS；INSPECTION → INSPECTION）。
    pub async fn repair_dispatch(
        conn: &mut PgConnection,
        snowflake: &SnowflakeIdGenerator,
        part_id: i64,
        req: RepairDispatchRequest,
        current: &CurrentUser,
    ) -> Result<crate::modules::part::dto::PartOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk, Role::Inspector])?;
        let part = PartRepo::get_part_inspected(&mut *conn, part_id)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_PART_NOT_FOUND, "part 不存在"))?;
        let batch = PartRepo::find_batch_by_id(&mut *conn, req.batch_id)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_PART_BATCH_NOT_FOUND, "batch 不存在"))?;
        validate_batch_ownership(batch.part_id, batch.id, part_id, req.version, batch.version)?;
        let from = PartStatus::from_str(&batch.status)
            .ok_or_else(|| AppError::biz(code::BIZ_INVALID_VALUE, "batch.status 非法"))?;
        // 入口白名单（PR-M 2026-08-04：INSPECTION / READY_TO_SHIP / IN_PROCESS / DELIVERED）
        if !matches!(
            from,
            PartStatus::INSPECTION
                | PartStatus::READY_TO_SHIP
                | PartStatus::IN_PROCESS
                | PartStatus::DELIVERED
        ) {
            return Err(AppError::biz(
                code::BIZ_INVALID_TRANSITION,
                format!("repair-dispatch: 起点 {from:?} 不允许"),
            ));
        }
        let shelf = ShelfRepo::get_by_id(&mut *conn, req.shelf_id)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_SHELF_NOT_FOUND, "shelf 不存在"))?;
        if !shelf.is_active {
            return Err(AppError::biz(code::BIZ_SHELF_INACTIVE, "shelf 已停用"));
        }
        let (new_status, new_location, next_proc_id) = match shelf.zone.as_str() {
            "PRODUCTION" => {
                let np = req.next_process_id.ok_or_else(|| {
                    AppError::biz(code::BIZ_INVALID_VALUE, "PRODUCTION 区需要 next_process_id")
                })?;
                assert_shelf_maps_process(conn, req.shelf_id, np).await?;
                ("IN_PROCESS", Some("PRODUCTION_SHELF"), Some(np))
            }
            "INSPECTION" => ("INSPECTION", Some("INSPECTION_SHELF"), None),
            other => {
                return Err(AppError::biz(
                    code::BIZ_INVALID_VALUE,
                    format!("repair-dispatch 不允许 zone={other}"),
                ));
            }
        };
        // 一步式：先发 REPAIR_STARTED 事件，再 UPDATE；状态机视角：起 → REPAIRING → 目标
        // 但本实现是「一步式」：单条 UPDATE + 两条事件日志（REPAIR_STARTED + REPAIR_COMPLETED），
        // 与 Python `repair_dispatch` 一致。
        let n = mark_batch_with_status_and_meta(
            &mut *conn,
            batch.id,
            req.version,
            new_status,
            new_location,
            Some(req.shelf_id),
            next_proc_id,
            current.id,
        )
        .await?;
        if n == 0 {
            return Err(AppError::biz(code::VERSION_CONFLICT, "batch 版本冲突"));
        }
        // has_been_repaired 仍要写（一致性）
        sqlx::query(
            "UPDATE t_part_batch SET has_been_repaired = TRUE \
             WHERE id = $1 AND has_been_repaired = FALSE")
            .bind(batch.id)
        .execute(&mut *conn)
        .await?;
        let _ = Self::sync_from_batch_change(conn, part_id, current).await?;
        // 两条事件
        PartRepo::insert_part_event(
            &mut *conn,
            NewPartEvent {
                id: snowflake.next_id(),
                part_id,
                event_type: "REPAIR_STARTED",
                from_status: Some(from.as_str()),
                to_status: Some("REPAIRING"),
                batch_id: Some(batch.id),
                quantity: Some(batch.quantity),
                drawing_code: Some(&part.drawing_no),
                badge_code: None,
                note: req.reason.as_deref(),
                created_by: Some(current.id),
            },
        )
        .await?;
        PartRepo::insert_part_event(
            &mut *conn,
            NewPartEvent {
                id: snowflake.next_id(),
                part_id,
                event_type: "REPAIR_COMPLETED",
                from_status: Some("REPAIRING"),
                to_status: Some(new_status),
                batch_id: Some(batch.id),
                quantity: Some(batch.quantity),
                drawing_code: Some(&part.drawing_no),
                badge_code: None,
                note: req.note.as_deref(),
                created_by: Some(current.id),
            },
        )
        .await?;
        let fresh = PartRepo::get_part_inspected(&mut *conn, part_id)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_PART_NOT_FOUND, "repair-dispatch 后查不到"))?;
        Ok(crate::modules::part::dto::PartOut::from(fresh))
    }

    /// `GET /parts/repair-batches`：DELIVERED 批次列表（M+C+I）。
    /// 复用 `InspectionBatchListQuery` + repo；status=DELIVERED。
    pub async fn list_repair_batches(
        conn: &mut PgConnection,
        query: &InspectionBatchListQuery,
        current: &CurrentUser,
    ) -> Result<InspectionBatchListOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk, Role::Inspector])?;
        Self::list_batches_with_status(conn, query, &["DELIVERED"], current).await
    }

    /// `GET /parts/repairing-batches`：REPAIRING 批次列表（M+C+I）。
    pub async fn list_repairing_batches(
        conn: &mut PgConnection,
        query: &InspectionBatchListQuery,
        current: &CurrentUser,
    ) -> Result<InspectionBatchListOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk, Role::Inspector])?;
        Self::list_batches_with_status(conn, query, &["REPAIRING"], current).await
    }

    /// 通用 INSPECTION / DELIVERED / REPAIRING / 等批次列表实现。
    async fn list_batches_with_status(
        conn: &mut PgConnection,
        query: &InspectionBatchListQuery,
        statuses: &[&str],
        _current: &CurrentUser,
    ) -> Result<InspectionBatchListOut, AppError> {
        let limit = query.limit.unwrap_or(200).clamp(1, 500);
        let offset = query.offset.unwrap_or(0).max(0);
        let keyword = query.keyword.as_deref().unwrap_or("");
        let rows: Vec<InspectionRepairRow> = sqlx::query_as::<_, InspectionRepairRow>(
            "SELECT b.id AS batch_id, b.part_id, b.batch_no, b.quantity, b.status,              b.location, b.version, b.placed_at, b.has_been_repaired, b.parent_batch_id,              b.current_holder_id, COALESCE(s.name, w.name, oc.name) AS holder_name,              b.next_process_id, p2.name AS next_process_name,              b.delivery_note_id, dn.delivery_note_no,              p.serial_no, p.drawing_no, p.name, p.order_no, p.planned_delivery_date,              p.is_urgent, p.version AS part_version, p.created_at, p.updated_at,              p.customer_id, c.name AS customer_name, c_l1.name AS l1_customer_name              FROM t_part_batch b JOIN t_part p ON p.id = b.part_id              LEFT JOIN t_customer c ON c.id = p.customer_id              LEFT JOIN t_customer c_l1 ON c_l1.id = c.parent_id AND c_l1.deleted_at IS NULL              LEFT JOIN t_shelf s ON s.id = b.current_holder_id              LEFT JOIN t_worker w ON w.id = b.current_holder_id              LEFT JOIN t_outsource_company oc ON oc.id = b.current_holder_id              LEFT JOIN t_process p2 ON p2.id = b.next_process_id              LEFT JOIN t_delivery_note dn ON dn.id = b.delivery_note_id              WHERE b.deleted_at IS NULL AND p.deleted_at IS NULL              AND b.status = ANY($1)              AND ($2 = '' OR p.drawing_no ILIKE '%' || $2 || '%' OR p.name ILIKE '%' || $2 || '%')              AND ($3::bigint IS NULL OR p.customer_id = $3)              AND ($4::text IS NULL OR p.serial_no ILIKE '%' || $4 || '%')              AND ($5::date IS NULL OR p.planned_delivery_date >= $5)              AND ($6::date IS NULL OR p.planned_delivery_date <= $6)              ORDER BY b.id DESC LIMIT $7 OFFSET $8",
        )
        .bind(statuses)
        .bind(keyword)
        .bind(query.customer_id)
        .bind(query.serial_no.as_deref())
        .bind(query.planned_delivery_date_from)
        .bind(query.planned_delivery_date_to)
        .bind(limit)
        .bind(offset)
        .fetch_all(&mut *conn)
        .await?;
        let items: Vec<InspectionBatchListItemOut> = rows
            .into_iter()
            .map(|r| InspectionBatchListItemOut {
                batch_id: r.batch_id,
                batch_no: r.batch_no,
                quantity: r.quantity,
                status: r.status,
                location: r.location,
                version: r.version,
                placed_at: r.placed_at,
                has_been_repaired: r.has_been_repaired,
                parent_batch_id: r.parent_batch_id,
                current_holder_id: r.current_holder_id,
                holder_name: r.holder_name,
                next_process_id: r.next_process_id,
                next_process_name: r.next_process_name,
                delivery_note_id: r.delivery_note_id,
                delivery_note_no: r.delivery_note_no,
                part_id: r.part_id,
                serial_no: r.serial_no,
                drawing_no: r.drawing_no,
                name: r.name,
                order_no: r.order_no,
                planned_delivery_date: r.planned_delivery_date,
                is_urgent: r.is_urgent,
                part_version: r.part_version,
                created_at: r.created_at,
                updated_at: r.updated_at,
                customer_id: r.customer_id,
                customer_name: r.customer_name,
                l1_customer_name: r.l1_customer_name,
            })
            .collect();
        // 计数（轻量重发一次同条件但不带分页）
        let total: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) AS n \
             FROM t_part_batch b JOIN t_part p ON p.id = b.part_id \
             WHERE b.deleted_at IS NULL AND p.deleted_at IS NULL \
             AND b.status = ANY($1) \
             AND ($2 = '' OR p.drawing_no ILIKE '%' || $2 || '%' OR p.name ILIKE '%' || $2 || '%') \
             AND ($3::bigint IS NULL OR p.customer_id = $3) \
             AND ($4::text IS NULL OR p.serial_no ILIKE '%' || $4 || '%') \
             AND ($5::date IS NULL OR p.planned_delivery_date >= $5) \
             AND ($6::date IS NULL OR p.planned_delivery_date <= $6)",
        )
        .bind(statuses)
        .bind(keyword)
        .bind(query.customer_id)
        .bind(query.serial_no.as_deref())
        .bind(query.planned_delivery_date_from)
        .bind(query.planned_delivery_date_to)
        .fetch_one(&mut *conn)
        .await?;
        Ok(InspectionBatchListOut {
            items,
            total,
            limit,
            offset,
        })
    }

    // ===== 1.5 批次拆分 / 取消 =====

    /// `POST /parts/{id}/batches/split`：拆出部分量为新批次。
    ///
    /// 不变量 `Σ(未删批次.quantity) = t_part.quantity` 由
    /// `PartBatchRepo::split_batch` 强制（同一事务内连发 max+1 / INSERT / UPDATE 三条 SQL，
    /// OCC 守源批次）。`quantity` ∈ [1, source.quantity - 1]（split_batch 内部守）。
    pub async fn split_batch(
        conn: &mut PgConnection,
        snowflake: &SnowflakeIdGenerator,
        part_id: i64,
        req: SplitBatchRequest,
        current: &CurrentUser,
    ) -> Result<i64, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk])?;
        let part = PartRepo::get_part_inspected(&mut *conn, part_id)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_PART_NOT_FOUND, "part 不存在"))?;
        let batch = PartRepo::find_batch_by_id(&mut *conn, req.batch_id)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_PART_BATCH_NOT_FOUND, "batch 不存在"))?;
        validate_batch_ownership(batch.part_id, batch.id, part_id, req.version, batch.version)?;
        // 数量校验
        let qty: i32 = req.quantity.try_into().map_err(|_| {
            AppError::biz(code::BIZ_PART_BATCH_INVALID_QUANTITY, "quantity 超出 i32 范围")
        })?;
        if qty <= 0 {
            return Err(AppError::biz(
                code::BIZ_PART_BATCH_INVALID_QUANTITY,
                format!("quantity {qty} 必须 > 0"),
            ));
        }
        if qty >= batch.quantity {
            return Err(AppError::biz(
                code::BIZ_PART_BATCH_INVALID_QUANTITY,
                format!(
                    "quantity {qty} 必须 < batch.quantity {}（拆批部分量）",
                    batch.quantity
                ),
            ));
        }
        let new_batch_id = snowflake.next_id();
        let when = crate::infra::clock::now_naive();
        let new_id = PartBatchRepo::split_batch(
            &mut *conn,
            new_batch_id,
            batch.id,
            req.version,
            part_id,
            qty,
            &batch.status,
            batch.location.as_deref(),
            batch.current_holder_id,
            batch.next_process_id,
            batch.placed_at,
            when,
            Some(current.id),
            Some(current.id),
        )
        .await
        .map_err(|e| match e {
            sqlx::Error::RowNotFound => {
                AppError::biz(code::VERSION_CONFLICT, "batch 版本冲突")
            }
            other => AppError::from(other),
        })?;
        // SPLIT 事件
        PartRepo::insert_part_event(
            &mut *conn,
            NewPartEvent {
                id: snowflake.next_id(),
                part_id,
                event_type: "SPLIT",
                from_status: Some(&batch.status),
                to_status: Some(&batch.status),
                batch_id: Some(new_id),
                quantity: Some(qty),
                drawing_code: Some(&part.drawing_no),
                badge_code: None,
                note: req.note.as_deref().or(Some("manual split")),
                created_by: Some(current.id),
            },
        )
        .await?;
        let _ = Self::sync_from_batch_change(conn, part_id, current).await?;
        Ok(new_id)
    }

    /// `POST /parts/{id}/batches/{batch_id}/cancel`：批次级取消。
    pub async fn cancel_batch(
        conn: &mut PgConnection,
        snowflake: &SnowflakeIdGenerator,
        part_id: i64,
        batch_id: i64,
        req: CancelBatchRequest,
        current: &CurrentUser,
    ) -> Result<crate::modules::part::dto::PartOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk])?;
        let part = PartRepo::get_part_inspected(&mut *conn, part_id)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_PART_NOT_FOUND, "part 不存在"))?;
        let batch = PartRepo::find_batch_by_id(&mut *conn, batch_id)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_PART_BATCH_NOT_FOUND, "batch 不存在"))?;
        validate_batch_ownership(batch.part_id, batch.id, part_id, req.version, batch.version)?;
        // 终态保护
        let from = PartStatus::from_str(&batch.status)
            .ok_or_else(|| AppError::biz(code::BIZ_INVALID_VALUE, "batch.status 非法"))?;
        if from == PartStatus::COMPLETED || from == PartStatus::CANCELLED {
            return Err(AppError::biz(
                code::BIZ_INVALID_TRANSITION,
                format!("batch 当前状态 {} 不允许取消", from.as_str()),
            ));
        }
        let n = mark_batch_status_only(&mut *conn, batch.id, req.version, "CANCELLED", current.id).await?;
        if n == 0 {
            return Err(AppError::biz(code::VERSION_CONFLICT, "batch 版本冲突"));
        }
        let _ = Self::sync_from_batch_change(conn, part_id, current).await?;
        PartRepo::insert_part_event(
            &mut *conn,
            NewPartEvent {
                id: snowflake.next_id(),
                part_id,
                event_type: "BATCH_CANCELLED",
                from_status: Some(from.as_str()),
                to_status: Some("CANCELLED"),
                batch_id: Some(batch.id),
                quantity: Some(batch.quantity),
                drawing_code: Some(&part.drawing_no),
                badge_code: None,
                note: req.reason.as_deref(),
                created_by: Some(current.id),
            },
        )
        .await?;
        let fresh = PartRepo::get_part_inspected(&mut *conn, part_id)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_PART_NOT_FOUND, "cancel-batch 后查不到"))?;
        Ok(crate::modules::part::dto::PartOut::from(fresh))
    }

    /// `GET /parts/{id}/batches`：工单全部活跃批次。
    pub async fn list_batches(
        conn: &mut PgConnection,
        part_id: i64,
        current: &CurrentUser,
    ) -> Result<Vec<PartBatchListItemOut>, AppError> {
        current.require_any_role(&[
            Role::Manager,
            Role::Clerk,
            Role::Inspector,
            Role::CncProgrammer,
        ])?;
        let _ = PartRepo::get_part_inspected(&mut *conn, part_id).await?.ok_or_else(|| {
            AppError::biz(code::BIZ_PART_NOT_FOUND, "part 不存在")
        })?;
        let rows: Vec<BatchListRow> = sqlx::query_as::<_, BatchListRow>(
            "SELECT b.id AS id, b.batch_no, b.quantity, b.status, b.location,              b.current_holder_id, COALESCE(s.name, w.name, oc.name) AS holder_name,              b.next_process_id, b.placed_at, b.delivery_note_id, b.parent_batch_id,              b.has_been_repaired, b.version              FROM t_part_batch b              LEFT JOIN t_shelf s ON s.id = b.current_holder_id              LEFT JOIN t_worker w ON w.id = b.current_holder_id              LEFT JOIN t_outsource_company oc ON oc.id = b.current_holder_id              WHERE b.part_id = $1 AND b.deleted_at IS NULL              ORDER BY b.batch_no ASC",
        )
        .bind(part_id)
        .fetch_all(&mut *conn)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| PartBatchListItemOut {
                id: r.id,
                batch_no: r.batch_no,
                quantity: r.quantity,
                status: r.status,
                location: r.location,
                current_holder_id: r.current_holder_id,
                holder_name: r.holder_name,
                next_process_id: r.next_process_id,
                placed_at: r.placed_at,
                delivery_note_id: r.delivery_note_id,
                parent_batch_id: r.parent_batch_id,
                has_been_repaired: r.has_been_repaired,
                version: r.version,
            })
            .collect())
    }

    // ===== 1.6 事件历史 + 位置树 =====

    /// `GET /parts/{id}/events`：事件历史（按 created_at DESC）。
    pub async fn list_events(
        conn: &mut PgConnection,
        part_id: i64,
        current: &CurrentUser,
    ) -> Result<Vec<PartEventOut>, AppError> {
        current.require_any_role(&[
            Role::Manager,
            Role::Clerk,
            Role::Inspector,
            Role::CncProgrammer,
        ])?;
        let _ = PartRepo::get_part_inspected(&mut *conn, part_id).await?.ok_or_else(|| {
            AppError::biz(code::BIZ_PART_NOT_FOUND, "part 不存在")
        })?;
        let rows: Vec<EventListRow> = sqlx::query_as::<_, EventListRow>(
            "SELECT id, event_type, from_status, to_status, batch_id, quantity,              drawing_code, badge_code, note, created_at, created_by              FROM t_part_event WHERE part_id = $1 ORDER BY id DESC",
        )
        .bind(part_id)
        .fetch_all(&mut *conn)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| PartEventOut {
                id: r.id,
                event_type: r.event_type,
                from_status: r.from_status,
                to_status: r.to_status,
                batch_id: r.batch_id,
                quantity: r.quantity,
                drawing_code: r.drawing_code,
                badge_code: r.badge_code,
                note: r.note,
                created_at: r.created_at,
                created_by: r.created_by,
            })
            .collect())
    }

    /// `GET /parts/location-tree`：按 shelf/status 聚合位置树。
    pub async fn location_tree(
        conn: &mut PgConnection,
        current: &CurrentUser,
    ) -> Result<LocationTreeOut, AppError> {
        current.require_any_role(&[
            Role::Manager,
            Role::Clerk,
            Role::CncProgrammer,
            Role::Inspector,
        ])?;
        // 收集每个 shelf 的批次计数（按 location IN ('PRODUCTION_SHELF', 'INSPECTION_SHELF')）
        let shelf_counts: Vec<HolderCountRow> = sqlx::query_as::<_, HolderCountRow>(
            "SELECT b.current_holder_id AS holder_id, COUNT(*) AS n              FROM t_part_batch b JOIN t_part p ON p.id = b.part_id              WHERE b.deleted_at IS NULL AND p.deleted_at IS NULL              AND b.location IN ('PRODUCTION_SHELF', 'INSPECTION_SHELF')              AND b.status NOT IN ('CANCELLED', 'COMPLETED')              GROUP BY b.current_holder_id",
        )
        .fetch_all(&mut *conn)
        .await?;
        let mut shelf_count_map: std::collections::HashMap<i64, i64> =
            std::collections::HashMap::new();
        for r in shelf_counts {
            shelf_count_map.insert(r.holder_id, r.n);
        }
        // workers
        let worker_counts: Vec<HolderCountRow> = sqlx::query_as::<_, HolderCountRow>(
            "SELECT b.current_holder_id AS holder_id, COUNT(*) AS n              FROM t_part_batch b JOIN t_part p ON p.id = b.part_id              WHERE b.deleted_at IS NULL AND p.deleted_at IS NULL              AND b.location = 'WORKER'              AND b.status NOT IN ('CANCELLED', 'COMPLETED')              GROUP BY b.current_holder_id",
        )
        .fetch_all(&mut *conn)
        .await?;
        let mut worker_count_map: std::collections::HashMap<i64, i64> =
            std::collections::HashMap::new();
        for r in worker_counts {
            worker_count_map.insert(r.holder_id, r.n);
        }
        // outsource
        let outsource_counts: Vec<HolderCountRow> = sqlx::query_as::<_, HolderCountRow>(
            "SELECT b.current_holder_id AS holder_id, COUNT(*) AS n              FROM t_part_batch b JOIN t_part p ON p.id = b.part_id              WHERE b.deleted_at IS NULL AND p.deleted_at IS NULL              AND b.location = 'OUTSOURCE_COMPANY'              AND b.status NOT IN ('CANCELLED', 'COMPLETED')              GROUP BY b.current_holder_id",
        )
        .fetch_all(&mut *conn)
        .await?;
        let mut outsource_count_map: std::collections::HashMap<i64, i64> =
            std::collections::HashMap::new();
        for r in outsource_counts {
            outsource_count_map.insert(r.holder_id, r.n);
        }
        // OFFICE 计数：所有 PENDING / PROGRAMMING 状态的工单
        let office_count: i64 = sqlx::query_scalar(
            r#"
            SELECT COUNT(*) AS "n!"
            FROM t_part p
            WHERE p.deleted_at IS NULL
              AND p.status IN ('PENDING', 'PROGRAMMING')
            "#,
        )
        .fetch_one(&mut *conn)
        .await?;
        // 装载 active shelf / worker / outsource
        let shelves = ShelfRepo::list_with_filters(&mut *conn, None, None, Some(true), 500, 0)
            .await
            .unwrap_or_default();
        let workers = WorkerRepo::list_with_filters(&mut *conn, None, Some(true), 500, 0)
            .await
            .unwrap_or_default();
        // outsource 域为 Phase 2 stub，直接 SQL 取 active 列表
        let outsource_rows: Vec<(i64, String, bool)> = sqlx::query_as(
            "SELECT id, name, is_active FROM t_outsource_company WHERE deleted_at IS NULL",
        )
        .fetch_all(&mut *conn)
        .await
        .unwrap_or_default();
        let outsources: Vec<OutsourceLite> = outsource_rows
            .into_iter()
            .map(|(id, name, is_active)| OutsourceLite { id, name, is_active })
            .collect();
        let mut items: Vec<LocationTreeNodeOut> = Vec::new();
        // OFFICE 父节点
        items.push(LocationTreeNodeOut {
            id: "OFFICE".into(),
            label: "办公室".into(),
            kind: "OFFICE".into(),
            parent_id: None,
            count: office_count,
        });
        // PRODUCTION_SHELF 父节点
        let production_shelves: Vec<_> = shelves
            .iter()
            .filter(|s| s.zone == "PRODUCTION" && s.is_active)
            .collect();
        let production_total: i64 = production_shelves
            .iter()
            .filter_map(|s| shelf_count_map.get(&s.id).copied())
            .sum();
        items.push(LocationTreeNodeOut {
            id: "PRODUCTION_SHELF".into(),
            label: "生产货架".into(),
            kind: "PRODUCTION_SHELF".into(),
            parent_id: None,
            count: production_total,
        });
        for s in production_shelves {
            items.push(LocationTreeNodeOut {
                id: s.id.to_string(),
                label: format!("{} {}", s.code, s.name),
                kind: "SHELF".into(),
                parent_id: None,
                count: shelf_count_map.get(&s.id).copied().unwrap_or(0),
            });
        }
        // WORKER 父节点
        let workers_active: Vec<_> = workers.iter().filter(|w| w.is_active).collect();
        let worker_total: i64 = workers_active
            .iter()
            .filter_map(|w| worker_count_map.get(&w.id).copied())
            .sum();
        items.push(LocationTreeNodeOut {
            id: "WORKER".into(),
            label: "工人".into(),
            kind: "WORKER".into(),
            parent_id: None,
            count: worker_total,
        });
        for w in workers_active {
            items.push(LocationTreeNodeOut {
                id: w.id.to_string(),
                label: w.name.clone(),
                kind: "WORKER".into(),
                parent_id: None,
                count: worker_count_map.get(&w.id).copied().unwrap_or(0),
            });
        }
        // INSPECTION_SHELF 父节点
        let inspection_shelves: Vec<_> = shelves
            .iter()
            .filter(|s| s.zone == "INSPECTION" && s.is_active)
            .collect();
        let inspection_total: i64 = inspection_shelves
            .iter()
            .filter_map(|s| shelf_count_map.get(&s.id).copied())
            .sum();
        items.push(LocationTreeNodeOut {
            id: "INSPECTION_SHELF".into(),
            label: "品检货架".into(),
            kind: "INSPECTION_SHELF".into(),
            parent_id: None,
            count: inspection_total,
        });
        for s in inspection_shelves {
            items.push(LocationTreeNodeOut {
                id: s.id.to_string(),
                label: format!("{} {}", s.code, s.name),
                kind: "SHELF".into(),
                parent_id: None,
                count: shelf_count_map.get(&s.id).copied().unwrap_or(0),
            });
        }
        // OUTSOURCE_COMPANY 父节点
        let outsources_active: Vec<_> = outsources.iter().filter(|o| o.is_active).collect();
        let outsource_total: i64 = outsources_active
            .iter()
            .filter_map(|o| outsource_count_map.get(&o.id).copied())
            .sum();
        items.push(LocationTreeNodeOut {
            id: "OUTSOURCE_COMPANY".into(),
            label: "外协公司".into(),
            kind: "OUTSOURCE_COMPANY".into(),
            parent_id: None,
            count: outsource_total,
        });
        for o in outsources_active {
            items.push(LocationTreeNodeOut {
                id: o.id.to_string(),
                label: o.name.clone(),
                kind: "OUTSOURCE_COMPANY".into(),
                parent_id: None,
                count: outsource_count_map.get(&o.id).copied().unwrap_or(0),
            });
        }
        Ok(LocationTreeOut { items })
    }

    // ===== 1.7 扫码检 / 司机扫码 =====

    /// `POST /parts/{id}/scan-inspect`：扫码快捷品检（一步式）。
    ///
    /// `{PENDING, PROGRAMMING, IN_PROCESS}` → INSPECTION（target_shelf）→ READY_TO_SHIP（pass=true）
    /// 或 → REPAIRING（pass=false + shelf_id + next_process_id）。
    pub async fn scan_inspect(
        conn: &mut PgConnection,
        snowflake: &SnowflakeIdGenerator,
        part_id: i64,
        req: ScanInspectRequest,
        current: &CurrentUser,
    ) -> Result<crate::modules::part::dto::PartOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Inspector])?;
        let part = PartRepo::get_part_inspected(&mut *conn, part_id)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_PART_NOT_FOUND, "part 不存在"))?;
        let batch = PartRepo::find_batch_by_id(&mut *conn, req.batch_id)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_PART_BATCH_NOT_FOUND, "batch 不存在"))?;
        validate_batch_ownership(batch.part_id, batch.id, part_id, req.version, batch.version)?;
        let from = PartStatus::from_str(&batch.status)
            .ok_or_else(|| AppError::biz(code::BIZ_INVALID_VALUE, "batch.status 非法"))?;
        // 入口白名单
        if !matches!(
            from,
            PartStatus::PENDING | PartStatus::PROGRAMMING | PartStatus::IN_PROCESS
        ) {
            return Err(AppError::biz(
                code::BIZ_INVALID_TRANSITION,
                format!("scan-inspect: 起点 {from:?} 不允许"),
            ));
        }
        validate_shelf_zone(conn, req.target_inspection_shelf_id, "INSPECTION").await?;
        // 第一步：到 INSPECTION
        let n1 = mark_batch_with_status_and_meta(
            &mut *conn,
            batch.id,
            req.version,
            "INSPECTION",
            Some("INSPECTION_SHELF"),
            Some(req.target_inspection_shelf_id),
            None,
            current.id,
        )
        .await?;
        if n1 == 0 {
            return Err(AppError::biz(code::VERSION_CONFLICT, "batch 版本冲突"));
        }
        let mid_version = batch.version + 1;
        // 第二步：pass=true → READY_TO_SHIP；pass=false → REPAIRING
        if req.pass {
            let n2 = mark_batch_status_only(
                &mut *conn,
                batch.id,
                mid_version,
                "READY_TO_SHIP",
                current.id,
            )
            .await?;
            if n2 == 0 {
                return Err(AppError::biz(code::VERSION_CONFLICT, "batch 版本冲突（INSPECTION→READY_TO_SHIP）"));
            }
        } else {
            // FAIL：INSPECTION → REPAIRING；保留 shelf 为 INSPECTION_SHELF（carry 状态由下一步 complete_repair 接管）
            let n2 = mark_batch_status_only(&mut *conn, batch.id, mid_version, "REPAIRING", current.id).await?;
            if n2 == 0 {
                return Err(AppError::biz(code::VERSION_CONFLICT, "batch 版本冲突（INSPECTION→REPAIRING）"));
            }
            sqlx::query(
                "UPDATE t_part_batch SET has_been_repaired = TRUE \
                 WHERE id = $1 AND has_been_repaired = FALSE")
            .bind(batch.id)
            .execute(&mut *conn)
            .await?;
        }
        let _ = Self::sync_from_batch_change(conn, part_id, current).await?;
        // 事件日志（两条：INSPECTED + INSPECTION_RESULT）
        PartRepo::insert_part_event(
            &mut *conn,
            NewPartEvent {
                id: snowflake.next_id(),
                part_id,
                event_type: "INSPECTED",
                from_status: Some(from.as_str()),
                to_status: Some("INSPECTION"),
                batch_id: Some(batch.id),
                quantity: Some(batch.quantity),
                drawing_code: Some(&part.drawing_no),
                badge_code: None,
                note: None,
                created_by: Some(current.id),
            },
        )
        .await?;
        let (to_status, event_type) = if req.pass {
            ("READY_TO_SHIP", "BATCH_PASSED")
        } else {
            ("REPAIRING", "INSPECTION_FAILED")
        };
        PartRepo::insert_part_event(
            &mut *conn,
            NewPartEvent {
                id: snowflake.next_id(),
                part_id,
                event_type,
                from_status: Some("INSPECTION"),
                to_status: Some(to_status),
                batch_id: Some(batch.id),
                quantity: Some(batch.quantity),
                drawing_code: Some(&part.drawing_no),
                badge_code: None,
                note: req.note.as_deref(),
                created_by: Some(current.id),
            },
        )
        .await?;
        let fresh = PartRepo::get_part_inspected(&mut *conn, part_id)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_PART_NOT_FOUND, "scan-inspect 后查不到"))?;
        Ok(crate::modules::part::dto::PartOut::from(fresh))
    }

    /// `POST /parts/scan/deliver-part`：司机扫码发货。
    /// `part_serial_no` 反查 part_id；`worker_badge_code` 校验必须是「送货司机」工种。
    /// 状态机：`READY_TO_SHIP` → `DELIVERED`（复用 `deliver` 流程的核心）。
    pub async fn scan_deliver_part(
        conn: &mut PgConnection,
        snowflake: &SnowflakeIdGenerator,
        req: ScanDeliverPartRequest,
        current: &CurrentUser,
    ) -> Result<crate::modules::part::dto::PartOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::ShelfAccount])?;
        // 反查 part
        let part: Option<crate::modules::part::model::TPart> = sqlx::query_as::<_, crate::modules::part::model::TPart>(
            "SELECT id, serial_no, name, drawing_no, applicant_name, quantity, \
             request_date, planned_delivery_date, actual_delivery_date, \
             customer_id, assembly_id, status, location, \
             is_urgent, current_holder_id, placed_at, next_process_id, \
             order_no, system_delivery_date, note, has_been_repaired, \
             version, created_at, created_by, updated_at, updated_by, \
             deleted_at, delivery_note_id, process_chain_id \
             FROM t_part WHERE serial_no = $1 AND deleted_at IS NULL",
        )
        .bind(&req.part_serial_no)
        .fetch_optional(&mut *conn)
        .await?;
        let part = part.ok_or_else(|| {
            AppError::biz(code::BIZ_PART_NOT_FOUND, format!("serial_no {} 找不到 part", req.part_serial_no))
        })?;
        // 校验 worker 是送货司机
        let worker = WorkerRepo::get_by_badge_code(&mut *conn, &req.worker_badge_code, false)
            .await?
            .ok_or_else(|| {
                AppError::biz(code::BIZ_WORKER_NOT_FOUND, "工牌码无效")
            })?;
        if !worker.is_active {
            return Err(AppError::biz(code::BIZ_WORKER_INACTIVE, "工人已停用"));
        }
        // 校验工种
        let wt_code: Option<String> = if let Some(wt_id) = worker.work_type_id {
            sqlx::query_scalar("SELECT code FROM t_work_type WHERE id = $1 AND deleted_at IS NULL")
                .bind(wt_id)
                .fetch_optional(&mut *conn)
                .await?
        } else {
            None
        };
        if wt_code.as_deref() != Some("DRIVER") {
            return Err(AppError::biz(
                code::BIZ_DELIVERY_NOTE_DRIVER_INVALID,
                format!("工牌 {} 的工种不是 DRIVER", req.worker_badge_code),
            ));
        }
        // 校验 part.status == READY_TO_SHIP
        let from = PartStatus::from_str(&part.status).ok_or_else(|| {
            AppError::biz(code::BIZ_INVALID_VALUE, "part.status 非法")
        })?;
        if from != PartStatus::READY_TO_SHIP {
            return Err(AppError::biz(
                code::BIZ_DELIVERY_NOTE_PART_NOT_READY,
                format!("part {} 当前 {} 不允许 deliver", part.id, from.as_str()),
            ));
        }
        // 找 READY_TO_SHIP 批次
        let batch = PartRepo::find_inprocess_batch_for_part(&mut *conn, part.id, None)
            .await
            .map_err(|_| AppError::biz(code::BIZ_PART_BATCH_NOT_FOUND, "批次不存在"))?;
        let batch = match batch {
            Some(b) if b.status == "READY_TO_SHIP" => b,
            _ => {
                return Err(AppError::biz(
                    code::BIZ_PART_BATCH_NOT_FOUND,
                    "找不到 READY_TO_SHIP 批次",
                ));
            }
        };
        let n = PartRepo::mark_batch_delivered(&mut *conn, batch.id, batch.version, current.id).await?;
        if n == 0 {
            return Err(AppError::biz(code::VERSION_CONFLICT, "batch 版本冲突"));
        }
        // 写 actual_delivery_date
        sqlx::query(
            "UPDATE t_part SET actual_delivery_date = CURRENT_DATE, version = version + 1, \
             updated_at = now(), updated_by = $1 WHERE id = $2 AND deleted_at IS NULL")
            .bind(current.id)
            .bind(part.id)
        .execute(&mut *conn)
        .await?;
        let _ = Self::sync_from_batch_change(conn, part.id, current).await?;
        // 事件
        PartRepo::insert_part_event(
            &mut *conn,
            NewPartEvent {
                id: snowflake.next_id(),
                part_id: part.id,
                event_type: "DELIVERED",
                from_status: Some("READY_TO_SHIP"),
                to_status: Some("DELIVERED"),
                batch_id: Some(batch.id),
                quantity: Some(batch.quantity),
                drawing_code: Some(&part.drawing_no),
                badge_code: Some(&req.worker_badge_code),
                note: req.note.as_deref(),
                created_by: Some(current.id),
            },
        )
        .await?;
        let fresh = PartRepo::get_part_inspected(&mut *conn, part.id)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_PART_NOT_FOUND, "scan_deliver 后查不到"))?;
        Ok(crate::modules::part::dto::PartOut::from(fresh))
    }

    // ===== 1.8 批量创建增强 =====

    /// `POST /parts/batch-with-pdfs`：multipart JSON + PDFs。
    ///
    /// 2026-09-14 Phase 3 完整化：
    /// - **page1 = master part**：建装配件 master（PENDING），serial 走 L1 客户 serial_prefix。
    /// - **page2..N = auto-created children parts**：serial 派生 `{master_serial}-{i:02d}`，
    ///   上限 99（与 `BIZ_ASSEMBLY_TOO_MANY_CHILDREN` 保持一致）。
    /// - **page_count == 1**：仅创建 master，不派发子件（与之前行为一致）。
    /// - **page_count == 0**（无 PDF）：仅创建 master，无 serial（与之前行为一致）。
    pub async fn batch_with_pdfs(
        conn: &mut PgConnection,
        snowflake: &SnowflakeIdGenerator,
        req: &BatchWithPdfsRequest,
        pdf_files: &[Vec<u8>],
        current: &CurrentUser,
    ) -> Result<crate::modules::part::dto_crud::PartDetailOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk])?;
        if req.customer_id == 0 {
            return Err(AppError::biz(code::BIZ_CUSTOMER_NOT_FOUND, "customer_id 必填"));
        }
        let _customer = CustomerRepo::get_by_id(&mut *conn, req.customer_id, false)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_CUSTOMER_NOT_FOUND, "customer 不存在"))?;
        // 解析 PDF 总页数
        let mut page_count: i32 = 0;
        if !pdf_files.is_empty() {
            for pdf in pdf_files {
                let doc = lopdf::Document::load_mem(pdf).map_err(|e| {
                    AppError::biz(
                        code::BIZ_ASSEMBLY_PDF_INVALID,
                        format!("PDF 解析失败: {e}"),
                    )
                })?;
                page_count += doc.get_pages().len() as i32;
            }
        }
        // 子件数量上限 99（page2..N 共 99 个）
        let child_count = std::cmp::max(0, page_count - 1);
        if child_count > 99 {
            return Err(AppError::biz(
                code::BIZ_ASSEMBLY_TOO_MANY_CHILDREN,
                format!("batch-with-pdfs 子件最多 99 个，当前 PDF 页数={page_count}"),
            ));
        }

        let today = chrono::Local::now().date_naive();
        let new_id = snowflake.next_id();
        let name = format!("装配件-{}", today.format("%Y%m%d"));
        let drawing_no = format!("ASM-{}", today.format("%Y%m%d"));

        // 若有 PDF → 派 master serial（从 L1 客户 serial_prefix 拿）
        let master_serial: Option<String> = if page_count > 0 {
            let l1_id: i64 = sqlx::query_scalar(
                "SELECT COALESCE(parent_id, id) FROM t_customer WHERE id = $1 AND deleted_at IS NULL",
            )
            .bind(req.customer_id)
            .fetch_one(&mut *conn)
            .await?;
            let prefix_str: Option<String> = sqlx::query_scalar(
                "SELECT serial_prefix FROM t_customer WHERE id = $1 AND deleted_at IS NULL",
            )
            .bind(l1_id)
            .fetch_one(&mut *conn)
            .await?;
            let p = prefix_str.ok_or_else(|| {
                AppError::biz(code::BIZ_CUSTOMER_NO_SERIAL_PREFIX, "L1 客户无 serial_prefix")
            })?;
            let ch = p.chars().next().ok_or_else(|| {
                AppError::biz(code::BIZ_INVALID_VALUE, "serial_prefix 为空")
            })?;
            Some(crate::shared::serial::acquire(&mut *conn, ch).await?)
        } else {
            None
        };

        let new = crate::modules::part::repo::part::NewPartCreate {
            id: new_id,
            name: &name,
            drawing_no: &drawing_no,
            applicant_name: req.applicant_name.as_deref().unwrap_or(""),
            quantity: 1,
            request_date: req.request_date.unwrap_or(today),
            planned_delivery_date: req.planned_delivery_date.unwrap_or(today),
            is_urgent: req.is_urgent.unwrap_or(false),
            customer_id: req.customer_id,
            assembly_id: None,
            order_no: None,
            system_delivery_date: None,
            note: req.note.as_deref(),
            created_by: current.id,
        };
        PartRepo::create_part(&mut *conn, new).await?;
        // master 设置 serial_no（仅在有 PDF 时）
        if let Some(sn) = &master_serial {
            sqlx::query("UPDATE t_part SET serial_no = $1 WHERE id = $2 AND deleted_at IS NULL")
                .bind(sn)
                .bind(new_id)
                .execute(&mut *conn)
                .await?;
        }
        // 初始批次
        let initial_batch_id = snowflake.next_id();
        PartBatchRepo::create_initial_batch(
            &mut *conn,
            crate::modules::part_batch::repo::NewInitialBatch {
                id: initial_batch_id,
                part_id: new_id,
                quantity: 1,
                location: None,
                created_by: Some(current.id),
            },
        )
        .await?;

        // 自动派发子件（page2..N）
        if child_count > 0 {
            let master_serial = master_serial.as_deref().expect("master_serial present when child_count > 0");
            for i in 1..=child_count {
                let child_id = snowflake.next_id();
                let child_serial = format!("{}-{:02}", master_serial, i);
                let child = crate::modules::part::repo::part::NewPartCreate {
                    id: child_id,
                    name: &format!("{name}-{:02}", i),
                    drawing_no: &format!("{drawing_no}-{:02}", i),
                    applicant_name: req.applicant_name.as_deref().unwrap_or(""),
                    quantity: 1,
                    request_date: req.request_date.unwrap_or(today),
                    planned_delivery_date: req.planned_delivery_date.unwrap_or(today),
                    is_urgent: req.is_urgent.unwrap_or(false),
                    customer_id: req.customer_id,
                    assembly_id: None,
                    order_no: None,
                    system_delivery_date: None,
                    note: req.note.as_deref(),
                    created_by: current.id,
                };
                PartRepo::create_part(&mut *conn, child).await?;
                sqlx::query("UPDATE t_part SET serial_no = $1 WHERE id = $2 AND deleted_at IS NULL")
                    .bind(&child_serial)
                    .bind(child_id)
                    .execute(&mut *conn)
                    .await?;
                // 初始批次
                PartBatchRepo::create_initial_batch(
                    &mut *conn,
                    crate::modules::part_batch::repo::NewInitialBatch {
                        id: snowflake.next_id(),
                        part_id: child_id,
                        quantity: 1,
                        location: None,
                        created_by: Some(current.id),
                    },
                )
                .await?;
            }
        }

        // 重读 master
        let part: crate::modules::part::model::TPart = PartRepo::get_by_id(&mut *conn, new_id, false)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_PART_NOT_FOUND, "create 后查不到"))?;
        Ok(crate::modules::part::dto_crud::PartDetailOut::from_with_customer_extra(
            part,
            None,
            None,
            None,
        ))
    }

    /// `POST /parts/match-by-excel-items`：Excel 行（drawing_no 或 serial_no）→ 现有 part id。
    pub async fn match_by_excel_items(
        conn: &mut PgConnection,
        req: &MatchByExcelItemsRequest,
        current: &CurrentUser,
    ) -> Result<Vec<MatchByExcelItemResult>, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk])?;
        let mut out: Vec<MatchByExcelItemResult> = Vec::new();
        for item in &req.items {
            // 先尝试 serial_no
            if let Some(sn) = item.serial_no.as_deref().filter(|s| !s.is_empty()) {
                let rows: Vec<(i64,)> = sqlx::query_as::<_, (i64,)>(
                    "SELECT id FROM t_part WHERE serial_no = $1 AND deleted_at IS NULL",
                )
                .bind(sn)
                .fetch_all(&mut *conn)
                .await?;
                if rows.len() == 1 {
                    out.push(MatchByExcelItemResult {
                        drawing_no: item.drawing_no.clone(),
                        serial_no: Some(sn.into()),
                        part_id: Some(rows[0].0),
                        status: "MATCHED".into(),
                        message: None,
                    });
                    continue;
                }
                if rows.is_empty() {
                    out.push(MatchByExcelItemResult {
                        drawing_no: item.drawing_no.clone(),
                        serial_no: Some(sn.into()),
                        part_id: None,
                        status: "NOT_FOUND".into(),
                        message: Some("serial_no 找不到".into()),
                    });
                    continue;
                }
                out.push(MatchByExcelItemResult {
                    drawing_no: item.drawing_no.clone(),
                    serial_no: Some(sn.into()),
                    part_id: None,
                    status: "AMBIGUOUS".into(),
                    message: Some(format!("{} 个匹配", rows.len())),
                });
                continue;
            }
            // 再尝试 drawing_no（取最近一条 active）
            if let Some(dn) = item.drawing_no.as_deref().filter(|s| !s.is_empty()) {
                let rows: Vec<(i64,)> = sqlx::query_as::<_, (i64,)>(
                    "SELECT id FROM t_part WHERE drawing_no = $1 AND deleted_at IS NULL \
                     ORDER BY id DESC LIMIT 5",
                )
                .bind(dn)
                .fetch_all(&mut *conn)
                .await?;
                if rows.len() == 1 {
                    out.push(MatchByExcelItemResult {
                        drawing_no: Some(dn.into()),
                        serial_no: item.serial_no.clone(),
                        part_id: Some(rows[0].0),
                        status: "MATCHED".into(),
                        message: None,
                    });
                    continue;
                }
                if rows.is_empty() {
                    out.push(MatchByExcelItemResult {
                        drawing_no: Some(dn.into()),
                        serial_no: item.serial_no.clone(),
                        part_id: None,
                        status: "NOT_FOUND".into(),
                        message: Some("drawing_no 找不到".into()),
                    });
                    continue;
                }
                out.push(MatchByExcelItemResult {
                    drawing_no: Some(dn.into()),
                    serial_no: item.serial_no.clone(),
                    part_id: None,
                    status: "AMBIGUOUS".into(),
                    message: Some(format!("{} 个匹配", rows.len())),
                });
                continue;
            }
            out.push(MatchByExcelItemResult {
                drawing_no: item.drawing_no.clone(),
                serial_no: item.serial_no.clone(),
                part_id: None,
                status: "NOT_FOUND".into(),
                message: Some("serial_no 和 drawing_no 均缺失".into()),
            });
        }
        Ok(out)
    }

    /// `POST /parts/batch-update-order-info`：批量回填 order_no / system_delivery_date / note。
    pub async fn batch_update_order_info(
        conn: &mut PgConnection,
        req: &BatchUpdateOrderInfoRequest,
        current: &CurrentUser,
    ) -> Result<BatchUpdateOrderInfoOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk])?;
        if req.items.is_empty() {
            return Err(AppError::validation("items 不能为空"));
        }
        let mut updated = 0_i64;
        let mut failed: Vec<BatchUpdateOrderInfoFailure> = Vec::new();
        for item in &req.items {
            let upd = PartUpdate {
                name: None,
                drawing_no: None,
                applicant_name: None,
                quantity: None,
                order_no: item.order_no.as_deref(),
                system_delivery_date: item.system_delivery_date,
                planned_delivery_date: None,
                actual_delivery_date: None,
                note: item.note.as_deref(),
                is_urgent: None,
                updated_by: current.id,
            };
            let n = PartRepo::update_part(&mut *conn, item.part_id, item.version, upd).await;
            match n {
                Ok(1) => updated += 1,
                Ok(_) => failed.push(BatchUpdateOrderInfoFailure {
                    part_id: item.part_id,
                    code: code::VERSION_CONFLICT,
                    message: "版本冲突或 part 已软删".into(),
                }),
                Err(e) => failed.push(BatchUpdateOrderInfoFailure {
                    part_id: item.part_id,
                    code: code::DATABASE,
                    message: format!("{e}"),
                }),
            }
        }
        Ok(BatchUpdateOrderInfoOut { updated, failed })
    }

    // ===== 1.3 外协辅助列表 =====

    /// `GET /parts/outsource-in-flight`：status=OUTSOURCE 工单一览。
    /// 简化版：复用 `list_parts` 但强制 status=OUTSOURCE。
    pub async fn list_outsource_in_flight(
        conn: &mut PgConnection,
        query: &super::super::dto_crud::PartListQuery,
        current: &CurrentUser,
    ) -> Result<PartListOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk])?;
        let limit = query.limit.unwrap_or(50).clamp(1, 200);
        let offset = query.offset.unwrap_or(0).max(0);
        let f = PartListFilters {
            customer_ids: &[],
            status: Some("OUTSOURCE"),
            statuses: &[],
            is_urgent: query.is_urgent,
            keyword: Some(query.keyword.as_deref().unwrap_or("")),
            sort_by: match query.sort_by.as_deref().unwrap_or("PLANNED_DELIVERY_DATE") {
                "CREATED_AT" => "created_at",
                "UPDATED_AT" => "updated_at",
                "PLANNED_DELIVERY_DATE" => "planned_delivery_date",
                "REQUEST_DATE" => "request_date",
                "SERIAL_NO" => "serial_no",
                "DRAWING_NO" => "drawing_no",
                "NAME" => "name",
                _ => "planned_delivery_date",
            },
            sort_dir: query.sort_dir.as_deref().unwrap_or("ASC"),
            limit,
            offset,
            include_deleted: false,
        };
        let items = PartRepo::list_with_filters(&mut *conn, &f).await?;
        let total = PartRepo::count_with_filters(&mut *conn, &f).await?;
        let list_items: Vec<super::super::dto_crud::PartListItem> = items
            .into_iter()
            .map(|p| super::super::dto_crud::PartListItem {
                part: p,
                customer_name: None,
                l1_customer_name: None,
            })
            .collect();
        Ok(PartListOut {
            items: list_items,
            total,
            limit,
            offset,
        })
    }

    /// `GET /parts/outsource-sendable`：可发外协的零件。
    /// 简化版：status ∈ {PENDING, IN_PROCESS}（service 层不引入 OUTSOURCE_QUOTE 表的依赖）。
    pub async fn list_outsource_sendable(
        conn: &mut PgConnection,
        query: &super::super::dto_crud::PartListQuery,
        current: &CurrentUser,
    ) -> Result<PartListOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk, Role::Inspector])?;
        let limit = query.limit.unwrap_or(50).clamp(1, 200);
        let offset = query.offset.unwrap_or(0).max(0);
        let f = PartListFilters {
            customer_ids: &[],
            status: None,
            statuses: &["PENDING".into(), "IN_PROCESS".into()],
            is_urgent: query.is_urgent,
            keyword: Some(query.keyword.as_deref().unwrap_or("")),
            sort_by: match query.sort_by.as_deref().unwrap_or("PLANNED_DELIVERY_DATE") {
                "CREATED_AT" => "created_at",
                "UPDATED_AT" => "updated_at",
                "PLANNED_DELIVERY_DATE" => "planned_delivery_date",
                "REQUEST_DATE" => "request_date",
                "SERIAL_NO" => "serial_no",
                "DRAWING_NO" => "drawing_no",
                "NAME" => "name",
                _ => "planned_delivery_date",
            },
            sort_dir: query.sort_dir.as_deref().unwrap_or("ASC"),
            limit,
            offset,
            include_deleted: false,
        };
        let items = PartRepo::list_with_filters(&mut *conn, &f).await?;
        let total = PartRepo::count_with_filters(&mut *conn, &f).await?;
        let list_items: Vec<super::super::dto_crud::PartListItem> = items
            .into_iter()
            .map(|p| super::super::dto_crud::PartListItem {
                part: p,
                customer_name: None,
                l1_customer_name: None,
            })
            .collect();
        Ok(PartListOut {
            items: list_items,
            total,
            limit,
            offset,
        })
    }

    // ===== Phase 2 (2026-09-13) — 领取链路 (B 方案：手动 pick-up 兜底) =====

    /// `POST /parts/{id}/pick-up`：手动 pick-up。
    /// 起点：PENDING / IN_PROCESS+PRODUCTION_SHELF → 目标 IN_PROCESS+WORKER。
    ///
    /// 不变量：
    /// - worker 必须 is_active 且 work_type_id 不为 NULL
    /// - shelf 必须 zone=PRODUCTION 且 active
    /// - 状态机迁移：`{PENDING, IN_PROCESS} → IN_PROCESS`（DB 状态相同；service 守 location）
    /// - 写 PICKED_UP 事件 + 广播 PART_PICKED_UP WS
    pub async fn pick_up(
        conn: &mut PgConnection,
        snowflake: &SnowflakeIdGenerator,
        part_id: i64,
        req: PickUpRequest,
        current: &CurrentUser,
    ) -> Result<crate::modules::part::dto::PartOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk, Role::ShelfAccount])?;
        let part = PartRepo::get_part_inspected(&mut *conn, part_id)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_PART_NOT_FOUND, "part 不存在"))?;
        let batch = PartRepo::find_batch_by_id(&mut *conn, req.batch_id)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_PART_BATCH_NOT_FOUND, "batch 不存在"))?;
        validate_batch_ownership(batch.part_id, batch.id, part_id, req.version, batch.version)?;
        let from = PartStatus::from_str(&batch.status)
            .ok_or_else(|| AppError::biz(code::BIZ_INVALID_VALUE, "batch.status 非法"))?;
        if from != PartStatus::PENDING && from != PartStatus::IN_PROCESS {
            return Err(AppError::biz(
                code::BIZ_INVALID_TRANSITION,
                format!("pick-up 起点 {from:?} 不允许"),
            ));
        }
        if from == PartStatus::IN_PROCESS && batch.location.as_deref() != Some("PRODUCTION_SHELF") {
            return Err(AppError::biz(
                code::BIZ_INVALID_TRANSITION,
                "pick-up: IN_PROCESS 批次必须在 PRODUCTION_SHELF 上",
            ));
        }
        // worker 必须 active + 有 work_type
        let worker: Option<(bool, Option<i64>)> = sqlx::query_as(
            "SELECT is_active, work_type_id FROM t_worker \
             WHERE id = $1 AND deleted_at IS NULL",
        )
        .bind(req.worker_id)
        .fetch_optional(&mut *conn)
        .await?;
        let (is_active, wt_id) = worker.ok_or_else(|| {
            AppError::biz(code::BIZ_WORKER_NOT_FOUND, format!("worker {} 不存在", req.worker_id))
        })?;
        if !is_active {
            return Err(AppError::biz(
                code::BIZ_WORKER_INACTIVE,
                format!("worker {} 已停用", req.worker_id),
            ));
        }
        if wt_id.is_none() {
            return Err(AppError::biz(
                code::BIZ_WORKER_NO_WORK_TYPE,
                format!("worker {} 未绑定 work_type", req.worker_id),
            ));
        }
        validate_shelf_zone(conn, req.shelf_id, "PRODUCTION").await?;
        // 翻状态：PENDING → IN_PROCESS+WORKER；IN_PROCESS+PRODUCTION_SHELF → IN_PROCESS+WORKER
        let n = if from == PartStatus::PENDING {
            mark_batch_with_status_and_meta(
                &mut *conn,
                batch.id,
                req.version,
                "IN_PROCESS",
                Some("WORKER"),
                Some(req.worker_id),
                batch.next_process_id,
                current.id,
            )
            .await?
        } else {
            // IN_PROCESS：只翻 location+holder，status 保持 IN_PROCESS
            sqlx::query(
                "UPDATE t_part_batch SET location = 'WORKER', current_holder_id = $3, \
                 placed_at = COALESCE(placed_at, now()), \
                 version = version + 1, updated_at = now(), updated_by = $4 \
                 WHERE id = $1 AND version = $2 AND status = 'IN_PROCESS' \
                   AND deleted_at IS NULL",
            )
            .bind(batch.id)
            .bind(req.version)
            .bind(req.worker_id)
            .bind(current.id)
            .execute(&mut *conn)
            .await?
            .rows_affected()
        };
        if n == 0 {
            return Err(AppError::biz(code::VERSION_CONFLICT, "batch 版本冲突"));
        }
        let _ = Self::sync_from_batch_change(conn, part_id, current).await?;
        PartRepo::insert_part_event(
            &mut *conn,
            NewPartEvent {
                id: snowflake.next_id(),
                part_id,
                event_type: "PICKED_UP",
                from_status: Some(from.as_str()),
                to_status: Some("IN_PROCESS"),
                batch_id: Some(batch.id),
                quantity: Some(batch.quantity),
                drawing_code: Some(&part.drawing_no),
                badge_code: None,
                note: req.note.as_deref(),
                created_by: Some(current.id),
            },
        )
        .await?;
        let fresh = PartRepo::get_part_inspected(&mut *conn, part_id)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_PART_NOT_FOUND, "pick-up 后查不到"))?;
        Ok(crate::modules::part::dto::PartOut::from(fresh))
    }

    /// `GET /parts/by-work-type/{work_type_id}`：可领件（按工种过滤）。
    ///
    /// 实现：worker.work_type_id = $1 → t_part_batch.current_holder_id = worker.id，
    /// 且 batch.location='WORKER'。简化：直接按 worker 反查（每工种有多个 worker）。
    pub async fn list_by_work_type(
        conn: &mut PgConnection,
        work_type_id: i64,
        query: &ByWorkTypeQuery,
        current: &CurrentUser,
    ) -> Result<super::super::dto_crud::PartListOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk, Role::Inspector, Role::ShelfAccount])?;
        let limit = query.limit.unwrap_or(50).clamp(1, 200);
        let offset = query.offset.unwrap_or(0).max(0);
        // 直接列出该工种所有 worker 当前持有的件（IN_PROCESS + location=WORKER）
        let rows: Vec<(i64, String, String, i32, i64, Option<String>)> = sqlx::query_as(
            "SELECT p.id, p.serial_no, p.drawing_no, b.quantity, b.id AS bid, w.name AS worker_name \
             FROM t_part_batch b \
             JOIN t_part p ON p.id = b.part_id \
             JOIN t_worker w ON w.id = b.current_holder_id \
             WHERE b.deleted_at IS NULL AND p.deleted_at IS NULL \
               AND w.deleted_at IS NULL AND w.is_active = true \
               AND w.work_type_id = $1 \
               AND b.status = 'IN_PROCESS' AND b.location = 'WORKER' \
             ORDER BY b.id DESC LIMIT $2 OFFSET $3",
        )
        .bind(work_type_id)
        .bind(limit)
        .bind(offset)
        .fetch_all(&mut *conn)
        .await?;
        let items: Vec<super::super::dto_crud::PartListItem> = rows
            .into_iter()
            .map(|(id, serial, drawing, qty, bid, worker_name)| {
                let p = crate::modules::part::model::TPart {
                    id,
                    serial_no: Some(serial),
                    name: drawing.clone(),
                    drawing_no: drawing,
                    applicant_name: String::new(),
                    quantity: qty,
                    request_date: chrono::NaiveDate::from_ymd_opt(1970, 1, 1).unwrap(),
                    planned_delivery_date: chrono::NaiveDate::from_ymd_opt(1970, 1, 1).unwrap(),
                    actual_delivery_date: None,
                    customer_id: 0,
                    assembly_id: None,
                    status: "IN_PROCESS".to_string(),
                    location: Some("WORKER".to_string()),
                    is_urgent: false,
                    current_holder_id: None,
                    placed_at: None,
                    next_process_id: None,
                    order_no: None,
                    system_delivery_date: None,
                    note: None,
                    has_been_repaired: false,
                    version: 0,
                    created_at: chrono::NaiveDateTime::from_timestamp_opt(0, 0).unwrap(),
                    created_by: None,
                    updated_at: chrono::NaiveDateTime::from_timestamp_opt(0, 0).unwrap(),
                    updated_by: None,
                    deleted_at: None,
                    delivery_note_id: None,
                    process_chain_id: None,
                };
                let item: super::super::dto_crud::PartListItem =
                    super::super::dto_crud::PartListItem {
                        part: p,
                        customer_name: None,
                        l1_customer_name: None,
                    };
                // 附加 worker_name（轻量：DTO 上没字段，仅放 batch_id 展示）
                let _ = bid;
                let _ = worker_name;
                item
            })
            .collect();
        let total: i64 = sqlx::query_scalar(
            "SELECT COUNT(*)::bigint FROM t_part_batch b \
             JOIN t_worker w ON w.id = b.current_holder_id \
             WHERE b.deleted_at IS NULL AND w.deleted_at IS NULL \
               AND w.is_active = true AND w.work_type_id = $1 \
               AND b.status = 'IN_PROCESS' AND b.location = 'WORKER'",
        )
        .bind(work_type_id)
        .fetch_one(&mut *conn)
        .await?;
        Ok(super::super::dto_crud::PartListOut {
            items,
            total,
            limit,
            offset,
        })
    }

    /// `GET /parts/pickable-by-work-type/{work_type_id}`：可领取件（货架上、绑了对应工序）。
    pub async fn list_pickable_by_work_type(
        conn: &mut PgConnection,
        work_type_id: i64,
        query: &ByWorkTypeQuery,
        current: &CurrentUser,
    ) -> Result<super::super::dto_crud::PartListOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk, Role::Inspector, Role::ShelfAccount])?;
        let limit = query.limit.unwrap_or(50).clamp(1, 200);
        let offset = query.offset.unwrap_or(0).max(0);
        let shelf_filter = query.shelf_id;
        // 列：t_part_batch WHERE location=PRODUCTION_SHELF AND batch.next_process_id IN (工种→工序映射)
        let rows: Vec<(i64, String, String, i32, Option<i64>)> = sqlx::query_as(
            "SELECT p.id, p.serial_no, p.drawing_no, b.quantity, b.next_process_id \
             FROM t_part_batch b \
             JOIN t_part p ON p.id = b.part_id \
             JOIN t_work_type_process wtp ON wtp.process_id = b.next_process_id \
             JOIN t_shelf s ON s.id = b.current_holder_id \
             WHERE b.deleted_at IS NULL AND p.deleted_at IS NULL \
               AND b.status = 'IN_PROCESS' AND b.location = 'PRODUCTION_SHELF' \
               AND s.is_active = true AND s.zone = 'PRODUCTION' \
               AND wtp.work_type_id = $1 \
               AND ($2::bigint IS NULL OR b.current_holder_id = $2) \
             ORDER BY p.is_urgent DESC, p.planned_delivery_date ASC, b.id ASC \
             LIMIT $3 OFFSET $4",
        )
        .bind(work_type_id)
        .bind(shelf_filter)
        .bind(limit)
        .bind(offset)
        .fetch_all(&mut *conn)
        .await?;
        let items: Vec<super::super::dto_crud::PartListItem> = rows
            .into_iter()
            .map(|(id, serial, drawing, qty, _np)| super::super::dto_crud::PartListItem {
                part: crate::modules::part::model::TPart {
                    id,
                    serial_no: Some(serial),
                    name: drawing.clone(),
                    drawing_no: drawing,
                    applicant_name: String::new(),
                    quantity: qty,
                    request_date: chrono::NaiveDate::from_ymd_opt(1970, 1, 1).unwrap(),
                    planned_delivery_date: chrono::NaiveDate::from_ymd_opt(1970, 1, 1).unwrap(),
                    actual_delivery_date: None,
                    customer_id: 0,
                    assembly_id: None,
                    status: "IN_PROCESS".to_string(),
                    location: Some("PRODUCTION_SHELF".to_string()),
                    is_urgent: false,
                    current_holder_id: None,
                    placed_at: None,
                    next_process_id: None,
                    order_no: None,
                    system_delivery_date: None,
                    note: None,
                    has_been_repaired: false,
                    version: 0,
                    created_at: chrono::NaiveDateTime::from_timestamp_opt(0, 0).unwrap(),
                    created_by: None,
                    updated_at: chrono::NaiveDateTime::from_timestamp_opt(0, 0).unwrap(),
                    updated_by: None,
                    deleted_at: None,
                    delivery_note_id: None,
                    process_chain_id: None,
                },
                customer_name: None,
                l1_customer_name: None,
            })
            .collect();
        let total: i64 = sqlx::query_scalar(
            "SELECT COUNT(*)::bigint FROM t_part_batch b \
             JOIN t_work_type_process wtp ON wtp.process_id = b.next_process_id \
             JOIN t_shelf s ON s.id = b.current_holder_id \
             WHERE b.deleted_at IS NULL \
               AND b.status = 'IN_PROCESS' AND b.location = 'PRODUCTION_SHELF' \
               AND s.is_active = true AND s.zone = 'PRODUCTION' \
               AND wtp.work_type_id = $1 \
               AND ($2::bigint IS NULL OR b.current_holder_id = $2)",
        )
        .bind(work_type_id)
        .bind(shelf_filter)
        .fetch_one(&mut *conn)
        .await?;
        Ok(super::super::dto_crud::PartListOut {
            items,
            total,
            limit,
            offset,
        })
    }

    /// `GET /parts/by-worker/{worker_id}`：工人当前持有件。
    pub async fn list_by_worker(
        conn: &mut PgConnection,
        worker_id: i64,
        query: &super::super::dto_crud::ByWorkerQuery,
        current: &CurrentUser,
    ) -> Result<super::super::dto_crud::PartListOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk, Role::Inspector, Role::ShelfAccount])?;
        let limit = query.limit.unwrap_or(50).clamp(1, 200);
        let offset = query.offset.unwrap_or(0).max(0);
        let rows: Vec<(i64, String, String, i32)> = sqlx::query_as(
            "SELECT p.id, p.serial_no, p.drawing_no, b.quantity \
             FROM t_part_batch b \
             JOIN t_part p ON p.id = b.part_id \
             WHERE b.deleted_at IS NULL AND p.deleted_at IS NULL \
               AND b.status = 'IN_PROCESS' AND b.location = 'WORKER' \
               AND b.current_holder_id = $1 \
             ORDER BY b.id DESC LIMIT $2 OFFSET $3",
        )
        .bind(worker_id)
        .bind(limit)
        .bind(offset)
        .fetch_all(&mut *conn)
        .await?;
        let items: Vec<super::super::dto_crud::PartListItem> = rows
            .into_iter()
            .map(|(id, serial, drawing, qty)| super::super::dto_crud::PartListItem {
                part: crate::modules::part::model::TPart {
                    id,
                    serial_no: Some(serial),
                    name: drawing.clone(),
                    drawing_no: drawing,
                    applicant_name: String::new(),
                    quantity: qty,
                    request_date: chrono::NaiveDate::from_ymd_opt(1970, 1, 1).unwrap(),
                    planned_delivery_date: chrono::NaiveDate::from_ymd_opt(1970, 1, 1).unwrap(),
                    actual_delivery_date: None,
                    customer_id: 0,
                    assembly_id: None,
                    status: "IN_PROCESS".to_string(),
                    location: Some("WORKER".to_string()),
                    is_urgent: false,
                    current_holder_id: Some(worker_id),
                    placed_at: None,
                    next_process_id: None,
                    order_no: None,
                    system_delivery_date: None,
                    note: None,
                    has_been_repaired: false,
                    version: 0,
                    created_at: chrono::NaiveDateTime::from_timestamp_opt(0, 0).unwrap(),
                    created_by: None,
                    updated_at: chrono::NaiveDateTime::from_timestamp_opt(0, 0).unwrap(),
                    updated_by: None,
                    deleted_at: None,
                    delivery_note_id: None,
                    process_chain_id: None,
                },
                customer_name: None,
                l1_customer_name: None,
            })
            .collect();
        let total: i64 = sqlx::query_scalar(
            "SELECT COUNT(*)::bigint FROM t_part_batch b \
             WHERE b.deleted_at IS NULL \
               AND b.status = 'IN_PROCESS' AND b.location = 'WORKER' \
               AND b.current_holder_id = $1",
        )
        .bind(worker_id)
        .fetch_one(&mut *conn)
        .await?;
        Ok(super::super::dto_crud::PartListOut {
            items,
            total,
            limit,
            offset,
        })
    }
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

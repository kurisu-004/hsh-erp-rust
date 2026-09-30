//! part 域 Phase 1（2026-09-13）业务逻辑：补齐 14 端点。
//!
//! 2026-09-22 D-6 重构：原 `phase1.rs`（3084 行超限）按业务动作拆为子模块：
//! - `lifecycle_helpers` 1.1 上架 / 召回 + 1.5 批次列表（place_on_shelf /
//!   recall_to_pending / list_pending_programming / list_batches）
//! - `programming` 1.2 CNC 编程流转（`release_from_programming`）。
//!   2026-09-29 端点下线：`send_to_programming` 与 `recall_to_programming` 整体
//!   删除（PROGRAMMING 状态废弃进入路径；新进入路径是工艺链 + CNC step +
//!   待编程一览由 `t_process.is_cnc` 列驱动）
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
//! - 1.2 CNC 编程流转（`release_from_programming` / `pending_programming`）
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
//! 状态机扩展见 `part/statemachine.rs`（2026-09-29 缩至 19 个合法迁移）。
//! 错误码全部沿用 `shared/error.rs::code` 已声明常量（201xx / 205xx）。
//!
//! ## 批次守恒不变量
//! 拆分时 `Σ(未删批次.quantity) = t_part.quantity` 必须保持；
//! 由 `PartBatchRepo::split_batch` 强制（同一事务内连发 max+1 / INSERT / UPDATE 三条 SQL，
//! OCC 守源批次），handler 层再加 `BIZ_PART_BATCH_INVALID_QUANTITY` 防御性校验。

#![allow(deprecated, clippy::too_many_arguments, clippy::type_complexity)]

use sqlx::PgConnection;

use crate::modules::part::repo::status_gate::{self, StatusChange};
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
    /// 2026-09-30 新增（来自 b.part_id，对齐 PartBatchListItemOut 新字段）
    part_id: i64,
    batch_no: i32,
    quantity: i32,
    status: String,
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
///
/// 2026-09-30 新增第 8 个 bind 参数 `new_current_process_id: Option<i64>`，
/// 写入 `t_part_batch.current_process_id`。两者语义**必须分清**：
/// - `new_current_process_id` —— **池归属的权威依据**（逻辑 FK → t_process.id）。
///   写入不变式：进池（`status='IN_PROCESS'` + `location='PRODUCTION_SHELF'`）
///   写目标 `process_id`；出池（转 PENDING / INSPECTION / INSPECTION_SHELF）
///   传 `None`；池内移动不经过本函数，故无「不动」分支。
/// - `new_current_process_step_id` —— **可选的显示用定位信息**（逻辑 FK →
///   t_process_chain_step.id），仅当工单已绑定工序链时才写，允许 NULL。
///   NULL 不影响入池（旧设计的死状态已由 `current_process_id` 打破）。
///   ⚠️ 措辞（2026-09-30 review 第 3 轮附带发现）：该列**只在首次定位工序时写、
///   之后不再推进**（worker-scan RETURNED / 送检都不写），故**不是**「当前走到
///   第几步」的进度指针。
///
/// 写入不变式第 4 行「非生产流 → NULL」的**唯一例外**（2026-09-30 review 第 1 轮
/// M1 补记，勿按表机械核对后误判为 bug）：
///
/// `send_to_outsource`（`outsource.rs:158`，`status='OUTSOURCE'` +
/// `location='OUTSOURCE_COMPANY'`）传 `Some(req.process_id)` 而非 NULL。三条理由：
///
/// 1. 外协加工的就是这道工序，rollup 派生 `t_part.next_process_id` 需要它；
/// 2. 与本次改动**前**的行为一致（旧代码写 `Some(step_id)`，rollup 再翻成
///    process_id），不写才是行为变更；
/// 3. `status='OUTSOURCE'` + `location='OUTSOURCE_COMPANY'` 使其**不可能**被任何
///    工序池查询命中（4 条池 SQL 与 `list_pickable_by_work_type` 均硬限定
///    `status='IN_PROCESS'` 叠加 `location='PRODUCTION_SHELF'`）。
///
/// 初始批次 / 子批次仍为严格 NULL（见 `part/batch/repo.rs::create_initial_batch`
/// 与 `part/repo/sql/part_sql.rs::insert_child_for_assembly`，两者都不写该列）。
///
/// 2026-10-01：改为 `status_gate::apply_batch_status_change` 的薄包装
/// （全仓唯一的 `t_part_batch.status` 写入口）。
///
/// 语义映射（保持与改造前逐条等价）：
/// - WHERE 的 `status NOT IN ('CANCELLED','COMPLETED')` 换成 status_gate 的
///   **正向** `allowed_from` 白名单（= 全状态减两个终态）。之所以改成正向：
///   status_gate 的白名单是「本次允许的源状态」，反向排除无法直接表达，
///   而正向表多写 9 个状态值换来的是「新增状态时若忘了加进白名单会被拒」
///   ——fail-safe 方向正确。
/// - `new_current_process_id = None` 在原语义里是「**写 NULL**」（出池），
///   而 status_gate 的 `None` 是「保持原值」。故额外置
///   `clear_process_id: new_current_process_id.is_none()`。
/// - `new_location` / `new_holder_id` / `new_current_process_step_id` 的
///   `None` 语义两边一致（SQL 直写 NULL vs COALESCE 保持原值）——
///   ⚠️ **这里有一处刻意的不等价**：原实现直写 NULL，本实现是「保持原值」。
///   全部 8 个调用点（work_type / programming / lifecycle_helpers / outsource
///   ×3 / repair ×2 / scan）传的 location / holder / step 均非 None 或与
///   「保持原值」等价，故行为不变；后续若要传 None 表达「清空」，
///   需先在 status_gate 补对应 flag。
#[allow(clippy::too_many_arguments)]
async fn mark_batch_with_status_and_meta(
    conn: &mut PgConnection,
    batch_id: i64,
    expected_version: i32,
    new_status: &str,
    new_location: Option<&str>,
    new_holder_id: Option<i64>,
    new_current_process_step_id: Option<i64>,
    new_current_process_id: Option<i64>,
    updated_by: i64,
) -> Result<u64, AppError> {
    status_gate::apply_batch_status_change(
        conn,
        StatusChange {
            batch_id,
            new_status,
            new_location,
            new_holder_id,
            new_process_id: new_current_process_id,
            new_process_step_id: new_current_process_step_id,
            // 2026-10-01：本漏斗 = 「批次离开返修态」的清零点。
            //
            // 规则依据：设 `is_repairing=true` 的入口只有 2 个 ——
            // `mark_batch_repairing`（start-repair）与 `mark_batch_status_only`
            // （scan-inspect FAIL），二者都**不走**本漏斗的返修分支；本漏斗的
            // 8 个调用点（place_on_shelf / recall_to_pending /
            // release_from-programming / outsource 收发 ×3 / complete_repair /
            // repair_dispatch / scan-inspect 第一步）全部表示「批次回到了正常
            // 生产流 / 送检 / 完成返修」，此刻必须清标记，否则返修态永远挂着。
            // 其中 `complete_repair` 与 `repair_dispatch` 正是最关键的两个
            // 清除点（前者=返修完成，后者=一步式起修并直接到位）。
            is_repairing: Some(false),
            expected_version: Some(expected_version),
            // 终态不可流转：COMPLETED / CANCELLED 之外的全部状态。
            //
            // 末尾的 "REPAIRING" 是**过渡期源状态别名**：REPAIRING 已从
            // PartStatus 删除，但 migration 006 之前的存量行、以及
            // 直接写库的历史数据仍可能是该值。把它列进白名单是
            // `PartStatus::from_str` 里 `"REPAIRING" => IN_PROCESS`
            // 兼容分支在 SQL 层的对应物（两侧容忍度必须一致，否则会出现
            // 「service 层放行、SQL 守卫拒掉」的诡异 409）。
            allowed_from: &[
                "PENDING",
                "PROGRAMMING",
                "IN_PROCESS",
                "INSPECTION",
                "READY_TO_SHIP",
                "DELIVERED",
                "OUTSOURCE",
                "REPAIRING",
            ],
            updated_by,
            clear_process_id: new_current_process_id.is_none(),
        },
    )
    .await
    .map(|_| 1u64)
}

/// mark_batch 的轻量版本（不写 location/holder/process；用于状态机迁移但保持原 holder 的场景，如 CANCELLED）。
///
/// 2026-10-01：改为 `status_gate::apply_batch_status_change` 的薄包装。
///
/// 3 个调用点传入的目标状态是 `READY_TO_SHIP`（scan-inspect pass）/
/// `"REPAIRING"`（scan-inspect FAIL）/ `CANCELLED`（cancel-batch）。
/// 其中 **`"REPAIRING"` 是过渡别名**（2026-10-01 REPAIRING 降级为标记列）：
/// 本函数把它翻译成 `status='IN_PROCESS' + is_repairing=true`，这样
/// `phase1/scan.rs` 这个属于 REPAIRING 下游消费方改造范围的调用点**本轮零改动**。
/// 后续那一轮应让 scan 显式传标记、删掉本别名分支。
///
/// `allowed_from` 由目标状态反查（`status_guard_for_target`）——原实现
/// 「无源状态守卫」，本实现补上；等价性由该函数的 doc 逐目标状态论证。
async fn mark_batch_status_only(
    conn: &mut PgConnection,
    batch_id: i64,
    expected_version: i32,
    new_status: &str,
    updated_by: i64,
) -> Result<u64, AppError> {
    let (real_status, is_repairing) = resolve_status_alias(new_status);
    status_gate::apply_batch_status_change(
        conn,
        StatusChange {
            batch_id,
            new_status: real_status,
            new_location: None,
            new_holder_id: None,
            new_process_id: None,
            new_process_step_id: None,
            is_repairing,
            expected_version: Some(expected_version),
            allowed_from: status_guard_for_target(real_status),
            updated_by,
            clear_process_id: false,
        },
    )
    .await
    .map(|_| 1u64)
}

/// 2026-10-01 新增：目标状态别名翻译（过渡期）。
///
/// `"REPAIRING"` → `("IN_PROCESS", Some(true))`；其余原样返回、
/// 标记不写（`None` = 保持原值）。REPAIRING 降级为 `is_repairing` 标记列
/// （migration 005/006）后它已不是合法 `PartStatus`，但仍有 1 个调用点
/// （`phase1/scan.rs`）按老词汇传字符串。
#[inline]
fn resolve_status_alias(target: &str) -> (&str, Option<bool>) {
    if target == "REPAIRING" {
        ("IN_PROCESS", Some(true))
    } else {
        (target, None)
    }
}

/// 2026-10-01 新增：由**目标**状态反查 `mark_batch_status_only` 的源状态白名单。
///
/// 等价性论证（原实现 WHERE 无 status 守卫，调用点各自在 service 层守）：
/// - `READY_TO_SHIP`：唯一调用点是 `scan_inspect` 的 pass 分支，源恒为上一步
///   刚写下的 `INSPECTION`（同一函数前半段写死）→ 白名单 `["INSPECTION"]` 等价。
/// - `IN_PROCESS`（由 `"REPAIRING"` 翻译）：唯一调用点是 `scan_inspect` 的
///   FAIL 分支，源恒为 `INSPECTION` → 等价。
/// - `CANCELLED`：调用点 `batch_ops::cancel_batch` 在 service 层已守
///   `from != COMPLETED && from != CANCELLED`，故白名单取
///   「全状态减两个终态」→ 等价。
fn status_guard_for_target(target: &str) -> &'static [&'static str] {
    match target {
        "READY_TO_SHIP" | "IN_PROCESS" => &["INSPECTION"],
        _ => &[
            "PENDING",
            "PROGRAMMING",
            "IN_PROCESS",
            "INSPECTION",
            "READY_TO_SHIP",
            "DELIVERED",
            "OUTSOURCE",
        ],
    }
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
async fn require_process_chain(conn: &mut PgConnection, part_id: i64) -> Result<i64, AppError> {
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
        ensure_transition(PartStatus::OUTSOURCE, PartStatus::IN_PROCESS, "test").unwrap();
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

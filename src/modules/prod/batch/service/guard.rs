//! prod::batch 全部批次用例共用的自由函数：状态机守卫 / OCC / 货架校验 /
//! `status_gate` 薄包装。
//!
//! 2026-10-02 随批次用例整体迁入 prod 域：本文件原先是 `part::service::phase1`
//! 的私有 helper 层，9 个函数与 `InspectionRepairRow` 的**全部**调用点都在
//! 以批次为对象的用例里（`list_*` 端点不碰它们），故随调用方一同迁走。
//!
//! 这里是 part → prod 依赖的反向证明：这层守卫只经 `status_gate` 写
//! `t_part_batch.status`，不引用 part 域任何 service / vo。

use sqlx::PgConnection;

use crate::modules::part::statemachine::PartStatus;
use crate::modules::prod::batch::status_gate::{self, StatusChange};
use crate::modules::shelf::repo::ShelfRepo;
use crate::shared::error::{AppError, code};

#[derive(sqlx::FromRow)]
pub(crate) struct InspectionRepairRow {
    pub(crate) batch_id: i64,
    pub(crate) part_id: i64,
    pub(crate) batch_no: i32,
    pub(crate) quantity: i32,
    pub(crate) status: String,
    /// 2026-10-01 review 第 1 轮 M5 新增（migration 005）：返修标记随列表投出
    pub(crate) is_repairing: bool,
    pub(crate) location: Option<String>,
    pub(crate) version: i32,
    pub(crate) current_process_step_id: Option<i64>,
    pub(crate) parent_batch_id: Option<i64>,
    pub(crate) current_holder_id: Option<i64>,
    pub(crate) holder_name: Option<String>,
    pub(crate) next_process_id: Option<i64>,
    pub(crate) next_process_name: Option<String>,
    pub(crate) delivery_note_id: Option<i64>,
    pub(crate) delivery_note_no: Option<String>,
    pub(crate) serial_no: Option<String>,
    pub(crate) drawing_no: String,
    pub(crate) name: String,
    pub(crate) order_no: Option<String>,
    pub(crate) planned_delivery_date: chrono::NaiveDate,
    pub(crate) is_urgent: bool,
    pub(crate) part_version: i32,
    pub(crate) created_at: chrono::NaiveDateTime,
    pub(crate) updated_at: chrono::NaiveDateTime,
    pub(crate) customer_id: i64,
    pub(crate) customer_name: Option<String>,
    pub(crate) l1_customer_name: Option<String>,
}

/// 状态机迁移守卫 + 错误码映射（在 phase1.rs 内复用）：
/// - 起点状态非法 → 20103 `BIZ_INVALID_TRANSITION`
/// - 起点已是终态 → 20115 `BIZ_PART_ALREADY_CANCELLED`（仅 cancel 路径）
#[inline]
pub(crate) fn ensure_transition(
    from: PartStatus,
    to: PartStatus,
    ctx: &str,
) -> Result<(), AppError> {
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

/// 锚定 `version` 的 caller 侧乐观锁守卫（OCC）。
///
/// 2026-10-02：原 `validate_batch_ownership` 还兼做「batch 属于 part」断言，
/// 该断言随 `part_id` 路径参数退场（part_id 改由批次行反查）而恒真，已删 ——
/// 批次 id 全局唯一即锚点，不存在「跨 part 批次」这一场景。函数随之更名为
/// `validate_batch_version` 并去掉两个 part 形参。
#[inline]
pub(crate) fn validate_batch_version(
    batch_id: i64,
    expected_version: i32,
    actual_version: i32,
) -> Result<(), AppError> {
    if batch_version_mismatch(batch_id, expected_version, actual_version) {
        return Err(AppError::biz(
            code::VERSION_CONFLICT,
            format!("batch {batch_id} 版本冲突（期望 {expected_version}，实际 {actual_version}）"),
        ));
    }
    Ok(())
}

#[inline]
pub(crate) fn batch_version_mismatch(_batch_id: i64, expected: i32, actual: i32) -> bool {
    expected != actual
}

/// 校验 shelf 存在 + active + zone 一致。
pub(crate) async fn validate_shelf_zone(
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
pub(crate) async fn assert_shelf_maps_process(
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
/// 初始批次 / 子批次仍为严格 NULL（见 `prod/batch/repo/queries.rs::create_initial_batch`
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
/// - 其余 3 个可选列的 `None` 在原语义里**同样是「写 NULL」**（SQL 是
///   `location = $4, current_holder_id = $5, current_process_step_id = $6`），
///   而 status_gate 的 `None` 是「保持原值」。故本包装函数对 4 列一律
///   `clear_*: 形参.is_none()`，把「传 None = 清 NULL」这一**既有约定**如实
///   翻译过去。
///
///   ⚠️ 2026-10-01 review 第 1 轮 M2：上一轮（本改造的首版）只给
///   `clear_process_id` 做了翻译，并在注释里断言「全部 8 个调用点传的
///   location / holder / step 均非 None 或与保持原值等价」——**该断言是错的**，
///   实际有 5 个调用点靠 `None` 表达「清空」，被误改成「保持原值」后：
///   - `lifecycle_helpers.rs`（recall-to-pending，`None,None,None,None`）：
///     PENDING 批次仍留着 `location='PRODUCTION_SHELF'` + `current_holder_id`
///     + 陈旧 step，UI 上「待投产」的工单显示还压在生产架上；
///   - `outsource.rs`（外协收回 → INSPECTION）、`scan.rs`（scan-inspect 第一步）、
///     `repair.rs` ×2（complete-repair / repair-dispatch 的 INSPECTION 分支）：
///     `current_process_step_id` 不再清，而展示用的 `next_process_id` 正是由它
///     经 `t_process_chain_step` JOIN 派生 → 出池批次显示上一道工序。
///
///   依赖该约定的 5 个调用点（改任何一处都要连带复核这 5 行）：
///   `lifecycle_helpers.rs:153`（recall）、`outsource.rs:407`、
///   `scan.rs:70`、`repair.rs:159`、`repair.rs:308`。
///   其余 5 个调用点（共 **10** 个调用点，place_on_shelf / release_from_programming /
///   send_to_outsource / receive_from_outsource / work_type pick-up）4 列全传
///   `Some(..)`，走不到 clear 分支。
#[allow(clippy::too_many_arguments)]
pub(crate) async fn mark_batch_with_status_and_meta(
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
            // `mark_batch_repairing`（start-repair，只置标记不改 status）与
            // `mark_batch_status_only`（scan-inspect FAIL，形参
            // `is_repairing = Some(true)`），二者都**不走**本漏斗的返修分支；
            // 本漏斗的 8 个调用点（place_on_shelf / recall_to_pending /
            // release_from_programming / outsource 收发 ×3 / complete_repair /
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
            // 2026-10-01 review 第 1 轮 M2：本包装函数沿用「形参 None = 写 NULL」
            // 的**既有约定**（改造前 SQL 是 4 列直写），故 4 列一律
            // `clear_* = 形参.is_none()`，把旧语义如实翻译进 status_gate 的
            // 「None = 保持原值」三态模型。理由与受影响调用点清单见本函数 doc。
            clear_location: new_location.is_none(),
            clear_holder_id: new_holder_id.is_none(),
            clear_process_id: new_current_process_id.is_none(),
            clear_process_step_id: new_current_process_step_id.is_none(),
            // 本漏斗的目标状态没有一个是终态（COMPLETED 走
            // `mark_batch_completed`、CANCELLED 走 `mark_batch_status_only`），
            // 故永远不会触发 step 4 的终态序列号归档。
            event_id: None,
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
/// `IN_PROCESS`（scan-inspect FAIL，**同时置 `is_repairing=true`**）/
/// `CANCELLED`（cancel-batch）。返修标记由形参 `is_repairing` 显式传入，
/// 本函数不做任何别名翻译 —— 上一轮为兼容老调用点留的
/// `resolve_status_alias("REPAIRING")` 过渡分支已在本轮删除（REPAIRING 已从
/// `PartStatus` 删除，DB 层也不再产生该 status，别名只会让读代码的人以为它
/// 还是合法状态）。
///
/// `allowed_from` 由目标状态反查（`status_guard_for_target`）——原实现
/// 「无源状态守卫」，本实现补上；等价性由该函数的 doc 逐目标状态论证。
pub(crate) async fn mark_batch_status_only(
    conn: &mut PgConnection,
    batch_id: i64,
    expected_version: i32,
    new_status: &str,
    is_repairing: Option<bool>,
    updated_by: i64,
    event_id: Option<i64>,
) -> Result<u64, AppError> {
    status_gate::apply_batch_status_change(
        conn,
        StatusChange {
            batch_id,
            new_status,
            new_location: None,
            new_holder_id: None,
            new_process_id: None,
            new_process_step_id: None,
            is_repairing,
            expected_version: Some(expected_version),
            allowed_from: status_guard_for_target(new_status),
            updated_by,
            clear_location: false,
            clear_holder_id: false,
            clear_process_id: false,
            clear_process_step_id: false,
            // 2026-10-01 review 第 1 轮 M4：只有 `new_status='CANCELLED'` 会让
            // part 新进终态（cancel-batch），caller 需在那种情况下传雪花 id；
            // `READY_TO_SHIP` / `IN_PROCESS` 两个目标传 `None` 即可。
            event_id,
        },
    )
    .await
    .map(|_| 1u64)
}

/// 2026-10-01：由**目标**状态反查 `mark_batch_status_only` 的源状态白名单。
///
/// 等价性论证（原实现 WHERE 无 status 守卫，调用点各自在 service 层守）：
/// - `READY_TO_SHIP`：唯一调用点是 `scan_inspect` 的 pass 分支，源恒为上一步
///   刚写下的 `INSPECTION`（同一函数前半段写死）→ 白名单 `["INSPECTION"]` 等价。
/// - `IN_PROCESS`：唯一调用点是 `scan_inspect` 的
///   FAIL 分支，源恒为 `INSPECTION` → 等价。
/// - `CANCELLED`：调用点 `batch_ops::cancel_batch` 在 service 层已守
///   `from != COMPLETED && from != CANCELLED`，故白名单取
///   「全状态减两个终态」→ 等价。
pub(crate) fn status_guard_for_target(target: &str) -> &'static [&'static str] {
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
pub(crate) async fn require_process_chain(
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

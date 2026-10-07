//! DeliveryNoteService 状态流转与读视图（提交 / 撤回 / 拣货 / 软删）。
//!
//! 2026-10-08：`pickup_scan`（司机端逐件扫码核销）与 `list_candidate_parts`
//! （候选取批列表）随入单入口收敛一并删除 —— 前端不再有「先挑批次再入单」两条
//! 并行路径，也不再有独立的送货台扫码核销端点。
//!
//! ## 2026-09-22 D-5 + review 第 1 轮修正（service by-value trait）
//! - 所有方法签名从 `pub async fn xxx(conn: &mut PgConnection, snowflake: &SnowflakeIdGenerator, ...)`
//!   改成 `pub async fn xxx<R: DeliveryNoteRepoTrait>(&self, mut repo: R, ...)`——对齐
//!   iam / shelf / customer 严格范本。snowflake 改为 `&self.snowflake`（service 装线
//!   时一次性 set）。
//! - 跨域 ZST 静态调用（`WorkerRepo::xxx` / `WorkTypeRepo::xxx` / `PartRepo::xxx` /
//!   `PartBatchRepo::xxx` / `CustomerRepo::xxx`）走 `&mut *repo.conn_mut()` 借位
//!   传入（与 part 域 D-6 conn_mut 模式一致）。
//! - 跨域 service 委托（`PartService::sync_from_batch_change_with_conn`）走
//!   `&mut *repo.conn_mut()` 同样模式。
//! - 原 sqlx::query! 直调（如 `soft_delete` 的 UPDATE batches）走
//!   `sqlx::query!(...).execute(&mut *repo.conn_mut())` 替换 `&mut *conn`。
//! - 私有 helper（`build_note_outs`）签名仍收 `&mut PgConnection`——
//!   helper 是同态私有 helper，调用方走 `&mut *repo.conn_mut()` 喂入（与 iam
//!   `AccountService::assemble_user_out` 等私有 helper 一致）。

use crate::auth::rbac::{CurrentUser, Role};
use crate::infra::clock::now_naive;
use crate::modules::com::delivery_note::repo::DeliveryNoteRepoTrait;
use crate::modules::part::service::PartService;
use crate::modules::prod::batch::repo::PartBatchRepo;
use crate::modules::prod::work_type::repo::WorkTypeRepo;
use crate::modules::prod::worker::repo::WorkerRepo;
use crate::shared::error::{AppError, code};

use super::inner::{build_note_outs, note_not_found, note_version_conflict};

use super::DeliveryNoteService;

/// 司机工种常量（与 `t_work_type.code` 严格一致）
const WORK_TYPE_DRIVER_CODE: &str = "送货司机";

/// P2 业务状态常量（与 DB 列 `status` 一致；与 `DeliveryNoteStatus::as_str()` 对齐）
const STATUS_DRAFT: &str = "DRAFT";
const STATUS_SUBMITTED: &str = "SUBMITTED";
const STATUS_PICKED_UP: &str = "PICKED_UP";

/// P2 业务批次状态常量（入单与提交闸门只允许 `READY_TO_SHIP`）
const STATUS_READY_TO_SHIP: &str = "READY_TO_SHIP";

impl DeliveryNoteService {
    // ---------- submit ----------

    /// 返回值就是提交后的送货单 id（JSON string）。
    ///
    /// 2026-10-08：`SubmitDeliveryOut` / `SubmitOutcomeDto` 塌缩为 `R<String>`。
    /// 旧的候选分流（`CANDIDATES_AVAILABLE`）依赖「单上挂着 INSPECTION 批次」这个
    /// 状态，而入单入口已收敛为只允许 `READY_TO_SHIP`（见 `POST /scan` 的 21405
    /// 闸门）⇒ DRAFT 单上不可能再有 INSPECTION 批次，候选分支恒不可达。
    pub async fn submit<R: DeliveryNoteRepoTrait>(
        &self,
        mut repo: R,
        note_id: i64,
        version: i32,
        current: &CurrentUser,
    ) -> Result<i64, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk, Role::Inspector])?;

        let mut obj = repo
            .note_get_by_id(note_id, false)
            .await?
            .ok_or_else(|| note_not_found(note_id))?;
        if obj.version != version {
            return Err(note_version_conflict(note_id, obj.version, version));
        }
        if obj.status != STATUS_DRAFT {
            return Err(AppError::biz(
                code::BIZ_DELIVERY_NOTE_INVALID_TRANSITION,
                format!("only DRAFT can be submitted, current={}", obj.status),
            ));
        }

        // 单上必须非空，且全部批次只能是 READY_TO_SHIP。
        //
        // 2026-10-08 收窄：入单只允许 READY_TO_SHIP ⇒ DRAFT 单上不可能挂到
        // INSPECTION 批次 ⇒ 旧的「有 INSPECTION 就返候选让前端一键过检」分流恒不可达，
        // 连同 `AvailableBatchDto` / `UnresolvedTargetDto` / `SubmitOutcomeDto` 一并
        // 删除。这里保留一道状态闸门：批次在挂单后被旁路改了状态（既非 READY_TO_SHIP
        // 也非 DELIVERED 之类可解释态）时 fail-loud，而不是把脏数据提交出去。
        let note_batches =
            PartBatchRepo::list_by_delivery_note(&mut *repo.conn_mut(), note_id).await?;
        if note_batches.is_empty() {
            return Err(AppError::biz(
                code::BIZ_DELIVERY_NOTE_INVALID_VALUE,
                "empty delivery note; add parts before submit",
            ));
        }
        for b in &note_batches {
            if b.status != STATUS_READY_TO_SHIP {
                return Err(AppError::biz(
                    code::BIZ_DELIVERY_BATCH_STATE_INVALID,
                    format!(
                        "批次 {} status={}；入单只允许 READY_TO_SHIP（挂单后被旁路改状态）",
                        b.batch_no, b.status
                    ),
                ));
            }
        }

        // 状态机：DRAFT → SUBMITTED
        let now = now_naive();
        obj.status = STATUS_SUBMITTED.to_string();
        obj.submitted_at = Some(now);
        obj.submitted_by = Some(current.id);
        obj.version += 1;
        obj.updated_at = now;
        obj.updated_by = Some(current.id);

        let affected = repo.note_update(&obj).await?;
        if affected == 0 {
            return Err(AppError::biz(
                code::VERSION_CONFLICT,
                "concurrent modification detected",
            ));
        }
        obj = repo
            .note_get_by_id(note_id, false)
            .await?
            .ok_or_else(|| note_not_found(note_id))?;
        Ok(obj.id)
    }

    // ---------- recall ----------

    pub async fn recall<R: DeliveryNoteRepoTrait>(
        &self,
        mut repo: R,
        note_id: i64,
        version: i32,
        current: &CurrentUser,
    ) -> Result<super::super::vo::DeliveryNoteOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk, Role::Inspector])?;

        let mut obj = repo
            .note_get_by_id(note_id, false)
            .await?
            .ok_or_else(|| note_not_found(note_id))?;
        if obj.version != version {
            return Err(note_version_conflict(note_id, obj.version, version));
        }
        if obj.status != STATUS_SUBMITTED {
            return Err(AppError::biz(
                code::BIZ_DELIVERY_NOTE_NOT_SUBMITTED,
                format!("only SUBMITTED can be recalled, current={}", obj.status),
            ));
        }

        // 建单判定键撞唯一（21419）：2026-10-08 起判定键是 `(customer_id, DRAFT)`
        // 单键，若该 L1 名下已有另一张活跃 DRAFT，recall 会撞数据库部分唯一索引
        // `uk_t_delivery_note_l1_open_draft` ⇒ 提前用业务码拒，给出可读原因。
        // 判据不用「排除自己」：被 recall 的单当前是 SUBMITTED，不可能被这条查询命中。
        if repo
            .note_find_open_draft_by_l1(obj.customer_id)
            .await?
            .is_some()
        {
            return Err(AppError::biz(
                code::BIZ_DELIVERY_NOTE_DRAFT_SCOPE_CONFLICT,
                "该 L1 名下已存在 DRAFT 草稿；请先处理现有草稿再 recall",
            ));
        }

        let now = now_naive();
        obj.status = STATUS_DRAFT.to_string();
        obj.submitted_at = None;
        obj.submitted_by = None;
        obj.version += 1;
        obj.updated_at = now;
        obj.updated_by = Some(current.id);

        let affected = repo.note_update(&obj).await?;
        if affected == 0 {
            return Err(AppError::biz(
                code::VERSION_CONFLICT,
                "concurrent modification detected",
            ));
        }
        obj = repo
            .note_get_by_id(note_id, false)
            .await?
            .ok_or_else(|| note_not_found(note_id))?;
        let out = build_note_outs(&mut *repo.conn_mut(), std::slice::from_ref(&obj)).await?;
        Ok(out.into_iter().next().unwrap())
    }

    // ---------- pickup ----------

    pub async fn pickup<R: DeliveryNoteRepoTrait>(
        &self,
        mut repo: R,
        note_id: i64,
        driver_worker_id: i64,
        version: i32,
        _badge_code: Option<&str>,
        current: &CurrentUser,
    ) -> Result<super::super::vo::DeliveryNoteOut, AppError> {
        // 任意已登录账号即可（service 层校验司机）
        let _ = current;

        let mut obj = repo
            .note_get_by_id(note_id, false)
            .await?
            .ok_or_else(|| note_not_found(note_id))?;
        if obj.version != version {
            return Err(note_version_conflict(note_id, obj.version, version));
        }
        if obj.status != STATUS_SUBMITTED {
            return Err(AppError::biz(
                code::BIZ_DELIVERY_NOTE_NOT_SUBMITTED,
                format!("only SUBMITTED can be picked up, current={}", obj.status),
            ));
        }

        // 司机校验
        let driver = WorkerRepo::get_by_id(&mut *repo.conn_mut(), driver_worker_id, false)
            .await?
            .ok_or_else(|| {
                AppError::biz(
                    code::BIZ_DELIVERY_NOTE_DRIVER_INVALID,
                    format!("driver worker {driver_worker_id} not found or inactive"),
                )
            })?;
        if !driver.is_active {
            return Err(AppError::biz(
                code::BIZ_DELIVERY_NOTE_DRIVER_INVALID,
                format!("driver worker {driver_worker_id} not active"),
            ));
        }
        if let Some(wt_id) = driver.work_type_id {
            let wt = WorkTypeRepo::get_by_id(&mut *repo.conn_mut(), wt_id)
                .await?
                .ok_or_else(|| {
                    AppError::biz(
                        code::BIZ_DELIVERY_NOTE_DRIVER_INVALID,
                        "driver work_type not found",
                    )
                })?;
            if wt.code != WORK_TYPE_DRIVER_CODE {
                return Err(AppError::biz(
                    code::BIZ_DELIVERY_NOTE_DRIVER_INVALID,
                    format!("driver work_type {} != {WORK_TYPE_DRIVER_CODE:?}", wt.code),
                ));
            }
        } else {
            return Err(AppError::biz(
                code::BIZ_DELIVERY_NOTE_DRIVER_INVALID,
                "driver has no work_type",
            ));
        }

        // 校验所有批次 READY_TO_SHIP + 非空
        let mut note_batches =
            PartBatchRepo::list_by_delivery_note(&mut *repo.conn_mut(), note_id).await?;
        if note_batches.is_empty() {
            return Err(AppError::biz(
                code::BIZ_DELIVERY_NOTE_INVALID_VALUE,
                "empty delivery note; cannot pick up",
            ));
        }
        for b in &note_batches {
            if b.status != STATUS_READY_TO_SHIP {
                return Err(AppError::biz(
                    code::BIZ_DELIVERY_NOTE_PART_NOT_READY,
                    format!(
                        "batch {} status={}, must be READY_TO_SHIP at pickup",
                        b.batch_no, b.status
                    ),
                ));
            }
        }

        let now = now_naive();

        // 把每个批次的 status 推到 DELIVERED，清 holder/location，version++
        let mut affected_part_ids: Vec<i64> = Vec::new();
        for b in &mut note_batches {
            b.status = "DELIVERED".to_string();
            b.current_holder_id = None;
            b.location = None;
            b.version += 1;
            b.updated_at = now;
            b.updated_by = Some(current.id);
            let affected = PartBatchRepo::update(
                &mut *repo.conn_mut(),
                b.id,
                b.version - 1,      // expected_version 是之前的
                b.delivery_note_id, // 保留 delivery_note_id（PICKED_UP/ARCHIVED 后仍可打印）
                Some("DELIVERED"),
                Some(current.id),
            )
            .await?;
            if affected == 0 {
                // TODO(2026-10-01 review 第 2 轮 MINOR-5，follow-up PR)：本分支
                // 是**死代码**。`PartBatchRepo::update` 在 `status = Some(..)` 时恒
                // `return Ok(1)`（0 行已由 `batch_status::apply_batch_status_change`
                // 转成 `VERSION_CONFLICT` 抛出），而本处必然传 `Some("DELIVERED")`。
                // 保留它无害（将来 `update` 改回「可能 0 行」时它又是对的），但
                // 读代码的人会误以为这里还能拦下并发。修法二选一：删掉本分支，或
                // 把 `update` 的 `Ok(1)` 改成真实的 `rows_affected()` 并在此处
                // 重新变成活代码（后者更符合函数名语义）。
                return Err(AppError::biz(
                    code::VERSION_CONFLICT,
                    "concurrent modification detected",
                ));
            }
            affected_part_ids.push(b.part_id);
        }

        // PR-B2：pickup 翻完所有 batch 后按 part_id 去重逐个
        // sync_from_batch_change；状态机 DELIVERED → COMPLETED / 全部非 CANCELLED
        // 已 DELIVERED 时 part → DELIVERED，否则 part 维持更慢批次的状态。
        let mut seen = std::collections::HashSet::new();
        for pid in affected_part_ids {
            if seen.insert(pid) {
                // `event_id=None`（M4）：pickup 把批次推到 DELIVERED（非终态），
                // min-progress 推不出 part 终态，归档事件分支不可达。
                PartService::sync_from_batch_change_with_conn(
                    &mut *repo.conn_mut(),
                    pid,
                    current,
                    None,
                )
                .await?;
            }
        }

        // 状态机：SUBMITTED → PICKED_UP
        obj.status = STATUS_PICKED_UP.to_string();
        obj.picked_up_at = Some(now);
        obj.picked_up_by = Some(current.id);
        obj.driver_worker_id = Some(driver_worker_id);
        obj.version += 1;
        obj.updated_at = now;
        obj.updated_by = Some(current.id);

        let affected = repo.note_update(&obj).await?;
        if affected == 0 {
            return Err(AppError::biz(
                code::VERSION_CONFLICT,
                "concurrent modification detected",
            ));
        }
        obj = repo
            .note_get_by_id(note_id, false)
            .await?
            .ok_or_else(|| note_not_found(note_id))?;
        let out = build_note_outs(&mut *repo.conn_mut(), std::slice::from_ref(&obj)).await?;
        Ok(out.into_iter().next().unwrap())
    }

    // ---------- soft_delete ----------

    pub async fn soft_delete<R: DeliveryNoteRepoTrait>(
        &self,
        mut repo: R,
        note_id: i64,
        version: i32,
        current: &CurrentUser,
    ) -> Result<(), AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk, Role::Inspector])?;

        let obj = repo
            .note_get_by_id(note_id, false)
            .await?
            .ok_or_else(|| note_not_found(note_id))?;
        if obj.version != version {
            return Err(note_version_conflict(note_id, obj.version, version));
        }
        if obj.status != STATUS_DRAFT {
            return Err(AppError::biz(
                code::BIZ_DELIVERY_NOTE_NOT_DRAFT,
                "only DRAFT delivery notes can be soft-deleted",
            ));
        }

        // 清批次 delivery_note_id
        let _ = sqlx::query!(
            r#"
            UPDATE t_part_batch
            SET delivery_note_id = NULL,
                version          = version + 1,
                updated_at       = $2,
                updated_by       = $3
            WHERE delivery_note_id = $1 AND deleted_at IS NULL
            "#,
            note_id,
            now_naive(),
            Some(current.id),
        )
        .execute(&mut *repo.conn_mut())
        .await?;

        let affected = repo
            .note_soft_delete(note_id, obj.version, now_naive(), Some(current.id))
            .await?;
        if affected == 0 {
            return Err(AppError::biz(
                code::VERSION_CONFLICT,
                "concurrent modification detected",
            ));
        }
        Ok(())
    }
}

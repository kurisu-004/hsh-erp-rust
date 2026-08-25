//! DeliveryNoteService 状态流转与读视图（提交 / 撤回 / 拣货 / 软删 / 事件 / 候选）。

use std::collections::{HashMap, HashSet};

use sqlx::PgConnection;

use crate::auth::rbac::{CurrentUser, Role};
use crate::infra::clock::now_naive;
use crate::infra::snowflake::SnowflakeIdGenerator;
use crate::modules::customer::repo::CustomerRepo;
use crate::modules::part::repo::PartRepo;
use crate::modules::part_batch::repo::PartBatchRepo;
use crate::modules::worker::repo::WorkerRepo;
use crate::modules::work_type::repo::WorkTypeRepo;
use crate::shared::error::{code, AppError};

use super::super::dto::{
    DeliveryNoteCandidatePart, DeliveryNoteEventOut, DeliveryNotePickupScanOut,
};
use super::super::model::DeliveryNoteEventType;
use super::super::repo::{DeliveryNoteEventRepo, DeliveryNoteRepo};
use super::inner::{build_note_outs, note_not_found, note_version_conflict, scope_from_note, write_event};

use super::DeliveryNoteService;

/// 司机工种常量（与 `t_work_type.code` 严格一致）
const WORK_TYPE_DRIVER_CODE: &str = "送货司机";

/// P2 业务状态常量（与 DB 列 `status` 一致；与 `DeliveryNoteStatus::as_str()` 对齐）
const STATUS_DRAFT: &str = "DRAFT";
const STATUS_SUBMITTED: &str = "SUBMITTED";
const STATUS_PICKED_UP: &str = "PICKED_UP";

/// P2 业务批次状态常量
const STATUS_INSPECTION: &str = "INSPECTION";
const STATUS_READY_TO_SHIP: &str = "READY_TO_SHIP";

/// 候选取批上限（与 Python `list_batches_with_part(limit=2000)` 对齐）
const CANDIDATE_LIMIT: i64 = 2000;

impl DeliveryNoteService {
    // ---------- submit ----------

    pub async fn submit(
        conn: &mut PgConnection,
        snowflake: &SnowflakeIdGenerator,
        note_id: i64,
        version: i32,
        current: &CurrentUser,
    ) -> Result<super::super::dto::DeliveryNoteOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk, Role::Inspector])?;

        let mut obj = DeliveryNoteRepo::get_by_id(&mut *conn, note_id, false)
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

        // 所有批次必须 READY_TO_SHIP
        let batches = PartBatchRepo::list_by_delivery_note(&mut *conn, note_id).await?;
        if batches.is_empty() {
            return Err(AppError::biz(
                code::BIZ_DELIVERY_NOTE_INVALID_VALUE,
                "empty delivery note; add parts before submit",
            ));
        }
        for b in &batches {
            if b.status != STATUS_READY_TO_SHIP {
                return Err(AppError::biz(
                    code::BIZ_DELIVERY_NOTE_PART_NOT_READY,
                    format!(
                        "batch {} status={} (must be READY_TO_SHIP at submit)",
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

        write_event(
            conn,
            snowflake,
            note_id,
            DeliveryNoteEventType::Submitted,
            Some(STATUS_DRAFT.to_string()),
            Some(STATUS_SUBMITTED.to_string()),
            None,
            Some(current.id),
        )
        .await?;

        let affected = DeliveryNoteRepo::update(&mut *conn, &obj).await?;
        if affected == 0 {
            return Err(AppError::biz(
                code::VERSION_CONFLICT,
                "concurrent modification detected",
            ));
        }
        obj = DeliveryNoteRepo::get_by_id(&mut *conn, note_id, false)
            .await?
            .ok_or_else(|| note_not_found(note_id))?;
        let out = build_note_outs(conn, std::slice::from_ref(&obj)).await?;
        Ok(out.into_iter().next().unwrap())
    }

    // ---------- recall ----------

    pub async fn recall(
        conn: &mut PgConnection,
        snowflake: &SnowflakeIdGenerator,
        note_id: i64,
        version: i32,
        current: &CurrentUser,
    ) -> Result<super::super::dto::DeliveryNoteOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk, Role::Inspector])?;

        let mut obj = DeliveryNoteRepo::get_by_id(&mut *conn, note_id, false)
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

        // 同范围 DRAFT 撞唯一（设计 §3.3 / 21419）：如果存在另一张同范围的活跃 DRAFT 则拒
        let scope = scope_from_note(&obj);
        if let Some(_other) =
            DeliveryNoteRepo::find_open_draft_by_scope(&mut *conn, obj.customer_id, scope, Some(note_id))
                .await?
        {
            return Err(AppError::biz(
                code::BIZ_DELIVERY_NOTE_DRAFT_SCOPE_CONFLICT,
                "同范围已存在 DRAFT 草稿；请先处理现有草稿再 recall",
            ));
        }

        let now = now_naive();
        obj.status = STATUS_DRAFT.to_string();
        obj.submitted_at = None;
        obj.submitted_by = None;
        obj.version += 1;
        obj.updated_at = now;
        obj.updated_by = Some(current.id);

        write_event(
            conn,
            snowflake,
            note_id,
            DeliveryNoteEventType::Withdrawn,
            Some(STATUS_SUBMITTED.to_string()),
            Some(STATUS_DRAFT.to_string()),
            None,
            Some(current.id),
        )
        .await?;

        let affected = DeliveryNoteRepo::update(&mut *conn, &obj).await?;
        if affected == 0 {
            return Err(AppError::biz(
                code::VERSION_CONFLICT,
                "concurrent modification detected",
            ));
        }
        obj = DeliveryNoteRepo::get_by_id(&mut *conn, note_id, false)
            .await?
            .ok_or_else(|| note_not_found(note_id))?;
        let out = build_note_outs(conn, std::slice::from_ref(&obj)).await?;
        Ok(out.into_iter().next().unwrap())
    }

    // ---------- pickup_scan ----------

    pub async fn pickup_scan(
        conn: &mut PgConnection,
        note_id: i64,
        part_serial: &str,
        _badge_code: Option<&str>,
        current: &CurrentUser,
    ) -> Result<DeliveryNotePickupScanOut, AppError> {
        let _ = current;

        let obj = DeliveryNoteRepo::get_by_id(&mut *conn, note_id, false)
            .await?
            .ok_or_else(|| note_not_found(note_id))?;
        if obj.status != STATUS_SUBMITTED {
            return Err(AppError::biz(
                code::BIZ_DELIVERY_NOTE_NOT_SUBMITTED,
                format!("only SUBMITTED can be scanned, current={}", obj.status),
            ));
        }

        let part = PartRepo::get_by_serial(&mut *conn, part_serial, false).await?;
        let note_batches = PartBatchRepo::list_by_delivery_note(&mut *conn, note_id).await?;
        if part.is_none() || !note_batches.iter().any(|b| Some(b.part_id) == part.as_ref().map(|p| p.id)) {
            return Err(AppError::biz(
                code::BIZ_DELIVERY_NOTE_SCAN_MISMATCH,
                format!("serial {part_serial:?} is not in this delivery note"),
            ));
        }

        Ok(DeliveryNotePickupScanOut {
            delivery_note_id: note_id,
            scanned_count: 0,
            expected_count: note_batches.len() as i64,
            ready: false,
            scanned_serials: vec![],
        })
    }

    // ---------- pickup ----------

    pub async fn pickup(
        conn: &mut PgConnection,
        snowflake: &SnowflakeIdGenerator,
        note_id: i64,
        driver_worker_id: i64,
        version: i32,
        _badge_code: Option<&str>,
        current: &CurrentUser,
    ) -> Result<super::super::dto::DeliveryNoteOut, AppError> {
        // 任意已登录账号即可（service 层校验司机）
        let _ = current;

        let mut obj = DeliveryNoteRepo::get_by_id(&mut *conn, note_id, false)
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
        let driver = WorkerRepo::get_by_id(&mut *conn, driver_worker_id, false)
            .await?
            .ok_or_else(|| AppError::biz(
                code::BIZ_DELIVERY_NOTE_DRIVER_INVALID,
                format!("driver worker {driver_worker_id} not found or inactive"),
            ))?;
        if !driver.is_active {
            return Err(AppError::biz(
                code::BIZ_DELIVERY_NOTE_DRIVER_INVALID,
                format!("driver worker {driver_worker_id} not active"),
            ));
        }
        if let Some(wt_id) = driver.work_type_id {
            let wt = WorkTypeRepo::get_by_id(&mut *conn, wt_id)
                .await?
                .ok_or_else(|| AppError::biz(
                    code::BIZ_DELIVERY_NOTE_DRIVER_INVALID,
                    "driver work_type not found",
                ))?;
            if wt.code != WORK_TYPE_DRIVER_CODE {
                return Err(AppError::biz(
                    code::BIZ_DELIVERY_NOTE_DRIVER_INVALID,
                    format!(
                        "driver work_type {} != {WORK_TYPE_DRIVER_CODE:?}",
                        wt.code
                    ),
                ));
            }
        } else {
            return Err(AppError::biz(
                code::BIZ_DELIVERY_NOTE_DRIVER_INVALID,
                "driver has no work_type",
            ));
        }

        // 校验所有批次 READY_TO_SHIP + 非空
        let mut note_batches = PartBatchRepo::list_by_delivery_note(&mut *conn, note_id).await?;
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
        for b in &mut note_batches {
            b.status = "DELIVERED".to_string();
            b.current_holder_id = None;
            b.location = None;
            b.version += 1;
            b.updated_at = now;
            b.updated_by = Some(current.id);
            let affected = PartBatchRepo::update(
                &mut *conn,
                b.id,
                b.version - 1, // expected_version 是之前的
                b.delivery_note_id, // 保留 delivery_note_id（PICKED_UP/ARCHIVED 后仍可打印）
                Some("DELIVERED"),
                now,
                Some(current.id),
            )
            .await?;
            if affected == 0 {
                return Err(AppError::biz(
                    code::VERSION_CONFLICT,
                    "concurrent modification detected",
                ));
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

        write_event(
            conn,
            snowflake,
            note_id,
            DeliveryNoteEventType::PickedUp,
            Some(STATUS_SUBMITTED.to_string()),
            Some(STATUS_PICKED_UP.to_string()),
            None,
            Some(current.id),
        )
        .await?;

        let affected = DeliveryNoteRepo::update(&mut *conn, &obj).await?;
        if affected == 0 {
            return Err(AppError::biz(
                code::VERSION_CONFLICT,
                "concurrent modification detected",
            ));
        }
        obj = DeliveryNoteRepo::get_by_id(&mut *conn, note_id, false)
            .await?
            .ok_or_else(|| note_not_found(note_id))?;
        let out = build_note_outs(conn, std::slice::from_ref(&obj)).await?;
        Ok(out.into_iter().next().unwrap())
    }

    // ---------- soft_delete ----------

    pub async fn soft_delete(
        conn: &mut PgConnection,
        note_id: i64,
        version: i32,
        current: &CurrentUser,
    ) -> Result<(), AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk, Role::Inspector])?;

        let obj = DeliveryNoteRepo::get_by_id(&mut *conn, note_id, false)
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
        .execute(&mut *conn)
        .await?;

        let affected =
            DeliveryNoteRepo::soft_delete(&mut *conn, note_id, obj.version, now_naive(), Some(current.id))
                .await?;
        if affected == 0 {
            return Err(AppError::biz(
                code::VERSION_CONFLICT,
                "concurrent modification detected",
            ));
        }
        Ok(())
    }

    // ---------- list_events ----------

    pub async fn list_events(
        conn: &mut PgConnection,
        note_id: i64,
    ) -> Result<Vec<DeliveryNoteEventOut>, AppError> {
        // 任何已登录账号可看；service 不做角色硬限（与 Python 一致）
        let events = DeliveryNoteEventRepo::list_by_note(&mut *conn, note_id).await?;
        Ok(events
            .into_iter()
            .map(|e| DeliveryNoteEventOut {
                id: e.id,
                delivery_note_id: e.delivery_note_id,
                event_type: e.event_type,
                from_status: e.from_status,
                to_status: e.to_status,
                note: e.note,
                created_by: e.created_by,
                created_at: Some(e.created_at),
            })
            .collect())
    }

    // ---------- list_candidate_parts ----------

    pub async fn list_candidate_parts(
        conn: &mut PgConnection,
        customer_id: i64,
        current: &CurrentUser,
    ) -> Result<Vec<DeliveryNoteCandidatePart>, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk, Role::Inspector])?;

        let cust = CustomerRepo::get_by_id(&mut *conn, customer_id, false)
            .await?
            .ok_or_else(|| super::inner::customer_not_found(customer_id))?;
        if cust.parent_id.is_some() {
            return Err(AppError::biz(
                code::BIZ_INVALID_VALUE,
                format!("customer {customer_id} 不是一级客户；candidate-parts 必须传一级客户"),
            ));
        }

        // L1 根下所有 active 子客户 (L2) + L1 自身
        let children = CustomerRepo::list_children(&mut *conn, customer_id, false).await?;
        let mut customer_ids: Vec<i64> = children.iter().map(|c| c.id).collect();
        customer_ids.push(customer_id);
        let mut name_by_id: HashMap<i64, String> =
            children.iter().map(|c| (c.id, c.name.clone())).collect();
        name_by_id.insert(customer_id, cust.name.clone());
        let root_name = cust.name.clone();

        let statuses = [STATUS_INSPECTION, STATUS_READY_TO_SHIP];
        let rows = PartBatchRepo::list_batches_with_part_in_customers(
            &mut *conn,
            &statuses,
            &customer_ids,
            CANDIDATE_LIMIT,
        )
        .await?;

        // 过滤：在 active 单 (DRAFT/SUBMITTED) 上的批次排除
        let linked_note_ids: HashSet<i64> = rows
            .iter()
            .filter_map(|(b, _)| b.delivery_note_id)
            .collect();
        let active_note_ids: HashSet<i64> = if linked_note_ids.is_empty() {
            HashSet::new()
        } else {
            let notes = DeliveryNoteRepo::list_by_ids(
                &mut *conn,
                &linked_note_ids.iter().copied().collect::<Vec<_>>(),
                false,
            )
            .await?;
            notes
                .into_iter()
                .filter(|n| n.status == STATUS_DRAFT || n.status == STATUS_SUBMITTED)
                .map(|n| n.id)
                .collect()
        };

        let mut result = Vec::with_capacity(rows.len());
        for (b, p) in rows {
            if let Some(dnid) = b.delivery_note_id
                && active_note_ids.contains(&dnid)
            {
                continue;
            }
            let leaf_name = name_by_id.get(&p.customer_id).cloned();
            let path = match (&root_name, &leaf_name) {
                (r, Some(l)) if r != l => Some(format!("{r} / {l}")),
                _ => leaf_name.clone(),
            };
            result.push(DeliveryNoteCandidatePart {
                id: p.id,
                batch_id: b.id,
                batch_no: b.batch_no,
                batch_label: match &p.serial_no {
                    Some(s) => format!("{s}B{:02}", b.batch_no),
                    None => format!("批次{}", b.batch_no),
                },
                serial_no: p.serial_no.clone().unwrap_or_default(),
                drawing_no: p.drawing_no.clone(),
                name: p.name.clone(),
                quantity: b.quantity,
                applicant_name: None, // TPart 当前投影不含该列（part 域实施阶段补）
                status: b.status.clone(),
                planned_delivery_date: None, // 同上
                order_no: None,              // 同上
                customer_name: leaf_name.clone(),
                parent_customer_name: Some(root_name.clone()),
                customer_path: path,
            });
        }
        Ok(result)
    }
}

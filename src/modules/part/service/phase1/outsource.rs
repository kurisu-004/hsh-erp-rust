//! Phase 1 / 1.3 外协流转（含 1.3 末尾的辅助列表）
//!
//! 方法：`send_to_outsource` / `receive_from_outsource` /
// `receive_from_outsource_to_inspection` / `list_outsource_in_flight` /
// `list_outsource_sendable`。
//!
//! 2026-09-22 D-6：从原 `phase1.rs` 按业务动作拆出。共享 helper 在 `phase1/mod.rs` 同 crate 内可见。

use crate::auth::rbac::{CurrentUser, Role};
use crate::infra::snowflake::SnowflakeIdGenerator;
use crate::modules::part::model::NewPartEvent;
use crate::modules::part::repo::PartListFilters;
use crate::modules::part::repo::PartRepoTrait;
use crate::modules::part::statemachine::PartStatus;
use crate::modules::prod::process_chain::repo::ProcessChainRepo;
use crate::shared::error::{AppError, code};

use super::super::super::dto_crud::{
    PartListItem, PartListOut, PartListQuery, PlaceOnShelfRequest,
    ReceiveFromOutsourceToInspectionRequest, SendToOutsourceRequest,
};
use super::super::PartService;

use super::{
    assert_shelf_maps_process, ensure_transition, mark_batch_with_status_and_meta,
    require_process_chain, validate_batch_ownership, validate_shelf_zone,
};

impl PartService {
    // ===== 1.3 外协流转 =====

    /// `POST /parts/{id}/send-to-outsource`：PENDING / IN_PROCESS+PRODUCTION_SHELF → OUTSOURCE。
    ///
    /// Phase 2（2026-09-13）扩展：
    /// - 同事务 INSERT t_outsource_shipment（status=OUTSOURCING，quantity=batch.quantity）
    /// - 若 `quote_id` 提供：校验 APPROVED 状态，写 `t_outsource_quote_event` `SENT`
    /// - `direct=true` 时 stub 返回 501 NOT_IMPLEMENTED（follow-up）
    pub async fn send_to_outsource<R: PartRepoTrait>(
        mut repo: R,
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
        let part = repo
            .get_part_inspected(part_id)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_PART_NOT_FOUND, "part 不存在"))?;
        let batch = repo
            .find_batch_by_id(req.batch_id)
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
        // 2026-09-16 PR-3：part 进入生产流前必须已绑定工艺链
        let chain_id = require_process_chain(repo.conn_mut(), part_id).await?;
        // 校验 outsource 公司存在 + 启用
        let company_row: Option<(bool,)> = sqlx::query_as(
            "SELECT is_active FROM t_outsource_company WHERE id = $1 AND deleted_at IS NULL",
        )
        .bind(req.outsource_company_id)
        .fetch_optional(repo.conn_mut())
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
        let proc: Option<(i64,)> =
            sqlx::query_as("SELECT id FROM t_process WHERE id = $1 AND deleted_at IS NULL")
                .bind(req.process_id)
                .fetch_optional(repo.conn_mut())
                .await?;
        if proc.is_none() {
            return Err(AppError::biz(
                code::BIZ_PROCESS_NOT_FOUND,
                format!("process {} 不存在", req.process_id),
            ));
        }
        // PR-3：解析 step_id（chain 内 process_id → step_id）写入 OUTSOURCE 批次
        let step_id =
            ProcessChainRepo::resolve_step_id_by_process(repo.conn_mut(), chain_id, req.process_id)
                .await?
                .ok_or_else(|| {
                    AppError::biz(
                        code::BIZ_PROCESS_CHAIN_STEP_NOT_FOUND,
                        format!(
                            "chain {} 内找不到 process_id={} 的活跃 step",
                            chain_id, req.process_id
                        ),
                    )
                })?;
        // Phase 2：quote_id 可选；若提供必须 APPROVED 状态
        let (quote_id_opt, unit_price): (Option<i64>, Option<rust_decimal::Decimal>) =
            if let Some(qid) = req.quote_id {
                let row: Option<(String, rust_decimal::Decimal, i64, i64, i64)> = sqlx::query_as(
                    "SELECT status, price, part_id, outsource_company_id, process_id \
                 FROM t_outsource_quote \
                 WHERE id = $1 AND deleted_at IS NULL",
                )
                .bind(qid)
                .fetch_optional(repo.conn_mut())
                .await?;
                let (status, price, q_part_id, q_company_id, q_process_id) =
                    row.ok_or_else(|| {
                        AppError::biz(
                            code::BIZ_OUTSOURCE_QUOTE_NOT_FOUND,
                            format!("quote {qid} 不存在"),
                        )
                    })?;
                if status != "APPROVED" {
                    return Err(AppError::biz(
                        code::BIZ_OUTSOURCE_QUOTE_NOT_APPROVED,
                        format!("quote {qid} 当前 {status}，非 APPROVED 不可发送"),
                    ));
                }
                if q_part_id != part_id
                    || q_company_id != req.outsource_company_id
                    || q_process_id != req.process_id
                {
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
            repo.conn_mut(),
            batch.id,
            req.version,
            "OUTSOURCE",
            Some("OUTSOURCE_COMPANY"),
            Some(req.outsource_company_id),
            Some(step_id),
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
        .execute(repo.conn_mut())
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
            .execute(repo.conn_mut())
            .await?;
        }
        let _ = Self::sync_from_batch_change(&mut repo, part_id, current).await?;
        repo.insert_part_event(
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
        let fresh = repo
            .get_part_inspected(part_id)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_PART_NOT_FOUND, "send-to-outsource 后查不到"))?;
        Ok(crate::modules::part::dto::PartOut::from(fresh))
    }

    /// `POST /parts/{id}/receive-from-outsource`：OUTSOURCE → IN_PROCESS（PRODUCTION_SHELF）。
    ///
    /// Phase 2（2026-09-13）扩展：同事务把批次开口 shipment 标 RECEIVED + 写 RECEIVED 事件。
    ///
    /// 2026-09-16 PR-3 批次 step 化：chain 必须性守卫 + req.next_process_id
    /// 解析为 step_id 写入 current_process_step_id。
    pub async fn receive_from_outsource<R: PartRepoTrait>(
        mut repo: R,
        snowflake: &SnowflakeIdGenerator,
        part_id: i64,
        req: PlaceOnShelfRequest,
        current: &CurrentUser,
    ) -> Result<crate::modules::part::dto::PartOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk, Role::Inspector])?;
        let part = repo
            .get_part_inspected(part_id)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_PART_NOT_FOUND, "part 不存在"))?;
        let batch = repo
            .find_batch_by_id(req.batch_id)
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
        // PR-3：part 必须已绑定工艺链
        let chain_id = require_process_chain(repo.conn_mut(), part_id).await?;
        validate_shelf_zone(repo.conn_mut(), req.shelf_id, "PRODUCTION").await?;
        assert_shelf_maps_process(repo.conn_mut(), req.shelf_id, req.next_process_id).await?;
        // PR-3：解析 step_id
        let step_id =
            ProcessChainRepo::resolve_step_id_by_process(repo.conn_mut(), chain_id, req.next_process_id)
                .await?
                .ok_or_else(|| {
                    AppError::biz(
                        code::BIZ_PROCESS_CHAIN_STEP_NOT_FOUND,
                        format!(
                            "chain {} 内找不到 process_id={} 的活跃 step",
                            chain_id, req.next_process_id
                        ),
                    )
                })?;
        let n = mark_batch_with_status_and_meta(
            repo.conn_mut(),
            batch.id,
            req.version,
            "IN_PROCESS",
            Some("PRODUCTION_SHELF"),
            Some(req.shelf_id),
            Some(step_id),
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
        .fetch_optional(repo.conn_mut())
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
            .execute(repo.conn_mut())
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
                .execute(repo.conn_mut())
                .await?;
            }
        }
        let _ = Self::sync_from_batch_change(&mut repo, part_id, current).await?;
        repo.insert_part_event(
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
        let fresh = repo
            .get_part_inspected(part_id)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_PART_NOT_FOUND, "receive 后查不到"))?;
        Ok(crate::modules::part::dto::PartOut::from(fresh))
    }

    /// `POST /parts/{id}/receive-from-outsource-to-inspection`：OUTSOURCE → INSPECTION。
    ///
    /// Phase 2（2026-09-13）扩展：同事务把批次对应开口 shipment 标 RECEIVED（与
    /// `receive_from_outsource` 同样的"整批接收"语义）。
    pub async fn receive_from_outsource_to_inspection<R: PartRepoTrait>(
        mut repo: R,
        snowflake: &SnowflakeIdGenerator,
        part_id: i64,
        req: ReceiveFromOutsourceToInspectionRequest,
        current: &CurrentUser,
    ) -> Result<crate::modules::part::dto::PartOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk, Role::Inspector])?;
        let part = repo
            .get_part_inspected(part_id)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_PART_NOT_FOUND, "part 不存在"))?;
        let batch = repo
            .find_batch_by_id(req.batch_id)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_PART_BATCH_NOT_FOUND, "batch 不存在"))?;
        validate_batch_ownership(batch.part_id, batch.id, part_id, req.version, batch.version)?;
        let from = PartStatus::from_str(&batch.status)
            .ok_or_else(|| AppError::biz(code::BIZ_INVALID_VALUE, "batch.status 非法"))?;
        ensure_transition(
            from,
            PartStatus::INSPECTION,
            "receive-from-outsource-to-inspection",
        )?;
        if from != PartStatus::OUTSOURCE {
            return Err(AppError::biz(
                code::BIZ_INVALID_TRANSITION,
                "receive-from-outsource-to-inspection: 源状态必须是 OUTSOURCE",
            ));
        }
        validate_shelf_zone(repo.conn_mut(), req.shelf_id, "INSPECTION").await?;
        let n = mark_batch_with_status_and_meta(
            repo.conn_mut(),
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
        .fetch_optional(repo.conn_mut())
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
            .execute(repo.conn_mut())
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
                .execute(repo.conn_mut())
                .await?;
            }
        }
        let _ = Self::sync_from_batch_change(&mut repo, part_id, current).await?;
        repo.insert_part_event(
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
        let fresh = repo
            .get_part_inspected(part_id)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_PART_NOT_FOUND, "receive 后查不到"))?;
        Ok(crate::modules::part::dto::PartOut::from(fresh))
    }

    // ===== 1.3 外协辅助列表 =====

    /// `GET /parts/outsource-in-flight`：status=OUTSOURCE 工单一览。
    /// 简化版：复用 `list_parts` 但强制 status=OUTSOURCE。
    pub async fn list_outsource_in_flight<R: PartRepoTrait>(
        mut repo: R,
        query: &PartListQuery,
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
            // 2026-09-17 PR-4 守卫修复：list_outsource_in_flight 不透传
            // locations/holder_ids（业务语义固定 OUTSOURCE 状态）
            locations: &[],
            holder_ids: &[],
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
        let items = repo.list_with_filters(&f).await?;
        let total = repo.count_with_filters(&f).await?;
        let list_items: Vec<PartListItem> = items
            .into_iter()
            .map(|p| PartListItem {
                part: p,
                customer_name: None,
                l1_customer_name: None,
                location: None,
                holder_name: None,
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
    pub async fn list_outsource_sendable<R: PartRepoTrait>(
        mut repo: R,
        query: &PartListQuery,
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
            // 2026-09-17 PR-4 守卫修复：list_outsource_sendable 不透传
            // locations/holder_ids（业务语义固定 PENDING+IN_PROCESS 状态）
            locations: &[],
            holder_ids: &[],
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
        let items = repo.list_with_filters(&f).await?;
        let total = repo.count_with_filters(&f).await?;
        let list_items: Vec<PartListItem> = items
            .into_iter()
            .map(|p| PartListItem {
                part: p,
                customer_name: None,
                l1_customer_name: None,
                location: None,
                holder_name: None,
            })
            .collect();
        Ok(PartListOut {
            items: list_items,
            total,
            limit,
            offset,
        })
    }
}
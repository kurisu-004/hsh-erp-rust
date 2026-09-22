//! part 域单件 CRUD 业务逻辑（2026-09-16 M2-C 拆分）
//!
//! 本文件承载单件 CRUD + 列表 + 上传 + 历史 / 位置树等查询；批量创建相关逻辑
//! （含直传 COS 文件绑定 `batch_create_parts_with_bindings` / legacy 路径 /
//! `prepare_binding_head_copy`）已迁出到 `service/batch.rs`，避免单文件超 1000 行。
//!
//! ## 范围（本文件）
//! - `create_part` / `list_parts` / `list_inspection_batches` / `get_part` /
//!   `get_part_by_serial` / `get_part_batches_by_serial` / `update_part` /
//!   `soft_delete_part` / `upload_part_file` / `upload_drawing` / `upload_3d_model`
//! - `batch_create_parts` —— 薄包装，转 `service/batch.rs::batch_create_parts_legacy`
//!
//! ## helpers（pub(super)，供 batch.rs 复用）
//! - `map_create_error` —— sqlx 错误码 → 业务错误码
//! - `expand_customer_id` —— L1+L2 客户 id 展开
//! - `lookup_customer_names` —— 取客户名 + L1 名
//!
//! 2026-09-22 D-6 重构：方法签名 `<R: PartRepoTrait>`（by-value；trait 已直接
//! `impl for &mut PgConnection`）。生产 `R = &mut PgConnection`，handler/service
//! 借 `&mut *tx` / `repo.conn_mut()` 即可喂给 trait 与跨域 ZST 调用。Inline sqlx 查询
//! 与 ZST 跨域调用走 `repo.conn_mut()`（同一 `PgConnection` 借位，trait 与
//! `CustomerRepo` / `ProcessChainRepo` / `PartBatchRepo` 等同时持有）。

use sqlx::PgConnection;
use std::sync::Arc;

use chrono::NaiveDate;

use crate::auth::rbac::{CurrentUser, Role};
use crate::infra::snowflake::SnowflakeIdGenerator;
use crate::modules::com::customer::repo::CustomerRepo;
use crate::modules::part::dto::{
    InspectionBatchListItemOut, InspectionBatchListOut, InspectionBatchListQuery, PartBatchScanOut,
    PartScanContextOut, PartScanInfoOut,
};
use crate::modules::part::model::NewPartEvent;
use crate::modules::part::repo::PartRepoTrait;
use crate::modules::part::repo::{NewPartCreate, PartListFilters, PartUpdate};
use crate::modules::part::batch::repo::{NewInitialBatch, PartBatchRepo};
use crate::modules::part_file::model::TPartFile;
use crate::modules::part_file::policy; // 2026-09-11 新增：kind → 扩展名 / content_type 白名单
use crate::modules::part_file::repo::{NewPartFile, PartFileRepo, hash_bytes};
use crate::modules::prod::process_chain::repo::ProcessChainRepo;
use crate::shared::error::{AppError, code};
use crate::state::AppState;

use super::super::dto_crud::{
    PartBatchCreateRequest, PartCreateRequest, PartDetailOut, PartListItem, PartListOut,
    PartListQuery, PartUpdateRequest,
};
use super::PartService;
use super::list_enrichment::enrich_part_list_with_location_and_holder;

/// 扫码快捷品检上下文内部 FromRow 结构。
///
/// 仅本 crate 可见：`get_part_batches_by_serial` 用 `sqlx::query_as!` 接收
/// `t_part` 的窄字段（id + 8 列），避免读 28 列 `TPart`。由
/// `PartScanInfoOut::from` 转 DTO，转换实现位于 `src/modules/part/dto.rs`
/// （与 DTO 同处，便于维护）。
#[derive(sqlx::FromRow)]
pub(crate) struct TPartScanRow {
    pub(crate) id: i64,
    pub(crate) drawing_no: String,
    pub(crate) name: String,
    pub(crate) quantity: i32,
    pub(crate) customer_id: i64,
    pub(crate) system_delivery_date: Option<NaiveDate>,
    pub(crate) is_urgent: bool,
    pub(crate) order_no: Option<String>,
    pub(crate) note: Option<String>,
}

impl PartService {
    pub async fn create_part<R: PartRepoTrait>(
        mut repo: R,
        snowflake: &SnowflakeIdGenerator,
        req: &PartCreateRequest,
        current: &CurrentUser,
    ) -> Result<PartDetailOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk])?;
        if req.name.trim().is_empty()
            || req.drawing_no.trim().is_empty()
            || req.applicant_name.trim().is_empty()
        {
            return Err(AppError::validation(
                "name / drawing_no / applicant_name 均不可为空",
            ));
        }
        if req.quantity <= 0 {
            return Err(AppError::validation("quantity 必须 > 0"));
        }
        let _customer = CustomerRepo::get_by_id(repo.conn_mut(), req.customer_id, false)
            .await?
            .ok_or_else(|| {
                AppError::biz(
                    code::BIZ_CUSTOMER_NOT_FOUND,
                    format!("customer {} 不存在", req.customer_id),
                )
            })?;
        let new_id = snowflake.next_id();
        let new = NewPartCreate {
            id: new_id,
            name: req.name.trim(),
            drawing_no: req.drawing_no.trim(),
            applicant_name: req.applicant_name.trim(),
            quantity: req.quantity,
            request_date: req.request_date,
            planned_delivery_date: req.planned_delivery_date,
            is_urgent: req.is_urgent,
            customer_id: req.customer_id,
            assembly_id: req.assembly_id,
            order_no: req.order_no.as_deref(),
            system_delivery_date: req.system_delivery_date,
            note: req.note.as_deref(),
            created_by: current.id,
        };
        if let Err(e) = repo.create_part(new).await {
            return Err(map_create_error(e));
        }
        // 2026-09-11 part/assembly/batch 重构方案 §4.1 (PR-B1)：同事务插入初始
        // t_part_batch（batch_no=1 / status='PENDING' / location=NULL），让新建工单
        // 即可走 to_inspection / to_ship / pickup 等 batch-锚定流转。
        let initial_batch_id = snowflake.next_id();
        PartBatchRepo::create_initial_batch(
            repo.conn_mut(),
            NewInitialBatch {
                id: initial_batch_id,
                part_id: new_id,
                quantity: req.quantity,
                location: None,
                created_by: Some(current.id),
            },
        )
        .await?;
        let part = repo
            .get_part_detail(new_id)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_PART_NOT_FOUND, "新建 part 查不到"))?;
        let (cn, l1cn) = lookup_customer_names(repo.conn_mut(), part.customer_id).await?;
        let current_batch_id = repo.find_current_inspection_batch_id(part.id).await?;
        Ok(PartDetailOut::from_with_customer_extra(
            part,
            current_batch_id,
            cn,
            l1cn,
        ))
    }

    pub async fn batch_create_parts<R: PartRepoTrait>(
        repo: R,
        snowflake: &SnowflakeIdGenerator,
        req: &PartBatchCreateRequest,
        current: &CurrentUser,
    ) -> Result<crate::modules::part::dto_crud::PartBatchCreateOut, AppError> {
        // 2026-09-16 M2-B + M2-C：薄包装转 legacy 实现（不绑定文件）。
        // 文件绑定走 `batch_create_parts_with_bindings`（handler 层显式选，
        // 实现已迁出到 `service/batch.rs`）。
        Self::batch_create_parts_legacy(repo, snowflake, req, current).await
    }

    pub async fn list_parts<R: PartRepoTrait>(
        mut repo: R,
        query: &PartListQuery,
        current: &CurrentUser,
    ) -> Result<PartListOut, AppError> {
        current.require_any_role(&[
            Role::Manager,
            Role::Clerk,
            Role::Inspector,
            Role::CncProgrammer,
        ])?;
        let limit = query.limit.unwrap_or(50).clamp(1, 200);
        let offset = query.offset.unwrap_or(0).max(0);
        let sort_by = [
            "CREATED_AT",
            "UPDATED_AT",
            "PLANNED_DELIVERY_DATE",
            "REQUEST_DATE",
            "SERIAL_NO",
            "DRAWING_NO",
            "NAME",
        ]
        .iter()
        .find(|&&s| Some(s) == query.sort_by.as_deref())
        .copied()
        .unwrap_or("CREATED_AT");
        let sort_dir = if query
            .sort_dir
            .as_deref()
            .map(|s| s.eq_ignore_ascii_case("ASC"))
            .unwrap_or(false)
        {
            "ASC"
        } else {
            "DESC"
        };

        let customer_ids_owned: Vec<i64>;
        let customer_ids: &[i64] = if let Some(cid) = query.customer_id {
            customer_ids_owned = expand_customer_id(repo.conn_mut(), cid).await?;
            &customer_ids_owned
        } else {
            &[]
        };
        let statuses_owned: Vec<String> = query
            .statuses
            .as_deref()
            .map(|s| {
                s.split(',')
                    .filter(|x| !x.is_empty())
                    .map(|s| s.to_string())
                    .collect()
            })
            .unwrap_or_default();
        // 2026-09-17 PR-4 守卫修复：locations 逗号分隔 → Vec<String>
        let locations_owned: Vec<String> = query
            .locations
            .as_deref()
            .map(|s| {
                s.split(',')
                    .filter(|x| !x.is_empty())
                    .map(|s| s.trim().to_string())
                    .collect()
            })
            .unwrap_or_default();
        // 2026-09-17 PR-4 守卫修复：holder_ids 逗号分隔雪花 ID 字符串 → Vec<i64>
        // 解析失败 → 返回 40001 VALIDATION_ERROR（前端拿到 4xx 立即修正）
        let holder_ids_owned: Vec<i64> = match query.holder_ids.as_deref() {
            Some(s) if !s.is_empty() => s
                .split(',')
                .filter(|x| !x.is_empty())
                .map(|x| {
                    x.trim()
                        .parse::<i64>()
                        .map_err(|_| AppError::validation(format!("holder_ids 含非法雪花 ID: {x}")))
                })
                .collect::<Result<Vec<_>, _>>()?,
            _ => Vec::new(),
        };

        let filters = PartListFilters {
            customer_ids,
            status: query.status.as_deref(),
            statuses: &statuses_owned,
            is_urgent: query.is_urgent,
            keyword: query.keyword.as_deref(),
            locations: &locations_owned,
            holder_ids: &holder_ids_owned,
            sort_by,
            sort_dir,
            limit,
            offset,
            include_deleted: false,
        };
        let rows = repo.list_with_filters(&filters).await?;
        let total = repo.count_with_filters(&filters).await?;

        // 2026-09-16 PR-2 瘦身（migration 027）：t_part 删 `location` /
        // `current_holder_id`（已删列），列表页需要的「位置 / 持有人」展示由
        // service 层在 list_parts 内按 min-progress 活跃批次派生。
        //
        // 实现：
        // 1. 用 PartBatchRepo::list_active_by_part_ids 一次拿齐页内 part 的全部
        //    活跃批次（防 N+1）。
        // 2. Rust 内按 min-progress（见 part::statemachine::part_status_progress）
        //    选每 part 的派生批次。
        // 3. 把目标批次的 current_holder_id 按 batch.location 分桶（SHELF →
        //    t_shelf；WORKER → t_worker；OUTSOURCE_COMPANY → t_outsource_company），
        //    每桶 1 条 IN 查询解析名称（最多 3 条 SQL，与页大小 N 无关）。
        let part_ids: Vec<i64> = rows.iter().map(|p| p.id).collect();
        let batch_enrichment =
            enrich_part_list_with_location_and_holder(&mut repo, &part_ids).await?;

        let mut items = Vec::with_capacity(rows.len());
        for p in rows {
            let (cn, l1cn) = lookup_customer_names(repo.conn_mut(), p.customer_id).await?;
            let (loc, holder) = batch_enrichment.get(&p.id).cloned().unwrap_or((None, None));
            items.push(PartListItem {
                part: p,
                customer_name: cn,
                l1_customer_name: l1cn,
                location: loc,
                holder_name: holder,
            });
        }
        Ok(PartListOut {
            items,
            total,
            limit,
            offset,
        })
    }

    /// `GET /parts/inspection-batches` 列表：对齐 Python v1
    /// `PartService.list_inspection_batches`，返回 status=INSPECTION 全部活跃批次
    /// （含工单 / holder / process / delivery_note / customer 名称，一次 JOIN 解析，
    /// 服务层无 N+1）。
    ///
    /// 权限：Manager + Inspector（对齐 v1）。
    /// 限流：`limit ∈ [1, 200]`，默认 200；`offset` 默认 0。
    /// customer_id：单值 → `expand_customer_id` 展开为 L1+L2 ids（与 `list_parts` 同逻辑）。
    /// keyword / serial_no：service 层拼 `%...%` 加通配符；为防 SQL 注入风险，
    /// 拒绝 `%` / `_` / `\\` 等通配符特殊字符（含任一 → VALIDATION_ERROR 40001）。
    pub async fn list_inspection_batches<R: PartRepoTrait>(
        mut repo: R,
        query: &InspectionBatchListQuery,
        current: &CurrentUser,
    ) -> Result<InspectionBatchListOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Inspector])?;

        let limit = query.limit.unwrap_or(200).clamp(1, 200);
        let offset = query.offset.unwrap_or(0).max(0);

        // customer_id 展开：单值 → [L1, 所有 L2]；None → 不传（走全客户）
        let customer_ids_owned: Vec<i64>;
        let customer_ids: &[i64] = if let Some(cid) = query.customer_id {
            customer_ids_owned = expand_customer_id(repo.conn_mut(), cid).await?;
            &customer_ids_owned
        } else {
            &[]
        };

        // keyword：service 层拼 `%...%` + 拒绝 SQL 通配符特殊字符（% _ \）
        let keyword_owned: Option<String> = match query.keyword.as_deref() {
            Some(kw) => {
                if kw.contains(['%', '_', '\\']) {
                    return Err(AppError::validation("keyword 不能包含通配符 % _ \\"));
                }
                Some(format!("%{kw}%"))
            }
            None => None,
        };
        let keyword: Option<&str> = keyword_owned.as_deref();

        // serial_no：同上
        let serial_no_owned: Option<String> = match query.serial_no.as_deref() {
            Some(sn) => {
                if sn.contains(['%', '_', '\\']) {
                    return Err(AppError::validation("serial_no 不能包含通配符 % _ \\"));
                }
                Some(format!("%{sn}%"))
            }
            None => None,
        };
        let serial_no: Option<&str> = serial_no_owned.as_deref();

        let statuses: &[&str] = &["INSPECTION"];

        let rows = PartBatchRepo::list_batches_with_part(
            repo.conn_mut(),
            statuses,
            customer_ids,
            keyword,
            serial_no,
            query.planned_delivery_date_from,
            query.planned_delivery_date_to,
            limit,
            offset,
        )
        .await?;

        let total = PartBatchRepo::count_batches_with_part(
            repo.conn_mut(),
            statuses,
            customer_ids,
            keyword,
            serial_no,
            query.planned_delivery_date_from,
            query.planned_delivery_date_to,
        )
        .await?;

        Ok(InspectionBatchListOut {
            items: rows
                .into_iter()
                .map(InspectionBatchListItemOut::from)
                .collect(),
            total,
            limit,
            offset,
        })
    }

    pub async fn get_part<R: PartRepoTrait>(
        mut repo: R,
        part_id: i64,
        current: &CurrentUser,
    ) -> Result<PartDetailOut, AppError> {
        current.require_any_role(&[
            Role::Manager,
            Role::Clerk,
            Role::Inspector,
            Role::CncProgrammer,
        ])?;
        let part = repo
            .get_part_detail(part_id)
            .await?
            .ok_or_else(|| {
                AppError::biz(
                    code::BIZ_PART_NOT_FOUND,
                    format!("part {part_id} 不存在或已删除"),
                )
            })?;
        let (cn, l1cn) = lookup_customer_names(repo.conn_mut(), part.customer_id).await?;
        let current_batch_id = repo.find_current_inspection_batch_id(part.id).await?;
        Ok(PartDetailOut::from_with_customer_extra(
            part,
            current_batch_id,
            cn,
            l1cn,
        ))
    }

    pub async fn get_part_by_serial<R: PartRepoTrait>(
        mut repo: R,
        serial_no: &str,
        current: &CurrentUser,
    ) -> Result<PartDetailOut, AppError> {
        current.require_any_role(&[
            Role::Manager,
            Role::Clerk,
            Role::Inspector,
            Role::CncProgrammer,
        ])?;
        let p = repo
            .get_by_serial(serial_no, false)
            .await?
            .ok_or_else(|| {
                AppError::biz(
                    code::BIZ_PART_NOT_FOUND,
                    format!("serial_no {serial_no} 不存在"),
                )
            })?;
        Self::get_part(repo, p.id, current).await
    }

    /// 扫码快捷品检上下文：通过 serial 查工单窄字段 + 全部活跃批次（含 holder 名称）。
    ///
    /// 权限与 `get_part_by_serial` 一致（Manager / Clerk / Inspector / CncProgrammer）。
    /// 用于前端扫码弹窗，让用户直接看到批次（id + quantity + status + holder +
    /// version）并据此拼出 `POST /parts/{part_id}/to-ship` 的 `{ batch_id, version }`
    /// 入参。
    pub async fn get_part_batches_by_serial<R: PartRepoTrait>(
        mut repo: R,
        serial_no: &str,
        current: &CurrentUser,
    ) -> Result<PartScanContextOut, AppError> {
        current.require_any_role(&[
            Role::Manager,
            Role::Clerk,
            Role::Inspector,
            Role::CncProgrammer,
        ])?;

        // ① 查工单窄字段（仅 8 列 + id，避免读 28 列）
        let part = sqlx::query_as!(
            TPartScanRow,
            r#"
            SELECT id, drawing_no, name, quantity, customer_id,
                   system_delivery_date, is_urgent, order_no, note
            FROM t_part
            WHERE serial_no = $1 AND deleted_at IS NULL
            "#,
            serial_no,
        )
        .fetch_optional(repo.conn_mut())
        .await?
        .ok_or_else(|| {
            AppError::biz(
                code::BIZ_PART_NOT_FOUND,
                format!("serial_no {serial_no} 不存在"),
            )
        })?;

        // ② 查全部活跃批次（含 holder 名称）
        let batches =
            PartBatchRepo::list_active_by_part_id_with_holder(repo.conn_mut(), part.id).await?;

        // ③ 拼 DTO
        Ok(PartScanContextOut {
            part: PartScanInfoOut::from(part),
            batches: batches.into_iter().map(PartBatchScanOut::from).collect(),
        })
    }

    pub async fn update_part<R: PartRepoTrait>(
        mut repo: R,
        part_id: i64,
        req: &PartUpdateRequest,
        current: &CurrentUser,
    ) -> Result<PartDetailOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk])?;
        let n = repo
            .update_part(
                part_id,
                req.version,
                PartUpdate {
                    name: req.name.as_deref(),
                    drawing_no: req.drawing_no.as_deref(),
                    applicant_name: req.applicant_name.as_deref(),
                    quantity: req.quantity,
                    order_no: req.order_no.as_deref(),
                    system_delivery_date: req.system_delivery_date,
                    planned_delivery_date: req.planned_delivery_date,
                    note: req.note.as_deref(),
                    is_urgent: req.is_urgent,
                    updated_by: current.id,
                },
            )
            .await?;
        if n == 0 {
            return Err(AppError::biz(
                code::VERSION_CONFLICT,
                format!("part {part_id} 版本冲突或已删除"),
            ));
        }
        Self::get_part(repo, part_id, current).await
    }

    pub async fn soft_delete_part<R: PartRepoTrait>(
        mut repo: R,
        snowflake: &SnowflakeIdGenerator,
        part_id: i64,
        expected_version: i32,
        current: &CurrentUser,
    ) -> Result<(), AppError> {
        current.require_role(Role::Manager)?;
        // 2026-09-16 PR-2 瘦身（migration 027）：t_part.delivery_note_id 列已删；
        // 「已挂送货单禁删」守卫移出 PartRepo::soft_delete_part UPDATE，
        // 改在 service 层用 PartBatchRepo::has_active_batch_on_delivery_note
        // 预检（批次级真相源）。
        if repo
            .part_batch_has_active_on_delivery_note(part_id)
            .await?
        {
            return Err(AppError::biz(
                code::BIZ_DELIVERY_NOTE_LOCKED_PART,
                format!("part {part_id} 存在活跃批次已挂送货单，禁 soft-delete"),
            ));
        }
        let n = repo
            .soft_delete_part(part_id, expected_version, current.id)
            .await?;
        match n {
            1 => {
                // 2026-09-16 FK 翻转（migration 026）级联：part 有工艺链时同事务
                // 软删链 + steps 并 unlink（顺序：steps → chain → unlink）。
                // unlink 必须清掉已软删 part 的 process_chain_id，让出
                // uq_t_part_process_chain 部分唯一索引槽位。
                let chain_id = repo
                    .get_by_id(part_id, true)
                    .await?
                    .and_then(|p| p.process_chain_id);
                if let Some(chain_id) = chain_id {
                    ProcessChainRepo::soft_delete_all_steps_for_chain(repo.conn_mut(), chain_id)
                        .await?;
                    ProcessChainRepo::soft_delete_chain(repo.conn_mut(), chain_id, current.id).await?;
                    ProcessChainRepo::unlink_part_from_chain(repo.conn_mut(), chain_id, current.id)
                        .await?;
                }
                repo.insert_part_event(NewPartEvent {
                    id: snowflake.next_id(),
                    part_id,
                    event_type: "SOFT_DELETED",
                    from_status: None,
                    to_status: None,
                    batch_id: None,
                    quantity: None,
                    drawing_code: None,
                    badge_code: None,
                    note: Some("manager soft-delete"),
                    created_by: Some(current.id),
                })
                .await?;
                Ok(())
            }
            _ => {
                // soft_delete SQL 0 行可能由 4 类原因触发，分支映射到不同错误码：
                // 1) part_id 不存在                  → 20101 BIZ_PART_NOT_FOUND (404)
                // 2) 已软删                          → 20101 BIZ_PART_NOT_FOUND (404, "已软删")
                // 3) version 不匹配                  → 40901 VERSION_CONFLICT (409)
                // 4) 终态 (DELIVERED/COMPLETED)       → 20119 BIZ_PART_NOT_DELETABLE (409)
                // 注：21420 BIZ_DELIVERY_NOTE_LOCKED_PART 已在上方预检拦截（service 层
                // 调用 PartBatchRepo::has_active_batch_on_delivery_note），不会进入
                // 此 match。
                let p = repo.get_by_id(part_id, true).await?;
                match p {
                    None => Err(AppError::biz(
                        code::BIZ_PART_NOT_FOUND,
                        format!("part {part_id} 不存在"),
                    )),
                    Some(p) if p.deleted_at.is_some() => Err(AppError::biz(
                        code::BIZ_PART_NOT_FOUND,
                        format!("part {part_id} 已软删"),
                    )),
                    Some(p) if p.version != expected_version => Err(AppError::biz(
                        code::VERSION_CONFLICT,
                        format!(
                            "part {part_id} 版本冲突（期望 {expected_version}，实际 {}）",
                            p.version
                        ),
                    )),
                    Some(p) if matches!(p.status.as_str(), "DELIVERED" | "COMPLETED") => {
                        Err(AppError::biz(
                            code::BIZ_PART_NOT_DELETABLE,
                            format!("part {part_id} 状态 {} 终态禁删", p.status),
                        ))
                    }
                    Some(p) => {
                        // 兜底：理论上 soft_delete SQL 已包含 `deleted_at IS NULL`
                        // 守卫，此分支不可达。映射成 50000 让上游看到错误模式。
                        Err(AppError::internal(format!(
                            "soft_delete_part 兜底：part {part_id} status={} 触发未识别条件",
                            p.status
                        )))
                    }
                }
            }
        }
    }

    /// 通用 part 文件上传（2026-09-11 重构）：
    /// DRAWING / 3D_MODEL 等 kind 共用同一段上传 + 落库逻辑。
    /// 调用方（`upload_drawing` / `upload_3d_model`）只负责决定 kind 和 file_type。
    #[allow(clippy::too_many_arguments)]
    pub async fn upload_part_file<R: PartRepoTrait>(
        mut repo: R,
        snowflake: &SnowflakeIdGenerator,
        state: &Arc<AppState>,
        part_id: i64,
        bytes: &[u8],
        original_filename: &str,
        content_type: &str,
        kind: &str,
        file_type: &str,
        current: &CurrentUser,
    ) -> Result<TPartFile, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk])?;
        if bytes.is_empty() {
            // 空字节不是「过大」而是「无效输入」：用 VALIDATION_ERROR (40001)
            // 而不是 BIZ_PART_FILE_TOO_LARGE (21103)
            return Err(AppError::validation(format!("{file_type} 字节为空")));
        }
        // 2026-09-11 修改：改用 CosConfig.max_file_size 配置（默认 300MB），
        // 不再写死 50MB。`.env` 改 COS_MAX_FILE_SIZE 即生效。
        let max = state.config.cos.max_file_size;
        if bytes.len() > max {
            return Err(AppError::biz(
                code::BIZ_PART_FILE_TOO_LARGE,
                format!("{file_type} > {max} bytes（{}MB）", max / 1024 / 1024),
            ));
        }
        // 2026-09-11 新增：kind → 扩展名 / content_type 白名单校验
        let ext = policy::ext_of(original_filename)
            .ok_or_else(|| AppError::biz(code::BIZ_PART_FILE_BAD_TYPE, "缺少扩展名"))?;
        let allowed = policy::allowed_exts(kind);
        if !allowed.contains(&ext.as_str()) {
            return Err(AppError::biz(
                code::BIZ_PART_FILE_BAD_TYPE,
                format!("扩展名 {ext} 不在 kind={kind} 白名单（{allowed:?}）"),
            ));
        }
        let ct_ok = policy::expected_content_types_for_ext(&ext);
        if !ct_ok.contains(&content_type) {
            return Err(AppError::biz(
                code::BIZ_PART_FILE_BAD_TYPE,
                format!("content_type {content_type} 与扩展名 {ext} 不一致"),
            ));
        }
        // 上传前 part 必须存在
        if repo.get_part_detail(part_id).await?.is_none() {
            return Err(AppError::biz(
                code::BIZ_PART_FILE_OWNER_NOT_FOUND,
                format!("part {part_id} 不存在"),
            ));
        }

        let sha = hash_bytes(bytes);
        let new_file_id = snowflake.next_id();
        // 2026-09-11 改为 Python 同款 CAS key：`{prefix}{kind}/{id}/{KIND}/{sha16}_{safe_name}`
        let real_key = crate::util::cos_key::build_cas_key(
            &state.config.cos.upload_prefix,
            "part",
            part_id,
            kind,
            &sha,
            original_filename,
        );
        state
            .cos
            .put_object(&real_key, bytes.to_vec(), content_type)
            .await
            .map_err(|e| {
                AppError::biz(
                    code::BIZ_PART_FILE_UPLOAD_FAILED,
                    format!("COS 上传失败: {e}"),
                )
            })?;
        PartFileRepo::create_part_file(
            repo.conn_mut(),
            NewPartFile {
                id: new_file_id,
                part_id,
                owner_kind: "PART",
                kind,
                file_type,
                object_key: &real_key,
                original_filename,
                file_size: bytes.len() as i64,
                content_type,
                upload_status: "READY",
                content_sha256: Some(&sha),
                created_by: current.id,
            },
        )
        .await
        .map_err(|e| {
            if let sqlx::Error::Database(db) = &e
                && db.code().as_deref() == Some("23505")
            {
                return AppError::biz(code::BIZ_PART_FILE_DUPLICATE, "相同文件已存在");
            }
            AppError::from(e)
        })?;
        let pf = PartFileRepo::get_by_part_kind(repo.conn_mut(), part_id, kind)
            .await?
            .ok_or_else(|| AppError::internal("刚 INSERT 的 file 查不到"))?;
        Ok(pf)
    }

    /// 上传 part 图纸 PDF（multipart 处理上传到 COS + INSERT t_part_file）。
    /// 2026-09-11 修改：改为对 `upload_part_file` 的薄包装。
    #[allow(clippy::too_many_arguments)]
    pub async fn upload_drawing<R: PartRepoTrait>(
        repo: R,
        snowflake: &SnowflakeIdGenerator,
        state: &Arc<AppState>,
        part_id: i64,
        bytes: &[u8],
        original_filename: &str,
        content_type: &str,
        current: &CurrentUser,
    ) -> Result<TPartFile, AppError> {
        Self::upload_part_file(
            repo,
            snowflake,
            state,
            part_id,
            bytes,
            original_filename,
            content_type,
            "DRAWING",
            "PDF",
            current,
        )
        .await
    }

    /// 上传 part 3D 模型（STEP / STP / IGES / IGS / STL / OBJ / 3MF）。
    /// 2026-09-11 新增：与 Python `POST /api/v1/parts/{id}/3d-models` 对齐。
    /// file_type 由扩展名推导（`policy::file_type_for_ext`）。
    #[allow(clippy::too_many_arguments)]
    pub async fn upload_3d_model<R: PartRepoTrait>(
        repo: R,
        snowflake: &SnowflakeIdGenerator,
        state: &Arc<AppState>,
        part_id: i64,
        bytes: &[u8],
        original_filename: &str,
        content_type: &str,
        current: &CurrentUser,
    ) -> Result<TPartFile, AppError> {
        let ext = policy::ext_of(original_filename)
            .ok_or_else(|| AppError::biz(code::BIZ_PART_FILE_BAD_TYPE, "缺少扩展名"))?;
        let file_type = policy::file_type_for_ext(&ext).ok_or_else(|| {
            AppError::biz(
                code::BIZ_PART_FILE_BAD_TYPE,
                format!("未知 3D 模型扩展名: {ext}"),
            )
        })?;
        Self::upload_part_file(
            repo,
            snowflake,
            state,
            part_id,
            bytes,
            original_filename,
            content_type,
            "3D_MODEL",
            file_type,
            current,
        )
        .await
    }
}

// ===== helpers =====

/// `create_part` 的 sqlx 错误码映射：唯一索引冲突（`23505`） → 业务语义
/// `BIZ_PART_NOT_FOUND`（serial_no 已被使用；可能是软删旧件占号导致
/// `uk_t_part_serial_no` 触发。当前 INSERT 路径 serial_no 写 NULL，partial
/// unique 不生效；此分支为预留，等 serial_no 变成可写时启用）。
///
/// `pub(super)`：暴露给 `lifecycle.rs`（如需要）。
pub(super) fn map_create_error(e: sqlx::Error) -> AppError {
    if let sqlx::Error::Database(db) = &e
        && db.code().as_deref() == Some("23505")
    {
        return AppError::biz(
            code::BIZ_PART_NOT_FOUND,
            "serial_no 已被使用（可能软删旧件占号）",
        );
    }
    AppError::from(e)
}

/// 展开 `customer_id` 为 `[id]`（含自身 + 子节点）。
///
/// 语义：
/// - L1 客户（无 parent_id）→ 自身 + 全部 L2 子节点 ids
/// - L2 客户（有 parent_id）→ 自身 + 同 L1 下所有兄弟 L2 ids
pub(super) async fn expand_customer_id(
    conn: &mut PgConnection,
    cid: i64,
) -> Result<Vec<i64>, AppError> {
    let row: Option<(i64, Option<i64>)> =
        sqlx::query_as("SELECT id, parent_id FROM t_customer WHERE id = $1 AND deleted_at IS NULL")
            .bind(cid)
            .fetch_optional(&mut *conn)
            .await?;
    let (_id, parent_id) = row.ok_or_else(|| {
        AppError::biz(
            code::BIZ_CUSTOMER_NOT_FOUND,
            format!("customer {cid} 不存在"),
        )
    })?;
    if let Some(p) = parent_id {
        let mut rows: Vec<i64> = sqlx::query_scalar(
            "SELECT id FROM t_customer WHERE parent_id = $1 AND deleted_at IS NULL",
        )
        .bind(p)
        .fetch_all(&mut *conn)
        .await?;
        if !rows.contains(&cid) {
            rows.push(cid);
        }
        Ok(rows)
    } else {
        sqlx::query_scalar(
            "SELECT id FROM t_customer WHERE (parent_id = $1 OR id = $1) AND deleted_at IS NULL",
        )
        .bind(cid)
        .fetch_all(&mut *conn)
        .await
        .map_err(Into::into)
    }
}

/// 取客户名 + L1 名（用于 `PartDetailOut` / `PartListItem` 冗余字段）。
///
/// 返回 `(Some(name), Some(l1_name))`：当自身为 L1 时 l1_name 与 name 同；
/// 当 customer 不存在 → `(None, None)`（service 层可容忍）。
pub(super) async fn lookup_customer_names(
    conn: &mut PgConnection,
    customer_id: i64,
) -> Result<(Option<String>, Option<String>), AppError> {
    let row: Option<(String, Option<i64>)> = sqlx::query_as(
        "SELECT name, parent_id FROM t_customer WHERE id = $1 AND deleted_at IS NULL",
    )
    .bind(customer_id)
    .fetch_optional(&mut *conn)
    .await?;
    let (name, parent_id) = match row {
        Some(r) => r,
        None => return Ok((None, None)),
    };
    let l1_name = match parent_id {
        Some(pid) => {
            sqlx::query_scalar::<_, String>(
                "SELECT name FROM t_customer WHERE id = $1 AND deleted_at IS NULL",
            )
            .bind(pid)
            .fetch_optional(&mut *conn)
            .await?
        }
        None => Some(name.clone()),
    };
    Ok((Some(name), l1_name))
}

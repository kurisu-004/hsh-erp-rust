//! outsource 域 service — 报价子模块
//!
//! 覆盖端点：
//! - list_quotes       — 列表 + 过滤 + 分页
//! - get_quote         — 详情
//! - create_quote      — 创建 DRAFT（part/company/process 必填校验 + 重复检查）
//! - update_quote      — 更新 DRAFT（OCC）
//! - submit_quote      — DRAFT → SUBMITTED
//! - approve_quote     — SUBMITTED → APPROVED（MANAGER-only；自动 reject 竞争报价）
//! - reject_quote      — SUBMITTED → REJECTED（review_note 必填；MANAGER-only）
//! - soft_delete_quote — 软删（DRAFT / REJECTED 状态才允许）
//!
//! ## 事务边界（2026-09-22 refactor 对齐 iam 范本）
//! 事务移交 handler：service 仅业务逻辑，所有跨 repo 操作经 `repo: R`
//! （by-value；`R: OutsourceRepoTrait`）参数传入——handler/service 借 `&mut *tx` /
//! `&mut *conn` 喂给 `OutsourceRepoTrait` trait（trait 已直接 `impl for &mut PgConnection`）。
//! service 不知事务——handler `pool.begin()` + `tx.commit()` 包外。
//!
//! impl 块直接挂在 `OutsourceService` 上（与 mod.rs / company.rs / shipment.rs 共同 impl）。
//!
//! ## helper 测试
//! 承接原 `service.rs::mod tests` 中 quote 子域用到的 2 个 helper 测试：
//! `parse_snowflake_id_valid` / `parse_snowflake_id_invalid`。

use std::collections::HashMap;

use crate::auth::rbac::{CurrentUser, Role};
use crate::modules::outsource::dto::{
    OutsourceQuoteCreateRequest, OutsourceQuoteListQuery, OutsourceQuoteUpdateRequest,
};
use crate::modules::outsource::vo::{OutsourceQuoteListOut, OutsourceQuoteOut};
use crate::modules::outsource::model::{NewOutsourceQuote, NewOutsourceQuoteEvent, TOutsourceQuote};
use crate::modules::outsource::repo::OutsourceRepoTrait;
use crate::modules::outsource::statemachine::OutsourceQuoteStatus;
use crate::shared::error::{AppError, code};

use super::{
    DEFAULT_LIMIT, MAX_LIMIT, OutsourceService, current_id_to_snowflake, format_price,
    not_found_company, not_found_quote, parse_price, parse_snowflake_id, version_conflict,
};

/// quote_out_many：批量拼装 `OutsourceQuoteOut`（part/company/process 名称补全）。
async fn quote_out_many<R: OutsourceRepoTrait>(
    repo: &mut R,
    quotes: Vec<TOutsourceQuote>,
) -> Result<Vec<OutsourceQuoteOut>, AppError> {
    if quotes.is_empty() {
        return Ok(Vec::new());
    }
    let part_ids: Vec<i64> = quotes.iter().map(|q| q.part_id).collect();
    let company_ids: Vec<i64> = quotes.iter().map(|q| q.outsource_company_id).collect();
    let process_ids: Vec<i64> = quotes.iter().map(|q| q.process_id).collect();

    let part_map: HashMap<
        i64,
        (Option<String>, String, String, bool, Option<String>),
    > = {
        let rows = repo.part_map_for_quote(&part_ids).await?;
        rows.into_iter()
            .map(|r| (r.0, (r.1, r.2, r.3, r.4, r.5)))
            .collect()
    };
    let company_map: HashMap<i64, String> = {
        let rows = repo.company_map_name(&company_ids).await?;
        rows.into_iter().collect()
    };
    let process_map: HashMap<i64, (String, String)> = {
        let rows = repo.process_map_short(&process_ids).await?;
        rows.into_iter().map(|r| (r.0, (r.1, r.2))).collect()
    };

    let mut out = Vec::with_capacity(quotes.len());
    for q in quotes {
        let (serial, drawing, name, urgent, unit_price) = part_map
            .get(&q.part_id)
            .cloned()
            .unwrap_or_else(|| (None, String::new(), String::new(), false, None));
        let company_name = company_map.get(&q.outsource_company_id).cloned();
        let (proc_code, proc_name) = process_map
            .get(&q.process_id)
            .cloned()
            .unwrap_or_else(|| (String::new(), String::new()));
        out.push(OutsourceQuoteOut {
            id: q.id,
            version: q.version,
            part_id: q.part_id,
            outsource_company_id: q.outsource_company_id,
            process_id: q.process_id,
            price: format_price(&q.price),
            note: q.note,
            status: q.status,
            submitted_at: q.submitted_at,
            reviewed_at: q.reviewed_at,
            review_note: q.review_note,
            created_at: q.created_at,
            updated_at: q.updated_at,
            part_serial_no: serial,
            part_drawing_no: Some(drawing),
            part_name: Some(name),
            outsource_company_name: company_name,
            process_code: Some(proc_code),
            process_name: Some(proc_name),
            customer_path: None,
            part_unit_price: unit_price,
            is_urgent: urgent,
        });
    }
    Ok(out)
}

/// quote_out：单条拼装（借 quote_out_many 实现）。
async fn quote_out<R: OutsourceRepoTrait>(
    repo: &mut R,
    q: TOutsourceQuote,
) -> Result<OutsourceQuoteOut, AppError> {
    let mut items = quote_out_many(repo, vec![q]).await?;
    Ok(items.remove(0))
}

impl OutsourceService {
    // =======================================================================
    // Quote — 列表 / 详情 / 新建 / 更新 / 状态机
    // =======================================================================

    pub async fn list_quotes<R: OutsourceRepoTrait>(
        &self,
        mut repo: R,
        query: &OutsourceQuoteListQuery,
        current: &CurrentUser,
    ) -> Result<OutsourceQuoteListOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk, Role::Inspector])?;
        let limit = query.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
        let offset = query.offset.unwrap_or(0).max(0);
        let sort_by = query.sort_by.as_deref().unwrap_or("CREATED_AT");
        let sort_dir = query.sort_dir.as_deref().unwrap_or("DESC");

        // keyword / customer_id → part_ids
        let part_ids_in: Vec<i64> = if query.keyword.is_some() || query.customer_id.is_some() {
            let kw = query
                .keyword
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty());
            let _cid = if let Some(s) = query.customer_id.as_deref().filter(|s| !s.is_empty()) {
                Some(s.parse::<i64>().map_err(|_| {
                    AppError::biz(code::BIZ_INVALID_VALUE, "customer_id 非整数")
                })?)
            } else {
                None
            };
            // 仅按 keyword（忽略 customer_id 展开以避免跨表依赖）
            if let Some(k) = kw {
                repo.part_keyword_search(k).await?
            } else {
                Vec::new()
            }
        } else {
            Vec::new()
        };
        // 若提供了 customer_id 但未提供 keyword：返回空（简化实现，复杂展开留给 service 扩展）
        if (query.customer_id.is_some()) && query.keyword.is_none() {
            return Ok(OutsourceQuoteListOut {
                items: vec![],
                total: 0,
                limit,
                offset,
            });
        }
        let part_id: Option<i64> = if let Some(s) = query.part_id.as_deref().filter(|s| !s.is_empty())
        {
            Some(
                s.parse::<i64>()
                    .map_err(|_| AppError::biz(code::BIZ_INVALID_VALUE, "part_id 非整数"))?,
            )
        } else {
            None
        };
        let company_id: Option<i64> = if let Some(s) = query
            .outsource_company_id
            .as_deref()
            .filter(|s| !s.is_empty())
        {
            Some(s.parse::<i64>().map_err(|_| {
                AppError::biz(code::BIZ_INVALID_VALUE, "outsource_company_id 非整数")
            })?)
        } else {
            None
        };
        let rows = repo
            .quote_list_with_filters(
                query.status.as_deref(),
                &[],
                part_id,
                &part_ids_in,
                company_id,
                sort_by,
                sort_dir,
                limit,
                offset,
            )
            .await?;
        let total = repo
            .quote_count_with_filters(
                query.status.as_deref(),
                &[],
                part_id,
                &part_ids_in,
                company_id,
            )
            .await?;
        let items = quote_out_many(&mut repo, rows).await?;
        Ok(OutsourceQuoteListOut {
            items,
            total,
            limit,
            offset,
        })
    }

    pub async fn get_quote<R: OutsourceRepoTrait>(
        &self,
        mut repo: R,
        id: i64,
        current: &CurrentUser,
    ) -> Result<OutsourceQuoteOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk, Role::Inspector])?;
        let q = repo
            .quote_get_by_id(id, false)
            .await?
            .ok_or_else(|| not_found_quote(id))?;
        quote_out(&mut repo, q).await
    }

    pub async fn create_quote<R: OutsourceRepoTrait>(
        &self,
        mut repo: R,
        req: &OutsourceQuoteCreateRequest,
        current: &CurrentUser,
    ) -> Result<OutsourceQuoteOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk])?;
        let part_id = parse_snowflake_id(&req.part_id, "part_id")?;
        let company_id = parse_snowflake_id(&req.outsource_company_id, "outsource_company_id")?;
        let process_id = parse_snowflake_id(&req.process_id, "process_id")?;
        let price = parse_price(&req.price)?;

        // part 存在
        if !repo.part_exists(part_id).await? {
            return Err(AppError::biz(
                code::BIZ_PART_NOT_FOUND,
                format!("part {part_id} 不存在"),
            ));
        }
        // 公司存在 + active
        let _company = repo
            .company_get_by_id(company_id, false)
            .await?
            .ok_or_else(|| not_found_company(company_id))?;
        // 工序存在 + OUTSOURCE 类别
        let proc_category = repo.process_get_category(process_id).await?.ok_or_else(|| {
            AppError::biz(
                code::BIZ_PROCESS_NOT_FOUND,
                format!("process {process_id} 不存在"),
            )
        })?;
        if proc_category != "OUTSOURCE" {
            return Err(AppError::biz(
                code::BIZ_OUTSOURCE_COMPANY_BAD_PROCESS,
                format!("工序 {process_id} 不是 OUTSOURCE 类别"),
            ));
        }
        // 重复检查（应用层预校验 + DB 部分唯一索引双重兜底）
        if let Some(existing) = repo
            .quote_get_active_for_tuple(part_id, company_id, process_id)
            .await?
        {
            return Err(AppError::biz(
                code::BIZ_OUTSOURCE_QUOTE_DUPLICATE,
                format!(
                    "同一 (part={part_id} / company={company_id} / process={process_id}) 已存在活跃报价 #{}",
                    existing.id
                ),
            ));
        }
        // 创建 DRAFT
        let id = self.snowflake.next_id();
        let new = NewOutsourceQuote {
            id,
            part_id,
            outsource_company_id: company_id,
            process_id,
            price,
            note: req
                .note
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string),
            created_by: current.id,
        };
        let q = repo.quote_create(new).await.map_err(|e| {
            // uq_t_outsource_quote_approved_part_process 兜底
            if e.as_database_error().and_then(|d| d.code()).as_deref() == Some("23505") {
                AppError::biz(
                    code::BIZ_OUTSOURCE_QUOTE_DUPLICATE,
                    "同一 (零件 / 外协公司 / 工序) 已存在活跃报价",
                )
            } else {
                AppError::from(e)
            }
        })?;
        // CREATED 事件
        repo.quote_event_create(NewOutsourceQuoteEvent {
            id: current_id_to_snowflake(&self.snowflake),
            quote_id: q.id,
            event_type: "CREATED".to_string(),
            from_status: None,
            to_status: Some(q.status.clone()),
            note: None,
            created_by: current.id,
        })
        .await?;
        quote_out(&mut repo, q).await
    }

    pub async fn update_quote<R: OutsourceRepoTrait>(
        &self,
        mut repo: R,
        id: i64,
        req: &OutsourceQuoteUpdateRequest,
        current: &CurrentUser,
    ) -> Result<OutsourceQuoteOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk])?;
        let q = repo
            .quote_get_by_id(id, false)
            .await?
            .ok_or_else(|| not_found_quote(id))?;
        if q.status != "DRAFT" {
            return Err(AppError::biz(
                code::BIZ_OUTSOURCE_QUOTE_INVALID_TRANSITION,
                format!("只有 DRAFT 状态的报价可以修改，当前 {}", q.status),
            ));
        }
        if q.version != req.version {
            return Err(AppError::biz(
                code::VERSION_CONFLICT,
                format!("报价版本不一致：当前 {}，请求 {}", q.version, req.version),
            ));
        }
        let price = req.price.as_deref().map(parse_price).transpose()?;
        let note = req
            .note
            .as_ref()
            .map(|inner| inner.as_deref().map(|s| s.trim()).filter(|s| !s.is_empty()));
        let n = repo
            .quote_update(id, req.version, price, note, current.id)
            .await?;
        if n == 0 {
            return Err(version_conflict());
        }
        // EDITED 事件
        repo.quote_event_create(NewOutsourceQuoteEvent {
            id: current_id_to_snowflake(&self.snowflake),
            quote_id: id,
            event_type: "EDITED".to_string(),
            from_status: Some(q.status.clone()),
            to_status: Some(q.status.clone()),
            note: None,
            created_by: current.id,
        })
        .await?;
        let fresh = repo
            .quote_get_by_id(id, false)
            .await?
            .ok_or_else(|| not_found_quote(id))?;
        quote_out(&mut repo, fresh).await
    }

    pub async fn submit_quote<R: OutsourceRepoTrait>(
        &self,
        mut repo: R,
        id: i64,
        current: &CurrentUser,
    ) -> Result<OutsourceQuoteOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk])?;
        let q = repo
            .quote_get_by_id(id, false)
            .await?
            .ok_or_else(|| not_found_quote(id))?;
        if q.status != "DRAFT" {
            return Err(AppError::biz(
                code::BIZ_OUTSOURCE_QUOTE_INVALID_TRANSITION,
                format!("当前状态 {} 不允许 submit", q.status),
            ));
        }
        let from = OutsourceQuoteStatus::DRAFT;
        let to = OutsourceQuoteStatus::SUBMITTED;
        if !from.can_transition_to(to) {
            return Err(AppError::biz(
                code::BIZ_OUTSOURCE_QUOTE_INVALID_TRANSITION,
                format!("{:?} → {:?} 不允许", from, to),
            ));
        }
        let n = repo.quote_submit(id, q.version, current.id).await?;
        if n == 0 {
            return Err(version_conflict());
        }
        repo.quote_event_create(NewOutsourceQuoteEvent {
            id: current_id_to_snowflake(&self.snowflake),
            quote_id: id,
            event_type: "SUBMITTED".to_string(),
            from_status: Some(from.as_str().to_string()),
            to_status: Some(to.as_str().to_string()),
            note: None,
            created_by: current.id,
        })
        .await?;
        let fresh = repo
            .quote_get_by_id(id, false)
            .await?
            .ok_or_else(|| not_found_quote(id))?;
        quote_out(&mut repo, fresh).await
    }

    pub async fn approve_quote<R: OutsourceRepoTrait>(
        &self,
        mut repo: R,
        id: i64,
        review_note: Option<&str>,
        version: i32,
        current: &CurrentUser,
    ) -> Result<OutsourceQuoteOut, AppError> {
        // MANAGER-only
        current.require_role(Role::Manager)?;
        let q = repo
            .quote_get_by_id(id, false)
            .await?
            .ok_or_else(|| not_found_quote(id))?;
        if q.status != "SUBMITTED" {
            return Err(AppError::biz(
                code::BIZ_OUTSOURCE_QUOTE_INVALID_TRANSITION,
                format!("当前状态 {} 不允许 approve", q.status),
            ));
        }
        if q.version != version {
            return Err(AppError::biz(
                code::VERSION_CONFLICT,
                "报价版本不一致，请刷新后重试",
            ));
        }
        // 自动拒绝同 (part_id, process_id) 的其他 SUBMITTED/APPROVED
        let _ = repo
            .quote_reject_competitors(
                q.part_id,
                q.process_id,
                id,
                "被新批准报价取代",
                current.id,
            )
            .await?;
        let from = OutsourceQuoteStatus::SUBMITTED;
        let to = OutsourceQuoteStatus::APPROVED;
        if !from.can_transition_to(to) {
            return Err(AppError::biz(
                code::BIZ_OUTSOURCE_QUOTE_INVALID_TRANSITION,
                format!("{:?} → {:?} 不允许", from, to),
            ));
        }
        let n = repo
            .quote_approve(id, version, review_note, current.id)
            .await?;
        if n == 0 {
            return Err(version_conflict());
        }
        repo.quote_event_create(NewOutsourceQuoteEvent {
            id: current_id_to_snowflake(&self.snowflake),
            quote_id: id,
            event_type: "APPROVED".to_string(),
            from_status: Some(from.as_str().to_string()),
            to_status: Some(to.as_str().to_string()),
            note: review_note.map(|s| s.to_string()),
            created_by: current.id,
        })
        .await?;
        let fresh = repo
            .quote_get_by_id(id, false)
            .await?
            .ok_or_else(|| not_found_quote(id))?;
        quote_out(&mut repo, fresh).await
    }

    pub async fn reject_quote<R: OutsourceRepoTrait>(
        &self,
        mut repo: R,
        id: i64,
        review_note: &str,
        version: i32,
        current: &CurrentUser,
    ) -> Result<OutsourceQuoteOut, AppError> {
        current.require_role(Role::Manager)?;
        if review_note.trim().is_empty() {
            return Err(AppError::biz(code::BIZ_INVALID_VALUE, "review_note 必填"));
        }
        let q = repo
            .quote_get_by_id(id, false)
            .await?
            .ok_or_else(|| not_found_quote(id))?;
        if q.status != "SUBMITTED" {
            return Err(AppError::biz(
                code::BIZ_OUTSOURCE_QUOTE_INVALID_TRANSITION,
                format!("当前状态 {} 不允许 reject", q.status),
            ));
        }
        if q.version != version {
            return Err(AppError::biz(
                code::VERSION_CONFLICT,
                "报价版本不一致，请刷新后重试",
            ));
        }
        let from = OutsourceQuoteStatus::SUBMITTED;
        let to = OutsourceQuoteStatus::REJECTED;
        if !from.can_transition_to(to) {
            return Err(AppError::biz(
                code::BIZ_OUTSOURCE_QUOTE_INVALID_TRANSITION,
                format!("{:?} → {:?} 不允许", from, to),
            ));
        }
        let n = repo
            .quote_reject(id, version, review_note, current.id)
            .await?;
        if n == 0 {
            return Err(version_conflict());
        }
        repo.quote_event_create(NewOutsourceQuoteEvent {
            id: current_id_to_snowflake(&self.snowflake),
            quote_id: id,
            event_type: "REJECTED".to_string(),
            from_status: Some(from.as_str().to_string()),
            to_status: Some(to.as_str().to_string()),
            note: Some(review_note.to_string()),
            created_by: current.id,
        })
        .await?;
        let fresh = repo
            .quote_get_by_id(id, false)
            .await?
            .ok_or_else(|| not_found_quote(id))?;
        quote_out(&mut repo, fresh).await
    }

    pub async fn soft_delete_quote<R: OutsourceRepoTrait>(
        &self,
        mut repo: R,
        id: i64,
        current: &CurrentUser,
    ) -> Result<(), AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk])?;
        let q = repo
            .quote_get_by_id(id, false)
            .await?
            .ok_or_else(|| not_found_quote(id))?;
        if !matches!(q.status.as_str(), "DRAFT" | "REJECTED") {
            return Err(AppError::biz(
                code::BIZ_OUTSOURCE_QUOTE_INVALID_TRANSITION,
                format!("已提交 / 已批准 / 已使用的报价不可删除；当前 {}", q.status),
            ));
        }
        let n = repo.quote_soft_delete(id, q.version, current.id).await?;
        if n == 0 {
            return Err(version_conflict());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    //! 2026-09-22 refactor：原 `service.rs::mod tests` 5 helper 测试拆分；
    //! 本文件承接 quote 子域用到的 2 个：`parse_snowflake_id_valid` /
    //! `parse_snowflake_id_invalid`。
    use super::super::parse_snowflake_id;

    #[test]
    fn parse_snowflake_id_valid() {
        assert_eq!(parse_snowflake_id("123", "x").unwrap(), 123);
        assert_eq!(parse_snowflake_id("  456  ", "x").unwrap(), 456);
    }

    #[test]
    fn parse_snowflake_id_invalid() {
        assert!(parse_snowflake_id("abc", "x").is_err());
        assert!(parse_snowflake_id("", "x").is_err());
    }
}

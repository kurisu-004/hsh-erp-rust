//! outsource 域 service — 报价子模块
//!
//! 覆盖端点：
//! - list_quotes       — 列表 + 过滤 + 分页
//! - create_quote      — 创建 DRAFT（part/company/process 必填校验 + 重复检查）
//! - submit_quote      — DRAFT → SUBMITTED（OCC）
//! - approve_quote     — SUBMITTED → APPROVED（MANAGER-only；自动 reject 竞争报价）
//! - reject_quote      — SUBMITTED → REJECTED（review_note 必填；MANAGER-only）
//! - soft_delete_quote — 软删（DRAFT / REJECTED 状态才允许；OCC）
//! - list_quotable_parts   — 报价 picker（还没下发的零件，
//!   一零件一行；同日由「零件 × OUTSOURCE 工序」简化而来）。此前路由未注册，
//!   被 `quote_router` 的 `/{id}`（`Path<i64>`）吞掉 → `PathRejection` → 恒 400。
//!
//! 2026-10-09：`get_quote` / `update_quote` 两个函数随
//! `GET /{id}` / `POST /{id}/update` 端点硬切下线而删除（前端零消费），相应的
//! `OutsourceQuoteUpdateRequest` 与 repo 的 `quote_update` 随之删除。
//!
//! ## 事务边界（2026-09-22 refactor 对齐 iam 范式）
//! 事务移交 handler：service 仅业务逻辑，所有跨 repo 操作经 `repo: R`
//! （by-value；`R: OutsourceRepoTrait`）参数传入——handler/service 借 `&mut *tx` /
//! `&mut *conn` 喂给 `OutsourceRepoTrait` trait（trait 已直接 `impl for &mut PgConnection`）。
//! service 不知事务——handler `pool.begin()` + `tx.commit()` 包外。
//!
//! impl 块直接挂在 `OutsourceService` 上（与 mod.rs / company.rs / shipment.rs /
//! sendable.rs 共同 impl）。
//!
//! ## helper 测试
//! 承接原 `service.rs::mod tests` 中 quote 子域用到的 2 个 helper 测试：
//! `parse_snowflake_id_valid` / `parse_snowflake_id_invalid`。

use std::collections::HashMap;

use crate::auth::rbac::{CurrentUser, Role};
use crate::modules::outsource::dto::{
    OutsourceQuotablePartListQuery, OutsourceQuoteCreateRequest, OutsourceQuoteListQuery,
    OutsourceQuoteSoftDeleteRequest, OutsourceQuoteSubmitRequest,
};
use crate::modules::outsource::model::{
    NewOutsourceQuote, NewOutsourceQuoteEvent, TOutsourceQuote,
};
use crate::modules::outsource::repo::OutsourceRepoTrait;
use crate::modules::outsource::statemachine::OutsourceQuoteStatus;
use crate::modules::outsource::vo::{
    OutsourceQuoteListOut, OutsourceQuoteOut, QuotablePartListOut, QuotablePartOut,
};
use crate::shared::error::{AppError, code};

use super::{
    DEFAULT_LIMIT, LIST_MAX_LIMIT, MAX_LIMIT, OutsourceService, current_id_to_snowflake,
    format_price, join_customer_path, keyword_pattern, not_found_company, not_found_quote,
    parse_price, parse_snowflake_id, version_conflict,
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
        (
            Option<String>,
            String,
            String,
            bool,
            Option<String>,
            Option<String>,
            Option<String>,
        ),
    > = {
        let rows = repo.part_map_for_quote(&part_ids).await?;
        rows.into_iter()
            .map(|r| (r.0, (r.1, r.2, r.3, r.4, r.5, r.6, r.7)))
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
        let (serial, drawing, name, urgent, unit_price, cust_l2, cust_l1) = part_map
            .get(&q.part_id)
            .cloned()
            .unwrap_or_else(|| (None, String::new(), String::new(), false, None, None, None));
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
            // 2026-10-03 新增：此前硬编码 `None` → 前端报价一览「客户」列恒 `—`
            customer_path: join_customer_path(cust_l1.as_deref(), cust_l2.as_deref()),
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

        // `drawing_no` / `name` 直连 ILIKE（2026-10-09 取代 `keyword`）：归一化成
        // `%kw%` 后 bind 进 SQL，**零中间查询、零截断风险**，因此也不需要「给了
        // 关键词却零命中要早返回」的守卫 —— 零命中在 SQL 里自然就是零行。
        let drawing_no_pat = keyword_pattern(query.drawing_no.as_deref());
        let name_pat = keyword_pattern(query.name.as_deref());

        // `customer_id` → part_id 集合（唯一还需要「展开成中间集合」的维度：
        // 客户子树无法写成对 `t_outsource_quote` 的单表谓词）。
        //
        // 2026-10-09：`keyword` 拆成直连 ILIKE 后，两个维度**不再在 service 求交**
        // —— 各自落成 SQL 的一个 WHERE 段，由 DB 求交。旧的 HashSet 求交 + 零命中
        // 早返回分支一并删除。
        let cid = if let Some(s) = query.customer_id.as_deref().filter(|s| !s.is_empty()) {
            Some(
                s.parse::<i64>()
                    .map_err(|_| AppError::biz(code::BIZ_INVALID_VALUE, "customer_id 非整数"))?,
            )
        } else {
            None
        };
        let part_ids_in: Vec<i64> = match cid {
            Some(cid) => repo.part_ids_by_customer(cid).await?,
            None => Vec::new(),
        };
        // 客户子树零命中必须早返回：SQL 谓词
        // `AND (cardinality($4::bigint[]) = 0 OR q.part_id = ANY($4))` 里，空数组让
        // `cardinality = 0` 成立、整个客户条件被短路掉；不在这兜住，「选了一个零件
        // 都没有的客户」会返回**全量**报价（list 与 count 同时错）。
        // 「零件侧筛选」不需要对应守卫 —— 它们的谓词形如 `($6::text IS NULL OR …)`，
        // NULL 时短路、给值时正常求值，零命中就是零行。
        let cid_given = cid.is_some();
        if cid_given && part_ids_in.is_empty() {
            return Ok(OutsourceQuoteListOut {
                items: vec![],
                total: 0,
                limit,
                offset,
            });
        }
        let part_id: Option<i64> =
            if let Some(s) = query.part_id.as_deref().filter(|s| !s.is_empty()) {
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
        // `statuses` 是逗号分隔单值（见 `OutsourceQuoteListQuery::statuses` 的注释：
        // `serde_urlencoded` 填不出 `Vec<String>`，故 DTO 收 String、这里展开）。
        // 展开成空 Vec ⇔ 不过滤（SQL 侧 `cardinality($2::text[]) = 0` 的语义）。
        let statuses: Vec<String> = query
            .statuses
            .as_deref()
            .map(|raw| {
                raw.split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        let rows = repo
            .quote_list_with_filters(
                query.status.as_deref(),
                &statuses,
                part_id,
                &part_ids_in,
                company_id,
                drawing_no_pat.as_deref(),
                name_pat.as_deref(),
                query.is_urgent,
                sort_by,
                sort_dir,
                limit,
                offset,
            )
            .await?;
        let total = repo
            .quote_count_with_filters(
                query.status.as_deref(),
                &statuses,
                part_id,
                &part_ids_in,
                company_id,
                drawing_no_pat.as_deref(),
                name_pat.as_deref(),
                query.is_urgent,
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

    /// `GET /outsource-quotes/quotable-parts`（2026-10-03 新增）
    ///
    /// 返回「可以给它建外协报价」的零件，**一行 = 一个零件**（该零件存在 PENDING
    /// 批次）。筛选与去重的口径全部落在 repo SQL（`OutsourceQuotableRepo`），service
    /// 只做参数归一化 + VO 组装。报价工序由用户在建报价时从 `category='OUTSOURCE'`
    /// 的工序列表里选，本端点不预置。
    pub async fn list_quotable_parts<R: OutsourceRepoTrait>(
        &self,
        mut repo: R,
        query: &OutsourceQuotablePartListQuery,
        current: &CurrentUser,
    ) -> Result<QuotablePartListOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk])?;
        let limit = query.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
        let offset = query.offset.unwrap_or(0).max(0);
        let pat = keyword_pattern(query.keyword.as_deref());

        let rows = repo.quotable_list(pat.as_deref(), limit, offset).await?;
        let total = repo.quotable_count(pat.as_deref()).await?;

        let items = rows
            .into_iter()
            .map(|r| QuotablePartOut {
                id: r.id,
                serial_no: r.serial_no,
                drawing_no: r.drawing_no,
                name: r.name,
                is_urgent: r.is_urgent,
                unit_price: r.unit_price,
                customer_id: r.customer_id,
                customer_name: r.customer_name.clone(),
                l1_customer_name: r.parent_customer_name.clone(),
                customer_path: join_customer_path(
                    r.parent_customer_name.as_deref(),
                    r.customer_name.as_deref(),
                ),
            })
            .collect();

        Ok(QuotablePartListOut {
            items,
            total,
            limit,
            offset,
        })
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
        let proc_category = repo
            .process_get_category(process_id)
            .await?
            .ok_or_else(|| {
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

    pub async fn submit_quote<R: OutsourceRepoTrait>(
        &self,
        mut repo: R,
        id: i64,
        req: &OutsourceQuoteSubmitRequest,
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
        // ⚠️ 必须用**调用方传的** version，而不是上面刚读到的 `q.version`：后者等于
        // 「用服务端自己读到的值守自己的乐观锁」，`UPDATE … WHERE version = <刚读的>`
        // 在同一行上恒成立，`n == 0` 的分支永不可达，守卫形同虚设。
        if q.version != req.version {
            return Err(AppError::biz(
                code::VERSION_CONFLICT,
                "报价版本不一致，请刷新后重试",
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
        let n = repo.quote_submit(id, req.version, current.id).await?;
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
            .quote_reject_competitors(q.part_id, q.process_id, id, "被新批准报价取代", current.id)
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
        req: &OutsourceQuoteSoftDeleteRequest,
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
        // 同 `submit_quote`：守**调用方传的** version，不能用刚读到的 `q.version`
        // 自守（那样 UPDATE 恒命中，`n == 0` 分支不可达）。
        if q.version != req.version {
            return Err(AppError::biz(
                code::VERSION_CONFLICT,
                "报价版本不一致，请刷新后重试",
            ));
        }
        let n = repo.quote_soft_delete(id, req.version, current.id).await?;
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

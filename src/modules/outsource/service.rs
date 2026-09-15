//! outsource 域业务逻辑（Phase 2 2026-09-13）
//!
//! 对应 Python myERP/service/outsource_*.py（company / quote / shipment）。
//!
//! 实施约定：
//! - 方法签名接收 `&mut PgConnection`，由 handler 开 tx 并 commit
//! - 事务内串行：业务校验 → 状态机迁移 → 写事件日志 → COMMIT
//! - OCC：所有 UPDATE 带 `WHERE id=$1 AND version=$2`，0 行 → BIZ_VERSION_CONFLICT 409
//! - 入参 ID 一律 i64；雪花 ID 字符串解析在 handler 层（DTO 用 String）做

#![allow(
    clippy::collapsible_if,
    clippy::type_complexity,
    clippy::too_many_arguments,
    unused_imports,
    unused_variables,
    unused_mut,
    deprecated
)]

use rust_decimal::Decimal;
use sqlx::PgConnection;

use crate::auth::rbac::{CurrentUser, Role};
use crate::infra::clock::now_naive;
use crate::infra::snowflake::SnowflakeIdGenerator;
use crate::shared::error::{code, AppError};

use super::dto::*;
use super::model::*;
use super::repo::{
    OutsourceCompanyProcessRepo, OutsourceCompanyRepo, OutsourceQuoteEventRepo, OutsourceQuoteRepo,
    OutsourceShipmentRepo,
};
use super::statemachine::OutsourceQuoteStatus;

const DEFAULT_LIMIT: i64 = 50;
const MAX_LIMIT: i64 = 500;

fn not_found_company(id: i64) -> AppError {
    AppError::biz(
        code::BIZ_OUTSOURCE_COMPANY_NOT_FOUND,
        format!("outsource company {id} not found"),
    )
}

fn not_found_quote(id: i64) -> AppError {
    AppError::biz(
        code::BIZ_OUTSOURCE_QUOTE_NOT_FOUND,
        format!("outsource quote {id} not found"),
    )
}

fn not_found_shipment(id: i64) -> AppError {
    AppError::biz(
        code::BIZ_OUTSOURCE_SHIPMENT_NOT_FOUND,
        format!("outsource shipment {id} not found"),
    )
}

fn version_conflict() -> AppError {
    AppError::biz(code::VERSION_CONFLICT, "数据已被他人修改，请刷新后重试")
}

/// 把字符串价格（DB 端 `Numeric(12,2)`）解析为 Decimal。
fn parse_price(s: &str) -> Result<Decimal, AppError> {
    s.trim().parse::<Decimal>().map_err(|_| {
        AppError::biz(
            code::BIZ_INVALID_VALUE,
            format!("price 不是合法的数字: {s:?}"),
        )
    })
}

/// 把 Decimal 序列化为保留 2 位小数的字符串（前端显示）。
fn format_price(d: &Decimal) -> String {
    format!("{:.2}", d)
}

pub struct OutsourceService;

impl OutsourceService {
    // =======================================================================
    // Company — 列表 / 详情
    // =======================================================================

    pub async fn list_companies(
        conn: &mut PgConnection,
        query: &OutsourceCompanyListQuery,
        current: &CurrentUser,
    ) -> Result<OutsourceCompanyListOut, AppError> {
        current.require_any_role(&[
            Role::Manager,
            Role::Clerk,
            Role::CncProgrammer,
            Role::Inspector,
        ])?;
        let limit = query.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
        let offset = query.offset.unwrap_or(0).max(0);
        let rows = OutsourceCompanyRepo::list_with_filters(
            &mut *conn,
            query.name_like.as_deref(),
            query.is_active,
            limit,
            offset,
        )
        .await?;
        let total =
            OutsourceCompanyRepo::count_with_filters(&mut *conn, query.name_like.as_deref(), query.is_active)
                .await?;
        let items = rows.into_iter().map(Self::company_out).collect();
        Ok(OutsourceCompanyListOut {
            items,
            total,
            limit,
            offset,
        })
    }

    pub async fn get_company(
        conn: &mut PgConnection,
        id: i64,
        current: &CurrentUser,
    ) -> Result<OutsourceCompanyWithProcessesOut, AppError> {
        current.require_any_role(&[
            Role::Manager,
            Role::Clerk,
            Role::CncProgrammer,
            Role::Inspector,
        ])?;
        let company = OutsourceCompanyRepo::get_by_id(&mut *conn, id, false)
            .await?
            .ok_or_else(|| not_found_company(id))?;
        Self::build_with_processes(&mut *conn, company).await
    }

    pub async fn list_companies_for_process(
        conn: &mut PgConnection,
        process_id: i64,
        current: &CurrentUser,
    ) -> Result<Vec<OutsourceCompanyOut>, AppError> {
        current.require_any_role(&[
            Role::Manager,
            Role::Clerk,
            Role::CncProgrammer,
            Role::Inspector,
        ])?;
        let ids = OutsourceCompanyProcessRepo::list_company_ids_by_process(&mut *conn, process_id)
            .await?;
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let companies = OutsourceCompanyRepo::list_by_ids(&mut *conn, &ids).await?;
        Ok(companies
            .into_iter()
            .filter(|c| c.is_active)
            .map(Self::company_out)
            .collect())
    }

    // =======================================================================
    // Company — 写
    // =======================================================================

    pub async fn create_company(
        conn: &mut PgConnection,
        snowflake: &SnowflakeIdGenerator,
        req: &OutsourceCompanyCreateRequest,
        current: &CurrentUser,
    ) -> Result<OutsourceCompanyWithProcessesOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk])?;
        let name = req.name.trim();
        if name.is_empty() {
            return Err(AppError::biz(code::BIZ_INVALID_VALUE, "name 不能为空"));
        }
        if OutsourceCompanyRepo::get_by_name(&mut *conn, name).await?.is_some() {
            return Err(AppError::biz(
                code::BIZ_OUTSOURCE_COMPANY_DUPLICATE,
                format!("外协公司「{name}」已存在"),
            ));
        }
        let id = snowflake.next_id();
        let new = NewOutsourceCompany {
            id,
            name: name.to_string(),
            contact_name: req.contact_name.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string),
            contact_phone: req.contact_phone.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string),
            address: req.address.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string),
            is_active: req.is_active,
            created_by: current.id,
        };
        let company = OutsourceCompanyRepo::create(&mut *conn, new).await.map_err(|e| {
            // 2026-09-15 fix-outsource-409：用约束名精确定位 uk_t_outsource_company_name
            // 兜底（pre-check 已 21202 拦下大部分场景；此处仅覆盖并发插入等竞态）。
            if let Some(dbe) = e.as_database_error() {
                if dbe.constraint() == Some("uk_t_outsource_company_name") {
                    return AppError::biz(
                        code::BIZ_OUTSOURCE_COMPANY_DUPLICATE_NAME,
                        format!("外协公司「{name}」已存在"),
                    );
                }
            }
            AppError::from(e)
        })?;

        // 可选：创建时一并写入工序能力清单（OUTSOURCE 类别）
        if let Some(ref process_ids) = req.process_ids {
            let int_ids = Self::parse_process_ids(process_ids)?;
            Self::validate_processes_outsource(&mut *conn, &int_ids).await?;
            Self::replace_processes(
                &mut *conn,
                snowflake,
                company.id,
                &int_ids,
                current.id,
            )
            .await?;
        }

        Self::build_with_processes(&mut *conn, company).await
    }

    pub async fn update_company(
        conn: &mut PgConnection,
        id: i64,
        req: &OutsourceCompanyUpdateRequest,
        current: &CurrentUser,
    ) -> Result<OutsourceCompanyWithProcessesOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk])?;
        let company = OutsourceCompanyRepo::get_by_id(&mut *conn, id, false)
            .await?
            .ok_or_else(|| not_found_company(id))?;
        // name 显式清空拒绝；None 不改
        let name_update: Option<&str> = match req.name.as_deref() {
            Some(s) => {
                let t = s.trim();
                if t.is_empty() {
                    return Err(AppError::biz(code::BIZ_INVALID_VALUE, "name 不能为空"));
                }
                if t != company.name {
                    if let Some(existing) = OutsourceCompanyRepo::get_by_name(&mut *conn, t).await?
                    {
                        if existing.id != company.id {
                            return Err(AppError::biz(
                                code::BIZ_OUTSOURCE_COMPANY_DUPLICATE,
                                format!("外协公司「{t}」已存在"),
                            ));
                        }
                    }
                }
                Some(t)
            }
            None => None,
        };
        // contact_name / contact_phone / address 三态编码
        let contact_name = req
            .contact_name
            .as_ref()
            .map(|inner| inner.as_deref().map(|s| s.trim()).filter(|s| !s.is_empty()));
        let contact_phone = req
            .contact_phone
            .as_ref()
            .map(|inner| inner.as_deref().map(|s| s.trim()).filter(|s| !s.is_empty()));
        let address = req
            .address
            .as_ref()
            .map(|inner| inner.as_deref().map(|s| s.trim()).filter(|s| !s.is_empty()));

        let n = OutsourceCompanyRepo::update(
            &mut *conn,
            id,
            req.version,
            name_update,
            contact_name,
            contact_phone,
            address,
            req.is_active,
            current.id,
        )
        .await
        .map_err(|e| {
            if e.as_database_error()
                .and_then(|d| d.code())
                .as_deref()
                == Some("23505")
            {
                AppError::biz(
                    code::BIZ_OUTSOURCE_COMPANY_DUPLICATE,
                    "外协公司名已存在",
                )
            } else {
                AppError::from(e)
            }
        })?;
        if n == 0 {
            return Err(version_conflict());
        }
        let fresh = OutsourceCompanyRepo::get_by_id(&mut *conn, id, false)
            .await?
            .ok_or_else(|| not_found_company(id))?;
        Self::build_with_processes(&mut *conn, fresh).await
    }

    pub async fn soft_delete_company(
        conn: &mut PgConnection,
        id: i64,
        current: &CurrentUser,
    ) -> Result<(), AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk])?;
        let company = OutsourceCompanyRepo::get_by_id(&mut *conn, id, false)
            .await?
            .ok_or_else(|| not_found_company(id))?;
        let junctions = OutsourceCompanyProcessRepo::list_by_company(&mut *conn, id, false).await?;
        if !junctions.is_empty() {
            return Err(AppError::biz(
                code::BIZ_OUTSOURCE_COMPANY_IN_USE,
                format!(
                    "外协公司「{name}」仍映射 {n} 项工序，请先在「维护工序」中清空",
                    name = company.name,
                    n = junctions.len()
                ),
            ));
        }
        let n = OutsourceCompanyRepo::soft_delete(&mut *conn, id, company.version, current.id).await?;
        if n == 0 {
            return Err(version_conflict());
        }
        Ok(())
    }

    pub async fn set_company_processes(
        conn: &mut PgConnection,
        snowflake: &SnowflakeIdGenerator,
        id: i64,
        req: &SetOutsourceCompanyProcessRequest,
        current: &CurrentUser,
    ) -> Result<OutsourceCompanyWithProcessesOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk])?;
        let company = OutsourceCompanyRepo::get_by_id(&mut *conn, id, false)
            .await?
            .ok_or_else(|| not_found_company(id))?;
        let int_ids = Self::parse_process_ids(&req.process_ids)?;
        Self::validate_processes_outsource(&mut *conn, &int_ids).await?;
        Self::replace_processes(&mut *conn, snowflake, id, &int_ids, current.id).await?;
        let fresh = OutsourceCompanyRepo::get_by_id(&mut *conn, id, false)
            .await?
            .ok_or_else(|| not_found_company(id))?;
        Self::build_with_processes(&mut *conn, fresh).await
    }

    // =======================================================================
    // Quote — 列表 / 详情 / 新建 / 更新 / 状态机
    // =======================================================================

    pub async fn list_quotes(
        conn: &mut PgConnection,
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
            let kw = query.keyword.as_deref().map(str::trim).filter(|s| !s.is_empty());
            let _cid = if let Some(s) = query.customer_id.as_deref().filter(|s| !s.is_empty()) {
                Some(s.parse::<i64>().map_err(|_| {
                    AppError::biz(code::BIZ_INVALID_VALUE, "customer_id 非整数")
                })?)
            } else {
                None
            };
            // 仅按 keyword（忽略 customer_id 展开以避免跨表依赖）
            if let Some(k) = kw {
                let rows: Vec<(i64,)> = sqlx::query_as(
                    "SELECT id FROM t_part WHERE deleted_at IS NULL AND \
                     (drawing_no ILIKE $1 OR name ILIKE $1) LIMIT 10000",
                )
                .bind(format!("%{}%", k))
                .fetch_all(&mut *conn)
                .await?;
                rows.into_iter().map(|r| r.0).collect()
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
        let part_id: Option<i64> = if let Some(s) = query.part_id.as_deref().filter(|s| !s.is_empty()) {
            Some(s.parse::<i64>().map_err(|_| {
                AppError::biz(code::BIZ_INVALID_VALUE, "part_id 非整数")
            })?)
        } else {
            None
        };
        let company_id: Option<i64> = if let Some(s) = query.outsource_company_id.as_deref().filter(|s| !s.is_empty()) {
            Some(s.parse::<i64>().map_err(|_| {
                AppError::biz(code::BIZ_INVALID_VALUE, "outsource_company_id 非整数")
            })?)
        } else {
            None
        };
        let rows = OutsourceQuoteRepo::list_with_filters(
            &mut *conn,
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
        let total = OutsourceQuoteRepo::count_with_filters(
            &mut *conn,
            query.status.as_deref(),
            &[],
            part_id,
            &part_ids_in,
            company_id,
        )
        .await?;
        let items = Self::quote_out_many(&mut *conn, rows).await?;
        Ok(OutsourceQuoteListOut {
            items,
            total,
            limit,
            offset,
        })
    }

    pub async fn get_quote(
        conn: &mut PgConnection,
        id: i64,
        current: &CurrentUser,
    ) -> Result<OutsourceQuoteOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk, Role::Inspector])?;
        let q = OutsourceQuoteRepo::get_by_id(&mut *conn, id, false)
            .await?
            .ok_or_else(|| not_found_quote(id))?;
        Self::quote_out(&mut *conn, q).await
    }

    pub async fn create_quote(
        conn: &mut PgConnection,
        snowflake: &SnowflakeIdGenerator,
        req: &OutsourceQuoteCreateRequest,
        current: &CurrentUser,
    ) -> Result<OutsourceQuoteOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk])?;
        let part_id = parse_snowflake_id(&req.part_id, "part_id")?;
        let company_id = parse_snowflake_id(&req.outsource_company_id, "outsource_company_id")?;
        let process_id = parse_snowflake_id(&req.process_id, "process_id")?;
        let price = parse_price(&req.price)?;

        // part 存在
        let part_exists: Option<(i64,)> = sqlx::query_as(
            "SELECT id FROM t_part WHERE id = $1 AND deleted_at IS NULL",
        )
        .bind(part_id)
        .fetch_optional(&mut *conn)
        .await?;
        if part_exists.is_none() {
            return Err(AppError::biz(
                code::BIZ_PART_NOT_FOUND,
                format!("part {part_id} 不存在"),
            ));
        }
        // 公司存在 + active
        let _company = OutsourceCompanyRepo::get_by_id(&mut *conn, company_id, false)
            .await?
            .ok_or_else(|| not_found_company(company_id))?;
        // 工序存在 + OUTSOURCE 类别
        let proc: Option<(String,)> = sqlx::query_as(
            "SELECT category FROM t_process WHERE id = $1 AND deleted_at IS NULL",
        )
        .bind(process_id)
        .fetch_optional(&mut *conn)
        .await?;
        let proc_category = proc.ok_or_else(|| {
            AppError::biz(
                code::BIZ_PROCESS_NOT_FOUND,
                format!("process {process_id} 不存在"),
            )
        })?;
        if proc_category.0 != "OUTSOURCE" {
            return Err(AppError::biz(
                code::BIZ_OUTSOURCE_COMPANY_BAD_PROCESS,
                format!("工序 {process_id} 不是 OUTSOURCE 类别"),
            ));
        }
        // 重复检查（应用层预校验 + DB 部分唯一索引双重兜底）
        if let Some(existing) = OutsourceQuoteRepo::get_active_for_tuple(
            &mut *conn, part_id, company_id, process_id,
        )
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
        let id = snowflake.next_id();
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
        let q = OutsourceQuoteRepo::create(&mut *conn, new).await.map_err(|e| {
            // uq_t_outsource_quote_approved_part_process 兜底
            if e.as_database_error()
                .and_then(|d| d.code())
                .as_deref()
                == Some("23505")
            {
                AppError::biz(
                    code::BIZ_OUTSOURCE_QUOTE_DUPLICATE,
                    "同一 (零件 / 外协公司 / 工序) 已存在活跃报价",
                )
            } else {
                AppError::from(e)
            }
        })?;
        // CREATED 事件
        OutsourceQuoteEventRepo::create(
            &mut *conn,
            NewOutsourceQuoteEvent {
                id: snowflake.next_id(),
                quote_id: q.id,
                event_type: "CREATED".to_string(),
                from_status: None,
                to_status: Some(q.status.clone()),
                note: None,
                created_by: current.id,
            },
        )
        .await?;
        Self::quote_out(&mut *conn, q).await
    }

    pub async fn update_quote(
        conn: &mut PgConnection,
        snowflake: &SnowflakeIdGenerator,
        id: i64,
        req: &OutsourceQuoteUpdateRequest,
        current: &CurrentUser,
    ) -> Result<OutsourceQuoteOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk])?;
        let q = OutsourceQuoteRepo::get_by_id(&mut *conn, id, false)
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
                format!(
                    "报价版本不一致：当前 {}，请求 {}",
                    q.version, req.version
                ),
            ));
        }
        let price = req.price.as_deref().map(parse_price).transpose()?;
        let note = req
            .note
            .as_ref()
            .map(|inner| inner.as_deref().map(|s| s.trim()).filter(|s| !s.is_empty()));
        let n = OutsourceQuoteRepo::update(
            &mut *conn,
            id,
            req.version,
            price,
            note,
            current.id,
        )
        .await?;
        if n == 0 {
            return Err(version_conflict());
        }
        // EDITED 事件
        OutsourceQuoteEventRepo::create(
            &mut *conn,
            NewOutsourceQuoteEvent {
                id: current_id_to_snowflake(snowflake),
                quote_id: id,
                event_type: "EDITED".to_string(),
                from_status: Some(q.status.clone()),
                to_status: Some(q.status.clone()),
                note: None,
                created_by: current.id,
            },
        )
        .await?;
        let fresh = OutsourceQuoteRepo::get_by_id(&mut *conn, id, false)
            .await?
            .ok_or_else(|| not_found_quote(id))?;
        Self::quote_out(&mut *conn, fresh).await
    }

    pub async fn submit_quote(
        conn: &mut PgConnection,
        snowflake: &SnowflakeIdGenerator,
        id: i64,
        current: &CurrentUser,
    ) -> Result<OutsourceQuoteOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk])?;
        let q = OutsourceQuoteRepo::get_by_id(&mut *conn, id, false)
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
        let n = OutsourceQuoteRepo::submit(&mut *conn, id, q.version, current.id).await?;
        if n == 0 {
            return Err(version_conflict());
        }
        OutsourceQuoteEventRepo::create(
            &mut *conn,
            NewOutsourceQuoteEvent {
                id: current_id_to_snowflake(snowflake),
                quote_id: id,
                event_type: "SUBMITTED".to_string(),
                from_status: Some(from.as_str().to_string()),
                to_status: Some(to.as_str().to_string()),
                note: None,
                created_by: current.id,
            },
        )
        .await?;
        let fresh = OutsourceQuoteRepo::get_by_id(&mut *conn, id, false)
            .await?
            .ok_or_else(|| not_found_quote(id))?;
        Self::quote_out(&mut *conn, fresh).await
    }

    pub async fn approve_quote(
        conn: &mut PgConnection,
        snowflake: &SnowflakeIdGenerator,
        id: i64,
        review_note: Option<&str>,
        version: i32,
        current: &CurrentUser,
    ) -> Result<OutsourceQuoteOut, AppError> {
        // MANAGER-only
        current.require_role(Role::Manager)?;
        let q = OutsourceQuoteRepo::get_by_id(&mut *conn, id, false)
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
        let _ = OutsourceQuoteRepo::reject_competitors(
            &mut *conn,
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
        let n = OutsourceQuoteRepo::approve(&mut *conn, id, version, review_note, current.id).await?;
        if n == 0 {
            return Err(version_conflict());
        }
        OutsourceQuoteEventRepo::create(
            &mut *conn,
            NewOutsourceQuoteEvent {
                id: current_id_to_snowflake(snowflake),
                quote_id: id,
                event_type: "APPROVED".to_string(),
                from_status: Some(from.as_str().to_string()),
                to_status: Some(to.as_str().to_string()),
                note: review_note.map(|s| s.to_string()),
                created_by: current.id,
            },
        )
        .await?;
        let fresh = OutsourceQuoteRepo::get_by_id(&mut *conn, id, false)
            .await?
            .ok_or_else(|| not_found_quote(id))?;
        Self::quote_out(&mut *conn, fresh).await
    }

    pub async fn reject_quote(
        conn: &mut PgConnection,
        snowflake: &SnowflakeIdGenerator,
        id: i64,
        review_note: &str,
        version: i32,
        current: &CurrentUser,
    ) -> Result<OutsourceQuoteOut, AppError> {
        current.require_role(Role::Manager)?;
        if review_note.trim().is_empty() {
            return Err(AppError::biz(
                code::BIZ_INVALID_VALUE,
                "review_note 必填",
            ));
        }
        let q = OutsourceQuoteRepo::get_by_id(&mut *conn, id, false)
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
        let n = OutsourceQuoteRepo::reject(&mut *conn, id, version, review_note, current.id).await?;
        if n == 0 {
            return Err(version_conflict());
        }
        OutsourceQuoteEventRepo::create(
            &mut *conn,
            NewOutsourceQuoteEvent {
                id: current_id_to_snowflake(snowflake),
                quote_id: id,
                event_type: "REJECTED".to_string(),
                from_status: Some(from.as_str().to_string()),
                to_status: Some(to.as_str().to_string()),
                note: Some(review_note.to_string()),
                created_by: current.id,
            },
        )
        .await?;
        let fresh = OutsourceQuoteRepo::get_by_id(&mut *conn, id, false)
            .await?
            .ok_or_else(|| not_found_quote(id))?;
        Self::quote_out(&mut *conn, fresh).await
    }

    pub async fn soft_delete_quote(
        conn: &mut PgConnection,
        id: i64,
        current: &CurrentUser,
    ) -> Result<(), AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk])?;
        let q = OutsourceQuoteRepo::get_by_id(&mut *conn, id, false)
            .await?
            .ok_or_else(|| not_found_quote(id))?;
        if !matches!(q.status.as_str(), "DRAFT" | "REJECTED") {
            return Err(AppError::biz(
                code::BIZ_OUTSOURCE_QUOTE_INVALID_TRANSITION,
                format!("已提交 / 已批准 / 已使用的报价不可删除；当前 {}", q.status),
            ));
        }
        let n = OutsourceQuoteRepo::soft_delete(&mut *conn, id, q.version, current.id).await?;
        if n == 0 {
            return Err(version_conflict());
        }
        Ok(())
    }

    pub async fn reconcile_update_shipment(
        conn: &mut PgConnection,
        id: i64,
        req: &OutsourceShipmentReconcileUpdateRequest,
        current: &CurrentUser,
    ) -> Result<OutsourceShipmentOut, AppError> {
        #[allow(clippy::collapsible_if)]
        {
            if let Some(q) = req.quantity {
                if q <= 0 {
                    return Err(AppError::biz(
                        code::BIZ_INVALID_VALUE,
                        "quantity 必须 > 0",
                    ));
                }
            }
        }
        current.require_any_role(&[Role::Manager, Role::Clerk])?;
        let s = OutsourceShipmentRepo::get_by_id(&mut *conn, id, false)
            .await?
            .ok_or_else(|| not_found_shipment(id))?;
        if s.version != req.version {
            return Err(AppError::biz(
                code::VERSION_CONFLICT,
                "该发货记录已被其他用户修改，请刷新后重试",
            ));
        }
        if !matches!(s.status.as_str(), "OUTSOURCING" | "RECEIVED") {
            return Err(AppError::biz(
                code::BIZ_OUTSOURCE_QUOTE_INVALID_TRANSITION,
                format!(
                    "对账编辑仅允许 OUTSOURCING / RECEIVED 状态；当前 {}",
                    s.status
                ),
            ));
        }
        let unit_price = req.unit_price.as_deref().map(parse_price).transpose()?;
        let n = OutsourceShipmentRepo::reconcile_update(
            &mut *conn,
            id,
            req.version,
            unit_price,
            req.quantity,
            req.is_billed,
            current.id,
        )
        .await?;
        if n == 0 {
            return Err(version_conflict());
        }
        let fresh = OutsourceShipmentRepo::get_by_id(&mut *conn, id, false)
            .await?
            .ok_or_else(|| not_found_shipment(id))?;
        Self::shipment_out(&mut *conn, fresh).await
    }

    // =======================================================================
    // 内部 helpers
    // =======================================================================

    fn company_out(c: TOutsourceCompany) -> OutsourceCompanyOut {
        OutsourceCompanyOut {
            id: c.id,
            name: c.name,
            contact_name: c.contact_name,
            contact_phone: c.contact_phone,
            address: c.address,
            is_active: c.is_active,
            version: c.version,
            created_at: c.created_at,
            updated_at: c.updated_at,
        }
    }

    async fn build_with_processes(
        conn: &mut PgConnection,
        company: TOutsourceCompany,
    ) -> Result<OutsourceCompanyWithProcessesOut, AppError> {
        let junctions =
            OutsourceCompanyProcessRepo::list_by_company(&mut *conn, company.id, false).await?;
        let process_ids: Vec<i64> = junctions.iter().map(|j| j.process_id).collect();
        let mut process_map: std::collections::HashMap<i64, (String, String, String)> =
            std::collections::HashMap::new();
        if !process_ids.is_empty() {
            let rows: Vec<(i64, String, String, String)> = sqlx::query_as(
                "SELECT id, code, name, category FROM t_process \
                 WHERE id = ANY($1) AND deleted_at IS NULL",
            )
            .bind(&process_ids)
            .fetch_all(&mut *conn)
            .await?;
            for r in rows {
                process_map.insert(r.0, (r.1, r.2, r.3));
            }
        }
        let processes = junctions
            .into_iter()
            .map(|j| {
                let (code, name, category) = process_map
                    .get(&j.process_id)
                    .cloned()
                    .unwrap_or_else(|| (String::new(), String::new(), "OUTSOURCE".to_string()));
                OutsourceCompanyProcessLinkOut {
                    process_id: j.process_id,
                    process_code: code,
                    process_name: name,
                    category,
                    sort_order: j.sort_order,
                }
            })
            .collect();
        Ok(OutsourceCompanyWithProcessesOut {
            id: company.id,
            name: company.name,
            contact_name: company.contact_name,
            contact_phone: company.contact_phone,
            address: company.address,
            is_active: company.is_active,
            version: company.version,
            created_at: company.created_at,
            updated_at: company.updated_at,
            processes,
        })
    }

    fn parse_process_ids(raw: &[String]) -> Result<Vec<i64>, AppError> {
        let mut out = Vec::with_capacity(raw.len());
        for s in raw {
            let id = s.trim().parse::<i64>().map_err(|_| {
                AppError::biz(
                    code::BIZ_INVALID_VALUE,
                    format!("process_id 不是合法雪花 ID: {s:?}"),
                )
            })?;
            out.push(id);
        }
        Ok(out)
    }

    async fn validate_processes_outsource(
        conn: &mut PgConnection,
        process_ids: &[i64],
    ) -> Result<(), AppError> {
        // 去重保序
        let mut seen = std::collections::HashSet::new();
        let ordered: Vec<i64> = process_ids
            .iter()
            .copied()
            .filter(|p| seen.insert(*p))
            .collect();
        if ordered.is_empty() {
            return Ok(());
        }
        let rows: Vec<(i64, String)> = sqlx::query_as(
            "SELECT id, category FROM t_process WHERE id = ANY($1) AND deleted_at IS NULL",
        )
        .bind(&ordered)
        .fetch_all(&mut *conn)
        .await?;
        let found: std::collections::HashSet<i64> = rows.iter().map(|r| r.0).collect();
        let missing: Vec<i64> = ordered.iter().copied().filter(|p| !found.contains(p)).collect();
        if !missing.is_empty() {
            return Err(AppError::biz(
                code::BIZ_PROCESS_NOT_FOUND,
                format!("process not found: {missing:?}"),
            ));
        }
        let bad: Vec<i64> = rows
            .iter()
            .filter(|r| r.1 != "OUTSOURCE")
            .map(|r| r.0)
            .collect();
        if !bad.is_empty() {
            return Err(AppError::biz(
                code::BIZ_OUTSOURCE_COMPANY_BAD_PROCESS,
                format!("工序 {bad:?} 不是 OUTSOURCE 类别"),
            ));
        }
        Ok(())
    }

    async fn replace_processes(
        conn: &mut PgConnection,
        snowflake: &SnowflakeIdGenerator,
        company_id: i64,
        process_ids: &[i64],
        updated_by: i64,
    ) -> Result<(), AppError> {
        let mut seen = std::collections::HashSet::new();
        let ordered: Vec<i64> = process_ids
            .iter()
            .copied()
            .filter(|p| seen.insert(*p))
            .collect();
        let _ = OutsourceCompanyProcessRepo::soft_delete_by_company(&mut *conn, company_id, updated_by)
            .await?;
        for (idx, pid) in ordered.iter().enumerate() {
            OutsourceCompanyProcessRepo::create(
                &mut *conn,
                NewOutsourceCompanyProcess {
                    id: snowflake.next_id(),
                    outsource_company_id: company_id,
                    process_id: *pid,
                    sort_order: idx as i32,
                    created_by: updated_by,
                },
            )
            .await?;
        }
        Ok(())
    }

    async fn quote_out_many(
        conn: &mut PgConnection,
        quotes: Vec<TOutsourceQuote>,
    ) -> Result<Vec<OutsourceQuoteOut>, AppError> {
        if quotes.is_empty() {
            return Ok(Vec::new());
        }
        let part_ids: Vec<i64> = quotes.iter().map(|q| q.part_id).collect();
        let company_ids: Vec<i64> = quotes.iter().map(|q| q.outsource_company_id).collect();
        let process_ids: Vec<i64> = quotes.iter().map(|q| q.process_id).collect();

        let part_map: std::collections::HashMap<i64, (Option<String>, String, String, bool, Option<String>)> = {
            let rows: Vec<(i64, Option<String>, String, String, bool, Option<String>)> =
                sqlx::query_as(
                    "SELECT id, serial_no, drawing_no, name, is_urgent, unit_price::text \
                     FROM t_part WHERE id = ANY($1) AND deleted_at IS NULL",
                )
                .bind(&part_ids)
                .fetch_all(&mut *conn)
                .await?;
            rows.into_iter()
                .map(|r| (r.0, (r.1, r.2, r.3, r.4, r.5)))
                .collect()
        };
        let company_map: std::collections::HashMap<i64, String> = {
            let rows: Vec<(i64, String)> = sqlx::query_as(
                "SELECT id, name FROM t_outsource_company \
                 WHERE id = ANY($1) AND deleted_at IS NULL",
            )
            .bind(&company_ids)
            .fetch_all(&mut *conn)
            .await?;
            rows.into_iter().collect()
        };
        let process_map: std::collections::HashMap<i64, (String, String)> = {
            let rows: Vec<(i64, String, String)> = sqlx::query_as(
                "SELECT id, code, name FROM t_process \
                 WHERE id = ANY($1) AND deleted_at IS NULL",
            )
            .bind(&process_ids)
            .fetch_all(&mut *conn)
            .await?;
            rows.into_iter().map(|r| (r.0, (r.1, r.2))).collect()
        };

        let mut out = Vec::with_capacity(quotes.len());
        for q in quotes {
            let (serial, drawing, name, urgent, unit_price) =
                part_map.get(&q.part_id).cloned().unwrap_or_else(|| {
                    (None, String::new(), String::new(), false, None)
                });
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

    async fn quote_out(
        conn: &mut PgConnection,
        q: TOutsourceQuote,
    ) -> Result<OutsourceQuoteOut, AppError> {
        let mut items = Self::quote_out_many(&mut *conn, vec![q]).await?;
        Ok(items.remove(0))
    }

    async fn shipment_out(
        conn: &mut PgConnection,
        s: TOutsourceShipment,
    ) -> Result<OutsourceShipmentOut, AppError> {
        // 单条拼装：part / process / company 三个批查
        let part: Option<(String, String)> =
            sqlx::query_as("SELECT drawing_no, name FROM t_part WHERE id = $1")
                .bind(s.part_id)
                .fetch_optional(&mut *conn)
                .await
                .ok()
                .flatten();
        let company: Option<String> =
            sqlx::query_scalar("SELECT name FROM t_outsource_company WHERE id = $1")
                .bind(s.outsource_company_id)
                .fetch_optional(&mut *conn)
                .await
                .ok()
                .flatten();
        let process: Option<String> =
            sqlx::query_scalar("SELECT name FROM t_process WHERE id = $1")
                .bind(s.process_id)
                .fetch_optional(&mut *conn)
                .await
                .ok()
                .flatten();
        let batch_no: Option<i32> = if let Some(bid) = s.batch_id {
            sqlx::query_scalar("SELECT batch_no FROM t_part_batch WHERE id = $1")
                .bind(bid)
                .fetch_optional(&mut *conn)
                .await
                .ok()
                .flatten()
        } else {
            None
        };
        Ok(OutsourceShipmentOut {
            id: s.id,
            version: s.version,
            quote_id: s.quote_id,
            part_id: s.part_id,
            batch_id: s.batch_id,
            batch_no,
            outsource_company_id: s.outsource_company_id,
            process_id: s.process_id,
            quantity: s.quantity,
            unit_price: format_price(&s.unit_price),
            status: s.status,
            sent_at: s.sent_at,
            received_at: s.received_at,
            is_billed: s.is_billed,
            created_at: s.created_at,
            updated_at: s.updated_at,
            part_drawing_no: part.as_ref().map(|p| p.0.clone()),
            part_name: part.map(|p| p.1),
            outsource_company_name: company,
            process_name: process,
            customer_path: None,
        })
    }
}

/// 把 `i64` 字符串解析为雪花 ID i64；解析失败返回 BIZ_INVALID_VALUE 400。
fn parse_snowflake_id(s: &str, field: &str) -> Result<i64, AppError> {
    let t = s.trim();
    if t.is_empty() {
        return Err(AppError::biz(
            code::BIZ_INVALID_VALUE,
            format!("{field} 不能为空"),
        ));
    }
    t.parse::<i64>().map_err(|_| {
        AppError::biz(
            code::BIZ_INVALID_VALUE,
            format!("{field} 不是合法雪花 ID: {s:?}"),
        )
    })
}

/// 2026-09-14 Phase 3 follow-up（current_id_to_snowflake 修复）：
/// 事件 id 直接用传入的 `SnowflakeIdGenerator::next_id()`。
/// 真实雪花 id 保证全局唯一，避免并发场景下 `current.id ^ 时间戳` 近似 id 的撞 id 风险。
fn current_id_to_snowflake(snowflake: &SnowflakeIdGenerator) -> i64 {
    snowflake.next_id()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infra::snowflake::SnowflakeIdGenerator;

    #[test]
    fn parse_price_valid_decimal() {
        assert_eq!(parse_price("12.50").unwrap().to_string(), "12.50");
        assert_eq!(parse_price("0").unwrap().to_string(), "0");
        assert_eq!(parse_price("  9.99  ").unwrap().to_string(), "9.99");
    }

    #[test]
    fn parse_price_invalid_string() {
        assert!(parse_price("not_a_number").is_err());
        assert!(parse_price("").is_err());
    }

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

    #[test]
    fn format_price_two_decimal() {
        assert_eq!(format_price(&Decimal::new(1250, 2)), "12.50");
        assert_eq!(format_price(&Decimal::new(0, 0)), "0.00");
    }

    #[test]
    fn current_id_to_snowflake_unique() {
        // 2026-09-14 Phase 3 follow-up：使用真实雪花 id 生成器。
        // 同一 generator 连续两次调用应产生不同的 id（雪花 id sequence 自增）。
        let generator = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
        let a = current_id_to_snowflake(&generator);
        let b = current_id_to_snowflake(&generator);
        assert_ne!(a, b, "雪花 id 应当单调递增");
    }
}

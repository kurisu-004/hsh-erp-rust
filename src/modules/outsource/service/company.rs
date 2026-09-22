//! outsource 域 service — 外协公司子模块
//!
//! 覆盖端点：
//! - list_companies       — 列表 + 过滤 + 分页
//! - get_company          — 详情（含工序映射）
//! - list_companies_for_process — 按工序反查 active 公司
//! - create_company       — 创建（可选一并写入工序能力清单）
//! - update_company       — 部分字段更新（OCC）
//! - soft_delete_company  — 软删（仍映射工序时 409）
//! - set_company_processes — 整体替换工序映射
//!
//! ## 事务边界（2026-09-22 refactor 对齐 iam 范本）
//! 事务移交 handler：service 仅业务逻辑，所有跨 repo 操作经 `repo: R`
//! （by-value；`R: OutsourceRepoTrait`）参数传入——handler/service 借 `&mut *tx` /
//! `&mut *conn` 喂给 `OutsourceRepoTrait` trait（trait 已直接 `impl for &mut PgConnection`）。
//! service 不知事务——handler `pool.begin()` + `tx.commit()` 包外。
//!
//! impl 块直接挂在 `OutsourceService` 上（与 mod.rs 共同 impl），通过 `super::super::OutsourceService`
//! 引用 struct 类型。本文件还承载 2 个 helper 测试（`parse_price_valid_decimal` /
//! `parse_price_invalid_string` —— helper 内部共用）。

use std::collections::{HashMap, HashSet};

use crate::auth::rbac::{CurrentUser, Role};
use crate::modules::outsource::dto::{
    OutsourceCompanyCreateRequest, OutsourceCompanyListQuery, OutsourceCompanyUpdateRequest,
    SetOutsourceCompanyProcessRequest,
};
use crate::modules::outsource::vo::{
    OutsourceCompanyListOut, OutsourceCompanyOut, OutsourceCompanyProcessLinkOut,
    OutsourceCompanyWithProcessesOut,
};
use crate::modules::outsource::model::{NewOutsourceCompany, NewOutsourceCompanyProcess, TOutsourceCompany};
use crate::modules::outsource::repo::OutsourceRepoTrait;
use crate::shared::error::{AppError, code};

use super::{
    DEFAULT_LIMIT, MAX_LIMIT, OutsourceService, format_price, not_found_company, version_conflict,
};

/// 公司出参 helper（不含工序映射）。
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

/// 拼装 `OutsourceCompanyWithProcessesOut`（含工序映射 + 工序名称补全）。
async fn build_with_processes<R: OutsourceRepoTrait>(
    repo: &mut R,
    company: TOutsourceCompany,
) -> Result<OutsourceCompanyWithProcessesOut, AppError> {
    let junctions = repo
        .junction_list_by_company(company.id, false)
        .await?;
    let process_ids: Vec<i64> = junctions.iter().map(|j| j.process_id).collect();
    let mut process_map: HashMap<i64, (String, String, String)> = HashMap::new();
    if !process_ids.is_empty() {
        let rows = repo.process_map_full(&process_ids).await?;
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

/// 把字符串 ID 列表解析为 i64 列表。
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

/// 校验给定的 process_ids 全是 OUTSOURCE 类别 + 存在。
async fn validate_processes_outsource<R: OutsourceRepoTrait>(
    repo: &mut R,
    process_ids: &[i64],
) -> Result<(), AppError> {
    // 去重保序
    let mut seen = HashSet::new();
    let ordered: Vec<i64> = process_ids
        .iter()
        .copied()
        .filter(|p| seen.insert(*p))
        .collect();
    if ordered.is_empty() {
        return Ok(());
    }
    let rows = repo.process_map_category(&ordered).await?;
    let found: HashSet<i64> = rows.iter().map(|r| r.0).collect();
    let missing: Vec<i64> = ordered
        .iter()
        .copied()
        .filter(|p| !found.contains(p))
        .collect();
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

/// 整体替换工序映射：先软删所有 junction，再按顺序 insert 新集合。
///
/// snowflake id 通过 `service.snowflake` 生成（而非借 trait 注入），与 iam 范本同形：
/// `service` 持有 `Arc<SnowflakeIdGenerator>` 字段，helper 借用 `&OutsourceService` 取 id。
async fn replace_processes<R: OutsourceRepoTrait>(
    service: &OutsourceService,
    repo: &mut R,
    company_id: i64,
    process_ids: &[i64],
    updated_by: i64,
) -> Result<(), AppError> {
    let mut seen = HashSet::new();
    let ordered: Vec<i64> = process_ids
        .iter()
        .copied()
        .filter(|p| seen.insert(*p))
        .collect();
    let _ = repo
        .junction_soft_delete_by_company(company_id, updated_by)
        .await?;
    for (idx, pid) in ordered.iter().enumerate() {
        repo.junction_create(NewOutsourceCompanyProcess {
            id: service.snowflake.next_id(),
            outsource_company_id: company_id,
            process_id: *pid,
            sort_order: idx as i32,
            created_by: updated_by,
        })
        .await?;
    }
    Ok(())
}

impl OutsourceService {
    // =======================================================================
    // Company — 列表 / 详情
    // =======================================================================

    pub async fn list_companies<R: OutsourceRepoTrait>(
        &self,
        mut repo: R,
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
        let rows = repo
            .company_list_with_filters(query.name_like.as_deref(), query.is_active, limit, offset)
            .await?;
        let total = repo
            .company_count_with_filters(query.name_like.as_deref(), query.is_active)
            .await?;
        let items = rows.into_iter().map(company_out).collect();
        Ok(OutsourceCompanyListOut {
            items,
            total,
            limit,
            offset,
        })
    }

    pub async fn get_company<R: OutsourceRepoTrait>(
        &self,
        mut repo: R,
        id: i64,
        current: &CurrentUser,
    ) -> Result<OutsourceCompanyWithProcessesOut, AppError> {
        current.require_any_role(&[
            Role::Manager,
            Role::Clerk,
            Role::CncProgrammer,
            Role::Inspector,
        ])?;
        let company = repo
            .company_get_by_id(id, false)
            .await?
            .ok_or_else(|| not_found_company(id))?;
        build_with_processes(&mut repo, company).await
    }

    pub async fn list_companies_for_process<R: OutsourceRepoTrait>(
        &self,
        mut repo: R,
        process_id: i64,
        current: &CurrentUser,
    ) -> Result<Vec<OutsourceCompanyOut>, AppError> {
        current.require_any_role(&[
            Role::Manager,
            Role::Clerk,
            Role::CncProgrammer,
            Role::Inspector,
        ])?;
        let ids = repo.junction_list_company_ids_by_process(process_id).await?;
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let companies = repo.company_list_by_ids(&ids).await?;
        Ok(companies
            .into_iter()
            .filter(|c| c.is_active)
            .map(company_out)
            .collect())
    }

    // =======================================================================
    // Company — 写
    // =======================================================================

    pub async fn create_company<R: OutsourceRepoTrait>(
        &self,
        mut repo: R,
        req: &OutsourceCompanyCreateRequest,
        current: &CurrentUser,
    ) -> Result<OutsourceCompanyWithProcessesOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk])?;
        let name = req.name.trim();
        if name.is_empty() {
            return Err(AppError::biz(code::BIZ_INVALID_VALUE, "name 不能为空"));
        }
        if repo.company_get_by_name(name).await?.is_some() {
            return Err(AppError::biz(
                code::BIZ_OUTSOURCE_COMPANY_DUPLICATE,
                format!("外协公司「{name}」已存在"),
            ));
        }
        let id = self.snowflake.next_id();
        let new = NewOutsourceCompany {
            id,
            name: name.to_string(),
            contact_name: req
                .contact_name
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string),
            contact_phone: req
                .contact_phone
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string),
            address: req
                .address
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string),
            is_active: req.is_active,
            created_by: current.id,
        };
        let company = repo.company_create(new).await.map_err(|e| {
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
            let int_ids = parse_process_ids(process_ids)?;
            validate_processes_outsource(&mut repo, &int_ids).await?;
            replace_processes(self, &mut repo, company.id, &int_ids, current.id).await?;
        }

        build_with_processes(&mut repo, company).await
    }

    pub async fn update_company<R: OutsourceRepoTrait>(
        &self,
        mut repo: R,
        id: i64,
        req: &OutsourceCompanyUpdateRequest,
        current: &CurrentUser,
    ) -> Result<OutsourceCompanyWithProcessesOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk])?;
        let company = repo
            .company_get_by_id(id, false)
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
                    if let Some(existing) = repo.company_get_by_name(t).await? {
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

        let n = repo
            .company_update(
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
                if e.as_database_error().and_then(|d| d.code()).as_deref() == Some("23505") {
                    AppError::biz(code::BIZ_OUTSOURCE_COMPANY_DUPLICATE, "外协公司名已存在")
                } else {
                    AppError::from(e)
                }
            })?;
        if n == 0 {
            return Err(version_conflict());
        }
        let fresh = repo
            .company_get_by_id(id, false)
            .await?
            .ok_or_else(|| not_found_company(id))?;
        build_with_processes(&mut repo, fresh).await
    }

    pub async fn soft_delete_company<R: OutsourceRepoTrait>(
        &self,
        mut repo: R,
        id: i64,
        current: &CurrentUser,
    ) -> Result<(), AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk])?;
        let company = repo
            .company_get_by_id(id, false)
            .await?
            .ok_or_else(|| not_found_company(id))?;
        let junctions = repo.junction_list_by_company(id, false).await?;
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
        let n = repo
            .company_soft_delete(id, company.version, current.id)
            .await?;
        if n == 0 {
            return Err(version_conflict());
        }
        Ok(())
    }

    pub async fn set_company_processes<R: OutsourceRepoTrait>(
        &self,
        mut repo: R,
        id: i64,
        req: &SetOutsourceCompanyProcessRequest,
        current: &CurrentUser,
    ) -> Result<OutsourceCompanyWithProcessesOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk])?;
        let company = repo
            .company_get_by_id(id, false)
            .await?
            .ok_or_else(|| not_found_company(id))?;
        let int_ids = parse_process_ids(&req.process_ids)?;
        validate_processes_outsource(&mut repo, &int_ids).await?;
        replace_processes(self, &mut repo, id, &int_ids, current.id).await?;
        let fresh = repo
            .company_get_by_id(id, false)
            .await?
            .ok_or_else(|| not_found_company(id))?;
        build_with_processes(&mut repo, fresh).await
    }
}

#[cfg(test)]
mod tests {
    //! 2026-09-22 refactor：原 `service.rs::mod tests` 5 helper 测试拆分；
    //! 本文件承接 company 子域用到的 2 个：`parse_price_valid_decimal` /
    //! `parse_price_invalid_string`。
    use rust_decimal::Decimal;

    use super::super::format_price;
    use super::super::parse_price;

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
    fn format_price_two_decimal() {
        // company 子域虽不直接调用，但 format_price 是共享 helper，回归一下防退化
        assert_eq!(format_price(&Decimal::new(1250, 2)), "12.50");
        assert_eq!(format_price(&Decimal::new(0, 0)), "0.00");
    }
}

//! process 域业务逻辑
//!
//! 对应 Python myERP/service/process_service.py。实施约定：方法签名接收
//! `&mut PgConnection`，由 handler 开 tx 并 commit。
//!
//! ## 业务约束（service 层 enforce）
//! - INHOUSE 强制 `requires_approval = false`（与 Python `_assert_inhouse_no_approval` 对齐）
//! - OUTSOURCE 保留请求值（默认 `true`）
//! - `code` 业务唯一键，update 不允许改
//! - 软删前查 `t_work_type_process` + `t_outsource_company_process` +
//!   `t_shelf_process` + `t_part.next_process_id` 引用计数（best-effort，见
//!   `ProcessRepo::count_process_references`）

use sqlx::PgConnection;

use crate::auth::rbac::{CurrentUser, Role};
use crate::infra::snowflake::SnowflakeIdGenerator;
use crate::modules::process::dto::*;
use crate::modules::process::model::TProcess;
use crate::modules::process::repo::ProcessRepo;
use crate::shared::error::{code, AppError};

const DEFAULT_LIMIT: i64 = 50;
const MAX_LIMIT: i64 = 500;

const CATEGORY_INHOUSE: &str = "INHOUSE";
const CATEGORY_OUTSOURCE: &str = "OUTSOURCE";

fn process_not_found() -> AppError {
    AppError::biz(code::BIZ_PROCESS_NOT_FOUND, "工序不存在")
}

fn version_conflict() -> AppError {
    AppError::biz(code::VERSION_CONFLICT, "数据已被他人修改，请刷新后重试")
}

/// 把 service 的 `TProcess` 转 `ProcessOut`。
fn to_process_out(p: TProcess) -> ProcessOut {
    ProcessOut {
        id: p.id,
        code: p.code,
        name: p.name,
        category: p.category,
        sort_order: p.sort_order,
        description: p.description,
        requires_approval: p.requires_approval,
        version: p.version,
        created_at: p.created_at,
        updated_at: p.updated_at,
    }
}

/// 校验 `category` ∈ {INHOUSE, OUTSOURCE}；返回规范化大写字符串。空 / 其他值都拒。
fn check_category(s: &str) -> Result<String, AppError> {
    let upper = s.trim().to_uppercase();
    match upper.as_str() {
        CATEGORY_INHOUSE | CATEGORY_OUTSOURCE => Ok(upper),
        _ => Err(AppError::biz(
            code::BIZ_INVALID_VALUE,
            "category 必须是 INHOUSE 或 OUTSOURCE",
        )),
    }
}

pub struct ProcessService;

impl ProcessService {
    // =======================================================================
    // 列表 / 详情
    // =======================================================================

    pub async fn list_processes(
        conn: &mut PgConnection,
        query: &ProcessListQuery,
        user: &CurrentUser,
    ) -> Result<ProcessListOut, AppError> {
        user.require_any_role(&[
            Role::Manager,
            Role::Clerk,
            Role::CncProgrammer,
            Role::ShelfAccount,
            Role::Inspector,
        ])?;

        let limit = query.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
        let offset = query.offset.unwrap_or(0).max(0);
        let code_like = query
            .code_like
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let category = query
            .category
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());

        let items =
            ProcessRepo::list_with_filters(&mut *conn, code_like, category, limit, offset).await?;
        let total = ProcessRepo::count_with_filters(&mut *conn, code_like, category).await?;

        Ok(ProcessListOut {
            items: items.into_iter().map(to_process_out).collect(),
            total,
            limit,
            offset,
        })
    }

    pub async fn get_process(
        conn: &mut PgConnection,
        id: i64,
        user: &CurrentUser,
    ) -> Result<ProcessOut, AppError> {
        user.require_any_role(&[
            Role::Manager,
            Role::Clerk,
            Role::CncProgrammer,
            Role::ShelfAccount,
            Role::Inspector,
        ])?;

        let p = ProcessRepo::get_by_id(&mut *conn, id, false)
            .await?
            .ok_or_else(process_not_found)?;
        Ok(to_process_out(p))
    }

    // =======================================================================
    // 创建 / 更新 / 软删
    // =======================================================================

    pub async fn create_process(
        conn: &mut PgConnection,
        snowflake: &SnowflakeIdGenerator,
        req: &ProcessCreateRequest,
        user: &CurrentUser,
    ) -> Result<ProcessOut, AppError> {
        user.require_role(Role::Manager)?;

        let code = req.code.trim();
        if code.is_empty() {
            return Err(AppError::biz(code::BIZ_INVALID_VALUE, "code 不能为空"));
        }
        let name = req.name.trim();
        if name.is_empty() {
            return Err(AppError::biz(code::BIZ_INVALID_VALUE, "name 不能为空"));
        }
        let category = check_category(&req.category)?;
        let sort_order = req.sort_order.unwrap_or(0);
        let description = req.description.as_deref().map(str::trim).filter(|s| !s.is_empty());

        // INHOUSE 强制 requires_approval = false（无视请求值）；OUTSOURCE 保留（默认 true）。
        let requires_approval = if category == CATEGORY_INHOUSE {
            false
        } else {
            req.requires_approval.unwrap_or(true)
        };

        let id = snowflake.next_id();
        let p = ProcessRepo::create(
            &mut *conn,
            id,
            code,
            name,
            &category,
            sort_order,
            description,
            requires_approval,
            user.id,
        )
        .await
        .map_err(|e| match e.as_database_error().and_then(|d| d.code()).as_deref() {
            // uk_t_process_code：活跃行唯一
            Some("23505") => AppError::biz(
                code::BIZ_PROCESS_DUPLICATE_CODE,
                format!("code '{code}' 已被占用"),
            ),
            // ck_t_process_category：CHECK 约束兜底（理论 service 已 catch）
            Some("23514") => AppError::biz(
                code::BIZ_INVALID_VALUE,
                "category 必须是 INHOUSE 或 OUTSOURCE",
            ),
            _ => AppError::from(e),
        })?;

        Ok(to_process_out(p))
    }

    pub async fn update_process(
        conn: &mut PgConnection,
        id: i64,
        req: &ProcessUpdateRequest,
        user: &CurrentUser,
    ) -> Result<ProcessOut, AppError> {
        user.require_role(Role::Manager)?;

        // code 业务唯一键不可变 —— 不论传值都拒（含空串、null 之外的任何值）
        if req.code.is_some() {
            return Err(AppError::biz(
                code::BIZ_INVALID_VALUE,
                "本接口不支持修改 code（业务唯一键不可变）",
            ));
        }
        if req.category.is_some() {
            return Err(AppError::biz(
                code::BIZ_INVALID_VALUE,
                "本接口不支持修改 category",
            ));
        }

        let current = ProcessRepo::get_by_id(&mut *conn, id, false)
            .await?
            .ok_or_else(process_not_found)?;

        // name: Some("") ⇒ 显式拒；None ⇒ 不修改
        let name_update: Option<&str> = match req.name.as_deref() {
            Some(s) => {
                let t = s.trim();
                if t.is_empty() {
                    return Err(AppError::biz(code::BIZ_INVALID_VALUE, "name 不能为空"));
                }
                Some(t)
            }
            None => None,
        };

        // description 三态：None ⇒ 不改；Some(None) ⇒ 清空；Some(Some(v)) ⇒ 改值
        // 改值场景的 owned String 保留到 await 之后
        let new_desc_owned: Option<String>;
        let desc_update: Option<Option<&str>> = match &req.description {
            None => None,
            Some(None) => Some(None),
            Some(Some(s)) => {
                let trimmed = s.trim();
                new_desc_owned = Some(trimmed.to_string());
                Some(Some(new_desc_owned.as_deref().unwrap()))
            }
        };

        // requires_approval：INHOUSE 强制 false；其他情况尊重请求值（None 不改）
        let requires_approval_update: Option<bool> = if current.category == CATEGORY_INHOUSE {
            Some(false)
        } else {
            req.requires_approval
        };

        let affected = ProcessRepo::update(
            &mut *conn,
            id,
            current.version,
            name_update,
            req.sort_order,
            desc_update,
            requires_approval_update,
            user.id,
        )
        .await
        .map_err(|e| match e.as_database_error().and_then(|d| d.code()).as_deref() {
            Some("23514") => AppError::biz(
                code::BIZ_INVALID_VALUE,
                "category 必须是 INHOUSE 或 OUTSOURCE",
            ),
            _ => AppError::from(e),
        })?;
        if affected == 0 {
            return Err(version_conflict());
        }

        // 回读最新行
        Self::get_process(conn, id, user).await
    }

    pub async fn soft_delete_process(
        conn: &mut PgConnection,
        id: i64,
        user: &CurrentUser,
    ) -> Result<(), AppError> {
        user.require_role(Role::Manager)?;

        let current = ProcessRepo::get_by_id(&mut *conn, id, false)
            .await?
            .ok_or_else(process_not_found)?;

        // 软删前查引用计数（best-effort：见 repo 注释）
        let ref_count = ProcessRepo::count_process_references(&mut *conn, id).await?;
        if ref_count > 0 {
            return Err(AppError::biz(
                code::BIZ_PROCESS_IN_USE,
                format!("工序仍被引用（{ref_count} 处），无法软删"),
            ));
        }

        let affected =
            ProcessRepo::soft_delete(&mut *conn, id, current.version, user.id).await?;
        if affected == 0 {
            return Err(version_conflict());
        }
        Ok(())
    }
}
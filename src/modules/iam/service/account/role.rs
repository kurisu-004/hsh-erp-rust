//! `AccountService` 的 `t_user_role` 子块：角色列表 / 授予 / 撤销 + SHELF_ACCOUNT scope 校验
//!
//! `add_role` 是纯 INSERT（新行没有 version），故无 OCC 锚点；`remove_role` 的 OCC
//! 锚点是 `expected_version`（客户端在 body 里显式传该行 `UserRoleOut.version`）。
//!
//! 写侧查重走 `has_user_role_with_scope`（`IS NOT DISTINCT FROM` 预检），因为唯一
//! 约束 `uk_t_user_role_user_role_scope` 是**普通 UNIQUE** 而非 partial：PG 视 NULL
//! 互不相等，`(user_id, role, NULL, NULL)` 这类组合索引拦不住。详见
//! `docs/api/iam.md` 的「已知偏差登记」。

use crate::auth::rbac::{CurrentUser, Role};
use crate::infra::clock::now_naive;
use crate::shared::error::{AppError, code};

use super::super::super::dto::UserAddRoleRequest;
use super::super::super::repo::{IamRepoTrait, UserRoleInsert};
use super::super::super::vo::UserRoleOut;
use super::AccountService;
use super::{
    ALLOWED_SHELF_ZONES, SCOPE_TYPE_SHELF, map_duplicate_role, to_role_out, user_not_found,
    version_conflict,
};

impl AccountService {
    /// `GET /api/v2/iam/users/{id}/roles`
    pub async fn list_user_roles<R: IamRepoTrait>(
        &self,
        mut repo: R,
        user_id: i64,
        current: &CurrentUser,
    ) -> Result<Vec<UserRoleOut>, AppError> {
        current.require_role(Role::Manager)?;

        repo.get_user_by_id(user_id)
            .await?
            .ok_or_else(|| user_not_found(user_id))?;

        let rows = repo.list_user_roles_by_user_id(user_id).await?;
        Ok(rows.iter().map(to_role_out).collect())
    }

    /// `POST /api/v2/iam/users/{id}/roles`：授予一个角色（纯 INSERT，无 OCC）。
    pub async fn add_role<R: IamRepoTrait>(
        &self,
        mut repo: R,
        user_id: i64,
        req: &UserAddRoleRequest,
        current: &CurrentUser,
    ) -> Result<UserRoleOut, AppError> {
        current.require_role(Role::Manager)?;

        repo.get_user_by_id(user_id)
            .await?
            .ok_or_else(|| user_not_found(user_id))?;

        Self::validate_role_scope(&mut repo, req).await?;

        let role_str = req.role.as_str();
        let scope_type = req.scope_type.as_deref();

        // 显式查重。唯一约束 `uk_t_user_role_user_role_scope` 是普通 UNIQUE（非 partial），
        // PG 视 NULL 互不相等 ⇒ `(user_id, role, NULL, NULL)` 这类含 NULL 的组合索引拦不住，
        // 非货架角色会被重复添加。这里用 IS NOT DISTINCT FROM 显式查重堵住该缺口。
        if repo
            .has_user_role_with_scope(user_id, role_str, scope_type, req.scope_id)
            .await?
        {
            return Err(AppError::biz(
                code::ROLE_DUPLICATE,
                format!(
                    "role {role_str} (scope={}/{}) already assigned to this user",
                    scope_type.unwrap_or("null"),
                    req.scope_id
                        .map(|v| v.to_string())
                        .unwrap_or_else(|| "null".into()),
                ),
            ));
        }

        let insert = UserRoleInsert {
            id: self.snowflake.next_id(),
            user_id,
            role: role_str.to_string(),
            scope_type: scope_type.map(str::to_string),
            scope_id: req.scope_id,
            created_at: now_naive(),
            created_by: Some(current.id),
        };
        repo.create_user_role(&insert)
            .await
            .map_err(map_duplicate_role)?;

        let rows = repo.list_user_roles_by_user_id(user_id).await?;
        rows.iter()
            .find(|r| r.id == insert.id)
            .map(to_role_out)
            .ok_or_else(|| AppError::internal("创建后回读角色失败"))
    }

    /// `POST /api/v2/iam/users/{id}/roles/{role_id}/remove`：撤销一个角色。
    /// OCC 锚点 = `expected_version`（客户端传该角色行当前的 version）。
    pub async fn remove_role<R: IamRepoTrait>(
        &self,
        mut repo: R,
        user_id: i64,
        role_id: i64,
        expected_version: i32,
        current: &CurrentUser,
    ) -> Result<(), AppError> {
        current.require_role(Role::Manager)?;

        repo.get_user_by_id(user_id)
            .await?
            .ok_or_else(|| user_not_found(user_id))?;

        let r = repo.get_user_role_by_id(role_id).await?;
        // 角色必须存在且属于该用户，否则一律 404（不泄露他人角色是否存在）
        let r = match r {
            Some(r) if r.user_id == user_id => r,
            _ => {
                return Err(AppError::biz(
                    code::ROLE_NOT_FOUND,
                    format!("role {role_id} not found for user {user_id}"),
                ));
            }
        };

        let affected = repo
            .soft_delete_user_role(r.id, expected_version, now_naive(), Some(current.id))
            .await?;
        if affected == 0 {
            return Err(version_conflict());
        }
        Ok(())
    }

    // =======================================================================
    // 内部
    // =======================================================================

    /// 校验 SHELF_ACCOUNT 角色的 scope 形态、货架存在、zone 白名单、is_active。
    /// 收 `&mut R` 而非 `&mut uow`：调用方把 repo 借进来，helper 只取 shelf_repo。
    async fn validate_role_scope<R: IamRepoTrait>(
        repo: &mut R,
        req: &UserAddRoleRequest,
    ) -> Result<(), AppError> {
        if req.role == Role::ShelfAccount {
            // 校验顺序与 Python `_validate_role_scope` 一致：
            // scope 形态 → 货架存在 → zone 白名单 → is_active
            if req.scope_type.as_deref() != Some(SCOPE_TYPE_SHELF) || req.scope_id.is_none() {
                return Err(AppError::validation(
                    "SHELF_ACCOUNT role requires scope_type='shelf' and scope_id",
                ));
            }
            let shelf_id = req.scope_id.expect("上一步已校验非空");
            let shelf = repo.get_shelf_by_id(shelf_id).await?.ok_or_else(|| {
                AppError::biz(code::NOT_FOUND, format!("shelf {shelf_id} not found"))
            })?;
            if !ALLOWED_SHELF_ZONES.contains(&shelf.zone.as_str()) {
                return Err(AppError::biz(
                    code::NOT_FOUND,
                    format!("shelf {shelf_id} has invalid zone '{}'", shelf.zone),
                ));
            }
            if !shelf.is_active {
                return Err(AppError::biz(
                    code::NOT_FOUND,
                    format!("shelf '{}' is inactive; cannot bind", shelf.code),
                ));
            }
        } else if req.scope_type.is_some() || req.scope_id.is_some() {
            // MANAGER / CLERK / INSPECTOR / CNC_PROGRAMMER 暂不接 scope
            return Err(AppError::validation(format!(
                "role {} does not accept scope",
                req.role.as_str()
            )));
        }
        Ok(())
    }
}

//! `AccountService` 的 `t_user` 子块：列表 / 详情 / 创建 / 更新 / 停用 / 密码
//!
//! 与另两个子块的分工：`role` 管 `t_user_role` 的增删查与 SHELF_ACCOUNT 的 scope
//! 校验，`wx` 管 `t_wx_identity` 的管理端读写；本文件只碰 `t_user` 本身
//! （`list_user_roles_*` 属于「读账号带出角色」的组装动作，故留在本文件）。
//!
//! ## 乐观锁（OCC）约定
//! `version` 有两个来源，按端点分：
//! - **客户端 body 传入**（`update_user` 的 `req.version` / `deactivate_user` 的
//!   `expected_version`）。看板数据是 30s 缓存的快照，「读-再-比」式隐式 OCC 会让
//!   「用户看到 5 件 → 实际只动 3 件」静默成功；这类端点里 service 的
//!   `get_user_by_id` 只用于 404 归因（区分「不存在」与「版本冲突」）。
//! - **service 从 DB 读到**：`admin_reset_password`（OCC 豁免的幂等端点，
//!   `CLAUDE.md` 的豁免清单登记了这一条）与 `change_own_password`（必须先读出
//!   `password_hash` 才能校验旧密码，本仓改密端点一律不收 OCC 锚点）。

use crate::auth::password;
use crate::auth::rbac::{CurrentUser, Role};
use crate::infra::clock::now_naive;
use crate::shared::error::{AppError, code};

use super::super::super::dto::{UserCreateRequest, UserListQuery, UserUpdateRequest};
use super::super::super::repo::model::User;
use super::super::super::repo::{IamRepoTrait, UserInsert, UserPartialUpdate};
use super::super::super::vo::{UserListOut, UserOut};
use super::AccountService;
use super::{
    DEFAULT_LIMIT, DEFAULT_RESET_PASSWORD, MAX_LIMIT, assemble_user_out, bucket_roles_by_user,
    map_duplicate_username, trimmed_or_none, user_not_found, version_conflict,
};

impl AccountService {
    // =======================================================================
    // 列表 / 详情
    // =======================================================================

    /// `GET /api/v2/iam/users`：分页 + 过滤，每行附带该账号的角色。
    ///
    /// 角色用**一次**批量查询（`list_user_roles_by_user_ids`）取回本页全部行再按
    /// `user_id` 分桶，不是逐行 `list_user_roles_by_user_id` —— 后者在 20 行/页时是
    /// 21 次查询。
    pub async fn list_users<R: IamRepoTrait>(
        &self,
        mut repo: R,
        query: &UserListQuery,
        current: &CurrentUser,
    ) -> Result<UserListOut, AppError> {
        current.require_role(Role::Manager)?;

        let limit = query.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
        let offset = query.offset.unwrap_or(0).max(0);
        let like = query
            .username_like
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());

        let rows = repo
            .list_users_with_filters(like, query.is_active, limit, offset)
            .await?;
        let total = repo.count_users_with_filters(like, query.is_active).await?;

        let user_ids: Vec<i64> = rows.iter().map(|u| u.id).collect();
        // 空数组时 repo 侧短路返空 Vec、不发 SQL（见 `sql::user_role`）
        let roles_by_user =
            bucket_roles_by_user(repo.list_user_roles_by_user_ids(&user_ids).await?);

        let items = rows
            .iter()
            .map(|u| {
                let roles = roles_by_user.get(&u.id).map(Vec::as_slice).unwrap_or(&[]);
                assemble_user_out(u, roles)
            })
            .collect();

        Ok(UserListOut {
            items,
            total,
            limit,
            offset,
        })
    }

    /// `GET /api/v2/iam/users/{id}`。单行详情走单账号版角色查询（无需批量）。
    pub async fn get_user<R: IamRepoTrait>(
        &self,
        mut repo: R,
        user_id: i64,
        current: &CurrentUser,
    ) -> Result<UserOut, AppError> {
        current.require_role(Role::Manager)?;

        let u = repo
            .get_user_by_id(user_id)
            .await?
            .ok_or_else(|| user_not_found(user_id))?;
        let roles = repo.list_user_roles_by_user_id(u.id).await?;
        Ok(assemble_user_out(&u, &roles))
    }

    // =======================================================================
    // 创建 / 更新 / 停用
    // =======================================================================

    /// `POST /api/v2/iam/users`。纯 INSERT，无 OCC 锚点。
    pub async fn create_user<R: IamRepoTrait>(
        &self,
        mut repo: R,
        req: &UserCreateRequest,
        current: &CurrentUser,
    ) -> Result<UserOut, AppError> {
        current.require_role(Role::Manager)?;

        let username = req.username.trim().to_lowercase();
        if username.is_empty() {
            return Err(AppError::validation("username 不能为空"));
        }
        if req.password.is_empty() {
            return Err(AppError::validation("password 不能为空"));
        }

        // 显式查重（partial unique 索引仍是最终防线，见下方 INSERT 的错误映射）
        if repo.get_user_by_username(&username).await?.is_some() {
            return Err(AppError::biz(
                code::DUPLICATE_USERNAME,
                format!("username '{username}' already exists"),
            ));
        }

        let full_name = req.full_name.trim();
        if full_name.is_empty() {
            return Err(AppError::validation("full_name 不能为空"));
        }

        let insert = UserInsert {
            id: self.snowflake.next_id(),
            username,
            password_hash: password::hash(&req.password)?,
            full_name: full_name.to_string(),
            phone: req.phone.as_deref().and_then(trimmed_or_none),
            is_active: true,
            created_at: now_naive(),
            created_by: Some(current.id),
        };

        repo.create_user(&insert)
            .await
            .map_err(map_duplicate_username)?;

        let u = repo
            .get_user_by_id(insert.id)
            .await?
            .ok_or_else(|| AppError::internal("创建后回读用户失败"))?;
        let roles = repo.list_user_roles_by_user_id(u.id).await?;
        Ok(assemble_user_out(&u, &roles))
    }

    /// `POST /api/v2/iam/users/{id}/update`：部分更新，OCC 锚点 = `req.version`。
    pub async fn update_user<R: IamRepoTrait>(
        &self,
        mut repo: R,
        user_id: i64,
        req: &UserUpdateRequest,
        current: &CurrentUser,
    ) -> Result<UserOut, AppError> {
        current.require_role(Role::Manager)?;

        // 只用于 404 归因：账号不存在 vs 存在但 version 过期，两者的响应码不同
        repo.get_user_by_id(user_id)
            .await?
            .ok_or_else(|| user_not_found(user_id))?;

        // full_name 若提供则必须非空（对齐 Python `min_length=1`）
        let full_name = match req.full_name.as_deref() {
            Some(v) => {
                let t = v.trim();
                if t.is_empty() {
                    return Err(AppError::validation("full_name 不能为空"));
                }
                Some(t.to_string())
            }
            None => None,
        };

        // phone 提供空串 = 显式清空（Python `.strip() or None`）
        let set_phone = req.phone.is_some();
        let phone = req.phone.as_deref().and_then(trimmed_or_none);

        // 管理员在此改密**不**轮转 refresh_token_version（对齐 Python update_user）
        let password_hash = match req.password.as_deref() {
            Some("") => return Err(AppError::validation("password 不能为空")),
            Some(p) => Some(password::hash(p)?),
            None => None,
        };

        let affected = repo
            .update_user_partial(
                user_id,
                req.version,
                &UserPartialUpdate {
                    full_name: full_name.as_deref(),
                    set_phone,
                    phone: phone.as_deref(),
                    password_hash: password_hash.as_deref(),
                    is_active: req.is_active,
                    when: now_naive(),
                    updated_by: Some(current.id),
                },
            )
            .await?;
        if affected == 0 {
            return Err(version_conflict());
        }

        let updated = repo
            .get_user_by_id(user_id)
            .await?
            .ok_or_else(|| user_not_found(user_id))?;
        let roles = repo.list_user_roles_by_user_id(updated.id).await?;
        Ok(assemble_user_out(&updated, &roles))
    }

    /// `POST /api/v2/iam/users/{id}/deactivate`：停用账号 = 软删
    /// （置 `deleted_at` + `is_active = false`）。OCC 锚点 = `expected_version`。
    pub async fn deactivate_user<R: IamRepoTrait>(
        &self,
        mut repo: R,
        user_id: i64,
        expected_version: i32,
        current: &CurrentUser,
    ) -> Result<UserOut, AppError> {
        current.require_role(Role::Manager)?;

        let u = repo
            .get_user_by_id(user_id)
            .await?
            .ok_or_else(|| user_not_found(user_id))?;

        let affected = repo
            .soft_delete_user(user_id, expected_version, now_naive(), Some(current.id))
            .await?;
        if affected == 0 {
            return Err(version_conflict());
        }

        // 软删后 get_by_id 会过滤掉该行，故用内存中的行 + 手工推进字段组装出参
        // （对齐 Python `_to_out(u, include_deleted=True)`）。
        let roles = repo.list_user_roles_by_user_id(u.id).await?;
        let now = now_naive();
        Ok(assemble_user_out(
            &User {
                is_active: false,
                deleted_at: Some(now),
                updated_at: now,
                updated_by: Some(current.id),
                version: expected_version + 1,
                ..u
            },
            &roles,
        ))
    }

    // =======================================================================
    // 密码
    // =======================================================================

    /// 自助改密：校验旧密码，写新哈希并轮转 refresh token（同一条 UPDATE，原子）。
    ///
    /// 允许本人或 MANAGER 调用。**不**再自己清 Redis session——handler 在 commit 之后
    /// 调 `state.session.delete_all_user_sessions(user_id)`（best-effort）。DB 的
    /// `refresh_token_version` 轮转是兜底。
    ///
    /// 条件写用读到的 `u.version`（该端点不收 OCC 锚点：改密是本人显式操作，不与
    /// 看板上的账号编辑争并发）。
    pub async fn change_own_password<R: IamRepoTrait>(
        &self,
        mut repo: R,
        user_id: i64,
        old_password: &str,
        new_password: &str,
        current: &CurrentUser,
    ) -> Result<(), AppError> {
        if user_id != current.id && !current.has_role(Role::Manager) {
            return Err(AppError::biz(code::FORBIDDEN, "只能修改本人密码"));
        }
        if new_password.is_empty() {
            return Err(AppError::validation("new_password 不能为空"));
        }

        let u = repo
            .get_user_by_id(user_id)
            .await?
            .ok_or_else(|| user_not_found(user_id))?;
        // get_by_id 已过滤 deleted_at；此处再挡停用账号（对齐 Python 的三重判断）
        if !u.is_active {
            return Err(user_not_found(user_id));
        }

        if !password::verify(old_password, &u.password_hash)? {
            return Err(AppError::biz(code::OLD_PASSWORD_MISMATCH, "旧密码不正确"));
        }

        let affected = repo
            .update_user_password_and_rotate(
                u.id,
                u.version,
                &password::hash(new_password)?,
                now_naive(),
                Some(current.id),
            )
            .await?;
        if affected == 0 {
            return Err(version_conflict());
        }
        Ok(())
    }

    /// 管理员重置密码为默认口令 `changeme`，并轮转 refresh token（踢下线）。
    /// **不**再自己清 Redis session——handler 在 commit 之后清（best-effort）。
    ///
    /// OCC 豁免：幂等端点（`CLAUDE.md` 的豁免清单登记了这一条），body 不收
    /// `version`，用 service 读到的 version 做条件写。
    pub async fn admin_reset_password<R: IamRepoTrait>(
        &self,
        mut repo: R,
        user_id: i64,
        current: &CurrentUser,
    ) -> Result<UserOut, AppError> {
        current.require_role(Role::Manager)?;

        let u = repo
            .get_user_by_id(user_id)
            .await?
            .ok_or_else(|| user_not_found(user_id))?;

        let affected = repo
            .update_user_password_and_rotate(
                u.id,
                u.version,
                &password::hash(DEFAULT_RESET_PASSWORD)?,
                now_naive(),
                Some(current.id),
            )
            .await?;
        if affected == 0 {
            return Err(version_conflict());
        }

        let updated = repo
            .get_user_by_id(user_id)
            .await?
            .ok_or_else(|| user_not_found(user_id))?;
        let roles = repo.list_user_roles_by_user_id(updated.id).await?;
        Ok(assemble_user_out(&updated, &roles))
    }
}

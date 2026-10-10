//! `AccountService` 的 `t_user_role` 子块：角色列表 / 授予 / 撤销 + scope 校验
//!
//! `add_role` 是纯 INSERT（新行没有 version），故无 OCC 锚点；`remove_role` 的 OCC
//! 锚点是 `expected_version`（客户端在 body 里显式传该行 `UserRoleOut.version`）。
//!
//! 写侧查重走 `has_user_role_with_scope`（`IS NOT DISTINCT FROM` 预检），因为唯一
//! 约束 `uk_t_user_role_user_role_scope` 是**普通 UNIQUE** 而非 partial：PG 视 NULL
//! 互不相等，`(user_id, role, NULL, NULL)` 这类组合索引拦不住。详见
//! `docs/api/iam.md` 的「已知偏差登记」。
//!
//! ## 角色码是原始字符串，不是枚举（2026-10-14）
//! 入参 `UserAddRoleRequest::role` 由 `Role` 枚举改成 `String`。`t_user_role.role`
//! 是无 FK varchar、能存 `t_role` 里的自定义角色码；用枚举时 serde 会在进入
//! handler **之前**就把未知码拒成 422，自定义角色根本没法被授予。
//!
//! 合法性改由 [`Self::validate_role_exists`] 查 `t_role` 判定（存在且
//! `is_active = true`）。枚举只在**需要 scope 规则**与**ADMIN 护栏**两处用
//! `parse_role_string` 认一下：认不出（如 `QC_LEAD`）就走「自定义角色」分支。
//!
//! ## 踢下线
//! `add_role` / `remove_role` 都返回 `KickList`（= 目标用户 id，恒长度 ≤ 1），
//! 由 **handler 在 `tx.commit()` 之后**调 `delete_all_user_sessions`。理由：
//! 授权数据装在 Redis session 缓存里，不踢就等于这次授予/撤销对目标无效
//! 直到 session 自然过期；而 Redis 不在 DB 事务内，先踢会在事务回滚时把
//! 用户误踢下线。
//!
//! ## ADMIN 自锁护栏
//! 下列两种操作若会让「active 且持有 ADMIN 的用户数」归零 → 400，且因返回
//! `Err`（handler 未 commit）**不会**触发踢下线：
//! 1. 撤销用户的 ADMIN 角色（本文件 `remove_role`）；
//! 2. 停用持有 ADMIN 的用户（`user::deactivate_user`）。

use crate::auth::rbac::{CurrentUser, Role, parse_role_string};
use crate::infra::clock::now_naive;
use crate::modules::iam::rbac::service::ADMIN_ROLE_CODE;
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
    ///
    /// 返回 `(UserRoleOut, KickList)`；`KickList` 恒为 `[user_id]`
    /// （新增授权必然改变该用户的权限集），handler 在 commit 后踢其 session。
    pub async fn add_role<R: IamRepoTrait>(
        &self,
        mut repo: R,
        user_id: i64,
        req: &UserAddRoleRequest,
        current: &CurrentUser,
    ) -> Result<(UserRoleOut, Vec<i64>), AppError> {
        current.require_role(Role::Manager)?;

        repo.get_user_by_id(user_id)
            .await?
            .ok_or_else(|| user_not_found(user_id))?;

        // 合法性 = `t_role` 里存在且 is_active（自定义角色由此可被授予）
        Self::validate_role_exists(&mut repo, &req.role).await?;
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
        let out = rows
            .iter()
            .find(|r| r.id == insert.id)
            .map(to_role_out)
            .ok_or_else(|| AppError::internal("创建后回读角色失败"))?;
        Ok((out, vec![user_id]))
    }

    /// `POST /api/v2/iam/users/{id}/roles/{role_id}/remove`：撤销一个角色。
    /// OCC 锚点 = `expected_version`（客户端传该角色行当前的 version）。
    ///
    /// 返回 `KickList`（恒 `[user_id]`），handler 在 commit 后踢其 session。
    /// **撤销最后一个 ADMIN 会被护栏拒绝**（见 [`Self::guard_admin_lockout`]），
    /// 此时返回 `Err` ⇒ handler 未 commit ⇒ 不会踢任何人。
    pub async fn remove_role<R: IamRepoTrait>(
        &self,
        mut repo: R,
        user_id: i64,
        role_id: i64,
        expected_version: i32,
        current: &CurrentUser,
    ) -> Result<Vec<i64>, AppError> {
        current.require_role(Role::Manager)?;

        let u = repo
            .get_user_by_id(user_id)
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

        // ADMIN 自锁护栏：撤的是 ADMIN 且撤完就没有 active 管理员了 → 400。
        // 目标用户已停用时跳过（它不在 active 管理员计数里，撤其 ADMIN 不构成
        // 新的自锁风险）—— 与 `deactivate_user` 的 `u.is_active` 快筛同口径。
        if r.role == ADMIN_ROLE_CODE && u.is_active {
            Self::guard_admin_lockout(&mut repo).await?;
        }

        let affected = repo
            .soft_delete_user_role(r.id, expected_version, now_naive(), Some(current.id))
            .await?;
        if affected == 0 {
            return Err(version_conflict());
        }
        Ok(vec![user_id])
    }

    // =======================================================================
    // 内部
    // =======================================================================

    /// **ADMIN 自锁护栏**（用户域版）：若「当前 active 且持有 ADMIN 的用户数」
    /// ≤ 1（即这次撤销会把它打成 0），返 400。
    ///
    /// 判据（`<= 1`）只有 rbac 侧一处实现，本函数委托过去，避免两处阈值各写一遍
    /// 后漂移；错误形态也走 rbac 侧的 `policy_reject`（HTTP 400，不是
    /// `AppError::validation` 的 422）。
    ///
    /// **先取 `t_role` ADMIN 行的行锁再计数**：护栏是「计数 → 判定 → 写」的复合
    /// 操作，两个并发请求各撤销一个管理员时可能双双数到 2 而放行。行锁让第二个
    /// 事务的计数排在第一个提交之后（READ COMMITTED 逐语句快照）。
    pub(crate) async fn guard_admin_lockout<R: IamRepoTrait>(repo: &mut R) -> Result<(), AppError> {
        repo.lock_admin_role_for_update().await?;
        let n =
            crate::modules::iam::rbac::service::RbacService::count_active_users_holding_admin(repo)
                .await?;
        if n <= 1 {
            return Err(crate::modules::iam::rbac::service::policy_reject(
                "不能撤销最后一个系统管理员的 ADMIN 角色",
            ));
        }
        Ok(())
    }

    /// 校验角色码在 `t_role` 中存在且启用。
    ///
    /// 取代原先「`Role` 枚举 parse 成功即合法」的判据：枚举只覆盖内置 6 项，
    /// 自定义角色（`is_system=false`）必须能被授予。停用（`is_active=false`）
    /// 的角色不可**新**授予 —— 存量授予行不受影响（登录时按 `t_user_role`
    /// 现存行判定）。
    ///
    /// 错误形态：HTTP 400（`policy_reject`）。「角色代码不是可用值」是业务规则
    /// 拒绝而非请求形态错误，与 ADMIN 护栏同一族。
    async fn validate_role_exists<R: IamRepoTrait>(
        repo: &mut R,
        role: &str,
    ) -> Result<(), AppError> {
        let r = repo.get_active_role_by_code(role).await?.ok_or_else(|| {
            crate::modules::iam::rbac::service::policy_reject(format!("角色 {role} 不存在或已停用"))
        })?;
        if r.is_active {
            Ok(())
        } else {
            // get_active_role_by_code 只按 code 过滤 deleted_at；is_active 单独判，
            // 让「存在但停用」与「不存在」返回不同文案便于现场排障。
            Err(crate::modules::iam::rbac::service::policy_reject(format!(
                "角色 {role} 已停用，不能授予"
            )))
        }
    }

    /// 校验 scope 形态、货架存在、zone 白名单、is_active。
    ///
    /// 收 `&mut R` 而非 `&mut uow`：调用方把 repo 借进来，helper 只取 shelf_repo。
    ///
    /// ## ADMIN 与自定义角色的 scope 语义（2026-10-14 补）
    /// - **ADMIN**：不接 scope。与其它非货架角色同规则（scope 必须为空）——
    ///   ADMIN 的「全店通行」由 `has_role` 的元角色通配承担，不靠 `scope_type`；
    /// - **自定义角色（如 `QC_LEAD`）**：不接 scope，scope 填 `NULL`。它们没有
    ///   任何货架绑定语义，`scope_type`/`scope_id` 一律留空。**不要**因为它
    ///   「不在 6 个已知角色里」就套用 SHELF_ACCOUNT 的必填规则 —— 那会让
    ///   自定义角色一律授不进去。
    async fn validate_role_scope<R: IamRepoTrait>(
        repo: &mut R,
        req: &UserAddRoleRequest,
    ) -> Result<(), AppError> {
        // 仅 SHELF_ACCOUNT 走「scope 必填」这一支；其余角色（含自定义）走
        // 「scope 必须为空」。判定用枚举 parse，认不出的自定义码自然落到
        // else 分支 —— 正是想要的语义。
        let known = parse_role_string(&req.role);
        if known == Some(Role::ShelfAccount) {
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
            // MANAGER / ADMIN / CLERK / INSPECTOR / CNC_PROGRAMMER / 自定义角色
            // 一律不接 scope
            return Err(AppError::validation(format!(
                "role {} does not accept scope",
                req.role.as_str()
            )));
        }
        Ok(())
    }
}

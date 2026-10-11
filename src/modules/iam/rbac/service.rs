//! RBAC 子模块 service：角色 CRUD + 授权整包替换 + 权限点 / 菜单树读取
//!
//! ## 8 个方法与 8 条端点一一对应
//! | 方法 | 端点 |
//! |---|---|
//! | [`RbacService::list_roles`] | `GET /api/v2/iam/roles` |
//! | [`RbacService::create_role`] | `POST /api/v2/iam/roles` |
//! | [`RbacService::update_role`] | `POST /api/v2/iam/roles/{id}/update` |
//! | [`RbacService::deactivate_role`] | `POST /api/v2/iam/roles/{id}/deactivate` |
//! | [`RbacService::get_role_grants`] | `GET /api/v2/iam/roles/{id}/grants` |
//! | [`RbacService::set_role_grants`] | `POST /api/v2/iam/roles/{id}/grants` |
//! | [`RbacService::list_permission_groups`] | `GET /api/v2/iam/permissions` |
//! | [`RbacService::list_menus`] | `GET /api/v2/iam/menus` |
//!
//! ## 事务与副作用的分工
//! 事务由 handler `pool.begin()` / `tx.commit()` 收（与全仓 handler 对齐），
//! service 不知事务。**踢下线（`delete_all_user_sessions`）一律由 handler 在
//! commit 之后调** —— Redis 不在 DB 事务里，先踢会出现「session 已清、事务回滚」
//! 的窗口，把仍持有旧授权的用户误踢下线。
//!
//! 每个写方法都返回「本次该踢哪些用户 id」，由 handler 统一执行踢人；
//! service **不接触 Redis**（这样它对事务的依赖是纯 DB 的，单测也能直调）。
//!
//! ## ADMIN 元角色：读全量、写拒绝
//! - `get_role_grants`：`code == "ADMIN"` 时两个数组返**全量** active 菜单码 /
//!   权限码（面板要能整页渲染，返空会让管理员以为「ADMIN 什么都没有」）；
//! - `set_role_grants` / `deactivate_role` / `update_role`（带 code）：一律拒绝。
//!
//! ## 系统角色保护（固定 400，不做计数）
//! - **ADMIN 角色不可停用 / 删除**：停用生效后（登录侧只认 `t_role.is_active =
//!   true` 的角色），所有管理员立即失去唯一能恢复它的元角色 —— count >= 2 时
//!   同样会导致全员降权。删除本就因 `is_system` 被拒，合并为同一条文案；
//! - **MANAGER 角色不可停用 / 删除**：它是本面板的守卫角色（`require_role(Manager)`），
//!   停用后面板对所有 MANAGER 关闭，而当前没有启用端点能把角色重新启回去。
//!   删除同样因 `is_system` 被拒。
//!
//! 其余系统角色与自定义角色可停用（`delete=false`），只有自定义角色可删
//! （`delete=true`）；**停用为终态** —— 当前没有启用（反向）端点，如需恢复
//! 只能改库把 `is_active` 置回 `true`。
//!
//! ## ADMIN 自锁护栏（计数）
//! 三条**计数护栏**在「会让 active 且持有 ADMIN 的用户数归零」时返 400，
//! **且不调** `delete_all_user_sessions`：
//! 1. 撤销用户的 ADMIN 角色（用户域 `remove_role`）；
//! 2. 停用持有 ADMIN 的用户（用户域 `deactivate_user`）；
//! 3. 更新持有 ADMIN 的用户并置 `is_active=false`（用户域 `update_user`）。
//!
//! 判据是「`t_user.is_active` 且 ADMIN 授权行未软删的用户数 <= 1」，且**只在
//! 操作对象就是 ADMIN 时触发** —— 其它角色一律不管，否则管理员连删一个自定义
//! 角色都要先算「还剩几个管理员」，那是产品决策不是护栏。计数与写入之间由用户域
//! 对 `t_role` ADMIN 行的行锁串行化（见 `IamRepoTrait::lock_admin_role_for_update`），
//! 修复「并发撤销两个管理员时双双放行」的窗口。

use std::collections::HashSet;
use std::sync::Arc;

use sqlx::PgConnection;

use crate::auth::rbac::{CurrentUser, Role};
use crate::infra::clock::now_naive;
use crate::infra::snowflake::SnowflakeIdGenerator;
use crate::modules::iam::rbac::dto::{
    CODE_MAX_LEN, RoleCreateRequest, RoleDeactivateRequest, RoleGrantsRequest, RoleUpdateRequest,
    validate_code_shape, validate_name,
};
use crate::modules::iam::rbac::model::{TPermission, TRole};
use crate::modules::iam::rbac::repo::{RbacRepo, RoleInsert, RolePartialUpdate, RoleWithUserCount};
use crate::modules::iam::rbac::vo::{
    PermissionGroup, PermissionItem, RoleDeactivateOut, RoleDef, RoleGrants,
};
use crate::modules::iam::repo::IamRepoTrait;
use crate::modules::iam::repo::sql as iam_repo;
use crate::modules::iam::vo::MenuNodeOut;
use crate::shared::error::{AppError, code};

/// ADMIN 元角色的 code —— 「通配 / 拒写 / 自锁护栏」三处判据的唯一来源。
pub const ADMIN_ROLE_CODE: &str = "ADMIN";

/// MANAGER 角色的 code —— 本面板守卫（`require_role(Manager)`）的角色，
/// 「不可停用 / 删除」固定 400 判据的唯一来源。
pub const MANAGER_ROLE_CODE: &str = "MANAGER";

/// **策略拒绝**（护栏 / 元角色保护）的统一错误构造：HTTP 400 + `40000 BAD_REQUEST`。
///
/// 刻意不用 `AppError::validation`（本仓映射 HTTP **422**）：422 的语义是「请求体
/// 形态不合法」（如缺 `version`、code 过长），而这些拒绝与请求形态无关 —— 请求
/// 完全合法，只是**业务规则**不允许（会自锁 / 元角色不接受授权数据写入）。
/// 用 400 让前端能把它与「表单填错」区分开。
pub(crate) fn policy_reject(msg: impl Into<String>) -> AppError {
    AppError::biz(code::BAD_REQUEST, msg)
}

/// RBAC service。字段仅 `snowflake`（事务与 Redis 副作用都在 handler）。
pub struct RbacService {
    snowflake: Arc<SnowflakeIdGenerator>,
}

/// 写方法的「提交后该踢谁」返回值。
///
/// 与返回值的**粒度**：一个用户 id 列表。handler 逐个
/// `delete_all_user_sessions`。空列表 = 不该踢任何人（handler 仍可安全地
/// 什么都不做）。
pub type KickList = Vec<i64>;

impl RbacService {
    pub fn new(snowflake: Arc<SnowflakeIdGenerator>) -> Self {
        Self { snowflake }
    }

    // =======================================================================
    // C1 GET /api/v2/iam/roles
    // =======================================================================

    /// 角色列表（含派生 `user_count`），排序 `is_system DESC, code ASC`。
    pub async fn list_roles(
        &self,
        conn: &mut PgConnection,
        current: &CurrentUser,
    ) -> Result<Vec<RoleDef>, AppError> {
        current.require_role(Role::Manager)?;
        let rows = RbacRepo::list_roles_with_user_count(&mut *conn).await?;
        Ok(rows.iter().map(to_role_def).collect())
    }

    // =======================================================================
    // C2 POST /api/v2/iam/roles
    // =======================================================================

    /// 新建自定义角色（`is_system=false`，写侧固定）。**不踢任何人**
    /// （新角色还没有持有者）。
    pub async fn create_role(
        &self,
        conn: &mut PgConnection,
        req: &RoleCreateRequest,
        current: &CurrentUser,
    ) -> Result<RoleDef, AppError> {
        current.require_role(Role::Manager)?;

        let code = normalize_code(&req.code)?;
        validate_code_shape(&code)?;
        let name = req.name.trim().to_string();
        validate_name(&name)?;
        let description = trimmed_or_none(req.description.as_deref());

        // 显式查重（partial unique 索引 `uk_t_role_code` 仍是并发兜底）
        if RbacRepo::get_role_by_code(&mut *conn, &code)
            .await?
            .is_some()
        {
            return Err(policy_reject(format!("角色码 {code} 已存在")));
        }

        let id = self.snowflake.next_id();
        RbacRepo::create_role(
            &mut *conn,
            &RoleInsert {
                id,
                code: &code,
                name: &name,
                description: description.as_deref(),
                created_at: now_naive(),
                created_by: Some(current.id),
            },
        )
        .await
        .map_err(map_duplicate_role_code)?;

        Ok(RoleDef {
            id,
            code,
            name,
            description,
            is_system: false,
            is_active: true,
            version: 0,
            user_count: 0,
        })
    }

    // =======================================================================
    // C3 POST /api/v2/iam/roles/{id}/update
    // =======================================================================

    /// 改角色。返回 `KickList` = 本次该被踢的用户。
    ///
    /// **仅 code 变更才踢**：`t_user_role.role` 级联改名后，持有者 Redis session
    /// 缓存里的角色串（`CachedUserProfile.roles`）已与 DB 脱节，必须踢掉重登；
    /// name / description 不进 session 缓存（权限 / 菜单都按 code 组装），改名
    /// 不改码不打断任何人的登录。
    ///
    /// **改 code 会同事务级联三张关联表**（`t_user_role` / `t_role_menu` /
    /// `t_role_permission`），级联会 bump `t_user_role.version`（该行的 OCC 锚点）。
    pub async fn update_role(
        &self,
        conn: &mut PgConnection,
        id: i64,
        req: &RoleUpdateRequest,
        current: &CurrentUser,
    ) -> Result<(RoleDef, KickList), AppError> {
        current.require_role(Role::Manager)?;

        let role = role_or_not_found(RbacRepo::get_role_by_id(&mut *conn, id).await?, id)?;
        let name = req.name.trim().to_string();
        validate_name(&name)?;
        let description = trimmed_or_none(req.description.as_deref());

        // code 省略 = 不改；带了就必须校验形态
        let new_code = match req.code.as_deref() {
            None => None,
            Some(raw) => {
                let c = normalize_code(raw)?;
                validate_code_shape(&c)?;
                // 系统角色**一律**不接受 code 入参 —— 无论是否等于现值。等于现值
                // 时若按「没带」放行，面板与手工调用会误以为系统角色可改名。
                if role.is_system {
                    return Err(policy_reject("系统角色的 code 不可改（名称 / 描述可改）"));
                }
                if c == role.code {
                    None // 与现值相同 —— 不触发级联
                } else {
                    Some(c)
                }
            }
        };

        // 与 create 同口径的显式查重（partial unique 索引仍是并发兜底），撞索引
        // 时经 `map_duplicate_role_code` 翻成 400 而非 500。
        if let Some(c) = &new_code
            && RbacRepo::get_role_by_code(&mut *conn, c).await?.is_some()
        {
            return Err(policy_reject(format!("角色码 {c} 已存在")));
        }

        let affected = RbacRepo::update_role(
            &mut *conn,
            id,
            req.version,
            &RolePartialUpdate {
                code: new_code.as_deref(),
                name: &name,
                description: description.as_deref(),
                updated_at: now_naive(),
                updated_by: Some(current.id),
            },
        )
        .await
        .map_err(map_duplicate_role_code)?;
        if affected == 0 {
            return Err(version_conflict());
        }

        let code_changed = new_code.is_some();
        let final_code = new_code.clone().unwrap_or_else(|| role.code.clone());
        if let Some(new_code) = &new_code {
            RbacRepo::cascade_rename_role_code(
                &mut *conn,
                &role.code,
                new_code,
                now_naive(),
                Some(current.id),
            )
            .await
            .map_err(map_cascade_rename_conflict)?;
        }

        let holders = RbacRepo::list_active_user_ids_by_role_code(&mut *conn, &final_code).await?;
        let kicks = if code_changed {
            holders.clone()
        } else {
            Vec::new()
        };

        Ok((
            RoleDef {
                id: role.id,
                code: final_code,
                name,
                description,
                is_system: role.is_system,
                // ⚠️ deactivate 不会改 is_active，但 update 也不碰它 —— 沿用原值。
                // version 恒 +1（OCC UPDATE 必然发生，affected==0 已提前返回）。
                is_active: role.is_active,
                version: req.version + 1,
                user_count: holders.len() as i64,
            },
            kicks,
        ))
    }

    // =======================================================================
    // C4 POST /api/v2/iam/roles/{id}/deactivate
    // =======================================================================

    /// 停用（`delete=false`）或软删（`delete=true`）角色。
    ///
    /// - `delete=true` 仅 `is_system=false` 可用（系统角色可停用不可删，
    ///   ADMIN / MANAGER 见下一条）；
    /// - `delete=true` 同事务连带软删该角色的 `t_user_role` /
    ///   `t_role_menu` / `t_role_permission`；
    /// - **停用与删除都会踢该角色的全部在册持有者**（`KickList`）：停用即降权，
    ///   登录 / refresh / me 的角色解析只认 `t_role.is_active = true` 的角色；
    ///   不踢则持有者的 session 缓存（roles / permissions / menus）会按旧角色
    ///   继续判定到自然过期；
    /// - `affected_user_count` = 被连带软删的 `t_user_role` 行数（`delete=false`
    ///   恒为 0，但 `KickList` 仍非空）；
    /// - **ADMIN / MANAGER 角色一律 400**（停用 / 删除都不允许，见模块头）；
    /// - 其余角色**停用为终态**：当前没有启用（反向）端点，恢复只能改库。
    pub async fn deactivate_role(
        &self,
        conn: &mut PgConnection,
        id: i64,
        req: &RoleDeactivateRequest,
        current: &CurrentUser,
    ) -> Result<(RoleDeactivateOut, KickList), AppError> {
        current.require_role(Role::Manager)?;

        let role = role_or_not_found(RbacRepo::get_role_by_id(&mut *conn, id).await?, id)?;
        // ADMIN 是元角色、MANAGER 是本面板的守卫角色：停用生效后（登录侧只认
        // is_active 的角色）前者让所有管理员失去唯一能恢复它的元角色，后者让
        // 面板对所有 MANAGER 关闭；两者都没有启用端点能恢复，故不做计数护栏、
        // 一律拒绝。
        if role.code == ADMIN_ROLE_CODE || role.code == MANAGER_ROLE_CODE {
            return Err(policy_reject("系统管理员 / 管理角色不可停用或删除"));
        }
        if req.delete && role.is_system {
            return Err(policy_reject("系统角色不可删除（如需下线请用停用）"));
        }

        let now = now_naive();
        let affected = if req.delete {
            RbacRepo::soft_delete_role(&mut *conn, id, req.version, now, Some(current.id)).await?
        } else {
            RbacRepo::deactivate_role(&mut *conn, id, req.version, now, Some(current.id)).await?
        };
        if affected == 0 {
            return Err(version_conflict());
        }

        // 踢人名单要在连带软删**之前**取：删完之后 `deleted_at IS NULL` 过滤
        // 会把这些人全滤掉。
        let kicks = RbacRepo::list_active_user_ids_by_role_code(&mut *conn, &role.code).await?;

        let mut affected_user_count = 0i64;
        if req.delete {
            affected_user_count = RbacRepo::soft_delete_user_roles_by_role_code(
                &mut *conn,
                &role.code,
                now,
                Some(current.id),
            )
            .await? as i64;
            // 授权行一并软删：不删的话角色复活（同名重建）时会继承旧的授权。
            RbacRepo::soft_delete_all_grants_by_role(&mut *conn, &role.code, now, Some(current.id))
                .await?;
        }

        Ok((
            RoleDeactivateOut {
                affected_user_count,
            },
            kicks,
        ))
    }

    // =======================================================================
    // C5 GET /api/v2/iam/roles/{id}/grants
    // =======================================================================

    /// 读授权整包。ADMIN 返**全量** active 菜单码 / 权限码（见模块头）。
    pub async fn get_role_grants(
        &self,
        conn: &mut PgConnection,
        id: i64,
        current: &CurrentUser,
    ) -> Result<RoleGrants, AppError> {
        current.require_role(Role::Manager)?;

        let role = role_or_not_found(RbacRepo::get_role_by_id(&mut *conn, id).await?, id)?;
        let (menu_codes, permission_codes) = if role.code == ADMIN_ROLE_CODE {
            (
                RbacRepo::list_all_active_menu_codes(&mut *conn).await?,
                RbacRepo::list_all_active_permission_codes(&mut *conn).await?,
            )
        } else {
            (
                RbacRepo::list_menu_codes_by_role(&mut *conn, &role.code).await?,
                RbacRepo::list_permission_codes_by_role(&mut *conn, &role.code).await?,
            )
        };
        Ok(RoleGrants {
            role_id: role.id,
            role_code: role.code,
            is_system: role.is_system,
            version: role.version,
            menu_codes,
            permission_codes,
        })
    }

    // =======================================================================
    // C6 POST /api/v2/iam/roles/{id}/grants
    // =======================================================================

    /// 授权整包替换（先删后插，单事务）。**踢该角色的全部持有者**。
    ///
    /// ⚠️ 踢人是**必须**的，不是顺手为之：授权数据装在 Redis session 缓存里
    ///   （`CachedUserProfile.permissions`），不踢就等于这次提交对所有人无效，
    ///   直到他们自然过期（session TTL 默认 900s）。
    pub async fn set_role_grants(
        &self,
        conn: &mut PgConnection,
        id: i64,
        req: &RoleGrantsRequest,
        current: &CurrentUser,
    ) -> Result<(RoleGrants, KickList), AppError> {
        current.require_role(Role::Manager)?;

        let role = role_or_not_found(RbacRepo::get_role_by_id(&mut *conn, id).await?, id)?;
        if role.code == ADMIN_ROLE_CODE {
            return Err(policy_reject(
                "系统管理员角色是元角色（代码级通配），不接受授权数据写入",
            ));
        }

        // OCC：锚在 t_role.version（与角色信息更新共用同一行）
        let bumped = sqlx::query!(
            r#"
            UPDATE t_role SET version = version + 1, updated_at = $2, updated_by = $3
            WHERE id = $1 AND version = $4 AND deleted_at IS NULL
            "#,
            id,
            now_naive(),
            current.id,
            req.version,
        )
        .execute(&mut *conn)
        .await?
        .rows_affected();
        if bumped == 0 {
            return Err(version_conflict());
        }

        // 入参校验①：重复项**显式拒绝**（整包语义 = 集合，重复项无意义；静默去重
        // 会让前端把「按错」提交成成功）。错误走 40000 policy_reject —— 与「码不
        // 存在 / 已停用」的 422 形态校验分开，前端能区分「表单填错」与「提交里有
        // 重复项」。
        if req.menu_codes.len() != dedup_len(&req.menu_codes) {
            return Err(policy_reject("menu_codes 含重复的菜单码"));
        }
        if req.permission_codes.len() != dedup_len(&req.permission_codes) {
            return Err(policy_reject("permission_codes 含重复的权限码"));
        }

        // 入参校验②：只接受启用中的菜单码 / 权限码（去重 + 排序由 SQL 给出）
        let menu_codes = RbacRepo::filter_active_menu_codes(&mut *conn, &req.menu_codes).await?;
        if menu_codes.len() != req.menu_codes.len() {
            return Err(AppError::validation(
                "menu_codes 含不存在 / 已停用的菜单码".to_string(),
            ));
        }
        let permission_codes =
            RbacRepo::filter_active_permission_codes(&mut *conn, &req.permission_codes).await?;
        if permission_codes.len() != req.permission_codes.len() {
            return Err(AppError::validation(
                "permission_codes 含不存在 / 已停用的权限码".to_string(),
            ));
        }

        let now = now_naive();
        RbacRepo::soft_delete_all_grants_by_role(&mut *conn, &role.code, now, Some(current.id))
            .await?;
        let menu_ids: Vec<i64> = menu_codes
            .iter()
            .map(|_| self.snowflake.next_id())
            .collect();
        RbacRepo::insert_role_menus(
            &mut *conn,
            &role.code,
            &menu_ids,
            &menu_codes,
            now,
            Some(current.id),
        )
        .await?;
        let perm_ids: Vec<i64> = permission_codes
            .iter()
            .map(|_| self.snowflake.next_id())
            .collect();
        RbacRepo::insert_role_permissions(
            &mut *conn,
            &role.code,
            &perm_ids,
            &permission_codes,
            now,
            Some(current.id),
        )
        .await?;

        let kicks = RbacRepo::list_active_user_ids_by_role_code(&mut *conn, &role.code).await?;

        Ok((
            RoleGrants {
                role_id: role.id,
                role_code: role.code,
                is_system: role.is_system,
                version: req.version + 1,
                menu_codes,
                permission_codes,
            },
            kicks,
        ))
    }

    // =======================================================================
    // C7 GET /api/v2/iam/permissions
    // =======================================================================

    /// 权限点清单，按 `domain` 分组（组内 `code ASC`）。只含启用中的。
    pub async fn list_permission_groups(
        &self,
        conn: &mut PgConnection,
        current: &CurrentUser,
    ) -> Result<Vec<PermissionGroup>, AppError> {
        current.require_role(Role::Manager)?;
        let perms = RbacRepo::list_active_permissions(&mut *conn).await?;
        Ok(group_by_domain(perms))
    }

    // =======================================================================
    // C8 GET /api/v2/iam/menus
    // =======================================================================

    /// 菜单树，**全量启用中菜单**。出参与 `GET /iam/me` 的 `menus` **逐字段
    /// 同形**（复用同一个 `MenuNodeOut` + 同一个 `build_menu_tree` 纯函数）。
    ///
    /// 这是管理员的**配置界面**用树：必须能看到全部菜单（含自己没被授权的，
    /// 如只授 `SHELF_ACCOUNT` 的 `scan_badge`），否则无法给任何角色勾选 / 收回。
    /// 可见范围已由 `require_role(Manager)` 收口，不再按调用者的角色授权收窄。
    pub async fn list_menus(
        &self,
        conn: &mut PgConnection,
        current: &CurrentUser,
    ) -> Result<Vec<MenuNodeOut>, AppError> {
        current.require_role(Role::Manager)?;

        let menus = iam_repo::list_all_active_menus(&mut *conn).await?;
        Ok(crate::modules::iam::service::build_menu_tree(menus))
    }

    // =======================================================================
    // 内部 helper
    // =======================================================================

    /// ADMIN 自锁护栏判据的**trait 泛型版**（供用户域 `AccountService` 调用）。
    ///
    /// 用户域的 `AccountService` 方法签名收 `R: IamRepoTrait`（为了单测可注入
    /// mock），不能直接吃 `&mut PgConnection`。判据是
    /// `IamRepoTrait::list_active_admin_user_ids`（委托到
    /// `RbacRepo::list_active_user_ids_by_role_code`）；阈值 `<= 1` 与行锁收在
    /// 用户域的 `AccountService::guard_admin_lockout` 一处 —— 用户域的降权入口
    /// （撤销 ADMIN / 停用账号 / 更新账号置停用）共用同一 guard，rbac 侧不再计数。
    pub(crate) async fn count_active_users_holding_admin<R: IamRepoTrait>(
        repo: &mut R,
    ) -> Result<i64, AppError> {
        Ok(repo.list_active_admin_user_ids().await?.len() as i64)
    }
}

// ===========================================================================
// 模块内纯函数
// ===========================================================================

fn normalize_code(raw: &str) -> Result<String, AppError> {
    let c = raw.trim().to_uppercase();
    if c.chars().count() > CODE_MAX_LEN {
        return Err(AppError::validation(format!(
            "code 长度须 ≤ {CODE_MAX_LEN}"
        )));
    }
    Ok(c)
}

fn trimmed_or_none(v: Option<&str>) -> Option<String> {
    v.map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn role_or_not_found(role: Option<TRole>, id: i64) -> Result<TRole, AppError> {
    role.ok_or_else(|| AppError::biz(code::NOT_FOUND, format!("角色 {id} 不存在")))
}

fn to_role_def(r: &RoleWithUserCount) -> RoleDef {
    RoleDef {
        id: r.id,
        code: r.code.clone(),
        name: r.name.clone(),
        description: r.description.clone(),
        is_system: r.is_system,
        is_active: r.is_active,
        version: r.version,
        user_count: r.user_count,
    }
}

/// 去重后的长度（入参可能重复，SQL 只返一次）。
fn dedup_len(codes: &[String]) -> usize {
    codes.iter().collect::<HashSet<_>>().len()
}

fn group_by_domain(perms: Vec<TPermission>) -> Vec<PermissionGroup> {
    let mut groups: Vec<PermissionGroup> = Vec::new();
    for p in perms {
        let item = PermissionItem {
            code: p.code,
            name: p.name,
            description: p.description,
            is_active: p.is_active,
        };
        match groups.iter_mut().find(|g| g.domain == p.domain) {
            Some(g) => g.items.push(item),
            None => groups.push(PermissionGroup {
                domain: p.domain,
                items: vec![item],
            }),
        }
    }
    groups
}

/// 唯一索引兜底：并发建同名角色 / 改名撞 `uk_t_role_code` 时翻成 400 而非 500。
///
/// 用 `policy_reject`（40000 → HTTP 400）与显式查重同码：重复的角色码是业务规则
/// 拒绝，不是请求形态错（计划口径要的是 400，不是 422）。
fn map_duplicate_role_code(e: sqlx::Error) -> AppError {
    if matches!(&e, sqlx::Error::Database(db) if db.code().as_deref() == Some("23505")) {
        policy_reject("角色码已存在")
    } else {
        AppError::from(e)
    }
}

/// 改名级联撞唯一索引（数据异常）时翻成明确 400 而非 500。
///
/// 正常路径下新 code 上不会已有同型授权行（`get_role_by_code` 预检 + 唯一索引
/// 兜底），但 `uk_t_user_role_user_role_scope` **不是 partial 索引**：软删的历史
/// 行仍占索引位，级联 UPDATE 改出来的一行可能撞上它（`t_role_menu` /
/// `t_role_permission` 同理）。这类冲突没有可自动收敛的方向，明确拒绝并提示先清理。
fn map_cascade_rename_conflict(e: sqlx::Error) -> AppError {
    if matches!(&e, sqlx::Error::Database(db) if db.code().as_deref() == Some("23505")) {
        policy_reject("角色码改名冲突：目标 code 上已存在同型授权行（数据异常），请先排查重复数据")
    } else {
        AppError::from(e)
    }
}

fn version_conflict() -> AppError {
    AppError::biz(code::VERSION_CONFLICT, "数据已被他人修改，请刷新后重试")
}

// ===========================================================================
// 纯函数单测
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::{group_by_domain, normalize_code, validate_code_shape, validate_name};
    use crate::modules::iam::rbac::model::TPermission;
    use chrono::NaiveDate;

    fn perm(code: &str, domain: &str) -> TPermission {
        let now = NaiveDate::from_ymd_opt(2026, 10, 14)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap();
        TPermission {
            id: 1,
            code: code.into(),
            name: "n".into(),
            domain: domain.into(),
            description: None,
            is_active: true,
            version: 0,
            created_at: now,
            created_by: None,
            updated_at: now,
            updated_by: None,
            deleted_at: None,
        }
    }

    #[test]
    fn code_is_trimmed_and_upper_cased() {
        assert_eq!(normalize_code("  qc_lead ").unwrap(), "QC_LEAD");
        assert!(normalize_code("Q").is_ok());
    }

    #[test]
    fn code_shape_rejects_non_upper_alnum_underscore() {
        assert!(validate_code_shape("QC_LEAD").is_ok());
        assert!(validate_code_shape("QC-LEAD").is_err());
        assert!(validate_code_shape("品检组长").is_err());
        assert!(validate_code_shape("").is_err());
        // 超长（列宽 20）
        assert!(validate_code_shape("A".repeat(21).as_str()).is_err());
    }

    #[test]
    fn name_must_be_non_empty_after_trim() {
        assert!(validate_name(" 文员 ").is_ok());
        assert!(validate_name("   ").is_err());
    }

    #[test]
    fn permissions_group_by_domain_in_first_seen_order() {
        let groups = group_by_domain(vec![
            perm("part.drawing.print", "part"),
            perm("queue.dispatch", "queue"),
            perm("part.new", "part"),
        ]);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].domain, "part");
        assert_eq!(groups[0].items.len(), 2);
        assert_eq!(groups[1].domain, "queue");
        assert_eq!(groups[1].items.len(), 1);
    }
}

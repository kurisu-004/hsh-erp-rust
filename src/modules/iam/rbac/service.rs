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
//! - `set_role_grants` / `deactivate_role` / `update_role`（改 code）：一律拒绝。
//!
//! ## 三条「ADMIN 自锁护栏」
//! 下面三种操作若会让「active 且持有 ADMIN 的用户数」归零 → 400，**且不调**
//! `delete_all_user_sessions`：
//! 1. 停用 / 删除 ADMIN 角色本身；
//! 2. 撤销用户的 ADMIN 角色（BE-4，用户域）；
//! 3. 停用持有 ADMIN 的用户（BE-4，用户域）。
//!
//! 判据都是 [`Self::count_active_users_holding`]：统计 `t_user.is_active AND
//! t_user_role.role = <code> AND t_user_role.deleted_at IS NULL`。**护栏只在
//! 「操作对象就是 ADMIN」时触发**，其它角色一律不管 —— 否则管理员连删一个
//! 自定义角色都要先算「还剩几个管理员」，那是产品决策不是护栏。

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
            return Err(AppError::validation(format!("角色码 {code} 已存在")));
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

    /// 改角色。返回 `KickList` = 本次该被踢的用户（改名或改描述时）。
    ///
    /// **改 code 会同事务级联三张关联表**（`t_user_role` / `t_role_menu` /
    /// `t_role_permission`），级联完再踢该角色的全部持有者 —— 不踢的话，持有者
    /// 的 session 缓存里仍是旧 role code 的一份数据。
    ///
    /// ⚠️ 改 name / description **也踢**：OCC 锚在 `t_role.version`，而
    /// `grants` 的整包替换同样 bump 这个 version。不踢的后果是「A 改完名字，
    /// B 的面板还按旧 version 提交 grants 却被拒」，且 B 完全不知道自己被拒的
    /// 原因（他没改任何授权）。
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
                if c == role.code {
                    None // 与现值相同 —— 不触发级联，也不算「带了 code」
                } else if role.is_system {
                    return Err(policy_reject(
                        "系统角色的 code 不可修改（可停用，不可改名）",
                    ));
                } else {
                    Some(c)
                }
            }
        };

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
        .await?;
        if affected == 0 {
            return Err(version_conflict());
        }

        let final_code = new_code.clone().unwrap_or_else(|| role.code.clone());
        if let Some(new_code) = &new_code {
            RbacRepo::cascade_rename_role_code(
                &mut *conn,
                &role.code,
                new_code,
                now_naive(),
                Some(current.id),
            )
            .await?;
        }

        let kicks = RbacRepo::list_active_user_ids_by_role_code(&mut *conn, &final_code).await?;

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
                user_count: kicks.len() as i64,
            },
            kicks,
        ))
    }

    // =======================================================================
    // C4 POST /api/v2/iam/roles/{id}/deactivate
    // =======================================================================

    /// 停用（`delete=false`）或软删（`delete=true`）角色。
    ///
    /// - `delete=true` 仅 `is_system=false` 可用（系统角色可停用不可删）；
    /// - `delete=true` 同事务连带软删该角色的 `t_user_role` /
    ///   `t_role_menu` / `t_role_permission`；
    /// - `affected_user_count` = 被连带软删的 `t_user_role` 行数 = 本次该踢的
    ///   用户数；
    /// - ADMIN 角色：走护栏（见 [`Self::count_active_users_holding`]）。
    pub async fn deactivate_role(
        &self,
        conn: &mut PgConnection,
        id: i64,
        req: &RoleDeactivateRequest,
        current: &CurrentUser,
    ) -> Result<(RoleDeactivateOut, KickList), AppError> {
        current.require_role(Role::Manager)?;

        let role = role_or_not_found(RbacRepo::get_role_by_id(&mut *conn, id).await?, id)?;
        if req.delete && role.is_system {
            return Err(policy_reject("系统角色不可删除（如需下线请用停用）"));
        }
        if role.code == ADMIN_ROLE_CODE {
            Self::guard_admin_lockout(conn, "系统管理员角色不可停用或删除").await?;
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

        // 入参校验：只接受启用中的菜单码 / 权限码（去重 + 排序由 SQL 给出）
        let menu_codes = RbacRepo::filter_active_menu_codes(&mut *conn, &req.menu_codes).await?;
        if menu_codes.len() != dedup_len(&req.menu_codes) {
            return Err(AppError::validation(
                "menu_codes 含不存在 / 已停用 / 重复的菜单码".to_string(),
            ));
        }
        let permission_codes =
            RbacRepo::filter_active_permission_codes(&mut *conn, &req.permission_codes).await?;
        if permission_codes.len() != dedup_len(&req.permission_codes) {
            return Err(AppError::validation(
                "permission_codes 含不存在 / 已停用 / 重复的权限码".to_string(),
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

    /// 菜单树。出参与 `GET /iam/me` 的 `menus` **逐字段同形**（复用同一个
    /// `MenuNodeOut` + 同一个 `build_menu_tree` 纯函数）。
    ///
    /// ADMIN → 全量启用中菜单；其余 → 本人角色被授权的菜单。
    /// 这里读的是**调用者自己**的菜单（不是被编辑角色的），因为端点是
    /// 「管理员配置界面上要显示一棵完整的菜单勾选树」。
    pub async fn list_menus(
        &self,
        conn: &mut PgConnection,
        current: &CurrentUser,
    ) -> Result<Vec<MenuNodeOut>, AppError> {
        current.require_role(Role::Manager)?;

        let menus = if current.has_role(Role::Admin) {
            iam_repo::list_all_active_menus(&mut *conn).await?
        } else {
            let role_strs: Vec<String> = current
                .roles
                .iter()
                .map(|r| r.as_str().to_string())
                .collect();
            iam_repo::list_active_menus_by_roles(&mut *conn, &role_strs).await?
        };
        Ok(crate::modules::iam::service::build_menu_tree(menus))
    }

    // =======================================================================
    // 内部 helper
    // =======================================================================

    /// **ADMIN 自锁护栏**：若「当前 active 且持有 ADMIN 的用户数」≤ 1（即这次
    /// 操作会把它打成 0），返 400。
    ///
    /// 调用方已确认「操作对象与 ADMIN 有关」，本函数只数数。阈值取 `<= 1`：
    /// 只有一个管理员时任何降级动作都会归零，两个及以上时降级到 1 不构成自锁
    /// （还剩人能恢复）。
    pub(crate) async fn guard_admin_lockout(
        conn: &mut PgConnection,
        reason: &str,
    ) -> Result<(), AppError> {
        let n = Self::count_active_users_holding(conn, ADMIN_ROLE_CODE).await?;
        if n <= 1 {
            return Err(policy_reject(reason.to_string()));
        }
        Ok(())
    }

    /// active 且持有某角色的用户数（`t_user.is_active AND 授权行未软删`）。
    pub(crate) async fn count_active_users_holding(
        conn: &mut PgConnection,
        role_code: &str,
    ) -> Result<i64, AppError> {
        let ids = RbacRepo::list_active_user_ids_by_role_code(&mut *conn, role_code).await?;
        Ok(ids.len() as i64)
    }

    /// ADMIN 自锁护栏判据的**trait 泛型版**（供用户域 `AccountService` 调用）。
    ///
    /// 用户域的 `AccountService` 方法签名收 `R: IamRepoTrait`（为了单测可注入
    /// mock），不能直接吃 `&mut PgConnection`。两边走的是**同一条 SQL**
    /// （`IamRepoTrait::list_active_admin_user_ids` 委托到
    /// `RbacRepo::list_active_user_ids_by_role_code`）与**同一个阈值**（`<= 1`）。
    ///
    /// 为什么要单点：两处各写一遍阈值后漂移的表现是「角色域拦得住、用户域拦
    /// 不住」，而后者恰好是最容易自锁的那条路（撤销最后一个管理员的 ADMIN）。
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

/// 唯一索引兜底：并发建同名角色时翻成 400 而非 500。
fn map_duplicate_role_code(e: sqlx::Error) -> AppError {
    if matches!(&e, sqlx::Error::Database(db) if db.code().as_deref() == Some("23505")) {
        AppError::validation("角色码已存在".to_string())
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

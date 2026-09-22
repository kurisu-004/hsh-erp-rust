//! shelf 域 CRUD service
//!
//! 列表 / 详情 / 创建 / 更新 / 软删（deactivate）—— 共 5 个端点。
//!
//! ## 业务约束（service 层 enforce）
//! - `zone` ∈ {PRODUCTION, INSPECTION}
//! - `code` 业务唯一键，update 不允许改
//! - `deactivate` 等价于 soft-delete：同时 `is_active = false` + `deleted_at = now()`
//! - `deactivate` 前查 `t_part.current_holder_id = shelf_id` 且
//!   `status IN ('IN_PROCESS','INSPECTION','REPAIRING')` 引用，>0 ⇒ 20503 拒
//!
//! ## picker 端点
//! 见同级 `service::picker`（for-return / for-inspection / 全集 process 映射）。
//!
//! ## mapping 端点
//! 见 `crate::modules::shelf::process_mapping`（per-shelf set/list）。
//!
//! ## 事务边界（2026-09-22 重构对齐 iam 范本）
//! 事务移交 handler（与 20 个 handler 文件现状对齐）：service 仅业务逻辑，所有
//! 跨 repo 操作经 `repo: R`（by-value；`R: ShelfRepo`）参数传入——handler/service
//! 借 `&mut *tx` / `&mut *conn` 喂给 `ShelfRepo` trait（trait 已直接
//! `impl for &mut PgConnection`）。service 不知事务——handler `pool.begin()` +
//! `tx.commit()` 包外。
//!
//! `ShelfService` 是 unit struct（无字段依赖，iam 范本 §6）；方法签名
//! `<R: ShelfRepoTrait>(&self, mut repo: R, ...)`，生产 `R = &mut PgConnection`。

use crate::auth::rbac::{CurrentUser, Role};
use crate::shared::error::{AppError, code};

use super::super::dto::*;
use super::super::model::TShelf;
use super::super::vo::*;
use super::super::repo::ShelfRepoTrait;
use super::{DEFAULT_LIMIT, MAX_LIMIT, ZONE_INSPECTION, ZONE_PRODUCTION};

fn shelf_not_found() -> AppError {
    AppError::biz(code::BIZ_SHELF_NOT_FOUND, "货架不存在")
}

fn version_conflict() -> AppError {
    AppError::biz(code::VERSION_CONFLICT, "数据已被他人修改，请刷新后重试")
}

/// 把 `TShelf` 转 `ShelfOut`。`account_count` 由 caller 在 list 时统一 GROUP BY
/// 批量补全；单条 get 不查 account_count（默认 0）。
fn to_shelf_out(s: TShelf, account_count: i64) -> ShelfOut {
    ShelfOut {
        id: s.id,
        code: s.code,
        name: s.name,
        zone: s.zone,
        location: s.location,
        is_active: s.is_active,
        display_order: s.display_order,
        account_count,
        version: s.version,
        created_at: s.created_at,
        updated_at: s.updated_at,
    }
}

/// 校验 `zone` ∈ {PRODUCTION, INSPECTION}；返回规范化大写字符串。
fn check_zone(s: &str) -> Result<String, AppError> {
    let upper = s.trim().to_uppercase();
    match upper.as_str() {
        ZONE_PRODUCTION | ZONE_INSPECTION => Ok(upper),
        _ => Err(AppError::biz(
            code::BIZ_INVALID_VALUE,
            "zone 必须是 PRODUCTION 或 INSPECTION",
        )),
    }
}

pub struct ShelfService;

impl ShelfService {
    // =======================================================================
    // 列表 / 详情
    // =======================================================================

    pub async fn list_shelves<R: ShelfRepoTrait>(
        &self,
        mut repo: R,
        query: &ShelfListQuery,
        current: &CurrentUser,
    ) -> Result<ShelfListOut, AppError> {
        current.require_any_role(&[
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
        let zone = query
            .zone
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());

        let items = repo
            .list_with_filters(code_like, zone, query.is_active, limit, offset)
            .await?;
        let total = repo
            .count_with_filters(code_like, zone, query.is_active)
            .await?;

        // account_count 单条 GROUP BY 批量算（防 N+1）
        let ids: Vec<i64> = items.iter().map(|s| s.id).collect();
        let account_rows = repo.count_accounts_by_shelf(&ids).await?;
        let account_map: std::collections::HashMap<i64, i64> = account_rows.into_iter().collect();

        let out_items = items
            .into_iter()
            .map(|s| {
                let count = account_map.get(&s.id).copied().unwrap_or(0);
                to_shelf_out(s, count)
            })
            .collect();

        Ok(ShelfListOut {
            items: out_items,
            total,
            limit,
            offset,
        })
    }

    pub async fn get_shelf<R: ShelfRepoTrait>(
        &self,
        mut repo: R,
        id: i64,
        current: &CurrentUser,
    ) -> Result<ShelfOut, AppError> {
        current.require_any_role(&[
            Role::Manager,
            Role::Clerk,
            Role::CncProgrammer,
            Role::ShelfAccount,
            Role::Inspector,
        ])?;

        let s = repo
            .get_by_id(id)
            .await?
            .ok_or_else(shelf_not_found)?;

        if !current.can_access_shelf(s.id) {
            return Err(AppError::biz(
                code::SHELF_MISMATCH,
                format!("无权访问 shelf {}", s.id),
            ));
        }

        // 单条 get 不批量算 account_count（默认 0）。若 caller 需要，由 list 端点补全。
        Ok(to_shelf_out(s, 0))
    }

    // =======================================================================
    // 创建 / 更新 / 软删（deactivate）
    // =======================================================================

    pub async fn create_shelf<R: ShelfRepoTrait>(
        &self,
        mut repo: R,
        snowflake: &crate::infra::snowflake::SnowflakeIdGenerator,
        req: &ShelfCreateRequest,
        current: &CurrentUser,
    ) -> Result<ShelfOut, AppError> {
        current.require_role(Role::Manager)?;

        let code = req.code.trim();
        if code.is_empty() {
            return Err(AppError::biz(code::BIZ_INVALID_VALUE, "code 不能为空"));
        }
        let name = req.name.trim();
        if name.is_empty() {
            return Err(AppError::biz(code::BIZ_INVALID_VALUE, "name 不能为空"));
        }
        let zone = check_zone(&req.zone)?;
        let location = req
            .location
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let display_order = req.display_order.unwrap_or(0);

        let id = snowflake.next_id();
        let s = repo
            .create(id, code, name, &zone, location, display_order, current.id)
            .await
            .map_err(
                |e| match e.as_database_error().and_then(|d| d.code()).as_deref() {
                    // uk_t_shelf_code：活跃行唯一
                    Some("23505") => AppError::biz(
                        code::BIZ_SHELF_DUPLICATE_CODE,
                        format!("code '{code}' 已被占用"),
                    ),
                    _ => AppError::from(e),
                },
            )?;

        Ok(to_shelf_out(s, 0))
    }

    pub async fn update_shelf<R: ShelfRepoTrait>(
        &self,
        mut repo: R,
        id: i64,
        req: &ShelfUpdateRequest,
        current: &CurrentUser,
    ) -> Result<ShelfOut, AppError> {
        current.require_role(Role::Manager)?;

        let current_shelf = repo
            .get_by_id(id)
            .await?
            .ok_or_else(shelf_not_found)?;

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

        // location 三态：None ⇒ 不改；Some(None) ⇒ 清空；Some(Some(v)) ⇒ 改
        let new_loc_owned: Option<String>;
        let loc_update: Option<Option<&str>> = match &req.location {
            None => None,
            Some(None) => Some(None),
            Some(Some(s)) => {
                let trimmed = s.trim();
                new_loc_owned = Some(trimmed.to_string());
                Some(Some(new_loc_owned.as_deref().unwrap()))
            }
        };

        let affected = repo
            .update(
                id,
                current_shelf.version,
                name_update,
                loc_update,
                req.display_order,
                current.id,
            )
            .await
            .map_err(
                |e| match e.as_database_error().and_then(|d| d.code()).as_deref() {
                    // zone CHECK（理论 service 已 catch）
                    Some("23514") => AppError::biz(
                        code::BIZ_INVALID_VALUE,
                        "zone 必须是 PRODUCTION 或 INSPECTION",
                    ),
                    _ => AppError::from(e),
                },
            )?;
        if affected == 0 {
            return Err(version_conflict());
        }

        // 回读最新行
        self.get_shelf(repo, id, current).await
    }

    /// 软删 + 停用（`is_active = false` 同时 `deleted_at = now()`）。
    /// 软删前查 `t_part.current_holder_id = shelf_id` 且
    /// `status IN ('IN_PROCESS','INSPECTION','REPAIRING')` 引用，>0 ⇒ 20503 拒。
    pub async fn soft_delete_shelf<R: ShelfRepoTrait>(
        &self,
        mut repo: R,
        id: i64,
        current: &CurrentUser,
    ) -> Result<(), AppError> {
        current.require_role(Role::Manager)?;

        let shelf = repo
            .get_by_id(id)
            .await?
            .ok_or_else(shelf_not_found)?;

        let in_use = repo.count_in_use_parts(id).await?;
        if in_use > 0 {
            return Err(AppError::biz(
                code::BIZ_SHELF_IN_USE,
                format!(
                    "货架 {} (id={}) 仍被 {in_use} 个 IN_PROCESS/INSPECTION/REPAIRING 零件引用，无法软删",
                    shelf.code, shelf.id
                ),
            ));
        }

        let affected = repo.soft_delete(id, shelf.version, current.id).await?;
        if affected == 0 {
            return Err(version_conflict());
        }
        Ok(())
    }
}
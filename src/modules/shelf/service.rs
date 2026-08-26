//! shelf 域业务逻辑
//!
//! 对应 Python myERP/service/shelf_service.py。实施约定：方法签名接收
//! `&mut PgConnection`，由 handler 开 tx 并 commit。
//!
//! ## 业务约束（service 层 enforce）
//! - `zone` ∈ {PRODUCTION, INSPECTION}
//! - `code` 业务唯一键，update 不允许改
//! - `deactivate` 等价于 soft-delete：同时 `is_active = false` + `deleted_at = now()`
//! - `deactivate` 前查 `t_part.current_holder_id = shelf_id` 且
//!   `status IN ('IN_PROCESS','INSPECTION','REPAIRING')` 引用，>0 ⇒ 20503 拒
//! - `list_for_return` SHELF_ACCOUNT scope：用 `user.can_access_shelf` 限制可见
//!   货架集；最低 current_load 标 `is_recommended`
//!
//! ## mapping 端点
//! `set_shelf_processes` / `list_shelf_processes` / `list_all_process_mappings` /
//! `list_for_return` 中 process_id 映射的子模块职责抽到
//! `crate::modules::shelf::process_mapping`，避免本文件超 1000 行硬上限。

use sqlx::PgConnection;

use crate::auth::rbac::{CurrentUser, Role};
use crate::infra::snowflake::SnowflakeIdGenerator;
use crate::modules::process::repo::ProcessRepo;
use crate::shared::error::{code, AppError};

use super::dto::*;
use super::model::TShelf;
use super::process_mapping::ShelfProcessRepo;
use super::repo::{ShelfRepo, TShelfWithLoad};

const DEFAULT_LIMIT: i64 = 50;
const MAX_LIMIT: i64 = 500;

const ZONE_PRODUCTION: &str = "PRODUCTION";
const ZONE_INSPECTION: &str = "INSPECTION";

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

    pub async fn list_shelves(
        conn: &mut PgConnection,
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

        let items =
            ShelfRepo::list_with_filters(&mut *conn, code_like, zone, query.is_active, limit, offset)
                .await?;
        let total =
            ShelfRepo::count_with_filters(&mut *conn, code_like, zone, query.is_active).await?;

        // account_count 单条 GROUP BY 批量算（防 N+1）
        let ids: Vec<i64> = items.iter().map(|s| s.id).collect();
        let account_rows = ShelfRepo::count_accounts_by_shelf(&mut *conn, &ids).await?;
        let account_map: std::collections::HashMap<i64, i64> =
            account_rows.into_iter().collect();

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

    pub async fn get_shelf(
        conn: &mut PgConnection,
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

        let s = ShelfRepo::get_by_id(&mut *conn, id)
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

    pub async fn create_shelf(
        conn: &mut PgConnection,
        snowflake: &SnowflakeIdGenerator,
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
        let s = ShelfRepo::create(
            &mut *conn,
            id,
            code,
            name,
            &zone,
            location,
            display_order,
            current.id,
        )
        .await
        .map_err(|e| match e.as_database_error().and_then(|d| d.code()).as_deref() {
            // uk_t_shelf_code：活跃行唯一
            Some("23505") => {
                AppError::biz(code::BIZ_SHELF_DUPLICATE_CODE, format!("code '{code}' 已被占用"))
            }
            _ => AppError::from(e),
        })?;

        Ok(to_shelf_out(s, 0))
    }

    pub async fn update_shelf(
        conn: &mut PgConnection,
        id: i64,
        req: &ShelfUpdateRequest,
        current: &CurrentUser,
    ) -> Result<ShelfOut, AppError> {
        current.require_role(Role::Manager)?;

        let current_shelf = ShelfRepo::get_by_id(&mut *conn, id)
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

        let affected = ShelfRepo::update(
            &mut *conn,
            id,
            current_shelf.version,
            name_update,
            loc_update,
            req.display_order,
            current.id,
        )
        .await
        .map_err(|e| match e.as_database_error().and_then(|d| d.code()).as_deref() {
            // zone CHECK（理论 service 已 catch）
            Some("23514") => AppError::biz(
                code::BIZ_INVALID_VALUE,
                "zone 必须是 PRODUCTION 或 INSPECTION",
            ),
            _ => AppError::from(e),
        })?;
        if affected == 0 {
            return Err(version_conflict());
        }

        // 回读最新行
        Self::get_shelf(conn, id, current).await
    }

    /// 软删 + 停用（`is_active = false` 同时 `deleted_at = now()`）。
    /// 软删前查 `t_part.current_holder_id = shelf_id` 且
    /// `status IN ('IN_PROCESS','INSPECTION','REPAIRING')` 引用，>0 ⇒ 20503 拒。
    pub async fn soft_delete_shelf(
        conn: &mut PgConnection,
        id: i64,
        current: &CurrentUser,
    ) -> Result<(), AppError> {
        current.require_role(Role::Manager)?;

        let shelf = ShelfRepo::get_by_id(&mut *conn, id)
            .await?
            .ok_or_else(shelf_not_found)?;

        let in_use = ShelfRepo::count_in_use_parts(&mut *conn, id).await?;
        if in_use > 0 {
            return Err(AppError::biz(
                code::BIZ_SHELF_IN_USE,
                format!(
                    "货架 {} (id={}) 仍被 {in_use} 个 IN_PROCESS/INSPECTION/REPAIRING 零件引用，无法软删",
                    shelf.code, shelf.id
                ),
            ));
        }

        let affected = ShelfRepo::soft_delete(&mut *conn, id, shelf.version, current.id).await?;
        if affected == 0 {
            return Err(version_conflict());
        }
        Ok(())
    }

    // =======================================================================
    // Picker 端点
    // =======================================================================

    /// `GET /shelves/for-return?next_process_id=`：PRODUCTION 区活跃货架按
    /// `current_load` 升序，SHELF_ACCOUNT scope 仅看到 user.shelf_ids 绑定的
    /// 货架（用 `can_access_shelf`），Manager 见全集；最空货架标 `is_recommended`。
    ///
    /// `next_process_id` 仅占位校验：service 校验 process 存在即可（不强制该货架
    /// 必映射此 process —— picker 前端会把 next_process_id 与候选 shelf 一并
    /// 提交给 worker-scan，由 worker-scan 在后端强校验）。
    pub async fn list_for_return(
        conn: &mut PgConnection,
        query: &ShelfForReturnQuery,
        current: &CurrentUser,
    ) -> Result<ShelfForReturnOut, AppError> {
        current.require_any_role(&[
            Role::Manager,
            Role::Clerk,
            Role::ShelfAccount,
            Role::CncProgrammer,
        ])?;

        // 校验 next_process_id 存在（若有）
        let next_pid_opt = match query.next_process_id.as_deref().map(str::trim) {
            Some(s) if !s.is_empty() => {
                let pid = s.parse::<i64>().map_err(|_| {
                    AppError::biz(
                        code::BIZ_INVALID_VALUE,
                        "next_process_id 必须为雪花 ID 字符串",
                    )
                })?;
                if ProcessRepo::get_by_id(&mut *conn, pid, false).await?.is_none() {
                    return Err(AppError::biz(
                        code::BIZ_PROCESS_NOT_FOUND,
                        format!("process {pid} 不存在"),
                    ));
                }
                Some(pid)
            }
            _ => None,
        };
        let _ = next_pid_opt; // 占位校验（service 契约）

        // SHELF_ACCOUNT scope：先按 user.shelf_ids 收窄。
        // 策略：拉全集 → 过滤 → 按 current_load 排序（统一一次拉取，避免 ORDER BY
        // 在 PG 与 filter 在 app 不同步；量小，几十条以内）。
        let all = ShelfRepo::list_active_production_ordered(&mut *conn).await?;
        let scoped: Vec<TShelfWithLoad> = if current.shelf_wildcard || current.has_role(Role::Manager)
        {
            all
        } else {
            all.into_iter()
                .filter(|s| current.can_access_shelf(s.id))
                .collect()
        };

        let mut items: Vec<ShelfForReturnItem> = scoped
            .into_iter()
            .map(|s| ShelfForReturnItem {
                id: s.id,
                code: s.code,
                name: s.name,
                zone: s.zone,
                location: s.location,
                current_load: s.current_load,
                is_recommended: false, // 后面再标
            })
            .collect();

        // 第一条 = 最低 current_load ⇒ is_recommended
        if let Some(first) = items.first_mut() {
            first.is_recommended = true;
        }

        Ok(ShelfForReturnOut { items })
    }

    /// `GET /shelves/for-inspection`：仅 `zone='INSPECTION' AND is_active=true`。
    /// 不过滤 SHELF_ACCOUNT scope（品检架通常由全员可见）。
    pub async fn list_for_inspection(
        conn: &mut PgConnection,
        current: &CurrentUser,
    ) -> Result<ShelfForInspectionOut, AppError> {
        // 任意已登录
        current.require_any_role(&[
            Role::Manager,
            Role::Clerk,
            Role::CncProgrammer,
            Role::ShelfAccount,
            Role::Inspector,
        ])?;

        // 直接复用 list_with_filters，zone='INSPECTION' AND is_active=true
        let shelves = ShelfRepo::list_with_filters(
            &mut *conn,
            None,
            Some(ZONE_INSPECTION),
            Some(true),
            MAX_LIMIT,
            0,
        )
        .await?;
        let items = shelves
            .into_iter()
            .map(|s| ShelfForInspectionItem {
                id: s.id,
                code: s.code,
                name: s.name,
                zone: s.zone,
                location: s.location,
                is_active: s.is_active,
            })
            .collect();
        Ok(ShelfForInspectionOut { items })
    }

    // =======================================================================
    // mapping 端点（per-shelf + 全集）
    // =======================================================================

    /// `GET /shelves/processes`：所有 active shelf 的全部 mapping，单条 SQL
    /// JOIN（防 N+1）。任意已登录可调。
    pub async fn list_all_process_mappings(
        conn: &mut PgConnection,
        current: &CurrentUser,
    ) -> Result<AllShelfProcessMappingOut, AppError> {
        current.require_any_role(&[
            Role::Manager,
            Role::Clerk,
            Role::CncProgrammer,
            Role::ShelfAccount,
            Role::Inspector,
        ])?;

        let rows = ShelfProcessRepo::list_all_active_mappings(&mut *conn).await?;

        // SHELF_ACCOUNT scope：仅保留 user.shelf_ids 命中的映射
        let items: Vec<AllShelfProcessMappingItem> = if current.shelf_wildcard
            || current.has_role(Role::Manager)
        {
            rows.into_iter()
                .map(|(sid, pid, sc, pc)| AllShelfProcessMappingItem {
                    shelf_id: sid,
                    shelf_code: sc,
                    process_id: pid,
                    process_code: pc,
                })
                .collect()
        } else {
            rows.into_iter()
                .filter(|(sid, _, _, _)| current.can_access_shelf(*sid))
                .map(|(sid, pid, sc, pc)| AllShelfProcessMappingItem {
                    shelf_id: sid,
                    shelf_code: sc,
                    process_id: pid,
                    process_code: pc,
                })
                .collect()
        };

        Ok(AllShelfProcessMappingOut { items })
    }
}
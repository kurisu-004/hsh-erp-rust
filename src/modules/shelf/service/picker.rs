//! shelf 域 picker service
//!
//! 3 个端点：
//! - `list_for_return`           —— PRODUCTION 区活跃货架按 `current_load` 升序，
//!   SHELF_ACCOUNT scope 收窄到 user.shelf_ids 绑定的货架
//! - `list_for_inspection`       —— 仅 `zone='INSPECTION' AND is_active=true`，
//!   不过滤 scope（品检架通常由全员可见）
//! - `list_all_process_mappings` —— 所有 active shelf 的全部 mapping 批量返回
//!   （供 part_batch / worker_pool 一次性拿全集，防 N+1）
//!
//! ## SHELF_ACCOUNT scope 收窄（Fix B 简化）
//! `CurrentUser::can_access_shelf` 已经对 `shelf_wildcard` / `Role::Manager`
//! 短路返回 true，所以无需在外层再分支判断。统一调 `can_access_shelf` 即可：
//! - Manager / wildcard：每条都返回 true ⇒ 不过滤（等价于「全集」分支）
//! - 其他：按 user.shelf_ids 过滤（等价于「else」分支）
//!
//! 对 `list_all_process_mappings` 同理改造。

use sqlx::PgConnection;

use crate::auth::rbac::{CurrentUser, Role};
use crate::modules::process::repo::ProcessRepo;
use crate::shared::error::{code, AppError};

use super::super::dto::*;
use super::super::process_mapping::ShelfProcessRepo;
use super::super::repo::{ShelfRepo, TShelfWithLoad};
use super::{MAX_LIMIT, ZONE_INSPECTION};

impl super::crud::ShelfService {
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

        // SHELF_ACCOUNT scope：统一走 `can_access_shelf`。该方法已经对
        // shelf_wildcard / Role::Manager 短路返回 true，等价于原先的
        // 「if manager/wildcard 则全集 else 按 shelf_ids 过滤」分支，但代码更短。
        // 策略：拉全集 → 过滤 → 按 current_load 排序（统一一次拉取，避免 ORDER BY
        // 在 PG 与 filter 在 app 不同步；量小，几十条以内）。
        let all = ShelfRepo::list_active_production_ordered(&mut *conn).await?;
        let scoped: Vec<TShelfWithLoad> = all
            .into_iter()
            .filter(|s| current.can_access_shelf(s.id))
            .collect();

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
    // mapping 端点（全集）
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

        // SHELF_ACCOUNT scope：统一走 `can_access_shelf`（见 list_for_return
        // 上方注释）。Manager / wildcard 始终看到全集；其他按 shelf_ids 收窄。
        let items: Vec<AllShelfProcessMappingItem> = rows
            .into_iter()
            .filter(|(sid, _, _, _)| current.can_access_shelf(*sid))
            .map(|(sid, pid, sc, pc)| AllShelfProcessMappingItem {
                shelf_id: sid,
                shelf_code: sc,
                process_id: pid,
                process_code: pc,
            })
            .collect();

        Ok(AllShelfProcessMappingOut { items })
    }
}
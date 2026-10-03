//! shelf 域 picker service
//!
//! 2 个端点：
//! - `list_for_return`     —— PRODUCTION 区活跃货架按 `current_load` 升序，
//!   SHELF_ACCOUNT scope 收窄到 user.shelf_ids 绑定的货架
//! - `list_for_inspection` —— 仅 `zone='INSPECTION' AND is_active=true`，
//!   不过滤 scope（品检架通常由全员可见）
//!
//! ## 两个 picker 的 `current_load` 口径必须一致（2026-10-04）
//! 两者的聚合 SQL 在 `shelf/repo/sql.rs` 的 `list_active_production_ordered` 与
//! `list_active_inspection_with_load` 里，**聚合子查询逐字相同**（同 status 列表、
//! 同 `SUM(quantity)`、同 `deleted_at IS NULL`）。口径分叉会让两个 picker 对同一个架
//! 给出不同的数。差异仅在 `zone` 与是否按 load 排序。
//!
//! ## SHELF_ACCOUNT scope 收窄（Fix B 简化）
//! `CurrentUser::can_access_shelf` 已经对 `shelf_wildcard` / `Role::Manager`
//! 短路返回 true，所以无需在外层再分支判断。统一调 `can_access_shelf` 即可：
//! - Manager / wildcard：每条都返回 true ⇒ 不过滤（等价于「全集」分支）
//! - 其他：按 user.shelf_ids 过滤（等价于「else」分支）
//!
//! ## 2026-10-02 域拆分（本文件两处删除）
//! - `list_all_process_mappings`（原 `GET /shelves/processes`）随工序映射端点搬到
//!   `src/modules/prod/shelf_process/service.rs::ShelfProcessService::list_all_mappings`
//! - `list_for_return` 里 `next_process_id` 的**跨域占位校验**段删除
//!
//! ## 事务边界（2026-09-22 重构对齐 iam 范本）
//! 事务移交 handler；service 方法签名 `<R: ShelfRepoTrait>(&self, mut repo: R, ...)`，
//! 生产 `R = &mut PgConnection`。

use crate::auth::rbac::{CurrentUser, Role};
use crate::shared::error::AppError;

use super::super::dto::*;
use super::super::repo::{ShelfRepoTrait, TShelfWithLoad};
use super::super::vo::*;

impl super::crud::ShelfService {
    // =======================================================================
    // Picker 端点
    // =======================================================================

    /// `GET /shelves/for-return?next_process_id=`：PRODUCTION 区活跃货架按
    /// `current_load` 升序，SHELF_ACCOUNT scope 仅看到 user.shelf_ids 绑定的
    /// 货架（用 `can_access_shelf`），Manager 见全集；最空货架标 `is_recommended`。
    ///
    /// `next_process_id` **不做服务端校验**（2026-10-02 删除占位校验段）：原实现在
    /// 这里跨域调 prod `ProcessRepo::get_by_id` 确认工序存在，随后立刻
    /// `let _ = next_pid_opt;` 丢弃结果 —— 是真跨域依赖 + 一次多余查询，且自带注释
    /// 写明「不强制该货架必映射此 process」。该语义本就由 picker 前端把
    /// next_process_id 与候选 shelf 一并提交给 worker-scan、由 worker-scan 在后端
    /// 强校验（20507 `BIZ_SHELF_PROCESS_NOT_MAPPED`）承担，故整段删除。
    /// Query 字段 `next_process_id` 保留（前端继续传，只是不再由本端点消费）。
    pub async fn list_for_return<R: ShelfRepoTrait>(
        &self,
        mut repo: R,
        // 2026-10-02：占位校验删除后本形参不再被消费，改 `_query` 消 unused 告警；
        // 形参保留是为了 handler 侧 `Query<ShelfForReturnQuery>` 契约不变
        // （前端继续传 `next_process_id`，语义由 worker-scan 承担）。
        _query: &ShelfForReturnQuery,
        current: &CurrentUser,
    ) -> Result<ShelfForReturnOut, AppError> {
        current.require_any_role(&[
            Role::Manager,
            Role::Clerk,
            Role::ShelfAccount,
            Role::CncProgrammer,
        ])?;

        // SHELF_ACCOUNT scope：统一走 `can_access_shelf`。该方法已经对
        // shelf_wildcard / Role::Manager 短路返回 true，等价于原先的
        // 「if manager/wildcard 则全集 else 按 shelf_ids 过滤」分支，但代码更短。
        // 策略：拉全集 → 过滤 → 按 current_load 排序（统一一次拉取，避免 ORDER BY
        // 在 PG 与 filter 在 app 不同步；量小，几十条以内）。
        let all = repo.list_active_production_ordered().await?;
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
    ///
    /// 2026-10-04：改调 `list_active_inspection_with_load`（原调
    /// `list_with_filters(None, Some(ZONE_INSPECTION), Some(true), MAX_LIMIT, 0)`，
    /// 取的是裸 `TShelf`、**无聚合**）—— 出参因此缺 `current_load`，前端品检架卡片
    /// 无守卫地渲染「在架 N 件」⇒ 每张卡片显示「在架 **undefined** 件」。补在**后端**，
    /// 聚合口径与 `list_for_return` 逐字一致（同 status 列表、同 `SUM(quantity)`、
    /// 同 `deleted_at IS NULL`），两个 picker 对同一个架不会给出不同的数。
    ///
    /// 本端点**不**标 `is_recommended`：品检架没有「最空优先」的选架语义，
    /// 也没有消费方（for-return 独有）。
    pub async fn list_for_inspection<R: ShelfRepoTrait>(
        &self,
        mut repo: R,
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

        let shelves = repo.list_active_inspection_with_load().await?;
        let items = shelves
            .into_iter()
            .map(|s| ShelfForInspectionItem {
                id: s.id,
                code: s.code,
                name: s.name,
                zone: s.zone,
                location: s.location,
                is_active: s.is_active,
                current_load: s.current_load,
            })
            .collect();
        Ok(ShelfForInspectionOut { items })
    }
}

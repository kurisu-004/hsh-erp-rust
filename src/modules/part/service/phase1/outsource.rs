//! part 域：外协系列只读端点
//!
//! - `list_outsource_in_flight` —— `GET /parts/outsource-in-flight`
//! - `list_outsource_sendable` —— `GET /parts/outsource-sendable`
//!
//! 2026-10-02：外协收发三写点（`send_to_outsource` / `receive_from_outsource` /
//! `receive_from_outsource_to_inspection`）随批次用例迁往
//! `crate::modules::prod::batch::service::outsource`，两条 list 端点留在 part 域。

use crate::auth::rbac::{CurrentUser, Role};
use crate::modules::part::repo::{PartListFilters, PartRepoTrait};
use crate::modules::part::vo::{PartListItem, PartListOut};
use crate::shared::error::AppError;

use super::super::PartService;
use crate::modules::part::dto_crud::PartListQuery;

impl PartService {
    // ===== 1.3 外协流转 =====

    // ===== 1.3 外协辅助列表 =====

    /// `GET /parts/outsource-in-flight`：status=OUTSOURCE 工单一览。
    /// 简化版：复用 `list_parts` 但强制 status=OUTSOURCE。
    pub async fn list_outsource_in_flight<R: PartRepoTrait>(
        mut repo: R,
        query: &PartListQuery,
        current: &CurrentUser,
    ) -> Result<PartListOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk])?;
        let limit = query.limit.unwrap_or(50).clamp(1, 200);
        let offset = query.offset.unwrap_or(0).max(0);
        let f = PartListFilters {
            customer_ids: &[],
            status: Some("OUTSOURCE"),
            statuses: &[],
            is_urgent: query.is_urgent,
            keyword: Some(query.keyword.as_deref().unwrap_or("")),
            // 2026-09-17 PR-4 守卫修复：list_outsource_in_flight 不透传
            // locations/holder_ids（业务语义固定 OUTSOURCE 状态）
            locations: &[],
            holder_ids: &[],
            // 2026-09-30 新增：外协 endpoint 不暴露日期窗口过滤（仅 com::union_list
            // 使用），固定 None 维持旧行为。
            planned_delivery_date_from: None,
            planned_delivery_date_to: None,
            // 2026-09-30 新增：外协 endpoint 不暴露 10 字段（4 文本 + 4 日期
            // + 2 IS NULL），固定 None 维持旧行为；com::union_list 专用。
            drawing_no_pat: None,
            name_pat: None,
            order_no_pat: None,
            serial_no_pat: None,
            request_date_from: None,
            request_date_to: None,
            system_delivery_date_from: None,
            system_delivery_date_to: None,
            order_no_is_null: None,
            system_delivery_date_is_null: None,
            // 2026-09-28 新增：内部 caller（外协在途）不过滤装配体子件（语义
            // 固定 OUTSOURCE 单件状态），与历史行为一致。
            part_only: false,
            sort_by: match query.sort_by.as_deref().unwrap_or("PLANNED_DELIVERY_DATE") {
                "CREATED_AT" => "created_at",
                "UPDATED_AT" => "updated_at",
                "PLANNED_DELIVERY_DATE" => "planned_delivery_date",
                "REQUEST_DATE" => "request_date",
                "SERIAL_NO" => "serial_no",
                "DRAWING_NO" => "drawing_no",
                "NAME" => "name",
                _ => "planned_delivery_date",
            },
            sort_dir: query.sort_dir.as_deref().unwrap_or("ASC"),
            limit,
            offset,
            include_deleted: false,
        };
        let items = repo.list_with_filters(&f).await?;
        let total = repo.count_with_filters(&f).await?;
        // 2026-09-27 review 第 1 轮修复：PartListItem 改显式列字段（不再
        // flatten TPart），用 `From<TPart>` 派生；customer_name / l1_customer_name /
        // location / holder_name 4 派生字段保持 None（service 层不再 enrich）。
        let list_items: Vec<PartListItem> = items.into_iter().map(PartListItem::from).collect();
        Ok(PartListOut {
            items: list_items,
            total,
            limit,
            offset,
        })
    }

    /// `GET /parts/outsource-sendable`：可发外协的零件。
    /// 简化版：status ∈ {PENDING, IN_PROCESS}（service 层不引入 OUTSOURCE_QUOTE 表的依赖）。
    pub async fn list_outsource_sendable<R: PartRepoTrait>(
        mut repo: R,
        query: &PartListQuery,
        current: &CurrentUser,
    ) -> Result<PartListOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk, Role::Inspector])?;
        let limit = query.limit.unwrap_or(50).clamp(1, 200);
        let offset = query.offset.unwrap_or(0).max(0);
        let f = PartListFilters {
            customer_ids: &[],
            status: None,
            statuses: &["PENDING".into(), "IN_PROCESS".into()],
            is_urgent: query.is_urgent,
            keyword: Some(query.keyword.as_deref().unwrap_or("")),
            // 2026-09-17 PR-4 守卫修复：list_outsource_sendable 不透传
            // locations/holder_ids（业务语义固定 PENDING+IN_PROCESS 状态）
            locations: &[],
            holder_ids: &[],
            // 2026-09-30 新增：外协 endpoint 不暴露日期窗口过滤（仅 com::union_list
            // 使用），固定 None 维持旧行为。
            planned_delivery_date_from: None,
            planned_delivery_date_to: None,
            // 2026-09-30 新增：外协 endpoint 不暴露 10 字段（4 文本 + 4 日期
            // + 2 IS NULL），固定 None 维持旧行为；com::union_list 专用。
            drawing_no_pat: None,
            name_pat: None,
            order_no_pat: None,
            serial_no_pat: None,
            request_date_from: None,
            request_date_to: None,
            system_delivery_date_from: None,
            system_delivery_date_to: None,
            order_no_is_null: None,
            system_delivery_date_is_null: None,
            // 2026-09-28 新增：内部 caller（外协可发）不过滤装配体子件（语义
            // 固定 PENDING+IN_PROCESS 单件状态），与历史行为一致。
            part_only: false,
            sort_by: match query.sort_by.as_deref().unwrap_or("PLANNED_DELIVERY_DATE") {
                "CREATED_AT" => "created_at",
                "UPDATED_AT" => "updated_at",
                "PLANNED_DELIVERY_DATE" => "planned_delivery_date",
                "REQUEST_DATE" => "request_date",
                "SERIAL_NO" => "serial_no",
                "DRAWING_NO" => "drawing_no",
                "NAME" => "name",
                _ => "planned_delivery_date",
            },
            sort_dir: query.sort_dir.as_deref().unwrap_or("ASC"),
            limit,
            offset,
            include_deleted: false,
        };
        let items = repo.list_with_filters(&f).await?;
        let total = repo.count_with_filters(&f).await?;
        // 2026-09-27 review 第 1 轮修复：PartListItem 改显式列字段（不再
        // flatten TPart），用 `From<TPart>` 派生；customer_name / l1_customer_name /
        // location / holder_name 4 派生字段保持 None（service 层不再 enrich）。
        let list_items: Vec<PartListItem> = items.into_iter().map(PartListItem::from).collect();
        Ok(PartListOut {
            items: list_items,
            total,
            limit,
            offset,
        })
    }
}

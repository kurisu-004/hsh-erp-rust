//! prod::batch 的 INSPECTION 状态批次集合读
//!
//! `GET /api/v2/prod/batches/inspection` —— 返回 `status='INSPECTION'` 的全部活跃
//! 批次（含工单 / holder / process / delivery_note / customer 名称，一次 JOIN
//! 解析，服务层无 N+1）。`GET /repair` 与 `GET /repairing` 是同形的另两条判据，
//! 见 `repair.rs`。
//!
//! - 权限：Manager + Inspector
//! - 限流：`limit ∈ [1, 200]`，默认 200；`offset` 默认 0
//! - `customer_id`：单值 → `expand_customer_id` 展开为 L1+L2 ids（与 `list_parts` 同逻辑）
//! - `keyword` / `serial_no`：service 层拼 `%...%` 加通配符；为防 SQL 注入风险，
//!   拒绝 `%` / `_` / `\` 等通配符特殊字符（含任一 → VALIDATION_ERROR 40001）

use crate::auth::rbac::{CurrentUser, Role};
use crate::modules::part::repo::PartRepoTrait;
use crate::modules::part::service::crud::expand_customer_id;
use crate::modules::prod::batch::dto::InspectionBatchListQuery;
use crate::modules::prod::batch::repo::PartBatchRepo;
use crate::modules::prod::batch::vo::{InspectionBatchListItemOut, InspectionBatchListOut};
use crate::shared::error::AppError;

use super::BatchService;

impl BatchService {
    /// `GET /prod/batches/inspection` 列表：对齐 Python v1
    /// `PartService.list_inspection_batches`，返回 status=INSPECTION 全部活跃批次
    /// （含工单 / holder / process / delivery_note / customer 名称，一次 JOIN 解析，
    /// 服务层无 N+1）。
    ///
    /// 权限：Manager + Inspector（对齐 v1）。
    /// 限流：`limit ∈ [1, 200]`，默认 200；`offset` 默认 0。
    /// customer_id：单值 → `expand_customer_id` 展开为 L1+L2 ids（与 `list_parts` 同逻辑）。
    /// keyword / serial_no：service 层拼 `%...%` 加通配符；为防 SQL 注入风险，
    /// 拒绝 `%` / `_` / `\\` 等通配符特殊字符（含任一 → VALIDATION_ERROR 40001）。
    pub async fn list_inspection_batches<R: PartRepoTrait>(
        mut repo: R,
        query: &InspectionBatchListQuery,
        current: &CurrentUser,
    ) -> Result<InspectionBatchListOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Inspector])?;

        let limit = query.limit.unwrap_or(200).clamp(1, 200);
        let offset = query.offset.unwrap_or(0).max(0);

        // customer_id 展开：单值 → [L1, 所有 L2]；None → 不传（走全客户）
        let customer_ids_owned: Vec<i64>;
        let customer_ids: &[i64] = if let Some(cid) = query.customer_id {
            customer_ids_owned = expand_customer_id(repo.conn_mut(), cid).await?;
            &customer_ids_owned
        } else {
            &[]
        };

        // keyword：service 层拼 `%...%` + 拒绝 SQL 通配符特殊字符（% _ \）
        let keyword_owned: Option<String> = match query.keyword.as_deref() {
            Some(kw) => {
                if kw.contains(['%', '_', '\\']) {
                    return Err(AppError::validation("keyword 不能包含通配符 % _ \\"));
                }
                Some(format!("%{kw}%"))
            }
            None => None,
        };
        let keyword: Option<&str> = keyword_owned.as_deref();

        // serial_no：同上
        let serial_no_owned: Option<String> = match query.serial_no.as_deref() {
            Some(sn) => {
                if sn.contains(['%', '_', '\\']) {
                    return Err(AppError::validation("serial_no 不能包含通配符 % _ \\"));
                }
                Some(format!("%{sn}%"))
            }
            None => None,
        };
        let serial_no: Option<&str> = serial_no_owned.as_deref();

        let statuses: &[&str] = &["INSPECTION"];

        let rows = PartBatchRepo::list_batches_with_part(
            repo.conn_mut(),
            statuses,
            customer_ids,
            keyword,
            serial_no,
            query.planned_delivery_date_from,
            query.planned_delivery_date_to,
            limit,
            offset,
        )
        .await?;

        let total = PartBatchRepo::count_batches_with_part(
            repo.conn_mut(),
            statuses,
            customer_ids,
            keyword,
            serial_no,
            query.planned_delivery_date_from,
            query.planned_delivery_date_to,
        )
        .await?;

        Ok(InspectionBatchListOut {
            items: rows
                .into_iter()
                .map(InspectionBatchListItemOut::from)
                .collect(),
            total,
            limit,
            offset,
        })
    }
}

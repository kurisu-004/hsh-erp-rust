//! outsource 域 service — 发货记录 / 对账单子模块
//!
//! 覆盖端点：
//! - reconcile_update_shipment — 对账页更新 shipment（unit_price / quantity / is_billed）
//! - list_company_sent_parts    — 2026-10-03 新增：对账页 sent-parts 一览
//!   （`GET /outsource-companies/{id}/sent-parts`；此前路由未注册 → 前端 404）
//! - list_in_flight             — 2026-10-03 新增：在途批次一览
//!   （`GET /outsource-shipments/in-flight`；替代 part 域错形状的
//!   `/parts/outsource-in-flight`）
//!
//! ## 事务边界（2026-09-22 refactor 对齐 iam 范本）
//! 事务移交 handler：service 仅业务逻辑，所有跨 repo 操作经 `repo: R`
//! （by-value；`R: OutsourceRepoTrait`）参数传入——handler/service 借 `&mut *tx` /
//! `&mut *conn` 喂给 `OutsourceRepoTrait` trait（trait 已直接 `impl for &mut PgConnection`）。
//! service 不知事务——handler `pool.begin()` + `tx.commit()` 包外。
//!
//! impl 块直接挂在 `OutsourceService` 上（与 mod.rs / company.rs / quote.rs / sendable.rs
//! 共同 impl）。

use rust_decimal::Decimal;

use crate::auth::rbac::{CurrentUser, Role};
use crate::modules::outsource::dto::{
    OutsourceInFlightListQuery, OutsourceSentPartListQuery, OutsourceShipmentReconcileUpdateRequest,
};
use crate::modules::outsource::model::TOutsourceShipment;
use crate::modules::outsource::repo::OutsourceRepoTrait;
use crate::modules::outsource::vo::{
    OutsourceInFlightItem, OutsourceInFlightListOut, OutsourceSentPartListOut,
    OutsourceSentPartOut, OutsourceShipmentOut,
};
use crate::shared::error::{AppError, code};

use super::{
    DEFAULT_LIMIT, LIST_MAX_LIMIT, OutsourceService, format_price, join_customer_path,
    keyword_pattern, not_found_shipment, parse_price, version_conflict,
};

/// shipment_out：单条拼装（part / process / company 三个批查 + 批次号补全）。
async fn shipment_out<R: OutsourceRepoTrait>(
    repo: &mut R,
    s: TOutsourceShipment,
) -> Result<OutsourceShipmentOut, AppError> {
    // 单条拼装：part / process / company 三个批查
    let part = repo.part_drawing_name(s.part_id).await?;
    let company = repo.company_map_name(&[s.outsource_company_id]).await?;
    let company = company.into_iter().next().map(|(_, name)| name);
    let process = repo.process_get_name(s.process_id).await?;
    let batch_no = if let Some(bid) = s.batch_id {
        repo.batch_get_no(bid).await?
    } else {
        None
    };
    Ok(OutsourceShipmentOut {
        id: s.id,
        version: s.version,
        quote_id: s.quote_id,
        part_id: s.part_id,
        batch_id: s.batch_id,
        batch_no,
        outsource_company_id: s.outsource_company_id,
        process_id: s.process_id,
        quantity: s.quantity,
        unit_price: format_price(&s.unit_price),
        status: s.status,
        sent_at: s.sent_at,
        received_at: s.received_at,
        is_billed: s.is_billed,
        created_at: s.created_at,
        updated_at: s.updated_at,
        part_drawing_no: part.as_ref().map(|p| p.0.clone()),
        part_name: part.map(|p| p.1),
        outsource_company_name: company,
        process_name: process,
        customer_path: None,
    })
}

impl OutsourceService {
    pub async fn reconcile_update_shipment<R: OutsourceRepoTrait>(
        &self,
        mut repo: R,
        id: i64,
        req: &OutsourceShipmentReconcileUpdateRequest,
        current: &CurrentUser,
    ) -> Result<OutsourceShipmentOut, AppError> {
        #[allow(clippy::collapsible_if)]
        {
            if let Some(q) = req.quantity {
                if q <= 0 {
                    return Err(AppError::biz(code::BIZ_INVALID_VALUE, "quantity 必须 > 0"));
                }
            }
        }
        current.require_any_role(&[Role::Manager, Role::Clerk])?;
        let s = repo
            .shipment_get_by_id(id, false)
            .await?
            .ok_or_else(|| not_found_shipment(id))?;
        if s.version != req.version {
            return Err(AppError::biz(
                code::VERSION_CONFLICT,
                "该发货记录已被其他用户修改，请刷新后重试",
            ));
        }
        if !matches!(s.status.as_str(), "OUTSOURCING" | "RECEIVED") {
            return Err(AppError::biz(
                code::BIZ_OUTSOURCE_QUOTE_INVALID_TRANSITION,
                format!(
                    "对账编辑仅允许 OUTSOURCING / RECEIVED 状态；当前 {}",
                    s.status
                ),
            ));
        }
        let unit_price = req.unit_price.as_deref().map(parse_price).transpose()?;
        let n = repo
            .shipment_reconcile_update(
                id,
                req.version,
                unit_price,
                req.quantity,
                req.is_billed,
                current.id,
            )
            .await?;
        if n == 0 {
            return Err(version_conflict());
        }
        let fresh = repo
            .shipment_get_by_id(id, false)
            .await?
            .ok_or_else(|| not_found_shipment(id))?;
        shipment_out(&mut repo, fresh).await
    }

    /// `GET /outsource-companies/{company_id}/sent-parts`（2026-10-03 新增）
    ///
    /// 角色与 `reconcile_update_shipment` 对齐（Manager / Clerk）——对账页能看
    /// 就能改，反之不成立。
    ///
    /// **防 N+1**：list SQL 已把 part / 客户 / 工序 / 批次号 JOIN 出来，service
    /// 只做 Decimal 乘法与 VO 组装，不再逐行回查。
    pub async fn list_company_sent_parts<R: OutsourceRepoTrait>(
        &self,
        mut repo: R,
        company_id: i64,
        query: &OutsourceSentPartListQuery,
        current: &CurrentUser,
    ) -> Result<OutsourceSentPartListOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk])?;
        let limit = query
            .limit
            .unwrap_or(DEFAULT_LIMIT)
            .clamp(1, LIST_MAX_LIMIT);
        let offset = query.offset.unwrap_or(0).max(0);
        // 排序列白名单归一化：**用户输入只在这里被映射成 3 个 token 之一**，
        // 之后一律 bind 进 SQL（`CASE WHEN $7 = ...`），绝不拼进 SQL 文本。
        let sort_by = match query.sort_by.as_deref().map(str::trim) {
            Some("PRICE") => "PRICE",
            Some("RECEIVED_AT") => "RECEIVED_AT",
            _ => "SENT_AT",
        };
        let sort_dir = match query.sort_dir.as_deref().map(str::trim) {
            Some("ASC") => "ASC",
            _ => "DESC",
        };

        // keyword → part_ids（复用现成的 ILIKE 语义；空结果时 `part_ids_in` 非空
        // 但全不命中，靠 SQL 的 `part_id = ANY($2)` 自然返回 0 行）。
        let part_ids_in: Vec<i64> = match query.keyword.as_deref().map(str::trim) {
            Some(kw) if !kw.is_empty() => repo.part_keyword_search(kw).await?,
            _ => Vec::new(),
        };

        let rows = repo
            .shipment_list_for_company(
                company_id,
                &part_ids_in,
                query.sent_from,
                query.sent_to,
                query.received_from,
                query.received_to,
                sort_by,
                sort_dir,
                limit,
                offset,
            )
            .await?;
        let total = repo
            .shipment_count_for_company(
                company_id,
                &part_ids_in,
                query.sent_from,
                query.sent_to,
                query.received_from,
                query.received_to,
            )
            .await?;

        let items = rows
            .into_iter()
            .map(|r| {
                let unit_price = parse_price(&r.unit_price).unwrap_or(Decimal::ZERO);
                let total_price = unit_price * Decimal::from(r.quantity);
                OutsourceSentPartOut {
                    shipment_id: r.id,
                    version: r.version,
                    quote_id: r.quote_id,
                    part_id: r.part_id,
                    part_drawing_no: r.drawing_no,
                    part_name: r.name,
                    customer_path: join_customer_path(
                        r.parent_customer_name.as_deref(),
                        r.customer_name.as_deref(),
                    ),
                    batch_no: r.batch_no,
                    process_id: r.process_id,
                    process_name: r.process_name,
                    quantity: r.quantity,
                    unit_price: format_price(&unit_price),
                    total_price: format_price(&total_price),
                    sent_at: r.sent_at,
                    received_at: r.received_at,
                    status: r.status,
                    is_billed: r.is_billed,
                    is_urgent: r.is_urgent,
                }
            })
            .collect();

        Ok(OutsourceSentPartListOut {
            items,
            total,
            limit,
            offset,
        })
    }

    /// `GET /outsource-shipments/in-flight`（2026-10-03 新增）
    ///
    /// 取代 part 域旧 `list_outsource_in_flight`（后者返回通用 `PartListItem`，
    /// 与前端外协域的字段需求完全不匹配）。角色守卫与旧实现一致。
    pub async fn list_in_flight<R: OutsourceRepoTrait>(
        &self,
        mut repo: R,
        query: &OutsourceInFlightListQuery,
        current: &CurrentUser,
    ) -> Result<OutsourceInFlightListOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk])?;
        let limit = query
            .limit
            .unwrap_or(DEFAULT_LIMIT)
            .clamp(1, LIST_MAX_LIMIT);
        let offset = query.offset.unwrap_or(0).max(0);
        let pat = keyword_pattern(query.keyword.as_deref());

        let rows = repo
            .shipment_list_in_flight(pat.as_deref(), limit, offset)
            .await?;
        let total = repo.shipment_count_in_flight(pat.as_deref()).await?;

        let items = rows
            .into_iter()
            .map(|r| OutsourceInFlightItem {
                part_id: r.id,
                batch_id: r.batch_id,
                batch_no: r.batch_no,
                // 剩余待收量取批次行（部分接收的 max 值）
                quantity: r.quantity,
                serial_no: r.serial_no,
                drawing_no: r.drawing_no,
                name: r.name,
                is_urgent: r.is_urgent,
                customer_path: join_customer_path(
                    r.parent_customer_name.as_deref(),
                    r.customer_name.as_deref(),
                ),
                next_process_id: Some(r.process_id),
                next_process_name: r.process_name,
                outsource_company_id: r.outsource_company_id,
                outsource_company_name: r.outsource_company_name,
                sent_at: r.sent_at,
                // OCC 锚取批次 version（不是 shipment.version）
                version: r.version,
            })
            .collect();

        Ok(OutsourceInFlightListOut {
            items,
            total,
            limit,
            offset,
        })
    }
}

#[cfg(test)]
mod tests {
    //! 2026-09-22 refactor：原 `service.rs::mod tests` 5 helper 测试拆分；
    //! 本文件承接 shipment 子域用到的 1 个：`current_id_to_snowflake_unique`。
    use crate::infra::snowflake::SnowflakeIdGenerator;

    use super::super::current_id_to_snowflake;

    #[test]
    fn current_id_to_snowflake_unique() {
        // 2026-09-14 Phase 3 follow-up：使用真实雪花 id 生成器。
        // 同一 generator 连续两次调用应产生不同的 id（雪花 id sequence 自增）。
        let generator = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
        let a = current_id_to_snowflake(&generator);
        let b = current_id_to_snowflake(&generator);
        assert_ne!(a, b, "雪花 id 应当单调递增");
    }
}

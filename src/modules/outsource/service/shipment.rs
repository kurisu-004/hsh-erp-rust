//! outsource 域 service — 发货记录 / 对账单子模块
//!
//! 覆盖端点：
//! - reconcile_update_shipment — 对账页更新 shipment（unit_price / quantity / is_billed）
//!
//! ## 事务边界（2026-09-22 refactor 对齐 iam 范本）
//! 事务移交 handler：service 仅业务逻辑，所有跨 repo 操作经 `repo: R`
//! （by-value；`R: OutsourceRepoTrait`）参数传入——handler/service 借 `&mut *tx` /
//! `&mut *conn` 喂给 `OutsourceRepoTrait` trait（trait 已直接 `impl for &mut PgConnection`）。
//! service 不知事务——handler `pool.begin()` + `tx.commit()` 包外。
//!
//! impl 块直接挂在 `OutsourceService` 上（与 mod.rs / company.rs / quote.rs 共同 impl）。

use crate::auth::rbac::{CurrentUser, Role};
use crate::modules::outsource::dto::{
    OutsourceShipmentOut, OutsourceShipmentReconcileUpdateRequest,
};
use crate::modules::outsource::model::TOutsourceShipment;
use crate::modules::outsource::repo::OutsourceRepoTrait;
use crate::shared::error::{AppError, code};

use super::{OutsourceService, format_price, not_found_shipment, parse_price, version_conflict};

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

//! outsource 域 service — 发货记录 / 对账单子模块
//!
//! 覆盖端点：
//! - reconcile_update_shipment — 对账页更新 shipment（unit_price / quantity / is_billed）
//! - list_company_sent_parts    — 对账页 sent-parts 一览
//!   （`GET /outsource-companies/{id}/sent-parts`）
//! - list_in_flight             — 在途批次一览
//!   （`GET /outsource-shipments/in-flight`）
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
use crate::modules::outsource::repo::{OutsourceRepoTrait, OutsourceSentPartFilter};
use crate::modules::outsource::vo::{
    OutsourceInFlightItem, OutsourceInFlightListOut, OutsourceSentPartListOut,
    OutsourceSentPartOut, OutsourceShipmentOut,
};
use crate::shared::error::{AppError, code};

use super::{
    DEFAULT_LIMIT, LIST_MAX_LIMIT, OutsourceService, format_price, join_customer_path,
    keyword_pattern, not_found_shipment, parse_optional_snowflake, parse_price, version_conflict,
};

/// shipment_out：单条拼装（part / 客户 / process / company 批查 + 批次号补全）。
async fn shipment_out<R: OutsourceRepoTrait>(
    repo: &mut R,
    s: TOutsourceShipment,
) -> Result<OutsourceShipmentOut, AppError> {
    // 单条拼装：part / 客户 / process / company 四个批查
    let part = repo.part_drawing_name(s.part_id).await?;
    // 2026-10-03 起 customer_path 真算（此前硬编码 None，对账行编辑每次回读都把
    // 客户列抹掉）。口径与 list 侧 `OutsourceSentPartRow` 的两条 LEFT JOIN 逐条
    // 一致（客户自身 `deleted_at IS NULL` 才给名），故同一条历史 shipment 经写
    // 端点回读与经 list 读到的 customer_path 恒相同。
    let (l2_name, l1_name) = repo.part_customer_names(s.part_id).await?;
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
        customer_path: join_customer_path(l1_name.as_deref(), l2_name.as_deref()),
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

    /// `GET /outsource-companies/{company_id}/sent-parts`
    ///
    /// 角色与 `reconcile_update_shipment` 对齐（Manager / Clerk）——对账页能看
    /// 就能改，反之不成立。
    ///
    /// **防 N+1**：list SQL 已把 part / 客户 / 工序 / 批次号 JOIN 出来，service
    /// 只做 Decimal 乘法与 VO 组装，另加一次公司名补全（信封字段，前端渲染页头用），
    /// 不再逐行回查。
    ///
    /// 2026-10-09：`keyword` 拆成 `drawing_no` / `name` 两个直连 ILIKE，并新增
    /// `customer_id` / `process_id` / `is_billed` 三个精确维度。**省掉了三样东西**：
    /// `part_keyword_search` 那条预搜索（`LIMIT 10000` 且无 `ORDER BY` ⇒ 触顶时静默
    /// 返回非确定性子集，`total` 偏小）、配套的「给了 keyword 却零命中要早返回」分支
    /// （SQL 谓词是 `($N::text IS NULL OR col ILIKE $N)` 形态，零命中天然就是零行，
    /// 不需要 service 兜底）、以及两个 repo 方法里的 `part_ids_in` 形参。
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
        // 之后一律 bind 进 SQL（`CASE WHEN $11 = ...`），绝不拼进 SQL 文本。
        // `to_ascii_uppercase` 后再 match，白名单**大小写不敏感**
        //（`?sort_by=price` 与 `?sort_by=PRICE` 等价）。
        let sort_by = match query
            .sort_by
            .as_deref()
            .map(str::trim)
            .map(str::to_ascii_uppercase)
            .as_deref()
        {
            Some("PRICE") => "PRICE",
            Some("RECEIVED_AT") => "RECEIVED_AT",
            _ => "SENT_AT",
        };
        let sort_dir = match query
            .sort_dir
            .as_deref()
            .map(str::trim)
            .map(str::to_ascii_uppercase)
            .as_deref()
        {
            Some("ASC") => "ASC",
            _ => "DESC",
        };

        // 公司名补全（信封字段）：公司已软删 / 不存在时给 `null` —— 本端点不因公司
        // 缺失而 404，前端标题位要能显示「未知公司」。
        let outsource_company_name = repo
            .company_map_name(&[company_id])
            .await?
            .into_iter()
            .next()
            .map(|(_, name)| name);

        let drawing_no_pat = keyword_pattern(query.drawing_no.as_deref());
        let name_pat = keyword_pattern(query.name.as_deref());
        let filter = OutsourceSentPartFilter {
            drawing_no: drawing_no_pat.as_deref(),
            name: name_pat.as_deref(),
            customer_id: parse_optional_snowflake(query.customer_id.as_deref(), "customer_id")?,
            process_id: parse_optional_snowflake(query.process_id.as_deref(), "process_id")?,
            is_billed: query.is_billed,
            sent_from: query.sent_from,
            sent_to: query.sent_to,
            received_from: query.received_from,
            received_to: query.received_to,
        };

        let rows = repo
            .shipment_list_for_company(company_id, &filter, sort_by, sort_dir, limit, offset)
            .await?;
        let total = repo.shipment_count_for_company(company_id, &filter).await?;

        // `unit_price` 解析失败用 `?` 传播（`create_quote` 同款），不降级成
        // `Decimal::ZERO` —— 对账页上的「假 0.00 金额」是最坏的失败模式
        // （`total_price` 静默变 0，用户看不出是数据坏了）。该列 DB 侧是
        // `Numeric(12,2) NOT NULL`，正常不可达，是兜底闸门。
        let mut items = Vec::with_capacity(rows.len());
        for r in rows {
            let unit_price = parse_price(&r.unit_price)?;
            let total_price = unit_price * Decimal::from(r.quantity);
            items.push(OutsourceSentPartOut {
                shipment_id: r.id,
                version: r.version,
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
            });
        }

        Ok(OutsourceSentPartListOut {
            outsource_company_id: company_id,
            outsource_company_name,
            items,
            total,
            limit,
            offset,
        })
    }

    /// `GET /outsource-shipments/in-flight`（2026-10-03 新增）
    ///
    /// 取代 part 域旧 `list_outsource_in_flight`（后者返回通用 `PartListItem`，
    /// 与前端外协域的字段需求完全不匹配）。
    ///
    /// 2026-10-04 权限对齐为 **Manager + Clerk + Inspector**：外协移动写端点
    /// （`POST /outsource-queue/move` 的三个方向）与菜单
    /// `outsource_send_receive_list` 都已授予 INSPECTOR，导致 Inspector 能发能收却
    /// 看不到在途列表、点不到「接收」按钮。本端点纯只读，且返回体不含任何价格列
    /// （`OutsourceInFlightItem` 没有 `price` / `unit_price` 任何一列），故放宽不涉
    /// 商务敏感数据。
    ///
    /// 同域的 `reconcile_update_shipment`（写对账单价 / 数量 / 开票标记）与
    /// `list_company_sent_parts`（对账页列，含 `unit_price`）**维持 Manager + Clerk**。
    pub async fn list_in_flight<R: OutsourceRepoTrait>(
        &self,
        mut repo: R,
        query: &OutsourceInFlightListQuery,
        current: &CurrentUser,
    ) -> Result<OutsourceInFlightListOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk, Role::Inspector])?;
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
    //!
    //! 2026-10-09：generator 由本地 `SnowflakeIdGenerator::new(.., 1)` 改为
    //! `crate::shared::test_snowflake::shared_test_snowflake()`（**lib 单测进程内唯一**
    //! 的 generator 对象；为什么不用 test-support 的同名函数见该模块顶部 doc）。根因：
    //! 位布局 `ts << 22 | instance << 12 | seq` 里 `last_ms` / `sequence` 是 generator
    //! **对象私有**字段、`new()` 从 0 起步 ⇒ 两个 instance 相同、对象不同的 generator
    //! 同毫秒各取 seq 0 会发出逐字节相同的 id（`t_*_pkey` 23505）。instance 这 10 bit
    //! 现在只留给跨进程区分（1024 槽），进程内唯一性由共享对象串行发号保证。
    //!
    //! 共享对象不会削弱本用例的前提：它仍是「同一 generator 连续两次调用」，只是
    //! `sequence` 由进程内已用过多少号决定，而非每次从 0 重新起步。
    use super::super::current_id_to_snowflake;

    #[test]
    fn current_id_to_snowflake_unique() {
        // 2026-09-14 Phase 3 follow-up：使用真实雪花 id 生成器。
        // 同一 generator 连续两次调用应产生不同的 id（雪花 id sequence 自增）。
        let generator = crate::shared::test_snowflake::shared_test_snowflake();
        let a = current_id_to_snowflake(generator);
        let b = current_id_to_snowflake(generator);
        assert_ne!(a, b, "雪花 id 应当单调递增");
    }
}

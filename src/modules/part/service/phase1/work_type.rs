//! part 域：工种维度只读端点
//!
//! - `list_by_work_type` —— `GET /parts/by-work-type/{work_type_id}`
//! - `list_pickable_by_work_type` —— `GET /parts/pickable-by-work-type/{work_type_id}`
//! - `list_by_worker` —— `GET /parts/by-worker/{worker_id}`
//!
//! 2026-10-02：手动 `pick_up`（B 方案兜底）随批次用例迁往
//! `crate::modules::prod::batch::service::pickup`，三条 list 端点留在 part 域。

use crate::auth::rbac::{CurrentUser, Role};
use crate::modules::part::repo::PartRepoTrait;
use crate::modules::part::vo::{PartListItem, PartListOut};
use crate::shared::error::AppError;

use super::super::PartService;
use crate::modules::part::dto_crud::{ByWorkTypeQuery, ByWorkerQuery};

impl PartService {
    // ===== Phase 2 (2026-09-13) — 领取链路 (B 方案：手动 pick-up 兜底) =====

    /// `GET /parts/by-work-type/{work_type_id}`：可领件（按工种过滤）。
    ///
    /// 实现：worker.work_type_id = $1 → t_part_batch.current_holder_id = worker.id，
    /// 且 batch.location='WORKER'。简化：直接按 worker 反查（每工种有多个 worker）。
    pub async fn list_by_work_type<R: PartRepoTrait>(
        mut repo: R,
        work_type_id: i64,
        query: &ByWorkTypeQuery,
        current: &CurrentUser,
    ) -> Result<PartListOut, AppError> {
        current.require_any_role(&[
            Role::Manager,
            Role::Clerk,
            Role::Inspector,
            Role::ShelfAccount,
        ])?;
        let limit = query.limit.unwrap_or(50).clamp(1, 200);
        let offset = query.offset.unwrap_or(0).max(0);
        // 直接列出该工种所有 worker 当前持有的件（IN_PROCESS + location=WORKER）
        //
        // ⚠️ `p.serial_no` 按 `Option<String>` 收（2026-10-03 review 第 1 轮
        // Major-2 修）：`t_part.serial_no` 是 nullable（手工工单无序列号，见
        // baseline `serial_no character varying(15)` 无 NOT NULL），原先按
        // `String` 解码 → 遇到任一 `serial_no IS NULL` 的 part 就整页 500
        // （`unexpected null; try decoding as an Option`）。手工工单是常态，
        // 故这是真会触发的路径。同一缺陷的第三处见 `list_pickable_by_work_type`
        // （2026-10-03 已修）与本文件 `list_by_worker`（同批已修）。
        let rows: Vec<(i64, Option<String>, String, i32, i64, Option<String>)> = sqlx::query_as(
            "SELECT p.id, p.serial_no, p.drawing_no, b.quantity, b.id AS bid, w.name AS worker_name \
             FROM t_part_batch b \
             JOIN t_part p ON p.id = b.part_id \
             JOIN t_worker w ON w.id = b.current_holder_id \
             WHERE b.deleted_at IS NULL AND p.deleted_at IS NULL \
               AND w.deleted_at IS NULL AND w.is_active = true \
               AND w.work_type_id = $1 \
               AND b.status = 'IN_PROCESS' AND b.location = 'WORKER' \
             ORDER BY b.id DESC LIMIT $2 OFFSET $3",
        )
        .bind(work_type_id)
        .bind(limit)
        .bind(offset)
        .fetch_all(repo.conn_mut())
        .await?;
        let items: Vec<PartListItem> = rows
            .into_iter()
            .map(|(id, serial, drawing, qty, bid, worker_name)| {
                // 2026-09-27 review 第 1 轮修复：PartListItem 改显式列字段，
                // 通过 `From<TPart>` 派生基础字段（next_process_id 自动不复制）。
                let p = crate::modules::part::model::TPart {
                    id,
                    serial_no: serial,
                    name: drawing.clone(),
                    drawing_no: drawing,
                    applicant_name: String::new(),
                    quantity: qty,
                    request_date: chrono::NaiveDate::from_ymd_opt(1970, 1, 1).unwrap(),
                    planned_delivery_date: chrono::NaiveDate::from_ymd_opt(1970, 1, 1).unwrap(),
                    customer_id: 0,
                    assembly_id: None,
                    status: "IN_PROCESS".to_string(),
                    is_urgent: false,
                    next_process_id: None,
                    order_no: None,
                    system_delivery_date: None,
                    note: None,
                    unit_price: rust_decimal::Decimal::ZERO,
                    total_price: rust_decimal::Decimal::ZERO,
                    version: 0,
                    created_at: chrono::NaiveDateTime::from_timestamp_opt(0, 0).unwrap(),
                    created_by: None,
                    updated_at: chrono::NaiveDateTime::from_timestamp_opt(0, 0).unwrap(),
                    updated_by: None,
                    deleted_at: None,
                    process_chain_id: None,
                };
                // 附加 worker_name（轻量：DTO 上没字段，仅放 batch_id 展示）
                let _ = bid;
                let _ = worker_name;
                PartListItem::from(p)
            })
            .collect();
        let total: i64 = sqlx::query_scalar(
            "SELECT COUNT(*)::bigint FROM t_part_batch b \
             JOIN t_worker w ON w.id = b.current_holder_id \
             WHERE b.deleted_at IS NULL AND w.deleted_at IS NULL \
               AND w.is_active = true AND w.work_type_id = $1 \
               AND b.status = 'IN_PROCESS' AND b.location = 'WORKER'",
        )
        .bind(work_type_id)
        .fetch_one(repo.conn_mut())
        .await?;
        Ok(PartListOut {
            items,
            total,
            limit,
            offset,
        })
    }

    /// `GET /parts/pickable-by-work-type/{work_type_id}`：可领取件（货架上、绑了对应工序）。
    pub async fn list_pickable_by_work_type<R: PartRepoTrait>(
        mut repo: R,
        work_type_id: i64,
        query: &ByWorkTypeQuery,
        current: &CurrentUser,
    ) -> Result<PartListOut, AppError> {
        current.require_any_role(&[
            Role::Manager,
            Role::Clerk,
            Role::Inspector,
            Role::ShelfAccount,
        ])?;
        let limit = query.limit.unwrap_or(50).clamp(1, 200);
        let offset = query.offset.unwrap_or(0).max(0);
        let shelf_filter = query.shelf_id;
        // 列：t_part_batch WHERE location=PRODUCTION_SHELF AND batch.current_process_id IN (工种→工序映射)
        //
        // 2026-10-03 补投影 `b.id` / `b.version`：本端点的行本来就是「批次行」，
        // 而出参 VO 只有 part 级字段，扫码台「领料」拿不到批次 id 就发不出写请求。
        // 两者填进 `PartListItem::batch_id` / `batch_version`（仅本端点填，
        // 其它复用该 VO 的端点恒 null，见 vo/part.rs 字段 doc）。
        //
        // 2026-09-30 修复（migration 004）：原写法是
        // `JOIN t_process_chain_step s ON s.id = b.current_process_step_id
        //  JOIN t_work_type_process wtp ON wtp.process_id = s.process_id` ——
        // 与此前 worker_pool 池查询同款的 INNER JOIN 盲区：batch 的
        // current_process_step_id 为 NULL（新下发批次的常态，无工序链工单恒为
        // NULL）时匹配不到任何 step 行，批次会从「可领取」列表里**整条消失**。
        // 改直读 b.current_process_id（工序归属的权威列）后该盲区消失。
        //
        // 2026-10-02 修：JOIN 条件补 `wtp.deleted_at IS NULL` —— 工种↔工序映射走
        // 「整组替换」（软删旧行 + 插新行），不过滤则已取消勾选的工序仍会把批次
        // 匹配进本工种的可领取列表。下方 COUNT 同步用**同一 `wtp` 谓词**，否则
        // `total` 与 `items` 对不上（两处的 `t_part` 侧不对称见 COUNT 处注释）。
        // ⚠️ `p.serial_no` 按 `Option<String>` 收（2026-10-03 修）：`t_part.serial_no`
        // 是 nullable（手工工单无序列号），原先按 `String` 解码 → 遇到任一
        // `serial_no IS NULL` 的 part 就整页 500（`unexpected null; try decoding as
        // an Option`）。手工工单是常态，故这是真会触发的路径。
        let rows: Vec<(i64, Option<String>, String, i32, Option<i64>, i64, i32)> = sqlx::query_as(
            "SELECT p.id, p.serial_no, p.drawing_no, b.quantity, b.current_process_id, \
                    b.id, b.version \
             FROM t_part_batch b \
             JOIN t_part p ON p.id = b.part_id \
             JOIN t_work_type_process wtp ON wtp.process_id = b.current_process_id \
                AND wtp.deleted_at IS NULL \
             JOIN t_shelf sh ON sh.id = b.current_holder_id \
             WHERE b.deleted_at IS NULL AND p.deleted_at IS NULL \
               AND b.status = 'IN_PROCESS' AND b.location = 'PRODUCTION_SHELF' \
               AND sh.is_active = true AND sh.zone = 'PRODUCTION' \
               AND wtp.work_type_id = $1 \
               AND ($2::bigint IS NULL OR b.current_holder_id = $2) \
             ORDER BY p.is_urgent DESC, p.planned_delivery_date ASC, b.id ASC \
             LIMIT $3 OFFSET $4",
        )
        .bind(work_type_id)
        .bind(shelf_filter)
        .bind(limit)
        .bind(offset)
        .fetch_all(repo.conn_mut())
        .await?;
        let items: Vec<PartListItem> = rows
            .into_iter()
            .map(|(id, serial, drawing, qty, _np, batch_id, batch_version)| {
                // 2026-09-27 review 第 1 轮修复：PartListItem 改显式列字段，
                // 通过 `From<TPart>` 派生基础字段（next_process_id 自动不复制）。
                let p = crate::modules::part::model::TPart {
                    id,
                    serial_no: serial,
                    name: drawing.clone(),
                    drawing_no: drawing,
                    applicant_name: String::new(),
                    quantity: qty,
                    request_date: chrono::NaiveDate::from_ymd_opt(1970, 1, 1).unwrap(),
                    planned_delivery_date: chrono::NaiveDate::from_ymd_opt(1970, 1, 1).unwrap(),
                    customer_id: 0,
                    assembly_id: None,
                    status: "IN_PROCESS".to_string(),
                    is_urgent: false,
                    next_process_id: None,
                    order_no: None,
                    system_delivery_date: None,
                    note: None,
                    unit_price: rust_decimal::Decimal::ZERO,
                    total_price: rust_decimal::Decimal::ZERO,
                    // ⚠️ 本 VO 的 `version` 是 **part 级**（`t_part.version`），
                    // 而取行 SQL 压根没投影 `p.version`（只投影了 p.id /
                    // p.serial_no / p.drawing_no）—— 恒 0 是**有意的占位**，
                    // 不是漏取值。批次乐观锁版本走 2026-10-03 新增的
                    // `PartListItem::batch_version`（取自 `b.version`）；
                    // 下一个读者请勿把本字段当批次版本用。
                    version: 0,
                    created_at: chrono::NaiveDateTime::from_timestamp_opt(0, 0).unwrap(),
                    created_by: None,
                    updated_at: chrono::NaiveDateTime::from_timestamp_opt(0, 0).unwrap(),
                    updated_by: None,
                    deleted_at: None,
                    process_chain_id: None,
                };
                let mut item = PartListItem::from(p);
                // 批次锚点：本端点是全仓唯一填这两字段的路径（出参契约见
                // vo/part.rs::PartListItem::batch_id 的字段 doc）。
                item.batch_id = Some(batch_id);
                item.batch_version = Some(batch_version);
                item
            })
            .collect();
        let total: i64 = sqlx::query_scalar(
            // 2026-10-02 订正：与取行查询同 `wtp` 谓词（含 `wtp.deleted_at IS NULL`），
            // 但**不等于同 WHERE** —— 取行查询额外 `JOIN t_part p` 且带
            // `p.deleted_at IS NULL`，本 COUNT 不 join `t_part`。故软删 part 的 active
            // batch 会计入 `total` 而不计入 `items`，软删 part 下该工种的可领批次分页
            // 总数偏大。是否补 join 属 `total` 语义决策，未在本处改动。
            "SELECT COUNT(*)::bigint FROM t_part_batch b \
             JOIN t_work_type_process wtp ON wtp.process_id = b.current_process_id \
                AND wtp.deleted_at IS NULL \
             JOIN t_shelf sh ON sh.id = b.current_holder_id \
             WHERE b.deleted_at IS NULL \
               AND b.status = 'IN_PROCESS' AND b.location = 'PRODUCTION_SHELF' \
               AND sh.is_active = true AND sh.zone = 'PRODUCTION' \
               AND wtp.work_type_id = $1 \
               AND ($2::bigint IS NULL OR b.current_holder_id = $2)",
        )
        .bind(work_type_id)
        .bind(shelf_filter)
        .fetch_one(repo.conn_mut())
        .await?;
        Ok(PartListOut {
            items,
            total,
            limit,
            offset,
        })
    }

    /// `GET /parts/by-worker/{worker_id}`：工人当前持有件。
    pub async fn list_by_worker<R: PartRepoTrait>(
        mut repo: R,
        worker_id: i64,
        query: &ByWorkerQuery,
        current: &CurrentUser,
    ) -> Result<PartListOut, AppError> {
        current.require_any_role(&[
            Role::Manager,
            Role::Clerk,
            Role::Inspector,
            Role::ShelfAccount,
        ])?;
        let limit = query.limit.unwrap_or(50).clamp(1, 200);
        let offset = query.offset.unwrap_or(0).max(0);
        // ⚠️ `p.serial_no` 按 `Option<String>` 收（2026-10-03 review 第 1 轮
        // Major-2 修）：同 `list_by_work_type` / `list_pickable_by_work_type`，
        // `t_part.serial_no` nullable，原先按 `String` 解码会让
        // `serial_no IS NULL` 的手工工单把整页打成 500。这是本文件同款写法的
        // 最后一处。
        let rows: Vec<(i64, Option<String>, String, i32)> = sqlx::query_as(
            "SELECT p.id, p.serial_no, p.drawing_no, b.quantity \
             FROM t_part_batch b \
             JOIN t_part p ON p.id = b.part_id \
             WHERE b.deleted_at IS NULL AND p.deleted_at IS NULL \
               AND b.status = 'IN_PROCESS' AND b.location = 'WORKER' \
               AND b.current_holder_id = $1 \
             ORDER BY b.id DESC LIMIT $2 OFFSET $3",
        )
        .bind(worker_id)
        .bind(limit)
        .bind(offset)
        .fetch_all(repo.conn_mut())
        .await?;
        let items: Vec<PartListItem> = rows
            .into_iter()
            .map(|(id, serial, drawing, qty)| {
                // 2026-09-27 review 第 1 轮修复：PartListItem 改显式列字段，
                // 通过 `From<TPart>` 派生基础字段（next_process_id 自动不复制）。
                let p = crate::modules::part::model::TPart {
                    id,
                    serial_no: serial,
                    name: drawing.clone(),
                    drawing_no: drawing,
                    applicant_name: String::new(),
                    quantity: qty,
                    request_date: chrono::NaiveDate::from_ymd_opt(1970, 1, 1).unwrap(),
                    planned_delivery_date: chrono::NaiveDate::from_ymd_opt(1970, 1, 1).unwrap(),
                    customer_id: 0,
                    assembly_id: None,
                    status: "IN_PROCESS".to_string(),
                    is_urgent: false,
                    next_process_id: None,
                    order_no: None,
                    system_delivery_date: None,
                    note: None,
                    unit_price: rust_decimal::Decimal::ZERO,
                    total_price: rust_decimal::Decimal::ZERO,
                    version: 0,
                    created_at: chrono::NaiveDateTime::from_timestamp_opt(0, 0).unwrap(),
                    created_by: None,
                    updated_at: chrono::NaiveDateTime::from_timestamp_opt(0, 0).unwrap(),
                    updated_by: None,
                    deleted_at: None,
                    process_chain_id: None,
                };
                PartListItem::from(p)
            })
            .collect();
        let total: i64 = sqlx::query_scalar(
            "SELECT COUNT(*)::bigint FROM t_part_batch b \
             WHERE b.deleted_at IS NULL \
               AND b.status = 'IN_PROCESS' AND b.location = 'WORKER' \
               AND b.current_holder_id = $1",
        )
        .bind(worker_id)
        .fetch_one(repo.conn_mut())
        .await?;
        Ok(PartListOut {
            items,
            total,
            limit,
            offset,
        })
    }
}

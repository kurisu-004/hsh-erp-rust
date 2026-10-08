//! part 域：工种维度只读端点
//!
//! - `list_by_work_type` —— `GET /parts/by-work-type/{work_type_id}`
//!
//! 2026-10-10：报工台的两条 list 端点（`pickable-by-work-type` / `by-worker`）连同
//! 它们的取行投影、货架 scope 收窄一并迁往 `crate::modules::prod::scan::listing`，
//! 新路径 `GET /api/v2/prod/scan/pickable` 与 `GET /api/v2/prod/scan/held`
//! （硬切无 alias）。本文件只剩 `by-work-type` 一条 —— 它**没有任何前端消费方**
//! （前端三页的数据源都是报工台那两个端点），保留是因为它是 part 域 list 族里
//! 唯一的「按工种列工人持有件」读端点，且已有集成测试覆盖其 part 侧投影。

use chrono::{NaiveDate, NaiveDateTime};
use rust_decimal::Decimal;

use crate::auth::rbac::{CurrentUser, Role};
use crate::modules::part::dto_crud::ByWorkTypeQuery;
use crate::modules::part::repo::PartRepoTrait;
use crate::modules::part::vo::{ChainState, PartListItem, PartListOut};
use crate::shared::error::AppError;

use super::super::PartService;

/// 1970-01-01：未投影的 `date` 类占位值。
///
/// 本端点的取行 SQL 不投影 `request_date`，恒为占位。前端没有消费 `request_date`，
/// 故保持占位不动。
const PLACEHOLDER_DATE: NaiveDate = NaiveDate::from_ymd_opt(1970, 1, 1).unwrap();

/// Unix epoch：未投影的 `timestamp` 类占位值。
const PLACEHOLDER_TS: NaiveDateTime = NaiveDateTime::from_timestamp_opt(0, 0).unwrap();

/// `by-work-type` 的取行投影。
///
/// ## 为什么不是 `TPart` 字面量
/// 2026-10-04 之前本端点手抄一份 20 字段的 `TPart { ... }` 字面量再交给
/// `PartListItem::from`，两个后果：
/// 1. **占位值伪装成业务值** —— 取行 SQL 只投影 `p.id` / `p.serial_no` /
///    `p.drawing_no` 三列，其余全靠字面量填，于是 `name` 填成图号、
///    `is_urgent` 填 `false`、`planned_delivery_date` 填 1970-01-01；
/// 2. **漏字段不报错** —— `TPart` 30+ 字段，加字段时要手改字面量。
///
/// 本 struct 把「SQL 投影了什么」收敛成**一处**事实：字段名即 SQL 别名
/// （`#[derive(sqlx::FromRow)]` 按列名匹配），取行 SQL 必须逐字投影每一个字段。
/// ⚠️ 这是**运行期**校验（`sqlx::query_as` 而非 `query_as!` 宏，宏才有编译期校验）：
/// 少投影一列 → 运行时报 missing column；**多投影一列 → 静默忽略**。
/// 本端点不填的字段在 SQL 里显式投影成 `NULL::<type> AS <字段名>`，把「本端点不填」
/// 写进 SQL 而不是靠 struct 缺省。
///
/// ## 字段填充口径
/// - part 侧 `id` / `serial_no` / `name` / `drawing_no` / `is_urgent` /
///   `planned_delivery_date` / `system_delivery_date`：投影**真实值**。
/// - `quantity`：取自**批次**（`b.quantity`）而非 part。
/// - `batch_id` / `batch_version` / `process_chain_id` / 链四字段 /
///   `has_process_chain`：本端点**一律投影 NULL**，理由与 `vo/part.rs` 的字段
///   doc 同款 —— 这些是**批次级**语义，而本端点的行是「该工种所有工人持有的批次」
///   的并集，一个批次对应一行，填批次锚点虽成立但报工台已由专用的
///   `prod::scan` 两条端点承担；链位置同理（真正需要它的放回页读
///   `GET /scan/held`）。
#[derive(Debug, sqlx::FromRow)]
struct WorkTypeByWorkTypeRow {
    // ---- part 侧（投影真实值）----
    id: i64,
    serial_no: Option<String>,
    name: String,
    drawing_no: String,
    is_urgent: bool,
    planned_delivery_date: NaiveDate,
    /// `t_part.system_delivery_date` 是**可空**列（`date` 无 NOT NULL）。
    system_delivery_date: Option<NaiveDate>,
    // ---- 批次侧 ----
    /// 取自 `b.quantity`（批次数量），不是 `p.quantity`。
    quantity: i32,
}

impl WorkTypeByWorkTypeRow {
    /// 投影行 → `PartListItem`（穷尽 struct 字面量，让「哪些是投影、哪些是占位」
    /// 一眼可辨）。
    ///
    /// 仍为占位值的字段（前端无消费方）：`applicant_name` / `request_date` /
    /// `customer_id` / `status` / `order_no` / `note` / `unit_price` /
    /// `total_price` / `version` / 4 个审计字段 / `assembly_id` /
    /// `customer_name` / `l1_customer_name` / `location` / `holder_name`。
    /// `batch_id` / `batch_version` / `chain_*` / `has_process_chain` 同样取保守
    /// 默认（`None` / `None` / `NONE` / `"0"` / `null` / `null` / `false`）——
    /// `NONE` 语义是「让用户手填下一道工序」，与「不知道」同向。
    fn into_list_item(self) -> PartListItem {
        PartListItem {
            id: self.id,
            serial_no: self.serial_no,
            name: self.name,
            drawing_no: self.drawing_no,
            is_urgent: self.is_urgent,
            planned_delivery_date: self.planned_delivery_date,
            system_delivery_date: self.system_delivery_date,
            quantity: self.quantity,
            // ---- 以下为占位值（前端无消费方，见方法 doc）----
            applicant_name: String::new(),
            request_date: PLACEHOLDER_DATE,
            customer_id: 0,
            status: "IN_PROCESS".to_string(),
            order_no: None,
            note: None,
            unit_price: Decimal::ZERO,
            total_price: Decimal::ZERO,
            // ⚠️ 本 VO 的 `version` 是 **part 级**（`t_part.version`），而本端点的
            // 取行 SQL 都没投影 `p.version` —— 恒 0 是**有意的占位**，不是漏取值。
            // 批次乐观锁版本走 `PartListItem::batch_version`（取自 `b.version`）；
            // 下一个读者请勿把本字段当批次版本用。
            version: 0,
            created_at: PLACEHOLDER_TS,
            created_by: None,
            updated_at: PLACEHOLDER_TS,
            updated_by: None,
            deleted_at: None,
            assembly_id: None,
            customer_name: None,
            l1_customer_name: None,
            location: None,
            holder_name: None,
            row_type: Some("PART".to_string()),
            has_children: false,
            child_count: None,
            has_cnc_program: false,
            process_chain_id: None,
            batch_id: None,
            batch_version: None,
            has_process_chain: false,
            chain_state: ChainState::None,
            chain_next_process_id: 0,
            chain_next_process_name: None,
            chain_current_process_name: None,
            delivered_quantity: None,
        }
    }
}

impl PartService {
    // ===== Phase 2 (2026-09-13) — 领取链路 =====

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
        // ⚠️ `p.serial_no` 按 `Option<String>` 收：`t_part.serial_no` 是 nullable
        // （手工工单无序列号，见 baseline `serial_no character varying(15)` 无
        // NOT NULL），原先按 `String` 解码 → 遇到任一 `serial_no IS NULL` 的 part
        // 就整页 500（`unexpected null; try decoding as an Option`）。手工工单是
        // 常态，故这是真会触发的路径。
        //
        // 2026-10-04 补投影 part 侧 4 个真实列（`name` / `is_urgent` /
        // `system_delivery_date` / `planned_delivery_date`）：此前 `name` 填的是
        // 图号副本、`is_urgent` 恒 false、两个交期是占位值。补投影只为与报工台
        // 那两条端点的口径一致，避免下次又各自漂移。
        //
        // 2026-10-04 删两列死投影（`b.id AS bid` / `w.name AS worker_name`）：原
        // 代码取出来只 `let _ = bid;` / `let _ = worker_name;` 丢弃，从未进过出参。
        // `FromRow` 按列名匹配，**多余列会被静默忽略**（少投影才报 missing column）。
        //
        // ⚠️ 不加货架 scope 谓词：与报工台的 `pickable` 端点不同，本端点列的是
        // **工人手上**的件（不是架上的候选池），收窄货架 scope 在这里没有对应语义。
        let rows: Vec<WorkTypeByWorkTypeRow> = sqlx::query_as(
            "SELECT p.id, p.serial_no, p.name, p.drawing_no, p.is_urgent, \
                    p.system_delivery_date, p.planned_delivery_date, b.quantity \
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
            .map(WorkTypeByWorkTypeRow::into_list_item)
            .collect();
        // COUNT 与取行的 `t_part` 侧不对称（COUNT 不 join `t_part`）是既有语义决策：
        // 软删 part 的 active batch 会计入 `total` 而不计入 `items`。是否补 join 属
        // `total` 语义决策，未在本处改动。
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
}

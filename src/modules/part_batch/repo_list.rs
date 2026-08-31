//! part_batch 域 —— 待检批次列表 / COUNT 专用查询。
//!
//! 从 `repo.rs` 抽出，对应 `GET /parts/inspection-batches` 列表 + 配套分页
//! count。本文件只放 `PartBatchRepo` 的两个 list/count 方法 + 它们唯一的
//! 依赖（`InspectionBatchListRow` / `NaiveDate`），便于把 `repo.rs` 控制
//! 在 1000 行硬上限内（conventions.md §2）。

use chrono::NaiveDate;
use sqlx::PgExecutor;

use super::model::InspectionBatchListRow;
use super::repo::PartBatchRepo;

impl PartBatchRepo {
    /// `GET /parts/inspection-batches` 专用：返回 `status=INSPECTION` 全部活跃
    /// 批次（含工单 + holder/process/delivery_note/customer 全部名称一次解析）。
    ///
    /// 与 v1 Python `PartBatchRepository.list_batches_with_part(statuses=[INSPECTION], ...)`
    /// 行为一致；过滤条件：
    /// - `pb.status = $1::text`（caller 传 'INSPECTION'；保留 statuses Vec 形参为后续扩
    ///   repair/repairing 复用，与 v1 `list_inspection_batches` 同签名）
    /// - `pb.deleted_at IS NULL` / `p.deleted_at IS NULL`
    /// - `customer_id = ANY($2)`（已展开的 L1+L2 ids，空数组短路走 0 行）
    /// - 关键字：`p.drawing_no ILIKE '%kw%' OR p.name ILIKE '%kw%' OR p.serial_no ILIKE '%kw%' OR p.order_no ILIKE '%kw%'`（caller 控 % 通配符）
    /// - 序列号：`p.serial_no ILIKE '%sn%'`
    /// - 计划交期：`p.planned_delivery_date BETWEEN $3 AND $4`（NULL 边界跳过）
    ///
    /// 排序：`p.is_urgent DESC, p.planned_delivery_date ASC, pb.id ASC`
    /// （与 v1 一致；紧急件优先 + 交期近优先 + 批次 id 兜底）。
    ///
    /// 返回 `Vec<InspectionBatchListRow>`：8 个 JOIN 一次拿齐所有名称，service
    /// 无需 N+1 二次查表。
    ///
    /// **已知 holder 歧义 bug**（沿用现有模式，不在本任务修复）：
    /// `COALESCE(s.name, w.name, oc.name)` 假定 `current_holder_id` 在三表之间
    /// 互不重叠；如果某 holder_id 同时命中 shelf/worker/outsource_company 之一
    /// 的 PK，会优先取 `s.name`。仓库已有相同模式（`list_active_by_part_id_with_holder`
    /// 行 558），属于遗留问题。后续统一修：用 `pb.location` 作 discriminator
    /// （`CASE pb.location WHEN 'WORKER' THEN w.name WHEN 'OUTSOURCE_COMPANY' THEN oc.name ELSE s.name END`）。
    /// 本函数为了与既有 repo 函数保持一致，**不**做该修复；如需修，会另开
    /// repo 层 patch PR 覆盖全部 4 处 COALESCE。
    #[allow(clippy::too_many_arguments)]
    pub async fn list_batches_with_part<'e, E: PgExecutor<'e>>(
        executor: E,
        statuses: &[&str],
        customer_ids: &[i64],
        keyword: Option<&str>,
        serial_no: Option<&str>,
        date_from: Option<NaiveDate>,
        date_to: Option<NaiveDate>,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<InspectionBatchListRow>, sqlx::Error> {
        if statuses.is_empty() {
            return Ok(Vec::new());
        }
        // `query!` 宏的 text[] 绑定要求 `&[String]` / `Vec<String>`，不接受
        // `&[&str]`（sqlx 编译期推断的 PG 编码器）；在 repo 内做一次性转换，
        // 保留对外 `&[&str]` 签名（与 v1 Python `list_batches_with_part` 对齐）。
        let statuses_vec: Vec<String> = statuses.iter().map(|s| s.to_string()).collect();
        // keyword 透传 caller 加好的 %...% 通配符（service 层负责拼，避免 SQL
        // 注入；repo 不做转义）。
        let rows = sqlx::query!(
            r#"
            SELECT
                pb.id              AS "pb_id!",
                pb.part_id         AS "pb_part_id!",
                pb.batch_no        AS "pb_batch_no!",
                pb.quantity        AS "pb_quantity!",
                pb.status          AS "pb_status!",
                pb.location        AS "pb_location?",
                pb.version         AS "pb_version!",
                pb.placed_at       AS "pb_placed_at?",
                pb.has_been_repaired AS "pb_has_been_repaired!",
                pb.parent_batch_id AS "pb_parent_batch_id?",
                pb.current_holder_id AS "pb_current_holder_id?",
                pb.next_process_id AS "pb_next_process_id?",
                pb.delivery_note_id AS "pb_delivery_note_id?",
                p.serial_no        AS "p_serial_no?",
                p.drawing_no       AS "p_drawing_no!",
                p.name             AS "p_name!",
                p.order_no         AS "p_order_no?",
                p.planned_delivery_date AS "p_planned_delivery_date!",
                p.is_urgent        AS "p_is_urgent!",
                p.version          AS "p_version!",
                p.created_at       AS "p_created_at!",
                p.updated_at       AS "p_updated_at!",
                p.customer_id      AS "p_customer_id!",
                c.name             AS "c_name?",
                c.parent_id        AS "c_parent_id?",
                pc.name            AS "pc_name?",
                COALESCE(s.name, w.name, oc.name) AS "holder_name?",
                np.name            AS "np_name?",
                dn.delivery_note_no AS "dn_no?"
            FROM t_part_batch pb
            JOIN t_part p
              ON p.id = pb.part_id
            JOIN t_customer c
              ON c.id = p.customer_id
            LEFT JOIN t_customer pc
              ON pc.id = c.parent_id
            LEFT JOIN t_shelf s
              ON s.id = pb.current_holder_id
            LEFT JOIN t_worker w
              ON w.id = pb.current_holder_id
            LEFT JOIN t_outsource_company oc
              ON oc.id = pb.current_holder_id
            LEFT JOIN t_process np
              ON np.id = pb.next_process_id
            LEFT JOIN t_delivery_note dn
              ON dn.id = pb.delivery_note_id
            WHERE pb.status = ANY($1)
              AND pb.deleted_at IS NULL
              AND p.deleted_at IS NULL
              -- customer_id 是可选过滤项：空数组 → 命中全部客户；
              -- 非空 → 限定到展开后的 L1+L2 ids。
              AND (cardinality($2::bigint[]) = 0 OR p.customer_id = ANY($2))
              AND ($3::text IS NULL
                   OR p.drawing_no ILIKE $3
                   OR p.name       ILIKE $3
                   OR p.serial_no  ILIKE $3
                   OR p.order_no   ILIKE $3)
              AND ($4::text IS NULL OR p.serial_no ILIKE $4)
              AND ($5::date IS NULL OR p.planned_delivery_date >= $5)
              AND ($6::date IS NULL OR p.planned_delivery_date <= $6)
            ORDER BY p.is_urgent DESC, p.planned_delivery_date ASC, pb.id ASC
            LIMIT $7 OFFSET $8
            "#,
            &statuses_vec,
            customer_ids,
            keyword,
            serial_no,
            date_from,
            date_to,
            limit,
            offset,
        )
        .fetch_all(executor)
        .await?;

        Ok(rows
            .into_iter()
            .map(|r| {
                // l1_customer_name：当 c.parent_id IS NOT NULL 时取 pc.name；否则
                // L1 = c.name（自身为 L1）。
                let l1_customer_name = if r.c_parent_id.is_some() {
                    r.pc_name.clone().or_else(|| r.c_name.clone())
                } else {
                    r.c_name.clone()
                };
                InspectionBatchListRow {
                    batch_id: r.pb_id,
                    part_id: r.pb_part_id,
                    batch_no: r.pb_batch_no,
                    quantity: r.pb_quantity,
                    status: r.pb_status,
                    location: r.pb_location,
                    version: r.pb_version,
                    placed_at: r.pb_placed_at,
                    has_been_repaired: r.pb_has_been_repaired,
                    parent_batch_id: r.pb_parent_batch_id,
                    current_holder_id: r.pb_current_holder_id,
                    holder_name: r.holder_name,
                    next_process_id: r.pb_next_process_id,
                    next_process_name: r.np_name,
                    delivery_note_id: r.pb_delivery_note_id,
                    delivery_note_no: r.dn_no,
                    serial_no: r.p_serial_no,
                    drawing_no: r.p_drawing_no,
                    name: r.p_name,
                    order_no: r.p_order_no,
                    planned_delivery_date: r.p_planned_delivery_date,
                    is_urgent: r.p_is_urgent,
                    part_version: r.p_version,
                    created_at: r.p_created_at,
                    updated_at: r.p_updated_at,
                    customer_id: r.p_customer_id,
                    customer_name: r.c_name,
                    l1_customer_name,
                }
            })
            .collect())
    }

    /// 配套 COUNT：与 `list_batches_with_part` 同 WHERE / 不同 SELECT / 无 ORDER。
    pub async fn count_batches_with_part<'e, E: PgExecutor<'e>>(
        executor: E,
        statuses: &[&str],
        customer_ids: &[i64],
        keyword: Option<&str>,
        serial_no: Option<&str>,
        date_from: Option<NaiveDate>,
        date_to: Option<NaiveDate>,
    ) -> Result<i64, sqlx::Error> {
        if statuses.is_empty() {
            return Ok(0);
        }
        // `query_scalar!` 宏的 text[] 绑定要求 `&[String]` / `Vec<String>`，
        // 同样在 repo 内做一次性转换。
        let statuses_vec: Vec<String> = statuses.iter().map(|s| s.to_string()).collect();
        let n: i64 = sqlx::query_scalar!(
            r#"
            SELECT COUNT(*) AS "n!"
            FROM t_part_batch pb
            JOIN t_part p
              ON p.id = pb.part_id
            WHERE pb.status = ANY($1)
              AND pb.deleted_at IS NULL
              AND p.deleted_at IS NULL
              -- customer_id 是可选过滤项：空数组 → 命中全部客户；
              -- 非空 → 限定到展开后的 L1+L2 ids（与 list_batches_with_part 同逻辑）。
              AND (cardinality($2::bigint[]) = 0 OR p.customer_id = ANY($2))
              AND ($3::text IS NULL
                   OR p.drawing_no ILIKE $3
                   OR p.name       ILIKE $3
                   OR p.serial_no  ILIKE $3
                   OR p.order_no   ILIKE $3)
              AND ($4::text IS NULL OR p.serial_no ILIKE $4)
              AND ($5::date IS NULL OR p.planned_delivery_date >= $5)
              AND ($6::date IS NULL OR p.planned_delivery_date <= $6)
            "#,
            &statuses_vec,
            customer_ids,
            keyword,
            serial_no,
            date_from,
            date_to,
        )
        .fetch_one(executor)
        .await?;
        Ok(n)
    }
}

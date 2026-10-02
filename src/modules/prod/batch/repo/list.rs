//! `t_part_batch` 列表 SQL —— 8-JOIN 工单 / 名称一次解析。
//!
//! 2026-10-02 随 `t_part_batch` 归属迁入 prod 域，从单文件 `prod/batch/repo/queries.rs`
//! 拆出（单文件已超 conventions.md §2 的 1000 行上限）。SQL 文本、签名、
//! 可见性零变化。
//!
//! 2026-10-03 VO 收口：`GET /prod/batches/inspection` 改走本文件末尾的窄投影
//! （`list_inspection_queue` / `count_inspection_queue`，3 JOIN + 13 字段）。
//! 下方 `list_batches_with_part` / `count_batches_with_part` 保持逐字不变 ——
//! 它是返修两条端点共用的宽投影，**不再**被 inspection 端点调用。

use chrono::NaiveDate;
use sqlx::{PgExecutor, Postgres, QueryBuilder};

use super::queries::PartBatchRepo;
use crate::modules::prod::batch::model::{InspectionBatchListRow, InspectionQueueRow};

impl PartBatchRepo {
    /// 返修两条端点共用的宽投影列表（`status` / `is_repairing` 判据由 caller
    /// 传入的 `statuses` 决定）。返回工单 + holder/process/delivery_note/customer
    /// 全部名称一次解析。
    ///
    /// 2026-10-03 起本函数**不再**服务 `GET /prod/batches/inspection`（该端点
    /// 改走末尾的 `list_inspection_queue` 窄投影）；SQL 文本、签名逐字不变。
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
    ///
    /// 2026-09-16 PR-3 批次 step 化（migration 028）：
    /// - 删 `pb.placed_at`（t_part_batch 列已删）
    /// - `pb.next_process_id` / `np.name` 改为派生：`LEFT JOIN t_process_chain_step s
    ///   ON s.id = pb.current_process_step_id` 后取 `s.process_id` / `t_process.name`
    /// - `pb.current_process_step_id` 新增
    ///
    /// 2026-09-30 决定该查询**不直读** `current_process_id`，继续从 step 派生，
    /// 理由：
    ///
    /// 本函数服务的集合读端点 —— `GET /prod/batches/repair`（`statuses =
    /// ['DELIVERED']`）与 `GET /prod/batches/repairing`（`RepairBatchesOut` 是本行
    /// 结构的类型别名）—— 都不是**工序池**端点，判据是 `status` 或
    /// `is_repairing` 标记，与 `current_process_id` 无关。
    ///
    /// 「送检 = 出池 → `current_process_id = NULL`」是不变式，且
    /// **所有进 INSPECTION 的写点都把该列清成 NULL**
    /// （`mark_batch_inspected` / `phase1::scan` / `outsource::receive_to_inspection` /
    /// `repair::complete_repair` 的 INSPECTION 分支）。若本查询改直读 cpid，则
    /// `next_process_id` / `next_process_name` 在 DELIVERED 列表里**恒为 null**
    /// —— 用户可见回归。
    ///
    /// 因此 `current_process_id` 是**池归属权威列**，其读取方严格限定为工序池 SQL
    /// （take_one / take_specific / list_candidates / group_count /
    /// count_pool_by_shelf）+ rollup 派生；**展示类列表**继续从
    /// `current_process_step_id`（可选的显示用定位信息）派生。
    ///
    /// 本函数不投影 `pb_current_process_id`（行映射无对应字段）。
    ///
    /// 对外 DTO 字段名为 `next_process_id` / `next_process_name`，取自 step 派生列。
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
                -- 2026-10-01 review 第 1 轮 M5：随列表投出返修标记（REPAIRING
                -- 已不是 status，见 vo/inspection.rs 的 BREAKING 标注）
                pb.is_repairing    AS "pb_is_repairing!",
                pb.location        AS "pb_location?",
                pb.version         AS "pb_version!",
                pb.current_process_step_id AS "pb_current_process_step_id?",
                pb.parent_batch_id AS "pb_parent_batch_id?",
                pb.current_holder_id AS "pb_current_holder_id?",
                -- next_process_id 派生自 step.process_id（LEFT JOIN
                -- t_process_chain_step）。**刻意不直读 pb.current_process_id**：
                -- INSPECTION 批次按出池不变式该列恒为 NULL，直读会让本字段恒
                -- null —— 详见本函数 doc 的 review 第 3 轮 M3 段。
                s.process_id       AS "step_process_id?",
                np.name            AS "np_name?",
                pb.delivery_note_id AS "pb_delivery_note_id?",
                dn.delivery_note_no AS "dn_no?",
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
                COALESCE(sh.name, w.name, oc.name) AS "holder_name?"
            FROM t_part_batch pb
            JOIN t_part p
              ON p.id = pb.part_id
            JOIN t_customer c
              ON c.id = p.customer_id
            LEFT JOIN t_customer pc
              ON pc.id = c.parent_id
            LEFT JOIN t_shelf sh
              ON sh.id = pb.current_holder_id
            LEFT JOIN t_worker w
              ON w.id = pb.current_holder_id
            LEFT JOIN t_outsource_company oc
              ON oc.id = pb.current_holder_id
            -- step JOIN 取 process_id：next_process_id / next_process_name 由此派生
            -- （review 第 3 轮 M3 回退，理由见本函数 doc）
            LEFT JOIN t_process_chain_step s
              ON s.id = pb.current_process_step_id
            LEFT JOIN t_process np
              ON np.id = s.process_id
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
                    is_repairing: r.pb_is_repairing,
                    location: r.pb_location,
                    version: r.pb_version,
                    current_process_step_id: r.pb_current_process_step_id,
                    parent_batch_id: r.pb_parent_batch_id,
                    current_holder_id: r.pb_current_holder_id,
                    holder_name: r.holder_name,
                    // 派生自 step.process_id（LEFT JOIN t_process_chain_step，
                    // review 第 3 轮 M3 恢复）；保留 DTO 字段名兼容前端
                    next_process_id: r.step_process_id,
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

// ===========================================================================
//  待品检队列（`GET /prod/batches/inspection`）—— 2026-10-03 VO 收口新增
// ===========================================================================

/// 待品检队列列表入参。
///
/// 排序项收的是**已白名单化的列名 / 方向**（`p.system_delivery_date` / `ASC`
/// 这类字面量），白名单映射在 service 层完成 —— repo 收不到任何外部输入，
/// 故拼进 SQL 文本的只有这两个受控字符串（范式同 `part/repo/sql/part_sql.rs`
/// 的 `ORDER BY {order_col} {order_dir}`）。
#[derive(Debug, Clone, Default)]
pub struct InspectionQueueFilters<'a> {
    /// 已 `expand_customer_id` 展开的 L1+L2 ids；空切片 → 不按客户过滤。
    pub customer_ids: &'a [i64],
    /// 图号 ILIKE pattern（service 已拼 `%...%` 并拒通配符）；`None` → 不过滤。
    pub drawing_no_pat: Option<&'a str>,
    /// 名称 ILIKE pattern；`None` → 不过滤。
    pub name_pat: Option<&'a str>,
    /// 序列号 ILIKE pattern；`None` → 不过滤。
    pub serial_no_pat: Option<&'a str>,
    /// 系统交期下界（含）；`None` → 不过滤。
    pub date_from: Option<NaiveDate>,
    /// 系统交期上界（含）；`None` → 不过滤。
    pub date_to: Option<NaiveDate>,
    /// 排序列（service 白名单映射后的列名字面量）。
    pub order_col: &'a str,
    /// 排序方向：`"ASC"` / `"DESC"`。
    pub order_dir: &'a str,
    pub limit: i64,
    pub offset: i64,
}

/// `GET /prod/batches/inspection` 窄投影 SELECT（13 个输出列 + 派生 L1 名的 2 列原料）。
///
/// 只 JOIN 3 张表：`t_part`（工单）/ `t_customer`（客户）/ `t_customer` 自连（L1）。
/// **不** JOIN `t_shelf` / `t_worker` / `t_outsource_company` / `t_process_chain_step`
/// / `t_process` / `t_delivery_note` —— 待品检页不渲染 holder / 工序 / 送货单。
///
/// 列别名直接取语义名（`pb.id AS batch_id` …），行结构侧 `FromRow` 同名承接。
const INSPECTION_QUEUE_SELECT: &str = "SELECT \
     pb.id AS batch_id, \
     pb.part_id AS part_id, \
     pb.batch_no AS batch_no, \
     pb.quantity AS quantity, \
     pb.version AS version, \
     p.serial_no AS serial_no, \
     p.drawing_no AS drawing_no, \
     p.name AS name, \
     p.system_delivery_date AS system_delivery_date, \
     p.is_urgent AS is_urgent, \
     p.customer_id AS customer_id, \
     c.name AS customer_name, \
     c.parent_id AS customer_parent_id, \
     pc.name AS parent_customer_name \
     FROM t_part_batch pb \
     JOIN t_part p ON p.id = pb.part_id \
     JOIN t_customer c ON c.id = p.customer_id \
     LEFT JOIN t_customer pc ON pc.id = c.parent_id";

/// COUNT 版本的 FROM 子句（与 [`INSPECTION_QUEUE_SELECT`] 同 JOIN，`SELECT COUNT(*)`）。
const INSPECTION_QUEUE_COUNT_FROM: &str = "SELECT COUNT(*)::bigint AS n \
     FROM t_part_batch pb \
     JOIN t_part p ON p.id = pb.part_id \
     JOIN t_customer c ON c.id = p.customer_id \
     LEFT JOIN t_customer pc ON pc.id = c.parent_id";

/// list / count 共用的 WHERE 拼装器 —— 判据只此一份，天然杜绝「count 与 items
/// 各说各话」的分页 bug。
fn push_inspection_queue_where(qb: &mut QueryBuilder<Postgres>, f: &InspectionQueueFilters<'_>) {
    // 判据固定为 INSPECTION（本端点不接 statuses 参数）。
    qb.push(
        " WHERE pb.status = 'INSPECTION' \
              AND pb.deleted_at IS NULL \
              AND p.deleted_at IS NULL",
    );
    // customer_id 可选过滤：空数组 → 命中全部客户；非空 → 限定到展开后的 L1+L2 ids。
    // 同一数组绑两次（cardinality 判空 + ANY 匹配），值相同、位次不同。
    let customer_ids = f.customer_ids.to_vec();
    qb.push(" AND (cardinality(")
        .push_bind(customer_ids.clone())
        .push("::bigint[]) = 0 OR p.customer_id = ANY(")
        .push_bind(customer_ids)
        .push("))");
    // 表头 3 个文本列各一个独立 ILIKE（`$n::text IS NULL` 短路 → 不过滤）。
    for (col, pat) in [
        ("p.drawing_no", f.drawing_no_pat),
        ("p.name", f.name_pat),
        ("p.serial_no", f.serial_no_pat),
    ] {
        qb.push(" AND (")
            .push_bind(pat)
            .push("::text IS NULL OR ")
            .push(col)
            .push(" ILIKE ")
            .push_bind(pat)
            .push(")");
    }
    // 系统交期区间（可空列，缺界不参与过滤）。
    qb.push(" AND (")
        .push_bind(f.date_from)
        .push("::date IS NULL OR p.system_delivery_date >= ")
        .push_bind(f.date_from);
    qb.push(") AND (")
        .push_bind(f.date_to)
        .push("::date IS NULL OR p.system_delivery_date <= ")
        .push_bind(f.date_to)
        .push(")");
}

/// `list_inspection_queue` 的行结构（FromRow）。
///
/// 手动 `#[derive(FromRow)]` 而非 `query_as!` —— SQL 由 `QueryBuilder` 动态拼装
/// （范式同 `part/repo/sql/pending_programming_sql.rs::PendingProgrammingItemRow`）。
/// 多出的 `customer_parent_id` / `parent_customer_name` 是 `l1_customer_name` 的派生
/// 原料，不进 VO。
#[derive(sqlx::FromRow)]
struct InspectionQueueRawRow {
    batch_id: i64,
    part_id: i64,
    batch_no: i32,
    quantity: i32,
    version: i32,
    serial_no: Option<String>,
    drawing_no: String,
    name: String,
    system_delivery_date: Option<NaiveDate>,
    is_urgent: bool,
    customer_id: i64,
    customer_name: Option<String>,
    customer_parent_id: Option<i64>,
    parent_customer_name: Option<String>,
}

impl PartBatchRepo {
    /// `GET /prod/batches/inspection` 列表（3 JOIN 窄投影 + 表头筛选 + 服务端排序）。
    ///
    /// 排序：`{order_col} {order_dir} NULLS LAST, pb.id ASC`。
    /// - `NULLS LAST` 是必需的：`p.system_delivery_date` 可空，而 PG 的默认值
    ///   ASC → `NULLS LAST` / DESC → `NULLS FIRST`，不显式指定时按交期倒序会把
    ///   未填交期的行顶到最前（与 `repo/mod.rs::list_pending_batches` 的既有做法一致）。
    /// - `pb.id ASC` 兜底：排序列可重复（同名不同批次），无兜底键时翻页会漏行 / 重复行。
    ///
    /// 走 `QueryBuilder`（动态 `ORDER BY` + 可选过滤，宏无法固化），故本查询不进
    /// `.sqlx` 离线元数据。
    pub async fn list_inspection_queue<'e, E: PgExecutor<'e>>(
        executor: E,
        f: &InspectionQueueFilters<'_>,
    ) -> Result<Vec<InspectionQueueRow>, sqlx::Error> {
        let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(INSPECTION_QUEUE_SELECT);
        push_inspection_queue_where(&mut qb, f);
        qb.push(format!(
            " ORDER BY {} {} NULLS LAST, pb.id ASC LIMIT ",
            f.order_col, f.order_dir
        ));
        qb.push_bind(f.limit);
        qb.push(" OFFSET ");
        qb.push_bind(f.offset);

        let rows: Vec<InspectionQueueRawRow> = qb.build_query_as().fetch_all(executor).await?;

        Ok(rows
            .into_iter()
            .map(|r| {
                // l1_customer_name 派生（与 `list_batches_with_part` 的同名逻辑一致）：
                // c.parent_id IS NOT NULL → pc.name.or(c.name)；否则（自身即 L1）→ c.name。
                let l1_customer_name = if r.customer_parent_id.is_some() {
                    r.parent_customer_name
                        .clone()
                        .or_else(|| r.customer_name.clone())
                } else {
                    r.customer_name.clone()
                };
                InspectionQueueRow {
                    batch_id: r.batch_id,
                    part_id: r.part_id,
                    batch_no: r.batch_no,
                    quantity: r.quantity,
                    version: r.version,
                    serial_no: r.serial_no,
                    drawing_no: r.drawing_no,
                    name: r.name,
                    system_delivery_date: r.system_delivery_date,
                    is_urgent: r.is_urgent,
                    customer_id: r.customer_id,
                    customer_name: r.customer_name,
                    l1_customer_name,
                }
            })
            .collect())
    }

    /// `GET /prod/batches/inspection` 配套 COUNT（与 `list_inspection_queue` 共用
    /// 同一个 WHERE 拼装器，无 ORDER BY / LIMIT / OFFSET）。
    pub async fn count_inspection_queue<'e, E: PgExecutor<'e>>(
        executor: E,
        f: &InspectionQueueFilters<'_>,
    ) -> Result<i64, sqlx::Error> {
        let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(INSPECTION_QUEUE_COUNT_FROM);
        push_inspection_queue_where(&mut qb, f);
        let (n,): (i64,) = qb.build_query_as().fetch_one(executor).await?;
        Ok(n)
    }
}

//! `t_part_batch` 集合读 SQL —— 待品检队列窄投影。
//!
//! 2026-10-02 随 `t_part_batch` 归属迁入 prod 域，从单文件 `prod/batch/repo/queries.rs`
//! 拆出（单文件已超 conventions.md §2 的 1000 行上限）。
//!
//! 2026-10-03 VO 收口：本文件只剩 `list_inspection_queue` / `count_inspection_queue`
//! 两条（3 JOIN + 13 字段），服务 `GET /prod/batches/inspection`。同批删掉原
//! 8-JOIN 宽投影 `list_batches_with_part` / `count_batches_with_part` —— 待品检端点
//! 是它们在本域的最后调用方，返修两条端点早已改为在 service 层直接构造
//! `vo::InspectionBatchListItemOut`（`service/repair.rs`），不查 repo。
//! 被删的 `count_batches_with_part` 漏了 `JOIN t_customer c`（list 有、count 没有），
//! 本就是一份已知的 count/list 不一致实现，留着只会诱导后来者复用。

use chrono::NaiveDate;
use sqlx::{PgExecutor, Postgres, QueryBuilder};

use super::queries::PartBatchRepo;
use crate::modules::prod::batch::model::InspectionQueueRow;

// ===========================================================================
//  待品检队列（`GET /prod/batches/inspection`）—— 2026-10-03 VO 收口新增
// ===========================================================================

/// 待品检队列列表入参。
///
/// 排序项收的是**已白名单化的列名 / 方向**（`p.system_delivery_date` / `ASC`
/// 这类字面量），白名单映射在 service 层完成 —— repo 收不到任何外部输入，
/// 故拼进 SQL 文本的只有这两个受控字符串（范式同 `part/repo/sql/part_sql.rs`
/// 的 `ORDER BY {order_col} {order_dir}`）。
///
/// 刻意**不**派生 `Default`：`Default` 会造出 `order_col = ""` / `order_dir = ""`，
/// 一旦被 `..Default::default()` 用上就生成 `ORDER BY  NULLS LAST` → 运行期 SQL
/// 语法错 500。调用方必须逐字段显式填（service 层的 `resolve_order_col` /
/// `resolve_order_dir` 兜底）。
#[derive(Debug, Clone)]
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
    // 同一数组绑两次（cardinality 判空 + ANY 匹配），`&[i64]` 可直接重复 push_bind，
    // 无需拷贝 —— `f` 的生命周期覆盖整个调用，两个 bind 借的是同一个不可变切片。
    qb.push(" AND (cardinality(")
        .push_bind(f.customer_ids)
        .push("::bigint[]) = 0 OR p.customer_id = ANY(")
        .push_bind(f.customer_ids)
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
                // l1_customer_name 派生：c.parent_id IS NOT NULL → pc.name.or(c.name)；
                // 否则（自身即 L1）→ c.name。与 `service/repair.rs::list_batches_matching`
                // 走 part 域 repo 时用的同名派生逐字一致。
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

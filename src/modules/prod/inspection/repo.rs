//! prod::inspection 子模块 repo 层 —— SQL 真源
//!
//! 2026-10-05 新增：ZST `InspectionScanRepo` + 5 个静态方法，全部走
//! `sqlx::query_as!` 宏（编译期按 `.env` 的 `DATABASE_URL` 连库校验，改列名 /
//! 改类型编译即失败）。不引入胖 trait：单 service + 纯读端点，无跨域调用方、
//! 无 mock 替身需求（范式同 `prod::programming::ProgrammingRepo`，见
//! `docs/repo-naming.md` §3.1）。
//!
//! 2026-10-07 新增：待品检队列读（`GET /api/v2/prod/inspection/queue`）自
//! `prod::batch::repo::list` 迁入本文件，与扫码读共用本文件，构成**两个 ZST**
//! （`InspectionScanRepo` / `InspectionQueueRepo`）——职责不重叠、都是无状态 ZST +
//! 固有静态方法，形制同 `prod::batch::repo` 的 `PartBatchRepo` + `BatchRepo`。
//! 两者 SQL 形态也不同：扫码读走 `query_as!` 字面量宏（编译期校验），队列读走
//! `QueryBuilder`（动态 `ORDER BY` + 可选过滤，宏无法固化，**不进 `.sqlx`**）。
//!
//! ## 5 个方法 / 单次请求的 SQL 条数
//! 一次扫码请求按分支取 2~4 条 SQL，**无 N+1**（子件再多也只有一条批次查询，
//! 走 `part_id = ANY($1)` 一次捞回全部子件的批次）：
//!
//! | 扫到的东西 | 依次执行的 SQL | 条数 |
//! |---|---|---:|
//! | 独立件（`assembly_id IS NULL`） | `find_part_by_serial` → `list_batches_by_part_ids` | 2 |
//! | 装配件子件（父装配件活跃） | `find_part_by_serial` → `find_assembly_by_id` → `list_parts_by_assembly` → `list_batches_by_part_ids` | 4 |
//! | 装配件子件（父装配件已软删） | `find_part_by_serial` → `find_assembly_by_id` → `list_batches_by_part_ids` | 3 |
//! | 装配件条码 | `find_assembly_by_serial` → `list_parts_by_assembly` → `list_batches_by_part_ids` | 3 |
//! | 两表皆未命中 | `find_part_by_serial` → `find_assembly_by_serial` | 2 |
//!
//! ⚠️ 「装配件条码」那行在**装配件无活跃子件**（无子件 / 全部子件被软删）时是 **2 条**：
//! `list_parts_by_assembly` 返回空 → `list_batches_by_part_ids` 对空切片提前返空、
//! 不发 SQL。
//!
//! ## 软删闸门（4 张表，逐条 SQL 写死）
//! - `t_part`：`p.deleted_at IS NULL`（命中查询 + 子件列表）
//! - `t_assembly`：`a.deleted_at IS NULL`（按序列号 + 按 id 两条）
//! - `t_part_batch`：`b.deleted_at IS NULL`
//! - 附带：`LEFT JOIN t_customer ... AND c.deleted_at IS NULL` —— 客户软删时
//!   客户名退化为 `null`，不影响该零件/批次返回
//!
//! 而 `LEFT JOIN` 进来的 `t_process` / `t_shelf` / `t_worker` /
//! `t_outsource_company` 四张表**不加**软删闸门 —— 与 `prod::batch::repo` 的
//! `list_active_by_part_id_with_holder` 既有写法一致（工序名、holder 名都是展示用
//! 附加信息，被软删也照常显示最后的样子）。故上面这张清单不是「本端点读过的全部表」
//! 的清单。
//!
//! ⚠️ 软删闸门**不是**「保守过滤」而是本端点的语义闸门：扫到软删行等于扫到一个
//! 业务上已不存在的码，前端据此弹「未找到」比弹一棵含已删数据的树更安全。
//!
//! ## ⚠️ 同一 `serial_no` 可能并存多行：取哪一行是**写死**的口径
//! `t_part` 的 `uk_t_part_serial_no` 是**部分**唯一索引
//! （`WHERE serial_no IS NOT NULL AND deleted_at IS NULL AND status <> 'CANCELLED'`），
//! 所以「软删行 + `CANCELLED` 行」与活跃行可以同号共存。命中查询按
//! `ORDER BY (p.status = 'CANCELLED') ASC, p.id DESC LIMIT 1` 取值：
//!
//! - `CANCELLED` 行**排到最后**（`false < true`）—— 扫到历史废弃工单毫无意义，
//!   而工单被 `cancel` 后同号重建是常规操作
//! - 剩余行由部分唯一索引保证至多一条，`p.id DESC` 只是兜底取新
//!
//! ⚠️ **只有 `CANCELLED` 行时照样返回正常树**（排序键只保证「活跃行优先」，不保证
//! 「必有活跃行」）：`POST /parts/{id}/cancel` 把 part 打成 `CANCELLED` 后，终态
//! 守卫会拦下 rollup 里的 `release_part_serial_no`，序列号因此**保留**在库中、
//! 同号重建前一直可扫。此时 `part.status` 原文透出 `CANCELLED`，前端应自行禁用
//! 该节点上的写操作按钮。刻意**不加** `AND p.status <> 'CANCELLED'`：那样会让
//! 已取消工单的货再也扫不到，属产品决策。
//!
//! `t_assembly` 的 `uk_t_assembly_serial_no` 是**全量**唯一索引
//! （`WHERE deleted_at IS NULL AND serial_no IS NOT NULL`），活跃行必然唯一，
//! 故 `find_assembly_by_serial` 不需要排除 `CANCELLED` 的排序键，只 `LIMIT 1`。
//!
//! ## 批次查询：4 张 `LEFT JOIN` 的窄投影
//! 投影列沿 `GET /api/v2/parts/{id}/batches`（`part/service/phase1/
//! lifecycle_helpers.rs::list_batches`）的 holder 三表 `COALESCE` 写法，**相对
//! 那条 SQL 有 2 处收窄，各有理由**：
//!
//! 1. **不 JOIN `t_process_chain_step`**，工序名直接 `LEFT JOIN t_process ON
//!    t_process.id = b.current_process_id`。本端点的 `process_name` 契约就是
//!    「批次当前工序」，`current_process_id` 是工序归属的权威列（migration 004）；
//!    `current_process_step_id` 只是一次性写入、永不推进的显示用定位信息。
//! 2. **不 JOIN `t_delivery_note`**：`delivery_note_no` 不在本端点契约里。
//!
//! ⚠️ `process_name` 对 `INSPECTION` / `DELIVERED` 批次**恒为 `null`**：这两态
//! 按「出池清 `current_process_id`」不变式把该列置 NULL，是**正确**结果
//! （详见 [`super::mod`] 模块 doc）。
//!
//! ### 排序
//! `ORDER BY b.part_id ASC, b.batch_no ASC, b.id ASC` —— 前两键让同一请求的
//! 批次在内存分组后仍按零件顺序、零件内按批次号排列（前端展开树时不用二次
//! 排序）；`b.id ASC` 兜底拆批产生的同号批次（`uq_t_part_batch_part_no` 只保证
//! `(part_id, batch_no)` 唯一，软删行不参与约束）。
//!
//! ## ⚠️ 已知缺陷：holder 三表 COALESCE 的多态歧义（本次新增第 6 处）
//! `current_holder_display` 沿用 `COALESCE(s.name, w.name, oc.name)`
//! （`t_shelf` / `t_worker` / `t_outsource_company` 三表各 JOIN 一次），该写法
//! **假定** holder id 在三表 PK 空间里互不重叠；一旦某 id 同时命中其中两表，
//! 取到的是 `t_shelf.name`。
//!
//! 完整说明（6 处清单 + 正确解法 `CASE location …` + 为何不动）见
//! `prod::batch::repo` 模块 doc 的「holder 三表 COALESCE 的多态歧义」一节；
//! **本文件是该形态的全仓第 6 处**（4 处 `t_shelf.name` 形态含本文件 + 2 处
//! `t_shelf.code` 变体）。
//!
//! 本次刻意**不**修：修这一处会让 6 条 SQL 对部分历史脏数据的行为发生变化，
//! 属独立改动；且「先补脏数据清洗还是先改判别式」需要产品侧确认。**修时必须
//! 6 处一起改**，逐处改会造成同一 holder 在不同端点显示不同名字。
//!
//! ## 为什么 `t_part` 的 11 列投影要写两份字面量
//! `find_part_by_serial` 与 `list_parts_by_assembly` 的投影列完全相同，但
//! `query_as!` 宏要求 SQL 是**字面量**（不能插 `const` / 拼 `format!`），故只能
//! 各写一份。两份的 `SELECT` 列表必须同步改 —— 字段集由 [`ScanPartRow`] 唯一
//! 决定，宏编译期会校验两份都与该结构匹配。
//!
//! ## 错误类型
//! repo 静态方法 → `sqlx::Error`（与项目惯例一致），由 service 层映射 `AppError`。
//!
//! ## 队列读（`GET /queue`）的 WHERE 五段
//! 判据只此一份（私有 [`push_inspection_queue_where`]，list 与 count 共用，天然
//! 杜绝「count 与 items 各说各话」的分页 bug），逐段为：
//!
//! 1. 状态 + 双软删闸门：`pb.status = 'INSPECTION' AND pb.deleted_at IS NULL
//!    AND p.deleted_at IS NULL`（本端点不接 statuses 参数）
//! 2. 客户：空数组 → 命中全部；非空 → `p.customer_id = ANY(展开后的 L1+L2 ids)`
//! 3. 表头 3 个文本列各一个独立 ILIKE（`$n::text IS NULL` 短路 → 不过滤）
//! 4. 系统交期区间（两个可空边界，缺界不参与过滤）
//! 5. list 额外的 `ORDER BY {order_col} {order_dir} NULLS LAST, pb.id ASC LIMIT /
//!    OFFSET`（count 无此段）
//!
//! ⚠️ `ORDER BY` 的 `{order_col}` / `{order_dir}` 是**拼进 SQL 文本**的两个字符串，
//! 它们由 service 层的 `resolve_order_col` / `resolve_order_dir` 白名单映射产出，
//! repo 收不到任何外部输入（白名单映射放 service 比放 repo 更安全）。
//!
//! ⚠️ 排序必须带 `NULLS LAST`：`p.system_delivery_date` 可空，而 PG 默认
//! ASC → `NULLS LAST` / DESC → `NULLS FIRST`，不显式指定时按交期倒序会把未填交期的
//! 行顶到最前。`pb.id ASC` 是兜底键（排序列可重复，无兜底键时翻页会漏行 / 重复行），
//! 覆盖用例：`tests/part/inspection_batches.rs::inspection_batches_pagination_tiebreak_by_batch_id_is_stable`。
//!
//! ### `l1_customer_name` 的派生口径（两处并存，勿统一）
//! 本查询的口径：`c.parent_id IS NOT NULL` → `pc.name.or(c.name)`（pc 的 LEFT JOIN
//! 不带 `deleted_at` 过滤，父行在即取父名，父行悬空才回落 `c.name`）；否则
//! （自身即 L1）→ `c.name`。
//! ⚠️ 与 `prod::batch::service::repair::list_batches_matching` 的同名派生**口径不同**
//! （那条不回落 `c.name`：客户自身即 L1、或父客户被软删时为 null）。两处都是有意
//! 分叉，改任一侧都要同步另一侧的注释。

use chrono::NaiveDate;
use sqlx::{PgConnection, PgExecutor, Postgres, QueryBuilder};

use crate::modules::prod::inspection::model::{
    InspectionQueueRow, ScanAssemblyRow, ScanBatchRow, ScanPartRow,
};

/// `prod::inspection` ZST 静态方法容器。
pub struct InspectionScanRepo;

impl InspectionScanRepo {
    /// 按序列号精确命中零件（命中查询，含 `LEFT JOIN t_customer`）。
    ///
    /// 返回 `None` = 无活跃同号零件，调用方据此回退查 `t_assembly`
    /// （`serial_no` 在两表都有值域，回退是唯一能同时覆盖装配件条码的口径）。
    pub async fn find_part_by_serial(
        conn: &mut PgConnection,
        serial_no: &str,
    ) -> Result<Option<ScanPartRow>, sqlx::Error> {
        sqlx::query_as!(
            ScanPartRow,
            r#"
            SELECT
                p.id AS "id!",
                p.serial_no AS "serial_no?",
                p.name AS "name!",
                p.drawing_no AS "drawing_no!",
                p.status AS "status!",
                p.quantity AS "quantity!",
                p.is_urgent AS "is_urgent!",
                p.system_delivery_date AS "system_delivery_date?",
                p.version AS "version!",
                p.assembly_id AS "assembly_id?",
                c.name AS "customer_name?"
            FROM t_part p
            LEFT JOIN t_customer c ON c.id = p.customer_id AND c.deleted_at IS NULL
            WHERE p.serial_no = $1
              AND p.deleted_at IS NULL
            ORDER BY (p.status = 'CANCELLED') ASC, p.id DESC
            LIMIT 1
            "#,
            serial_no,
        )
        .fetch_optional(&mut *conn)
        .await
    }

    /// 按序列号精确命中装配件（扫到的是装配件条码时的第一跳）。
    ///
    /// `serial_no` 精确匹配，**不做**前缀 / `ILIKE` 模糊 —— 扫码枪给的是完整
    /// 序列号，模糊匹配会让 `F100` 命中 `F1001`/`F1002`，弹错树比不弹更糟。
    pub async fn find_assembly_by_serial(
        conn: &mut PgConnection,
        serial_no: &str,
    ) -> Result<Option<ScanAssemblyRow>, sqlx::Error> {
        sqlx::query_as!(
            ScanAssemblyRow,
            r#"
            SELECT
                a.id AS "id!",
                a.serial_no AS "serial_no?",
                a.name AS "name!",
                a.drawing_no AS "drawing_no!",
                a.status AS "status!",
                a.quantity AS "quantity!",
                a.is_urgent AS "is_urgent!",
                a.system_delivery_date AS "system_delivery_date?",
                c.name AS "customer_name?"
            FROM t_assembly a
            LEFT JOIN t_customer c ON c.id = a.customer_id AND c.deleted_at IS NULL
            WHERE a.serial_no = $1
              AND a.deleted_at IS NULL
            LIMIT 1
            "#,
            serial_no,
        )
        .fetch_optional(&mut *conn)
        .await
    }

    /// 按 id 取装配件（扫到的是**子件**条码时，取它所属的装配件节点）。
    ///
    /// 返回 `None` 只可能是「父装配件已软删」—— 调用方据此把该响应退化成独立件
    /// 树（`assembly = null` + `children = [被扫中的那个]`），而不是返回一棵
    /// 「有子件但没有装配件节点」的孤儿树。
    pub async fn find_assembly_by_id(
        conn: &mut PgConnection,
        assembly_id: i64,
    ) -> Result<Option<ScanAssemblyRow>, sqlx::Error> {
        sqlx::query_as!(
            ScanAssemblyRow,
            r#"
            SELECT
                a.id AS "id!",
                a.serial_no AS "serial_no?",
                a.name AS "name!",
                a.drawing_no AS "drawing_no!",
                a.status AS "status!",
                a.quantity AS "quantity!",
                a.is_urgent AS "is_urgent!",
                a.system_delivery_date AS "system_delivery_date?",
                c.name AS "customer_name?"
            FROM t_assembly a
            LEFT JOIN t_customer c ON c.id = a.customer_id AND c.deleted_at IS NULL
            WHERE a.id = $1
              AND a.deleted_at IS NULL
            LIMIT 1
            "#,
            assembly_id,
        )
        .fetch_optional(&mut *conn)
        .await
    }

    /// 装配件的**全部**子件（软删子件已被闸门排除）。
    ///
    /// 排序 `serial_no ASC NULLS LAST, id ASC` 沿用 `part` 域
    /// `list_by_assembly_id` 的口径：子件序列号由父件序列号派生（`{asm}-{i:02d}`），
    /// 该序即业务上的装配序；`NULLS LAST` 让没序列号的手工子件排在末尾。
    pub async fn list_parts_by_assembly(
        conn: &mut PgConnection,
        assembly_id: i64,
    ) -> Result<Vec<ScanPartRow>, sqlx::Error> {
        sqlx::query_as!(
            ScanPartRow,
            r#"
            SELECT
                p.id AS "id!",
                p.serial_no AS "serial_no?",
                p.name AS "name!",
                p.drawing_no AS "drawing_no!",
                p.status AS "status!",
                p.quantity AS "quantity!",
                p.is_urgent AS "is_urgent!",
                p.system_delivery_date AS "system_delivery_date?",
                p.version AS "version!",
                p.assembly_id AS "assembly_id?",
                c.name AS "customer_name?"
            FROM t_part p
            LEFT JOIN t_customer c ON c.id = p.customer_id AND c.deleted_at IS NULL
            WHERE p.assembly_id = $1
              AND p.deleted_at IS NULL
            ORDER BY p.serial_no ASC NULLS LAST, p.id ASC
            "#,
            assembly_id,
        )
        .fetch_all(&mut *conn)
        .await
    }

    /// 一批零件的**全部**批次（一条 SQL 覆盖整棵树的批次层，无 N+1）。
    ///
    /// `part_ids` 为空切片时**直接返空 Vec 而不发 SQL** —— `= ANY('{}')` 在 PG
    /// 里恒为 false、结果本就为空，提前返还能省一次网络往返（装配件无子件、
    /// 或全部子件被软删时就会走到这里）。
    ///
    /// ⚠️ **不按 `status` 过滤**（含 `COMPLETED` / `CANCELLED` 等终态）：与
    /// `GET /api/v2/parts/{id}/batches` 同口径，状态闸门由前端按批次 status
    /// 决定按钮显隐。
    pub async fn list_batches_by_part_ids(
        conn: &mut PgConnection,
        part_ids: &[i64],
    ) -> Result<Vec<ScanBatchRow>, sqlx::Error> {
        if part_ids.is_empty() {
            return Ok(Vec::new());
        }
        sqlx::query_as!(
            ScanBatchRow,
            r#"
            SELECT
                b.id AS "id!",
                b.part_id AS "part_id!",
                b.batch_no AS "batch_no!",
                b.quantity AS "quantity!",
                b.status AS "status!",
                b.version AS "version!",
                b.is_repairing AS "is_repairing!",
                b.location AS "location?",
                COALESCE(s.name, w.name, oc.name) AS "current_holder_display?",
                pr.name AS "process_name?"
            FROM t_part_batch b
            LEFT JOIN t_shelf s ON s.id = b.current_holder_id
            LEFT JOIN t_worker w ON w.id = b.current_holder_id
            LEFT JOIN t_outsource_company oc ON oc.id = b.current_holder_id
            LEFT JOIN t_process pr ON pr.id = b.current_process_id
            WHERE b.part_id = ANY($1)
              AND b.deleted_at IS NULL
            ORDER BY b.part_id ASC, b.batch_no ASC, b.id ASC
            "#,
            part_ids,
        )
        .fetch_all(&mut *conn)
        .await
    }
}

// ===========================================================================
//  待品检队列（`GET /api/v2/prod/inspection/queue`）
//  2026-10-07 自 `prod::batch::repo::list` 迁入，SQL 与派生口径逐字未改
// ===========================================================================

/// 待品检队列列表入参。
///
/// 排序项收的是**已白名单化的列名 / 方向**（`p.system_delivery_date` / `ASC`
/// 这类字面量），白名单映射在 service 层完成 —— repo 收不到任何外部输入，
/// 故拼进 SQL 文本的只有这两个受控字符串。
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

/// `GET /api/v2/prod/inspection/queue` 窄投影 SELECT（13 个输出列 + 派生 L1 名的 2 列原料）。
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

/// [`list_inspection_queue`](InspectionQueueRepo::list_inspection_queue) 的
/// `FromRow` 行结构（13 输出列 + `l1_customer_name` 的 2 列原料）。
///
/// 手动 `#[derive(FromRow)]` 而非 `query_as!` —— SQL 由 `QueryBuilder` 动态拼装。
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

/// 待品检队列读的 SQL 真源（ZST，与 [`InspectionScanRepo`] 同形、无状态）。
///
/// 2026-10-07 自 `prod::batch` 迁入；泛型 `E: PgExecutor<'e>` 形参保留（与迁前
/// 逐字一致），生产调用方传 `&mut PgConnection`。
pub struct InspectionQueueRepo;

impl InspectionQueueRepo {
    /// `GET /api/v2/prod/inspection/queue` 列表
    /// （3-JOIN 窄投影 + 表头筛选 + 服务端排序）。
    ///
    /// 排序：`{order_col} {order_dir} NULLS LAST, pb.id ASC`
    /// （`NULLS LAST` 与 `pb.id ASC` 的理由见本文件模块 doc 的「队列读」小节）。
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
                // l1_customer_name 派生：c.parent_id IS NOT NULL → pc.name.or(c.name)
                // （pc 的 LEFT JOIN 不带 deleted_at 过滤，父行在即取父名，父行悬空才
                // 回落 c.name）；否则（自身即 L1）→ c.name。与
                // `prod::batch::service::repair::list_batches_matching` 的同名派生
                // **口径不同**（那条不回落 c.name），两处都有登记，勿统一。
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

    /// `GET /api/v2/prod/inspection/queue` 配套 COUNT（与 `list_inspection_queue`
    /// 共用同一个 WHERE 拼装器，无 ORDER BY / LIMIT / OFFSET）。
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

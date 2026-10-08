//! 送货单扫码三层树的 SQL 真源（`GET /api/v2/com/delivery/note/scan/{serial_no}`）
//!
//! 与 `prod::inspection::repo::InspectionScanRepo` 同形同口径（同一个前端树组件
//! 服务两个域），差异只有 3 处，全部在 `occupied_by_note_no` 与两个 `entry_max_*`
//! 的取数方式上。
//!
//! ## 单次请求的 SQL 条数
//! 一次扫码按分支取 2~7 条 SQL，**无 N+1**（批次层一条 `part_id = ANY($1)`）。
//! 表里的条数由 `com::delivery_note::sql_count_guard` 里的登记表逐条钉住（改动本表的
//! 数字必须同步改那里，否则单测立刻红）：
//!
//! | 扫到的东西 | 依次执行的 SQL | 条数 |
//! |---|---|---:|
//! | 独立件 | `find_part_by_serial` → `list_batches_by_part_ids` → `list_entryable_batches_by_part_ids` → `l1_of`（`CustomerRepo::get_by_id`）→ `note_find_open_draft_by_l1` | 5 |
//! | 装配件子件（父装配件活跃） | `find_part_by_serial` → `find_assembly_by_id` → `list_parts_by_assembly` → `list_batches_by_part_ids` → `list_entryable_batches_by_part_ids` → `l1_of` → `note_find_open_draft_by_l1` | 7 |
//! | 装配件子件（父装配件已软删） | `find_part_by_serial` → `find_assembly_by_id` → `list_batches_by_part_ids` → `list_entryable_batches_by_part_ids` → `l1_of` → `note_find_open_draft_by_l1` | 6 |
//! | 装配件条码 | `find_part_by_serial`（**未命中探测**）→ `find_assembly_by_serial` → `list_parts_by_assembly` → `list_batches_by_part_ids` → `list_entryable_batches_by_part_ids` → `l1_of` → `note_find_open_draft_by_l1` | 7 |
//! | 两表皆未命中 | `find_part_by_serial` → `find_assembly_by_serial` | 2 |
//!
//! 恒 4 条的「尾巴」（命中之后无分支地执行）：`list_batches_by_part_ids`（批次层，
//! 一条覆盖整棵树）+ `list_entryable_batches_by_part_ids`（两个 `entry_max_*` 的分子）
//! + `l1_of`（客户 id → L1）+ `note_find_open_draft_by_l1`。
//!
//! 分支差异只发生在命中段：1 条（独立件）/ 3 条（父活跃）/ 2 条（父软删）/ 3 条
//! （装配件码，含那次未命中探测）。
//!
//! 边界：「装配件父活跃、但它一个未删子件都没有」时 `parts` 为空 ⇒ 尾巴里那两条批次
//! 查询被 repo 的空 `part_ids` 短路（见本文件 `list_batches_by_part_ids` /
//! `list_entryable_batches_by_part_ids` 的 `part_ids.is_empty()` 提前返）各少 1 条，
//! 即 4 − 2 = 2 条，不在上表口径内。
//!
//! ⚠️ 此时 `resolve_draft` **仍会**发完 `l1_of` + `note_find_open_draft_by_l1` ——
//! `hit.assembly` 是 `Some`，走 `Some(a) => a.customer_id` 分支；它那个
//! 「`parts.first()` 取不到就提前返 `None`」的分支不可达（`HitTree` 的四个构造点
//! 里两个 `assembly: None` 都同时把 `parts` 设成非空的 `vec![p]`）。
//!
//! ## 同一 `serial_no` 可能并存多行：取哪一行是写死的口径
//! `t_part` 的 `uk_t_part_serial_no` 是**部分**唯一索引（`WHERE serial_no IS NOT
//! NULL AND deleted_at IS NULL AND status <> 'CANCELLED'`），软删行 / `CANCELLED`
//! 行可与活跃行同号共存。命中查询按
//! `ORDER BY (p.status = 'CANCELLED') ASC, p.id DESC LIMIT 1` 取值：`CANCELLED`
//! 排到最后（扫到历史废弃工单毫无意义，工单 cancel 后同号重建是常规操作），剩余行
//! 由部分唯一索引保证至多一条。
//!
//! `t_assembly` 的 `uk_t_assembly_serial_no` 是**全量**唯一索引（`WHERE
//! deleted_at IS NULL AND serial_no IS NOT NULL`），活跃行必然唯一 ⇒ 按序列号命中
//! 只需 `LIMIT 1`。
//!
//! ⚠️ **只有 `CANCELLED` 行时照样返回正常树**：排序键只保证「活跃行优先」，不保证
//! 「必有活跃行」。此时 `part.status` 原文透出 `CANCELLED`，前端应自行禁用该节点上
//! 的写操作。刻意不加 `AND p.status <> 'CANCELLED'` —— 那会让已取消工单的货再也
//! 扫不到，属产品决策。
//!
//! ## 软删闸门
//! `t_part` / `t_assembly` / `t_part_batch` 三张表逐条 SQL 写死 `deleted_at IS
//! NULL`；`LEFT JOIN t_customer` 也带闸门（客户软删时客户名退化为 `null`，不影响树
//! 返回）。而 `LEFT JOIN` 进来的 `t_process` / `t_shelf` / `t_worker` /
//! `t_outsource_company` 四张**展示用附表不加**闸门（工序名 / holder 名被软删也照常
//! 显示最后的样子），与 `prod::batch::repo` 的既有写法一致。

use chrono::NaiveDate;

use crate::shared::batch::TPartBatch;

/// 扫码树命中 `t_part` 时的窄投影（11 列）。
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ScanPartRow {
    pub id: i64,
    pub serial_no: Option<String>,
    pub name: String,
    pub drawing_no: String,
    pub status: String,
    pub quantity: i32,
    pub is_urgent: bool,
    pub system_delivery_date: Option<NaiveDate>,
    pub version: i32,
    pub assembly_id: Option<i64>,
    pub customer_id: i64,
    pub customer_name: Option<String>,
}

/// 扫码树命中 `t_assembly` 时的窄投影（9 列）。
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ScanAssemblyRow {
    pub id: i64,
    pub serial_no: Option<String>,
    pub name: String,
    pub drawing_no: String,
    pub status: String,
    pub quantity: i32,
    pub is_urgent: bool,
    pub system_delivery_date: Option<NaiveDate>,
    pub customer_id: i64,
    pub customer_name: Option<String>,
}

/// 扫码树批次层的窄投影（11 列 + 占用方单号）。
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ScanBatchRow {
    pub id: i64,
    pub part_id: i64,
    pub batch_no: i32,
    pub quantity: i32,
    pub status: String,
    pub version: i32,
    pub is_repairing: bool,
    pub location: Option<String>,
    pub current_holder_display: Option<String>,
    pub process_name: Option<String>,
    /// 已占用该批次的送货单单号（`NULL` = 未占用 / 占用方已软删）。
    pub occupied_by_note_no: Option<String>,
}

/// 送货单扫码树的 ZST 静态方法容器（与 `InspectionScanRepo` 同形、无状态）。
pub struct DeliveryScanRepo;

impl DeliveryScanRepo {
    /// 按序列号精确命中零件（命中查询，含 `LEFT JOIN t_customer`）。
    ///
    /// 返回 `None` = 无活跃同号零件，调用方据此回退查 `t_assembly`（`serial_no` 在
    /// 两表都有值域，回退是唯一能同时覆盖装配件条码的口径）。不做前缀 / `ILIKE`
    /// 模糊 —— 扫码枪给的是完整序列号，模糊匹配会让 `F100` 命中 `F1001`/`F1002`。
    pub async fn find_part_by_serial(
        conn: &mut sqlx::PgConnection,
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
                p.customer_id AS "customer_id!",
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
    pub async fn find_assembly_by_serial(
        conn: &mut sqlx::PgConnection,
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
                a.customer_id AS "customer_id!",
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
    /// 返回 `None` 只可能是「父装配件已软删」—— 调用方据此把响应退化成独立件树
    /// （`assembly = null` + `children = [被扫中的那个]`），而不是返回一棵「有子件
    /// 但没有装配件节点」的孤儿树。
    pub async fn find_assembly_by_id(
        conn: &mut sqlx::PgConnection,
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
                a.customer_id AS "customer_id!",
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
        conn: &mut sqlx::PgConnection,
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
                p.customer_id AS "customer_id!",
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
    /// `part_ids` 为空切片时**直接返空 Vec 而不发 SQL**（`= ANY('{}')` 在 PG 里恒
    /// 为 false，提前返还省一次网络往返）。
    ///
    /// ⚠️ **不按 `status` 过滤**（含 `COMPLETED` / `CANCELLED` 等终态）：扫码弹窗要
    /// 回答「这批货总共分了几批、每批现在什么状态」，砍掉终态就答不了；状态闸门在前端。
    ///
    /// ⚠️ `occupied_by_note_no` 的 JOIN 带 `dn.deleted_at IS NULL`：被**软删**的单
    /// 占用的批次视为未占用（可以重新入单）。
    pub async fn list_batches_by_part_ids(
        conn: &mut sqlx::PgConnection,
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
                pr.name AS "process_name?",
                dn.delivery_note_no AS "occupied_by_note_no?"
            FROM t_part_batch b
            LEFT JOIN t_shelf s ON s.id = b.current_holder_id
            LEFT JOIN t_worker w ON w.id = b.current_holder_id
            LEFT JOIN t_outsource_company oc ON oc.id = b.current_holder_id
            LEFT JOIN t_process pr ON pr.id = b.current_process_id
            LEFT JOIN t_delivery_note dn
                   ON dn.id = b.delivery_note_id AND dn.deleted_at IS NULL
            WHERE b.part_id = ANY($1)
              AND b.deleted_at IS NULL
            ORDER BY b.part_id ASC, b.batch_no ASC, b.id ASC
            "#,
            part_ids,
        )
        .fetch_all(&mut *conn)
        .await
    }

    /// 一批零件的「可入单」批次（`READY_TO_SHIP` + 未占用），供
    /// `entry_max_quantity` 与 `entry_max_sets` 的分子取数。
    ///
    /// ⚠️ **这是「可入单」的唯一定义**（`POST /scan` 的分配闸门同一条判据）：状态
    /// 是 `READY_TO_SHIP`，且 `delivery_note_id` 为 NULL **或**指向的送货单已
    /// 软删（与 `list_batches_by_part_ids` 的 `dn.deleted_at IS NULL` 同口径）。
    /// 软删闸门 `b.deleted_at IS NULL` + `p.deleted_at IS NULL`（零件软删则其批次
    /// 不可入单）。
    ///
    /// 排序 `quantity ASC, batch_no ASC` —— 这是 DP 分配算法要求的候选序（见
    /// `service/batch_allocation.rs` 模块 doc）。
    pub async fn list_entryable_batches_by_part_ids(
        conn: &mut sqlx::PgConnection,
        part_ids: &[i64],
    ) -> Result<Vec<TPartBatch>, sqlx::Error> {
        if part_ids.is_empty() {
            return Ok(Vec::new());
        }
        sqlx::query_as!(
            TPartBatch,
            r#"
            SELECT b.id, b.part_id, b.batch_no, b.quantity, b.status, b.location,
                   b.current_holder_id, b.current_process_id, b.current_process_step_id,
                   b.delivery_note_id, b.delivery_seq, b.parent_batch_id,
                   b.is_repairing,
                   b.version, b.created_at, b.created_by, b.updated_at, b.updated_by,
                   b.deleted_at
            FROM t_part_batch b
            JOIN t_part p ON p.id = b.part_id
            LEFT JOIN t_delivery_note dn
                   ON dn.id = b.delivery_note_id AND dn.deleted_at IS NULL
            WHERE b.part_id = ANY($1)
              AND b.status = 'READY_TO_SHIP'
              AND b.deleted_at IS NULL
              AND p.deleted_at IS NULL
              AND (b.delivery_note_id IS NULL OR dn.id IS NULL)
            ORDER BY b.quantity ASC, b.batch_no ASC
            "#,
            part_ids,
        )
        .fetch_all(&mut *conn)
        .await
    }
}

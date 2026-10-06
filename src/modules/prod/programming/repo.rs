//! prod::programming 子模块 repo 层 —— SQL 真源
//!
//! 2026-10-01 新增：本域 SQL 全部集中在本文件（ZST `ProgrammingRepo` + 2 静态
//! 方法 `list` / `count`），不引入胖 trait（与 `prod::batch::BatchRepo` 同形 ——
//! 单 service 不需要 mock 替身，service 收 `&mut PgConnection` 直调 ZST）。
//!
//! ## 过滤谓词（三规则并集 + part 状态闸门，part 级去重）
//! ```sql
//! FROM t_part p
//! LEFT JOIN t_customer c  ON c.id = p.customer_id          -- L2 叶子客户
//! LEFT JOIN t_customer pc ON pc.id = c.parent_id           -- L1 一级集团
//! WHERE p.deleted_at IS NULL
//!   AND p.status IN ('PENDING','IN_PROCESS','PROGRAMMING')  -- 三规则共用的状态闸门
//!   AND (
//!        -- 规则1：兼容旧筛选（历史 PROGRAMMING 状态仍允许消化）
//!        p.status = 'PROGRAMMING'
//!     OR -- 规则2：工单工艺链上含 CNC 工序
//!        EXISTS (SELECT 1 FROM t_process_chain_step s
//!                JOIN t_process pr ON pr.id = s.process_id AND pr.deleted_at IS NULL
//!               WHERE s.chain_id = p.process_chain_id
//!                 AND s.deleted_at IS NULL
//!                 AND pr.is_cnc = TRUE)
//!     OR -- 规则3：工单批次当前挂在 CNC 工序
//!        EXISTS (SELECT 1 FROM t_part_batch pb
//!                JOIN t_process pr ON pr.id = pb.current_process_id AND pr.deleted_at IS NULL
//!               WHERE pb.part_id = p.id
//!                 AND pb.deleted_at IS NULL
//!                 AND pb.status IN ('PENDING','IN_PROCESS','PROGRAMMING')
//!                 AND pr.is_cnc = TRUE)
//!   )
//!   AND ( <$has_cnc IS NULL> OR EXISTS (t_part_file kind='G_CODE') = <$has_cnc> )
//!   [AND (p.name ILIKE $kw ESCAPE '\' OR p.drawing_no ILIKE $kw ESCAPE '\' OR p.serial_no ILIKE $kw ESCAPE '\')]
//!   [AND p.serial_no = $serial_no]
//! ORDER BY <白名单列> <ASC|DESC> NULLS LAST, p.id DESC
//! LIMIT $limit OFFSET $offset
//! ```
//!
//! - 规则1：工单状态仍是 PROGRAMMING（该状态仍允许消化）
//! - 规则2：工单绑定的工艺链上有任一 `is_cnc` 工序 step（编程员据此进生产流）
//! - 规则3：工单存在在制批次，其当前工序就是 CNC 工序（链可能还没建，先由批次定位）
//!
//! ## ⚠️ part 状态闸门约束**全部三条规则**
//! `p.status IN ('PENDING','IN_PROCESS','PROGRAMMING')` 写在 `WHERE` 骨架最外层
//! （与 `p.deleted_at IS NULL` 同级、在三规则括号**之外**），因此规则2/3 同样受其约束。
//! 起因：`t_part.process_chain_id` **从不清空**，而规则2（链含 CNC 工序）本身不看
//! part 状态 → 历史上挂过 CNC 链的 `COMPLETED` / `CANCELLED` / `DELIVERED` /
//! `READY_TO_SHIP` 工单会**永久**命中「待编程一览」。
//! 回归测试：`tests/production/pending_programming.rs::part_status_gate_excludes_completed_and_delivered`。
//!
//! ## ⚠️ 规则3 必须用 `t_part_batch.current_process_id`（2026-10-01）
//! **严禁**改引 `t_part_batch.next_process_id`：该列已被 archive/028 DROP，
//! canonical baseline（`migrations/20260925000000_001_baseline.sql`）里**没有**
//! 这列——只有开发库因 `pg_restore` 旧备份残留才看得到它。在干净库上引用会直接
//! 500（`column "next_process_id" does not exist`）。
//! `current_process_id` 是 migration 004（`20260930000000_004_add_batch_current_process_id.sql`）
//! 引入的**批次工序归属唯一权威依据**，下发时由
//! `BatchRepo::update_batch_dispatched` 写入，worker_pool 候选池 3 条 SQL 也按它
//! 普通过滤。
//!
//! ## `has_cnc_program` 真相源
//! [`G_CODE_EXISTS`] —— `EXISTS (SELECT 1 FROM t_part_file WHERE part_id = p.id
//! AND kind = 'G_CODE' AND deleted_at IS NULL)`，与 worker_pool 候选池同源。
//! **同一常量**同时供 list 的 SELECT 列表与 WHERE 过滤复用（见该常量
//! doc 的改一同步二约定）。
//!
//! ## 批次锚点（2026-10-03 新增）
//! SELECT 列表挂一条 [`PROGRAMMING_BATCH_JOIN`]（`LEFT JOIN LATERAL`），取该 part 的
//! PROGRAMMING 活跃批次 id + `t_part_batch.version`，供前端拼
//! `POST /api/v2/prod/batches/{batch_id}/release-from-programming`。该端点以批次为锚
//! 且对批次做 OCC，故 part 级 `p.version` 无法替代。口径细节见该常量 doc。
//!
//! ## list / count 共用谓词
//! 两个方法共用私有 [`push_where`]（WHERE 骨架 + `has_cnc_program` + keyword +
//! `serial_no` 四段）与常量 [`FROM_SQL`]：谓词只写一份，list 与 count 不可能各自
//! 漂移出「`total` 与 `items` 对不上」的组合。
//!
//! ## SQL 拼接策略
//! 走 `sqlx::QueryBuilder`：固定骨架（FROM / WHERE / 白名单列名）走 `push` / `format!` 嵌入，动态入参走
//! `push_bind`。**不**使用 `query!` / `query_as!` 宏（动态 SQL 无法在编译期
//! 固化，且会污染 `.sqlx/` 离线元数据），行结构手动 `#[derive(sqlx::FromRow)]`。
//!
//! ## 错误类型
//! repo 静态方法 → `sqlx::Error`（与项目惯例一致），由 service 层映射 `AppError`。

use chrono::NaiveDate;
use sqlx::{FromRow, PgConnection, Postgres, QueryBuilder};

/// `prod::programming` ZST 静态方法容器。
pub struct ProgrammingRepo;

/// list / count 共用的 FROM 子句（客户 L1 / L2 两级 LEFT JOIN）。
///
/// 单独抽成常量而非各写一遍：`c.id = p.customer_id` / `pc.id = c.parent_id` 均走
/// 主键 JOIN，不产生行放大，count 复用它同样安全。
///
/// ⚠️ 客户侧**故意不过滤 `deleted_at`**（2026-10-01 review 第 1 轮 F 项确认）：
/// 与本文件其余 5 处严格软删过滤（`p` / `pb` / `pr` / `s` / `t_part_file`）的
/// 差异是**有意的** —— 历史工单需要显示其原客户名（软删客户后工单仍在册，名字不能
/// 变空）。若将来要改这一口径，须同步评估历史列表页的展示回归。
const FROM_SQL: &str = " FROM t_part p \
     LEFT JOIN t_customer c  ON c.id = p.customer_id \
     LEFT JOIN t_customer pc ON pc.id = c.parent_id";

/// 「该 part 是否已上传 G_CODE」的单条 EXISTS 表达式（`has_cnc_program` 真相源）。
///
/// 2026-10-01 review 第 1 轮 B 项：**改一必须同步改二** —— 本常量同时被
/// ① `list` 的 SELECT 列表（`{G_CODE_EXISTS} AS has_cnc_program`，即返回给前端的
/// 字段值）② `push_where` 的三态过滤（`{G_CODE_EXISTS} = $n`）复用。
/// 若只改 WHERE 侧（kind 字面量 / 软删条件）而漏改 SELECT 侧，会出现
/// 「`has_cnc_program` 字段值与过滤口径不一致」且无任何测试报警；
/// 集成测试 `soft_delete_filters_exclude_rows` 会覆盖 `deleted_at` 维度的漂移。
const G_CODE_EXISTS: &str = "EXISTS (SELECT 1 FROM t_part_file pf \
     WHERE pf.part_id = p.id AND pf.kind = 'G_CODE' AND pf.deleted_at IS NULL)";

/// list 的 SELECT 列表专用 LATERAL 子查询（2026-10-03 新增）：取该 part 的
/// **PROGRAMMING 活跃批次**（雪花 id + 批次 OCC 版本号）。
///
/// 只取 PROGRAMMING 是因为本列表的唯一写出口
/// `POST /api/v2/prod/batches/{batch_id}/release-from-programming` 硬要求源状态
/// 是 PROGRAMMING（`prod/batch/service/programming.rs` 的 `from != PROGRAMMING`
/// 直接 20103）——给 PENDING / IN_PROCESS 批次的 id 等于给前端一个必然失败的锚点。
/// 同 part 有多个 PROGRAMMING 批次时 `ORDER BY pb.id DESC LIMIT 1` 取最新
/// （雪花 ID 随时间单调递增）；无 PROGRAMMING 批次 → 两列都 `NULL`
/// （LEFT JOIN 语义），前端据此禁用「下发」按钮。
///
/// **与 `G_CODE_EXISTS` 的「改一同步二」义务不同**：本片段是**纯 SELECT 投影**，
/// WHERE 侧不引用它，故不存在改一处漏另一处的漂移面。两个 alias 名
/// （`batch_id` / `batch_version`）必须与 [`ProgrammingRow`] 同名字段对齐，
/// 否则 `FromRow` 取不到值。
///
/// 刻意**不**放进 [`FROM_SQL`]：`ProgrammingRepo::count` 只需要行数，不需要批次锚点，
/// 放进去会给每个待计数的 part 白跑一次相关子查询。
const PROGRAMMING_BATCH_JOIN: &str = " LEFT JOIN LATERAL ( \
     SELECT pb.id AS batch_id, pb.version AS batch_version \
     FROM t_part_batch pb \
     WHERE pb.part_id = p.id \
       AND pb.deleted_at IS NULL \
       AND pb.status = 'PROGRAMMING' \
     ORDER BY pb.id DESC \
     LIMIT 1) pb_prog ON true";

/// list / count 共用的 WHERE 骨架（part 状态闸门 + 三规则并集，不含四个动态段）。
///
/// ⚠️ `p.status IN (...)` 状态闸门（2026-10-01 review 第 1 轮 A 项）约束**全部
/// 三条规则**，详见模块 doc「part 状态闸门」段。
const WHERE_SKELETON: &str = " WHERE p.deleted_at IS NULL \
   AND p.status IN ('PENDING','IN_PROCESS','PROGRAMMING') \
   AND ( \
     p.status = 'PROGRAMMING' \
     OR EXISTS (SELECT 1 FROM t_process_chain_step s \
                JOIN t_process pr ON pr.id = s.process_id AND pr.deleted_at IS NULL \
               WHERE s.chain_id = p.process_chain_id \
                 AND s.deleted_at IS NULL \
                 AND pr.is_cnc = TRUE) \
     OR EXISTS (SELECT 1 FROM t_part_batch pb \
                JOIN t_process pr ON pr.id = pb.current_process_id AND pr.deleted_at IS NULL \
               WHERE pb.part_id = p.id \
                 AND pb.deleted_at IS NULL \
                 AND pb.status IN ('PENDING','IN_PROCESS','PROGRAMMING') \
                 AND pr.is_cnc = TRUE) \
   )";

/// 列表入参（service 层规范化后传入 repo）。
///
/// 独立 struct，不污染其它域的 Filters 类型。`keyword` / `serial_no` 在 service
/// 层已 trim 且把空串收敛成 `None`。
#[derive(Debug, Clone, Default)]
pub struct ProgrammingFilters {
    pub keyword: Option<String>,
    pub serial_no: Option<String>,
    pub sort_by: Option<String>,
    pub sort_dir: Option<String>,
    pub limit: i64,
    pub offset: i64,
    pub has_cnc_program: Option<bool>,
}

impl ProgrammingRepo {
    /// 待编程列表（part 级去重 + 客户 L1/L2 JOIN）。
    ///
    /// 返回 `Vec<ProgrammingRow>` —— service 内转换为 `vo::ProgrammingItemOut`。
    pub async fn list(
        conn: &mut PgConnection,
        f: &ProgrammingFilters,
    ) -> Result<Vec<ProgrammingRow>, sqlx::Error> {
        let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(format!(
            "SELECT p.id, p.version, p.serial_no, p.name, p.drawing_no, p.quantity, \
                    p.status, p.is_urgent, p.planned_delivery_date, p.system_delivery_date, \
                    c.name AS customer_name, pc.name AS parent_customer_name, \
                    {G_CODE_EXISTS} AS has_cnc_program, \
                    pb_prog.batch_id AS batch_id, pb_prog.batch_version AS batch_version \
             {FROM_SQL}{PROGRAMMING_BATCH_JOIN}"
        ));
        push_where(&mut qb, f);

        let (order_col, order_dir) = order_by(f);
        qb.push(format!(
            " ORDER BY {order_col} {order_dir} NULLS LAST, p.id DESC LIMIT "
        ));
        qb.push_bind(f.limit);
        qb.push(" OFFSET ");
        qb.push_bind(f.offset);

        let rows: Vec<ProgrammingRow> = qb.build_query_as().fetch_all(&mut *conn).await?;
        Ok(rows)
    }

    /// 列表配套 COUNT（与 `list` 共用 [`push_where`] 与 [`FROM_SQL`]）。
    pub async fn count(
        conn: &mut PgConnection,
        f: &ProgrammingFilters,
    ) -> Result<i64, sqlx::Error> {
        let mut qb: QueryBuilder<Postgres> =
            QueryBuilder::new(format!("SELECT COUNT(*)::bigint AS n {FROM_SQL}"));
        push_where(&mut qb, f);

        let (n,): (i64,) = qb.build_query_as().fetch_one(&mut *conn).await?;
        Ok(n)
    }
}

/// list / count 共用的 WHERE 拼装（2026-10-01 新增）。
///
/// 五段：⓿ part 状态闸门（并入 [`WHERE_SKELETON`]）① 三规则骨架 ② `has_cnc_program`
/// 三态 ③ keyword 模糊（通配符已转义）④ `serial_no` 精确。
/// ②③④ 按入参有无动态追加，故必须集中在这里——**只此一份**。
///
/// 注：本仓 sqlx 为 0.9，`QueryBuilder<DB>` 已无生命周期参数（0.7 时代是
/// `QueryBuilder<'_, DB>`），故签名写作 `&mut QueryBuilder<Postgres>`。
fn push_where(qb: &mut QueryBuilder<Postgres>, f: &ProgrammingFilters) {
    qb.push(WHERE_SKELETON);

    // 段②：has_cnc_program 三态（None → 恒真不过滤；Some → EXISTS 结果相等）
    // 与 SELECT 列表复用同一常量 G_CODE_EXISTS（改一同步二，见该常量 doc）
    qb.push(" AND ( ");
    qb.push_bind(f.has_cnc_program);
    qb.push("::bool IS NULL OR ");
    qb.push(G_CODE_EXISTS);
    qb.push(" = ");
    qb.push_bind(f.has_cnc_program);
    qb.push("::bool )");

    // 段③：keyword 模糊（name / drawing_no / serial_no 任一命中）
    // 2026-10-01 review 第 1 轮 G 项：用户 keyword 里的 `%` / `_` / `\` 走
    // escape_like 转义 + `ESCAPE '\'`，`keyword=50%` 只命中字面量含 `50%` 的行，
    // 不再退化成通配全匹配（注入面本就为 0 —— 走 push_bind）。
    if let Some(kw) = f.keyword.as_deref() {
        let pat = format!("%{}%", escape_like(kw));
        qb.push(" AND (p.name ILIKE ")
            .push_bind(pat.clone())
            .push(" OR p.drawing_no ILIKE ")
            .push_bind(pat.clone())
            .push(" OR p.serial_no ILIKE ")
            .push_bind(pat)
            .push(" ESCAPE '\\')");
    }

    // 段④：serial_no 精确
    if let Some(sn) = f.serial_no.as_deref() {
        qb.push(" AND p.serial_no = ").push_bind(sn.to_string());
    }
}

/// `ILIKE` 模式串的通配符转义（2026-10-01 review 第 1 轮 G 项）。
///
/// 把用户 `keyword` 里的 `\` / `%` / `_` 三个 LIKE 元字符各前置一个 `\`，
/// 配合 SQL 侧 `ESCAPE '\'` 使用：`keyword=50%` 只会命中**字面量**含 `50%` 的行，
/// `keyword=a_b` 不会把 `a` + 任意字符 + `b` 全扫进来。
/// 转义顺序固定「先补 `\`、再 push 原字符」，保证 `\` 自身也被正确转义（先转义
/// `\` 才不会出现 `\%` 被二次解读）。
fn escape_like(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len() + 8);
    for ch in raw.chars() {
        if matches!(ch, '\\' | '%' | '_') {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

/// 排序白名单（Rust 侧 `match` 兜底，杜绝 SQL 注入面）。
///
/// 未命中 / 缺省 → `p.planned_delivery_date`；方向仅识别 `DESC`（大小写不敏感），
/// 其余一律 `ASC`。
fn order_by(f: &ProgrammingFilters) -> (&'static str, &'static str) {
    let col = match f.sort_by.as_deref().unwrap_or("") {
        "CREATED_AT" => "p.created_at",
        "UPDATED_AT" => "p.updated_at",
        "PLANNED_DELIVERY_DATE" => "p.planned_delivery_date",
        "REQUEST_DATE" => "p.request_date",
        "SERIAL_NO" => "p.serial_no",
        "DRAWING_NO" => "p.drawing_no",
        "NAME" => "p.name",
        _ => "p.planned_delivery_date",
    };
    let dir = match f.sort_dir.as_deref().unwrap_or("") {
        d if d.eq_ignore_ascii_case("DESC") => "DESC",
        _ => "ASC",
    };
    (col, dir)
}

/// `list` 的行结构（`FromRow`，手写而非 `query_as!` —— SQL 动态拼装）。
///
/// 字段与 `vo::ProgrammingItemOut` 一一对应（15 字段）。
#[derive(Debug, Clone, FromRow)]
pub struct ProgrammingRow {
    pub id: i64,
    pub version: i32,
    pub serial_no: Option<String>,
    pub name: String,
    pub drawing_no: String,
    pub quantity: i32,
    pub status: String,
    pub is_urgent: bool,
    /// DB `NOT NULL`；行结构按 `Option` 收以便 service 统一做日期兜底。
    pub planned_delivery_date: Option<NaiveDate>,
    pub system_delivery_date: Option<NaiveDate>,
    pub customer_name: Option<String>,
    pub parent_customer_name: Option<String>,
    pub has_cnc_program: bool,
    /// 2026-10-03 新增：PROGRAMMING 活跃批次 id（[`PROGRAMMING_BATCH_JOIN`] 投影，
    /// alias 必须同名）；无该状态批次 → `None`。
    pub batch_id: Option<i64>,
    /// 2026-10-03 新增：同 `batch_id` 批次的 `t_part_batch.version`（批次 OCC）。
    pub batch_version: Option<i32>,
}

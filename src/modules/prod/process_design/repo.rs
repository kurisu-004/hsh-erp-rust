//! prod::process_design 子模块 repo 层 —— SQL 真源
//!
//! 2026-10-05 新增：本域 SQL 全部集中在本文件（ZST `ProcessDesignRepo` + 2 静态
//! 方法 `list` / `count`），不引入胖 trait（与 `prod::programming::ProgrammingRepo`
//! 同形 —— 单 service 不需要 mock 替身，service 收 `&mut PgConnection` 直调 ZST；
//! 另见 `docs/repo-naming.md` §3.1，本模块 ZST 无 trait 命名问题）。
//!
//! ## 过滤谓词（软删闸门 + PENDING 状态闸门）
//! ```sql
//! SELECT p.id, p.version, p.serial_no, p.name, p.drawing_no,
//!        p.process_chain_id, p.assembly_id
//! FROM t_part p
//! WHERE p.deleted_at IS NULL
//!   AND p.status = 'PENDING'
//! ORDER BY p.serial_no {ASC|DESC} NULLS LAST, p.id DESC
//! LIMIT $limit OFFSET $offset
//! ```
//!
//! ## ⚠️ 这里**刻意没有** `AND assembly_id IS NULL`（2026-10-05）
//! 被替换的 part 域 `GET /parts` 在 service 层硬置 `part_only: true`，repo 据此追加
//! 该守卫，把**装配件的子零件全部排除**（`t_part.assembly_id` 是子件指向父装配件的
//! 逻辑 FK）。本页需要「所有还没定工序的零件」，子件当然在内 —— 加回这道守卫等于让
//! 装配件的子零件重新从「制定工序」页消失，正是本次改动的起因。
//!
//! **后人不要"好心"把这道守卫加回来。** 详见 [`super`] 模块 doc 的警示段；回归测试
//! `tests/production/process_design.rs::assembly_child_parts_are_visible` 锁住该行为。
//!
//! ## list / count 共用谓词（从结构上杜绝漂移）
//! 两个方法共用 `FROM_SQL` 与 `WHERE_SKELETON` 两个私有常量。part 域旧端点把同一段
//! 谓词手抄两遍（list 一份、count 一份），改一处漏一处就会让 `total` 与 `items`
//! 对不上；本文件从结构上杜绝这种漂移。
//!
//! 刻意**不**引入 `SELECT_COLS` 之外的共享：`count` 只需行数，而把 7 列投影塞进
//! `SELECT COUNT(*) FROM t_part p` 会因**缺 `GROUP BY` 直接 SQL 报错**（PG 要求非聚合列
//! 必须出现在 `GROUP BY` 里，否则报 `column "p.id" must appear in the GROUP BY clause`），
//! 故 `SELECT_COLS` 只被 `list` 引用。
//!
//! ## ⚠️ 排序是**字典序**不是数值序（2026-10-05 口径确认，**不是缺陷**）
//! `serial_no` 是 `varchar(15)`，排序按字符比较：`F10` 排在 `F2` **前面**。
//! 这与 part 域旧端点传 `sort_by=SERIAL_NO` 时的行为同构（同一列、同一 collation），
//! 前端从旧端点切过来排序观感不变，故**按现状保留**。若将来要改数值序，只能改列
//! （如加数值列）或改前端本地排序，**不要**在本 SQL 里写 `NULLIF(serial_no,'')::int`
//! 这类表达式 —— 序列号里混着非纯数字前缀（`F1001-01` 等），转换会直接报错。
//!
//! ## `NULLS LAST` 是显式写死的（两个方向都适用）
//! `serial_no` 可空（手工工单没序列号）。PG 的 `ASC` 默认是 `NULLS LAST`、但 `DESC`
//! 默认是 `NULLS FIRST` —— 升序不加显式子句与降序的观感会不一致。故
//! **两种方向都显式带 `NULLS LAST`**，让「没序列号的件」在两种方向下都排在末尾，
//! 不会被升到最前面抢眼。
//!
//! ## SQL 拼接策略
//! 走 `sqlx::QueryBuilder`：固定骨架（SELECT 列表 / FROM / WHERE）走 `push` /
//! `format!` 嵌入，动态入参（`LIMIT` / `OFFSET`）走 `push_bind`。**不**使用
//! `query!` / `query_as!` 宏 —— `sort_dir` 无法在编译期固化（白名单收敛成
//! `ASC` / `DESC` 两个字面量），且宏会污染 `.sqlx/` 离线元数据。行结构手写
//! `#[derive(sqlx::FromRow)]`。
//!
//! ## 错误类型
//! repo 静态方法 → `sqlx::Error`（与项目惯例一致），由 service 层映射 `AppError`。

use sqlx::{FromRow, PgConnection, Postgres, QueryBuilder};

/// `prod::process_design` ZST 静态方法容器。
pub struct ProcessDesignRepo;

/// list 专用的 SELECT 列表（7 列，与 [`ProcessDesignRow`] 一一对应）。
///
/// 刻意**不**被 `count` 引用：`count` 只需 `COUNT(*)::bigint`，而这 7 列一旦出现在
/// `SELECT COUNT(*)` 里，会因**缺 `GROUP BY` 直接 SQL 报错**（PG 要求非聚合列进
/// `GROUP BY`）—— 是硬报错，不是「多读几列」的效率取舍。
const SELECT_COLS: &str = "p.id, p.version, p.serial_no, p.name, p.drawing_no, \
     p.process_chain_id, p.assembly_id";

/// list / count 共用的 FROM 子句。
const FROM_SQL: &str = " FROM t_part p";

/// list / count 共用的 WHERE 骨架（软删闸门 + PENDING 状态闸门）。
///
/// ⚠️ 本常量**不含**任何装配件相关谓词 —— `PENDING` 是本页业务闸门（写死，不暴露成
/// query 参数），而装配件子件必须保留（见模块 doc 警示段）。
const WHERE_SKELETON: &str = " WHERE p.deleted_at IS NULL \
   AND p.status = 'PENDING'";

/// 列表入参（service 层规范化后传入 repo）。
///
/// 独立 struct，不污染其它域的 Filters 类型。
///
/// ⚠️ 刻意**不** derive `Default`：全仓零调用点（service 层总是 3 字段全量显式构造），
/// 且 `Default` 会造出 `limit: 0` 的实例，直接进 SQL 就是 `LIMIT 0` 返空列表 —— 比
/// 「编译不过」难查得多。
#[derive(Debug, Clone)]
pub struct ProcessDesignFilters {
    pub sort_dir: Option<String>,
    pub limit: i64,
    pub offset: i64,
}

impl ProcessDesignRepo {
    /// 待制定工序的零件列表（**含装配件子件**）。
    ///
    /// 返回 `Vec<ProcessDesignRow>` —— service 内转换为
    /// `vo::ProcessDesignPartItemOut`。
    pub async fn list(
        conn: &mut PgConnection,
        f: &ProcessDesignFilters,
    ) -> Result<Vec<ProcessDesignRow>, sqlx::Error> {
        let mut qb: QueryBuilder<Postgres> =
            QueryBuilder::new(format!("SELECT {SELECT_COLS}{FROM_SQL}"));
        qb.push(WHERE_SKELETON);

        let dir = order_dir(f);
        qb.push(format!(
            " ORDER BY p.serial_no {dir} NULLS LAST, p.id DESC LIMIT "
        ));
        qb.push_bind(f.limit);
        qb.push(" OFFSET ");
        qb.push_bind(f.offset);

        let rows: Vec<ProcessDesignRow> = qb.build_query_as().fetch_all(&mut *conn).await?;
        Ok(rows)
    }

    /// 列表配套 COUNT（与 `list` 共用 `FROM_SQL` 与 `WHERE_SKELETON`）。
    ///
    /// 与 list 同谓词，故 `total` 必然等于「若不翻页能拿到的行数」。
    ///
    /// ⚠️ 入参 `_f` **当前刻意不使用**：`WHERE_SKELETON` 是全静态的（软删闸门 +
    /// PENDING 状态闸门，无任何动态段），而 [`ProcessDesignFilters`] 三个字段
    /// （`sort_dir` / `limit` / `offset`）按定义都不影响行数 —— 排序与分页不该改变
    /// `total`。参数保留是为了与 `list` 签名对称：将来若新增 `keyword` 之类**真过滤**
    /// 维度，两个方法都能拿到同一份 filters，届时去掉下划线即可，service 侧调用点
    /// 无需改动。
    pub async fn count(
        conn: &mut PgConnection,
        _f: &ProcessDesignFilters,
    ) -> Result<i64, sqlx::Error> {
        let mut qb: QueryBuilder<Postgres> =
            QueryBuilder::new(format!("SELECT COUNT(*)::bigint AS n{FROM_SQL}"));
        qb.push(WHERE_SKELETON);

        let (n,): (i64,) = qb.build_query_as().fetch_one(&mut *conn).await?;
        Ok(n)
    }
}

/// 排序方向白名单（Rust 侧 `match` 兜底，杜绝 SQL 注入面）。
///
/// 排序键固定 `p.serial_no`（不暴露 `sort_by`），故本函数只收敛方向：仅识别 `DESC`
/// （大小写不敏感），其余一律 `ASC`。返回的是两个 `&'static str` 字面量，不是用户
/// 输入。
fn order_dir(f: &ProcessDesignFilters) -> &'static str {
    match f.sort_dir.as_deref().unwrap_or("") {
        d if d.eq_ignore_ascii_case("DESC") => "DESC",
        _ => "ASC",
    }
}

/// `list` 的行结构（`FromRow`，手写而非 `query_as!` —— SQL 动态拼装）。
///
/// 字段与 `vo::ProcessDesignPartItemOut` 一一对应（7 字段），alias 名与
/// `SELECT_COLS` 的列名逐字对齐，否则 `FromRow` 取不到值。
#[derive(Debug, Clone, FromRow)]
pub struct ProcessDesignRow {
    pub id: i64,
    pub version: i32,
    pub serial_no: Option<String>,
    pub name: String,
    pub drawing_no: String,
    /// 已绑定的工艺链 id；`None` = 尚未制定工序。
    pub process_chain_id: Option<i64>,
    /// 所属装配件 id；`None` = 独立零件，非 `None` = 装配件子件（**不因此被过滤**）。
    pub assembly_id: Option<i64>,
}

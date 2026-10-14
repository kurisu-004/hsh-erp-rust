//! `serial` 域数据访问层（**2026-10-19 新建**）
//!
//! 本域**没有自有表**：候选集横跨 `t_assembly`（装配件）与 `t_part`（零件 / 子件）
//! 两张既有表，且这两个域本体的 repo 都不暴露「按终态 + 持号」的查询 ⇒ 候选 SELECT
//! 只能由本域自写。
//!
//! ## ⚠️ 守卫谓词是**唯一真源**，count / list / apply 三条路径共用
//!
//! [`TERMINAL_PREDICATE`] 是「什么算本端点要清的工单」的唯一定义。历史上这类
//! 「统计一条 SQL、干活另一条 SQL」的写法必然分叉：统计说 5 行、真跑动 4 行，
//! 而用户看到的确认框写的是统计那个数。
//!
//! 复用同一常量（而不是在三条 SQL 里各抄一份）还有一层更硬的理由：apply 路径调用的
//! 两个**既有原语**（`AssemblyRepo::clear_serial_no_if_terminal` 与
//! `shared::batch::status::release_part_serial_no`）**各自内联了同样的三条守卫**
//! （`deleted_at IS NULL` / `serial_no IS NOT NULL` / `status IN ('COMPLETED','CANCELLED')`）。
//! 本域的 SELECT 若与它们漂移，就会出现「候选选出来了、原语却 0 行」或反之 ——
//! 后者更糟：明细里列着一行、实跑没清它。三处谓词**必须逐字同源**，本文件是本域
//! 这一侧的唯一副本，对照基准写在 [`TERMINAL_PREDICATE`] 的 doc 里。
//!
//! ## 全部走 `sqlx::query` / `query_as`（**运行时校验**），不进 `query!` 宏
//!
//! 与仓内既有 repoid 同款取舍 ⇒ 本域新增 SQL **不需要**跑
//! `./scripts/sqlx_prepare.sh`、不产生 `.sqlx/` 缓存 diff。
//!
//! ## `AssertSqlSafe` 包裹：`format!` 的注入面为 0
//!
//! 四条 SQL 都由 `format!` 拼出（为了让谓词 / 投影 / `LIMIT` / `FOR UPDATE`
//! 共享同一份常量与开关，避免逐条抄写漂移），故按 sqlx 的 `SqlSafeStr` 要求
//! 包一层 `AssertSqlSafe`。**逐处填进去的只有编译期常量** —— [`TERMINAL_PREDICATE`] /
//! [`PROJECTION`] / `limit_sql`（其入参是 `Option<i64>`，无字符串可注入）/ `lock_sql`
//! —— 本域**没有任何一条 SQL 接收用户输入**，用户数据一律走 bind（在
//! `service` 侧传给两个既有原语的 `id` / `status` 都走 bind）。口径同
//! `shared::batch::chain::resolve_chain_position`。
//!
//! ## 行结构里为什么没有 `Option`
//!
//! [`TerminalSerialRow`] 的四个字段全部按「非空」解码成 `String` / `i64`：
//! `serial_no` 由谓词 `serial_no IS NOT NULL` 保证、`status` 由谓词的
//! `IN ('COMPLETED','CANCELLED')` 保证、`drawing_no` 是 baseline 里的 `NOT NULL` 列
//! （两表皆然，无后续 migration 放开）、`id` 是 pkey。**刻意不解成 `Option`** ——
//! 谓词一旦漂移就应当**解码当场失败**，而不是静默变成 `None` 混进响应里。

use sqlx::{AssertSqlSafe, PgConnection, Row};

use crate::shared::error::AppError;

/// 「本端点要清的工单」守卫谓词（**唯一真源**，见本文件模块 doc）。
///
/// 逐字对齐两个既有释放原语内联的谓词：
/// - `AssemblyRepo::clear_serial_no_if_terminal`（`modules::assembly::repo::sql`）；
/// - `shared::batch::status::release_part_serial_no` 的「先读原值」那条
///   （`WHERE id = $1 AND deleted_at IS NULL`）+ 其后的清列谓词。
pub const TERMINAL_PREDICATE: &str = "deleted_at IS NULL AND serial_no IS NOT NULL \
                                      AND status IN ('COMPLETED', 'CANCELLED')";

/// 一行候选工单（**内部结构**，不对外序列化）。
///
/// ⚠️ `id` 在这里还是 `i64`，出参前由 `dto::ReleasedSerialItem` 转字符串 —— 雪花 id
/// 不能当 JSON 数字传给前端。
#[derive(Debug, Clone)]
pub struct TerminalSerialRow {
    pub id: i64,
    pub serial_no: String,
    pub drawing_no: String,
    pub status: String,
}

/// 候选 SELECT 的固定投影（两张表同形，逐字一致）。
const PROJECTION: &str = "id, serial_no, drawing_no, status";

/// 把一条 DB 行解成 [`TerminalSerialRow`]（两张表共用，故单点实现）。
fn map_row(row: &sqlx::postgres::PgRow) -> TerminalSerialRow {
    TerminalSerialRow {
        id: row.get("id"),
        serial_no: row.get("serial_no"),
        drawing_no: row.get("drawing_no"),
        status: row.get("status"),
    }
}

/// 终态持号工单的**计数**（两张表各一条 `COUNT(*)`）→ `(装配件数, 零件数)`。
///
/// **不带 `LIMIT`** —— 计数必须是真实总数，`released_total` / `truncated` 都由它
/// 派生。谓词与 [`TERMINAL_PREDICATE`] 同一常量。
pub async fn count_terminal_with_serial(conn: &mut PgConnection) -> Result<(i64, i64), AppError> {
    let assembly_count: i64 = sqlx::query_scalar(AssertSqlSafe(format!(
        "SELECT COUNT(*) FROM t_assembly WHERE {TERMINAL_PREDICATE}"
    )))
    .fetch_one(&mut *conn)
    .await?;
    let part_count: i64 = sqlx::query_scalar(AssertSqlSafe(format!(
        "SELECT COUNT(*) FROM t_part WHERE {TERMINAL_PREDICATE}"
    )))
    .fetch_one(&mut *conn)
    .await?;
    Ok((assembly_count, part_count))
}

/// 终态持号**装配件**候选明细 → 行集合。
///
/// ## 参数
/// - `limit`：明细条数上限。**`None` = 不加 `LIMIT`**（apply 路径用这个形态把
///   候选**全量**锁住 —— 上限只该约束「回给前端看的明细」，不该约束「真正清多少行」，
///   否则 5000 行残留只清得掉 200 行，而 `released_total` 还报 5000）。
/// - `for_update`：是否追加 `FOR UPDATE`。**为什么 apply 路径要锁**：把候选行锁在
///   事务内，使「选出候选」与「逐行清列」之间没有可插入的窗口 —— 否则并发进来的
///   工单流转（如某个 part 正在被状态机推到 COMPLETED）可能在我们选完候选之后、
///   清列之前改掉 `status`，让 apply 与计数对不上。反过来锁住则语义是「本事务开始
///   时看到的那些行」，可复现。代价是候选极多时会长时间持锁，这是维护型端点
///   （MANAGER 单角色、人工触发）可接受的取舍，登记在 `docs/api/serial.md` §8。
///
/// ## 为什么**不用**「先 `UPDATE … RETURNING` 再判」
///
/// 零件侧必须**先归档事件、再清列**（`release_part_serial_no` 内部要先读出
/// `serial_no` 才能把它写进归档 `note`）。若本域自己先跑一条 `UPDATE … RETURNING`
/// 把列清了，那个原语内部那次读就会拿到 `NULL` ⇒ 走「已清空 → 无需归档」的早退分支，
/// **归档事件永久丢失**（序列号转交送货单后要从工单上消失，直接清会丢掉「这个工单
/// 曾经用过哪个序列号」这条审计链）。所以必须先 SELECT 出候选，再交给既有原语清。
/// ## 为什么**不在这里**返回 `truncated` 标志
///
/// 「是否被截断」看起来能从 `rows.len() >= limit` 推出来，但那是**假阳性**：候选恰好
/// 等于上限行数时 `LIMIT n` 正好返回 n 行，DB 无法区分「刚好这么多」与「还有更多」。
/// 本层的调用方（`service::release_terminal`）本来就要跑 `count_terminal_with_serial`
/// 取真实计数 ⇒ 由计数与上限直接比出 `truncated`，是精确值。
pub async fn list_terminal_assemblies(
    conn: &mut PgConnection,
    limit: Option<i64>,
    for_update: bool,
) -> Result<Vec<TerminalSerialRow>, AppError> {
    let sql = format!(
        "SELECT {PROJECTION} FROM t_assembly WHERE {TERMINAL_PREDICATE} ORDER BY id{}{}",
        limit_sql(limit),
        lock_sql(for_update),
    );
    Ok(sqlx::query(AssertSqlSafe(sql))
        .fetch_all(&mut *conn)
        .await?
        .iter()
        .map(map_row)
        .collect())
}

/// 终态持号**零件 / 子件**候选明细 → 行集合。
///
/// 参数语义与锁的理由见 [`list_terminal_assemblies`]（两张表同守卫，逐字一致）。
pub async fn list_terminal_parts(
    conn: &mut PgConnection,
    limit: Option<i64>,
    for_update: bool,
) -> Result<Vec<TerminalSerialRow>, AppError> {
    let sql = format!(
        "SELECT {PROJECTION} FROM t_part WHERE {TERMINAL_PREDICATE} ORDER BY id{}{}",
        limit_sql(limit),
        lock_sql(for_update),
    );
    Ok(sqlx::query(AssertSqlSafe(sql))
        .fetch_all(&mut *conn)
        .await?
        .iter()
        .map(map_row)
        .collect())
}

/// `limit` → SQL `LIMIT` **子句**（含关键字本身）：`Some(n)` → `" LIMIT n"`，
/// `None` → 空串（= 不加 `LIMIT`）。
///
/// ⚠️ **关键字必须与值一起条件化** —— 只把数字条件化会让 `None` 形态拼出
/// `ORDER BY id LIMIT  FOR UPDATE`（悬空的 `LIMIT`）⇒ 语法错误。`FOR UPDATE`
/// 跟在 `lock_sql` 后面追加，两者的先后由 PG 语法定死（`ORDER BY … LIMIT … FOR UPDATE`）。
fn limit_sql(limit: Option<i64>) -> String {
    match limit {
        Some(n) => format!(" LIMIT {n}"),
        None => String::new(),
    }
}

/// `for_update` → SQL 尾缀（`true` 时为 ` FOR UPDATE`）。
///
/// `FOR UPDATE` 必须排在 `ORDER BY … LIMIT` **之后**（PG 语法要求），故本函数
/// 返回的串由调用方拼在 SQL 最末。
fn lock_sql(for_update: bool) -> &'static str {
    if for_update { " FOR UPDATE" } else { "" }
}

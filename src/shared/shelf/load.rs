//! 货架负载（件数）的聚合真源。
//!
//! | 项 | 说明 |
//! |---|---|
//! | 口径 | `SUM(quantity)` —— **件数**，不是批次数 |
//! | 状态集 | `IN ('PENDING','IN_PROCESS','INSPECTION','OUTSOURCE')` |
//! | 软删 | 子查询自己带 `deleted_at IS NULL`（外层带了不算数） |
//! | 存储 | **不是存储列**，每次读时聚合 |

use sqlx::{AssertSqlSafe, PgConnection, Row};

/// `t_part_batch` 按 `current_holder_id` 聚合的负载子查询（**只此一处**）。
///
/// 2026-10-10 之前本仓有两处**逐字重复**的这段聚合：`iam::shelf::repo::sql`
/// 里两个 picker 专供查询的内联子查询。两处各写一份的直接后果是它们只能靠注释
/// 互相约束（「必须逐字一致」）—— 注释不执行，改一处忘了另一处时两个 picker 会
/// 对同一个架给出不同的负载数，而没有任何测试会红。搬成本常量后约束由类型承担
/// （那两个查询本身已于 picker 下线时删除，本常量现在被
/// [`crate::shared::shelf::select`] 与本文件的 [`loads_by_shelf_ids`] 共用）。
///
/// ## 口径里的三处「刻意」
///
/// - **`SUM(quantity)` 而不是 `COUNT(*)`**：负载是**件数**。工单按数量 10 件一批
///   下发时，`COUNT(*)` 会把它算成 1，与 `capacity`（件数上限）根本不同量纲。
/// - **状态列表含 `INSPECTION` / `OUTSOURCE`**：品检架与外协在途批次同样占着物理
///   位置。把它们排除会让品检架的负载恒为 0，选架时永远排第一。
/// - **子查询自己带 `deleted_at IS NULL`**：本仓 `repo/sql.rs` 的
///   「读查询一律带 `deleted_at IS NULL`」约定对 `LEFT JOIN` 的聚合子查询同样成立
///   —— 外层 `t_shelf` 带了不算数。不过滤则软删批次的 quantity 会被**永久**计入
///   所属货架的负载。
pub const LOAD_AGGREGATE_SQL: &str = "SELECT current_holder_id AS shelf_id, \
     SUM(quantity)::bigint AS cnt \
     FROM t_part_batch \
     WHERE status IN ('PENDING', 'IN_PROCESS', 'INSPECTION', 'OUTSOURCE') \
       AND deleted_at IS NULL \
     GROUP BY current_holder_id";

/// 货架负载（件数）。`load_ratio` 在 capacity 缺失或 `<= 0` 时为 `None`（不限）。
#[derive(Debug, Clone, PartialEq)]
pub struct ShelfLoad {
    pub id: i64,
    pub code: String,
    pub name: String,
    pub zone: String,
    pub location: Option<String>,
    pub capacity: Option<i32>,
    pub current_load: i64,
    /// `None` = 不限（capacity 为 `NULL` 或 `<= 0`）
    pub load_ratio: Option<f64>,
}

/// 负载比例 `current_load / capacity`；**不做截断**（允许 `> 1.0`）。
///
/// 「超载不拒」是选架的业务口径：候选集里全部货架都已 ≥100% 时仍取比例最低的那个，
/// 而不是把货拒在门外。拒收会让一批货既不能上架也不能送检，只能靠人工在 UI 上
/// 找一个已满的架手动放行 —— 与自动选架的目标相反。
///
/// `capacity` 为 `None` 或 `<= 0` 一律返回 `None`（「不限」）：除法分母为 0 在
/// `f64` 上是 `inf` / `NaN`，而 `inf` 参与排序的行为不可预期（见
/// [`crate::shared::shelf::select`] 的排序设计：不限架恒排最后）。
#[inline]
pub fn load_ratio(current_load: i64, capacity: Option<i32>) -> Option<f64> {
    match capacity {
        Some(c) if c > 0 => Some(current_load as f64 / c as f64),
        _ => None,
    }
}

/// 批量取一组货架的 `capacity` + `current_load`（一次往返，零 N+1）。
///
/// 返回 `id → (capacity, current_load)`。**不在 map 里的 id = 该架没有任何在架
/// 批次**（负载 0），调用方按 `capacity` 缺失处理即可 —— 本函数对空入参短路返回空
/// map（`ANY('{}')` 恒假，不值得为此发一次往返）。
///
/// ## 为什么是「按 id 批量补」而不是「把负载 JOIN 进货架列表查询」
///
/// 货架列表的过滤 / 分页由 `iam::shelf::repo::sql::ShelfRepo::list_with_filters` 用
/// `QueryBuilder` 动态拼（`code_like` / `zone` / `is_active` 三态过滤），那份
/// 查询**已经是** `t_shelf` 列表端点的权威口径。把负载 JOIN 进去需要让 shared 层
/// 接管那份 `QueryBuilder`（连带 `count_with_filters` 的重复 WHERE），代价是本层
/// 反过来要理解 shelf 域的筛选语义 —— 与「零域依赖」直接冲突。
///
/// 代价是一次额外往返 + 一个 `id → (capacity, load)` 的 join，这在列表页（几十行）
/// 上可忽略，而口径单一（`LOAD_AGGREGATE_SQL` 唯一）得到的收益是永久的。
pub async fn loads_by_shelf_ids(
    conn: &mut PgConnection,
    shelf_ids: &[i64],
) -> Result<std::collections::HashMap<i64, (Option<i32>, i64)>, sqlx::Error> {
    if shelf_ids.is_empty() {
        return Ok(std::collections::HashMap::new());
    }
    let sql = format!(
        "SELECT s.id, s.capacity, COALESCE(load.cnt, 0)::bigint AS current_load \
         FROM t_shelf s \
         LEFT JOIN ({LOAD_AGGREGATE_SQL}) load ON load.shelf_id = s.id \
         WHERE s.id = ANY($1::bigint[])"
    );
    let rows = sqlx::query(AssertSqlSafe(sql))
        .bind(shelf_ids)
        .fetch_all(&mut *conn)
        .await?;
    let mut out = std::collections::HashMap::with_capacity(rows.len());
    for r in rows {
        let id: i64 = r.get("id");
        let capacity: Option<i32> = r.get("capacity");
        let current_load: i64 = r.get("current_load");
        out.insert(id, (capacity, current_load));
    }
    Ok(out)
}

/// `ShelfLoad` 的构造（`load_ratio` 按 [`load_ratio`] 算）。
///
/// 收敛成一个函数而不是让每个调用方各拼一遍字段：`load_ratio` 的口径（`capacity`
/// 为 `NULL` 或 `<= 0` 一律 `None`）一旦两处各算一次，其中一处漏掉这条就会在选架
/// 排序里变成 `inf` / `NaN`。当前唯一调用方是
/// [`crate::shared::shelf::select::pick_least_loaded`]（行 → 结构体的转换点）。
pub fn shelf_load_from_parts(
    id: i64,
    code: String,
    name: String,
    zone: String,
    location: Option<String>,
    capacity: Option<i32>,
    current_load: i64,
) -> ShelfLoad {
    ShelfLoad {
        id,
        code,
        name,
        zone,
        location,
        capacity,
        current_load,
        load_ratio: load_ratio(current_load, capacity),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 比例的三个分支：`None` capacity / `<= 0` capacity → 不限；正常 → 真比例。
    #[test]
    fn load_ratio_treats_missing_or_non_positive_capacity_as_unbounded() {
        assert_eq!(load_ratio(80, Some(100)), Some(0.8));
        assert_eq!(load_ratio(80, None), None);
        assert_eq!(load_ratio(80, Some(0)), None);
        assert_eq!(load_ratio(80, Some(-1)), None);
    }

    /// 超载不拒：比例允许 > 1.0（不被截断到 1.0，也不变 `None`）。
    #[test]
    fn load_ratio_allows_over_capacity() {
        assert_eq!(load_ratio(150, Some(100)), Some(1.5));
    }

    /// 聚合常量的三处口径锚点（件数 / 状态集 / 软删闸门）。
    ///
    /// 它们是**文本**断言而非结果断言：口径漂移一定是常量文本被改，而改文本时
    /// 这条测试立刻红。真正验证聚合数值的是 `select.rs` 的 DB 单测。
    #[test]
    fn load_aggregate_sql_pins_quantity_status_list_and_soft_delete_gate() {
        assert!(LOAD_AGGREGATE_SQL.contains("SUM(quantity)"));
        assert!(
            LOAD_AGGREGATE_SQL
                .contains("status IN ('PENDING', 'IN_PROCESS', 'INSPECTION', 'OUTSOURCE')")
        );
        assert!(LOAD_AGGREGATE_SQL.contains("deleted_at IS NULL"));
        assert!(LOAD_AGGREGATE_SQL.contains("GROUP BY current_holder_id"));
        // 件数口径 ⇒ 不能是 COUNT(*)（两者只在 quantity 全为 1 时同值）
        assert!(!LOAD_AGGREGATE_SQL.contains("COUNT("));
    }
}

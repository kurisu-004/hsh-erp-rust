//! 「已交数量 / 已交套数」的跨域批量聚合（2026-10-13 自 part 域 + dashboard 域提取）
//!
//! ## 为什么在这里
//!
//! 这两条聚合此前各存两份**逐字同源**的副本：
//! - `part::service::list_enrichment::fetch_delivered_quantities` /
//!   `::fetch_delivered_sets`（零件列表 / 联单列表的派生列），与
//! - `dashboard::repo::delivery` 内的两个私有同名方法（大屏交期面板 / 下钻抽屉的
//!   `delivered_quantity` 展示值）。
//!
//! dashboard 那份之所以必须复刻，源码注释写得很直白：本域受
//! `modules::dashboard::tests::dashboard_domain_depends_on_no_other_domain` 约束
//! （`cargo test --lib` 强制扫 `src/modules/dashboard/**/*.rs`），**禁止 import 他域的
//! 任何实现**；只读跨域聚合虽是本仓既定 pattern，SQL 却只能在本域写全，于是产出副本。
//!
//! 副本的代价是静默漂移：口径改动要改两处，漏一处的表现是「大屏显示 0 套、列表显示
//! 3 套」这种**不报错**的分叉。
//!
//! 落到 `shared/` 之后副本可以消失、且**护栏强度不降**：`assert_no_foreign_domain`
//! 只扫 `src/modules/dashboard/**/*.rs`、找的是「域根段 + 他域路径」前缀失配，
//! `shared::analytics::delivered` 不属于任何域、合法可引。
//!
//! ## 口径约定（两条 SQL 与它们的边界处理，改口径必须改这一处）
//!
//! 两条都是**已交**（`t_part_batch.status IN ('DELIVERED','COMPLETED')`）口径。
//! ⚠️ 与 `crate::modules::com::delivery_note::service` 的私有 `shippable_sets`
//! 「本单可出货套数」**刻意不同**：那份是纯内存实现、分子只计本单 `READY_TO_SHIP`
//! 批次、问的是「这一单能出几套」而非「这个装配件历史上共出几套」。三者不是同一口径的
//! 两种实现，不要合并。
//!
//! SQL 条数：各 1 条，与入参个数 N 无关（防 N+1 往返）。

use std::collections::HashMap;

use sqlx::PgConnection;

/// 一批 part 的「已送数量」：未软删批次中 `status ∈ ('DELIVERED', 'COMPLETED')`
/// 的 `quantity` 之和。
///
/// 真相源是 `t_part_batch.status`（批次级「已交」的唯一依据，**不**从派生缓存
/// `t_part.status` 反推 —— 后者在 min-progress 规则下只有全部活跃批次都 DELIVERED
/// 才等于 DELIVERED，会把「部分已交」一律压成 0）。
///
/// 空 ids → 返回空 HashMap（不发起 SQL）。caller 侧用 `.copied().unwrap_or(0)`
/// 兜底零批次行。
///
/// SQL 条数：1 条，与页大小 N 无关（防 N+1 往返）；但**扫描量是 O(本页零件的批次
/// 总数)**，走 `ix_t_part_batch_part_id` 索引探测。不改写成 CTE hash 聚合
/// —— 收益不可测、风险大于收益。
pub async fn fetch_delivered_quantities(
    conn: &mut PgConnection,
    part_ids: &[i64],
) -> Result<HashMap<i64, i32>, sqlx::Error> {
    let mut out: HashMap<i64, i32> = HashMap::new();
    if part_ids.is_empty() {
        return Ok(out);
    }
    // `::int` cast 不可省：PG 的 `SUM(int4)` 返回 int8，直接绑 i32 解码会报类型不匹配
    // （整页 500）。`COALESCE(..., 0)` 不可省：空集合 SUM 返 NULL。
    // status 字面量全大写 —— `t_part_batch.status` 是 varchar（非 DB enum）。
    let rows: Vec<(i64, i32)> = sqlx::query_as(
        "SELECT part_id, COALESCE(SUM(quantity), 0)::int \
         FROM t_part_batch \
         WHERE part_id = ANY($1) AND deleted_at IS NULL \
           AND status IN ('DELIVERED', 'COMPLETED') \
         GROUP BY part_id",
    )
    .bind(part_ids)
    .fetch_all(&mut *conn)
    .await?;
    for (part_id, qty) in rows {
        out.insert(part_id, qty);
    }
    Ok(out)
}

/// 一批装配件的「已送套数」：能凑齐几套。
///
/// 口径：每套需要的子零件数 = `子件总量 / 套数`（用户在「新建装配件」时指定装配件
/// 套数与每个子零件的总数）。故某子件能支撑的套数 =
/// `子件已送件数 × 装配件套数 / 子件总量`，父装配件的可交套数取所有子件的 **min**
/// （PG 整数除法截断，凑不满整套就按 0 记），再对装配件总套数 `a.quantity` 收口
/// （`LEAST`）—— 不可能交付超过工单总套数的套数。
///
/// 边界处理：
/// - `COALESCE(SUM(...), 0)` 不可省 —— 未交任何批次的子件若贡献 NULL 会被 `MIN` 忽略，
///   那样「子件 A 交一半、子件 B 一件没交」会误判成 A 能撑的套数；
/// - `NULLIF(子件总量, 0)` —— 总量为 0 的子件让该项为 NULL 从而被 `MIN` 忽略
///   （不参与），既不整除零出错也不拖累 min；
/// - `LEAST(COALESCE(MIN(...), 0), a.quantity)` —— 收口到工单总套数，同时兜住
///   子件超交（子件已送 > 子件总量时按比例会算出超过总套数的值，UI 会出现
///   「20 / 10 套」）。**`COALESCE` 必须在 `LEAST` 里面**：PG 的 `LEAST` 会忽略
///   NULL 实参（与 `MIN` 聚合同语义），写成 `COALESCE(LEAST(MIN(...), a.quantity), 0)`
///   会在「子件总量全为 0、`MIN` 为 NULL」时返回 `a.quantity`（整套全交），
///   与「子件全零 → 0 套」的口径正好相反；
/// - `LEAST` 顺带消除 int8→int4 收窄溢出：`子件已送 × 装配件套数` 是 int8 乘积
///   （子件总量为 1 时等于乘积本身），`1e6 × 1e6 = 1e12` 超 int4 会让整页 500；
///   收口后上界是 `a.quantity`（int4），`::int` 不再可能溢出。
///   PART 侧的 [`fetch_delivered_quantities`] 保持纯 `::int` 不加钳制：那边只是
///   `SUM(quantity)` 不放大，无真实溢出路径，加钳制反而会掩盖「已交量 > 总量」。
///
/// 前提：假设 `t_assembly.quantity` / `t_part.quantity` 恒非负。三列都是
/// `integer NOT NULL` 且**无 CHECK**（全仓唯一 quantity CHECK 在 outsource 域），
/// `AssemblyCreateRequest.quantity` 是 `Option<i32>` + `unwrap_or(1)`、create 路径无
/// `>0` 校验，但业务不会建负量工单；PG 整数除法对负数是**向零截断**（`-1 / 2 = 0`）
/// 会让套数偏大，而 `NULLIF` 只挡 0 不挡负。
///
/// 无子件的装配件不产生结果行（SQL 以子件表为驱动表），caller 侧
/// `.copied().unwrap_or(0)` 兜 0。
///
/// SQL 条数：1 条，与页大小 N 无关（防 N+1 往返）；但**扫描量是 O(本页装配件的子件
/// 总数)**，每个子件一次 `ix_t_part_batch_part_id` 索引探测。不改写成 CTE hash 聚合
/// —— 收益不可测、风险大于收益。
pub async fn fetch_delivered_sets(
    conn: &mut PgConnection,
    asm_ids: &[i64],
) -> Result<HashMap<i64, i32>, sqlx::Error> {
    let mut out: HashMap<i64, i32> = HashMap::new();
    if asm_ids.is_empty() {
        return Ok(out);
    }
    // 以 `t_part`（子件）为驱动表，走 `(assembly_id, ...)` 前缀索引，因此整段仍只 1 条
    // SQL。不写死索引名：`ix_t_part_assembly_id_status` 与 `ix_t_part_assembly_id` 同
    // 前缀，planner 可能选后者，钉死名字必过期。
    // `GROUP BY c.assembly_id, a.quantity`：套数是表达式的一部分，必须进 GROUP BY。
    // ⚠️ `c.id` / `c.quantity` **未**进 GROUP BY 却被 SELECT 表达式引用，靠的是
    // 「相关标量子查询的外层引用不受 grouping 检查」这一 PG 行为 —— 标准 SQL 应拒绝
    // （PG 的函数依赖放宽只在 GROUP BY 含表主键时生效，`c.assembly_id` 不是 `t_part`
    // 的主键）。**把相关子查询改写成 LEFT JOIN 或改用窗口函数会立刻报**
    // `column "c.id" must appear in the GROUP BY clause`，改写前务必先跑
    // `tests/com/union_list.rs` 与 `tests/dashboard_ws_api.rs` 的
    // `delivered_quantity_*` 用例。
    let rows: Vec<(i64, i32)> = sqlx::query_as(
        "SELECT c.assembly_id, \
                LEAST(COALESCE(MIN( \
                    (COALESCE((SELECT SUM(b.quantity) FROM t_part_batch b \
                               WHERE b.part_id = c.id AND b.deleted_at IS NULL \
                                 AND b.status IN ('DELIVERED', 'COMPLETED')), 0) \
                     * a.quantity) / NULLIF(c.quantity, 0) \
                ), 0), a.quantity)::int AS delivered_sets \
         FROM t_part c \
         JOIN t_assembly a ON a.id = c.assembly_id AND a.deleted_at IS NULL \
         WHERE c.assembly_id = ANY($1) AND c.deleted_at IS NULL \
         GROUP BY c.assembly_id, a.quantity",
    )
    .bind(asm_ids)
    .fetch_all(&mut *conn)
    .await?;
    for (asm_id, sets) in rows {
        out.insert(asm_id, sets);
    }
    Ok(out)
}

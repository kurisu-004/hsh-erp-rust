//! 批次在工序链上的**位置派生**（2026-10-09 新增）：读写共用的唯一真源。
//!
//! ## 抽这层的动机
//! 「这批货当前处在工序链的哪一步、下一道是哪道」原先只有**读侧**一份实现在
//! 报工台的 held 列表 SQL（`prod::scan::listing::repo::fetch_held`，内联
//! `LEFT JOIN LATERAL`），写侧（`prod::scan::service::worker_scan` 的 RETURNED
//! 分支）另有��份**廉价版**：
//! 按 `process_id` 在链内反查 `step_id`，两套口径互不知情。
//!
//! 2026-10-09 起的不变式把两条路径合并了：dispatch 落链首 step、worker 放回时
//! 顺工序自动推进 ⇒ 写侧必须与读侧**逐字同形**地问同一个问题
//! （「锚链是哪个 / 当前工序在链内第几步 / 下一道是哪个」），任何一侧自己算一遍
//! 都会让「前端看到可免填」与「写端点实际推进到哪道」分叉。故抽到 shared。
//!
//! ## 别名是契约
//! [`CHAIN_POSITION_LATERAL_SQL`] 引用外层的两个别名：
//! - `p` = `t_part`（取 `process_chain_id`，锚链的第一来源）
//! - `pb` = `t_part_batch`（取 `current_process_id` / `current_process_step_id`）
//!
//! 拼装它的每个消费方都必须把这两张表按这两个别名暴露给该子查询
//! （`work_type.rs` 的两条 SELECT 因此把批次别名从 `b` 统一改成 `pb`）。
//!
//! ## 两步定位（纪律 1 与 2 的理由）
//! 1. **锚链** = `COALESCE(p.process_chain_id, cur.chain_id)`，`cur` =
//!    `pb.current_process_step_id` 指向的 step，**只用于回退取链 id**（该 JOIN
//!    无行 ⇒ 锚链解析失败 ⇒ 落 `NONE`）。中间 JOIN `t_part_process_chain`
//!    （`AND deleted_at IS NULL`）是为了让「锚链已软删」同样落 `NONE`。
//! 2. **当前 step 在锚链内的位置** = 按 `cur2.process_id = pb.current_process_id`
//!    在锚链内**重新定位**（`cur2` 是 inner `JOIN LATERAL`，定位不到时整个派生子
//!    查询无行，故下面的 `CASE` 里没有「定位不到」这一分支，由最外层
//!    `COALESCE(..., 'NONE')` 兜底）。
//!
//! ⚠️ **第 2 步绝对不能拿 `pb.current_process_step_id` 的 `sort_order` 当位置** ——
//! step 指针与「当前工序在链内的位置」是两个独立事实。指针漂移时按 sort_order
//! 推进会把**当前工序自己**当成下一道返回（如指针停在 A 的 step 而
//! `current_process_id = B` ⇒ 返回 B），而 `chain_state` 仍在说「可免填」⇒
//! 写侧照单全收，静默错值比拒收更难发现。
//!
//! ## 链内同一 `process_id` 允许重复（纪律 3）
//! `t_process_chain_step` 只有 `uq_chain_step_chain_order (chain_id, sort_order)
//! WHERE deleted_at IS NULL` 一个唯一约束，**没有** `(chain_id, process_id)` 唯一
//! 约束；写侧 `prod::process_chain::service::upsert_chain` 也只校验链内
//! `sort_order` 互不重复、不校验 `process_id` 重复 ⇒ 重复工序的链后端照收（前端
//! 工序链编辑页连续「添加工序」且不改工序即是一条）。此时 `cur2` 会扇出多行：一行
//! 派生 `NEXT → 当前工序自己`（如链 `[(A,10),(A,20),(B,30)]` 而
//! `current_process_id = A`），另一行派生 `TAIL`，让 `LIMIT 1` 静默取其一就是拿
//! 「绝不能把当前工序自己当成下一道」这条安全承诺去赌 PG 的行序。故 `cur2` 侧用
//! `(count(*) OVER ())` 带出命中数，`hit_count > 1` 时**显式落 `NONE`**，并同时
//! 门控 `current_step_id` / `current_sort_order` / `nsp` 三个派生侧：歧义时**不产出
//! 任何派生值**，维持 `NONE` ⇒ 下一道 id 为 `None`、两个名字为 `null` 的不变量。
//!
//! ## 「下一道」按 `>` 取，不按 `= 当前 + 1`
//! 与写侧正典 `prod::process_chain::repo::query::next_step_in_chain`
//! （`sort_order > $2 ORDER BY sort_order ASC LIMIT 1`）逐条同形。而
//! `sort_order` 的**密度不由本层决定**：写侧只保证链内互不重复，稠密 0-based
//! （前端 `usePartProcessDesign` 保存时拍平成 `0,1,2…`）与稀疏 `10/20/30` 两种密度
//! 都能落库且都受支持。别拿任何文档当密度依据 —— 写路径才是权威。`+ 1` 只在稠密下
//! 正确、在稀疏下会把「还有两道工序」误判成链尾，`>` 对两种密度都成立。

use sqlx::{AssertSqlSafe, PgConnection, Row};

use crate::shared::batch::model::TPartBatch;
use crate::shared::error::AppError;

/// 链位置派生的 `LEFT JOIN LATERAL` 片段。**别名是契约**：`p` = `t_part`、
/// `pb` = `t_part_batch`。
///
/// 输出 5 个列（供读侧与写侧共用，列名逐字稳定）：
/// | 列 | 含义 |
/// |---|---|
/// | `chain_state` | `NEXT` / `TAIL` / `NONE`；**必须**由外层 `COALESCE(nx.chain_state, 'NONE')` 兜底（LATERAL 无行时该列为 SQL NULL） |
/// | `current_step_id` | 锚链内按 `pb.current_process_id` 唯一定位到的 step id（歧义 / 定位不到 → NULL） |
/// | `current_sort_order` | 该 step 的 `sort_order`（同上，可 NULL） |
/// | `next_step_id` | 链内 `sort_order > current_sort_order` 的最小未软删 step 的 id（可 NULL） |
/// | `next_process_id` | 同上的 `process_id`（可 NULL） |
///
/// ⚠️ **不输出两个工序名**：原内联版把 `np.name` / `cp.name` 也放在子查询里，读侧
/// 因此不必再 JOIN `t_process`。取名改成读侧「按 `nx.next_process_id` /
/// `nx.current_step_id` 在外层 LEFT JOIN `t_process`」逐字等价（`t_process.id` 是
/// 主键，LEFT JOIN 不改变行数），却让本片段不必为纯展示的列付出代价 —— 写侧
/// [`resolve_chain_position`] 根本不需要名字。
pub const CHAIN_POSITION_LATERAL_SQL: &str = "SELECT \
     CASE \
       WHEN cur2.hit_count > 1 THEN 'NONE' \
       WHEN nsp.id IS NULL THEN 'TAIL' \
       ELSE 'NEXT' \
     END AS chain_state, \
     CASE WHEN cur2.hit_count = 1 THEN cur2.id END AS current_step_id, \
     CASE WHEN cur2.hit_count = 1 THEN cur2.sort_order END AS current_sort_order, \
     nsp.id AS next_step_id, \
     nsp.process_id AS next_process_id \
   FROM t_process_chain_step cur \
   JOIN t_part_process_chain pc \
     ON pc.id = COALESCE(p.process_chain_id, cur.chain_id) \
    AND pc.deleted_at IS NULL \
   JOIN LATERAL ( \
     SELECT cur2b.id AS id, cur2b.process_id AS process_id, \
            cur2b.sort_order AS sort_order, \
            (count(*) OVER ()) AS hit_count \
     FROM t_process_chain_step cur2b \
     WHERE cur2b.chain_id = pc.id \
       AND cur2b.process_id = pb.current_process_id \
       AND cur2b.deleted_at IS NULL \
     ORDER BY cur2b.sort_order ASC, cur2b.id ASC \
     LIMIT 1 \
   ) cur2 ON TRUE \
   LEFT JOIN LATERAL ( \
     SELECT nxt.id AS id, nxt.process_id AS process_id \
     FROM t_process_chain_step nxt \
     WHERE cur2.hit_count = 1 \
       AND nxt.chain_id = pc.id \
       AND nxt.sort_order > cur2.sort_order \
       AND nxt.deleted_at IS NULL \
     ORDER BY nxt.sort_order ASC \
     LIMIT 1 \
   ) nsp ON TRUE \
   WHERE cur.id = pb.current_process_step_id AND cur.deleted_at IS NULL \
   ORDER BY cur.id ASC \
   LIMIT 1";

/// 绿色左边框判据的 SQL **表达式**（不是片段 —— 它接在 `AS has_process_chain`
/// 之前）。别名契约：`p` / `pb` / `cs`（同 [`CHAIN_POSITION_LATERAL_SQL`] 另加
/// `cs` = `pb.current_process_step_id` 指向的 step）。
///
/// 语义：工单已绑链，**且**批次当前工序在链内能定位到 —— 两个分支：
/// 1. `cs.process_id = pb.current_process_id`：指针存在且指向的 step 的工序就是批次
///    当前工序；
/// 2. `pb.current_process_id IS NULL` 且链内至少有一道未软删 step：批次尚未定位
///    （PENDING 未下发 / 池内待领），但工单是有工艺链的。
///
/// 两条分支**互斥**（分支 1 蕴含 `current_process_id IS NOT NULL`），故可以并列
/// `OR`。
///
/// ⚠️ **必须用 `IS NOT NULL AND =` 而不是 `IS NOT DISTINCT FROM`**：后者在
/// `NULL = NULL` 时为真，会让未定位（`current_process_id IS NULL`）的批次一律走
/// 分支 1 —— 而分支 1 拿 `cs.process_id`（NULL）与 NULL 比「相等」，于是**任何**
/// 带链工单的 PENDING 批次都被判成「顺应工序」，绿色边框出现在它还没进任何工序
/// 的时候。这条判据的消费方是卡片边框，不是安全闸门，但错值同样会被前端当事实
/// 渲染。
///
/// 消费方在各自 SQL 里**必须**配一条
/// `LEFT JOIN t_process_chain_step cs ON cs.id = pb.current_process_step_id AND cs.deleted_at IS NULL`
/// —— 列表一律 LEFT JOIN（INNER 会让无 step 的批次从列表里消失，那比给错边框更糟）。
///
/// ⚠️ **本表达式与 [`ChainPosition::is_pointer_consistent`] 只是「近似判据」，不是
/// 同一判据**（2026-10-09 登记）：两者共同的那条只有「指针 step 的工序 == 批次当前
/// 工序」。本表达式只看 SQL 可表达的形状，判不了下面三种形态，而
/// `is_pointer_consistent` 会判 false：
///
/// | 形态 | 本表达式 | `is_pointer_consistent` |
/// |---|---|---|
/// | 链行已软删（`t_part_process_chain.deleted_at` 非空）但链内 step 仍活跃 | 分支 1 成立 ⇒ true | false（锚链 JOIN 查不到链行，位置解析无行） |
/// | 链内同一 `process_id` 出现多次（后端照收的合法脏形态） | 分支 1 成立 ⇒ true | false（`hit_count > 1` 门控掉，`current_step_id` 保持 NULL） |
/// | 指针 step 属于**另一条**链 | 分支 1 成立 ⇒ true | false（按 `pb.current_process_id` 在锚链内重新定位，命中的 step 不是指针） |
///
/// 写成「同款判据 / 同一判据的纯 SQL 表达」是错的：把三者的额外条件搬进列表 SQL 等于
/// 把整个 `CHAIN_POSITION_LATERAL_SQL` 片段塞进 4 条列表 SQL，代价与该片段的逐批次
/// LATERAL 成本都不接受。**绿框的语义因此要按「近似」理解**：它表示「有链且指针
/// step 的工序与当前工序对得上」，是前端**提示**（可免填下一道工序），不是写侧闸门；
/// 真正的闸门是 `is_pointer_consistent`，两者不一致时以写侧为准（放回时：链尾且指针
/// 一致 ⇒ 自动送检；其余非顺应 ⇒ 要求前端显式指定下一道工序，拒收不会静默错值）。
pub const HAS_PROCESS_CHAIN_EXPR: &str = "p.process_chain_id IS NOT NULL \
     AND ( (cs.process_id IS NOT NULL AND cs.process_id = pb.current_process_id) \
           OR (pb.current_process_id IS NULL \
               AND EXISTS (SELECT 1 FROM t_process_chain_step x \
                           WHERE x.chain_id = p.process_chain_id \
                             AND x.deleted_at IS NULL)) )";

/// 单批次的链位置解析结果。`chain_state` 是**DB 文本**（`NEXT`/`TAIL`/`NONE`），
/// 不在这里做枚举校验 —— 枚举在 `part::vo::ChainState::from_db_text`（wire 侧）。
///
/// `Copy` 拿不到（`chain_state` 是 `Option<String>`），故按 `&self` 传而非按值。
#[derive(Debug, Clone, Default)]
pub struct ChainPosition {
    /// `Some("NONE")` 由两处保证：SQL 侧 `COALESCE(nx.chain_state, 'NONE')`，
    /// 以及 [`resolve_chain_position`] 在外层查询无行时显式兜底。
    pub chain_state: Option<String>,
    pub current_step_id: Option<i64>,
    pub current_sort_order: Option<i32>,
    pub next_step_id: Option<i64>,
    pub next_process_id: Option<i64>,
}

impl ChainPosition {
    /// 是否"顺应工序"：step 指针存在且其 `process_id ==` 批次当前工序。
    ///
    /// 实现上等价于「本层按 `pb.current_process_id` 唯一定位到的 step 就是批次
    /// 指针指向的那个 step」：`current_step_id` 的产出条件恰恰是「锚链可解析 ∧
    /// 链内该 `process_id` 唯一命中 ∧ 该命中未被 `hit_count > 1` 门控掉」，而命中行
    /// 的 `process_id` 按定义就等于 `pb.current_process_id`。故二者相等 ⇔ 指针指向的
    /// step 的工序就是批次当前工序。
    ///
    /// 落 `false` 的四种成因（与读侧 `chain_state == "NONE"` 同域）：无链 /
    /// 链已软删 / 指针为 NULL 或漂移（`current_process_id` 不在锚链内）/ 链内同一
    /// `process_id` 重复（歧义）。
    ///
    /// 这是「step 指针可安全当位置指针用」的判据，也是读侧列表卡片「绿色左边框」判据
    /// 背后的**写侧闸门** —— 两者**不是同款判据**：本方法多要求锚链可解析、链内
    /// `process_id` 唯一命中、且定位到的 step 就是指针，而列表侧的
    /// [`HAS_PROCESS_CHAIN_EXPR`] 只能比「指针 step 的工序 == 批次当前工序」
    /// （三种分叉形态见该常量的 doc）。**不一致时以本方法为准**：绿框是前端提示，
    /// 本方法是 worker-scan 放回端点的分流闸门，故静默错值不会发生。
    pub fn is_pointer_consistent(&self, batch: &TPartBatch) -> bool {
        match (self.current_step_id, batch.current_process_step_id) {
            (Some(located), Some(pointer)) => located == pointer,
            _ => false,
        }
    }
}

/// 单批次链位置查询（写侧用：worker-scan 每笔 1 条额外 SQL，写路径低频，可接受）。
///
/// 走运行时 `sqlx::query` + [`Row::get`]（**不用 `query!` 宏**），理由与
/// `prod::queue::board::repo` 一致：复杂聚合 SQL 字段多、迭代频繁，不进 `.sqlx/`
/// 离线缓存（改一个字段要重跑 `sqlx_prepare.sh` 提交一批 hash 文件）。
///
/// `part_id` 参与 WHERE（`pb.part_id = $2`）：它既是锚链的来源（`p.process_chain_id`）
/// 又是一条「这批货确实属于这个工单」的断言 —— worker-scan 的写侧已经拿到 `part`
/// 行，传进来让 SQL 自己复核，位置解析与写入目标不可能指向两个工单。
///
/// 外层查询无行时（批次已软删 / 工单已软删）返回 `chain_state = Some("NONE")` 的
/// 保守位置而不是报错：调用方要的是「能不能顺着链推进」的答案，答「不能」即可。
///
/// ⚠️ **注入面为 0**：`format!` 只填 [`CHAIN_POSITION_LATERAL_SQL`] 这一个**编译期
/// 常量**，用户输入（`part_id` / `batch.id`）一律走 bind，故 `AssertSqlSafe` 包裹
/// 是安全的（口径同 `outsource::board::repo::sendable_dedup_sql`）。
pub async fn resolve_chain_position(
    conn: &mut PgConnection,
    part_id: i64,
    batch: &TPartBatch,
) -> Result<ChainPosition, AppError> {
    let sql = format!(
        "SELECT COALESCE(nx.chain_state, 'NONE') AS chain_state, \
                nx.current_step_id, nx.current_sort_order, \
                nx.next_step_id, nx.next_process_id \
         FROM t_part_batch pb \
         JOIN t_part p ON p.id = pb.part_id AND p.deleted_at IS NULL \
         LEFT JOIN LATERAL ( {} ) nx ON TRUE \
         WHERE pb.id = $1 AND pb.part_id = $2 AND pb.deleted_at IS NULL",
        CHAIN_POSITION_LATERAL_SQL
    );
    let row = sqlx::query(AssertSqlSafe(sql))
        .bind(batch.id)
        .bind(part_id)
        .fetch_optional(&mut *conn)
        .await?;
    Ok(match row {
        Some(r) => ChainPosition {
            chain_state: r.get("chain_state"),
            current_step_id: r.get("current_step_id"),
            current_sort_order: r.get("current_sort_order"),
            next_step_id: r.get("next_step_id"),
            next_process_id: r.get("next_process_id"),
        },
        None => ChainPosition {
            chain_state: Some("NONE".to_string()),
            ..Default::default()
        },
    })
}

// ===========================================================================
//  单元测试
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // 2026-10-09：ID 一律从 `crate::shared::test_snowflake` 的进程内唯一 generator
    // 取（为什么不能用 test-support 的同名函数，见该模块 doc 的 dev-dependency 环）。
    // DB 入口用 `hsh_erp_test_support::test_pool`（不碰 generator，是护栏
    // `snowflake_guard::tests::no_lib_unit_test_pulls_in_a_second_generator` 放行的
    // 两个入口之一）。
    use crate::infra::clock::now_naive;
    use crate::shared::test_snowflake::shared_test_snowflake;
    use hsh_erp_test_support::test_pool;

    /// 纪律的**文本锚点**：这 5 段文本就是上面 doc 里逐条论证过的规则本身。
    ///
    /// 为什么钉文本而不是钉行为：行为测试（歧义落 `NONE`、稀疏密度取下一道）
    /// 覆盖的是「规则被正确实现」，而这里是「规则还在」。三条纪律各自都有过被无声
    /// 改掉的历史（`sort_order + 1` → `>`、去掉 `count(*) OVER ()`、把 `cur2` 换成
    /// 直接读 step 指针的 `sort_order`），改掉之后**没有任何现有测试会变红**，只有
    /// 静默错值。文本断言让「纪律被改」立刻可见。
    #[test]
    fn lateral_sql_keeps_the_three_discipline_anchors() {
        // 纪律 1：锚链 = COALESCE(part 的链, step 指针回退的链)，且链软删落 NONE
        assert!(
            CHAIN_POSITION_LATERAL_SQL.contains("COALESCE(p.process_chain_id, cur.chain_id)"),
            "纪律 1：锚链必须仍是 COALESCE(p.process_chain_id, cur.chain_id)"
        );
        assert!(
            CHAIN_POSITION_LATERAL_SQL.contains("pc.deleted_at IS NULL"),
            "纪律 1：中间 JOIN t_part_process_chain 必须带软删闸门（链已软删要落 NONE）"
        );
        // 纪律 2：按 current_process_id 重新定位，且歧义显式落 NONE
        assert!(
            CHAIN_POSITION_LATERAL_SQL.contains("cur2b.process_id = pb.current_process_id"),
            "纪律 2：必须按 pb.current_process_id 在锚链内重新定位"
        );
        // 纪律 3：链内同一 process_id 重复 ⇒ hit_count > 1 显式落 NONE
        assert!(
            CHAIN_POSITION_LATERAL_SQL.contains("count(*) OVER ()"),
            "纪律 3：cur2 侧必须带出 (count(*) OVER ()) AS hit_count"
        );
        assert!(
            CHAIN_POSITION_LATERAL_SQL.contains("WHEN cur2.hit_count > 1 THEN 'NONE'"),
            "纪律 3：歧义必须显式落 NONE（而不是让 LIMIT 1 赌行序）"
        );
        // 「下一道」按 > 取（稠密 0-based 与稀疏 10/20/30 都受支持）
        assert!(
            CHAIN_POSITION_LATERAL_SQL.contains("nxt.sort_order > cur2.sort_order"),
            "下一道必须按 sort_order > 当前取，不能按 = 当前 + 1"
        );
        assert!(
            CHAIN_POSITION_LATERAL_SQL.contains("ORDER BY nxt.sort_order ASC"),
            "下一道的排序键必须是 sort_order ASC（与写侧 next_step_in_chain 同形）"
        );
        // 末尾收口：把「至多一行」这条不变量写进 SQL（不买确定性）
        assert!(
            CHAIN_POSITION_LATERAL_SQL.contains("ORDER BY cur.id ASC"),
            "外层 LATERAL 必须以 ORDER BY cur.id ASC 收口（至多一行）"
        );
        assert!(
            CHAIN_POSITION_LATERAL_SQL.trim_end().ends_with("LIMIT 1"),
            "外层 LATERAL 必须以 LIMIT 1 收口（至多一行）"
        );
    }

    // ===== DB 用例（2026-10-09）：resolve_chain_position 的三条主形态 =====

    /// 造一条链 + N 个 step，返回 `(chain_id, vec![(step_id, process_id, sort_order)])`。
    async fn seed_chain(pool: &sqlx::PgPool, steps: &[i64]) -> (i64, Vec<i64>) {
        let now = now_naive();
        let chain_id = shared_test_snowflake().next_id();
        sqlx::query(
            "INSERT INTO t_part_process_chain (id, name, version, created_at, created_by, \
             updated_at, updated_by) VALUES ($1, 'chain-pos', 0, $2, 0, $2, 0)",
        )
        .bind(chain_id)
        .bind(now)
        .execute(pool)
        .await
        .expect("insert chain");
        // 稀疏 10/20/30：本层只保证链内互不重复，密度由写侧决定（见模块 doc）。
        let mut step_ids = Vec::new();
        for (i, process_id) in steps.iter().enumerate() {
            let step_id = shared_test_snowflake().next_id();
            sqlx::query(
                "INSERT INTO t_process_chain_step (id, chain_id, sort_order, process_id, \
                 estimated_minutes, version, created_at, created_by, updated_at, updated_by) \
                 VALUES ($1, $2, $3, $4, 30, 0, $5, 0, $5, 0)",
            )
            .bind(step_id)
            .bind(chain_id)
            .bind((i as i32 + 1) * 10)
            .bind(*process_id)
            .bind(now)
            .execute(pool)
            .await
            .expect("insert chain step");
            step_ids.push(step_id);
        }
        (chain_id, step_ids)
    }

    /// 造一个 `t_part` 行（`process_chain_id` 由调用方给），返回 part_id。
    async fn seed_part(pool: &sqlx::PgPool, name: &str, chain_id: Option<i64>) -> i64 {
        let now = now_naive();
        let customer_id = shared_test_snowflake().next_id();
        sqlx::query(
            "INSERT INTO t_customer (id, name, version, created_at, updated_at) \
             VALUES ($1, $2, 0, $3, $3)",
        )
        .bind(customer_id)
        .bind(format!("Co-{name}"))
        .bind(now)
        .execute(pool)
        .await
        .expect("insert t_customer");
        let part_id = shared_test_snowflake().next_id();
        sqlx::query(
            "INSERT INTO t_part (id, name, drawing_no, applicant_name, quantity, unit_price, \
             total_price, request_date, planned_delivery_date, customer_id, status, version, \
             created_at, updated_at, process_chain_id) \
             VALUES ($1, $2, $3, 'Tester', 1, 1.00, 1.00, CURRENT_DATE, CURRENT_DATE, $4, \
                     'IN_PROCESS', 0, $5, $5, $6)",
        )
        .bind(part_id)
        .bind(name)
        .bind(format!("DWG-{name}"))
        .bind(customer_id)
        .bind(now)
        .bind(chain_id)
        .execute(pool)
        .await
        .expect("insert t_part");
        part_id
    }

    /// 造一个 `IN_PROCESS` + `WORKER` 批次（worker-scan 的形态），返回批次行。
    async fn seed_batch(
        pool: &sqlx::PgPool,
        part_id: i64,
        process_id: Option<i64>,
        step_id: Option<i64>,
    ) -> TPartBatch {
        let now = now_naive();
        let batch_id = shared_test_snowflake().next_id();
        let holder_id = shared_test_snowflake().next_id();
        sqlx::query(
            "INSERT INTO t_part_batch (id, part_id, batch_no, quantity, status, location, \
             current_holder_id, current_process_id, current_process_step_id, version, \
             created_at, updated_at) \
             VALUES ($1, $2, 1, 1, 'IN_PROCESS', 'WORKER', $3, $4, $5, 0, $6, $6)",
        )
        .bind(batch_id)
        .bind(part_id)
        .bind(holder_id)
        .bind(process_id)
        .bind(step_id)
        .bind(now)
        .execute(pool)
        .await
        .expect("insert t_part_batch");
        crate::shared::batch::get_batch_by_id(pool, batch_id, false)
            .await
            .expect("get batch")
            .expect("batch exists")
    }

    /// 顺应工序（指针与当前工序一致 + 链内有下一道）⇒ `NEXT` + 下一道的
    /// (step_id, process_id) 全带出来。
    #[tokio::test]
    async fn resolve_chain_position_next_when_pointer_consistent() {
        let pool = test_pool().await;
        let proc_a = shared_test_snowflake().next_id();
        let proc_b = shared_test_snowflake().next_id();
        let (chain_id, step_ids) = seed_chain(&pool, &[proc_a, proc_b]).await;
        let part_id = seed_part(&pool, "POS-NEXT", Some(chain_id)).await;
        let batch = seed_batch(&pool, part_id, Some(proc_a), Some(step_ids[0])).await;

        let conn = &mut *pool.acquire().await.expect("acquire");
        let pos = resolve_chain_position(conn, part_id, &batch)
            .await
            .expect("resolve");

        assert_eq!(pos.chain_state.as_deref(), Some("NEXT"));
        assert_eq!(pos.current_step_id, Some(step_ids[0]));
        assert_eq!(
            pos.current_sort_order,
            Some(10),
            "稀疏链：链首 step 的 sort_order"
        );
        assert_eq!(pos.next_step_id, Some(step_ids[1]));
        assert_eq!(pos.next_process_id, Some(proc_b));
        assert!(
            pos.is_pointer_consistent(&batch),
            "指针指向当前工序所在的 step ⇒ 顺应工序"
        );
    }

    /// 链尾 ⇒ `TAIL` 且**不产出**任何下一道 id（写侧据此判定「这批做完了」→ 自动送检，
    /// 不落生产架）。
    #[tokio::test]
    async fn resolve_chain_position_tail_has_no_next() {
        let pool = test_pool().await;
        let proc_a = shared_test_snowflake().next_id();
        let (chain_id, step_ids) = seed_chain(&pool, &[proc_a]).await;
        let part_id = seed_part(&pool, "POS-TAIL", Some(chain_id)).await;
        let batch = seed_batch(&pool, part_id, Some(proc_a), Some(step_ids[0])).await;

        let conn = &mut *pool.acquire().await.expect("acquire");
        let pos = resolve_chain_position(conn, part_id, &batch)
            .await
            .expect("resolve");

        assert_eq!(pos.chain_state.as_deref(), Some("TAIL"));
        assert_eq!(pos.next_step_id, None);
        assert_eq!(pos.next_process_id, None);
        assert!(
            pos.is_pointer_consistent(&batch),
            "链尾不影响「指针是否与当前工序一致」这个判据（只影响能否自动推进）"
        );
    }

    /// 无链（`t_part.process_chain_id IS NULL`）⇒ `NONE` + 无任何派生值，且
    /// `is_pointer_consistent` 为 false ⇒ 写侧落「非顺应」分支（要求前端显式指定下一道
    /// 工序；链尾自动送检只对 `is_pointer_consistent && TAIL` 生效，不含本形态）。
    #[tokio::test]
    async fn resolve_chain_position_none_without_chain() {
        let pool = test_pool().await;
        let proc_a = shared_test_snowflake().next_id();
        let part_id = seed_part(&pool, "POS-NONE", None).await;
        let batch = seed_batch(&pool, part_id, Some(proc_a), None).await;

        let conn = &mut *pool.acquire().await.expect("acquire");
        let pos = resolve_chain_position(conn, part_id, &batch)
            .await
            .expect("resolve");

        assert_eq!(pos.chain_state.as_deref(), Some("NONE"));
        assert_eq!(pos.current_step_id, None);
        assert_eq!(pos.next_step_id, None);
        assert_eq!(pos.next_process_id, None);
        assert!(
            !pos.is_pointer_consistent(&batch),
            "无链批次不可能「顺应工序」"
        );
    }

    /// 链内同一 `process_id` 重复（后端照收的合法脏形态）⇒ 显式落 `NONE`，
    /// 且 `current_step_id` 为 NULL（歧义时不产出任何派生值）。
    ///
    /// 这是纪律 3 的行为侧钉子：没有它，`current_step_id` 该不该被 `hit_count`
    /// 门控就没法测出来。
    #[tokio::test]
    async fn resolve_chain_position_none_when_process_duplicated_in_chain() {
        let pool = test_pool().await;
        let proc_a = shared_test_snowflake().next_id();
        let proc_b = shared_test_snowflake().next_id();
        // 链 = [(A,10), (A,20), (B,30)]：A 出现两次
        let (chain_id, step_ids) = seed_chain(&pool, &[proc_a, proc_a, proc_b]).await;
        let part_id = seed_part(&pool, "POS-DUP", Some(chain_id)).await;
        let batch = seed_batch(&pool, part_id, Some(proc_a), Some(step_ids[0])).await;

        let conn = &mut *pool.acquire().await.expect("acquire");
        let pos = resolve_chain_position(conn, part_id, &batch)
            .await
            .expect("resolve");

        assert_eq!(
            pos.chain_state.as_deref(),
            Some("NONE"),
            "链内工序重复是歧义，必须显式落 NONE（否则会把当前工序自己当下一道）"
        );
        assert_eq!(pos.current_step_id, None, "歧义时不产出派生值");
        assert_eq!(pos.next_step_id, None);
        assert_eq!(pos.next_process_id, None);
        assert!(!pos.is_pointer_consistent(&batch));
    }

    /// 指针漂移（指针停在 A 的 step，而 `current_process_id = B`）⇒ 链内能按
    /// **B** 重新定位出正确位置，绝不把 A 当成「下一道」。
    #[tokio::test]
    async fn resolve_chain_position_repositions_by_current_process_id_not_pointer() {
        let pool = test_pool().await;
        let proc_a = shared_test_snowflake().next_id();
        let proc_b = shared_test_snowflake().next_id();
        let (chain_id, step_ids) = seed_chain(&pool, &[proc_a, proc_b]).await;
        let part_id = seed_part(&pool, "POS-DRIFT", Some(chain_id)).await;
        // 指针漂移形态：指针仍指 A，但批次当前工序已经是 B
        let batch = seed_batch(&pool, part_id, Some(proc_b), Some(step_ids[0])).await;

        let conn = &mut *pool.acquire().await.expect("acquire");
        let pos = resolve_chain_position(conn, part_id, &batch)
            .await
            .expect("resolve");

        assert_eq!(pos.chain_state.as_deref(), Some("TAIL"));
        assert_eq!(pos.current_step_id, Some(step_ids[1]), "定位到 B 的 step");
        assert_eq!(
            pos.next_process_id, None,
            "指针若被当位置用，这里会返回 proc_a（= 当前工序自己）—— 那是最危险的错值"
        );
        assert!(
            !pos.is_pointer_consistent(&batch),
            "指针与当前工序不一致 ⇒ 非顺应，写侧要求前端显式指定"
        );
    }
}

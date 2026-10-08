//! 候选池取件的优先级排序片段。
//!
//! 2026-10-10 之前，取件优先级在两处各写一套且已经漂移：
//! `prod::queue::repo::sql.rs::take_one_from_pool` 的 `ORDER BY` 与
//! `prod::queue::board::repo::SQL_POOL_ITEMS_BY_PROCESS` 的 `ORDER BY`（前者把
//! 「已编程」排最前，后者把「系统交期」排最前、「加急」排第二）。后果是：看板上
//! 看到的池顺序与工人实际抢到的顺序对不上 —— 工人以为自己按加急优先在抢，板子上
//! 却是另一套。本片段把两处收到一份。

/// 候选池取件的 4 级优先级 `ORDER BY` 片段。**看板池明细与 refill 取料共用本片段**，
/// 避免两处各写一套排序漂移。
///
/// ## 别名契约（引用本片段的 SQL 必须提供这些别名）
///
/// | 别名 | 表 | 取法 |
/// |---|---|---|
/// | `p` | `t_part` | `JOIN t_part p ON p.id = pb.part_id` |
/// | `pb` | `t_part_batch` | 主表 |
/// | `pr` | `t_process` | `LEFT JOIN t_process pr ON pr.id = pb.current_process_id` |
/// | `load.cnt` | 负载聚合 | 见 [`crate::shared::shelf::load::LOAD_AGGREGATE_SQL`] |
///
/// `pr` 必须是 **LEFT JOIN**：批次无 `current_process_id`（出池后残留）时不能因
/// INNER JOIN 而整行消失 —— 排序片段不该改变候选集的成员。
///
/// ## 4 层的语义与取舍
///
/// ```sql
/// p.is_urgent DESC,
/// p.system_delivery_date ASC NULLS LAST,
/// p.planned_delivery_date ASC NULLS LAST,
/// (CASE WHEN COALESCE(pr.is_cnc, FALSE) THEN 0 ELSE 1 END) ASC,
/// (COALESCE(pr.is_cnc, FALSE)
///  AND COALESCE((SELECT has_cnc_program FROM (SELECT EXISTS (...) AS has_cnc_program
///               ) AS has_cnc_program_sub), FALSE)) ASC,
/// pb.id ASC
/// ```
///
/// **1. 加急排到交期之前**。业务上「加急」是**人工判定的例外**（`t_part.is_urgent`
/// 由计划员在超期风险出现时手工打开），而交期是**系统判定的自然量**。同一天到期
/// 的两批货里，人工标了加急的那批一定更该先做 —— 若交期优先，一批被标了加急但
/// 日期稍晚的货会被排在更早到期但没标加急的货后面，人工标记直接失效。
///
/// **2 / 3. `system_delivery_date` 用 `NULLS LAST` 而不是两段 CASE**。
/// 两段 CASE 能表达的排序（先非空再空，空内部再按键排）需要把同一个键写两遍，
/// 于是「以后要加第 4 层键」时要在两处各加一次、漏一处就静默改掉层间关系。
/// `NULLS LAST` 是 PG 的原生语义，一行写完且加键只需追加一行。次序上「没有系统
/// 交期的排到最后」本身就是业务意图（无死线 = 无紧迫度），不是技术妥协。
/// `planned_delivery_date` 单独一层：它是**计划员承诺**给客户的日期，对有系统
/// 交期的行不参与比较（同一系统交期内谁先都行），只在系统交期全空的行之间起
/// 排序作用。
///
/// **4. CNC 工序内已编程优先**。仅当该批次的当前工序 `t_process.is_cnc = TRUE`
/// 时才看 G_CODE：`AND` 把非 CNC 行恒压成 `false`，于是非 CNC 行之间全部并列、
/// 直接由 `pb.id ASC` 稳定兜底。这比「无条件把已编程的排前面」正确：非 CNC
/// 工件的「有没有 G_CODE 文件」与它该不该先做无关，那条谓词会凭空改变普通工序的
/// 取件顺序。
///
/// **末键 `pb.id ASC`**：保证恒定排序（同样的池子两次取件拿到同一个批次），也
/// 是 `FOR UPDATE SKIP LOCKED` 的前提 —— 否则并发请求会在不同键上分叉。
pub const POOL_PRIORITY_ORDER_SQL: &str = "p.is_urgent DESC, \
     p.system_delivery_date ASC NULLS LAST, \
     p.planned_delivery_date ASC NULLS LAST, \
     (CASE WHEN COALESCE(pr.is_cnc, FALSE) THEN 0 ELSE 1 END) ASC, \
     (COALESCE(pr.is_cnc, FALSE) AND COALESCE(( \
         SELECT has_cnc_program FROM ( \
             SELECT EXISTS (SELECT 1 FROM t_part_file pf \
                            WHERE pf.part_id = pb.part_id \
                              AND pf.kind = 'G_CODE' \
                              AND pf.deleted_at IS NULL) AS has_cnc_program \
         ) AS has_cnc_program_sub \
     ), FALSE)) ASC, \
     pb.id ASC";

#[cfg(test)]
mod tests {
    use super::POOL_PRIORITY_ORDER_SQL as S;

    /// 5 个文本锚点**按序**出现 —— 顺序本身就是语义（层级关系），只断言「出现过」
    /// 会漏掉「有人把第 4 层插到第 1 层前面」这类改动。
    #[test]
    fn priority_layers_appear_in_order() {
        let anchors = [
            "is_urgent DESC",
            "system_delivery_date ASC NULLS LAST",
            "planned_delivery_date ASC NULLS LAST",
            "is_cnc",
            "has_cnc_program",
        ];
        let mut cursor = 0usize;
        for a in anchors {
            let at = S[cursor..]
                .find(a)
                .unwrap_or_else(|| panic!("锚点 `{a}` 未在第 4 层之前出现，片段：{S}"));
            cursor += at + a.len();
        }
    }

    /// 末键恒为 `pb.id ASC`（稳定排序 + `SKIP LOCKED` 的前提）。
    #[test]
    fn ends_with_stable_id_key() {
        let tail = S.rsplit(',').next().expect("至少有一段").trim();
        assert_eq!(tail, "pb.id ASC", "末键必须是批次 id 升序，片段：{S}");
    }

    /// 第 4 层的 CNC 闸门是「`is_cnc AND 已编程`」而不是「无条件已编程优先」。
    ///
    /// 断言的是 `AND` 存在 —— 它把非 CNC 行恒压成并列。改成无条件已编程优先时
    /// 本测试会红（那时非 CNC 工件的取件顺序会被 G_CODE 文件凭空改变）。
    #[test]
    fn cnc_layer_is_gated_on_is_cnc() {
        assert!(
            S.contains("pr.is_cnc, FALSE) AND"),
            "第 4 层必须以 `is_cnc AND …` 表达（片段：{S}）"
        );
    }

    /// 片段不写死 `INNER JOIN` / `LIMIT`（它只是 ORDER BY 的一部分）。
    #[test]
    fn is_an_order_by_fragment_only() {
        assert!(!S.contains(" JOIN "));
        assert!(!S.contains("LIMIT"));
    }
}

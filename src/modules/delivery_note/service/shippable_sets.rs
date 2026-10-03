//! 2026-10-04 新增：本单口径的装配件「可出货套数」（纯内存计算，无 SQL）
//!
//! 口径与 `part::service::list_enrichment::fetch_delivered_sets`（**全局已送套数**）
//! 同源，唯一差别是「分子」从「全局已送批次量」换成「**本单**批次量」——
//! 打印要回答的是「这一单能出几套」，不是「这个装配件历史上共出几套」。
//!
//! 三个消费方共用本函数，避免同一公式在 handler / service 各写一遍：
//! - `handler/print.rs` —— 注入转发 body 的 `merge_quantities`（打印套数）
//! - `service/inner.rs::get_with_parts` —— 详情 `line_items[].shippable_sets`
//! - `service/crud.rs::get_many_with_parts` —— 批量详情同上
//!
//! ## 公式
//!
//! ```text
//! child_note_qty(c) = Σ 本单上子件 c 的 b.quantity          (i64，本单无批次 = 0)
//! per_set(c)        = child_note_qty(c) * asm.quantity / c.quantity
//! sets(asm)         = LEAST(COALESCE(MIN(per_set(c) for c ∈ asm 的**全部**子件), 0),
//!                            asm.quantity)                → i32
//! ```
//!
//! ## `min` 的定义域是「该装配件的全部子件」，不是「本单出现过的子件」
//!
//! 2026-10-04 review 第 1 轮修正：原实现只由本单批次行（`rows`）构建 `min` 的
//! 定义域，**本单完全没有批次的子件不参与** —— 「A 交一半、C 一件没交」会算成
//! A 能撑的套数。业务上这是错标签：剩余部分不能单独发货，必须等所有子件收齐，
//! 凑不齐整套就是 **0 套**。三条独立依据：
//! 1. 业务规则原文「剩余的部分不能单独发货，需要等待其他子零件收集齐组装为装配件出货」；
//! 2. 本仓同公式 SQL 版 `fetch_delivered_sets` 以 `t_part`（子件）为驱动表，本单
//!    无批次的子件以 `COALESCE(SUM,0)=0` 参与 `min`（其注释原话：「未交任何批次的
//!    子件若贡献 NULL 会被 MIN 忽略，那样『子件 A 交一半、子件 B 一件没交』会误判
//!    成 A 能撑的套数」）；
//! 3. `docs/api/delivery-notes/index.md` / `print.md` 的公式段写的就是「各子件」。
//!
//! 故入参必须有「该装配件的全部子件」`children_by_asm`，它由调用方从
//! `PartRepo::list_children`（单单，N 次小查询）/ `PartRepo::list_children_by_assemblies`
//! （批量，1 条 SQL）取，`include_deleted=false` 两条路径同口径。
//!
//! ## 边界（逐条对齐 `fetch_delivered_sets` 的注释，不许改口径）
//!
//! - **`part.quantity == 0` 的子件跳过**（PG 侧 `NULLIF(part.quantity, 0)` 让该项
//!   为 NULL 从而被 `MIN` 忽略）：既不整除零出错，也不拖累 min。
//! - **无子件参与 → 0 套**（PG 侧 `COALESCE(MIN(...), 0)`）。⚠️ 语义等价于
//!   「`COALESCE` 必须在 `LEAST` 里面」：PG 的 `LEAST` 会忽略 NULL 实参，写反成
//!   `COALESCE(LEAST(MIN(...), a.quantity), 0)` 会在「子件总量全为 0」时返回
//!   `a.quantity`（整套全交），与「子件全零 → 0 套」正好相反。故本实现先
//!   `unwrap_or(0)` 兜底再 `min(cap)`，顺序不可调换。
//! - **本单无批次的子件按 0 参与**（`note_qty_by_part.get(..).unwrap_or(0)`），
//!   对应 PG 的 `COALESCE(SUM(...), 0)`；见上节。
//! - **`LEAST(..., asm.quantity)` 顺带收口子件超交**（子件超交时按比例会算出超过
//!   工单总套数的值），并消除 int8→int4 收窄溢出：中间量用 i64，收口后上界是
//!   `asm.quantity`（i32）。
//! - **PG 整数除法向零截断，Rust `i64 / i64` 同语义**。
//! - 前提：`t_assembly.quantity` / `t_part.quantity` / `t_part_batch.quantity` 恒非负
//!   （三列都是 `integer NOT NULL` 且无 CHECK，业务不会建负量工单）。PG 侧对负数
//!   向零截断会让套数偏大而 `NULLIF` 只挡 0 不挡负；本实现在 i32 收窄处把越界值
//!   兜成 0（`try_from` 失败即 0），不把 wrap 后的垃圾值传下去。

use std::collections::HashMap;

use crate::modules::part::model::TPart;
use crate::modules::prod::batch::model::TPartBatch;

/// 算每个装配件的「本单可出货套数」，key = 装配件 id。
///
/// 入参：
/// - `rows` —— `PartBatchRepo::list_with_part_by_delivery_note` 的结果（本单
///   全部未删批次 × 对应工单，`ORDER BY pb.id ASC`）；
/// - `asm_quantity` —— `装配件 id → t_assembly.quantity`（工单总套数，既是
///   `per_set` 的比例因子、也是 `LEAST` 收口上界）。**只收 quantity 而不是整个
///   `TAssembly`**：本函数不需要装配件的其它字段，调用方就不必为了算套数而
///   clone 整行（`handler/print.rs` 原先的 `asms.iter().map(|(a.id, a.clone()))`）；
/// - `children_by_asm` —— `装配件 id → 全部未软删子件`（`min` 的定义域，见模块
///   文档「`min` 的定义域」一节）。缺键 = 该装配件无子件 ⇒ 0 套。
///
/// **返回集合 = `asm_quantity` 的 key**（不是「本单引用过」的装配件全集）：软删 /
/// 不存在的装配件不在 map 里，也就不出现在结果里 —— 打印侧据此不把它的 id 写进
/// `assembly_ids`，python 端 `assembly_map` 缺它，其子件按散件行打印。
pub(crate) fn note_shippable_sets(
    rows: &[(TPartBatch, TPart)],
    asm_quantity: &HashMap<i64, i32>,
    children_by_asm: &HashMap<i64, Vec<TPart>>,
) -> HashMap<i64, i32> {
    // 本单上每个子件 part 的批次量合计（同 part 多批次折叠，与 python 端
    // `_build_print_rows` 的 `qty_by_part` 求和同口径）。本单没批次的子件
    // 不进这张表，下面查不到时按 0 参与 min。
    let mut note_qty_by_part: HashMap<i64, i64> = HashMap::new();
    for (b, p) in rows {
        *note_qty_by_part.entry(p.id).or_insert(0) += i64::from(b.quantity);
    }

    // 每个装配件取「全部子件」的 per_set 最小值；`part.quantity == 0` 的子件不参与。
    let mut out: HashMap<i64, i32> = HashMap::with_capacity(asm_quantity.len());
    for (asm_id, cap) in asm_quantity {
        let cap = i64::from(*cap);
        let mut min_per_set: Option<i64> = None;
        let empty: Vec<TPart> = Vec::new();
        for child in children_by_asm.get(asm_id).unwrap_or(&empty) {
            if child.quantity == 0 {
                continue;
            }
            let note_qty = note_qty_by_part.get(&child.id).copied().unwrap_or(0);
            let per_set = note_qty * cap / i64::from(child.quantity);
            min_per_set = Some(match min_per_set {
                Some(cur) => cur.min(per_set),
                None => per_set,
            });
        }
        // 兜 0 → LEAST(..., cap) 收口 → 钳到 i32。
        let sets = min_per_set.unwrap_or(0).min(cap);
        out.insert(*asm_id, i32::try_from(sets).unwrap_or(0));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modules::assembly::model::TAssembly;
    use crate::modules::part::model::TPart;
    use crate::modules::prod::batch::model::TPartBatch;
    use chrono::NaiveDate;
    use rust_decimal::Decimal;

    fn asm(id: i64, quantity: i32) -> TAssembly {
        let d = NaiveDate::from_ymd_opt(2026, 10, 4).unwrap();
        TAssembly {
            id,
            drawing_no: "A-1".to_string(),
            name: "装配体".to_string(),
            applicant_name: None,
            customer_id: 1,
            request_date: d,
            planned_delivery_date: d,
            is_urgent: false,
            status: "ACTIVE".to_string(),
            version: 0,
            created_at: now(),
            created_by: None,
            updated_at: now(),
            updated_by: None,
            deleted_at: None,
            serial_no: None,
            quantity,
            unit_price: Some(Decimal::ZERO),
            total_price: Some(Decimal::ZERO),
            order_no: None,
            system_delivery_date: None,
            note: None,
        }
    }

    fn now() -> chrono::NaiveDateTime {
        crate::infra::clock::now_naive()
    }

    /// 造一个装配件子件 part：整单数量 `part_quantity`、所属装配件 `assembly_id`。
    fn child_part(id: i64, assembly_id: Option<i64>, part_quantity: i32) -> TPart {
        let d = NaiveDate::from_ymd_opt(2026, 10, 4).unwrap();
        TPart {
            id,
            serial_no: None,
            name: format!("子件{id}"),
            drawing_no: "D-1".to_string(),
            applicant_name: "张三".to_string(),
            quantity: part_quantity,
            request_date: d,
            planned_delivery_date: d,
            customer_id: 1,
            assembly_id,
            status: "READY_TO_SHIP".to_string(),
            is_urgent: false,
            next_process_id: None,
            order_no: None,
            system_delivery_date: None,
            note: None,
            unit_price: Decimal::ZERO,
            total_price: Decimal::ZERO,
            version: 0,
            created_at: now(),
            created_by: None,
            updated_at: now(),
            updated_by: None,
            deleted_at: None,
            process_chain_id: None,
        }
    }

    fn batch(id: i64, part_id: i64, quantity: i32) -> TPartBatch {
        TPartBatch {
            id,
            part_id,
            batch_no: 1,
            quantity,
            status: "READY_TO_SHIP".to_string(),
            location: None,
            current_holder_id: None,
            current_process_id: None,
            current_process_step_id: None,
            delivery_note_id: Some(1),
            parent_batch_id: None,
            is_repairing: false,
            version: 0,
            created_at: now(),
            created_by: None,
            updated_at: now(),
            updated_by: None,
            deleted_at: None,
        }
    }

    /// 「1 个装配件 + 1 个子件，子件本单出货 `note_qty`」的最小场景。
    ///
    /// 返回 `(本单批次行, 装配件套数 map, 装配件 → 全部子件 map)`。
    fn one_child(asm_id: i64, asm_qty: i32, part_qty: i32, note_qty: i32) -> OneChildScenario {
        let rows = vec![(
            batch(1, 11, note_qty),
            child_part(11, Some(asm_id), part_qty),
        )];
        let asms = HashMap::from([(asm_id, asm_qty)]);
        let children = HashMap::from([(asm_id, vec![child_part(11, Some(asm_id), part_qty)])]);
        (rows, asms, children)
    }

    /// [`one_child`] 的返回形态：`(本单行, 装配件总套数, 装配件 → 全部子件)`。
    type OneChildScenario = (
        Vec<(TPartBatch, TPart)>,
        HashMap<i64, i32>,
        HashMap<i64, Vec<TPart>>,
    );

    #[test]
    fn sets_are_min_over_child_parts() {
        // 装配件 10 套，子件 A 整单 10 件 / 本单送 10 件 → 10 套；
        // 子件 B 整单 10 件 / 本单送 5 件 → 5 套 ⇒ min = 5
        let rows = vec![
            (batch(1, 11, 10), child_part(11, Some(10), 10)),
            (batch(2, 12, 5), child_part(12, Some(10), 10)),
        ];
        let asms = HashMap::from([(10, 10)]);
        let children = HashMap::from([(
            10,
            vec![child_part(11, Some(10), 10), child_part(12, Some(10), 10)],
        )]);
        assert_eq!(
            note_shippable_sets(&rows, &asms, &children).get(&10),
            Some(&5)
        );
    }

    #[test]
    fn same_part_multiple_batches_are_summed() {
        let rows = vec![
            (batch(1, 11, 6), child_part(11, Some(10), 10)),
            (batch(2, 11, 4), child_part(11, Some(10), 10)),
        ];
        let asms = HashMap::from([(10, 10)]);
        let children = HashMap::from([(10, vec![child_part(11, Some(10), 10)])]);
        assert_eq!(
            note_shippable_sets(&rows, &asms, &children).get(&10),
            Some(&10)
        );
    }

    #[test]
    fn insufficient_child_yields_zero_sets() {
        // 装配件 10 套；子件整单 20 件（每套 2 件），本单只出 1 件 ⇒ 1*10/20 = 0 套
        let (rows, asms, children) = one_child(10, 10, 20, 1);
        assert_eq!(
            note_shippable_sets(&rows, &asms, &children).get(&10),
            Some(&0)
        );
    }

    #[test]
    fn over_delivery_is_capped_by_assembly_quantity() {
        // 子件整单 10 件，本单超交 100 件 ⇒ 100 套，LEAST 收口到 10
        let (rows, asms, children) = one_child(10, 10, 10, 100);
        assert_eq!(
            note_shippable_sets(&rows, &asms, &children).get(&10),
            Some(&10)
        );
    }

    #[test]
    fn zero_quantity_child_is_skipped_not_dragging_min() {
        // 子件 B 整单 0 件（本单送 100 件）：跳过，不参与 min ⇒ 仍取 A 的 8 套
        let rows = vec![
            (batch(1, 11, 8), child_part(11, Some(10), 10)),
            (batch(2, 12, 100), child_part(12, Some(10), 0)),
        ];
        let asms = HashMap::from([(10, 10)]);
        let children = HashMap::from([(
            10,
            vec![child_part(11, Some(10), 10), child_part(12, Some(10), 0)],
        )]);
        assert_eq!(
            note_shippable_sets(&rows, &asms, &children).get(&10),
            Some(&8)
        );
    }

    #[test]
    fn assembly_with_no_participating_child_yields_zero() {
        // 全部子件 quantity = 0 ⇒ 无参与项 ⇒ 兜 0（而不是 asm.quantity）
        let (rows, asms, children) = one_child(10, 10, 0, 100);
        assert_eq!(
            note_shippable_sets(&rows, &asms, &children).get(&10),
            Some(&0)
        );
    }

    #[test]
    fn loose_part_and_unresolved_assembly_are_absent_from_result() {
        // 散件（assembly_id = None）+ 装配件软删（不在 asm_quantity 里）⇒ 空结果
        let rows = vec![(batch(1, 11, 5), child_part(11, None, 10))];
        assert!(
            note_shippable_sets(&rows, &HashMap::new(), &HashMap::new()).is_empty(),
            "返回集合必须等于 asm_quantity 的 key（软删装配件不参与）"
        );
    }

    /// ★ 2026-10-04 review 第 1 轮：`min` 的定义域是「全部子件」，本单没批次的
    /// 子件按 0 参与 ⇒ 0 套。原实现只扫本单批次行，本例会误判成 8 套。
    #[test]
    fn child_absent_from_note_participates_with_zero() {
        // 装配件 10 套；子件 A 整单 10 件 / 本单送 8 件（8 套）；子件 C 整单 10 件、
        // **本单一件没送**（0 套）⇒ min = 0（凑不齐整套不能发）
        let rows = vec![(batch(1, 11, 8), child_part(11, Some(10), 10))];
        let asms = HashMap::from([(10, 10)]);
        let children = HashMap::from([(
            10,
            vec![child_part(11, Some(10), 10), child_part(13, Some(10), 10)],
        )]);
        assert_eq!(
            note_shippable_sets(&rows, &asms, &children).get(&10),
            Some(&0),
            "本单完全没交批次的子件必须参与 min（否则会印出物理上不存在的整套）"
        );
    }

    /// 装配件一个子件都没有（`children_by_asm` 缺键）⇒ 0 套。
    #[test]
    fn assembly_without_children_yields_zero() {
        let rows = vec![(batch(1, 11, 8), child_part(11, Some(10), 10))];
        let asms = HashMap::from([(10, 10)]);
        assert_eq!(
            note_shippable_sets(&rows, &asms, &HashMap::new()).get(&10),
            Some(&0)
        );
    }

    /// 单据行上的 part 若不在「全部子件」里（理论上不可能：装了同一 asm_id 的件
    /// 必然是它的子件），套数只看子件表，不看本单批次行 —— 钉死「驱动表是子件」。
    #[test]
    fn sets_ignore_note_rows_whose_part_is_not_a_child() {
        let rows = vec![(batch(1, 99, 100), child_part(99, Some(10), 1))];
        let asms = HashMap::from([(10, 10)]);
        let children = HashMap::from([(10, vec![child_part(11, Some(10), 10)])]);
        assert_eq!(
            note_shippable_sets(&rows, &asms, &children).get(&10),
            Some(&0),
            "子件 11 本单无批次 ⇒ 0 套；行里的 part 99 不该被当成子件 11 的量"
        );
    }

    /// 保留 `asm()` 构造器：装配件行的其它字段与套数无关，但仍断言函数签名
    /// 已从 `HashMap<i64, TAssembly>` 收窄为 `HashMap<i64, i32>`（只取 quantity）。
    #[test]
    fn assembly_quantity_is_the_only_needed_field() {
        let full = asm(10, 7);
        let asms = HashMap::from([(full.id, full.quantity)]);
        let (rows, _asms, children) = one_child(10, 7, 10, 3);
        // 子件整单 10 件 / 本单 3 件 ⇒ 3*7/10 = 2 套
        assert_eq!(
            note_shippable_sets(&rows, &asms, &children).get(&10),
            Some(&2)
        );
    }
}

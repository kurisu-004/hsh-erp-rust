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
//! child_note_qty = Σ 本单上该子件 part 的 b.quantity          (i64)
//! per_set        = child_note_qty * asm.quantity / part.quantity
//! sets           = LEAST(COALESCE(MIN(per_set 参与子件), 0), asm.quantity) → i32
//! ```
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
//! - **`LEAST(..., asm.quantity)` 顺带收口子件超交**（子件超交时按比例会算出超过
//!   工单总套数的值），并消除 int8→int4 收窄溢出：中间量用 i64，收口后上界是
//!   `asm.quantity`（i32）。
//! - **PG 整数除法向零截断，Rust `i64 / i64` 同语义**。
//! - 前提：`t_assembly.quantity` / `t_part.quantity` / `t_part_batch.quantity` 恒非负
//!   （三列都是 `integer NOT NULL` 且无 CHECK，业务不会建负量工单）。PG 侧对负数
//!   向零截断会让套数偏大而 `NULLIF` 只挡 0 不挡负；本实现在 i32 收窄处把越界值
//!   兜成 0（`try_from` 失败即 0），不把 wrap 后的垃圾值传下去。

use std::collections::HashMap;

use crate::modules::assembly::model::TAssembly;
use crate::modules::part::model::TPart;
use crate::modules::prod::batch::model::TPartBatch;

/// 本单上每个子件 part 的聚合口径：`p.assembly_id` / `p.quantity` / 本单批次量合计。
type ChildAgg = (Option<i64>, i32, i64);

/// 算每个装配件的「本单可出货套数」，key = 装配件 id。
///
/// 入参 `rows` 是 `PartBatchRepo::list_with_part_by_delivery_note` 的结果（本单
/// 全部未删批次 × 对应工单，`ORDER BY pb.id ASC`），`assembly_map` 是这些批次所属
/// 装配件的解析结果（`AssemblyRepo::list_by_ids(..., include_deleted=false)`）。
///
/// **返回集合 = `assembly_map` 的 key**（不是「本单引用过」的装配件全集）：软删 /
/// 不存在的装配件不在 map 里，也就不出现在结果里 —— 打印侧据此不把它的 id 写进
/// `assembly_ids`，python 端 `assembly_map` 缺它，其子件按散件行打印。
pub(crate) fn note_shippable_sets(
    rows: &[(TPartBatch, TPart)],
    assembly_map: &HashMap<i64, TAssembly>,
) -> HashMap<i64, i32> {
    // 本单上每个子件 part 的批次量合计（同 part 多批次折叠，与 python 端
    // `_build_print_rows` 的 `qty_by_part` 求和同口径）。
    let mut by_part: HashMap<i64, ChildAgg> = HashMap::new();
    for (b, p) in rows {
        let slot = by_part
            .entry(p.id)
            .or_insert((p.assembly_id, p.quantity, 0));
        slot.2 += i64::from(b.quantity);
    }

    // 每个装配件取参与子件的 per_set 最小值；`part.quantity == 0` 的子件不参与。
    let mut min_per_set: HashMap<i64, i64> = HashMap::new();
    for (asm_id, part_quantity, note_qty) in by_part.values() {
        let Some(asm_id) = asm_id else { continue };
        let Some(asm) = assembly_map.get(asm_id) else {
            continue;
        };
        if *part_quantity == 0 {
            continue;
        }
        let per_set = note_qty * i64::from(asm.quantity) / i64::from(*part_quantity);
        min_per_set
            .entry(*asm_id)
            .and_modify(|cur| {
                if per_set < *cur {
                    *cur = per_set;
                }
            })
            .or_insert(per_set);
    }

    // 兜 0 → LEAST(..., asm.quantity) 收口 → 钳到 i32。
    let mut out: HashMap<i64, i32> = HashMap::with_capacity(assembly_map.len());
    for (asm_id, asm) in assembly_map {
        let cap = i64::from(asm.quantity);
        let sets = min_per_set.get(asm_id).copied().unwrap_or(0).min(cap);
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

    fn one_child(
        asm_id: i64,
        asm_qty: i32,
        part_qty: i32,
        note_qty: i32,
    ) -> (Vec<(TPartBatch, TPart)>, HashMap<i64, TAssembly>) {
        let rows = vec![(
            batch(1, 11, note_qty),
            child_part(11, Some(asm_id), part_qty),
        )];
        let map = HashMap::from([(asm_id, asm(asm_id, asm_qty))]);
        (rows, map)
    }

    #[test]
    fn sets_are_min_over_child_parts() {
        // 装配件 10 套，子件 A 整单 10 件 / 本单送 10 件 → 10 套；
        // 子件 B 整单 10 件 / 本单送 5 件 → 5 套 ⇒ min = 5
        let rows = vec![
            (batch(1, 11, 10), child_part(11, Some(10), 10)),
            (batch(2, 12, 5), child_part(12, Some(10), 10)),
        ];
        let map = HashMap::from([(10, asm(10, 10))]);
        assert_eq!(note_shippable_sets(&rows, &map).get(&10), Some(&5));
    }

    #[test]
    fn same_part_multiple_batches_are_summed() {
        let rows = vec![
            (batch(1, 11, 6), child_part(11, Some(10), 10)),
            (batch(2, 11, 4), child_part(11, Some(10), 10)),
        ];
        let map = HashMap::from([(10, asm(10, 10))]);
        assert_eq!(note_shippable_sets(&rows, &map).get(&10), Some(&10));
    }

    #[test]
    fn insufficient_child_yields_zero_sets() {
        // 装配件 10 套；子件整单 20 件（每套 2 件），本单只出 1 件 ⇒ 1*10/20 = 0 套
        let (rows, map) = one_child(10, 10, 20, 1);
        assert_eq!(note_shippable_sets(&rows, &map).get(&10), Some(&0));
    }

    #[test]
    fn over_delivery_is_capped_by_assembly_quantity() {
        // 子件整单 10 件，本单超交 100 件 ⇒ 100 套，LEAST 收口到 10
        let (rows, map) = one_child(10, 10, 10, 100);
        assert_eq!(note_shippable_sets(&rows, &map).get(&10), Some(&10));
    }

    #[test]
    fn zero_quantity_child_is_skipped_not_dragging_min() {
        // 子件 B 整单 0 件（本单送 100 件）：跳过，不参与 min ⇒ 仍取 A 的 8 套
        let rows = vec![
            (batch(1, 11, 8), child_part(11, Some(10), 10)),
            (batch(2, 12, 100), child_part(12, Some(10), 0)),
        ];
        let map = HashMap::from([(10, asm(10, 10))]);
        assert_eq!(note_shippable_sets(&rows, &map).get(&10), Some(&8));
    }

    #[test]
    fn assembly_with_no_participating_child_yields_zero() {
        // 全部子件 quantity = 0 ⇒ 无参与项 ⇒ 兜 0（而不是 asm.quantity）
        let (rows, map) = one_child(10, 10, 0, 100);
        assert_eq!(note_shippable_sets(&rows, &map).get(&10), Some(&0));
    }

    #[test]
    fn loose_part_and_unresolved_assembly_are_absent_from_result() {
        // 散件（assembly_id = None）+ 装配件子件但装配件不在 map（软删）⇒ 空结果
        let rows = vec![(batch(1, 11, 5), child_part(11, None, 10))];
        assert!(note_shippable_sets(&rows, &HashMap::new()).is_empty());
    }
}

//! 2026-10-04 新增：本单口径的装配件「可出货套数」（纯内存计算，无 SQL）
//!
//! 口径与 `part::service::list_enrichment::fetch_delivered_sets`（**全局已送套数**）
//! 同源，差别只有一处：分子换成「**本单**批次量」（本单要回答的是「这一单能出几套」，
//! 不是「这个装配件历史上共出几套」）。分子另按 `READY_TO_SHIP` 过滤，理由见下方
//! 「与全局已送口径的有意分叉」一节。
//!
//! 两个消费方共用本函数，避免同一公式在 service 各写一遍：
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
//! 3. 本仓同公式的其它 SQL 版（`fetch_delivered_sets`）与打印链路都以「各子件」
//!    为驱动单元，与本式一致。
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
//!
//! ## 与全局已送口径的有意分叉：分子只计本单 `READY_TO_SHIP` 批次
//!
//! `part::service::list_enrichment::fetch_delivered_sets`（**全局已送套数**）的分子
//! 带 `b.status IN ('DELIVERED','COMPLETED')` 过滤；本单版的分子只计
//! `status == 'READY_TO_SHIP'` 的批次。两者是两套**刻意不同**的口径，不是同一口径
//! 的两种实现。
//!
//! 2026-10-08 起本函数开始过滤状态（此前不过滤）：入单入口收敛为「只允许
//! `READY_TO_SHIP`」（`POST /scan` 的 21405 闸门），DRAFT 单上只可能挂着
//! `READY_TO_SHIP` 批次（提交后翻 `DELIVERED`）⇒ **加过滤与不过滤在「本单批次集合」
//! 这个口径上结果相同**，但与「可入单」定义同源，且对「挂单后被旁路改状态」的脏数据
//! 不再虚高套数。
//!
//! 三个消费方（详情 VO `line_items[].shippable_sets`、批量详情同上、扫码三层树的
//! `entry_max_sets`）都必须走本函数，否则同一装配件在详情页与扫码弹窗会给出两个套数。

use std::collections::HashMap;

use crate::modules::part::model::TPart;
use crate::shared::batch::TPartBatch;

/// 计入分子的唯一批次状态（与 `POST /scan` 的入单闸门同源）。
const STATUS_READY_TO_SHIP: &str = "READY_TO_SHIP";

/// 套数公式的窄投影输入：公式只读这 4 个值，别的一概不看。
///
/// 为什么要它：两个消费方拿得到的数据形状不同 —— 详情 VO / 批量详情手上有完整的
/// `(TPartBatch, TPart)` 行，而扫码三层树为了「零 N+1 + 一条 SQL 取可入单批次」只
/// 投影了 `part_id` / `part_quantity` / `batch_quantity` / `batch_status` 四列。
/// 让公式只吃这个窄结构，两个消费方各自在边界摊平一次，**公式本体只有一份**。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SetsBatchRow {
    pub part_id: i64,
    /// 该行批次的件数。
    pub batch_quantity: i32,
    /// 该行批次的状态（分子只计 `READY_TO_SHIP`）。
    pub batch_status: String,
}

/// 子件的窄投影（`min` 的定义域只需要 id 与工单总件数）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SetsChild {
    pub part_id: i64,
    /// `t_part.quantity`（工单总件数，整套比例的分母）。
    pub part_quantity: i32,
}

/// 把 `(TPartBatch, TPart)` 行摊平成 [`SetsBatchRow`]。宽 → 窄的唯一入口。
fn narrow(rows: &[(TPartBatch, TPart)]) -> Vec<SetsBatchRow> {
    rows.iter()
        .map(|(b, p)| SetsBatchRow {
            part_id: p.id,
            batch_quantity: b.quantity,
            batch_status: b.status.clone(),
        })
        .collect()
}

/// 把「装配件 → 全部子件」摊平成 [`SetsChild`]。宽 → 窄的唯一入口。
fn narrow_children(children_by_asm: &HashMap<i64, Vec<TPart>>) -> HashMap<i64, Vec<SetsChild>> {
    children_by_asm
        .iter()
        .map(|(k, v)| {
            (
                *k,
                v.iter()
                    .map(|c| SetsChild {
                        part_id: c.id,
                        part_quantity: c.quantity,
                    })
                    .collect(),
            )
        })
        .collect()
}

/// 套数公式本体（纯内存、无 SQL、无 IO）。
///
/// 入参见 [`note_shippable_sets`] 的同名条目；本函数是唯一实现，
/// `note_shippable_sets` 只是宽 → 窄的适配壳。
pub(crate) fn shippable_sets(
    rows: &[SetsBatchRow],
    asm_quantity: &HashMap<i64, i32>,
    children_by_asm: &HashMap<i64, Vec<SetsChild>>,
) -> HashMap<i64, i32> {
    // 本单上每个子件 part 的批次量合计（同 part 多批次折叠）。
    //
    // ⚠️ 分子**只计 `READY_TO_SHIP`**（2026-10-08）：入单只允许该状态 ⇒ DRAFT 单上
    // 挂到的批次恒为 READY_TO_SHIP，本过滤在正常数据上不改变结果，但让本口径与
    // 「可入单」定义同源，且对「挂单后被旁路改成 INSPECTION / IN_PROCESS」的脏数据
    // 不再虚高套数。
    //
    // 过滤在**本函数内**做而不是改 `rows` 的来源（`list_with_part_by_delivery_note`）：
    // 该 repo 方法还服务详情 VO 的行项装配（行上要显示真实 status 与 quantity），
    // 由它顺带过滤会让「本单有哪些行」与「本单能出几套」两个口径分叉。
    let mut note_qty_by_part: HashMap<i64, i64> = HashMap::new();
    for r in rows {
        if r.batch_status != STATUS_READY_TO_SHIP {
            continue;
        }
        *note_qty_by_part.entry(r.part_id).or_insert(0) += i64::from(r.batch_quantity);
    }

    // 每个装配件取「全部子件」的 per_set 最小值；`part.quantity == 0` 的子件不参与。
    let mut out: HashMap<i64, i32> = HashMap::with_capacity(asm_quantity.len());
    for (asm_id, cap) in asm_quantity {
        let cap = i64::from(*cap);
        let mut min_per_set: Option<i64> = None;
        let empty: Vec<SetsChild> = Vec::new();
        for child in children_by_asm.get(asm_id).unwrap_or(&empty) {
            if child.part_quantity == 0 {
                continue;
            }
            let note_qty = note_qty_by_part.get(&child.part_id).copied().unwrap_or(0);
            let per_set = note_qty * cap / i64::from(child.part_quantity);
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

/// 算每个装配件的「本单可出货套数」，key = 装配件 id。
///
/// 入参：
/// - `rows` —— `PartBatchRepo::list_with_part_by_delivery_note` 的结果（本单
///   全部未删批次 × 对应工单，`ORDER BY pb.id ASC`）。**状态不在这里过滤**，
///   过滤在本函数内做（见上方「分子只计 READY_TO_SHIP」）；
/// - `asm_quantity` —— `装配件 id → t_assembly.quantity`（工单总套数，既是
///   `per_set` 的比例因子、也是 `LEAST` 收口上界）。**只收 quantity 而不是整个
///   `TAssembly`**：本函数不需要装配件的其它字段，调用方就不必为了算套数而
///   clone 整行（`inner.rs::get_with_parts` 就是直接 `assembly_map.iter().map(|(id, a)|
///   (*id, a.quantity))` 的）；
/// - `children_by_asm` —— `装配件 id → 全部未软删子件`（`min` 的定义域，见模块
///   文档「`min` 的定义域」一节）。缺键 = 该装配件无子件 ⇒ 0 套。
///
/// **分子只计 `status == "READY_TO_SHIP"` 的批次**（2026-10-08 起），理由见模块
/// doc「与全局已送口径的有意分叉」一节。
///
/// **返回集合 = `asm_quantity` 的 key**（不是「本单引用过」的装配件全集）：软删 /
/// 不存在的装配件不在 map 里，也就不出现在结果里 —— 对应行上 `shippable_sets`
/// 取 `None`，其子件按散件行展示。
pub(crate) fn note_shippable_sets(
    rows: &[(TPartBatch, TPart)],
    asm_quantity: &HashMap<i64, i32>,
    children_by_asm: &HashMap<i64, Vec<TPart>>,
) -> HashMap<i64, i32> {
    shippable_sets(
        &narrow(rows),
        asm_quantity,
        &narrow_children(children_by_asm),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modules::assembly::model::TAssembly;
    use crate::modules::part::model::TPart;
    use crate::shared::batch::TPartBatch;
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

    /// ★ 2026-10-08 回归锁：分子**只计 READY_TO_SHIP**。
    ///
    /// 装配件 3 套；两个子件各有一半在 INSPECTION：
    /// - F1001-01：整单 9 件，批次 5 件 READY_TO_SHIP + 4 件 IN_PROCESS
    /// - F1001-02：整单 6 件，批次 4 件 READY_TO_SHIP + 2 件 INSPECTION
    ///
    /// 收窄前（不过滤状态）：per_set 分别为 9×3/9 = 3 与 6×3/6 = 3 ⇒ min = **3 套**。
    /// 收窄后（只计 READY_TO_SHIP）：per_set 为 5×3/9 = 1（向零截断）与
    /// 4×3/6 = 2 ⇒ min = **1 套**。后者才是「这批货现在真能凑出几套整套」的答案
    /// —— INSPECTION 的货还没过检，不能算进可出货套数。
    #[test]
    fn only_ready_to_ship_batches_count_toward_sets() {
        let mut ready = batch(1, 11, 5);
        ready.status = "READY_TO_SHIP".to_string();
        let mut inspecting = batch(2, 11, 4);
        inspecting.status = "IN_PROCESS".to_string();

        let mut ready2 = batch(3, 12, 4);
        ready2.status = "READY_TO_SHIP".to_string();
        let mut inspecting2 = batch(4, 12, 2);
        inspecting2.status = "INSPECTION".to_string();

        let rows = vec![
            (ready, child_part(11, Some(20), 9)),
            (inspecting, child_part(11, Some(20), 9)),
            (ready2, child_part(12, Some(20), 6)),
            (inspecting2, child_part(12, Some(20), 6)),
        ];
        let asms = HashMap::from([(20, 3)]);
        let children = HashMap::from([(
            20,
            vec![child_part(11, Some(20), 9), child_part(12, Some(20), 6)],
        )]);
        assert_eq!(
            note_shippable_sets(&rows, &asms, &children).get(&20),
            Some(&1),
            "只计 READY_TO_SHIP：5×3/9=1（截断）与 4×3/6=2，min=1；\
             若把 INSPECTION / IN_PROCESS 也计入会算出 3 套（错的）"
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

//! 送货单行项（`DeliveryNoteLineItem`）的唯一装配点（2026-10-10 新增）
//!
//! ## 为什么抽出来
//!
//! `line_items` 有两条出参路径 —— 单张详情 `inner.rs::get_with_parts`
//! （`GET /com/delivery/note/{id}`）与批量详情 `crud.rs::get_many_with_parts`
//! （`GET /com/delivery/note/batch-detail`）—— 两者的查数方式不同（单单逐个 /
//! 批量 `ANY`），但**从 `(批次行, 工单行, 三张 map)` 到 `DeliveryNoteLineItem`
//! 的装配逻辑逐字相同**。口径（含申请人回落、装配件父行字段、可出货套数挂载点）
//! 必须只有一份实现，否则改一处忘一处 ⇒ 同一张单在两条路径上给不同的行项。
//!
//! 调用方各自负责的**只有取数**：客户 map（leaf / parent）、装配件 map、套数 map。
//!
//! ## 入参为什么不收连接
//!
//! 装配是纯内存的（不碰 IO、不碰错误），故签名里没有 `conn` / `Result` —— 让
//! 「装配口径」在类型上就与「取数」解耦，改口径时能一眼看出不需要 DB。

use std::collections::HashMap;

use crate::modules::assembly::model::TAssembly;
use crate::modules::com::customer::model::TCustomer;
use crate::modules::part::model::TPart;
use crate::shared::batch::TPartBatch;

use super::super::vo::{DeliveryNoteLineItem, delivery_seq_i32};

/// 行 = 批次。`b` 定身份与量 / 状态，`p` 定工单投影，`*_map` 定跨表补齐的字段。
///
/// 三张 map 的键口径（调用方保证，本函数不再回查）：
/// - `leaf_map`：`t_part.customer_id` → L2 叶子客户；
/// - `parent_map`：L2 的 `parent_id` → L1 客户；
/// - `assembly_map`：`t_part.assembly_id` → 装配件（`include_deleted=false`，
///   软删 / 不存在时该键缺失 ⇒ 整行按散件行退化）。
///
/// `sets_map` 是「装配件 id → 本单可出货套数」（`shippable_sets::note_shippable_sets`
/// 的产物），只有子件行会命中。
pub(super) fn build_line_item(
    b: &TPartBatch,
    p: &TPart,
    leaf_map: &HashMap<i64, TCustomer>,
    parent_map: &HashMap<i64, TCustomer>,
    assembly_map: &HashMap<i64, TAssembly>,
    sets_map: &HashMap<i64, i32>,
) -> DeliveryNoteLineItem {
    let leaf = leaf_map.get(&p.customer_id);
    let parent = leaf
        .and_then(|l| l.parent_id)
        .and_then(|pid| parent_map.get(&pid));
    let leaf_name = leaf.map(|c| c.name.clone());
    // L1 自指（顶层客户没有 parent）时回落 L2 名，保证 `parent_customer_name`
    // 恒有值；`customer_path` 下面那条 `p != l` 再把「自指」折回单段。
    let parent_name = parent.map(|c| c.name.clone()).or_else(|| leaf_name.clone());
    let path = match (&parent_name, &leaf_name) {
        (Some(p), Some(l)) if p != l => Some(format!("{p} / {l}")),
        _ => leaf_name.clone(),
    };

    let asm = p.assembly_id.and_then(|id| assembly_map.get(&id));

    let batch_label = match &p.serial_no {
        Some(s) => format!("{s}B{:02}", b.batch_no),
        None => format!("批次{}", b.batch_no),
    };

    DeliveryNoteLineItem {
        id: b.id,
        part_id: p.id,
        batch_no: b.batch_no,
        // 加入本单的次序（bigint → i32 的收窄口径见 `delivery_seq_i32`）
        delivery_seq: delivery_seq_i32(b.delivery_seq),
        batch_label,
        serial_no: p.serial_no.clone().unwrap_or_default(),
        drawing_no: p.drawing_no.clone(),
        name: p.name.clone(),
        quantity: b.quantity,
        status: b.status.clone(),
        applicant_name: resolve_applicant_name(p, asm),
        request_date: Some(p.request_date),
        planned_delivery_date: Some(p.planned_delivery_date),
        system_delivery_date: p.system_delivery_date,
        order_no: p.order_no.clone(),
        note: p.note.clone(),
        customer_name: leaf_name,
        parent_customer_name: parent_name,
        customer_path: path,
        // 与 `leaf_map` 的查表 key 同值，不额外查库
        customer_id: p.customer_id,
        assembly_id: asm.map(|a| a.id),
        assembly_serial_no: asm.and_then(|a| a.serial_no.clone()),
        assembly_drawing_no: asm.map(|a| a.drawing_no.clone()),
        assembly_name: asm.map(|a| a.name.clone()),
        assembly_order_no: asm.and_then(|a| a.order_no.clone()),
        // 装配件工单总套数 + 本单可出货套数（散件 None）
        assembly_quantity: asm.map(|a| a.quantity),
        shippable_sets: p.assembly_id.and_then(|id| sets_map.get(&id).copied()),
    }
}

/// 行项「申请人」取值：**装配件子件为空时回落所属装配件的申请人**（2026-10-10 新增）。
///
/// ## 口径
///
/// 优先级 `t_part.applicant_name`（子件自己的，判空口径同历史实现：空串 ≡ 无）
/// → `t_assembly.applicant_name`（仅当该行是装配件子件且父装配件解析得到时）
/// → `None`（前端渲染「—」）。
///
/// ## 为什么需要回落
///
/// 装配件**设计上**由父件向下继承申请人（`assembly::service::crud` 的建单与
/// update 级联都会写子件），但存量数据里这条继承**大量未落地**：开发库实测
/// 490 条装配件子件中 360 条 `t_part.applicant_name` 是空串，而 90 个
/// `t_assembly.applicant_name` 无一为空 —— 送货单行项直接读子件列会把这些行
/// 渲染成「—」。回落让出参回到业务本意（这套子件是同一个申请人提的单），代价
/// **零额外 DB 往返**：装配件行（`assembly_serial_no` 等）本来就已批查进
/// `assembly_map`，`TAssembly` 自带 `applicant_name`。
///
/// ## 为什么散件不回落
///
/// `assembly_map` 里只有本单出现过的装配件 ⇒ 散件行的 `asm` 恒为 `None`，
/// 「回落」这条分支对散件天然不成立，无需额外判据。
///
/// ## 为什么不改写数据
///
/// 回落是**读侧**口径，不做存量回填：补写那 360 行等于凭空造一条「谁在什么时候
/// 把它同步给子件」的审计轨迹。若日后真要清洗存量，应作为独立的数据迁移立项
/// （按 `t_part.assembly_id` 从父装配件回填），本函数不必跟着改。
fn resolve_applicant_name(p: &TPart, asm: Option<&TAssembly>) -> Option<String> {
    if !p.applicant_name.is_empty() {
        return Some(p.applicant_name.clone());
    }
    asm.and_then(|a| a.applicant_name.clone())
        .filter(|s| !s.is_empty())
}

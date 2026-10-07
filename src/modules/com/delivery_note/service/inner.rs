//! 跨子模块共享的私有 helper。
//!
//! 全部以 `pub(super)` 暴露给 `service/` 下的兄弟模块（`group` / `crud` /
//! `lifecycle` / `scan`）。本文件不对外导出。
//!
//! 2026-10-08：事件写入 helper `write_event` 随事件子系统下线删除。
//!
//! 2026-09-22 D-5 重构：保持 `pub(super) async fn xxx(conn: &mut PgConnection, ...)`
//! 私有 helper 形态——内部 SQL 调用走 trait 方法（`DeliveryNoteRepoTrait`）实现于
//! `&mut PgConnection`，或走跨域 ZST 静态方法（`CustomerRepo` / `PartBatchRepo` 等）。

use std::collections::{HashMap, HashSet};

use sqlx::PgConnection;

use crate::modules::assembly::repo::AssemblyRepo;
use crate::modules::com::customer::model::TCustomer;
use crate::modules::com::customer::repo::CustomerRepo;
use crate::modules::com::delivery_note::repo::DeliveryNoteRepoTrait;
use crate::modules::part::repo::PartRepo;
use crate::modules::prod::batch::repo::PartBatchRepo;
use crate::shared::error::{AppError, code};

use super::super::model::DeliveryNote;
use super::super::vo::{DeliveryNoteDetailOut, DeliveryNoteLineItem, DeliveryNoteOut};
use super::note_shippable_sets;

// ===========================================================================
//  types
// ===========================================================================

const NAME_MAX_LEN: usize = 100;

// ===========================================================================
//  shared helpers
// ===========================================================================

/// 把 `Vec<DeliveryNote>` 转 `Vec<DeliveryNoteOut>`（批查客户 / 司机）。
///
/// 2026-09-22 D-5：内部 SQL 调用通过 trait 方法（`DeliveryNoteRepoTrait::xxx`）+ 跨域
/// ZST 静态调用（`CustomerRepo::xxx` / `PartBatchRepo::xxx`）混合。
///
/// 2026-10-08：删掉分组名 / leaf 名 / `scope_label` 三处派生查询 —— 范围列逻辑废弃
/// 后这些字段不再出参（见 `vo::DeliveryNoteOut`），留着就是纯浪费的一次 DB 往返。
pub(super) async fn build_note_outs(
    conn: &mut PgConnection,
    rows: &[DeliveryNote],
) -> Result<Vec<DeliveryNoteOut>, AppError> {
    if rows.is_empty() {
        return Ok(Vec::new());
    }

    // 客户 id 批查（仅 customer；范围列已废弃，不再连带查 leaf）
    let mut customer_ids: HashSet<i64> = HashSet::new();
    for n in rows {
        customer_ids.insert(n.customer_id);
    }
    let customers = CustomerRepo::list_by_ids(
        &mut *conn,
        &customer_ids.iter().copied().collect::<Vec<_>>(),
        false,
    )
    .await?;
    let mut cust_map: HashMap<i64, TCustomer> = HashMap::new();
    for c in customers {
        cust_map.insert(c.id, c);
    }

    // driver 批查
    let driver_ids: Vec<i64> = rows.iter().filter_map(|n| n.driver_worker_id).collect();
    let mut driver_map: HashMap<i64, String> = HashMap::new();
    if !driver_ids.is_empty() {
        let drivers = sqlx::query!(
            r#"SELECT id, name FROM t_worker WHERE id = ANY($1) AND deleted_at IS NULL"#,
            &driver_ids
        )
        .fetch_all(&mut *conn)
        .await?;
        for d in drivers {
            driver_map.insert(d.id, d.name);
        }
    }

    let mut out = Vec::with_capacity(rows.len());
    for n in rows {
        let l1 = cust_map.get(&n.customer_id);
        let (customer_name, parent_customer_name, customer_path) = match l1 {
            Some(c) if c.parent_id.is_none() => (
                Some(c.name.clone()),
                Some(c.name.clone()),
                Some(c.name.clone()),
            ),
            Some(c) => {
                let parent_name = c
                    .parent_id
                    .and_then(|p| cust_map.get(&p).map(|p| p.name.clone()));
                let path = match (&parent_name, &Some(c.name.clone())) {
                    (Some(p), Some(l)) => Some(format!("{p} / {l}")),
                    _ => Some(c.name.clone()),
                };
                (Some(c.name.clone()), parent_name, path)
            }
            None => (None, None, None),
        };

        let part_count = PartBatchRepo::list_by_delivery_note(&mut *conn, n.id)
            .await?
            .len() as i64;

        let driver_worker_name = n
            .driver_worker_id
            .and_then(|id| driver_map.get(&id).cloned());

        out.push(DeliveryNoteOut {
            id: n.id,
            version: n.version,
            delivery_note_no: n.delivery_note_no.clone(),
            customer_id: n.customer_id,
            customer_name,
            parent_customer_name,
            customer_path,
            status: n.status.clone(),
            submitted_at: n.submitted_at,
            picked_up_at: n.picked_up_at,
            submitted_by: n.submitted_by,
            picked_up_by: n.picked_up_by,
            driver_worker_id: n.driver_worker_id,
            driver_worker_name,
            part_count,
            note: n.note.clone(),
            delivery_date: n.delivery_date,
            created_at: n.created_at,
            updated_at: n.updated_at,
        });
    }
    Ok(out)
}

/// `get_with_parts`：单子 + 批次行（行 = 批次）+ 装配件父行字段。
pub(super) async fn get_with_parts(
    mut conn: &mut PgConnection,
    note_id: i64,
) -> Result<DeliveryNoteDetailOut, AppError> {
    let n = conn
        .note_get_by_id(note_id, false)
        .await?
        .ok_or_else(|| note_not_found(note_id))?;

    let rows = PartBatchRepo::list_with_part_by_delivery_note(&mut *conn, note_id).await?;

    // 批查 part 所属客户 (L2) + 父 L1
    let mut leaf_ids: HashSet<i64> = HashSet::new();
    for (_b, p) in &rows {
        leaf_ids.insert(p.customer_id);
    }
    let leaf_list = CustomerRepo::list_by_ids(
        &mut *conn,
        &leaf_ids.iter().copied().collect::<Vec<_>>(),
        false,
    )
    .await?;
    let leaf_map: HashMap<i64, TCustomer> = leaf_list.into_iter().map(|c| (c.id, c)).collect();
    let parent_ids: HashSet<i64> = leaf_map.values().filter_map(|c| c.parent_id).collect();
    let parent_list = if parent_ids.is_empty() {
        Vec::new()
    } else {
        CustomerRepo::list_by_ids(
            &mut *conn,
            &parent_ids.iter().copied().collect::<Vec<_>>(),
            false,
        )
        .await?
    };
    let parent_map: HashMap<i64, TCustomer> = parent_list.into_iter().map(|c| (c.id, c)).collect();

    // 批查询装配件
    let asm_ids: Vec<i64> = rows
        .iter()
        .filter_map(|(_b, p)| p.assembly_id)
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let mut assembly_map: HashMap<i64, crate::modules::assembly::model::TAssembly> = HashMap::new();
    if !asm_ids.is_empty() {
        let asms = AssemblyRepo::list_by_ids(&mut *conn, &asm_ids, false).await?;
        for a in asms {
            assembly_map.insert(a.id, a);
        }
    }

    // 2026-10-04 新增：循环前先聚合一次本单可出货套数。`min` 的定义域是「该
    // 装配件的**全部**子件」（本单没批次的子件按 0 参与），故必须补取子件 ——
    // 按装配件逐个取（单单装配件通常 1~3 个），与 `handler/print.rs` 同一写法。
    let asm_quantity: HashMap<i64, i32> = assembly_map
        .iter()
        .map(|(id, a)| (*id, a.quantity))
        .collect();
    let mut children_by_asm: HashMap<i64, Vec<crate::modules::part::model::TPart>> =
        HashMap::with_capacity(assembly_map.len());
    // 2026-10-04 review 第 3 轮（INFO-5）：遍历 `assembly_map` 的 key 而不是
    // `asm_ids`。差集 = 被 `include_deleted=false` 判为软删的装配件，它的子件查回来
    // 也用不上（`asm_quantity` 里没有该 key，`note_shippable_sets` 直接跳过），
    // 对应行上 `shippable_sets` 取 `None` 与现状一致，纯省一次 DB 往返。
    for aid in assembly_map.keys() {
        children_by_asm.insert(
            *aid,
            PartRepo::list_children(&mut *conn, *aid, false).await?,
        );
    }
    let sets_map = note_shippable_sets(&rows, &asm_quantity, &children_by_asm);

    let mut items: Vec<DeliveryNoteLineItem> = Vec::with_capacity(rows.len());
    for (b, p) in rows {
        let leaf = leaf_map.get(&p.customer_id);
        let parent = leaf
            .and_then(|l| l.parent_id)
            .and_then(|pid| parent_map.get(&pid));
        let leaf_name = leaf.map(|c| c.name.clone());
        let parent_name = parent.map(|c| c.name.clone()).or_else(|| leaf_name.clone()); // L1 自指同 leaf
        let path = match (&parent_name, &leaf_name) {
            (Some(p), Some(l)) if p != l => Some(format!("{p} / {l}")),
            _ => leaf_name.clone(),
        };

        let asm = p.assembly_id.and_then(|id| assembly_map.get(&id));

        let batch_label = match &p.serial_no {
            Some(s) => format!("{s}B{:02}", b.batch_no),
            None => format!("批次{}", b.batch_no),
        };

        items.push(DeliveryNoteLineItem {
            id: b.id,
            part_id: p.id,
            batch_no: b.batch_no,
            batch_label,
            serial_no: p.serial_no.clone().unwrap_or_default(),
            drawing_no: p.drawing_no.clone(),
            name: p.name.clone(),
            quantity: b.quantity,
            is_urgent: false, // TPart 当前投影不含该列
            status: b.status.clone(),
            applicant_name: Some(p.applicant_name.clone()).filter(|s| !s.is_empty()),
            request_date: Some(p.request_date),
            planned_delivery_date: Some(p.planned_delivery_date),
            system_delivery_date: p.system_delivery_date,
            order_no: p.order_no.clone(),
            note: p.note.clone(),
            customer_name: leaf_name,
            parent_customer_name: parent_name,
            customer_path: path,
            is_scanned: false,
            scanned: false,
            assembly_id: asm.map(|a| a.id),
            assembly_serial_no: asm.and_then(|a| a.serial_no.clone()),
            assembly_drawing_no: asm.map(|a| a.drawing_no.clone()),
            assembly_name: asm.map(|a| a.name.clone()),
            assembly_order_no: asm.and_then(|a| a.order_no.clone()),
            // 2026-10-04 新增：装配件工单总套数 + 本单可出货套数（散件 None）
            assembly_quantity: asm.map(|a| a.quantity),
            shippable_sets: p.assembly_id.and_then(|id| sets_map.get(&id).copied()),
        });
    }

    let head_vec = build_note_outs(conn, std::slice::from_ref(&n)).await?;
    let head = head_vec.into_iter().next().unwrap();

    Ok(DeliveryNoteDetailOut {
        head,
        line_items: items,
        scanned_serials: vec![],
    })
}

// ===========================================================================
//  error helpers
// ===========================================================================

pub(super) fn customer_not_found(id: i64) -> AppError {
    AppError::biz(
        code::BIZ_CUSTOMER_NOT_FOUND,
        format!("customer {id} not found"),
    )
}

pub(super) fn note_not_found(id: i64) -> AppError {
    AppError::biz(
        code::BIZ_DELIVERY_NOTE_NOT_FOUND,
        format!("delivery note {id} not found"),
    )
}

pub(super) fn group_not_found(id: i64) -> AppError {
    AppError::biz(
        code::BIZ_DELIVERY_GROUP_NOT_FOUND,
        format!("delivery group {id} not found"),
    )
}

pub(super) fn note_version_conflict(id: i64, have: i32, want: i32) -> AppError {
    AppError::biz(
        code::VERSION_CONFLICT,
        format!("delivery note {id} version conflict: have {have}, request {want}"),
    )
}

pub(super) fn version_conflict(id: i64, have: i32, want: i32) -> AppError {
    AppError::biz(
        code::VERSION_CONFLICT,
        format!("group {id} version conflict: have {have}, request {want}"),
    )
}

pub(super) fn validate_group_name(raw: &str) -> Result<String, AppError> {
    let trimmed = raw.trim().to_string();
    if trimmed.is_empty() {
        return Err(AppError::validation("group name must not be empty"));
    }
    if trimmed.chars().count() > NAME_MAX_LEN {
        return Err(AppError::validation(format!(
            "group name length must be <= {NAME_MAX_LEN}"
        )));
    }
    Ok(trimmed)
}

pub(super) async fn validate_l2_members(
    mut conn: &mut PgConnection,
    l1_id: &i64,
    ids: &[i64],
) -> Result<Vec<i64>, AppError> {
    let mut seen: HashSet<i64> = HashSet::new();
    let mut ordered: Vec<i64> = Vec::with_capacity(ids.len());
    for raw_id in ids {
        if !seen.insert(*raw_id) {
            continue;
        }
        let cust = CustomerRepo::get_by_id(&mut *conn, *raw_id, false)
            .await?
            .ok_or_else(|| customer_not_found(*raw_id))?;
        if cust.parent_id.as_ref() != Some(l1_id) {
            return Err(AppError::biz(
                code::BIZ_INVALID_VALUE,
                format!(
                    "customer {raw_id} is not an L2 child of L1 {l1_id} (parent_id={:?})",
                    cust.parent_id
                ),
            ));
        }
        if conn
            .group_list_active_member_by_customer(*raw_id)
            .await?
            .is_some()
        {
            return Err(AppError::biz(
                code::BIZ_DELIVERY_GROUP_MEMBER_CONFLICT,
                format!("customer {raw_id} is already an active member of another group"),
            ));
        }
        ordered.push(*raw_id);
    }
    Ok(ordered)
}

pub(super) async fn l1_children_lookup(
    conn: &mut PgConnection,
    l2_id: i64,
) -> Result<String, AppError> {
    let c = CustomerRepo::get_by_id(&mut *conn, l2_id, true)
        .await?
        .ok_or_else(|| customer_not_found(l2_id))?;
    Ok(c.name)
}

// _TCustomer 用：保留 TCustomer 字段被读到的副作用
#[allow(dead_code)]
fn _ensure_tcustomer_used(_: &TCustomer) {}

//! 跨子模块共享的私有 helper。
//!
//! 全部以 `pub(super)` 暴露给 `service/` 下的兄弟模块（`group` / `crud` /
//! `lifecycle` / `scan` / `print`）。本文件不对外导出。

use std::collections::{HashMap, HashSet};

use sqlx::PgConnection;

use crate::auth::rbac::CurrentUser;
use crate::infra::clock::now_naive;
use crate::infra::snowflake::SnowflakeIdGenerator;
use crate::modules::assembly::repo::AssemblyRepo;
use crate::modules::customer::model::TCustomer;
use crate::modules::customer::repo::CustomerRepo;
use crate::modules::part::repo::PartRepo;
use crate::modules::part_batch::repo::PartBatchRepo;
use crate::shared::error::{code, AppError};

use super::super::dto::{
    DeliveryNoteAddItem, DeliveryNoteDetailOut, DeliveryNoteLineItem, DeliveryNoteOut,
};
use super::super::model::{DeliveryNote, DeliveryNoteEvent, NoteScope};
use super::super::repo::{DeliveryGroupRepo, DeliveryNoteEventRepo, DeliveryNoteRepo};

// ===========================================================================
//  types
// ===========================================================================

/// service 内部用的分组形态（DB 装载 + 内存投影，避免暴露 `DeliveryGroup` 全字段）
#[derive(Debug, Clone)]
pub(super) struct GroupWithMemberIds {
    pub(super) group_id: i64,
    pub(super) member_ids: Vec<i64>,
}

const NAME_MAX_LEN: usize = 100;

/// P2 业务状态常量（与 DB 列 `status` 一致；与 `DeliveryNoteStatus::as_str()` 对齐）
const STATUS_DRAFT: &str = "DRAFT";
const STATUS_SUBMITTED: &str = "SUBMITTED";

/// P2 业务批次状态常量
const STATUS_INSPECTION: &str = "INSPECTION";
const STATUS_READY_TO_SHIP: &str = "READY_TO_SHIP";

// ===========================================================================
//  shared helpers
// ===========================================================================

/// 把 `Vec<DeliveryNote>` 转 `Vec<DeliveryNoteOut>`（批查客户 / 司机 / 范围名）。
pub(super) async fn build_note_outs(
    conn: &mut PgConnection,
    rows: &[DeliveryNote],
) -> Result<Vec<DeliveryNoteOut>, AppError> {
    if rows.is_empty() {
        return Ok(Vec::new());
    }

    // 客户 id 批查（仅 customer / leaf_customer，不含 group / driver）
    let mut customer_ids: HashSet<i64> = HashSet::new();
    for n in rows {
        customer_ids.insert(n.customer_id);
        if let Some(lid) = n.leaf_customer_id {
            customer_ids.insert(lid);
        }
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

    // 分组名批查
    let group_ids: Vec<i64> = rows.iter().filter_map(|n| n.delivery_group_id).collect();
    let mut group_map: HashMap<i64, String> = HashMap::new();
    if !group_ids.is_empty() {
        let groups = sqlx::query!(
            r#"SELECT id, name FROM t_delivery_group WHERE id = ANY($1) AND deleted_at IS NULL"#,
            &group_ids
        )
        .fetch_all(&mut *conn)
        .await?;
        for g in groups {
            group_map.insert(g.id, g.name);
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
                let parent_name = c.parent_id.and_then(|p| cust_map.get(&p).map(|p| p.name.clone()));
                let path = match (&parent_name, &Some(c.name.clone())) {
                    (Some(p), Some(l)) => Some(format!("{p} / {l}")),
                    _ => Some(c.name.clone()),
                };
                (Some(c.name.clone()), parent_name, path)
            }
            None => (None, None, None),
        };

        let leaf_customer_name = n.leaf_customer_id.and_then(|id| {
            cust_map.get(&id).map(|c| c.name.clone())
        });
        let group_name = n.delivery_group_id.and_then(|id| group_map.get(&id).cloned());

        let scope_label = match (n.delivery_group_id, n.leaf_customer_id) {
            (Some(_), _) => group_name.clone().or_else(|| Some("(group)".to_string())),
            (None, Some(_)) => leaf_customer_name.clone().or_else(|| Some("(leaf)".to_string())),
            (None, None) => customer_name.clone().or_else(|| Some("(L1)".to_string())),
        };

        let part_count = PartBatchRepo::list_by_delivery_note(&mut *conn, n.id)
            .await?
            .len() as i64;

        let driver_worker_name = n.driver_worker_id.and_then(|id| driver_map.get(&id).cloned());

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
            delivery_group_id: n.delivery_group_id,
            delivery_group_name: group_name,
            leaf_customer_id: n.leaf_customer_id,
            leaf_customer_name,
            scope_label,
        });
    }
    Ok(out)
}

/// `get_with_parts`：单子 + 批次行（行 = 批次）+ 装配件父行字段。
pub(super) async fn get_with_parts(
    conn: &mut PgConnection,
    note_id: i64,
) -> Result<DeliveryNoteDetailOut, AppError> {
    let n = DeliveryNoteRepo::get_by_id(&mut *conn, note_id, false)
        .await?
        .ok_or_else(|| note_not_found(note_id))?;

    let rows = PartBatchRepo::list_with_part_by_delivery_note(&mut *conn, note_id).await?;

    // 批查 part 所属客户 (L2) + 父 L1
    let mut leaf_ids: HashSet<i64> = HashSet::new();
    for (_b, p) in &rows {
        leaf_ids.insert(p.customer_id);
    }
    let leaf_list = CustomerRepo::list_by_ids(&mut *conn, &leaf_ids.iter().copied().collect::<Vec<_>>(), false).await?;
    let leaf_map: HashMap<i64, TCustomer> = leaf_list.into_iter().map(|c| (c.id, c)).collect();
    let parent_ids: HashSet<i64> = leaf_map.values().filter_map(|c| c.parent_id).collect();
    let parent_list = if parent_ids.is_empty() {
        Vec::new()
    } else {
        CustomerRepo::list_by_ids(&mut *conn, &parent_ids.iter().copied().collect::<Vec<_>>(), false).await?
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

    let mut items: Vec<DeliveryNoteLineItem> = Vec::with_capacity(rows.len());
    for (b, p) in rows {
        let leaf = leaf_map.get(&p.customer_id);
        let parent = leaf.and_then(|l| l.parent_id).and_then(|pid| parent_map.get(&pid));
        let leaf_name = leaf.map(|c| c.name.clone());
        let parent_name = parent
            .map(|c| c.name.clone())
            .or_else(|| leaf_name.clone()); // L1 自指同 leaf
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
            applicant_name: None,
            request_date: None,
            planned_delivery_date: None,
            system_delivery_date: None,
            order_no: None,
            note: None,
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

/// 范围校验：根据 note 当前 scope 校验每个 (batch → part) 客户是否合规。
///
/// - `L1Wide`：只跨 L1（part.customer_id L1 == note.customer_id）
/// - `Group(gid)`：part.customer_id ∈ group.member_ids
/// - `Leaf(cid)`：part.customer_id == leaf_customer_id
pub(super) async fn check_scope(
    conn: &mut PgConnection,
    obj: &DeliveryNote,
    part_customer_id: i64,
) -> Result<(), AppError> {
    let scope = scope_from_note(obj);
    match scope {
        NoteScope::L1Wide => Ok(()),
        NoteScope::Group(gid) => {
            // 加载 group + members
            let grp = DeliveryGroupRepo::get_by_id(&mut *conn, gid, false)
                .await?
                .ok_or_else(|| AppError::biz(
                    code::BIZ_DELIVERY_GROUP_NOT_FOUND,
                    format!("delivery group {gid} not found"),
                ))?;
            // 必须仍是同一 L1 客户
            if grp.customer_id != obj.customer_id {
                return Err(AppError::biz(
                    code::BIZ_DELIVERY_NOTE_SCOPE_MISMATCH,
                    "group 与本单 L1 不匹配",
                ));
            }
            let members = DeliveryGroupRepo::list_members_by_group_ids(&mut *conn, &[gid], false).await?;
            if !members.iter().any(|m| m.customer_id == part_customer_id) {
                return Err(AppError::biz(
                    code::BIZ_DELIVERY_NOTE_SCOPE_MISMATCH,
                    format!("part 客户 {part_customer_id} 不在分组 {gid} 成员中"),
                ));
            }
            Ok(())
        }
        NoteScope::Leaf(cid) => {
            if part_customer_id != cid {
                return Err(AppError::biz(
                    code::BIZ_DELIVERY_NOTE_SCOPE_MISMATCH,
                    format!("part 客户 {part_customer_id} != 单厂单 leaf {cid}"),
                ));
            }
            Ok(())
        }
    }
}

pub(super) fn scope_from_note(n: &DeliveryNote) -> NoteScope {
    if let Some(gid) = n.delivery_group_id {
        NoteScope::Group(gid)
    } else if let Some(cid) = n.leaf_customer_id {
        NoteScope::Leaf(cid)
    } else {
        NoteScope::L1Wide
    }
}

/// add_parts 内部实现（被 create_draft 与 add_parts handler 复用）。
pub(super) async fn add_parts_inner(
    conn: &mut PgConnection,
    snowflake: &SnowflakeIdGenerator,
    note_id: i64,
    items: &[DeliveryNoteAddItem],
    version: i32,
    current: &CurrentUser,
) -> Result<(), AppError> {
    let obj = DeliveryNoteRepo::get_by_id(&mut *conn, note_id, false)
        .await?
        .ok_or_else(|| note_not_found(note_id))?;
    if obj.version != version {
        return Err(note_version_conflict(note_id, obj.version, version));
    }
    if obj.status != STATUS_DRAFT {
        return Err(AppError::biz(
            code::BIZ_DELIVERY_NOTE_PARTS_LOCKED,
            format!("送货单已提交（{}），不能新增零件；如需调整请先撤回。", obj.status),
        ));
    }
    if items.is_empty() {
        return Ok(());
    }

    // 加载所有 batch + part + part 客户
    let mut batches_by_id: HashMap<i64, crate::modules::part_batch::model::TPartBatch> = HashMap::new();
    for it in items {
        let b = PartBatchRepo::get_by_id(&mut *conn, it.batch_id, false)
            .await?
            .ok_or_else(|| AppError::biz(
                code::BIZ_PART_BATCH_NOT_FOUND,
                format!("batch {} 不存在或已删除", it.batch_id),
            ))?;
        batches_by_id.insert(it.batch_id, b);
    }

    let part_ids: Vec<i64> = batches_by_id.values().map(|b| b.part_id).collect();
    let parts = PartRepo::list_by_ids(&mut *conn, &part_ids, false).await?;
    let part_map: HashMap<i64, crate::modules::part::model::TPart> =
        parts.into_iter().map(|p| (p.id, p)).collect();

    // 加载所有 part 客户（含 L2）以推 L1
    let mut part_customer_ids: HashSet<i64> =
        part_map.values().map(|p| p.customer_id).collect();
    part_customer_ids.insert(obj.customer_id);
    let customers = CustomerRepo::list_by_ids(
        &mut *conn,
        &part_customer_ids.iter().copied().collect::<Vec<_>>(),
        false,
    )
    .await?;
    let cust_map: HashMap<i64, TCustomer> = customers.into_iter().map(|c| (c.id, c)).collect();

    let now = now_naive();

    for it in items {
        let batch = batches_by_id.get(&it.batch_id).cloned().unwrap();
        let part = part_map
            .get(&batch.part_id)
            .cloned()
            .ok_or_else(|| AppError::biz(
                code::BIZ_PART_NOT_FOUND,
                format!("batch {} 所属工单 {} 不存在", batch.id, batch.part_id),
            ))?;

        if batch.status != STATUS_INSPECTION && batch.status != STATUS_READY_TO_SHIP {
            return Err(AppError::biz(
                code::BIZ_DELIVERY_NOTE_PART_NOT_READY,
                format!(
                    "part {} 批次 {} status={}, only INSPECTION / READY_TO_SHIP allowed at draft entry",
                    part.id, batch.batch_no, batch.status
                ),
            ));
        }

        let part_cust = cust_map.get(&part.customer_id).cloned().ok_or_else(|| AppError::biz(
            code::BIZ_CUSTOMER_NOT_FOUND,
            format!("part {} 所属客户 {} 不存在", part.id, part.customer_id),
        ))?;
        let part_l1_id = match part_cust.parent_id {
            Some(pid) => pid,
            None => part_cust.id,
        };
        if part_l1_id != obj.customer_id {
            return Err(AppError::biz(
                code::BIZ_DELIVERY_NOTE_PARTS_MULTIPLE_CUSTOMERS,
                format!(
                    "part {} 一级客户 {} != note 一级客户 {}",
                    part.id, part_l1_id, obj.customer_id
                ),
            ));
        }

        // 范围校验（设计 §3.4）
        check_scope(conn, &obj, part.customer_id).await?;

        // 批次挂单冲突
        if let Some(other_id) = batch.delivery_note_id
            && other_id != note_id
            && let Some(other) = DeliveryNoteRepo::get_by_id(&mut *conn, other_id, false).await?
            && (other.status == STATUS_DRAFT || other.status == STATUS_SUBMITTED)
        {
            return Err(AppError::biz(
                code::BIZ_DELIVERY_NOTE_PART_ALREADY_ASSIGNED,
                format!(
                    "part {} 批次 {} already on active delivery note {}",
                    part.id, batch.batch_no, other_id
                ),
            ));
        }

        // 部分量 → 拆
        let target_id = if let Some(qty) = it.quantity {
            if qty <= 0 || qty > batch.quantity {
                return Err(AppError::biz(
                    code::BIZ_PART_BATCH_INVALID_QUANTITY,
                    format!(
                        "入单数量必须 ∈ [1, {}]（批次 {} 当前 {} 件），got {}",
                        batch.quantity, batch.batch_no, batch.quantity, qty
                    ),
                ));
            }
            if qty < batch.quantity {
                // 拆：构造新批次
                let new_id = snowflake.next_id();
                PartBatchRepo::split_batch(
                    conn,
                    new_id,
                    batch.id,
                    batch.version,
                    part.id,
                    qty,
                    &batch.status,
                    batch.location.as_deref(),
                    batch.current_holder_id,
                    batch.next_process_id,
                    batch.placed_at,
                    now,
                    Some(current.id),
                    Some(current.id),
                )
                .await?;
                new_id
            } else {
                batch.id
            }
        } else {
            batch.id
        };

        let target = if target_id == batch.id {
            batch.clone()
        } else {
            PartBatchRepo::get_by_id(&mut *conn, target_id, false)
                .await?
                .ok_or_else(|| AppError::biz(
                    code::BIZ_PART_BATCH_NOT_FOUND,
                    format!("newly split batch {target_id} not found"),
                ))?
        };

        let affected = PartBatchRepo::attach_to_note(
            &mut *conn,
            target.id,
            target.version,
            note_id,
            now,
            Some(current.id),
        )
        .await?;
        if affected == 0 {
            return Err(AppError::biz(
                code::VERSION_CONFLICT,
                format!("batch {} version conflict", target.id),
            ));
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn write_event(
    conn: &mut PgConnection,
    snowflake: &SnowflakeIdGenerator,
    note_id: i64,
    event_type: super::super::model::DeliveryNoteEventType,
    from_status: Option<String>,
    to_status: Option<String>,
    note: Option<String>,
    created_by: Option<i64>,
) -> Result<(), AppError> {
    let now = now_naive();
    let ev = DeliveryNoteEvent {
        id: snowflake.next_id(),
        delivery_note_id: note_id,
        event_type: event_type.as_str().to_string(),
        from_status,
        to_status,
        note,
        created_by,
        created_at: now,
    };
    DeliveryNoteEventRepo::add_event(&mut *conn, &ev).await?;
    Ok(())
}

// ===========================================================================
//  error helpers
// ===========================================================================

pub(super) fn customer_not_found(id: i64) -> AppError {
    AppError::biz(code::BIZ_CUSTOMER_NOT_FOUND, format!("customer {id} not found"))
}

pub(super) fn note_not_found(id: i64) -> AppError {
    AppError::biz(code::BIZ_DELIVERY_NOTE_NOT_FOUND, format!("delivery note {id} not found"))
}

pub(super) fn group_not_found(id: i64) -> AppError {
    AppError::biz(code::BIZ_DELIVERY_GROUP_NOT_FOUND, format!("delivery group {id} not found"))
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
    conn: &mut PgConnection,
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
        if DeliveryGroupRepo::list_active_member_by_customer(&mut *conn, *raw_id)
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

pub(super) async fn l1_children_lookup(conn: &mut PgConnection, l2_id: i64) -> Result<String, AppError> {
    let c = CustomerRepo::get_by_id(&mut *conn, l2_id, true)
        .await?
        .ok_or_else(|| customer_not_found(l2_id))?;
    Ok(c.name)
}

// _TCustomer 用：保留 TCustomer 字段被读到的副作用
#[allow(dead_code)]
fn _ensure_tcustomer_used(_: &TCustomer) {}

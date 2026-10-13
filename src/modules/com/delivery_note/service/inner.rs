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
use super::line_item::build_line_item;
use super::shippable_sets::note_shippable_sets;

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
///
/// 2026-10-17：客户那一趟从 `CustomerRepo::list_by_ids`（只取 name + parent_id，
/// 再在内存里拼「`父 / 子`」两段）换成 `shared::customer::paths_of` —— 送货单挂在
/// **L1 根**上（`find_or_create` 的判定键），但零件挂在任意深度的叶子上，两段拼接
/// 在三层以上的树上会丢掉中间层。往返条数不变（1 条），多出来的向上递归由该函数内部
/// 的 CTE 承担。
pub(super) async fn build_note_outs(
    conn: &mut PgConnection,
    rows: &[DeliveryNote],
) -> Result<Vec<DeliveryNoteOut>, AppError> {
    if rows.is_empty() {
        return Ok(Vec::new());
    }

    // 客户 id 批查（仅 customer；范围列已废弃，不再连带查 leaf）
    let customer_ids: Vec<i64> = rows.iter().map(|n| n.customer_id).collect();
    let paths = crate::shared::customer::paths_of(&mut *conn, &customer_ids).await?;

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
        // 送货单恒挂在 L1 根上 ⇒ `own_name` 与 `root_name` 相同、`path` 就是根自己的
        // 名字（`shared::customer::join_customer_path` 的 `len == 1` 分支）。两个字段
        // 都从同一份 map 取，前端按 `customer_path` 展示即可。
        let cp = paths.get(&n.customer_id);
        let customer_name = cp.map(|c| c.own_name.clone());
        let customer_path = cp.map(|c| c.path.clone()).unwrap_or_default();

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
            customer_path,
            status: n.status.clone(),
            submitted_at: n.submitted_at,
            picked_up_at: n.picked_up_at,
            driver_worker_name,
            part_count,
            note: n.note.clone(),
            delivery_date: n.delivery_date,
            created_at: n.created_at,
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

    // 2026-10-17：客户三列（自身名 / 根名 / 第 2 层到叶子的路径）由**一次**
    // `paths_of` 产出。此前是两趟 `list_by_ids`（零件自身客户 + 其直接父）再在内存里
    // 拼两段 —— 那是两层树的形状，深树上必丢中间层。
    let part_customer_ids: Vec<i64> = rows.iter().map(|(_b, p)| p.customer_id).collect();
    let paths_map = crate::shared::customer::paths_of(&mut *conn, &part_customer_ids).await?;

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
    // 按装配件逐个取（单单装配件通常 1~3 个）。
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
    for (b, p) in &rows {
        items.push(build_line_item(b, p, &paths_map, &assembly_map, &sets_map));
    }

    let head_vec = build_note_outs(conn, std::slice::from_ref(&n)).await?;
    let head = head_vec.into_iter().next().unwrap();

    Ok(DeliveryNoteDetailOut {
        head,
        line_items: items,
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

/// 校验送货分组的成员集合，返回**去重后**的成员 id（保持入参顺序）。
///
/// ## 口径（2026-10-17 语义变更）
///
/// 旧判据是「成员必须是 L1 的**直接子**」（`cust.parent_id == Some(l1_id)`）。那是
/// **两层树**下的等价写法（叶子恒是根的直接子）；客户树放开任意深度后它既漏掉更深的
/// 合法成员，也与「零件恒挂在叶子（L4）而分组成员可以是任意后代」的实际用法脱节。
/// 新判据：**成员必须是 `l1_id` 的任意后代**（即 `subtree_ids(l1_id)` 命中）。
///
/// ## 为什么用 `subtree_ids` 而不是逐个 `root_id_of` 比对
///
/// - **N+1**：`root_id_of` 每个成员一次往返，一个分组动辄十几个成员；`subtree_ids`
///   一次递归 CTE 拿全集，之后判 `contains` 是纯内存操作。
/// - **比名字不可靠**：`t_customer.name` **非唯一**（同名兄弟节点真实存在），按
///   `root_name` 比对会把两个同名不同根的分组判成同一个。
/// - **成环容错**：`subtree_ids` 用 `UNION` 去重，环形脏数据下收敛而不挂死；成员落
///   在环里时它只是 `contains` 为 false ⇒ 报 20104 而不是 500。
///
/// 注意**不包含 L1 自身**（`subtree_ids` 含自身，故显式剔除）：分组键就是那个根，
/// 把根本身列为自己的成员没有业务含义，且与旧口径（根的 `parent_id` 为 NULL ⇒ 恒被拒）
/// 保持一致。
///
/// 成员仍**不要求是叶子**：分组的语义是「把同一个 L1 名下的若干客户按送货批次分单」，
/// 允许挂中间层只是让历史两级数据和深树数据都能进同一张分组表；真正能进单的客户由
/// 零件侧的叶子闸门（`shared::customer::require_leaf_customer`）保证。
pub(super) async fn validate_group_members(
    mut conn: &mut PgConnection,
    l1_id: &i64,
    ids: &[i64],
) -> Result<Vec<i64>, AppError> {
    // 一次取回「根 + 全部后代」，循环内只做内存判包含（禁 N+1）。
    // ⚠️ `subtree_ids` **含自身**，而「根不是自己的后代」是硬要求（分组键就是它；
    // 旧口径 `parent_id == l1_id` 对根恒 false）⇒ 显式剔除。
    let mut subtree: HashSet<i64> = crate::shared::customer::subtree_ids(&mut *conn, *l1_id)
        .await?
        .into_iter()
        .collect();
    subtree.remove(l1_id);
    let mut seen: HashSet<i64> = HashSet::new();
    let mut ordered: Vec<i64> = Vec::with_capacity(ids.len());
    for raw_id in ids {
        if !seen.insert(*raw_id) {
            continue;
        }
        if !subtree.contains(raw_id) {
            // 分不出「不存在」与「存在但不是后代」时补一次单查：前者必须报 20102
            // （旧实现逐个 `get_by_id` 就是这个口径），后者才是 20104。只在**错误
            // 路径**上多这一次往返，正常路径仍是 1 条 SQL。
            if CustomerRepo::get_by_id(&mut *conn, *raw_id, false)
                .await?
                .is_none()
            {
                return Err(customer_not_found(*raw_id));
            }
            return Err(AppError::biz(
                code::BIZ_INVALID_VALUE,
                format!(
                    "customer {raw_id} 不是 customer {l1_id} 的后代（分组键本身不算后代；\
                     分组成员必须挂在该 L1 根下的任意深度）"
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

/// 按 id 取客户名（分组成员的显示名）。
///
/// 2026-10-17 改名：旧名 `l1_children_lookup` / 形参 `l2_id` 暗示「只查 L1 的直接子」，
/// 但实现一直是「按 id 单查」（任意深度都对）。批量场景请改用
/// [`crate::shared::customer::paths_of`] 一次取齐，避免逐个往返。
pub(super) async fn member_name_lookup(
    conn: &mut PgConnection,
    member_id: i64,
) -> Result<String, AppError> {
    let c = CustomerRepo::get_by_id(&mut *conn, member_id, true)
        .await?
        .ok_or_else(|| customer_not_found(member_id))?;
    Ok(c.name)
}

// _TCustomer 用：保留 TCustomer 字段被读到的副作用
#[allow(dead_code)]
fn _ensure_tcustomer_used(_: &TCustomer) {}

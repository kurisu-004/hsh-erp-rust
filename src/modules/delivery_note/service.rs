//! delivery_note 域业务逻辑
//!
//! 对应 Python myERP/service/delivery_note.py（及 _<d>_*.py helper）。
//! 实施约定：方法签名接收 `&mut PgConnection`，由 handler 开 tx 并 commit。
//!
//! ## Phase 范围
//! - **P1**：送货分组（DeliveryGroupService） + `classify()`
//! - **P2**：送货单生命周期（DeliveryNoteService：create / get / add / remove /
//!   submit / recall / pickup-scan / pickup / soft_delete / list / events /
//!   candidate_parts）
//! - **P3**：`scan_add`（自动路由 + 整套拒绝 + 幂等）
//! - **P4**：`print_xlsx`（note + labels + line_item_ids 子集；CPU 密集部分包
//!   `tokio::task::spawn_blocking`）

use std::collections::{HashMap, HashSet};
use std::path::Path;

use axum::http::StatusCode;
use sqlx::{PgConnection, PgPool};

use crate::auth::rbac::{CurrentUser, Role};
use crate::infra::clock::now_naive;
use crate::infra::serial::next_delivery_note_no;
use crate::infra::snowflake::SnowflakeIdGenerator;
use crate::modules::assembly::model::TAssembly;
use crate::modules::assembly::repo::AssemblyRepo;
use crate::modules::customer::model::TCustomer;
use crate::modules::customer::repo::CustomerRepo;
use crate::modules::part::model::TPart;
use crate::modules::part::repo::PartRepo;
use crate::modules::part_batch::model::TPartBatch;
use crate::modules::part_batch::repo::PartBatchRepo;
use crate::modules::worker::repo::WorkerRepo;
use crate::modules::work_type::repo::WorkTypeRepo;
use crate::shared::error::{code, AppError};

use super::dto::{
    DeliveryNoteAddItem, DeliveryNoteCandidatePart, DeliveryNoteCreateRequest,
    DeliveryNoteDetailOut, DeliveryNoteEventOut, DeliveryNoteLineItem, DeliveryNoteListOut,
    DeliveryNoteOut, DeliveryNotePickupScanOut, DeliveryNoteUpdateRequest, RecentItemDto,
    ResolvedEntityDto, ScanBatchDto, ScanDeliveryNoteSummaryDto, ScanDeliveryOut,
    ScanFailureDto, ScanOutcomeDto,
};
use super::model::{
    DeliveryGroup, DeliveryGroupMember, DeliveryNote, DeliveryNoteEvent,
    DeliveryNoteEventType, DeliveryNoteSortKey, NoteScope,
};
use super::print::{self, PrintRequest};
use super::repo::{DeliveryGroupRepo, DeliveryNoteEventRepo, DeliveryNoteRepo, SortDir};

// ---------------------------------------------------------------------------
//  NoteScope / classify  (设计 §3.2)
// ---------------------------------------------------------------------------

/// service 内部用的分组形态（DB 装载 + 内存投影，避免暴露 `DeliveryGroup` 全字段）
#[derive(Debug, Clone)]
pub struct GroupWithMemberIds {
    pub group_id: i64,
    pub member_ids: Vec<i64>,
}

impl NoteScope {
    pub fn classify(leaf_customer_id: i64, groups: &[GroupWithMemberIds]) -> Self {
        if groups.is_empty() {
            return Self::L1Wide;
        }
        for g in groups {
            if g.member_ids.contains(&leaf_customer_id) {
                return Self::Group(g.group_id);
            }
        }
        Self::Leaf(leaf_customer_id)
    }
}

// ---------------------------------------------------------------------------
//  scan_add 解析（设计 §5，纯函数 + service 内 combine DB 数据）
// ---------------------------------------------------------------------------

/// `scan_add` 第一步的解析形态（基于「part 优先 / 退避 assembly」结果）。
///
/// Idempotency 假设（与 Python `service/delivery_note.py::pickup_scan` 一致，
/// 在 comment block 中固化说明）：
/// > part serial 与 assembly serial 由同一 `t_serial_counter`（per-prefix）
/// > 池子发放；part 表 `uk_t_part_serial_no` partial unique + assembly 表
/// > `uk_t_assembly_serial_no` partial unique 都在 `serial_no IS NOT NULL AND
/// > deleted_at IS NULL` 域内全局唯一，因此 **同一 serial 不可能既挂在 part
/// > 也挂在 assembly** —— 解析分支不会有歧义。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScanKind {
    /// Part 命中，且 `part.assembly_id IS NULL` → 散件扫描。
    StandalonePart,
    /// Part 命中，但 `part.assembly_id IS NOT NULL` → 视作装配件整套。
    /// 取 `assembly_id` 加载装配件头 + 全部子件。
    PartOfAssembly(i64),
    /// Part 未命中但 Assembly 命中 → 装配件总图。
    Assembly,
    /// 两者都没命中 → 404 `BIZ_DELIVERY_SCAN_UNKNOWN_CODE`。
    Unknown,
}

/// 纯函数：根据 SQL 已装载的 part / assembly 行决定 scan 处理的形态。
/// 测试覆盖在 `mod scan_resolve_tests`。
pub fn resolve_scan_kind(part: Option<&TPart>, assembly: Option<&TAssembly>) -> ScanKind {
    match (part, assembly) {
        (Some(p), _) => {
            if let Some(aid) = p.assembly_id {
                ScanKind::PartOfAssembly(aid)
            } else {
                ScanKind::StandalonePart
            }
        }
        (None, Some(_)) => ScanKind::Assembly,
        (None, None) => ScanKind::Unknown,
    }
}

// ---------------------------------------------------------------------------
//  DeliveryGroupService  (P1，已实现)
// ---------------------------------------------------------------------------

const NAME_MAX_LEN: usize = 100;

pub struct DeliveryGroupService;

impl DeliveryGroupService {
    pub async fn list_for_l1(
        conn: &mut PgConnection,
        l1_id: i64,
        current: &CurrentUser,
    ) -> Result<super::dto::DeliveryGroupListOut, AppError> {
        current.require_any_role(&[
            Role::Manager,
            Role::Clerk,
            Role::Inspector,
            Role::CncProgrammer,
        ])?;

        let l1 = CustomerRepo::get_by_id(&mut *conn, l1_id, false)
            .await?
            .ok_or_else(|| customer_not_found(l1_id))?;
        if l1.parent_id.is_some() {
            return Err(AppError::biz(
                code::BIZ_INVALID_VALUE,
                format!("customer {l1_id} is not an L1 root (parent_id must be NULL)"),
            ));
        }

        let groups = DeliveryGroupRepo::list_by_customer(&mut *conn, l1_id, false).await?;
        let group_ids: Vec<i64> = groups.iter().map(|g| g.id).collect();
        let members =
            DeliveryGroupRepo::list_members_by_group_ids(&mut *conn, &group_ids, false).await?;
        let l2_children = CustomerRepo::list_children(&mut *conn, l1_id, false).await?;

        let mut groups_out = Vec::with_capacity(groups.len());
        let mut membered_l2_ids: HashSet<i64> = HashSet::new();
        for g in &groups {
            let mut mems = Vec::new();
            for m in members.iter().filter(|m| m.group_id == g.id) {
                membered_l2_ids.insert(m.customer_id);
                let name = l2_children
                    .iter()
                    .find(|c| c.id == m.customer_id)
                    .map(|c| c.name.clone())
                    .unwrap_or_else(|| "(已删除)".to_string());
                mems.push(super::dto::DeliveryGroupMemberOut {
                    customer_id: m.customer_id,
                    customer_name: name,
                });
            }
            groups_out.push(Self::assemble_group_out(g, mems));
        }

        let ungrouped_customers: Vec<super::dto::UngroupedCustomerOut> = l2_children
            .into_iter()
            .filter(|c| !membered_l2_ids.contains(&c.id))
            .map(|c| super::dto::UngroupedCustomerOut {
                id: c.id,
                name: c.name,
            })
            .collect();

        Ok(super::dto::DeliveryGroupListOut {
            groups: groups_out,
            ungrouped_customers,
        })
    }

    pub async fn create(
        conn: &mut PgConnection,
        snowflake: &SnowflakeIdGenerator,
        req: super::dto::CreateDeliveryGroupRequest,
        current: &CurrentUser,
    ) -> Result<super::dto::DeliveryGroupOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk])?;

        let l1 = CustomerRepo::get_by_id(&mut *conn, req.customer_id, false)
            .await?
            .ok_or_else(|| customer_not_found(req.customer_id))?;
        if l1.parent_id.is_some() {
            return Err(AppError::biz(
                code::BIZ_INVALID_VALUE,
                format!("customer {} is not an L1 root", req.customer_id),
            ));
        }

        let name = validate_group_name(&req.name)?;

        if DeliveryGroupRepo::get_by_name(&mut *conn, req.customer_id, &name, false)
            .await?
            .is_some()
        {
            return Err(AppError::biz(
                code::BIZ_DELIVERY_GROUP_DUPLICATE_NAME,
                format!("group name '{name}' already exists under customer {}", req.customer_id),
            ));
        }

        let validated_members =
            validate_l2_members(conn, &l1.id, &req.member_customer_ids).await?;

        let now = now_naive();
        let group = DeliveryGroup {
            id: snowflake.next_id(),
            customer_id: req.customer_id,
            name: name.clone(),
            version: 0,
            created_at: now,
            created_by: Some(current.id),
            updated_at: now,
            updated_by: Some(current.id),
            deleted_at: None,
        };
        DeliveryGroupRepo::insert(&mut *conn, &group).await?;

        for customer_id in &validated_members {
            let m = DeliveryGroupMember {
                id: snowflake.next_id(),
                group_id: group.id,
                customer_id: *customer_id,
                created_at: now,
                created_by: Some(current.id),
                deleted_at: None,
            };
            DeliveryGroupRepo::insert_member(&mut *conn, &m).await?;
        }

        let mut member_outs: Vec<super::dto::DeliveryGroupMemberOut> =
            Vec::with_capacity(validated_members.len());
        for cid in &validated_members {
            let name = l1_children_lookup(conn, *cid)
                .await
                .unwrap_or_else(|_| "(已删除)".into());
            member_outs.push(super::dto::DeliveryGroupMemberOut {
                customer_id: *cid,
                customer_name: name,
            });
        }
        Ok(Self::assemble_group_out(&group, member_outs))
    }

    pub async fn update(
        conn: &mut PgConnection,
        snowflake: &SnowflakeIdGenerator,
        group_id: i64,
        req: super::dto::UpdateDeliveryGroupRequest,
        current: &CurrentUser,
    ) -> Result<super::dto::DeliveryGroupOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk])?;

        let group = DeliveryGroupRepo::get_by_id(&mut *conn, group_id, false)
            .await?
            .ok_or_else(|| group_not_found(group_id))?;

        if group.version != req.version {
            return Err(version_conflict(group_id, group.version, req.version));
        }

        let mut next_name = group.name.clone();
        let mut name_changed = false;
        if let Some(ref raw) = req.name {
            let new_name = validate_group_name(raw)?;
            if new_name != group.name {
                if DeliveryGroupRepo::get_by_name(&mut *conn, group.customer_id, &new_name, false)
                    .await?
                    .is_some()
                {
                    return Err(AppError::biz(
                        code::BIZ_DELIVERY_GROUP_DUPLICATE_NAME,
                        format!(
                            "group name '{new_name}' already exists under customer {}",
                            group.customer_id
                        ),
                    ));
                }
                next_name = new_name;
                name_changed = true;
            }
        }

        let mut new_member_ids: Vec<i64> = Vec::new();
        let mut replace_members = false;
        if let Some(ref new_ids) = req.member_customer_ids {
            let validated = validate_l2_members(conn, &group.customer_id, new_ids).await?;
            new_member_ids = validated;
            replace_members = true;
        }

        let now = now_naive();

        if name_changed {
            let affected = DeliveryGroupRepo::update(
                &mut *conn,
                group_id,
                group.version,
                &next_name,
                now,
                Some(current.id),
            )
            .await?;
            if affected == 0 {
                return Err(AppError::biz(
                    code::VERSION_CONFLICT,
                    "concurrent modification detected",
                ));
            }
        }

        if replace_members {
            DeliveryGroupRepo::soft_delete_members_by_group(&mut *conn, group_id, now).await?;
            for cid in &new_member_ids {
                let m = DeliveryGroupMember {
                    id: snowflake.next_id(),
                    group_id,
                    customer_id: *cid,
                    created_at: now,
                    created_by: Some(current.id),
                    deleted_at: None,
                };
                DeliveryGroupRepo::insert_member(&mut *conn, &m).await?;
            }
        }

        let updated = DeliveryGroupRepo::get_by_id(&mut *conn, group_id, true)
            .await?
            .ok_or_else(|| group_not_found(group_id))?;
        let raw_members =
            DeliveryGroupRepo::list_members_by_group_ids(&mut *conn, &[group_id], false).await?;
        let mut members: Vec<super::dto::DeliveryGroupMemberOut> = Vec::with_capacity(raw_members.len());
        for m in raw_members.into_iter().filter(|m| m.group_id == group_id) {
            let name = l1_children_lookup(conn, m.customer_id)
                .await
                .unwrap_or_else(|_| "(已删除)".into());
            members.push(super::dto::DeliveryGroupMemberOut {
                customer_id: m.customer_id,
                customer_name: name,
            });
        }
        Ok(Self::assemble_group_out(&updated, members))
    }

    pub async fn soft_delete(
        conn: &mut PgConnection,
        group_id: i64,
        req: super::dto::DeliveryGroupIdRequest,
        current: &CurrentUser,
    ) -> Result<(), AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk])?;

        let group = DeliveryGroupRepo::get_by_id(&mut *conn, group_id, false)
            .await?
            .ok_or_else(|| group_not_found(group_id))?;

        if group.version != req.version {
            return Err(version_conflict(group_id, group.version, req.version));
        }

        let now = now_naive();
        let affected =
            DeliveryGroupRepo::soft_delete(&mut *conn, group_id, req.version, now, Some(current.id))
                .await?;
        if affected == 0 {
            return Err(AppError::biz(
                code::VERSION_CONFLICT,
                "concurrent modification detected",
            ));
        }
        DeliveryGroupRepo::soft_delete_members_by_group(&mut *conn, group_id, now).await?;
        Ok(())
    }

    fn assemble_group_out(
        g: &DeliveryGroup,
        members: Vec<super::dto::DeliveryGroupMemberOut>,
    ) -> super::dto::DeliveryGroupOut {
        super::dto::DeliveryGroupOut {
            id: g.id,
            customer_id: g.customer_id,
            name: g.name.clone(),
            members,
            version: g.version,
            created_at: g.created_at,
            updated_at: g.updated_at,
        }
    }
}

// ---------------------------------------------------------------------------
//  DeliveryNoteService  (P2)
// ---------------------------------------------------------------------------

/// 司机工种常量（与 `t_work_type.code` 严格一致）
const WORK_TYPE_DRIVER_CODE: &str = "送货司机";

/// P2 业务状态常量（与 DB 列 `status` 一致；与 `DeliveryNoteStatus::as_str()` 对齐）
const STATUS_DRAFT: &str = "DRAFT";
const STATUS_SUBMITTED: &str = "SUBMITTED";
const STATUS_PICKED_UP: &str = "PICKED_UP";

/// P2 业务批次状态常量
const STATUS_INSPECTION: &str = "INSPECTION";
const STATUS_READY_TO_SHIP: &str = "READY_TO_SHIP";

/// 候选取批上限（与 Python `list_batches_with_part(limit=2000)` 对齐）
const CANDIDATE_LIMIT: i64 = 2000;

pub struct DeliveryNoteService;

impl DeliveryNoteService {
    // ---------- list ----------

    #[allow(clippy::too_many_arguments)]
    pub async fn list_with_filters(
        conn: &mut PgConnection,
        statuses: &[&str],
        customer_id: Option<i64>,
        keyword: Option<&str>,
        sort_by: DeliveryNoteSortKey,
        sort_dir: SortDir,
        limit: i64,
        offset: i64,
        current: &CurrentUser,
    ) -> Result<DeliveryNoteListOut, AppError> {
        current.require_any_role(&[
            Role::Manager,
            Role::Clerk,
            Role::Inspector,
            Role::CncProgrammer,
        ])?;

        let rows =
            DeliveryNoteRepo::list_with_filters(&mut *conn, statuses, customer_id, keyword, sort_by, sort_dir, limit, offset)
                .await?;
        let total =
            DeliveryNoteRepo::count_with_filters(&mut *conn, statuses, customer_id, keyword)
                .await?;

        let items = build_note_outs(conn, &rows).await?;
        Ok(DeliveryNoteListOut {
            items,
            total,
            limit,
            offset,
        })
    }

    pub async fn list_for_pickup(
        conn: &mut PgConnection,
        customer_id: Option<i64>,
        current: &CurrentUser,
    ) -> Result<Vec<DeliveryNoteOut>, AppError> {
        // 司机扫码台用：任意已登录账号 + service 层校验 driver work_type
        // 这里不做角色硬限；具体 worker 校验在 pickup/pickup_scan 里。
        let _ = current;

        let rows = DeliveryNoteRepo::list_for_pickup(&mut *conn, customer_id).await?;
        build_note_outs(conn, &rows).await
    }

    // ---------- create_draft ----------

    pub async fn create_draft(
        conn: &mut PgConnection,
        snowflake: &SnowflakeIdGenerator,
        req: DeliveryNoteCreateRequest,
        current: &CurrentUser,
    ) -> Result<DeliveryNoteDetailOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk, Role::Inspector])?;

        // 1. 校验 L1 存在且是 L1（parent_id IS NULL）
        let l1 = CustomerRepo::get_by_id(&mut *conn, req.customer_id, false)
            .await?
            .ok_or_else(|| customer_not_found(req.customer_id))?;
        if l1.parent_id.is_some() {
            return Err(AppError::biz(
                code::BIZ_DELIVERY_NOTE_PARTS_MULTIPLE_CUSTOMERS,
                format!(
                    "customer {l1_id} 不是一级客户（parent_id 必须为 NULL）；送货单必须挂在一级客户下",
                    l1_id = req.customer_id
                ),
            ));
        }

        // 2. 发放单号
        let delivery_note_no = next_delivery_note_no(&mut *conn, req.customer_id).await?;

        // 3. 写入草稿
        let now = now_naive();
        let note = DeliveryNote {
            id: snowflake.next_id(),
            delivery_note_no,
            customer_id: req.customer_id,
            status: STATUS_DRAFT.to_string(),
            submitted_at: None,
            picked_up_at: None,
            submitted_by: None,
            picked_up_by: None,
            driver_worker_id: None,
            note: req.note,
            delivery_date: Some(req.delivery_date.unwrap_or_else(|| now.date())),
            version: 0,
            created_at: now,
            created_by: Some(current.id),
            updated_at: now,
            updated_by: Some(current.id),
            deleted_at: None,
            delivery_group_id: None,
            leaf_customer_id: None,
        };
        DeliveryNoteRepo::create(&mut *conn, &note).await?;

        // 4. CREATED 事件
        write_event(
            conn,
            snowflake,
            note.id,
            DeliveryNoteEventType::Created,
            None,
            Some(STATUS_DRAFT.to_string()),
            Some(format!("create draft for customer {}", l1.name)),
            Some(current.id),
        )
        .await?;

        // 5. 原子带入首批零件（如果给了 items）
        if !req.items.is_empty() {
            add_parts_inner(conn, snowflake, note.id, &req.items, note.version, current).await?;
        }

        get_with_parts(conn, note.id).await
    }

    // ---------- get_with_parts ----------

    pub async fn get_with_parts(
        conn: &mut PgConnection,
        note_id: i64,
    ) -> Result<DeliveryNoteDetailOut, AppError> {
        get_with_parts(conn, note_id).await
    }

    // ---------- update (partial) ----------

    pub async fn update(
        conn: &mut PgConnection,
        snowflake: &SnowflakeIdGenerator,
        note_id: i64,
        req: DeliveryNoteUpdateRequest,
        current: &CurrentUser,
    ) -> Result<DeliveryNoteOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk, Role::Inspector])?;

        let mut obj = DeliveryNoteRepo::get_by_id(&mut *conn, note_id, false)
            .await?
            .ok_or_else(|| note_not_found(note_id))?;

        if obj.version != req.version {
            return Err(note_version_conflict(note_id, obj.version, req.version));
        }
        if obj.status != STATUS_DRAFT && obj.status != STATUS_SUBMITTED {
            return Err(AppError::biz(
                code::BIZ_DELIVERY_NOTE_INVALID_TRANSITION,
                format!(
                    "cannot update {st} note; only DRAFT/SUBMITTED is editable",
                    st = obj.status
                ),
            ));
        }

        let now = now_naive();
        let mut changed = false;
        if let Some(d) = req.delivery_date
            && Some(d) != obj.delivery_date
        {
            obj.delivery_date = Some(d);
            changed = true;
        }
        if let Some(ref n) = req.note {
            // trim 后空字符串存 NULL，否则存 trim 结果
            let next: Option<String> = if n.trim().is_empty() {
                None
            } else {
                Some(n.trim().to_string())
            };
            if next != obj.note {
                obj.note = next;
                changed = true;
            }
        }
        if changed {
            obj.version += 1;
            obj.updated_at = now;
            obj.updated_by = Some(current.id);
            let affected = DeliveryNoteRepo::update(&mut *conn, &obj).await?;
            if affected == 0 {
                return Err(AppError::biz(
                    code::VERSION_CONFLICT,
                    "concurrent modification detected",
                ));
            }
            // 立即 reload 让 updated_at 拿到 server 值
            obj = DeliveryNoteRepo::get_by_id(&mut *conn, note_id, false)
                .await?
                .ok_or_else(|| note_not_found(note_id))?;
        }
        let _ = snowflake;
        let out = build_note_outs(conn, std::slice::from_ref(&obj)).await?;
        Ok(out.into_iter().next().unwrap())
    }

    // ---------- add_parts ----------

    pub async fn add_parts(
        conn: &mut PgConnection,
        snowflake: &SnowflakeIdGenerator,
        note_id: i64,
        items: &[DeliveryNoteAddItem],
        version: i32,
        current: &CurrentUser,
    ) -> Result<DeliveryNoteDetailOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk, Role::Inspector])?;
        add_parts_inner(conn, snowflake, note_id, items, version, current).await?;
        get_with_parts(conn, note_id).await
    }

    // ---------- remove_parts ----------

    pub async fn remove_parts(
        conn: &mut PgConnection,
        note_id: i64,
        batch_ids: &[i64],
        version: i32,
        current: &CurrentUser,
    ) -> Result<DeliveryNoteDetailOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk, Role::Inspector])?;

        let obj = DeliveryNoteRepo::get_by_id(&mut *conn, note_id, false)
            .await?
            .ok_or_else(|| note_not_found(note_id))?;
        if obj.version != version {
            return Err(note_version_conflict(note_id, obj.version, version));
        }
        if obj.status != STATUS_DRAFT {
            return Err(AppError::biz(
                code::BIZ_DELIVERY_NOTE_PARTS_LOCKED,
                format!(
                    "送货单已提交（{}），不能移除零件；如需调整请先撤回。",
                    obj.status
                ),
            ));
        }

        if batch_ids.is_empty() {
            return get_with_parts(conn, note_id).await;
        }

        let now = now_naive();
        // 清空确实属于本单的 batch.delivery_note_id（version 校验 + 仅限本单）
        for bid in batch_ids {
            let _ = sqlx::query!(
                r#"
                UPDATE t_part_batch
                SET delivery_note_id = NULL,
                    version          = version + 1,
                    updated_at       = $2,
                    updated_by       = $3
                WHERE id = $1 AND delivery_note_id = $4 AND deleted_at IS NULL
                "#,
                bid,
                now,
                Some(current.id),
                note_id,
            )
            .execute(&mut *conn)
            .await?;
        }

        get_with_parts(conn, note_id).await
    }

    // ---------- submit ----------

    pub async fn submit(
        conn: &mut PgConnection,
        snowflake: &SnowflakeIdGenerator,
        note_id: i64,
        version: i32,
        current: &CurrentUser,
    ) -> Result<DeliveryNoteOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk, Role::Inspector])?;

        let mut obj = DeliveryNoteRepo::get_by_id(&mut *conn, note_id, false)
            .await?
            .ok_or_else(|| note_not_found(note_id))?;
        if obj.version != version {
            return Err(note_version_conflict(note_id, obj.version, version));
        }
        if obj.status != STATUS_DRAFT {
            return Err(AppError::biz(
                code::BIZ_DELIVERY_NOTE_INVALID_TRANSITION,
                format!("only DRAFT can be submitted, current={}", obj.status),
            ));
        }

        // 所有批次必须 READY_TO_SHIP
        let batches = PartBatchRepo::list_by_delivery_note(&mut *conn, note_id).await?;
        if batches.is_empty() {
            return Err(AppError::biz(
                code::BIZ_DELIVERY_NOTE_INVALID_VALUE,
                "empty delivery note; add parts before submit",
            ));
        }
        for b in &batches {
            if b.status != STATUS_READY_TO_SHIP {
                return Err(AppError::biz(
                    code::BIZ_DELIVERY_NOTE_PART_NOT_READY,
                    format!(
                        "batch {} status={} (must be READY_TO_SHIP at submit)",
                        b.batch_no, b.status
                    ),
                ));
            }
        }

        // 状态机：DRAFT → SUBMITTED
        let now = now_naive();
        obj.status = STATUS_SUBMITTED.to_string();
        obj.submitted_at = Some(now);
        obj.submitted_by = Some(current.id);
        obj.version += 1;
        obj.updated_at = now;
        obj.updated_by = Some(current.id);

        write_event(
            conn,
            snowflake,
            note_id,
            DeliveryNoteEventType::Submitted,
            Some(STATUS_DRAFT.to_string()),
            Some(STATUS_SUBMITTED.to_string()),
            None,
            Some(current.id),
        )
        .await?;

        let affected = DeliveryNoteRepo::update(&mut *conn, &obj).await?;
        if affected == 0 {
            return Err(AppError::biz(
                code::VERSION_CONFLICT,
                "concurrent modification detected",
            ));
        }
        obj = DeliveryNoteRepo::get_by_id(&mut *conn, note_id, false)
            .await?
            .ok_or_else(|| note_not_found(note_id))?;
        let out = build_note_outs(conn, std::slice::from_ref(&obj)).await?;
        Ok(out.into_iter().next().unwrap())
    }

    // ---------- recall ----------

    pub async fn recall(
        conn: &mut PgConnection,
        snowflake: &SnowflakeIdGenerator,
        note_id: i64,
        version: i32,
        current: &CurrentUser,
    ) -> Result<DeliveryNoteOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk, Role::Inspector])?;

        let mut obj = DeliveryNoteRepo::get_by_id(&mut *conn, note_id, false)
            .await?
            .ok_or_else(|| note_not_found(note_id))?;
        if obj.version != version {
            return Err(note_version_conflict(note_id, obj.version, version));
        }
        if obj.status != STATUS_SUBMITTED {
            return Err(AppError::biz(
                code::BIZ_DELIVERY_NOTE_NOT_SUBMITTED,
                format!("only SUBMITTED can be recalled, current={}", obj.status),
            ));
        }

        // 同范围 DRAFT 撞唯一（设计 §3.3 / 21419）：如果存在另一张同范围的活跃 DRAFT 则拒
        let scope = scope_from_note(&obj);
        if let Some(_other) =
            DeliveryNoteRepo::find_open_draft_by_scope(&mut *conn, obj.customer_id, scope, Some(note_id))
                .await?
        {
            return Err(AppError::biz(
                code::BIZ_DELIVERY_NOTE_DRAFT_SCOPE_CONFLICT,
                "同范围已存在 DRAFT 草稿；请先处理现有草稿再 recall",
            ));
        }

        let now = now_naive();
        obj.status = STATUS_DRAFT.to_string();
        obj.submitted_at = None;
        obj.submitted_by = None;
        obj.version += 1;
        obj.updated_at = now;
        obj.updated_by = Some(current.id);

        write_event(
            conn,
            snowflake,
            note_id,
            DeliveryNoteEventType::Withdrawn,
            Some(STATUS_SUBMITTED.to_string()),
            Some(STATUS_DRAFT.to_string()),
            None,
            Some(current.id),
        )
        .await?;

        let affected = DeliveryNoteRepo::update(&mut *conn, &obj).await?;
        if affected == 0 {
            return Err(AppError::biz(
                code::VERSION_CONFLICT,
                "concurrent modification detected",
            ));
        }
        obj = DeliveryNoteRepo::get_by_id(&mut *conn, note_id, false)
            .await?
            .ok_or_else(|| note_not_found(note_id))?;
        let out = build_note_outs(conn, std::slice::from_ref(&obj)).await?;
        Ok(out.into_iter().next().unwrap())
    }

    // ---------- pickup_scan ----------

    pub async fn pickup_scan(
        conn: &mut PgConnection,
        note_id: i64,
        part_serial: &str,
        _badge_code: Option<&str>,
        current: &CurrentUser,
    ) -> Result<DeliveryNotePickupScanOut, AppError> {
        let _ = current;

        let obj = DeliveryNoteRepo::get_by_id(&mut *conn, note_id, false)
            .await?
            .ok_or_else(|| note_not_found(note_id))?;
        if obj.status != STATUS_SUBMITTED {
            return Err(AppError::biz(
                code::BIZ_DELIVERY_NOTE_NOT_SUBMITTED,
                format!("only SUBMITTED can be scanned, current={}", obj.status),
            ));
        }

        let part = PartRepo::get_by_serial(&mut *conn, part_serial, false).await?;
        let note_batches = PartBatchRepo::list_by_delivery_note(&mut *conn, note_id).await?;
        if part.is_none() || !note_batches.iter().any(|b| Some(b.part_id) == part.as_ref().map(|p| p.id)) {
            return Err(AppError::biz(
                code::BIZ_DELIVERY_NOTE_SCAN_MISMATCH,
                format!("serial {part_serial:?} is not in this delivery note"),
            ));
        }

        Ok(DeliveryNotePickupScanOut {
            delivery_note_id: note_id,
            scanned_count: 0,
            expected_count: note_batches.len() as i64,
            ready: false,
            scanned_serials: vec![],
        })
    }

    // ---------- pickup ----------

    pub async fn pickup(
        conn: &mut PgConnection,
        snowflake: &SnowflakeIdGenerator,
        note_id: i64,
        driver_worker_id: i64,
        version: i32,
        _badge_code: Option<&str>,
        current: &CurrentUser,
    ) -> Result<DeliveryNoteOut, AppError> {
        // 任意已登录账号即可（service 层校验司机）
        let _ = current;

        let mut obj = DeliveryNoteRepo::get_by_id(&mut *conn, note_id, false)
            .await?
            .ok_or_else(|| note_not_found(note_id))?;
        if obj.version != version {
            return Err(note_version_conflict(note_id, obj.version, version));
        }
        if obj.status != STATUS_SUBMITTED {
            return Err(AppError::biz(
                code::BIZ_DELIVERY_NOTE_NOT_SUBMITTED,
                format!("only SUBMITTED can be picked up, current={}", obj.status),
            ));
        }

        // 司机校验
        let driver = WorkerRepo::get_by_id(&mut *conn, driver_worker_id, false)
            .await?
            .ok_or_else(|| AppError::biz(
                code::BIZ_DELIVERY_NOTE_DRIVER_INVALID,
                format!("driver worker {driver_worker_id} not found or inactive"),
            ))?;
        if !driver.is_active {
            return Err(AppError::biz(
                code::BIZ_DELIVERY_NOTE_DRIVER_INVALID,
                format!("driver worker {driver_worker_id} not active"),
            ));
        }
        if let Some(wt_id) = driver.work_type_id {
            let wt = WorkTypeRepo::get_by_id(&mut *conn, wt_id, false)
                .await?
                .ok_or_else(|| AppError::biz(
                    code::BIZ_DELIVERY_NOTE_DRIVER_INVALID,
                    "driver work_type not found",
                ))?;
            if wt.code != WORK_TYPE_DRIVER_CODE {
                return Err(AppError::biz(
                    code::BIZ_DELIVERY_NOTE_DRIVER_INVALID,
                    format!(
                        "driver work_type {} != {WORK_TYPE_DRIVER_CODE:?}",
                        wt.code
                    ),
                ));
            }
        } else {
            return Err(AppError::biz(
                code::BIZ_DELIVERY_NOTE_DRIVER_INVALID,
                "driver has no work_type",
            ));
        }

        // 校验所有批次 READY_TO_SHIP + 非空
        let mut note_batches = PartBatchRepo::list_by_delivery_note(&mut *conn, note_id).await?;
        if note_batches.is_empty() {
            return Err(AppError::biz(
                code::BIZ_DELIVERY_NOTE_INVALID_VALUE,
                "empty delivery note; cannot pick up",
            ));
        }
        for b in &note_batches {
            if b.status != STATUS_READY_TO_SHIP {
                return Err(AppError::biz(
                    code::BIZ_DELIVERY_NOTE_PART_NOT_READY,
                    format!(
                        "batch {} status={}, must be READY_TO_SHIP at pickup",
                        b.batch_no, b.status
                    ),
                ));
            }
        }

        let now = now_naive();

        // 把每个批次的 status 推到 DELIVERED，清 holder/location，version++
        for b in &mut note_batches {
            b.status = "DELIVERED".to_string();
            b.current_holder_id = None;
            b.location = None;
            b.version += 1;
            b.updated_at = now;
            b.updated_by = Some(current.id);
            let affected = PartBatchRepo::update(
                &mut *conn,
                b.id,
                b.version - 1, // expected_version 是之前的
                b.delivery_note_id, // 保留 delivery_note_id（PICKED_UP/ARCHIVED 后仍可打印）
                Some("DELIVERED"),
                now,
                Some(current.id),
            )
            .await?;
            if affected == 0 {
                return Err(AppError::biz(
                    code::VERSION_CONFLICT,
                    "concurrent modification detected",
                ));
            }
        }

        // 状态机：SUBMITTED → PICKED_UP
        obj.status = STATUS_PICKED_UP.to_string();
        obj.picked_up_at = Some(now);
        obj.picked_up_by = Some(current.id);
        obj.driver_worker_id = Some(driver_worker_id);
        obj.version += 1;
        obj.updated_at = now;
        obj.updated_by = Some(current.id);

        write_event(
            conn,
            snowflake,
            note_id,
            DeliveryNoteEventType::PickedUp,
            Some(STATUS_SUBMITTED.to_string()),
            Some(STATUS_PICKED_UP.to_string()),
            None,
            Some(current.id),
        )
        .await?;

        let affected = DeliveryNoteRepo::update(&mut *conn, &obj).await?;
        if affected == 0 {
            return Err(AppError::biz(
                code::VERSION_CONFLICT,
                "concurrent modification detected",
            ));
        }
        obj = DeliveryNoteRepo::get_by_id(&mut *conn, note_id, false)
            .await?
            .ok_or_else(|| note_not_found(note_id))?;
        let out = build_note_outs(conn, std::slice::from_ref(&obj)).await?;
        Ok(out.into_iter().next().unwrap())
    }

    // ---------- soft_delete ----------

    pub async fn soft_delete(
        conn: &mut PgConnection,
        note_id: i64,
        version: i32,
        current: &CurrentUser,
    ) -> Result<(), AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk, Role::Inspector])?;

        let obj = DeliveryNoteRepo::get_by_id(&mut *conn, note_id, false)
            .await?
            .ok_or_else(|| note_not_found(note_id))?;
        if obj.version != version {
            return Err(note_version_conflict(note_id, obj.version, version));
        }
        if obj.status != STATUS_DRAFT {
            return Err(AppError::biz(
                code::BIZ_DELIVERY_NOTE_NOT_DRAFT,
                "only DRAFT delivery notes can be soft-deleted",
            ));
        }

        // 清批次 delivery_note_id
        let _ = sqlx::query!(
            r#"
            UPDATE t_part_batch
            SET delivery_note_id = NULL,
                version          = version + 1,
                updated_at       = $2,
                updated_by       = $3
            WHERE delivery_note_id = $1 AND deleted_at IS NULL
            "#,
            note_id,
            now_naive(),
            Some(current.id),
        )
        .execute(&mut *conn)
        .await?;

        let affected =
            DeliveryNoteRepo::soft_delete(&mut *conn, note_id, obj.version, now_naive(), Some(current.id))
                .await?;
        if affected == 0 {
            return Err(AppError::biz(
                code::VERSION_CONFLICT,
                "concurrent modification detected",
            ));
        }
        Ok(())
    }

    // ---------- list_events ----------

    pub async fn list_events(
        conn: &mut PgConnection,
        note_id: i64,
    ) -> Result<Vec<DeliveryNoteEventOut>, AppError> {
        // 任何已登录账号可看；service 不做角色硬限（与 Python 一致）
        let events = DeliveryNoteEventRepo::list_by_note(&mut *conn, note_id).await?;
        Ok(events
            .into_iter()
            .map(|e| DeliveryNoteEventOut {
                id: e.id,
                delivery_note_id: e.delivery_note_id,
                event_type: e.event_type,
                from_status: e.from_status,
                to_status: e.to_status,
                note: e.note,
                created_by: e.created_by,
                created_at: Some(e.created_at),
            })
            .collect())
    }

    // ---------- list_candidate_parts ----------

    pub async fn list_candidate_parts(
        conn: &mut PgConnection,
        customer_id: i64,
        current: &CurrentUser,
    ) -> Result<Vec<DeliveryNoteCandidatePart>, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk, Role::Inspector])?;

        let cust = CustomerRepo::get_by_id(&mut *conn, customer_id, false)
            .await?
            .ok_or_else(|| customer_not_found(customer_id))?;
        if cust.parent_id.is_some() {
            return Err(AppError::biz(
                code::BIZ_INVALID_VALUE,
                format!("customer {customer_id} 不是一级客户；candidate-parts 必须传一级客户"),
            ));
        }

        // L1 根下所有 active 子客户 (L2) + L1 自身
        let children = CustomerRepo::list_children(&mut *conn, customer_id, false).await?;
        let mut customer_ids: Vec<i64> = children.iter().map(|c| c.id).collect();
        customer_ids.push(customer_id);
        let mut name_by_id: HashMap<i64, String> =
            children.iter().map(|c| (c.id, c.name.clone())).collect();
        name_by_id.insert(customer_id, cust.name.clone());
        let root_name = cust.name.clone();

        let statuses = [STATUS_INSPECTION, STATUS_READY_TO_SHIP];
        let rows = PartBatchRepo::list_batches_with_part_in_customers(
            &mut *conn,
            &statuses,
            &customer_ids,
            CANDIDATE_LIMIT,
        )
        .await?;

        // 过滤：在 active 单 (DRAFT/SUBMITTED) 上的批次排除
        let linked_note_ids: HashSet<i64> = rows
            .iter()
            .filter_map(|(b, _)| b.delivery_note_id)
            .collect();
        let active_note_ids: HashSet<i64> = if linked_note_ids.is_empty() {
            HashSet::new()
        } else {
            let notes = DeliveryNoteRepo::list_by_ids(
                &mut *conn,
                &linked_note_ids.iter().copied().collect::<Vec<_>>(),
                false,
            )
            .await?;
            notes
                .into_iter()
                .filter(|n| n.status == STATUS_DRAFT || n.status == STATUS_SUBMITTED)
                .map(|n| n.id)
                .collect()
        };

        let mut result = Vec::with_capacity(rows.len());
        for (b, p) in rows {
            if let Some(dnid) = b.delivery_note_id
                && active_note_ids.contains(&dnid)
            {
                continue;
            }
            let leaf_name = name_by_id.get(&p.customer_id).cloned();
            let path = match (&root_name, &leaf_name) {
                (r, Some(l)) if r != l => Some(format!("{r} / {l}")),
                _ => leaf_name.clone(),
            };
            result.push(DeliveryNoteCandidatePart {
                id: p.id,
                batch_id: b.id,
                batch_no: b.batch_no,
                batch_label: match &p.serial_no {
                    Some(s) => format!("{s}B{:02}", b.batch_no),
                    None => format!("批次{}", b.batch_no),
                },
                serial_no: p.serial_no.clone().unwrap_or_default(),
                drawing_no: p.drawing_no.clone(),
                name: p.name.clone(),
                quantity: b.quantity,
                applicant_name: None, // TPart 当前投影不含该列（part 域实施阶段补）
                status: b.status.clone(),
                planned_delivery_date: None, // 同上
                order_no: None,              // 同上
                customer_name: leaf_name.clone(),
                parent_customer_name: Some(root_name.clone()),
                customer_path: path,
            });
        }
        Ok(result)
    }

    // ---------- scan_add (P3，§5) ----------

    /// 扫码入单（POST /delivery-notes/scan）。
    ///
    /// 流程概要（设计 §5）：
    /// 1. 解析（trim + exact match part→assembly→404）；
    /// 2. 锚点 leaf 客户 + 加载 L1 全部分组 → `classify()`；
    /// 3. find-or-create DRAFT 草稿（覆盖 21419 召回冲突 / 唯一索引并发兜底）；
    /// 4. 逐 target_part 评估批次（已挂本单 / 冲突其它 active 单 / 可入单 / 状态不符）；
    /// 5. 原子性：装配件整套 / 散件无货 / 散件在其它单 上报错；
    /// 6. attach_to_note 写 delivery_note_id（version-checked）；
    /// 7. 重新装载 + 构建响应。
    ///
    /// 事务边界：handler `pool.begin()` → 这里 → handler `commit()`。本方法不 commit。
    #[allow(clippy::too_many_lines)]
    pub async fn scan_add(
        conn: &mut PgConnection,
        snowflake: &SnowflakeIdGenerator,
        code: &str,
        current: &CurrentUser,
    ) -> Result<ScanDeliveryOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk, Role::Inspector])?;

        // ===== Step 1: 解析 =====
        let trimmed = code.trim().to_string();
        let trimmed_len = trimmed.chars().count();
        if trimmed_len == 0 || trimmed_len > 64 {
            return Err(AppError::biz(
                code::BIZ_INVALID_VALUE,
                format!("code length must be 1..=64 chars (trimmed), got {trimmed_len}"),
            ));
        }

        let part_opt = PartRepo::get_by_serial(&mut *conn, &trimmed, false).await?;
        let asm_opt = AssemblyRepo::get_by_serial(&mut *conn, &trimmed, false).await?;
        let kind = resolve_scan_kind(part_opt.as_ref(), asm_opt.as_ref());
        if matches!(kind, ScanKind::Unknown) {
            return Err(AppError::biz(
                code::BIZ_DELIVERY_SCAN_UNKNOWN_CODE,
                format!("scan code '{trimmed}' does not match any part or assembly"),
            ));
        }

        // 装配体 + children + 锚点（part.id 排序，幂等）
        let mut targets: Vec<TPart> = Vec::new();
        let (resolved, anchor_customer_id) = match kind {
            ScanKind::StandalonePart => {
                let p = part_opt.expect("StandalonePart implies Some(part)");
                targets.push(p.clone());
                (
                    ResolvedEntityDto {
                        kind: "PART",
                        id: p.id,
                        serial_no: p.serial_no.clone().unwrap_or_default(),
                        drawing_no: p.drawing_no.clone(),
                        name: p.name.clone(),
                        assembly_id: None,
                        child_count: None,
                    },
                    p.customer_id,
                )
            }
            ScanKind::PartOfAssembly(aid) => {
                let asm = AssemblyRepo::get_by_id(&mut *conn, aid, false)
                    .await?
                    .ok_or_else(|| AppError::biz(
                        code::BIZ_ASSEMBLY_NOT_FOUND,
                        format!("assembly {aid} not found"),
                    ))?;
                let cs = PartRepo::list_children(&mut *conn, aid, false).await?;
                for c in &cs {
                    targets.push(c.clone());
                }
                targets.sort_by_key(|p| p.id);
                let child_count = cs.len();
                let parent_serial = asm.serial_no.clone().unwrap_or_default();
                let _ = parent_serial; // asm serial 用作 ResolvedEntityDto 的 serial_no 字段
                let triggered_part = part_opt.expect("PartOfAssembly implies Some(part)");
                let mut dto = ResolvedEntityDto {
                    kind: "PART",
                    id: triggered_part.id,
                    serial_no: triggered_part.serial_no.clone().unwrap_or_default(),
                    drawing_no: asm.drawing_no.clone(),
                    name: asm.name.clone(),
                    assembly_id: Some(asm.id),
                    child_count: Some(child_count),
                };
                // 暴露触发扫码的子件详情；DTO 的 serial_no 由 client 重新查询更准
                dto.serial_no = triggered_part.serial_no.clone().unwrap_or_default();
                (dto, asm.customer_id)
            }
            ScanKind::Assembly => {
                let a = asm_opt.expect("Assembly implies Some(assembly)");
                let cs = PartRepo::list_children(&mut *conn, a.id, false).await?;
                for c in &cs {
                    targets.push(c.clone());
                }
                targets.sort_by_key(|p| p.id);
                let child_count = cs.len();
                (
                    ResolvedEntityDto {
                        kind: "ASSEMBLY",
                        id: a.id,
                        serial_no: a.serial_no.clone().unwrap_or_default(),
                        drawing_no: a.drawing_no.clone(),
                        name: a.name.clone(),
                        assembly_id: Some(a.id),
                        child_count: Some(child_count),
                    },
                    a.customer_id,
                )
            }
            ScanKind::Unknown => unreachable!("filtered above"),
        };

        // ===== Step 2: 锚点 + L1 + 分类 =====
        let leaf_cust =
            CustomerRepo::get_by_id(&mut *conn, anchor_customer_id, false)
                .await?
                .ok_or_else(|| AppError::biz(
                    code::BIZ_CUSTOMER_NOT_FOUND,
                    format!("anchor customer {anchor_customer_id} not found"),
                ))?;
        let l1_id = leaf_cust.parent_id.unwrap_or(leaf_cust.id);

        let groups_with_members =
            DeliveryGroupRepo::list_active_groups_with_members_for_l1(&mut *conn, l1_id)
                .await?;
        let groups_for_classify: Vec<GroupWithMemberIds> = groups_with_members
            .iter()
            .map(|(g, m)| GroupWithMemberIds {
                group_id: g.id,
                member_ids: m.clone(),
            })
            .collect();
        let scope = NoteScope::classify(anchor_customer_id, &groups_for_classify);

        // ===== Step 3: find-or-create 草稿 =====
        let note = Self::scan_find_or_create_draft(
            conn,
            snowflake,
            l1_id,
            scope,
            current,
        )
        .await?;

        // ===== Step 4 + 5: 逐 target_part 评估 + 原子性 =====
        // 一次性拿齐 targets 全部活跃批次
        let target_part_ids: Vec<i64> = targets.iter().map(|p| p.id).collect();
        let all_batches: Vec<TPartBatch> = if target_part_ids.is_empty() {
            Vec::new()
        } else {
            PartBatchRepo::list_active_by_part_ids(&mut *conn, &target_part_ids).await?
        };

        // 冲突 note id 集（其它 DRAFT/SUBMITTED 且 != 本 note）
        let other_note_ids: Vec<i64> = all_batches
            .iter()
            .filter_map(|b| b.delivery_note_id)
            .filter(|nid| *nid != note.id)
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        let other_notes: Vec<DeliveryNote> = if other_note_ids.is_empty() {
            Vec::new()
        } else {
            DeliveryNoteRepo::list_by_ids(&mut *conn, &other_note_ids, false).await?
        };
        let mut other_status_by_id: HashMap<i64, String> = HashMap::new();
        for n in &other_notes {
            if n.status == STATUS_DRAFT || n.status == STATUS_SUBMITTED {
                other_status_by_id.insert(n.id, n.status.clone());
            }
        }

        // 按 part_id 分桶
        let mut batches_by_part: HashMap<i64, Vec<TPartBatch>> = HashMap::new();
        for b in all_batches {
            batches_by_part.entry(b.part_id).or_default().push(b);
        }
        for v in batches_by_part.values_mut() {
            v.sort_by_key(|b| b.id);
        }

        let mut added_batches: Vec<ScanBatchDto> = Vec::new();
        let mut already_present: Vec<ScanBatchDto> = Vec::new();
        let mut assembly_failures: Vec<ScanFailureDto> = Vec::new();

        // 整盘批次的 part_id → 默认 first_conflict_no / first_not_ready_status
        let mut first_conflict_no: Option<String> = None;
        let mut first_not_ready_status: Option<String> = None;

        for target in &targets {
            let empty = Vec::new();
            let bs = batches_by_part.get(&target.id).unwrap_or(&empty);
            // 收集该 part 的合格 / 已挂 / 冲突 / 状态不符
            let mut eligible_in_part = 0usize;
            let mut already_in_part = 0usize;
            let mut part_failures: Vec<ScanFailureDto> = Vec::new();
            // 同 part 的可入单 batch id（用于 PART 路径直接入单 / 用于 ASSEMBLY 路径确认
            // part 整体是否 OK）
            let mut eligible_batch_ids_for_part: Vec<i64> = Vec::new();
            let mut present_batch_ids_for_part: Vec<i64> = Vec::new();

            for b in bs {
                let serial = target
                    .serial_no
                    .clone()
                    .unwrap_or_else(|| target.drawing_no.clone());
                let name = target.name.clone();
                let bstatus = b.status.as_str();
                // status 不在候选池 → 视为 not_ready
                if bstatus != STATUS_INSPECTION && bstatus != STATUS_READY_TO_SHIP {
                    if first_not_ready_status.is_none() {
                        first_not_ready_status = Some(b.status.clone());
                    }
                    part_failures.push(ScanFailureDto {
                        serial_no: serial,
                        name,
                        reason: format!("status={}", b.status),
                    });
                    continue;
                }
                match b.delivery_note_id {
                    None => {
                        // 都没挂本单 → eligible
                        eligible_in_part += 1;
                        eligible_batch_ids_for_part.push(b.id);
                    }
                    Some(nid) if nid == note.id => {
                        // 已挂本单 → already_present
                        already_in_part += 1;
                        present_batch_ids_for_part.push(b.id);
                        already_present.push(ScanBatchDto {
                            batch_id: b.id,
                            part_id: b.part_id,
                            serial_no: target
                                .serial_no
                                .clone()
                                .unwrap_or_else(|| target.drawing_no.clone()),
                            quantity: b.quantity,
                        });
                    }
                    Some(other_id) => {
                        // 挂在别的 active 单上 → conflict
                        if other_status_by_id.contains_key(&other_id) {
                            // 取其它单的 no
                            let other_no = other_notes
                                .iter()
                                .find(|n| n.id == other_id)
                                .map(|n| n.delivery_note_no.clone())
                                .unwrap_or_else(|| other_id.to_string());
                            if first_conflict_no.is_none() {
                                first_conflict_no = Some(other_no.clone());
                            }
                            part_failures.push(ScanFailureDto {
                                serial_no: serial,
                                name,
                                reason: format!("on note DN-{other_no}"),
                            });
                        } else {
                            // 挂在 PICKED_UP / ARCHIVED / 软删等：视为「可下架后入单」
                            // —— 与 Python `pickup_scan` 行为一致。
                            eligible_in_part += 1;
                            eligible_batch_ids_for_part.push(b.id);
                        }
                    }
                }
            }

            // 把当前 part 的所有可入单 batch 推到 added_batches（跨 ASSEMBLY / PART 形态统一）。
            // 用 eligible_batch_ids_for_part 而非 status-无关过滤，避免把已记入
            // part_failures 的 not_ready 批次误判为 eligible。
            let target_serial = || {
                target
                    .serial_no
                    .clone()
                    .unwrap_or_else(|| target.drawing_no.clone())
            };
            for b in bs
                .iter()
                .filter(|b| eligible_batch_ids_for_part.contains(&b.id))
            {
                added_batches.push(ScanBatchDto {
                    batch_id: b.id,
                    part_id: b.part_id,
                    serial_no: target_serial(),
                    quantity: b.quantity,
                });
            }

            // ASSEMBLY 额外判定：每个 target 必须有 ≥1 (eligible | already_present) 否则失败
            let was_assembly = matches!(kind, ScanKind::Assembly | ScanKind::PartOfAssembly(_));
            if was_assembly && eligible_in_part == 0 && already_in_part == 0 {
                // 整子件拒绝 → 失败明细并入全局（事务回滚）
                assembly_failures.extend(part_failures);
            }
        }

        // ===== 原子性判定 =====
        if matches!(kind, ScanKind::Assembly | ScanKind::PartOfAssembly(_)) {
            if !assembly_failures.is_empty() {
                let count = assembly_failures.len();
                let summary = assembly_failures
                    .iter()
                    .map(|f| format!("serial={} name={} ({})", f.serial_no, f.name, f.reason))
                    .collect::<Vec<_>>()
                    .join("\n  ");
                let message =
                    format!("装配件整套拒绝：{count} 个子件不可入单。失败明细：\n  {summary}");
                return Err(AppError::BizWithFailures {
                    code: code::BIZ_DELIVERY_ASSEMBLY_PARTS_NOT_READY,
                    message,
                    http: StatusCode::BAD_REQUEST,
                    failures: assembly_failures
                        .into_iter()
                        .map(|f| {
                            serde_json::json!({
                                "serial_no": f.serial_no,
                                "name": f.name,
                                "reason": f.reason,
                            })
                        })
                        .collect(),
                });
            }
        } else if added_batches.is_empty() && already_present.is_empty() {
            // PART: 一个都没入单也没 already
            if let Some(other_no) = first_conflict_no {
                return Err(AppError::biz(
                    code::BIZ_DELIVERY_NOTE_PART_ALREADY_ASSIGNED,
                    format!(
                        "part 批次已挂在送货单 DN-{other_no} 上，无法再次入单"
                    ),
                ));
            }
            if let Some(st) = first_not_ready_status {
                return Err(AppError::biz(
                    code::BIZ_DELIVERY_NOTE_PART_NOT_READY,
                    format!("part 批次状态 {st}，不可入单"),
                ));
            }
            // 兜底：targets 空 + no eligible + no already + no conflict + no not-ready
            //   实际不会发生（除非解析撞出 0 子件装配）；保守返回 21405
            return Err(AppError::biz(
                code::BIZ_DELIVERY_NOTE_PART_NOT_READY,
                "no eligible batches to attach for the scanned code",
            ));
        }

        // 去重 added_batches（同一 batch 不应出现两次，但保险起见）
        added_batches.sort_by_key(|b| b.batch_id);
        added_batches.dedup_by_key(|b| b.batch_id);
        already_present.sort_by_key(|b| b.batch_id);
        already_present.dedup_by_key(|b| b.batch_id);

        // ===== Step 6: Apply =====
        // 重新装载所需 target 的 active batches 拿 version（已被前面 select 过；
        // 装配件流程里新增的 batches 不会被我们先前 select 捕获 —— 但本流程不会新增，
        // 仅 attach，所以 in-memory 的版本仍然有效）
        //
        // 简化：直接按 added_batches.batch_id 在已分桶的 batches_by_part 找版本。
        // 若不可见（极边缘）→ reload 单条再 attach。
        let now = now_naive();
        for target in &added_batches {
            let b = batches_by_part
                .get(&target.part_id)
                .and_then(|v| v.iter().find(|b| b.id == target.batch_id))
                .cloned();
            let version = match b {
                Some(b) => b.version,
                None => {
                    // 边角 reload
                    PartBatchRepo::get_by_id(&mut *conn, target.batch_id, false)
                        .await?
                        .ok_or_else(|| AppError::biz(
                            code::BIZ_PART_BATCH_NOT_FOUND,
                            format!("batch {} not found", target.batch_id),
                        ))?
                        .version
                }
            };
            let affected = PartBatchRepo::attach_to_note(
                &mut *conn,
                target.batch_id,
                version,
                note.id,
                now,
                Some(current.id),
            )
            .await?;
            if affected == 0 {
                return Err(AppError::biz(
                    code::VERSION_CONFLICT,
                    format!("batch {} version conflict during scan attach", target.batch_id),
                ));
            }
        }

        // ===== Step 7: 重新装载 note + 构建响应 =====
        let fresh_note = DeliveryNoteRepo::get_by_id(&mut *conn, note.id, false)
            .await?
            .ok_or_else(|| note_not_found(note.id))?;
        let line_count = PartBatchRepo::list_by_delivery_note(&mut *conn, fresh_note.id)
            .await?
            .len();

        // 重新取一次 L1 客户名（scope_label L1Wide 路径要用）
        let l1_cust_name = CustomerRepo::get_by_id(&mut *conn, fresh_note.customer_id, false)
            .await?
            .map(|c| c.name);

        // scope_label / customer_path 由 scope 列确定（与 note 自身保持一致）
        let (group_name, leaf_name) = match scope {
            NoteScope::Group(gid) => {
                let gname = groups_with_members
                    .iter()
                    .find(|(g, _)| g.id == gid)
                    .map(|(g, _)| g.name.clone());
                (gname, None)
            }
            NoteScope::Leaf(_cid) => (None, Some(leaf_cust.name.clone())),
            NoteScope::L1Wide => (None, l1_cust_name.clone()),
        };
        let scope_label = match scope {
            NoteScope::Group(_) => group_name.clone().unwrap_or_else(|| "(group)".to_string()),
            NoteScope::Leaf(_) => leaf_name.clone().unwrap_or_else(|| "(leaf)".to_string()),
            NoteScope::L1Wide => l1_cust_name.clone().unwrap_or_else(|| "(L1)".to_string()),
        };
        let customer_path = match scope {
            // 设计 §5：customer_path = 「L1 / L2」或「L1」兜底
            NoteScope::Leaf(_) => match (&l1_cust_name, &leaf_name) {
                (Some(l1), Some(l2)) if l1 != l2 => format!("{l1} / {l2}"),
                (_, Some(l2)) => l2.clone(),
                (Some(l1), None) => l1.clone(),
                _ => leaf_cust.name.clone(),
            },
            NoteScope::L1Wide => l1_cust_name.clone().unwrap_or_else(|| leaf_cust.name.clone()),
            NoteScope::Group(_) => l1_cust_name.clone().unwrap_or_else(|| leaf_cust.name.clone()),
        };

        let outcome = if added_batches.is_empty() && !already_present.is_empty() {
            ScanOutcomeDto::AlreadyPresent
        } else {
            ScanOutcomeDto::Added
        };

        // 最近批次（卡片直接展示用）。limit=8 是设计 §5 + 前端约定的常量；
        // 后续如果前端要变多 / 变少，集中改这里。
        const RECENT_ITEMS_LIMIT: i64 = 8;
        let recent_items: Vec<RecentItemDto> =
            PartBatchRepo::list_recent_by_note(&mut *conn, fresh_note.id, RECENT_ITEMS_LIMIT)
                .await?
                .into_iter()
                .map(|r| RecentItemDto {
                    batch_id: r.batch_id,
                    part_id: r.part_id,
                    serial_no: r.serial_no,
                    drawing_no: r.drawing_no,
                    name: r.name,
                    order_no: r.order_no,
                })
                .collect();

        Ok(ScanDeliveryOut {
            outcome,
            resolved,
            note: ScanDeliveryNoteSummaryDto {
                id: fresh_note.id,
                delivery_note_no: fresh_note.delivery_note_no.clone(),
                version: fresh_note.version,
                status: fresh_note.status.clone(),
                scope_label,
                customer_path,
                line_count,
                recent_items,
            },
            added_batches,
            already_present,
            skipped: Vec::new(),
        })
    }

    /// find-or-create DRAFT 草稿（扫码入口专用）。
    ///
    /// 流程：
    /// - 先 `find_open_draft_by_scope`，命中 → 返回；
    /// - 未命中 → `next_delivery_note_no` 发放编号 + 雪花 id + INSERT；
    /// - INSERT 撞唯一索引（23505，仅 Group/Leaf scope，可能）→ 重查一次；
    /// - L1Wide scope 没有唯一索引，所以永不撞（设计 §3.3）。
    async fn scan_find_or_create_draft(
        conn: &mut PgConnection,
        snowflake: &SnowflakeIdGenerator,
        l1_id: i64,
        scope: NoteScope,
        current: &CurrentUser,
    ) -> Result<DeliveryNote, AppError> {
        if let Some(n) =
            DeliveryNoteRepo::find_open_draft_by_scope(&mut *conn, l1_id, scope, None).await?
        {
            return Ok(n);
        }

        let now = now_naive();
        let delivery_note_no = next_delivery_note_no(&mut *conn, l1_id).await?;
        let (dgid, lcid) = match scope {
            NoteScope::L1Wide => (None, None),
            NoteScope::Group(gid) => (Some(gid), None),
            NoteScope::Leaf(cid) => (None, Some(cid)),
        };
        let new_note = DeliveryNote {
            id: snowflake.next_id(),
            delivery_note_no: delivery_note_no.clone(),
            customer_id: l1_id,
            status: STATUS_DRAFT.to_string(),
            submitted_at: None,
            picked_up_at: None,
            submitted_by: None,
            picked_up_by: None,
            driver_worker_id: None,
            note: None,
            delivery_date: Some(now.date()),
            version: 0,
            created_at: now,
            created_by: Some(current.id),
            updated_at: now,
            updated_by: Some(current.id),
            deleted_at: None,
            delivery_group_id: dgid,
            leaf_customer_id: lcid,
        };

        match DeliveryNoteRepo::create(&mut *conn, &new_note).await {
            Ok(()) => DeliveryNoteRepo::get_by_id(&mut *conn, new_note.id, false)
                .await?
                .ok_or_else(|| note_not_found(new_note.id)),
            Err(sqlx::Error::Database(db_err))
                if db_err.code().as_deref() == Some("23505") =>
            {
                // 唯一索引撞 → 重查（同 scope 应有另一个 DRAFT 草稿）
                if let Some(n) = DeliveryNoteRepo::find_open_draft_by_scope(
                    &mut *conn,
                    l1_id,
                    scope,
                    None,
                )
                .await?
                {
                    Ok(n)
                } else {
                    // 重查仍未命中，抛 23505 原始
                    Err(AppError::Database(sqlx::Error::Database(db_err)))
                }
            }
            Err(e) => Err(e.into()),
        }
    }

    // -------------------------------------------------------------------
    //  P4：送货单打印（note + labels 复用）
    // -------------------------------------------------------------------

    /// 打印送货单 → xlsx bytes（含 `line_item_ids` 子集支持，labels 路径复用同入口）
    ///
    /// 流程：
    /// 1. 开 tx → `get_with_parts(note_id)` 拿 detail（head + line_items）；
    /// 2. 加载 L1 客户（取 `serial_prefix` → 模板 key）；
    /// 3. 派生 `PrintRequest` → `tokio::task::spawn_blocking` 调同步
    ///    `print::render_note` / `render_labels`（CPU 密集 umya 工作）；
    /// 4. tx.commit()（print 只读 tx，commit 不做事）。
    ///
    /// `custom_order` / `merge_assemblies` / `merge_quantities` / `line_item_ids`
    /// 全在 print.rs 内校验（事务前不进入 DB）。
    ///
    /// 备注：`is_labels` 由 `line_item_ids.is_some()` 推断；label 渲染走
    /// `print::render_labels`，否则 `print::render_note`。
    #[allow(clippy::too_many_arguments)]
    pub async fn print_xlsx(
        pool: &PgPool,
        note_id: i64,
        custom_order: Option<Vec<i64>>,
        merge_assemblies: bool,
        merge_quantities: HashMap<i64, i32>,
        line_item_ids: Option<Vec<i64>>,
        template_dir: &Path,
        current: &CurrentUser,
    ) -> Result<(Vec<u8>, char), AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk, Role::Inspector])?;

        let mut tx = pool.begin().await?;
        let detail = get_with_parts(&mut tx, note_id).await?;

        let l1 = CustomerRepo::get_by_id(&mut *tx, detail.head.customer_id, false)
            .await?
            .ok_or_else(|| customer_not_found(detail.head.customer_id))?;
        let prefix_upper = l1
            .serial_prefix
            .as_deref()
            .and_then(|s| s.chars().next())
            .map(|c| c.to_ascii_uppercase())
            .ok_or_else(|| {
                AppError::biz_with_status(
                    code::BIZ_DELIVERY_TEMPLATE_NOT_CONFIGURED,
                    format!(
                        "customer {} ({}) 缺序列号前缀；请先在客户编辑里填 L1 serial_prefix (A-Z)",
                        l1.id, l1.name
                    ),
                    StatusCode::BAD_REQUEST,
                )
            })?;

        let template_path = print::template_path(prefix_upper, template_dir)?;
        let is_labels = line_item_ids.is_some();
        let req = PrintRequest {
            prefix: prefix_upper,
            template_path,
            note_detail: detail,
            custom_order,
            merge_assemblies,
            merge_quantities,
            line_item_ids,
        };
        let req_for_blocking = req;

        let out = tokio::task::spawn_blocking(move || -> Result<(Vec<u8>, char), AppError> {
            if req_for_blocking.is_labels() {
                print::render_labels(&req_for_blocking)
            } else {
                print::render_note(&req_for_blocking)
            }
        })
        .await
        .map_err(|e| AppError::internal(format!("print spawn_blocking join: {e}")))??;

        let _ = is_labels;
        tx.commit().await?;
        Ok(out)
    }
}

// ===========================================================================
//  private helpers
// ===========================================================================

/// 把 `Vec<DeliveryNote>` 转 `Vec<DeliveryNoteOut>`（批查客户 / 司机 / 范围名）。
async fn build_note_outs(
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
async fn get_with_parts(conn: &mut PgConnection, note_id: i64) -> Result<DeliveryNoteDetailOut, AppError> {
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
async fn check_scope(
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

fn scope_from_note(n: &DeliveryNote) -> NoteScope {
    if let Some(gid) = n.delivery_group_id {
        NoteScope::Group(gid)
    } else if let Some(cid) = n.leaf_customer_id {
        NoteScope::Leaf(cid)
    } else {
        NoteScope::L1Wide
    }
}

/// add_parts 内部实现（被 create_draft 与 add_parts handler 复用）。
async fn add_parts_inner(
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
async fn write_event(
    conn: &mut PgConnection,
    snowflake: &SnowflakeIdGenerator,
    note_id: i64,
    event_type: DeliveryNoteEventType,
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

fn customer_not_found(id: i64) -> AppError {
    AppError::biz(code::BIZ_CUSTOMER_NOT_FOUND, format!("customer {id} not found"))
}

fn note_not_found(id: i64) -> AppError {
    AppError::biz(code::BIZ_DELIVERY_NOTE_NOT_FOUND, format!("delivery note {id} not found"))
}

fn group_not_found(id: i64) -> AppError {
    AppError::biz(code::BIZ_DELIVERY_GROUP_NOT_FOUND, format!("delivery group {id} not found"))
}

fn note_version_conflict(id: i64, have: i32, want: i32) -> AppError {
    AppError::biz(
        code::VERSION_CONFLICT,
        format!("delivery note {id} version conflict: have {have}, request {want}"),
    )
}

fn version_conflict(id: i64, have: i32, want: i32) -> AppError {
    AppError::biz(
        code::VERSION_CONFLICT,
        format!("group {id} version conflict: have {have}, request {want}"),
    )
}

fn validate_group_name(raw: &str) -> Result<String, AppError> {
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

async fn validate_l2_members(
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

async fn l1_children_lookup(conn: &mut PgConnection, l2_id: i64) -> Result<String, AppError> {
    let c = CustomerRepo::get_by_id(&mut *conn, l2_id, true)
        .await?
        .ok_or_else(|| customer_not_found(l2_id))?;
    Ok(c.name)
}

// _TCustomer 用：保留 TCustomer 字段被读到的副作用
#[allow(dead_code)]
fn _ensure_tcustomer_used(_: &TCustomer) {}

// classify 单元测试放在文件末尾，避免 `items after a test module` 警告
// （rust 2018+ 规定 #[cfg(test)] mod 之后只能再放 #[cfg(test)] 项）。

#[cfg(test)]
mod classify_tests {
    use super::*;

    fn g(id: i64, members: &[i64]) -> GroupWithMemberIds {
        GroupWithMemberIds {
            group_id: id,
            member_ids: members.to_vec(),
        }
    }

    #[test]
    fn classify_no_groups_returns_l1wide() {
        assert_eq!(NoteScope::classify(101, &[]), NoteScope::L1Wide);
    }

    #[test]
    fn classify_member_returns_group() {
        let groups = vec![g(10, &[101, 102, 103])];
        assert_eq!(NoteScope::classify(102, &groups), NoteScope::Group(10));
    }

    #[test]
    fn classify_non_member_returns_leaf() {
        let groups = vec![g(10, &[101, 102, 103])];
        assert_eq!(NoteScope::classify(104, &groups), NoteScope::Leaf(104));
    }

    #[test]
    fn classify_with_l1_self_returns_leaf_l1_id() {
        let groups = vec![g(10, &[101, 102, 103])];
        assert_eq!(NoteScope::classify(100, &groups), NoteScope::Leaf(100));
    }
}

#[cfg(test)]
mod scan_resolve_tests {
    use super::*;
    use crate::modules::part::model::TPart;
    use crate::modules::assembly::model::TAssembly;

    /// 构造一个最小化的 TPart 用作 fixture。
    fn make_part(id: i64, assembly_id: Option<i64>) -> TPart {
        TPart {
            id,
            serial_no: Some(format!("F{id:04}")),
            name: format!("Part {id}"),
            drawing_no: format!("D-{id:03}"),
            customer_id: 100 + id,
            assembly_id,
            status: "INSPECTION".to_string(),
            version: 0,
            created_at: chrono::NaiveDate::from_ymd_opt(2026, 8, 22)
                .unwrap()
                .and_hms_opt(0, 0, 0)
                .unwrap(),
            created_by: None,
            updated_at: chrono::NaiveDate::from_ymd_opt(2026, 8, 22)
                .unwrap()
                .and_hms_opt(0, 0, 0)
                .unwrap(),
            updated_by: None,
            deleted_at: None,
            delivery_note_id: None,
        }
    }

    fn make_assembly(id: i64) -> TAssembly {
        TAssembly {
            id,
            drawing_no: format!("A-{id:03}"),
            name: format!("Asm {id}"),
            customer_id: 900 + id,
            status: "ACTIVE".to_string(),
            serial_no: Some(format!("ASMR{id:04}")),
            version: 0,
            created_at: chrono::NaiveDate::from_ymd_opt(2026, 8, 22)
                .unwrap()
                .and_hms_opt(0, 0, 0)
                .unwrap(),
            created_by: None,
            updated_at: chrono::NaiveDate::from_ymd_opt(2026, 8, 22)
                .unwrap()
                .and_hms_opt(0, 0, 0)
                .unwrap(),
            updated_by: None,
            deleted_at: None,
            order_no: None,
        }
    }

    #[test]
    fn scan_resolve_part_no_assembly_returns_part_kind() {
        let p = make_part(1, None);
        assert_eq!(resolve_scan_kind(Some(&p), None), ScanKind::StandalonePart);
    }

    #[test]
    fn scan_resolve_part_with_assembly_returns_assembly_kind() {
        let p = make_part(2, Some(42));
        assert_eq!(
            resolve_scan_kind(Some(&p), None),
            ScanKind::PartOfAssembly(42)
        );
    }

    #[test]
    fn scan_resolve_assembly_serial_returns_assembly_kind() {
        let a = make_assembly(7);
        assert_eq!(resolve_scan_kind(None, Some(&a)), ScanKind::Assembly);
    }

    #[test]
    fn scan_resolve_both_hits_prefers_part() {
        // 两边都中：同 serial 不可能真发生（数据前提），但当输入同时给出时，
        // part 路径优先（设计 §5：t_part.serial_no == code 命中）。
        let p = make_part(3, Some(99));
        let a = make_assembly(99);
        assert_eq!(
            resolve_scan_kind(Some(&p), Some(&a)),
            ScanKind::PartOfAssembly(99)
        );
    }

    #[test]
    fn scan_resolve_unknown_returns_unknown() {
        assert_eq!(resolve_scan_kind(None, None), ScanKind::Unknown);
    }
}
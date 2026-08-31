//! P3：扫码入单（scan_add）+ 解析 helper（resolve_scan_kind）+ NoteScope 投影。
//!
//! 单元测试（`classify_tests` / `scan_resolve_tests` / `classify_5groups_tests` /
//! `outcome_tests`）就地保留在本模块。

use std::collections::HashMap;

use sqlx::PgConnection;

use crate::auth::rbac::{CurrentUser, Role};
use crate::infra::{
    clock::now_naive, serial::next_delivery_note_no, snowflake::SnowflakeIdGenerator,
};
use crate::modules::assembly::model::TAssembly;
use crate::modules::assembly::repo::AssemblyRepo;
use crate::modules::customer::repo::CustomerRepo;
use crate::modules::part::model::TPart;
use crate::modules::part::repo::PartRepo;
use crate::modules::part_batch::model::TPartBatch;
use crate::modules::part_batch::repo::PartBatchRepo;
use crate::shared::error::{code, AppError};

use super::super::dto::{
    AddedBatchDto, AttachableBatchDto, AvailableBatchDto, BatchStatusDto, RecentItemDto,
    ResolvedEntityDto, ResolvedKindDto, ScanDeliveryNoteSummaryDto, ScanDeliveryOut,
    ScanOutcomeDto, UnresolvedTargetDto,
};
use super::super::model::{DeliveryNote, NoteScope};
use super::super::repo::{DeliveryGroupRepo, DeliveryNoteRepo};
use super::inner::{note_not_found, GroupWithMemberIds};

use super::DeliveryNoteService;

const STATUS_DRAFT: &str = "DRAFT";

// ---------------------------------------------------------------------------
//  batch 状态 5 类分组（设计：scan-route-b-fix.md）
// ---------------------------------------------------------------------------

/// A 组：可直接 attach 入单（INSPECTION + READY_TO_SHIP）。
///
/// `pub(super)`：service::scan 与 service::attach 共用一份定义；不要在
/// service/ 之外的代码里直接调用，attach 模块走 `super::scan::is_attachable_state`。
pub(super) fn is_attachable_state(status: &str) -> bool {
    matches!(status, "READY_TO_SHIP" | "INSPECTION")
}

/// B 组：可送检。`IN_PROCESS` 需未被工人持有。
///
/// 「工人持有」以 `location = 'WORKER'` 判定（与 worker_pool / part repo 的
/// 全部查询一致）。**不能用 `current_holder_id`**：该列多态——批次放货架时
/// 存 `t_shelf.id`（`location = 'PRODUCTION_SHELF' / 'INSPECTION_SHELF'`），
/// 只有工人取件时才存 worker id（`location = 'WORKER'`）。
fn is_inspectable_state(b: &TPartBatch) -> bool {
    match b.status.as_str() {
        "PENDING" | "PROGRAMMING" | "REPAIRING" => true,
        "IN_PROCESS" => b.location.as_deref() != Some("WORKER"),
        _ => false,
    }
}

/// C 组：直接报错的非法状态。`IN_PROCESS` 被工人持有（`location = 'WORKER'`）归此类。
fn classify_invalid_state(b: &TPartBatch) -> Option<&'static str> {
    match b.status.as_str() {
        "DELIVERED" => Some("DELIVERED"),
        "OUTSOURCE" => Some("OUTSOURCE"),
        "COMPLETED" => Some("COMPLETED"),
        "CANCELLED" => Some("CANCELLED"),
        "IN_PROCESS" if b.location.as_deref() == Some("WORKER") => Some("IN_PROCESS_HELD_BY_WORKER"),
        _ => None,
    }
}

/// 单 target（part）的 batch 4 类分组结果（A/B/D）。
///
/// A 组：attachable（INSPECTION + READY_TO_SHIP）
/// B 组：inspectable（PENDING/PROGRAMMING/REPAIRING/IN_PROCESS 非工人持有）
/// D 组：conflict（已挂别的 active 单，由 service 层后续判定 21406）
///
/// C 组（DELIVERED/OUTSOURCE/COMPLETED/CANCELLED/IN_PROCESS 工人持有）由前置
/// `has_fully_invalid_target` 静默过滤，不入此 struct。
///
/// `had_invalid` 记录该 target 在 C 组过滤前**是否至少有 1 个 C 组 batch**。
/// 即便过滤后只剩 A/B，该 target 也会强制走弹窗路径（`classify_outcome` 短路），
/// 让前端能看到剩余的合法批次让用户确认（spec：前端必须能看到 C 被过滤的迹象，
/// 用户的语义预期是「即使只看到 A，也应该先确认再 attach」）。
///
/// `delivery_note_id == Some(note.id)` 的 batch 视为「已挂本单」不入任何 Vec，
/// 由调用方按需要去重。
struct TargetEvaluation {
    part: TPart,
    attachable: Vec<TPartBatch>,
    inspectable: Vec<TPartBatch>,
    conflict: Vec<TPartBatch>,
    /// 该 target 在 5 组分类前是否有 C 组被静默过滤。
    /// 即使分类后只剩 A，也会强制走弹窗路径（不让 A 静默自动 attach）。
    had_invalid: bool,
}

/// 5 组分类的 outcome 判定（纯函数，单测覆盖）。
///
/// 输入是从 evaluations 聚合而来的四个布尔量；返回的 ScanOutcomeDto 决定
/// handler 层后续是否 attach，以及响应里 `unresolved_targets` 的形状。
///
/// 关键约束：只要任一 target 原始有 C 组被过滤（C@WORKER / DELIVERED /
/// OUTSOURCE / COMPLETED / CANCELLED），就强制走弹窗路径
/// （CandidatesAvailable / PartialAdded），即使用户最终看不到 C 也要走弹窗。
/// 这是 spec 约定：前端代码依赖 `unresolved_targets` 展示剩余合法批次让
/// 用户确认，不能让 A 在 C 被静默过滤的语义下静默自动 attach。
fn classify_outcome(
    is_assembly: bool,
    any_inspectable: bool,
    all_attachable_empty: bool,
    any_had_invalid_filtered: bool,
) -> ScanOutcomeDto {
    if any_had_invalid_filtered {
        return if is_assembly {
            ScanOutcomeDto::PartialAdded
        } else {
            ScanOutcomeDto::CandidatesAvailable
        };
    }
    match (is_assembly, any_inspectable) {
        (false, true) => ScanOutcomeDto::CandidatesAvailable,
        (true, true) => ScanOutcomeDto::PartialAdded,
        (_, false) => {
            if all_attachable_empty {
                ScanOutcomeDto::AlreadyPresent
            } else {
                ScanOutcomeDto::Added
            }
        }
    }
}

/// 全 conflict 短路判定（保留 21406 硬错误；纯函数）。
///
/// 每个 target 的 conflict 非空、attachable 与 inspectable 都为空 → 用户
/// 期望的批次全被别的 active 单锁死。返回 true 时 caller 应直接
/// `BIZ_DELIVERY_NOTE_PART_ALREADY_ASSIGNED` 报错。
fn is_all_conflict(evaluations: &[TargetEvaluation]) -> bool {
    evaluations
        .iter()
        .all(|e| e.attachable.is_empty() && e.inspectable.is_empty() && !e.conflict.is_empty())
}

/// C 组分布判定（保留 21421 硬错误；纯函数，单测覆盖）。
///
/// 替代原「任一 C → 21421」全-or-无短路：原本工人持有（C）与货架上
/// （A/B）的合法批次同存于一个子零件时，会错误地整单拒绝。改成
/// 「按 part_id 聚合 → 任一 target 全 C 才报错」，且 C 组静默过滤，
/// 让前端弹窗只看到合法 B 组候选。
///
/// 返回 true 当且仅当存在至少一个 `part_id`，其加载到的全部 batch
/// 都落在 `classify_invalid_state` 命中集里。
fn has_fully_invalid_target(batches: &[TPartBatch]) -> bool {
    let mut by_part_total: HashMap<i64, usize> = HashMap::new();
    let mut by_part_invalid: HashMap<i64, usize> = HashMap::new();
    for b in batches {
        *by_part_total.entry(b.part_id).or_insert(0) += 1;
        if classify_invalid_state(b).is_some() {
            *by_part_invalid.entry(b.part_id).or_insert(0) += 1;
        }
    }
    by_part_total
        .iter()
        .any(|(part_id, total)| {
            *total > 0 && by_part_invalid.get(part_id).copied().unwrap_or(0) == *total
        })
}

/// 由 evaluations[i] 构造 `UnresolvedTargetDto`（含 part 元数据 + A/B 组批次）。
fn build_unresolved_target(e: TargetEvaluation) -> UnresolvedTargetDto {
    UnresolvedTargetDto {
        part_id: e.part.id,
        serial_no: e.part.serial_no.clone().unwrap_or_default(),
        drawing_no: e.part.drawing_no.clone(),
        name: e.part.name.clone(),
        available_batches: e.inspectable.into_iter().map(to_available_batch_dto).collect(),
        attachable_batches: e.attachable.into_iter().map(to_attachable_batch_dto).collect(),
    }
}

/// 把 `TPartBatch` 投影为 `AvailableBatchDto` / `AttachableBatchDto`。
///
/// ⚠️ **禁止合并为 generic helper**：`AvailableBatchDto` / `AttachableBatchDto`
/// 当前字段同形，但设计上独立——未来字段分叉（status 派生逻辑、OCC version
/// 来源、扩展字段）时各自演化。强行复用 generic 会导致所有调用点耦合。
///
/// 状态解析失败兜底为 `Pending`（与原 `build_unresolved_target` 行为一致）。
fn to_available_batch_dto(b: TPartBatch) -> AvailableBatchDto {
    AvailableBatchDto {
        batch_id: b.id,
        version: b.version,
        quantity: b.quantity,
        status: BatchStatusDto::from_db(&b.status).unwrap_or(BatchStatusDto::Pending),
    }
}

/// `TPartBatch` → `AttachableBatchDto`（A 组；status 仅有 INSPECTION / READY_TO_SHIP）。
///
/// 状态解析失败兜底为 `Pending`（与原 `build_unresolved_target` 行为一致）。
fn to_attachable_batch_dto(b: TPartBatch) -> AttachableBatchDto {
    AttachableBatchDto {
        batch_id: b.id,
        version: b.version,
        quantity: b.quantity,
        status: BatchStatusDto::from_db(&b.status).unwrap_or(BatchStatusDto::Pending),
    }
}

// ---------------------------------------------------------------------------
//  NoteScope / classify  (设计 §3.2)
// ---------------------------------------------------------------------------

impl NoteScope {
    pub(super) fn classify(leaf_customer_id: i64, groups: &[GroupWithMemberIds]) -> Self {
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
pub(super) enum ScanKind {
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
fn resolve_scan_kind(part: Option<&TPart>, assembly: Option<&TAssembly>) -> ScanKind {
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

impl DeliveryNoteService {
    // ---------- scan_add (P3，§5) ----------

    /// 扫码入单（POST /delivery-notes/scan）。
    ///
    /// 流程概要（设计 §5 + scan-route-b-fix.md）：
    /// 1. 解析（trim + exact match part→assembly→404）；
    /// 2. 锚点 leaf 客户 + 加载 L1 全部分组 → `classify()`；
    /// 3. find-or-create DRAFT 草稿（覆盖 21419 召回冲突 / 唯一索引并发兜底）；
    /// 4. 一次性加载全部 target 的活跃 batch → C 组短路 → 按 part 分桶
    ///    → 每 target 三 Vec 分类（attachable / inspectable / conflict；
    ///    已挂本单的 batch 跳过）；
    /// 5. outcome 判定：先全 conflict 短路（21406）→ 再 `classify_outcome`
    ///    （4 个变体：Added / AlreadyPresent / CandidatesAvailable / PartialAdded）；
    /// 6. attach（仅 Added 走；CandidatesAvailable / PartialAdded 由前端弹窗决定）
    /// 7. 重新装载 note + 按 outcome 构造 `unresolved_targets` → 返回。
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
                        kind: ResolvedKindDto::Part,
                        id: p.id,
                        serial_no: p.serial_no.clone().unwrap_or_default(),
                        drawing_no: p.drawing_no.clone(),
                        name: p.name.clone(),
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
                let _ = child_count; // DTO 精简后不再需要 child_count 字段
                let parent_serial = asm.serial_no.clone().unwrap_or_default();
                let _ = parent_serial; // asm serial 用作 ResolvedEntityDto 的 serial_no 字段
                let triggered_part = part_opt.expect("PartOfAssembly implies Some(part)");
                let mut dto = ResolvedEntityDto {
                    kind: ResolvedKindDto::Part,
                    id: triggered_part.id,
                    serial_no: triggered_part.serial_no.clone().unwrap_or_default(),
                    drawing_no: asm.drawing_no.clone(),
                    name: asm.name.clone(),
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
                let _ = child_count; // 字段精简后 DTO 不再需要
                (
                    ResolvedEntityDto {
                        kind: ResolvedKindDto::Assembly,
                        id: a.id,
                        serial_no: a.serial_no.clone().unwrap_or_default(),
                        drawing_no: a.drawing_no.clone(),
                        name: a.name.clone(),
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

        // ===== Step 4: 加载 target 全部活跃 batch → C 组短路 → 5 组分类 =====
        let target_part_ids: Vec<i64> = targets.iter().map(|p| p.id).collect();
        let all_batches: Vec<TPartBatch> = if target_part_ids.is_empty() {
            Vec::new()
        } else {
            PartBatchRepo::list_active_by_part_ids(&mut *conn, &target_part_ids).await?
        };

        // C 组分布判定：
        // 仅当存在「target 加载到批次但全部为 C 组」时硬错误（21421）。
        // 其它情况：过滤 C 组后继续走 A/B/D/E 分类。详见 helper
        // `has_fully_invalid_target` 与 `c_group_distribution_tests`。
        if has_fully_invalid_target(&all_batches) {
            return Err(AppError::biz(
                code::BIZ_DELIVERY_BATCH_STATE_INVALID,
                "at least one target has all batches in invalid state (DELIVERED/OUTSOURCE/COMPLETED/CANCELLED/IN_PROCESS-held-by-worker)".to_string(),
            ));
        }

        // 过滤 C 组前先按 part_id 记录「是否有 C 组被静默过滤」——
        // 这是 outcome 短路的依据：即便过滤后只剩 A，也要走弹窗路径，
        // 让前端展示剩余合法批次让用户确认（spec：不能让 A 在 C 被静默
        // 过滤的语义下静默自动 attach）。
        let mut had_invalid_by_part: HashMap<i64, bool> = HashMap::new();
        for b in &all_batches {
            if classify_invalid_state(b).is_some() {
                had_invalid_by_part.insert(b.part_id, true);
            }
        }

        // 过滤 C 组后继续走 A/B/D/E 分类（与原 5 组逻辑兼容）
        let all_batches: Vec<TPartBatch> = all_batches
            .into_iter()
            .filter(|b| classify_invalid_state(b).is_none())
            .collect();

        // 按 part_id 分桶（一次扫描）
        let mut batches_by_part: HashMap<i64, Vec<TPartBatch>> = HashMap::new();
        for b in all_batches {
            batches_by_part.entry(b.part_id).or_default().push(b);
        }
        for v in batches_by_part.values_mut() {
            v.sort_by_key(|b| b.id);
        }

        // 5 组分类：每个 target → attachable / inspectable / conflict 三 Vec
        // （已挂本单的 batch 直接跳过，不入任何 Vec）
        let mut evaluations: Vec<TargetEvaluation> = Vec::with_capacity(targets.len());
        for target in &targets {
            let empty = Vec::new();
            let bs = batches_by_part.get(&target.id).unwrap_or(&empty);
            let mut attachable: Vec<TPartBatch> = Vec::new();
            let mut inspectable: Vec<TPartBatch> = Vec::new();
            let mut conflict: Vec<TPartBatch> = Vec::new();
            for b in bs {
                match b.delivery_note_id {
                    Some(other_id) if other_id == note.id => {
                        // 已挂本单 → 跳过（不计入任何分组）
                    }
                    Some(_other_id) => {
                        // 挂在别的单上 → conflict
                        conflict.push(b.clone());
                    }
                    None => {
                        if is_attachable_state(&b.status) {
                            attachable.push(b.clone());
                        } else if is_inspectable_state(b) {
                            inspectable.push(b.clone());
                        }
                        // 其它：status 既非 attachable 也非 inspectable。
                        // 在 C 组短路后剩下的合法状态只有 INSPECTION / READY_TO_SHIP
                        // （已收进 attachable），其它都已被短路掉。这里走的是兜底
                        // —— 一致地把剩余 status 视为不可 attach 也不可 inspect。
                    }
                }
            }
            evaluations.push(TargetEvaluation {
                part: target.clone(),
                attachable,
                inspectable,
                conflict,
                had_invalid: had_invalid_by_part
                    .get(&target.id)
                    .copied()
                    .unwrap_or(false),
            });
        }

        // ===== Step 5: outcome 判定 =====
        let is_assembly = matches!(kind, ScanKind::Assembly | ScanKind::PartOfAssembly(_));
        let any_inspectable = evaluations.iter().any(|e| !e.inspectable.is_empty());
        let all_attachable_empty = evaluations.iter().all(|e| e.attachable.is_empty());
        let any_had_invalid_filtered = evaluations.iter().any(|e| e.had_invalid);

        if is_all_conflict(&evaluations) {
            return Err(AppError::biz(
                code::BIZ_DELIVERY_NOTE_PART_ALREADY_ASSIGNED,
                "all target batches are attached to other active notes".to_string(),
            ));
        }

        let outcome = classify_outcome(
            is_assembly,
            any_inspectable,
            all_attachable_empty,
            any_had_invalid_filtered,
        );

        // ===== Step 6: attach（仅 outcome = Added 全 A 走；CandidatesAvailable /
        // PartialAdded 由前端弹窗勾选 A 组决定是否 attach；本接口仅在「全 A 无 B」
        // 时自动 attach，避免在散件 / 装配件混合场景下替用户做"只过检"的决定）=====
        let mut added_batches: Vec<AddedBatchDto> = Vec::new();
        if matches!(outcome, ScanOutcomeDto::Added) {
            let now = now_naive();
            for e in &evaluations {
                for b in &e.attachable {
                    let affected = PartBatchRepo::attach_to_note(
                        &mut *conn,
                        b.id,
                        b.version,
                        note.id,
                        now,
                        Some(current.id),
                    )
                    .await?;
                    if affected == 0 {
                        return Err(AppError::biz(
                            code::VERSION_CONFLICT,
                            format!("batch {} version conflict during scan attach", b.id),
                        ));
                    }
                    added_batches.push(AddedBatchDto {
                        batch_id: b.id,
                        part_id: b.part_id,
                        serial_no: e.part.serial_no.clone().unwrap_or_default(),
                        quantity: b.quantity,
                    });
                }
            }
            // 同 batch 不会在多个 target 里出现，但保险起见按 batch_id 排序去重。
            added_batches.sort_by_key(|b| b.batch_id);
            added_batches.dedup_by_key(|b| b.batch_id);
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

        // unresolved_targets 按 outcome 分流构建
        let unresolved_targets = match outcome {
            ScanOutcomeDto::CandidatesAvailable => {
                // 散件 + 仅 B 组 → 单元素
                let e = evaluations
                    .into_iter()
                    .next()
                    .expect("CandidatesAvailable 必有 1 个 target");
                Some(vec![build_unresolved_target(e)])
            }
            ScanOutcomeDto::PartialAdded => {
                // 装配件 A+B 混合（或仅 A / 仅 B）→ 保留所有还有未决动作的子件
                // （有 A 让前端弹窗勾选 attach，有 B 让前端送检）。
                // 旧逻辑「仅 B」在「不再自动 attach A」之后会把 A-only 子件的
                // attachable_batches 静默丢弃，必须改为 A 或 B 任一非空即保留。
                Some(
                    evaluations
                        .into_iter()
                        .filter(|e| !e.inspectable.is_empty() || !e.attachable.is_empty())
                        .map(build_unresolved_target)
                        .collect(),
                )
            }
            _ => None,
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
            unresolved_targets,
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
}

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
            applicant_name: format!("Applicant {id}"),
            quantity: 1,
            request_date: chrono::NaiveDate::from_ymd_opt(2026, 8, 22).unwrap(),
            planned_delivery_date: chrono::NaiveDate::from_ymd_opt(2026, 8, 22).unwrap(),
            actual_delivery_date: None,
            customer_id: 100 + id,
            assembly_id,
            status: "INSPECTION".to_string(),
            location: None,
            is_urgent: false,
            current_holder_id: None,
            placed_at: None,
            next_process_id: None,
            order_no: None,
            system_delivery_date: None,
            note: None,
            has_been_repaired: false,
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
            applicant_name: None,
            customer_id: 900 + id,
            request_date: None,
            planned_delivery_date: None,
            actual_delivery_date: None,
            is_urgent: false,
            status: "ACTIVE".to_string(),
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
            serial_no: Some(format!("ASMR{id:04}")),
            quantity: 1,
            unit_price: None,
            total_price: None,
            order_no: None,
            system_delivery_date: None,
            note: None,
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

#[cfg(test)]
mod classify_5groups_tests {
    use super::*;

    fn b(status: &str, holder: Option<i64>, location: Option<&str>) -> TPartBatch {
        TPartBatch {
            id: 0,
            part_id: 1,
            batch_no: 1,
            quantity: 1,
            status: status.to_string(),
            location: location.map(str::to_string),
            current_holder_id: holder,
            next_process_id: None,
            placed_at: None,
            delivery_note_id: None,
            parent_batch_id: None,
            has_been_repaired: false,
            version: 0,
            created_at: chrono::DateTime::from_timestamp(0, 0).unwrap().naive_utc(),
            created_by: None,
            updated_at: chrono::DateTime::from_timestamp(0, 0).unwrap().naive_utc(),
            updated_by: None,
            deleted_at: None,
        }
    }

    #[test]
    fn c_group_delivered_short_circuits() {
        assert_eq!(classify_invalid_state(&b("DELIVERED", None, None)), Some("DELIVERED"));
        assert_eq!(classify_invalid_state(&b("OUTSOURCE", None, None)), Some("OUTSOURCE"));
        assert_eq!(classify_invalid_state(&b("COMPLETED", None, None)), Some("COMPLETED"));
        assert_eq!(classify_invalid_state(&b("CANCELLED", None, None)), Some("CANCELLED"));
    }

    #[test]
    fn c_group_in_process_held_is_invalid() {
        // 工人持有（location='WORKER'）→ C 组
        assert_eq!(
            classify_invalid_state(&b("IN_PROCESS", Some(42), Some("WORKER"))),
            Some("IN_PROCESS_HELD_BY_WORKER")
        );
        // 货架持有（holder = shelf id，location='PRODUCTION_SHELF'）→ 非 C 组（回归：多态 holder 误判）
        assert_eq!(
            classify_invalid_state(&b("IN_PROCESS", Some(42), Some("PRODUCTION_SHELF"))),
            None
        );
        assert_eq!(classify_invalid_state(&b("IN_PROCESS", None, None)), None);
    }

    #[test]
    fn a_group_attachable_states() {
        assert!(is_attachable_state("INSPECTION"));
        assert!(is_attachable_state("READY_TO_SHIP"));
        assert!(!is_attachable_state("PENDING"));
    }

    #[test]
    fn b_group_inspectable_includes_idle_in_process() {
        assert!(is_inspectable_state(&b("PENDING", None, None)));
        assert!(is_inspectable_state(&b("PROGRAMMING", None, None)));
        assert!(is_inspectable_state(&b("REPAIRING", None, None)));
        assert!(is_inspectable_state(&b("IN_PROCESS", None, None)));
        // 货架持有的 IN_PROCESS 也可送检（回归：多态 holder 误判）
        assert!(is_inspectable_state(&b("IN_PROCESS", Some(7), Some("PRODUCTION_SHELF"))));
        // 仅工人持有（location='WORKER'）不可
        assert!(!is_inspectable_state(&b("IN_PROCESS", Some(7), Some("WORKER"))));
    }
}

#[cfg(test)]
mod outcome_tests {
    use super::*;

    fn eval(part_id: i64, attachable: usize, inspectable: usize, conflict: usize) -> TargetEvaluation {
        fn mk(n: usize) -> Vec<TPartBatch> {
            (0..n)
                .map(|i| TPartBatch {
                    id: i as i64,
                    part_id: 1,
                    batch_no: 1,
                    quantity: 1,
                    status: "INSPECTION".to_string(),
                    location: None,
                    current_holder_id: None,
                    next_process_id: None,
                    placed_at: None,
                    delivery_note_id: None,
                    parent_batch_id: None,
                    has_been_repaired: false,
                    version: 0,
                    created_at: chrono::DateTime::from_timestamp(0, 0).unwrap().naive_utc(),
                    created_by: None,
                    updated_at: chrono::DateTime::from_timestamp(0, 0).unwrap().naive_utc(),
                    updated_by: None,
                    deleted_at: None,
                })
                .collect()
        }
        TargetEvaluation {
            part: TPart {
                id: part_id,
                serial_no: Some(format!("F{part_id:04}")),
                name: format!("Part {part_id}"),
                drawing_no: format!("D-{part_id:03}"),
                applicant_name: String::new(),
                quantity: 1,
                request_date: chrono::NaiveDate::from_ymd_opt(2026, 8, 22).unwrap(),
                planned_delivery_date: chrono::NaiveDate::from_ymd_opt(2026, 8, 22).unwrap(),
                actual_delivery_date: None,
                customer_id: 1,
                assembly_id: None,
                status: "INSPECTION".to_string(),
                location: None,
                is_urgent: false,
                current_holder_id: None,
                placed_at: None,
                next_process_id: None,
                order_no: None,
                system_delivery_date: None,
                note: None,
                has_been_repaired: false,
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
            },
            attachable: mk(attachable),
            inspectable: mk(inspectable),
            conflict: mk(conflict),
            had_invalid: false,
        }
    }

    #[test]
    fn outcome_added_when_all_attachable() {
        let evs = vec![eval(1, 1, 0, 0)];
        assert_eq!(classify_outcome(false, false, false, false), ScanOutcomeDto::Added);
        assert!(!is_all_conflict(&evs));
    }

    #[test]
    fn outcome_already_present_when_all_on_note() {
        // attachable 全空 + inspectable 全空 + 无 conflict
        let evs = vec![eval(1, 0, 0, 0)];
        assert_eq!(
            classify_outcome(false, false, true, false),
            ScanOutcomeDto::AlreadyPresent
        );
        assert!(!is_all_conflict(&evs));
    }

    #[test]
    fn outcome_candidates_available_when_standalone_only_inspectable() {
        let evs = vec![eval(1, 0, 1, 0)];
        assert_eq!(
            classify_outcome(false, true, true, false),
            ScanOutcomeDto::CandidatesAvailable
        );
        assert!(!is_all_conflict(&evs));
    }

    #[test]
    fn outcome_partial_added_when_assembly_mixed() {
        // assembly 路径 + A 组 + B 组混合
        let evs = vec![eval(1, 1, 1, 0)];
        assert_eq!(
            classify_outcome(true, true, false, false),
            ScanOutcomeDto::PartialAdded
        );
        assert!(!is_all_conflict(&evs));
    }

    #[test]
    fn outcome_assembly_only_inspectable_still_partial_added() {
        // 装配件全部 B 组 → 仍归 PartialAdded（resolved=Assembly）
        let _evs = [eval(1, 0, 1, 0)];
        assert_eq!(
            classify_outcome(true, true, true, false),
            ScanOutcomeDto::PartialAdded
        );
    }

    #[test]
    fn c_group_short_circuits_before_outcome_judgement() {
        // 全 conflict → 直接报 21406，不进 outcome 判定
        let evs = vec![eval(1, 0, 0, 1), eval(2, 0, 0, 2)];
        assert!(is_all_conflict(&evs));
    }
}

// C 组分布判定单元测试：覆盖「任一 target 全 C 才报 21421」+「C 组
// 静默过滤后只剩 A/B」两条核心语义（与 `classify_5groups_tests` 互为补充）。
#[cfg(test)]
mod c_group_distribution_tests {
    use super::*;

    /// 紧凑 mock：仅暴露本测试关注的字段，其余用 None / 0 / false 占位。
    fn b(id: i64, part_id: i64, status: &str, location: Option<&str>) -> TPartBatch {
        TPartBatch {
            id,
            part_id,
            batch_no: 1,
            quantity: 1,
            status: status.to_string(),
            location: location.map(str::to_string),
            current_holder_id: None,
            next_process_id: None,
            placed_at: None,
            delivery_note_id: None,
            parent_batch_id: None,
            has_been_repaired: false,
            version: 0,
            created_at: chrono::DateTime::from_timestamp(0, 0).unwrap().naive_utc(),
            created_by: None,
            updated_at: chrono::DateTime::from_timestamp(0, 0).unwrap().naive_utc(),
            updated_by: None,
            deleted_at: None,
        }
    }

    #[test]
    fn filter_invalid_state_keeps_attachable_and_inspectable() {
        // 1 个 part：READY_TO_SHIP（A 组）+ PENDING（B 组）+ IN_PROCESS@WORKER（C 组）
        // 过滤 C 组后剩 2 个（A/B）。
        let all = vec![
            b(1, 100, "READY_TO_SHIP", None),
            b(2, 100, "PENDING", None),
            b(3, 100, "IN_PROCESS", Some("WORKER")),
        ];
        let kept: Vec<TPartBatch> = all
            .into_iter()
            .filter(|x| classify_invalid_state(x).is_none())
            .collect();
        assert_eq!(kept.len(), 2);
        assert_eq!(kept[0].id, 1);
        assert_eq!(kept[1].id, 2);
    }

    #[test]
    fn fully_invalid_target_detection_assembly_case() {
        // 装配件：2 个 targets
        //   A (part 100): 4 个 batch，2B + 2C → 部分 C 不是全 C → 不触发
        //   B (part 200): 3 个 batch，全 C → 全 C → 触发
        let all = vec![
            b(1, 100, "PENDING", None),
            b(2, 100, "PENDING", None),
            b(3, 100, "DELIVERED", None),
            b(4, 100, "CANCELLED", None),
            b(5, 200, "DELIVERED", None),
            b(6, 200, "OUTSOURCE", None),
            b(7, 200, "COMPLETED", None),
        ];
        assert!(has_fully_invalid_target(&all));
    }

    #[test]
    fn fully_invalid_target_detection_standalone_case() {
        // 散件：1 个 target（part 100），3 个 batch 全 C → 触发。
        let all = vec![
            b(1, 100, "DELIVERED", None),
            b(2, 100, "CANCELLED", None),
            b(3, 100, "IN_PROCESS", Some("WORKER")),
        ];
        assert!(has_fully_invalid_target(&all));
    }

    #[test]
    fn partial_invalid_not_trigger_21421() {
        // 1 个 target（part 100），4 个 batch，B+B+C+C → 部分 C 不是全 C → 不触发。
        let all = vec![
            b(1, 100, "PENDING", None),
            b(2, 100, "PENDING", None),
            b(3, 100, "DELIVERED", None),
            b(4, 100, "CANCELLED", None),
        ];
        assert!(!has_fully_invalid_target(&all));
    }

    #[test]
    fn no_batches_does_not_trigger_21421() {
        // 未生产 → 不应触发 21421（设计：避免空数据误报硬错误）。
        let all: Vec<TPartBatch> = Vec::new();
        assert!(!has_fully_invalid_target(&all));
    }
}

// attachable_batches 单元测试：覆盖 build_unresolved_target 字段映射 +
// outcome 分流（PartialAdded / CandidatesAvailable / C 组过滤）三种场景。
#[cfg(test)]
mod attachable_batches_tests {
    use super::*;
    use crate::modules::delivery_note::dto::{
        AttachableBatchDto, AvailableBatchDto, BatchStatusDto, UnresolvedTargetDto,
    };
    use crate::modules::part::model::TPart;

    /// 紧凑 mock：仅暴露本测试关注的字段，其余用 None / 0 / false 占位。
    fn b(id: i64, part_id: i64, status: &str, location: Option<&str>, version: i32) -> TPartBatch {
        TPartBatch {
            id,
            part_id,
            batch_no: 1,
            quantity: 10,
            status: status.to_string(),
            location: location.map(str::to_string),
            current_holder_id: None,
            next_process_id: None,
            placed_at: None,
            delivery_note_id: None,
            parent_batch_id: None,
            has_been_repaired: false,
            version,
            created_at: chrono::DateTime::from_timestamp(0, 0).unwrap().naive_utc(),
            created_by: None,
            updated_at: chrono::DateTime::from_timestamp(0, 0).unwrap().naive_utc(),
            updated_by: None,
            deleted_at: None,
        }
    }

    fn part(id: i64, serial: &str) -> TPart {
        TPart {
            id,
            serial_no: Some(serial.to_string()),
            name: format!("Part {id}"),
            drawing_no: format!("D-{id:03}"),
            applicant_name: String::new(),
            quantity: 1,
            request_date: chrono::NaiveDate::from_ymd_opt(2026, 8, 22).unwrap(),
            planned_delivery_date: chrono::NaiveDate::from_ymd_opt(2026, 8, 22).unwrap(),
            actual_delivery_date: None,
            customer_id: 1,
            assembly_id: None,
            status: "INSPECTION".to_string(),
            location: None,
            is_urgent: false,
            current_holder_id: None,
            placed_at: None,
            next_process_id: None,
            order_no: None,
            system_delivery_date: None,
            note: None,
            has_been_repaired: false,
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

    /// 单 target 的 TargetEvaluation 构造助手。
    fn eval_for(
        part: TPart,
        attachable: Vec<TPartBatch>,
        inspectable: Vec<TPartBatch>,
        conflict: Vec<TPartBatch>,
    ) -> TargetEvaluation {
        TargetEvaluation {
            part,
            attachable,
            inspectable,
            conflict,
            had_invalid: false,
        }
    }

    /// 单 target 的 TargetEvaluation 构造助手（含 had_invalid 标记）。
    /// 用于测试 C 组过滤后强制走弹窗路径的 outcome 短路。
    fn eval_for_with_invalid(
        part: TPart,
        attachable: Vec<TPartBatch>,
        inspectable: Vec<TPartBatch>,
        conflict: Vec<TPartBatch>,
        had_invalid: bool,
    ) -> TargetEvaluation {
        TargetEvaluation {
            part,
            attachable,
            inspectable,
            conflict,
            had_invalid,
        }
    }

    #[test]
    fn build_unresolved_target_converts_attachable_to_dto() {
        // 直测 build_unresolved_target 的字段映射：
        // - part 元数据透传
        // - available_batches 来源于 inspectable
        // - attachable_batches 来源于 attachable
        let p = part(100, "SN100");
        let attachable = vec![
            b(1, 100, "INSPECTION", None, 5),
            b(2, 100, "READY_TO_SHIP", None, 7),
        ];
        let inspectable = vec![
            b(3, 100, "PENDING", None, 0),
            b(4, 100, "IN_PROCESS", None, 1),
        ];
        let eval = eval_for(p, attachable, inspectable, Vec::new());
        let out: UnresolvedTargetDto = build_unresolved_target(eval);

        assert_eq!(out.part_id, 100);
        assert_eq!(out.serial_no, "SN100");
        assert_eq!(out.drawing_no, "D-100");
        assert_eq!(out.name, "Part 100");

        // B 组：2 个 inspectable → 2 个 AvailableBatchDto
        assert_eq!(out.available_batches.len(), 2);
        let avail_ids: Vec<i64> = out.available_batches.iter().map(|x| x.batch_id).collect();
        assert_eq!(avail_ids, vec![3, 4]);
        // 状态正确：PENDING → Pending；IN_PROCESS 无 location（不是 WORKER）→ Inspect 状态；
        // 此处 from_db 校验 PENDING/IN_PROCESS 都能映射成对应 DTO
        assert!(matches!(
            out.available_batches[0].status,
            BatchStatusDto::Pending
        ));

        // A 组：2 个 attachable → 2 个 AttachableBatchDto
        assert_eq!(out.attachable_batches.len(), 2);
        let attach_ids: Vec<i64> = out.attachable_batches.iter().map(|x| x.batch_id).collect();
        assert_eq!(attach_ids, vec![1, 2]);
        // version 透传（用于前端 add-parts 转发）
        assert_eq!(out.attachable_batches[0].version, 5);
        assert_eq!(out.attachable_batches[1].version, 7);
        // quantity 透传
        assert_eq!(out.attachable_batches[0].quantity, 10);
        // status 透传
        assert!(matches!(
            out.attachable_batches[0].status,
            BatchStatusDto::Inspection
        ));
        assert!(matches!(
            out.attachable_batches[1].status,
            BatchStatusDto::ReadyToShip
        ));
    }

    #[test]
    fn attachable_batches_populated_when_outcome_partial_added() {
        // 装配件混合：sub-part 1 = [A,A]（attachable=2）、sub-part 2 = [B,B]（inspectable=2）
        // → PartialAdded → unresolved_targets 包含 2 个元素：
        //   sub-part 1 的 attachable_batches 非空，available_batches 空
        //   sub-part 2 的 available_batches 非空，attachable_batches 空
        let p1 = part(100, "SN100");
        let p2 = part(200, "SN200");
        let attachable_p1 = vec![
            b(10, 100, "INSPECTION", None, 1),
            b(11, 100, "READY_TO_SHIP", None, 2),
        ];
        let inspectable_p2 = vec![
            b(20, 200, "PENDING", None, 3),
            b(21, 200, "IN_PROCESS", None, 4),
        ];
        let evals = vec![
            eval_for(p1, attachable_p1, Vec::new(), Vec::new()),
            eval_for(p2, Vec::new(), inspectable_p2, Vec::new()),
        ];

        // Step 5 outcome 判定
        let is_assembly = true;
        let any_inspectable = evals.iter().any(|e| !e.inspectable.is_empty());
        let all_attachable_empty = evals.iter().all(|e| e.attachable.is_empty());
        let any_had_invalid_filtered = evals.iter().any(|e| e.had_invalid);
        let outcome = classify_outcome(
            is_assembly,
            any_inspectable,
            all_attachable_empty,
            any_had_invalid_filtered,
        );
        assert_eq!(outcome, ScanOutcomeDto::PartialAdded);

        // Step 7 unresolved_targets 构造（与生产代码 PartialAdded filter 一致：
        // A 或 B 任一非空的子件都保留，让前端能看到 A 组的 attachable_batches）
        let unresolved: Vec<UnresolvedTargetDto> = evals
            .into_iter()
            .filter(|e| !e.inspectable.is_empty() || !e.attachable.is_empty())
            .map(build_unresolved_target)
            .collect();
        assert_eq!(unresolved.len(), 2);

        // sub-part 100：有 attachable、无 inspectable → 进列表但 attachable_batches 含 2 个 A
        let u0 = &unresolved[0];
        assert_eq!(u0.part_id, 100);
        assert_eq!(u0.attachable_batches.len(), 2);
        assert_eq!(u0.available_batches.len(), 0);
        let a_ids: Vec<i64> = u0.attachable_batches.iter().map(|x| x.batch_id).collect();
        assert_eq!(a_ids, vec![10, 11]);

        // sub-part 200：无 attachable、有 inspectable → 进列表但 available_batches 含 2 个 B
        let u1 = &unresolved[1];
        assert_eq!(u1.part_id, 200);
        assert_eq!(u1.attachable_batches.len(), 0);
        assert_eq!(u1.available_batches.len(), 2);
        let b_ids: Vec<i64> = u1.available_batches.iter().map(|x| x.batch_id).collect();
        assert_eq!(b_ids, vec![20, 21]);
    }

    #[test]
    fn attachable_batches_empty_when_no_attachable() {
        // 散件场景：全 B（inspectable=2，attachable=0）→ CandidatesAvailable →
        // unresolved_targets 单元素，且 attachable_batches 必须为空 Vec
        // （不漏字段、不为 None）。
        let p = part(100, "SN100");
        let inspectable = vec![
            b(1, 100, "PENDING", None, 0),
            b(2, 100, "IN_PROCESS", None, 0),
        ];
        let evals = vec![eval_for(p, Vec::new(), inspectable, Vec::new())];

        let outcome = classify_outcome(false, true, true, false);
        assert_eq!(outcome, ScanOutcomeDto::CandidatesAvailable);

        let unresolved: Vec<UnresolvedTargetDto> = evals
            .into_iter()
            .next()
            .map(|e| vec![build_unresolved_target(e)])
            .unwrap();
        assert_eq!(unresolved.len(), 1);
        assert_eq!(unresolved[0].attachable_batches.len(), 0);
        assert_eq!(unresolved[0].available_batches.len(), 2);
        // 字段存在且为空 Vec（类型断言，编译期保证 Vec<AttachableBatchDto> 不是 Option）
        let _: &Vec<AttachableBatchDto> = &unresolved[0].attachable_batches;
        let _: &Vec<AvailableBatchDto> = &unresolved[0].available_batches;
    }

    #[test]
    fn attachable_batches_filtered_when_c_group_present() {
        // 散件 [A, B, C@WORKER]：
        //   - A 组 (INSPECTION) → 进 attachable
        //   - B 组 (PENDING) → 进 inspectable
        //   - C 组 (IN_PROCESS@WORKER) → 被 C 组短路过滤，不进任何 Vec
        // → CandidatesAvailable，attachable_batches 含 1 个 A，
        // available_batches 含 1 个 B。
        let all: Vec<TPartBatch> = vec![
            b(1, 100, "INSPECTION", None, 0),
            b(2, 100, "PENDING", None, 0),
            b(3, 100, "IN_PROCESS", Some("WORKER"), 0),
        ];

        // C 组过滤（与 scan_add Step 4 一致）
        let filtered: Vec<TPartBatch> = all
            .into_iter()
            .filter(|x| classify_invalid_state(x).is_none())
            .collect();
        assert_eq!(filtered.len(), 2);

        // 按 attachable/inspectable 分桶（与 Step 4 一致）
        let mut attachable = Vec::new();
        let mut inspectable = Vec::new();
        for b in &filtered {
            if is_attachable_state(&b.status) {
                attachable.push(b.clone());
            } else if is_inspectable_state(b) {
                inspectable.push(b.clone());
            }
        }
        assert_eq!(attachable.len(), 1);
        assert_eq!(attachable[0].id, 1);
        assert_eq!(inspectable.len(), 1);
        assert_eq!(inspectable[0].id, 2);

        // outcome：C 组过滤后只剩 B → CandidatesAvailable
        let outcome = classify_outcome(false, !inspectable.is_empty(), attachable.is_empty(), true);
        assert_eq!(outcome, ScanOutcomeDto::CandidatesAvailable);

        // build_unresolved_target：DTO 字段正确分离
        let p = part(100, "SN100");
        let eval = eval_for(p, attachable, inspectable, Vec::new());
        let out = build_unresolved_target(eval);
        assert_eq!(out.attachable_batches.len(), 1);
        assert_eq!(out.attachable_batches[0].batch_id, 1);
        assert_eq!(out.available_batches.len(), 1);
        assert_eq!(out.available_batches[0].batch_id, 2);
        // C 组 batch id=3 不出现在任何 Vec
        for b in &out.attachable_batches {
            assert_ne!(b.batch_id, 3);
        }
        for b in &out.available_batches {
            assert_ne!(b.batch_id, 3);
        }
    }

    // ---- had_invalid 短路 outcome 测试：覆盖 spec 约定的「原始含 C → 强制弹窗」 ----

    /// 散件 [A, C@WORKER] 混合 → outcome 必须是 CandidatesAvailable（即使只剩 A），
    /// A 不自动 attach，进入 attachable_batches 让前端弹窗确认。
    ///
    /// 这是本次 fix 的核心场景：spec 约定 C 被静默过滤后，剩余的合法批次
    /// 也必须走弹窗路径，不能让 A 静默自动 attach。
    #[test]
    fn had_invalid_standalone_a_plus_c_returns_candidates() {
        // 散件：1 个 target，attachable=[A]，inspectable=[]，had_invalid=true
        let p = part(100, "SN100");
        let attachable = vec![b(1, 100, "INSPECTION", None, 0)];
        let eval = eval_for_with_invalid(p, attachable, Vec::new(), Vec::new(), true);

        // outcome：C 被过滤（had_invalid=true）+ 散件 → CandidatesAvailable
        let outcome = classify_outcome(
            false,
            false, // any_inspectable
            false, // all_attachable_empty
            true,  // any_had_invalid_filtered
        );
        assert_eq!(outcome, ScanOutcomeDto::CandidatesAvailable);

        // 验证响应形态：unresolved_targets 单元素 + attachable_batches 含 A
        let unresolved: Vec<UnresolvedTargetDto> =
            vec![build_unresolved_target(eval)];
        assert_eq!(unresolved.len(), 1);
        assert_eq!(unresolved[0].attachable_batches.len(), 1);
        assert_eq!(unresolved[0].available_batches.len(), 0);
        assert_eq!(unresolved[0].attachable_batches[0].batch_id, 1);
    }

    /// 装配件 + 某子件 had_invalid=true → PartialAdded（即便该子件只剩 A）。
    ///
    /// 装配件场景下，C 被过滤后该子件的 A 也必须走弹窗（不能被静默 auto-attach），
    /// 让前端决定 attach 哪些子件。
    #[test]
    fn had_invalid_assembly_returns_partial_added() {
        // 装配件 1 个子件：attachable=[A]，had_invalid=true
        let p = part(100, "SN100");
        let attachable = vec![b(1, 100, "INSPECTION", None, 0)];
        let eval = eval_for_with_invalid(p, attachable, Vec::new(), Vec::new(), true);

        let outcome = classify_outcome(
            true,  // is_assembly
            false, // any_inspectable
            false, // all_attachable_empty
            true,  // any_had_invalid_filtered
        );
        assert_eq!(outcome, ScanOutcomeDto::PartialAdded);

        let unresolved: Vec<UnresolvedTargetDto> = vec![build_unresolved_target(eval)];
        assert_eq!(unresolved.len(), 1);
        assert_eq!(unresolved[0].attachable_batches.len(), 1);
        assert_eq!(unresolved[0].available_batches.len(), 0);
    }

    /// 散件 + 全 A（无 invalid + 无 inspectable）→ Added（既有行为不变）。
    ///
    /// 回归测试：had_invalid=false 时新参数完全不影响既有 outcome 分支。
    #[test]
    fn had_invalid_false_full_a_returns_added() {
        let outcome = classify_outcome(
            false, // is_assembly
            false, // any_inspectable
            false, // all_attachable_empty
            false, // any_had_invalid_filtered
        );
        assert_eq!(outcome, ScanOutcomeDto::Added);
    }

    /// 散件 + invalid + B 同时存在 → CandidatesAvailable（与无 invalid 的
    /// 「全 B 走 CandidatesAvailable」行为一致）。
    ///
    /// 验证 invalid 与 inspectable 共存时短路仍生效（CandidatesAvailable）。
    #[test]
    fn had_invalid_with_inspectable_returns_candidates() {
        let outcome = classify_outcome(
            false, // is_assembly
            true,  // any_inspectable
            false, // all_attachable_empty
            true,  // any_had_invalid_filtered
        );
        assert_eq!(outcome, ScanOutcomeDto::CandidatesAvailable);
    }

    /// 散件 + 仅 B（无 invalid）→ CandidatesAvailable（既有行为不变）。
    ///
    /// 回归测试：had_invalid=false + any_inspectable=true → CandidatesAvailable。
    #[test]
    fn no_invalid_with_inspectable_returns_candidates() {
        let outcome = classify_outcome(
            false, // is_assembly
            true,  // any_inspectable
            false, // all_attachable_empty
            false, // any_had_invalid_filtered
        );
        assert_eq!(outcome, ScanOutcomeDto::CandidatesAvailable);
    }

    /// 装配件 + 仅 B（无 invalid）→ PartialAdded（既有行为不变）。
    ///
    /// 回归测试：had_invalid=false + is_assembly=true + any_inspectable=true → PartialAdded。
    #[test]
    fn no_invalid_assembly_with_inspectable_returns_partial_added() {
        let outcome = classify_outcome(
            true,  // is_assembly
            true,  // any_inspectable
            true,  // all_attachable_empty
            false, // any_had_invalid_filtered
        );
        assert_eq!(outcome, ScanOutcomeDto::PartialAdded);
    }
}

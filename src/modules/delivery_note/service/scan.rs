//! P3：扫码入单（scan_add）+ 解析 helper（resolve_scan_kind）+ NoteScope 投影。
//!
//! 单元测试（`classify_tests` / `scan_resolve_tests`）就地保留在本模块。

use std::collections::{HashMap, HashSet};

use axum::http::StatusCode;
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
    RecentItemDto, ResolvedEntityDto, ScanBatchDto, ScanDeliveryNoteSummaryDto, ScanDeliveryOut,
    ScanFailureDto, ScanOutcomeDto,
};
use super::super::model::{DeliveryNote, NoteScope};
use super::super::repo::{DeliveryGroupRepo, DeliveryNoteRepo};
use super::inner::{note_not_found, GroupWithMemberIds};

use super::DeliveryNoteService;

const STATUS_DRAFT: &str = "DRAFT";
const STATUS_SUBMITTED: &str = "SUBMITTED";
const STATUS_INSPECTION: &str = "INSPECTION";
const STATUS_READY_TO_SHIP: &str = "READY_TO_SHIP";

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
                        part_id: target.id,
                        batch_id: Some(b.id),
                        drawing_no: Some(target.drawing_no.clone()),
                        status: Some(b.status.clone()),
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
                                part_id: target.id,
                                batch_id: Some(b.id),
                                drawing_no: Some(target.drawing_no.clone()),
                                status: Some(b.status.clone()),
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
                                "part_id": f.part_id.to_string(),
                                "batch_id": f.batch_id.map(|n| n.to_string()),
                                "drawing_no": f.drawing_no,
                                "status": f.status,
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

//! P3：扫码入单（scan_add）（2026-09-22 D-5 拆出，原 1617 行超 1000 行上限）
//!
//! ## 子模块拆分（2026-09-22 D-5 + review 第 1 轮）
//! - `classify` — 5 组分类 helpers（is_attachable_state / is_inspectable_state /
//!   classify_invalid_state / TargetEvaluation / classify_outcome / is_all_conflict /
//!   has_fully_invalid_target / build_unresolved_target）
//! - `resolve_scan_kind` — ScanKind enum + resolve_scan_kind 纯函数
//! - `helpers` — DTO 投影小 helpers（to_available_batch_dto / to_attachable_batch_dto）
//!   + NoteScope::classify impl
//! - `find_or_create` — `DeliveryNoteService::scan_find_or_create_draft`
//!   （review 第 1 轮抽出；原 1294 行超 1000 行上限）
//! - `mod.rs`（本文件） — `DeliveryNoteService::scan_add` 入口 + 单元测试（见下
//!   `tests.rs`：classify_tests / scan_resolve_tests / classify_5groups_tests /
//!   c_group_distribution_tests / attachable_batches_tests / outcome_tests）
//!
//! 单元测试就地保留在 `mod.rs` 末尾（rust 2018+ 规定 `#[cfg(test)] mod` 之后只能再放
//! `#[cfg(test)]` 项，不能放任何生产代码）。
//!
//! ## 跨域 SQL 调用（2026-09-22 D-5 + review 第 1 轮）
//! 跨域调用（t_part / t_assembly / t_customer / t_part_batch）通过 `repo.conn_mut()`
//! 拿 `&mut PgConnection`，再喂给 ZST 静态方法（`PartRepo::xxx` / `AssemblyRepo::xxx` /
//! `CustomerRepo::xxx` / `PartBatchRepo::xxx`）。本域 SQL 走 `DeliveryNoteRepoTrait`
//! trait 方法（`note_*` / `group_*` / `event_*`）。
//!
//! ## 事务边界（2026-09-22 D-5 + review 第 1 轮）
//! handler `state.pool.begin()` → 这里 → handler `commit()`；本方法不 commit。
//! service 不知事务——所有 SQL 通过 trait 形参（trait impl for &mut PgConnection）。

use std::collections::HashMap;

use crate::auth::rbac::{CurrentUser, Role};
use crate::infra::clock::now_naive;
use crate::modules::assembly::repo::AssemblyRepo;
use crate::modules::com::customer::repo::CustomerRepo;
use crate::modules::delivery_note::dto::{
    AddedBatchDto, RecentItemDto, ResolvedEntityDto, ResolvedKindDto, ScanDeliveryNoteSummaryDto,
    ScanDeliveryOut, ScanOutcomeDto,
};
use crate::modules::delivery_note::model::NoteScope;
use crate::modules::delivery_note::repo::DeliveryNoteRepoTrait;
use crate::modules::part::repo::PartRepo;
use crate::modules::part::batch::repo::PartBatchRepo;
use crate::shared::error::{AppError, code};

use super::inner::{note_not_found, GroupWithMemberIds};

use super::DeliveryNoteService;
mod classify;
mod find_or_create;
mod helpers;
mod resolve_scan_kind;

#[cfg(test)]
mod tests;

use classify::{
    build_unresolved_target, classify_outcome, has_fully_invalid_target, is_all_conflict,
    is_inspectable_state, TargetEvaluation,
};
use resolve_scan_kind::{resolve_scan_kind, ScanKind};

// 暴露给 sibling 模块（attach.rs）+ 本 mod 内部（生产 `scan_add` + 单元测试）
// 的 re-export。`pub(crate)` 同时覆盖两种用途，避免 `use classify::xxx` +
// `pub use classify::xxx` 触发 E0252 重复定义。
pub(crate) use classify::{classify_invalid_state, is_attachable_state};

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
    ///
    /// 2026-09-22 D-5 + review 第 1 轮：service 形参改 by-value trait（iam 严格范本）；
    /// snowflake 改 `&self.snowflake`。
    #[allow(clippy::too_many_lines)]
    pub async fn scan_add<R: DeliveryNoteRepoTrait>(
        &self,
        mut repo: R,
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

        let part_opt = PartRepo::get_by_serial(&mut *repo.conn_mut(), &trimmed, false).await?;
        let asm_opt = AssemblyRepo::get_by_serial(&mut *repo.conn_mut(), &trimmed, false).await?;
        let kind = resolve_scan_kind(part_opt.as_ref(), asm_opt.as_ref());
        if matches!(kind, ScanKind::Unknown) {
            return Err(AppError::biz(
                code::BIZ_DELIVERY_SCAN_UNKNOWN_CODE,
                format!("scan code '{trimmed}' does not match any part or assembly"),
            ));
        }

        // 装配体 + children + 锚点（part.id 排序，幂等）
        let mut targets: Vec<crate::modules::part::model::TPart> = Vec::new();
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
                let asm = AssemblyRepo::get_by_id(&mut *repo.conn_mut(), aid, false)
                    .await?
                    .ok_or_else(|| {
                        AppError::biz(
                            code::BIZ_ASSEMBLY_NOT_FOUND,
                            format!("assembly {aid} not found"),
                        )
                    })?;
                let cs = PartRepo::list_children(&mut *repo.conn_mut(), aid, false).await?;
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
                let cs = PartRepo::list_children(&mut *repo.conn_mut(), a.id, false).await?;
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
        let leaf_cust = CustomerRepo::get_by_id(&mut *repo.conn_mut(), anchor_customer_id, false)
            .await?
            .ok_or_else(|| {
                AppError::biz(
                    code::BIZ_CUSTOMER_NOT_FOUND,
                    format!("anchor customer {anchor_customer_id} not found"),
                )
            })?;
        let l1_id = leaf_cust.parent_id.unwrap_or(leaf_cust.id);

        let groups_with_members = repo
            .group_list_active_groups_with_members_for_l1(l1_id)
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
        let note = self
            .scan_find_or_create_draft(&mut *repo.conn_mut(), l1_id, scope, current)
            .await?;

        // ===== Step 4: 加载 target 全部活跃 batch → C 组短路 → 5 组分类 =====
        let target_part_ids: Vec<i64> = targets.iter().map(|p| p.id).collect();
        let all_batches: Vec<crate::modules::part::batch::model::TPartBatch> =
            if target_part_ids.is_empty() {
                Vec::new()
            } else {
                PartBatchRepo::list_active_by_part_ids(&mut *repo.conn_mut(), &target_part_ids)
                    .await?
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
        let all_batches: Vec<crate::modules::part::batch::model::TPartBatch> = all_batches
            .into_iter()
            .filter(|b| classify_invalid_state(b).is_none())
            .collect();

        // 按 part_id 分桶（一次扫描）
        let mut batches_by_part: HashMap<i64, Vec<crate::modules::part::batch::model::TPartBatch>> =
            HashMap::new();
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
            let mut attachable: Vec<crate::modules::part::batch::model::TPartBatch> = Vec::new();
            let mut inspectable: Vec<crate::modules::part::batch::model::TPartBatch> = Vec::new();
            let mut conflict: Vec<crate::modules::part::batch::model::TPartBatch> = Vec::new();
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
                        &mut *repo.conn_mut(),
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
        let fresh_note = repo
            .note_get_by_id(note.id, false)
            .await?
            .ok_or_else(|| note_not_found(note.id))?;
        let line_count = PartBatchRepo::list_by_delivery_note(&mut *repo.conn_mut(), fresh_note.id)
            .await?
            .len();

        // 重新取一次 L1 客户名（scope_label L1Wide 路径要用）
        let l1_cust_name = CustomerRepo::get_by_id(&mut *repo.conn_mut(), fresh_note.customer_id, false)
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
            NoteScope::L1Wide => l1_cust_name
                .clone()
                .unwrap_or_else(|| leaf_cust.name.clone()),
            NoteScope::Group(_) => l1_cust_name
                .clone()
                .unwrap_or_else(|| leaf_cust.name.clone()),
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
        let recent_items: Vec<RecentItemDto> = PartBatchRepo::list_recent_by_note(
            &mut *repo.conn_mut(),
            fresh_note.id,
            RECENT_ITEMS_LIMIT,
        )
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
}


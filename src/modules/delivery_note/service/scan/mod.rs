//! P3：扫码入单（scan_add）（2026-09-22 D-5 拆出，原 1617 行超 1000 行上限）
//!
//! ## 子模块拆分（2026-09-22 D-5）
//! - `classify`         — 5 组分类 helpers（is_attachable_state / is_inspectable_state /
//!                        classify_invalid_state / TargetEvaluation / classify_outcome /
//!                        is_all_conflict / has_fully_invalid_target / build_unresolved_target）
//! - `resolve_scan_kind`— ScanKind enum + resolve_scan_kind 纯函数
//! - `helpers`          — DTO 投影小 helpers（to_available_batch_dto / to_attachable_batch_dto）
//!                        + NoteScope::classify impl
//! - `mod.rs`（本文件）  — `DeliveryNoteService::scan_add` 入口 + `scan_find_or_create_draft` +
//!                        单元测试（classify_tests / scan_resolve_tests / classify_5groups_tests /
//!                        c_group_distribution_tests / attachable_batches_tests / outcome_tests）
//!
//! 单元测试就地保留在 `mod.rs` 末尾（rust 2018+ 规定 `#[cfg(test)] mod` 之后只能再放
//! `#[cfg(test)]` 项，不能放任何生产代码）。
//!
//! ## 跨域 SQL 调用（2026-09-22 D-5）
//! 跨域调用（t_part / t_assembly / t_customer / t_part_batch）通过 `&mut PgConnection`
//! 上的 `DeliveryNoteRepoTrait` trait 方法（`note_*` / `group_*` / `event_*`）+ 跨域
//! ZST 静态方法（`PartRepo::xxx` / `AssemblyRepo::xxx` / `CustomerRepo::xxx` /
//! `PartBatchRepo::xxx`）混合使用。
//!
//! ## 事务边界（2026-09-22 D-5）
//! handler `state.pool.begin()` → 这里 → handler `commit()`；本方法不 commit。
//! service 不知事务——所有 SQL 通过 trait 形参（trait impl for &mut PgConnection）。

use std::collections::HashMap;

use sqlx::PgConnection;

use crate::auth::rbac::{CurrentUser, Role};
use crate::infra::{
    clock::now_naive, serial::next_delivery_note_no, snowflake::SnowflakeIdGenerator,
};
use crate::modules::assembly::repo::AssemblyRepo;
use crate::modules::com::customer::repo::CustomerRepo;
use crate::modules::delivery_note::dto::{
    AddedBatchDto, RecentItemDto, ResolvedEntityDto, ResolvedKindDto, ScanDeliveryNoteSummaryDto,
    ScanDeliveryOut, ScanOutcomeDto,
};
use crate::modules::delivery_note::model::{DeliveryNote, NoteScope};
use crate::modules::delivery_note::repo::DeliveryNoteRepoTrait;
use crate::modules::part::repo::PartRepo;
use crate::modules::part_batch::repo::PartBatchRepo;
use crate::shared::error::{AppError, code};

use super::inner::{note_not_found, GroupWithMemberIds};

use super::super::DeliveryNoteService;
mod classify;
mod helpers;
mod resolve_scan_kind;

use classify::{
    build_unresolved_target, classify_invalid_state, classify_outcome, has_fully_invalid_target,
    is_all_conflict, is_attachable_state, is_inspectable_state, TargetEvaluation,
};
use resolve_scan_kind::{resolve_scan_kind, ScanKind};

const STATUS_DRAFT: &str = "DRAFT";

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
                let asm = AssemblyRepo::get_by_id(&mut *conn, aid, false)
                    .await?
                    .ok_or_else(|| {
                        AppError::biz(
                            code::BIZ_ASSEMBLY_NOT_FOUND,
                            format!("assembly {aid} not found"),
                        )
                    })?;
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
        let leaf_cust = CustomerRepo::get_by_id(&mut *conn, anchor_customer_id, false)
            .await?
            .ok_or_else(|| {
                AppError::biz(
                    code::BIZ_CUSTOMER_NOT_FOUND,
                    format!("anchor customer {anchor_customer_id} not found"),
                )
            })?;
        let l1_id = leaf_cust.parent_id.unwrap_or(leaf_cust.id);

        let groups_with_members = conn
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
        let note = Self::scan_find_or_create_draft(conn, snowflake, l1_id, scope, current).await?;

        // ===== Step 4: 加载 target 全部活跃 batch → C 组短路 → 5 组分类 =====
        let target_part_ids: Vec<i64> = targets.iter().map(|p| p.id).collect();
        let all_batches: Vec<crate::modules::part_batch::model::TPartBatch> =
            if target_part_ids.is_empty() {
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
        let all_batches: Vec<crate::modules::part_batch::model::TPartBatch> = all_batches
            .into_iter()
            .filter(|b| classify_invalid_state(b).is_none())
            .collect();

        // 按 part_id 分桶（一次扫描）
        let mut batches_by_part: HashMap<i64, Vec<crate::modules::part_batch::model::TPartBatch>> =
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
            let mut attachable: Vec<crate::modules::part_batch::model::TPartBatch> = Vec::new();
            let mut inspectable: Vec<crate::modules::part_batch::model::TPartBatch> = Vec::new();
            let mut conflict: Vec<crate::modules::part_batch::model::TPartBatch> = Vec::new();
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
        let fresh_note = conn
            .note_get_by_id(note.id, false)
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
        if let Some(n) = conn
            .note_find_open_draft_by_scope(l1_id, scope, None)
            .await?
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

        match conn.note_create(&new_note).await {
            Ok(()) => conn
                .note_get_by_id(new_note.id, false)
                .await?
                .ok_or_else(|| note_not_found(new_note.id)),
            Err(sqlx::Error::Database(db_err)) if db_err.code().as_deref() == Some("23505") => {
                // 唯一索引撞 → 重查（同 scope 应有另一个 DRAFT 草稿）
                if let Some(n) = conn
                    .note_find_open_draft_by_scope(l1_id, scope, None)
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

// =============================================================================
//  单元测试（classify / resolve_scan_kind / outcome / c_group_distribution /
//            attachable_batches 共 6 组，就地保留在 mod.rs 末尾）
// =============================================================================

#[cfg(test)]
mod classify_tests {
    use super::super::super::inner::GroupWithMemberIds;
    use crate::modules::delivery_note::model::NoteScope;

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
    use super::resolve_scan_kind::{resolve_scan_kind, ScanKind};
    use crate::modules::assembly::model::TAssembly;
    use crate::modules::delivery_note::model::TPart;

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
            customer_id: 100 + id,
            assembly_id,
            status: "INSPECTION".to_string(),
            is_urgent: false,
            next_process_id: None,
            order_no: None,
            system_delivery_date: None,
            note: None,
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
            process_chain_id: None,
        }
    }

    fn make_assembly(id: i64) -> TAssembly {
        TAssembly {
            id,
            drawing_no: format!("A-{id:03}"),
            name: format!("Asm {id}"),
            applicant_name: None,
            customer_id: 900 + id,
            request_date: chrono::NaiveDate::from_ymd_opt(2026, 8, 22).unwrap(),
            planned_delivery_date: chrono::NaiveDate::from_ymd_opt(2026, 9, 22).unwrap(),
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
    use super::classify::{
        classify_invalid_state, is_attachable_state, is_inspectable_state,
    };
    use crate::modules::part_batch::model::TPartBatch;

    fn b(status: &str, holder: Option<i64>, location: Option<&str>) -> TPartBatch {
        TPartBatch {
            id: 0,
            part_id: 1,
            batch_no: 1,
            quantity: 1,
            status: status.to_string(),
            location: location.map(str::to_string),
            current_holder_id: holder,
            current_process_step_id: None,
            delivery_note_id: None,
            parent_batch_id: None,
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
        assert_eq!(
            classify_invalid_state(&b("DELIVERED", None, None)),
            Some("DELIVERED")
        );
        assert_eq!(
            classify_invalid_state(&b("OUTSOURCE", None, None)),
            Some("OUTSOURCE")
        );
        assert_eq!(
            classify_invalid_state(&b("COMPLETED", None, None)),
            Some("COMPLETED")
        );
        assert_eq!(
            classify_invalid_state(&b("CANCELLED", None, None)),
            Some("CANCELLED")
        );
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
        assert!(is_inspectable_state(&b(
            "IN_PROCESS",
            Some(7),
            Some("PRODUCTION_SHELF")
        )));
        // 仅工人持有（location='WORKER'）不可
        assert!(!is_inspectable_state(&b(
            "IN_PROCESS",
            Some(7),
            Some("WORKER")
        )));
    }
}

#[cfg(test)]
mod c_group_distribution_tests {
    use super::classify::{classify_invalid_state, has_fully_invalid_target};
    use crate::modules::part_batch::model::TPartBatch;

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
            current_process_step_id: None,
            delivery_note_id: None,
            parent_batch_id: None,
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

#[cfg(test)]
mod attachable_batches_tests {
    use super::classify::{build_unresolved_target, classify_outcome, TargetEvaluation};
    use super::helpers::{to_attachable_batch_dto, to_available_batch_dto};
    use crate::modules::delivery_note::dto::{
        AttachableBatchDto, AvailableBatchDto, BatchStatusDto, ScanOutcomeDto, UnresolvedTargetDto,
    };
    use crate::modules::delivery_note::model::TPart;
    use crate::modules::part_batch::model::TPartBatch;

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
            current_process_step_id: None,
            delivery_note_id: None,
            parent_batch_id: None,
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
            customer_id: 1,
            assembly_id: None,
            status: "INSPECTION".to_string(),
            is_urgent: false,
            next_process_id: None,
            order_no: None,
            system_delivery_date: None,
            note: None,
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
            process_chain_id: None,
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
            .filter(|x| classify_invalid_state(x).is_none()) // 直接调，不依赖上面的 mod path
            .collect();
        assert_eq!(filtered.len(), 2);

        // 按 attachable/inspectable 分桶（与 Step 4 一致）
        use super::classify::{is_attachable_state, is_inspectable_state};
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

    #[test]
    fn helper_dto_under_separate_paths() {
        // 测试 helpers::to_available_batch_dto / to_attachable_batch_dto
        // 拆分后独立可用（覆盖子模块入口）。
        let batch = b(99, 1, "INSPECTION", None, 3);
        let avail: AvailableBatchDto = to_available_batch_dto(batch.clone());
        assert_eq!(avail.batch_id, 99);
        assert_eq!(avail.version, 3);
        assert_eq!(avail.quantity, 10);
        let attach: AttachableBatchDto = to_attachable_batch_dto(batch);
        assert_eq!(attach.batch_id, 99);
        assert_eq!(attach.version, 3);
        assert_eq!(attach.quantity, 10);
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
            false, false, // any_inspectable
            false, // all_attachable_empty
            true,  // any_had_invalid_filtered
        );
        assert_eq!(outcome, ScanOutcomeDto::CandidatesAvailable);

        // 验证响应形态：unresolved_targets 单元素 + attachable_batches 含 A
        let unresolved: Vec<UnresolvedTargetDto> = vec![build_unresolved_target(eval)];
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
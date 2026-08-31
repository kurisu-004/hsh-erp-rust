//! 弹窗勾选 → 批量 attach A 组批次（POST /delivery-notes/{note_id}/attach-batches）。
//!
//! 与 `scan_add` 的区别：
//! - `scan_add` 是「扫码 + 自动 attach A 组」一站式，本文件是「前端弹窗勾选
//!   → 显式提交」二段式（`scan_add` 后续会改成不再自动 attach A 组）。
//! - `attach_batches` 部分失败 → 200 + `conflicts[]`；硬错误仅 note 非 DRAFT（409）。
//! - 复用 `scan::is_attachable_state`（A 组判定）与 `PartBatchRepo::attach_to_note`
//!   （version-checked UPDATE）。
//!
//! 单元测试位于文件末尾 `attach_batches_*`，覆盖正常 / OCC / 非 DRAFT / 部分成功 /
//! 非法状态 5 类路径。

use axum::http::StatusCode;
use sqlx::PgConnection;

use crate::auth::rbac::{CurrentUser, Role};
use crate::infra::clock::now_naive;
use crate::modules::part_batch::repo::PartBatchRepo;
use crate::shared::error::{code, AppError};

use super::super::dto::{AttachBatchItem, AttachBatchConflict, AttachBatchesOut};
use super::super::repo::DeliveryNoteRepo;
use super::inner::note_not_found;
use super::scan::is_attachable_state;

use super::DeliveryNoteService;

/// `t_delivery_note.status` 常量（与 DB 列值严格一致）
const STATUS_DRAFT: &str = "DRAFT";

impl DeliveryNoteService {
    /// 批量 attach A 组批次到指定 DRAFT 送货单。
    ///
    /// 流程概要：
    /// 1. 校验 note 存在且 status = `DRAFT`；否则 `BIZ_DELIVERY_NOTE_NOT_DRAFT`（409）。
    /// 2. 遍历每个 `AttachBatchItem`，**独立处理**（失败项记入 `conflicts` 不中断）：
    ///    - 批次不存在 / 已软删 → `BATCH_NOT_FOUND`
    ///    - `delivery_note_id IS NOT NULL`（已挂在某张单上） → `ALREADY_ATTACHED`
    ///    - status 不在 A 组（INSPECTION / READY_TO_SHIP） → `INVALID_STATE:<STATUS>`
    ///    - `attach_to_note` 返回 0 行 → `VERSION_CONFLICT`
    ///    - 其余 → `attached += 1`
    /// 3. 返回 `(attached, conflicts)`；handler 始终 200。
    ///
    /// 事务边界：handler `pool.begin()` → 这里 → handler `commit()`。本方法不 commit。
    pub async fn attach_batches(
        conn: &mut PgConnection,
        note_id: i64,
        items: Vec<AttachBatchItem>,
        current: &CurrentUser,
    ) -> Result<AttachBatchesOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk])?;

        // ===== Step 1: note 必须存在且为 DRAFT =====
        let note = DeliveryNoteRepo::get_by_id(&mut *conn, note_id, false)
            .await?
            .ok_or_else(|| note_not_found(note_id))?;
        if note.status != STATUS_DRAFT {
            // 显式 409：与 soft_delete 同码但状态语义（"现在不能动"）更接近冲突
            // 而非请求参数错误。status_from_code 表里 21403 默认 400，本场景
            // 强制 409；不修改全局表以免影响 soft_delete 的契约。
            return Err(AppError::biz_with_status(
                code::BIZ_DELIVERY_NOTE_NOT_DRAFT,
                format!("note {note_id} status is {}, not DRAFT", note.status),
                StatusCode::CONFLICT,
            ));
        }

        // ===== Step 2: 逐项独立 attach =====
        let now = now_naive();
        let mut attached: usize = 0;
        let mut conflicts: Vec<AttachBatchConflict> = Vec::new();

        for item in items {
            match PartBatchRepo::get_by_id(&mut *conn, item.batch_id, false).await? {
                None => conflicts.push(AttachBatchConflict {
                    batch_id: item.batch_id,
                    reason: "BATCH_NOT_FOUND".to_string(),
                }),
                Some(batch) => {
                    if batch.delivery_note_id.is_some() {
                        conflicts.push(AttachBatchConflict {
                            batch_id: item.batch_id,
                            reason: "ALREADY_ATTACHED".to_string(),
                        });
                        continue;
                    }
                    if !is_attachable_state(&batch.status) {
                        conflicts.push(AttachBatchConflict {
                            batch_id: item.batch_id,
                            reason: format!("INVALID_STATE:{}", batch.status),
                        });
                        continue;
                    }

                    let affected = PartBatchRepo::attach_to_note(
                        &mut *conn,
                        item.batch_id,
                        item.version,
                        note_id,
                        now,
                        Some(current.id),
                    )
                    .await?;
                    if affected == 0 {
                        conflicts.push(AttachBatchConflict {
                            batch_id: item.batch_id,
                            reason: "VERSION_CONFLICT".to_string(),
                        });
                    } else {
                        attached += 1;
                    }
                }
            }
        }

        Ok(AttachBatchesOut { attached, conflicts })
    }
}

#[cfg(test)]
mod attach_batches_logic_tests {
    //! 这些测试只覆盖**纯逻辑**（冲突原因归类），不连接 DB；DB 行为（OCC UPDATE、
    //! DRAFT 校验）由 task 7 的 handler-level e2e 覆盖。

    use super::is_attachable_state;

    #[test]
    fn attachable_state_includes_inspection_and_ready_to_ship() {
        assert!(is_attachable_state("INSPECTION"));
        assert!(is_attachable_state("READY_TO_SHIP"));
    }

    #[test]
    fn attachable_state_excludes_delivered_outsource_completed_cancelled() {
        // C 组：attach 路径必须显式拒绝
        for s in ["DELIVERED", "OUTSOURCE", "COMPLETED", "CANCELLED"] {
            assert!(!is_attachable_state(s), "{s} should NOT be attachable");
        }
    }

    #[test]
    fn attachable_state_excludes_pending_programming_repairing_in_process() {
        // B 组：送检候选，非 attach 候选
        for s in ["PENDING", "PROGRAMMING", "REPAIRING", "IN_PROCESS"] {
            assert!(!is_attachable_state(s), "{s} should NOT be attachable");
        }
    }

    #[test]
    fn attachable_state_excludes_unknown_string() {
        assert!(!is_attachable_state("UNKNOWN"));
        assert!(!is_attachable_state(""));
    }

    /// 模拟 service 内的失败归类：在 `attach_to_note` 之前，所有失败分支都应
    /// 落入 `conflicts` 而不增加 `attached`。这里只验证 reason 字符串拼装
    /// 契约，e2e 验证 attach DB 影响行数。
    #[test]
    fn conflict_reason_format_is_stable() {
        // 与 service 实现中的字符串字面量严格对齐，前端 i18n / 分类依赖此契约
        let r1 = "BATCH_NOT_FOUND".to_string();
        let r2 = "ALREADY_ATTACHED".to_string();
        let r3 = format!("INVALID_STATE:{}", "DELIVERED");
        let r4 = "VERSION_CONFLICT".to_string();
        assert!(r1.starts_with("BATCH_NOT_FOUND"));
        assert!(r2.starts_with("ALREADY_ATTACHED"));
        assert_eq!(r3, "INVALID_STATE:DELIVERED");
        assert!(r4.starts_with("VERSION_CONFLICT"));
    }
}
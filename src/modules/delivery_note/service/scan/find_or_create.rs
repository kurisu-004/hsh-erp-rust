//! P3 find-or-create DRAFT 草稿（2026-09-22 review 第 1 轮抽出，原 `scan/mod.rs`
//! 1294 行超 1000 行上限）
//!
//! 流程：
//! - 先 `find_open_draft_by_scope`，命中 → 返回；
//! - 未命中 → `next_delivery_note_no` 发放编号 + 雪花 id + INSERT；
//! - INSERT 撞唯一索引（23505，仅 Group/Leaf scope，可能）→ 重查一次；
//! - L1Wide scope 没有唯一索引，所以永不撞（设计 §3.3）。
//!
//! 2026-09-22 review 第 1 轮：service 形参改 by-value trait（iam 严格范本）；
//! `note_find_open_draft_by_scope` / `note_create` / `note_get_by_id` 走 trait，
//! `next_delivery_note_no` 跨域走 `&mut *repo.conn_mut()`。

use crate::auth::rbac::CurrentUser;
use crate::infra::{clock::now_naive, serial::next_delivery_note_no};
use crate::modules::delivery_note::{
    model::{DeliveryNote, NoteScope},
    repo::DeliveryNoteRepoTrait,
};
use crate::shared::error::AppError;

use super::super::inner::note_not_found;
use super::super::DeliveryNoteService;

/// `t_delivery_note.status` 常量（与 DB 列值严格一致）
const STATUS_DRAFT: &str = "DRAFT";

impl DeliveryNoteService {
    /// find-or-create DRAFT 草稿（扫码入口专用）。
    ///
    /// 流程：
    /// - 先 `find_open_draft_by_scope`，命中 → 返回；
    /// - 未命中 → `next_delivery_note_no` 发放编号 + 雪花 id + INSERT；
    /// - INSERT 撞唯一索引（23505，仅 Group/Leaf scope，可能）→ 重查一次；
    /// - L1Wide scope 没有唯一索引，所以永不撞（设计 §3.3）。
    pub async fn scan_find_or_create_draft<R: DeliveryNoteRepoTrait>(
        &self,
        mut repo: R,
        l1_id: i64,
        scope: NoteScope,
        current: &CurrentUser,
    ) -> Result<DeliveryNote, AppError> {
        if let Some(n) = repo
            .note_find_open_draft_by_scope(l1_id, scope, None)
            .await?
        {
            return Ok(n);
        }

        let now = now_naive();
        let delivery_note_no = next_delivery_note_no(&mut *repo.conn_mut(), l1_id).await?;
        let (dgid, lcid) = match scope {
            NoteScope::L1Wide => (None, None),
            NoteScope::Group(gid) => (Some(gid), None),
            NoteScope::Leaf(cid) => (None, Some(cid)),
        };
        let new_note = DeliveryNote {
            id: self.snowflake.next_id(),
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

        match repo.note_create(&new_note).await {
            Ok(()) => repo
                .note_get_by_id(new_note.id, false)
                .await?
                .ok_or_else(|| note_not_found(new_note.id)),
            Err(sqlx::Error::Database(db_err)) if db_err.code().as_deref() == Some("23505") => {
                // 唯一索引撞 → 重查（同 scope 应有另一个 DRAFT 草稿）
                if let Some(n) = repo
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
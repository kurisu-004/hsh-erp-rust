//! find-or-create DRAFT 草稿（扫码入单与扫码树的建单入口共用）
//!
//! 流程：
//! - 先 `find_open_draft_by_l1`，命中 → 返回；
//! - 未命中 → `next_delivery_note_no` 发放编号 + 雪花 id + INSERT；
//! - INSERT 撞唯一索引 23505 → **重查一次同一 `(customer_id, status='DRAFT')`**，
//!   兜住「两个会话同时扫码、各自查不到草稿」这一并发窗口。
//!
//! 2026-10-08：判定键由 `(customer_id, scope)` 收敛为 `(customer_id, DRAFT)`
//! 单键，`NoteScope` 与 `find_open_draft_by_scope` 一并下线。数据库侧的部分唯一
//! 索引 `uk_t_delivery_note_l1_open_draft` 是本逻辑的硬兜底（见
//! `migrations/20261008110000_002_delivery_note_l1_single_draft.sql`）。

use crate::auth::rbac::CurrentUser;
use crate::infra::{clock::now_naive, serial::next_delivery_note_no};
use crate::modules::com::delivery_note::{model::DeliveryNote, repo::DeliveryNoteRepoTrait};
use crate::shared::error::AppError;

use super::super::DeliveryNoteService;
use super::super::inner::note_not_found;

/// `t_delivery_note.status` 常量（与 DB 列值严格一致）
const STATUS_DRAFT: &str = "DRAFT";

impl DeliveryNoteService {
    /// find-or-create DRAFT 草稿（扫码入口专用）。
    ///
    /// `l1_id` 必须是**一级客户** id（`t_customer.parent_id IS NULL`）—— 调用方负责
    /// 从扫码命中的零件 / 装配件所属客户往上推一级。
    pub async fn scan_find_or_create_draft<R: DeliveryNoteRepoTrait>(
        &self,
        mut repo: R,
        l1_id: i64,
        current: &CurrentUser,
    ) -> Result<DeliveryNote, AppError> {
        if let Some(n) = repo.note_find_open_draft_by_l1(l1_id).await? {
            return Ok(n);
        }

        let now = now_naive();
        let delivery_note_no = next_delivery_note_no(&mut *repo.conn_mut(), l1_id).await?;
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
            // 范围列 2026-10-08 起逻辑废弃：判定键不再分范围，新单一律写 NULL。
            delivery_group_id: None,
            leaf_customer_id: None,
        };

        match repo.note_create(&new_note).await {
            Ok(()) => repo
                .note_get_by_id(new_note.id, false)
                .await?
                .ok_or_else(|| note_not_found(new_note.id)),
            Err(sqlx::Error::Database(db_err)) if db_err.code().as_deref() == Some("23505") => {
                // 撞 `uk_t_delivery_note_l1_open_draft` ⇒ 并发窗口内另一个事务刚建了
                // 同一 L1 的 DRAFT。重查一次（判定键唯一，重查必然命中它）。
                if let Some(n) = repo.note_find_open_draft_by_l1(l1_id).await? {
                    Ok(n)
                } else {
                    // 重查仍未命中，说明撞的是别的唯一索引（单号 uq_t_delivery_note_no_active
                    // 或事件表残留）⇒ 抛 23505 原始错误，不吞。
                    Err(AppError::Database(sqlx::Error::Database(db_err)))
                }
            }
            Err(e) => Err(e.into()),
        }
    }
}

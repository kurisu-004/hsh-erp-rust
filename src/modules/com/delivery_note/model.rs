//! com::delivery_note 域数据模型
//!
//! 包含：
//! - sqlx `FromRow` 行结构（送货分组、成员、note、单号日计数器；含 version 乐观锁、
//!   deleted_at 软删、created/updated 审计字段）
//! - 域枚举（status / sort_key / scan outcome）
//!
//! 2026-10-08：事件行结构 `DeliveryNoteEvent` 与事件枚举 `DeliveryNoteEventType`
//! 随事件子系统下线一并删除（见 `migrations/20261008100000_001_drop_*`）。

use chrono::NaiveDateTime;

// ---------------------------------------------------------------------------
//  DeliveryGroup / DeliveryGroupMember  (P1 实装)
// ---------------------------------------------------------------------------

/// `t_delivery_group` 行（送货分组头）
///
/// `customer_id` 是 L1（一级集团，`t_customer.parent_id IS NULL`），
/// service 层负责校验。
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct DeliveryGroup {
    pub id: i64,
    pub customer_id: i64,
    pub name: String,
    pub version: i32,
    pub created_at: NaiveDateTime,
    pub created_by: Option<i64>,
    pub updated_at: NaiveDateTime,
    pub updated_by: Option<i64>,
    pub deleted_at: Option<NaiveDateTime>,
}

/// `t_delivery_group_member` 行
/// `customer_id` 必须是 L2（`t_customer.parent_id = group.customer_id`），
/// service 层负责校验。
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct DeliveryGroupMember {
    pub id: i64,
    pub group_id: i64,
    pub customer_id: i64,
    pub created_at: NaiveDateTime,
    pub created_by: Option<i64>,
    pub deleted_at: Option<NaiveDateTime>,
}

// ---------------------------------------------------------------------------
//  DeliveryNote / DeliveryNoteCounter
// ---------------------------------------------------------------------------

/// `t_delivery_note` 行（送货单本体）
///
/// ⚠️ `delivery_group_id` / `leaf_customer_id` 是 2026-10-08 起**逻辑废弃**的范围列：
/// `NoteScope` 三态判定已下线，建单判定键收敛为 `(customer_id, status='DRAFT')`
/// 单键，两列不再被任何查询使用，新单一律写 NULL（保留列本身不动，避免 DDL 抖动
/// 与既有 baseline 约束 `ck_t_delivery_note_scope_exclusive` 的耦合）。
#[derive(Debug, Clone, sqlx::FromRow)]
#[allow(dead_code)]
pub struct DeliveryNote {
    pub id: i64,
    pub delivery_note_no: String,
    pub customer_id: i64,
    pub status: String,
    pub submitted_at: Option<NaiveDateTime>,
    pub picked_up_at: Option<NaiveDateTime>,
    pub submitted_by: Option<i64>,
    pub picked_up_by: Option<i64>,
    pub driver_worker_id: Option<i64>,
    pub note: Option<String>,
    pub version: i32,
    pub created_at: NaiveDateTime,
    pub created_by: Option<i64>,
    pub updated_at: NaiveDateTime,
    pub updated_by: Option<i64>,
    pub deleted_at: Option<NaiveDateTime>,
    pub delivery_date: Option<chrono::NaiveDate>,
    pub delivery_group_id: Option<i64>,
    pub leaf_customer_id: Option<i64>,
}

/// `t_delivery_note_counter` 行（业务日计数器）
///
/// PK = `date_ymd`（自然日 YYYYMMDD），单号 `DN-YYYYMMDD-NNNN` 由本表
/// `last_value` 原子递增发放。
#[derive(Debug, Clone, sqlx::FromRow)]
#[allow(dead_code)]
pub struct DeliveryNoteCounter {
    pub date_ymd: String,
    pub last_value: i32,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
}

// ---------------------------------------------------------------------------
//  Enums
// ---------------------------------------------------------------------------

/// 送货单状态机（DB 存 `varchar(16)`）
///
/// 流转：DRAFT → SUBMITTED → PICKED_UP → ARCHIVED；SUBMITTED 可被 recall 回 DRAFT。
/// 由 `statemachine.rs` 提供迁移判定，本 enum 只给模式与字面量。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryNoteStatus {
    Draft,
    Submitted,
    PickedUp,
    Archived,
}

impl DeliveryNoteStatus {
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Draft => "DRAFT",
            Self::Submitted => "SUBMITTED",
            Self::PickedUp => "PICKED_UP",
            Self::Archived => "ARCHIVED",
        }
    }

    /// 状态机迁移表（与设计 §7 对齐）：
    /// - Draft → Submitted
    /// - Submitted → Draft  (recall)
    /// - Submitted → PickedUp
    /// - PickedUp → Archived
    pub fn can_transition_to(&self, next: DeliveryNoteStatus) -> bool {
        use DeliveryNoteStatus::*;
        matches!(
            (*self, next),
            (Draft, Submitted) | (Submitted, Draft) | (Submitted, PickedUp) | (PickedUp, Archived)
        )
    }
}

impl std::str::FromStr for DeliveryNoteStatus {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "DRAFT" => Ok(Self::Draft),
            "SUBMITTED" => Ok(Self::Submitted),
            "PICKED_UP" => Ok(Self::PickedUp),
            "ARCHIVED" => Ok(Self::Archived),
            _ => Err(()),
        }
    }
}

/// 送货单列表排序字段（与 Python `DeliveryNoteSortKey` 对齐）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum DeliveryNoteSortKey {
    CreatedAt,
    SubmittedAt,
    PickedUpAt,
    DeliveryNoteNo,
}

impl DeliveryNoteSortKey {
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::CreatedAt => "CREATED_AT",
            Self::SubmittedAt => "SUBMITTED_AT",
            Self::PickedUpAt => "PICKED_UP_AT",
            Self::DeliveryNoteNo => "DELIVERY_NOTE_NO",
        }
    }
}

/// 扫码入单结果（P3 用，P1 模型先就位）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum ScanOutcome {
    Added,
    AlreadyPresent,
}

impl ScanOutcome {
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Added => "ADDED",
            Self::AlreadyPresent => "ALREADY_PRESENT",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_machine_round_trip() {
        // 合法迁移
        assert!(DeliveryNoteStatus::Draft.can_transition_to(DeliveryNoteStatus::Submitted));
        assert!(DeliveryNoteStatus::Submitted.can_transition_to(DeliveryNoteStatus::Draft));
        assert!(DeliveryNoteStatus::Submitted.can_transition_to(DeliveryNoteStatus::PickedUp));
        assert!(DeliveryNoteStatus::PickedUp.can_transition_to(DeliveryNoteStatus::Archived));

        // 非法迁移（漏掉）
        assert!(!DeliveryNoteStatus::Draft.can_transition_to(DeliveryNoteStatus::Draft));
        assert!(!DeliveryNoteStatus::Draft.can_transition_to(DeliveryNoteStatus::PickedUp));
        assert!(!DeliveryNoteStatus::Draft.can_transition_to(DeliveryNoteStatus::Archived));
        assert!(!DeliveryNoteStatus::Submitted.can_transition_to(DeliveryNoteStatus::Archived));
        assert!(!DeliveryNoteStatus::PickedUp.can_transition_to(DeliveryNoteStatus::Draft));
        assert!(!DeliveryNoteStatus::PickedUp.can_transition_to(DeliveryNoteStatus::Submitted));
        assert!(!DeliveryNoteStatus::Archived.can_transition_to(DeliveryNoteStatus::Draft));
    }

    #[test]
    fn status_str_round_trip() {
        use std::str::FromStr;
        for s in [
            DeliveryNoteStatus::Draft,
            DeliveryNoteStatus::Submitted,
            DeliveryNoteStatus::PickedUp,
            DeliveryNoteStatus::Archived,
        ] {
            assert_eq!(DeliveryNoteStatus::from_str(s.as_str()).ok(), Some(s));
        }
        assert!(DeliveryNoteStatus::from_str("UNKNOWN").is_err());
    }
}

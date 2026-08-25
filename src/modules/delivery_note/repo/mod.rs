//! delivery_note 域数据访问层。
//!
//! 对应 Python myERP/repository/delivery_note_repository.py。函数签名接收
//! `impl PgExecutor<'_>`，兼容 `&PgPool` / `&mut PgConnection` / `&mut Transaction`。
//!
//! ## 子模块布局
//! - `query`  — 读查询（list / get / find_open_draft）+ 共享 helper `push_status_filter`
//! - `mutate` — 写查询（insert / update / soft_delete）
//!
//! ## Phase 范围
//! - **P1**：送货分组 CRUD（DeliveryGroupRepo）
//! - **P2**：送货单 CRUD + 事件 + 草稿查找（DeliveryNoteRepo / DeliveryNoteEventRepo）

mod mutate;
mod query;

pub struct DeliveryGroupRepo;
pub struct DeliveryNoteRepo;
pub struct DeliveryNoteEventRepo;

/// 排序方向（与 Python `model.enums::SortDir` 对齐）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortDir {
    Asc,
    Desc,
}
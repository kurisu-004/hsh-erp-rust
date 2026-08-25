//! delivery_note service 层入口
//!
//! 按功能拆为下列子模块：
//! - `group`     — DeliveryGroupService（P1 分组 CRUD）
//! - `crud`      — DeliveryNoteService 列表/草稿/编辑/添加/移除
//! - `lifecycle` — DeliveryNoteService 状态流转与读视图（提交/撤回/拣货/事件/候选）
//! - `scan`      — DeliveryNoteService::scan_add（P3 扫码入单）+ `NoteScope::classify` + `resolve_scan_kind`
//! - `print`     — DeliveryNoteService::print_xlsx（P4 Excel 打印）
//! - `inner`     — 跨子模块共享的私有 helper（`build_note_outs` / `add_parts_inner` / `write_event` / `validate_*` / 错误构造器 等）
//!
//! 对外 API（`handler.rs` 调用面）保持原路径：
//! - `service::DeliveryGroupService::{list_for_l1, create, update, soft_delete}`
//! - `service::DeliveryNoteService::{list_with_filters, list_for_pickup, create_draft, get_with_parts, get_many_with_parts, update, add_parts, remove_parts, submit, recall, pickup_scan, pickup, soft_delete, list_events, list_candidate_parts, scan_add, print_xlsx}`

mod crud;
mod group;
mod inner;
mod lifecycle;
mod print;
mod scan;

pub struct DeliveryGroupService;
pub struct DeliveryNoteService;

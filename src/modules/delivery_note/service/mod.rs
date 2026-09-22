//! delivery_note service 层入口
//!
//! 按功能拆为下列子模块：
//! - `group`     — DeliveryGroupService（P1 分组 CRUD）
//! - `crud`      — DeliveryNoteService 列表/草稿/编辑/添加/移除
//! - `lifecycle` — DeliveryNoteService 状态流转与读视图（提交/撤回/拣货/事件/候选）
//! - `scan`      — DeliveryNoteService::scan_add（P3 扫码入单）+ NoteScope 分类 +
//!                5 组分类 helpers + resolve_scan_kind（按业务子域再拆为
//!                `scan/{mod, classify, resolve_scan_kind, helpers}.rs`）
//! - `print`     — DeliveryNoteService::print_xlsx（P4 Excel 打印）
//! - `attach`    — DeliveryNoteService::attach_batches（P3+ 弹窗批量 attach）
//! - `inner`     — 跨子模块共享的私有 helper（`build_note_outs` / `add_parts_inner` /
//!                `write_event` / `validate_*` / 错误构造器 等）
//!
//! 对外 API（`handler.rs` 调用面）保持原路径：
//! - `service::DeliveryGroupService::{list_for_l1, create, update, soft_delete}`
//! - `service::DeliveryNoteService::{list_with_filters, list_for_pickup, create_draft,
//!    get_with_parts, get_many_with_parts, update, add_parts, remove_parts, submit, recall,
//!    pickup_scan, pickup, soft_delete, list_events, list_candidate_parts, scan_add,
//!    attach_batches, print_xlsx}`
//!
//! ## 2026-09-22 D-5 重构对齐 iam / shelf / customer 范本
//! - 本域 SQL 真源统一在 `repo/sql.rs`（原 `repo/query.rs` + `repo/mutate.rs`
//!   合并），ZST struct（`DeliveryGroupRepo` / `DeliveryNoteRepo` /
//!   `DeliveryNoteEventRepo`）保留为静态调用面。
//! - 新增胖 trait `DeliveryNoteRepoTrait`（23 方法 = 11 group + 10 note + 2 event），
//!   `impl for &mut PgConnection`——service 内部 SQL 调用全部走 trait 方法
//!   （`conn.note_xxx()` / `conn.group_xxx()` / `conn.event_xxx()`）。
//! - handler 三形态严格区分：① 纯写端点 `pool.begin() → service → commit`；
//!   ② 写 + post-commit 副作用（attach_batches / pickup / scan_add 等写后广播）
//!   `pool.begin() → service → commit → state.ws_hub.broadcast(...)`；
//!   ③ 读端点（me / list_users / get_user / list_user_roles）`pool.acquire() → service`，
//!   不开事务。
//! - WS 广播严格走 handler（service 不持有 `Arc<WsHub>`）——commit 后由 handler 调
//!   `state.ws_hub.broadcast(WsEvent::DashboardEvent{...})`。

mod attach;
mod crud;
mod group;
mod inner;
mod lifecycle;
mod print;
mod scan;

pub struct DeliveryGroupService;
pub struct DeliveryNoteService;
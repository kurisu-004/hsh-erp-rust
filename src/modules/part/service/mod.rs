//! part 域业务逻辑（按业务流聚合，`impl PartService`）
//!
//! 2026-10-02：`t_part_batch` 的归属连同**全部以批次为对象的用例**迁往
//! `crate::modules::prod::batch`（repo / model / status_gate / 25 条路由的服务层）。
//! 本模块自此只承载 part 级用例。
//!
//! - `crud.rs`：单件 CRUD（create_part / get_part / list_parts / update_part /
//!   soft_delete_part / upload_part_file / upload_drawing / upload_3d_model /
//!   get_part_batches_by_serial）+ helpers（map_create_error / expand_customer_id /
//!   lookup_customer_names）
//! - `batch.rs`：批量创建（batch_create_parts legacy + batch_create_parts_with_bindings +
//!   prepare_binding_head_copy + PreparedBinding），2026-09-16 M2-B + M2-C 重构
//! - `lifecycle.rs`：part 级多批次终态（cancel / force_complete）
//! - `phase1/events.rs`：事件历史 / 位置树 / 批量创建增强（list_events /
//!   location_tree / batch_with_pdfs / match_by_excel_items / batch_update_order_info）
//! - `phase1/lifecycle_helpers.rs`：待编程一览 + 批次列表（list_pending_programming /
//!   list_batches）
//! - `phase1/outsource.rs`：外协系列只读端点（list_outsource_in_flight /
//!   list_outsource_sendable）
//! - `phase1/work_type.rs`：工种维度只读端点（list_by_work_type /
//!   list_pickable_by_work_type / list_by_worker）
//! - `rollup.rs`：rollup 工具（sync_from_batch_change）
//! - `list_enrichment.rs`：list_parts 派生层「位置 / 持有人」跨三表解析
//!   helper（2026-09-22 review 第 2 轮从 crud.rs 抽出，原 1054 行超限）
//!
//! ## 对 prod 域的依赖（数据依赖，方向单一）
//! `part::repo` 与本模块的 `list_enrichment` / `batch` / `crud` 经
//! `crate::modules::prod::batch::repo::PartBatchRepo` 读写 `t_part_batch`，
//! `rollup.rs` 经 `crate::modules::prod::batch::status_gate::rollup_part_derived`
//! 做 batch → part 派生。`t_part_batch.status` 的写入口全仓唯一，在
//! `crate::modules::prod::batch::status_gate`。

pub mod batch;
pub mod crud;
pub mod lifecycle;
pub mod list_enrichment;
pub mod phase1;
pub mod rollup;

/// 批量端点单次请求最大 item 数（handler/service 双层校验）。
pub const BATCH_CREATE_PARTS_MAX_ITEMS: usize = 200;

pub struct PartService;

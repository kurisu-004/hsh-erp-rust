//! part 域业务逻辑（按业务流聚合，`impl PartService`）
//!
//! 2026-10-02：`t_part_batch` 的归属连同**全部以批次为对象的用例**迁往
//! `crate::modules::prod::batch`（repo / model / shared::batch::status / 批次路由的服务层）。
//! 本模块自此只承载 part 级用例。
//!
//! - `crud.rs`：单件 CRUD（create_part / get_part / list_parts / update_part /
//!   soft_delete_part / get_part_batches_by_serial）+ helpers（map_create_error /
//!   lookup_customer_names；客户 id 展开走 `crate::shared::customer`）。
//!   2026-10-13 PR1b：三条上传（upload_part_file / upload_drawing /
//!   upload_3d_model）删除，迁往 `files::part_file`
//! - `batch.rs`：批量创建（`batch_create_parts_legacy`；2026-10-15 起 bindings 分支
//!   随统一上传链路上线删除，文件改走 `files::part_file` 的 upload-intents + confirm）
//! - `lifecycle.rs`：part 级多批次终态（cancel / force_complete）
//! - `phase1/events.rs`：事件历史 / 位置树 / 批量创建增强（list_events /
//!   location_tree / batch_update_order_info；⚠️ `batch_with_pdfs` 已于 2026-10-13
//!   随端点删除）
//! - `phase1/excel_match.rs`：采购订单 Excel 匹配（2026-10-06 新增，从
//!   `events.rs` 迁出；含分档决策纯函数 `resolve_match_tier`）
//! - `phase1/lifecycle_helpers.rs`：批次列表（list_batches）
//!
//! ⚠️ 原 `phase1/work_type.rs`（工种维度只读端点 `list_by_work_type`）已于
//! 2026-10-13 随端点**整文件删除**（零消费方，移除记录见
//! `crate::modules::part::handler::lifecycle` 的模块 doc）—— 本模块已无工种维度
//! 读端点。
//! - `rollup.rs`：rollup 工具（sync_from_batch_change）
//! - `list_enrichment.rs`：list_parts 派生层「位置 / 持有人」跨三表解析
//!   helper（2026-09-22 review 第 2 轮从 crud.rs 抽出，原 1054 行超限）
//!
//! ## 对 prod 域的依赖（数据依赖，方向单一）
//! `part::repo` 与本模块的 `list_enrichment` / `batch` / `crud` 经
//! `crate::modules::prod::batch::repo::PartBatchRepo` 读写 `t_part_batch`，
//! `rollup.rs` 经 `crate::shared::batch::status::rollup_part_derived`
//! 做 batch → part 派生。`t_part_batch.status` 的写入口全仓唯一，在
//! `crate::shared::batch::status`。

pub mod batch;
pub mod crud;
pub mod lifecycle;
pub mod list_enrichment;
pub mod phase1;
pub mod rollup;

/// 批量端点单次请求最大 item 数（handler/service 双层校验）。
pub const BATCH_CREATE_PARTS_MAX_ITEMS: usize = 200;

pub struct PartService;

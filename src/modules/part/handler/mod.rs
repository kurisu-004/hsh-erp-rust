//! part 域 HTTP handler 总入口
//!
//! 按业务域拆为多文件：
//! - `crud.rs` —— 单件 CRUD：list / detail / create / update / soft-delete / by-serial /
//!   列表 / 事件 / 位置树 / match-by-excel / batch-update-order-info
//! - `lifecycle.rs` —— 终态 + 状态机扩展（deliver / cancel / complete / start-repair /
//!   place-on-shelf / recall / programming 流转 / outsource 流转 / repair / split / cancel-batch /
//!   pick-up / 各种 list 列表）
//! - `inspection.rs` —— to-XXX 流（to_ship / to_inspection / to_process）+ 扫码（scan-inspect /
//!   scan-deliver-part / worker-scan）+ 批 list（repair-batches / repairing-batches）
//! - `batch.rs` —— 批量创建（batch / batch-with-bindings / batch-with-pdfs）+ 批量流转
//!   （batch-to-ship / batch-to-inspection）+ 直传 COS confirm
//!
//! ## 约定
//! - 事务边界在 handler：`state.pool.begin()` → 传 `&mut tx` 给 service → 显式
//!   `tx.commit()`；提前 return（`?`）时 `Transaction` 的 Drop 自动回滚。
//! - 统一响应信封：`Result<Json<R<T>>, AppError>`。
//! - 权限在 handler（`current.require_any_role(...)`）；业务层 service 也会
//!   再校验一次（双层守卫，与现有其他域保持一致）。
//!
//! ## to-XXX / batch-to-* 路由顺序敏感
//! 静态段 `/batch-to-*` / `/worker-scan` 必须在 `/{part_id}/...` catch-all 之前注册，
//! 否则 axum 会把静态段解析成 part_id。组装见 `super::router()`（src/modules/part/mod.rs）。

pub mod batch;
pub mod crud;
pub mod inspection;
pub mod lifecycle;

// 2026-09-15 followup-cleanup A8：原 part 文件路由（cad-files / cnc-programs /
// setup-sheets / cnc-pair / files）已迁出到 `src/modules/part_file/handler.rs::part_nested_router()`，
// 在 `super::router()` nest 进来，保持历史 URL `/api/v2/parts/{part_id}/<file>...`。

// 2026-09-16 M2-C：handler 模块按 docs/conventions.md §2 拆分为 crud / lifecycle /
// inspection / batch 四个文件。为避免上层 `part::router()` 与既有测试需要改动全部
// handler::* 路径，**re-export** 所有公开 handler 到 `handler::xxx` 平铺路径。
// 调用方（如 `super::router()`）继续用 `handler::list_parts` / `handler::to_ship` 等
// 路径，无需关心 handler 内部拆分。

// ----- crud.rs -----
pub use crud::{
    batch_update_order_info, create_part, get_assembly_by_part, get_by_serial,
    get_by_serial_part_batches, get_location_tree, get_part_detail, list_inspection_batches,
    list_part_batches, list_part_events, list_parts, match_by_excel_items, soft_delete_part,
    update_part, upload_3d_model, upload_drawing,
};

// ----- lifecycle.rs -----
pub use lifecycle::{
    cancel, cancel_batch, complete, complete_repair, deliver, list_by_work_type, list_by_worker,
    list_outsource_in_flight, list_outsource_sendable, list_pending_programming,
    list_pickable_by_work_type, pick_up, place_on_shelf, recall_to_pending, recall_to_programming,
    receive_from_outsource, receive_from_outsource_to_inspection, release_from_programming,
    repair_dispatch, send_to_outsource, send_to_programming, split_batch, start_repair,
};

// ----- inspection.rs -----
pub use inspection::{
    batch_to_ship, list_repair_batches, list_repairing_batches, scan_deliver_part, scan_inspect,
    to_inspection, to_process, to_ship, worker_scan,
};

// ----- batch.rs -----
pub use batch::{batch_create_parts, batch_to_inspection, batch_with_pdfs, confirm_part_file};

//! part 域 HTTP handler 总入口
//!
//! 按业务域拆为多文件：
//! - `crud.rs` —— 单件 CRUD：list / detail / create / update / soft-delete / by-serial /
//!   列表 / 事件 / 位置树 / match-by-excel / batch-update-order-info
//!   （2026-10-13 PR1b：`upload-drawing` / `upload-3d-model` 两条已迁出本文件）
//! - `lifecycle.rs` —— part 级终态（cancel / force-complete）+ 各种 list 列表
//!   （by-work-type / by-worker）
//! - `batch.rs` —— 批量创建（batch / batch-with-bindings / batch-with-pdfs）
//!   （2026-10-13 PR1b：`confirm_part_file` 已迁至 `files::part_file::handler`）
//! - `print.rs` —— 打印转发（print-drawing / print-drawing-batch，纯 BFF：鉴权 + 转发到 python）
//!
//! 2026-10-02：原 `inspection.rs` 整体迁至 `crate::modules::prod::batch::handler`
//! （to-XXX 流 / 扫码 / worker-scan / 3 条批次集合读 + `lifecycle.rs` 里的 18 个
//! 批次级端点），URL 从 `/api/v2/parts/{part_id}/…` 硬切到
//! `/api/v2/prod/batches/{batch_id}/…`。
//!
//! ## 约定
//! - 事务边界在 handler：`state.pool.begin()` → 传 `&mut tx` 给 service → 显式
//!   `tx.commit()`；提前 return（`?`）时 `Transaction` 的 Drop 自动回滚。
//! - 统一响应信封：`Result<Json<R<T>>, AppError>`。
//! - 权限在 handler（`current.require_any_role(...)`）；业务层 service 也会
//!   再校验一次（双层守卫，与现有其他域保持一致）。
//!
//! ## 静态段路由顺序敏感
//! 静态段（`/batch` / `/by-serial` …）必须在
//! `/{part_id}/...` catch-all 之前注册，否则 axum 会把静态段解析成 part_id。
//! 组装见 `super::router()`（src/modules/part/mod.rs）。

pub mod batch;
pub mod crud;
pub mod lifecycle;
pub mod print;

// 2026-10-13：原 part 维度文件路由（cad-files / cnc-programs / setup-sheets /
// cnc-pair / files）随 part_file 域收编进 files 域**整段下线**（8 条，无 alias，
// 旧路径 404），本文件不再有对应 handler；逐条移除记录见
// `src/modules/files/part_file/handler.rs` 的模块 doc。

// 2026-09-16 M2-C：handler 模块按 docs/conventions.md §2 拆分为 crud / lifecycle /
// inspection / batch 四个文件。为避免上层 `part::router()` 与既有测试需要改动全部
// handler::* 路径，**re-export** 所有公开 handler 到 `handler::xxx` 平铺路径。
// 调用方（如 `super::router()`）继续用 `handler::list_parts` / `handler::to_ship` 等
// 路径，无需关心 handler 内部拆分。

// ----- crud.rs -----
// 2026-10-13 PR1b：`upload_3d_model` / `upload_drawing` 随两条端点删除 —— 它们的
// 写路径并进 `files::part_file` 的 `POST /api/v2/files/objects`（四合一）。
pub use crud::{
    batch_update_order_info, create_part, get_assembly_by_part, get_by_serial,
    get_by_serial_part_batches, get_location_tree, get_part_detail, list_part_batches,
    list_part_events, list_parts, match_by_excel_items, soft_delete_part, update_part,
};

// ----- lifecycle.rs -----
// 2026-09-29 端点下线：`send_to_programming` / `recall_to_programming` 已删除
// （PROGRAMMING 状态废弃进入路径）。
// 2026-10-02 批次级端点迁出：deliver / complete / start-repair / place-on-shelf /
// recall-to-pending / release-from-programming / outsource 三端点 / complete-repair /
// repair-dispatch / split-batch / cancel-batch / pick-up（见 prod::batch::handler::lifecycle）。
// 2026-10-10 报工台端点迁出：pickable-by-work-type / by-worker（见 prod::scan::handler）。
pub use lifecycle::{cancel, force_complete, list_by_work_type};

// ----- batch.rs -----
// 2026-10-13 PR1b：`confirm_part_file` 自本域删除（硬切到
// `POST /api/v2/files/objects/confirm`，**无 alias，旧路径 404**）。
pub use batch::{batch_create_parts, batch_with_pdfs};

//! part 域
//!
//! 对应 Python myERP：
//! - api/v1/part.py
//! - service/part_service.py
//! - repository/part_repository.py
//! - model/part.py
//! - schema/part.py
//!
//! 2026-10-02 批次路由迁出后，本域只留「多批次动作 + 非批次动作」：
//! `/{part_id}/cancel`（BATCH-N：翻转该 part 全部活跃批次）、
//! `/{part_id}/force-complete`（全部非 CANCELLED 批次）、
//! `/{part_id}/soft-delete`、`GET /{part_id}/batches`（part 的批次集合读）、
//! 全部 CRUD / 文件 / Excel 工具 / 各类 list 端点。
//!
//! 以**单个批次**为操作对象的 22 条端点（`to-*` / `deliver` / `complete` /
//! `pick-up` / `split-batch` / `cancel-batch` / `worker-scan` / 3 条批次集合读 …）
//! 已迁至 `crate::modules::prod::batch`，URL 改挂 `/api/v2/prod/batches/*`
//! （原 `/api/v2/parts/{part_id}/…` 404，**无 alias**）。
//!
//! 2026-10-10 下线报工台的 2 条 list 端点：`GET /pickable-by-work-type/{id}` 与
//! `GET /by-worker/{id}` —— 它们的唯一消费方是报工台三页，连同取行 SQL / 出参
//! 一并迁往 `crate::modules::prod::scan`（新路径
//! `GET /api/v2/prod/scan/pickable` 与 `GET /api/v2/prod/scan/held`，**无 alias**）。
//! 两条旧路径**实际返回干净 404**（不是 400）：它们是 2 段 path，而本域 `/{part_id}`
//! catch-all 只有 **1 段**，2 段路径够不着它 ⇒ `matchit` 无命中 ⇒ `Path<i64>`
//! extractor 根本没机会出手。⚠️ 别照抄下方外协那段的 400 —— 那两条是 **1 段**
//! 静态路径，落进 catch-all 才会被 `Path` 拒成 400。段数是分水岭。逐条实测表见
//! `docs/api/scan.md` §1.2（「教训登记」小节写了这条直觉为什么不总成立）。
//!
//! 2026-10-03 下线 2 条外协 list 端点：`/outsource-in-flight` /
//! `/outsource-sendable` —— 二者返回的是通用 `PartListItem`，与前端外协域需要的
//! 字段（批次级 version / quantity、外协公司、报价价、客户路径…）**形状不匹配**
//! （前端两个 tab 因此空白 / 全灰）。取代者迁往 outsource 域：
//! `GET /api/v2/outsource-shipments/in-flight` 与
//! `GET /api/v2/outsource-sendable`。**硬切无 alias**，旧 URL 实际返回 **400**
//! 而非 404 —— 本域 `/{part_id}`（`Path<i64>`）catch-all 兜住任何未注册的 1 段
//! 静态路径，再由 `Path` extractor 拒绝非数字段；该成因与「不改跨域路由语义」的
//! 取舍，本节即全仓最完整的一处说明（外协域侧另有同义登记）。

pub mod dto_crud;
pub mod handler;
pub mod model;
pub mod repo;
pub mod service;
pub mod statemachine;
pub mod vo;

use axum::{
    Router,
    routing::{get, post},
};
use std::sync::Arc;

use crate::state::AppState;

// 2026-09-29 修复：移除 2026-09-15 followup-cleanup A8 的兼容 nest。
// part 维度文件路由（cad-files / cnc-programs / setup-sheets / cnc-pair / files）
// 仅通过 part_file 域 canonical 第二入口
// `/api/v2/part-files/parts/{part_id}/<file>...` 对外暴露（见
// `backend-rust/src/modules/part_file/handler.rs::router()`）。

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        // ★ 静态段必须在 /{part_id}/... catch-all 之前注册，
        //   否则 axum 会把静态段（如 `batch`、`by-serial`）
        //   解析成 part_id。
        // ---- 列表 / 静态段 ----
        .route("/", get(handler::list_parts).post(handler::create_part))
        .route("/batch", post(handler::batch_create_parts))
        .route("/by-serial/{serial_no}", get(handler::get_by_serial))
        .route(
            "/by-serial/{serial_no}/part-batches",
            get(handler::get_by_serial_part_batches),
        )
        // ---- Phase 1（2026-09-13）静态段（在 {part_id} catch-all 之前注册）----
        .route("/location-tree", get(handler::get_location_tree))
        .route("/match-by-excel-items", post(handler::match_by_excel_items))
        .route(
            "/batch-update-order-info",
            post(handler::batch_update_order_info),
        )
        .route("/batch-with-pdfs", post(handler::batch_with_pdfs))
        // 2026-10-03 新增：批量零件图纸打印（纯 BFF 转发到 python 执行 PDF 光栅化 +
        // pikepdf 合并）。★静态段必须在 /{part_id}/... catch-all 之前注册。
        .route(
            "/print-drawing-batch",
            post(handler::print::print_drawing_batch),
        )
        // ---- Phase 2 (2026-09-13) 静态段（by-work-type）----
        .route(
            "/by-work-type/{work_type_id}",
            get(handler::list_by_work_type),
        )
        // ---- 单件 {part_id} ----
        .route("/{part_id}", get(handler::get_part_detail))
        .route("/{part_id}/update", post(handler::update_part))
        .route("/{part_id}/soft-delete", post(handler::soft_delete_part))
        .route("/{part_id}/upload-drawing", post(handler::upload_drawing))
        .route("/{part_id}/upload-3d-model", post(handler::upload_3d_model)) // 2026-09-11 新增：3D 模型上传
        // 2026-10-03 新增：单件零件图纸打印（图纸正面 + 条码背面的双面 PDF）。
        // 纯 BFF 转发到 python `GET /api/v1/parts/{id}/print`（v2/v1 路径不同名）。
        .route(
            "/{part_id}/print-drawing",
            get(handler::print::print_drawing),
        )
        // BATCH-N：翻转该 part 全部活跃批次 → part CANCELLED（留 part 域）
        .route("/{part_id}/cancel", post(handler::cancel))
        // BATCH-N：全部非 CANCELLED 批次强推 COMPLETED（留 part 域）
        .route("/{part_id}/force-complete", post(handler::force_complete)) // 2026-09-30 新增：MANAGER 单角色强推工单 + 所有活跃批次为 COMPLETED（绕状态机）
        .route("/{part_id}/events", get(handler::list_part_events))
        // part 的批次集合读（留 part 域：操作对象是「该 part 的批次集合」）
        .route("/{part_id}/batches", get(handler::list_part_batches))
        // ---- 2026-09-25 D-08 api-drift-fix：按 part 反查所属装配体 ----
        .route("/{part_id}/assembly", get(handler::get_assembly_by_part))
        // ---- 2026-09-29 修复：移除 2026-09-15 followup-cleanup A8 的兼容 nest。
        //      part 维度文件路由（cad-files / cnc-programs / setup-sheets / cnc-pair / files）
        //      仅通过 part_file 域 canonical 第二入口
        //      `/api/v2/part-files/parts/{part_id}/<file>...` 对外暴露（per
        //      backend-rust/src/modules/part_file/handler.rs::router() 注释）。
        // ---- 2026-09-16 M2-B 新增：直传 COS 链路 confirm 端点（POST /parts/{id}/files/confirm）----
        .route("/{part_id}/files/confirm", post(handler::confirm_part_file))
}

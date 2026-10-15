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
//! `/{part_id}/soft-delete`、`GET /{part_id}/batches`（part 的批次集合读）、
//! 全部 CRUD / 文件 / Excel 工具 / 各类 list 端点。
//!
//! ⚠️ **2026-10-13：`/{part_id}/force-complete` 已迁出本域**，硬切到
//! `POST /api/v2/prod/parts/{part_id}/force-complete`（见本文件 [`force_complete_router`]，
//! 与装配件侧的 `/prod/assemblies/{id}/force-complete` 对称）；`cancel` 因判据不同**留下**。
//! 移除记录与旧路径实测形态见本文件末尾的「2026-10-13 force-complete 硬切到 /prod 前缀」一节。
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
//!
//! ## 2026-10-13 force-complete 硬切到 /prod 前缀（移除记录，防后人重建）
//!
//! **硬切下线、**无 alias**：旧 `POST /api/v2/parts/{part_id}/force-complete`
//! → 新 `POST /api/v2/prod/parts/{part_id}/force-complete`
//! （挂载点 `prod::router()` 末尾的 `.nest("/parts", part::force_complete_router())`，
//! 工厂转发见本文件 [`force_complete_router`]）。
//!
//! **为什么搬**（判据：谁是主写入对象 + OCC 锚在哪张表）：该端点的唯一实质写入是
//! `force_complete_all_batches_for_part` —— 把该 part 的**全部非 CANCELLED 批次**强推
//! `COMPLETED`，`t_part.status` 只是同事务内由 `shared::batch::status` 派生出来的缓存、
//! 不是主写入对象 ⇒ 与「批次 / 队列动作」同属生产执行语义，与装配件侧那条
//! `POST /api/v2/prod/assemblies/{id}/force-complete`（2026-10-11，理由原文见
//! `src/modules/prod/mod.rs` 的模块 doc）**逐条同构**，故对称处理。
//!
//! **为什么 `cancel` 不跟着搬**：判据不同。`POST /parts/{part_id}/cancel` 的主写入是
//! `mark_part_cancelled`（写 `t_part.status`，OCC 锚在 `part.version`），级联取消批次
//! 只是副作用 ⇒ 留在 part 域的 `/parts/{id}/cancel`，本轮**一行未动**。判据不要
//! 简写成「凡是碰批次的都归 prod」—— 看的是**主写入对象**。
//!
//! **旧路径实测响应形态 = 干净 404、空 body**（钉在
//! `tests/part/lifecycle.rs::force_complete_old_part_path_is_gone`，实测钉死、不是推理）：
//! `force-complete` 段的第 2 段是**字面量**，本域全部 2 段路由的第 2 段也都是字面量
//! （`update` / `cancel` / `events` / …），而 `Path<i64>` catch-all `/{part_id}` 只有
//! **1 段**、够不着它 ⇒ matchit 无命中、`Path` extractor 根本没机会出手。
//! ⚠️ 同域的「1 段」一侧形态**不同**，别记混：`POST /parts/batch-with-pdfs`（1 段静态）
//! POST → **405**、GET → **400 纯文本**。段数是分水岭，但**方法**会改写形态。
//!
//! **契约逐字未变**：body `ForceCompleteRequest { note: Option<String> }`
//! （**无 `version`** —— 本端点属 OCC 豁免清单，见 CLAUDE.md「OCC」第 8 条的逃生口一档）；
//! 响应 `R<PartOut>`；错误码全在 `201xx`（`20101` 不存在 / `20119` 不可操作状态 /
//! `20123` `BIZ_PART_ALREADY_COMPLETED` 幂等拒）；角色守卫仍是 MANAGER **单角色**
//! （handler 首行 + service 入口各一道）；WS 仍是 commit 后 `PART_FORCE_COMPLETED`。
//!
//! **前端配套**：只需改 api 层**一处 URL 常量** ——
//! `frontend/src/api/parts/crud.ts` 的 `forceCompletePart()` 里那行
//! `` api.post(`/parts/${id}/force-complete`, …) `` 改成 `/prod/parts/${id}/…`；
//! 请求体（`ForceCompletePayload { note }`）/ 响应解析（`PartOutDto`）/ 错误码处理全部不动。
//! ⚠️ 前端仓另有一份**逐条 URL 守卫** `frontend/src/api/parts/__tests__/routes.spec.ts`，
//! 它对 force-complete **只有注释提及、没有断言** ⇒ 本次不需要改它；但它顶部「判据」
//! 一节把 force-complete 列为「留在 part 域」，建议顺手同步（纯注释，不影响测试）。

pub mod dto_crud;
pub mod handler;
pub mod model;
pub mod repo;
pub mod service;
// 2026-10-13：本域的 `statemachine` 子模块**整文件删除** —— 它的全部内容
// （`PartStatus` 枚举 + `can_transition_to` / `part_status_progress` /
// `compute_part_target` 及其单测）都是批次与工单**共用**的状态机纯函数，已上提到
// `crate::shared::batch::status_machine`。该文件原本不含任何 part 私有的状态判定。
pub mod vo;

use axum::{
    Router,
    routing::{get, post},
};
use std::sync::Arc;

use crate::state::AppState;

// 2026-09-29 修复：移除 2026-09-15 followup-cleanup A8 的兼容 nest。
//
// 2026-10-13：上面那条「part 维度文件路由（cad-files / cnc-programs / setup-sheets /
// cnc-pair / files）仅通过 part_file 域 canonical 第二入口
// `/api/v2/part-files/parts/{part_id}/<file>...` 对外暴露」的说明**已整体作废** ——
// 那条第二入口随 part_file 域收编进 files 域**整段下线**（连同
// `part_nested_router()` 一并删除；旧 URL **无 alias**，`/part-files/*` 一律 404）。
// ⚠️ 判定「某条 part 端点算不算 part_file 域的东西」时按**写不写 `t_part_file`** 判，
// 不是按「挂在哪个域」判 —— 这条口径**继续有效**，只是本域的命中项在 PR1b 之后
// 变成了**零条独立文件端点**（判定口径留给后续重组用，别连口径一起删掉）：
// - 与 part_file **无关**：`print-drawing` / `print-drawing-batch`（纯 BFF 转发到 python
//   出 PDF）、`batch-with-pdfs`（PDF 只喂 `lopdf` 数页数、不入库不上 COS）。
// - 与 part_file **相关**（PR1b 前）：`upload-drawing` / `upload-3d-model`（虽挂在 part
//   域却写 `t_part_file`）、`files/confirm`（STS 直传回执）。二者已分别并入
//   `POST /api/v2/files/objects` 与硬切到 `POST /api/v2/files/objects/confirm`。
// - ★ **2026-10-15：本域已零 `t_part_file` 写入**。`POST /parts/batch` 的 bindings
//   分支（`items[].drawing_file` / `model3d_file` + `prepare_binding_head_copy`）
//   随统一上传链路上线整条删除，该端点回归**纯建单**；零件文件一律走
//   `files::part_file` 的 upload-intents → 前端直传 → confirm（owner = 建单后拿到的
//   part_id）。`assemblies` 侧的 multipart PDF 仍在本域（只喂 `lopdf` 数页数、
//   **不入库不上 COS**）。
// 下线的 7 条端点逐条清单见 `src/modules/files/part_file/handler.rs` 的模块 doc
// 「2026-10-13 同批下线的 7 条端点（移除记录，防后人重建）」一节；PR1b 的 3 条
// （upload-drawing / upload-3d-model / files/confirm）见同一文件的
// 「2026-10-13 PR1b：写路径搬进本子模块（上传四合一）」一节。

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
        // 2026-10-03 新增：批量零件图纸打印（纯 BFF 转发到 python 执行 PDF 光栅化 +
        // pikepdf 合并）。★静态段必须在 /{part_id}/... catch-all 之前注册。
        .route(
            "/print-drawing-batch",
            post(handler::print::print_drawing_batch),
        )
        // ⚠️ 2026-10-13：`/batch-with-pdfs`（1 段静态）与 `/by-work-type/{id}`
        // （2 段）两条零消费方端点整条下线，逐条移除记录见 `handler/batch.rs` 与
        // `handler/lifecycle.rs` 的模块 doc。⚠️ **两条的旧路径响应形态不同，且
        // `batch-with-pdfs` 的 POST / GET 也不同**（实测钉死，见各自的守卫用例）：
        // - `GET /parts/by-work-type/{id}` → **干净 404**（2 段够不着 1 段 catch-all；
        //   本 router 全部 2 段路由的第二段都是字面量，没有 `/{part_id}/{param}`）。
        // - `POST /parts/batch-with-pdfs` → **405**（落进下面的 `/{part_id}`，而那条
        //   **只注册 GET** ⇒ matchit 先判方法，`Path<i64>` 压根没被调用）。
        // - `GET /parts/batch-with-pdfs` → **400 纯文本**（方法对上后才轮到
        //   `Path<i64>` 解不出整数）。
        // ---- 单件 {part_id} ----
        .route("/{part_id}", get(handler::get_part_detail))
        .route("/{part_id}/update", post(handler::update_part))
        .route("/{part_id}/soft-delete", post(handler::soft_delete_part))
        // 2026-10-03 新增：单件零件图纸打印（图纸正面 + 条码背面的双面 PDF）。
        // 纯 BFF 转发到 python `GET /api/v1/parts/{id}/print`（v2/v1 路径不同名）。
        .route(
            "/{part_id}/print-drawing",
            get(handler::print::print_drawing),
        )
        // BATCH-N：翻转该 part 全部活跃批次 → part CANCELLED（留 part 域）
        .route("/{part_id}/cancel", post(handler::cancel))
        // ⚠️ 2026-10-13：`/{part_id}/force-complete` **自本 router 摘除**，硬切到
        // `POST /api/v2/prod/parts/{part_id}/force-complete`（`/prod` 前缀下，与装配件侧
        // 的 `/prod/assemblies/{id}/force-complete` 对称）。理由、移除记录与实测响应
        // 形态见本文件模块 doc 的「2026-10-13 force-complete 硬切到 /prod 前缀」一节。
        .route("/{part_id}/events", get(handler::list_part_events))
        // part 的批次集合读（留 part 域：操作对象是「该 part 的批次集合」）
        .route("/{part_id}/batches", get(handler::list_part_batches))
        // ---- 2026-09-25 D-08 api-drift-fix：按 part 反查所属装配体 ----
        .route("/{part_id}/assembly", get(handler::get_assembly_by_part))
    // ---- 2026-10-13：part 维度的全部文件端点下线（三批，见上方模块级注记）----
    //   ① PR1a：cad-files / cnc-programs / setup-sheets / cnc-pair / files 共 **7** 条
    //      （2026-10-13 review 第 1 轮 NIT-9 订正：原文写 8 是把同批复活的
    //      `POST /part-files/` 数了进去）；
    //   ② PR1b：`upload-drawing` / `upload-3d-model` 并入
    //      `POST /api/v2/files/objects`（四合一）；
    //   ③ PR1b：`files/confirm` 硬切到 `POST /api/v2/files/objects/confirm`。
    //   三批**均无 alias，旧路径 404**；移除记录见
    //   `src/modules/files/part_file/handler.rs` 的模块 doc。
    // ⚠️ ② 的**响应形状与状态码也变了**（2026-10-13 review 第 1 轮 MINOR-5 登记，
    // 此前 part 侧这处变更三处 doc 都查不到）：HTTP 200 → **201**；
    // 出参 `R<TPartFile>` → `R<PartFileOut>`；owner 键 `part_id` → `owner_id`
    // （并新增 `owner_kind` 回显）；丢 `updated_at` / `updated_by` / `deleted_at`。
    // 逐字段对照见 `files/part_file/mod.rs` 的「零件侧上传的响应形状与状态码变更」一节。
    // ★ **2026-10-15：`POST /parts/batch` 的 bindings 分支已删除**（与上面的
    // 「响应形状与状态码变更」一起成为历史）—— 该端点现在**不碰** `t_part_file`，
    // 回归纯建单；响应里的 `cleanup_tmp_keys` 字段保留但恒为空（形状兼容）。
}

/// part 域的**生产链路逃生端点**子路由（不含公共前缀；由 `prod::router()` 桥接到
/// `/prod/parts`）。
///
/// 2026-10-13 新增，当前只含 1 条（自本域 [`router`] 摘出、硬切过来）：
/// - `POST /{part_id}/force-complete` —— MANAGER 单角色强推该 part 全部非
///   CANCELLED 批次为 `COMPLETED`（绕状态机、不走 OCC）。
///
/// **本域因此有 2 处挂载**：上面的 [`router`]（`modules::v2_router()` 的顶层
/// `/api/v2/parts` nest，域本体端点一行未动）+ 本工厂（`prod::router()` 里的
/// `/api/v2/prod/parts` nest）。归属判据与「为什么 `cancel` 不跟着搬」见本文件模块
/// doc 的「2026-10-13 force-complete 硬切到 /prod 前缀」一节。
pub fn force_complete_router() -> Router<Arc<AppState>> {
    handler::force_complete_router()
}

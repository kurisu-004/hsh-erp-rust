//! `files::part_file` 子模块 HTTP handler（2026-09-14 Phase 3 + 2026-09-15 takeover-fill
//! + followup-cleanup + 2026-09-16 M2-B 业务层 + 2026-09-18 upload-intents 删除
//! + 2026-09-22 对齐 iam 范式 + 2026-10-13 收编进 files 域）
//!
//! 对应 Python myERP/api/v1/part_file.py。
//!
//! ## 端点（4 条，挂在 `/api/v2/files/objects`，由 `files::handler::router()` 的
//! `.nest("/objects", …)` 桥接）
//!   - `GET  /`                    —— 列表查询 + 分页（`owner_kind` / `owner_id` / `kind` 过滤）
//!   - `GET  /{file_id}/url`       —— 单条详情 + COS 预签下载 URL
//!   - `GET  /{file_id}/content`   —— 后端代理文件内容
//!   - `POST /{file_id}/delete`    —— 软删 + COS 异步清理
//!
//! ⚠️ 列表端点在 wire 上是 `GET /api/v2/files/objects`（**无尾斜杠**）：axum 0.8 的
//! `nest` 把内层 `.route("/", …)` 注册成 `<prefix>` 本身。迁移前的
//! `GET /api/v2/part-files/` 同样是 404（行为未变），详见 `../../mod.rs` 模块 doc
//! 的尾斜杠一节。
//!
//! 2026-10-13：**自独立 `part_file` 域收编进 `files` 域**，URL 由
//! `/api/v2/part-files/*` 硬切到 `/api/v2/files/objects/*`（**无 alias，旧路径 404**）。
//!
//! ## ⚠️ 写路径不在本子模块
//! 本子模块只提供**读 + 软删**。HTTP 上的上传走两条路：
//! - 直传链路（前端实际用的）：STS 直签 → COS → part 域
//!   `POST /api/v2/parts/{part_id}/files/confirm`（入参 `ConfirmFileIn`，service 走
//!   `PartFileService::bind_uploaded_file`）
//! - `PartFileService::upload_file_for_owner` 作为**通用上传原语**保留（当前只有
//!   集成测试在用，HTTP 端点已下线，见 step 5 的 doc 注记）
//!
//! ## 2026-10-13 同批下线的 8 条端点（移除记录，防后人重建）
//!
//! 原「零件维度嵌套」第二入口 `/part-files/parts/{part_id}/<file>...` 整段下线
//! （`part_nested_router()` 一并删除），已核实零前端调用方：
//!
//! | 旧 URL | 方法 | 处置 |
//! |---|---|---|
//! | `/part-files/` | `POST` | 删（上传走 STS 直传 + part 域 confirm） |
//! | `/part-files/parts/{part_id}/files` | `GET` | 删（等价于 `GET /files/objects?owner_kind=PART&owner_id=…`） |
//! | `/part-files/parts/{part_id}/cad-files` | `POST` | 删 |
//! | `/part-files/parts/{part_id}/cnc-programs` | `POST` + `GET` | 删 |
//! | `/part-files/parts/{part_id}/setup-sheets` | `POST` + `GET` | 删 |
//! | `/part-files/parts/{part_id}/cnc-pair` | `POST` | 删（与 `POST /files/cnc/pairs` 重复） |
//!
//! 连带删除的死代码：`read_part_file_multipart`（private helper，4 个调用方全删）、
//! `UploadFileData`（multipart `data` JSON 字段）、`PartFilesQuery`。
//!
//! ## 事务边界（2026-09-22 重构对齐 iam 范式，2026-10-13 收编时不变）
//! handler 负责 `pool.begin()` / `tx.commit()` —— service 不知事务：
//! - ① **写 + post-commit 副作用**（soft_delete_part_file）：`pool.begin()` →
//!   `state.part_file_service.soft_delete_file(&mut tx, ...)` → 拿到 object_key 后
//!   `tx.commit()` → handler `tokio::spawn(cos.delete_object(...))`（review A2 约定：
//!   commit 在前，spawn 在后）。
//! - ② **读端点**（list_part_files / get_part_file_url / get_part_file_content）：
//!   `pool.begin()` → service 跑查询 → `tx.commit()`。
//!
//! service 仅业务逻辑（方法签名 `<R: PartFileRepoTrait>(&self, mut repo: R, ...)`，
//! `PartFileRepoTrait` 已对 `&mut PgConnection` 实现），handler/service 借
//! `&mut *tx` / `&mut *conn` 喂给 trait。
//!
//! ## 约束
//! - 统一响应信封：`Result<Json<R<T>>, AppError>`。
//! - 权限在 service 层（`current.require_any_role(...)` 守卫）。
//! - WS 广播：本子模块不上报 WS 事件（文件资产是只读引用）。

use std::sync::Arc;

use axum::{
    Json, Router,
    body::Body,
    extract::{Path, Query, State},
    http::{StatusCode, header},
    response::Response,
    routing::{get, post},
};

use crate::auth::rbac::CurrentUser;
use crate::modules::files::part_file::dto::{DeletePartFileRequest, PartFileListQuery};
use crate::modules::files::part_file::vo::{PartFileListOut, PartFileWithUrlOut};
use crate::shared::error::AppError;
use crate::shared::response::R;
use crate::state::AppState;

/// `GET /api/v2/files/objects` → 200 OK
///
/// 权限：`PartFileService::list_files` 的 5 角色（Manager / Clerk / Inspector /
/// CncProgrammer / **ShelfAccount**，2026-10-10 新增 ShelfAccount 供报工台工控机预览图纸）。
/// 角色集合与 `get_part_file_content` **逐字相同**（含 `/url` 的差别见各端点 doc）。
pub async fn list_part_files(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Query(query): Query<PartFileListQuery>,
) -> Result<Json<R<PartFileListOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = state
        .part_file_service
        .list_files(&mut *tx, &query, &current)
        .await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// `GET /api/v2/files/objects/{file_id}/url` → 200 OK
///
/// 权限：`PartFileService::get_file_with_url` 的 4 角色（Manager / Clerk / Inspector /
/// CncProgrammer）—— **刻意不含 `ShelfAccount`**。本端点回的是 COS 预签直链
/// （`url_expires_in_seconds = 3600`），拿到即可脱离本后端直接访问、直链可外传；
/// 工控机账号的图纸预览走 `get_part_file_content`（后端代理、逐次过 RBAC）即可。
/// 详见 `part_file/mod.rs` 模块 doc 的角色矩阵。
pub async fn get_part_file_url(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(file_id): Path<i64>,
) -> Result<Json<R<PartFileWithUrlOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = state
        .part_file_service
        .get_file_with_url(
            &mut *tx,
            state.cos.clone(),
            &state.config.cos.upload_prefix,
            file_id,
            &current,
        )
        .await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// `GET /api/v2/files/objects/{file_id}/content` —— 后端代理文件二进制流。
///
/// 拉 `t_part_file.object_key` + COS `get_object`，透传 `content_type` 头。
/// 权限：`PartFileService::get_file_content` 的 5 角色（Manager / Clerk / Inspector /
/// CncProgrammer / **ShelfAccount**，2026-10-10 新增 ShelfAccount 供报工台工控机预览图纸）。
///
/// ⚠️ 与 `get_part_file_url` **角色集合相同但风险面不同**，`/url` 刻意不含 ShelfAccount：
/// 本端点是**后端代理**（每次都过 RBAC）；`/url` 回的是 **COS 预签直链（1 小时有效）**，
/// 拿到即可脱离本后端直接访问、直链可外传 ⇒ 工控机账号不需要它（预览走 content 就够），
/// 不因「同一个 owner 的两个读端点」而一并放开。
///
/// 2026-09-29 扁平化：service 内部走 legacy→新模板 fallback（见
/// `resolve_effective_key`），handler 仅透传 `cfg.upload_prefix`。
pub async fn get_part_file_content(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(file_id): Path<i64>,
) -> Result<Response, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = state
        .part_file_service
        .get_file_content(
            &mut *tx,
            state.cos.clone(),
            &state.config.cos.upload_prefix,
            file_id,
            &current,
        )
        .await?;
    tx.commit().await?;
    let resp = Response::builder()
        .status(StatusCode::OK)
        .header(
            header::CONTENT_TYPE,
            out.content_type
                .unwrap_or_else(|| "application/octet-stream".to_string()),
        )
        .header(header::CONTENT_LENGTH, out.bytes.len())
        // 2026-09-29 新增：图纸 / 3D 预览 20 min 缓存语义，与前端 TanStack
        // usePartFilesListQuery staleTime 对称（浏览器 HTTP 缓存接管重复请求）。
        .header(header::CACHE_CONTROL, "private, max-age=1200")
        .body(Body::from(out.bytes))
        .map_err(|e| AppError::internal(format!("response build: {e}")))?;
    Ok(resp)
}

/// `POST /api/v2/files/objects/{file_id}/delete` —— 软删 + COS 异步清理。
///
/// 入参（JSON）：`{ version: i32 }`（OCC）。
/// 权限：按 kind 派生角色（DRAWING / 3D_MODEL / CAD_2D / SETUP_SHEET → M+C；
/// G_CODE → M+CNC）。
///
/// 2026-09-15 review A2 修：service 返回 `object_key` 后，handler 必须
/// 先 `tx.commit()` 再 `tokio::spawn(cos.delete_object(...))`，避免
/// commit 失败却已触发 COS 删除、产生孤儿对象。
pub async fn soft_delete_part_file(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(file_id): Path<i64>,
    Json(req): Json<DeletePartFileRequest>,
) -> Result<Json<R<()>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let object_key = state
        .part_file_service
        .soft_delete_file(&mut *tx, file_id, req.version, &current)
        .await?;
    // commit 在前：DB 已是最终态，再触发副作用
    tx.commit().await?;
    // commit 后再异步触发 COS 删除（best-effort，失败仅 warn）
    let cos = state.cos.clone();
    tokio::spawn(async move {
        if let Err(e) = cos.delete_object(&object_key).await {
            tracing::warn!(
                key = %object_key,
                error = %e,
                "part_file COS 异步清理失败（已软删，不影响 API 返回）"
            );
        }
    });
    Ok(Json(R::ok_empty()))
}

/// `files::part_file` 子模块 axum 子路由。
///
/// 2026-10-13 起只剩 4 条（原 `/` 的 `POST` 上传与整段 `/parts/{part_id}` 嵌套入口
/// 随零调用方清理下线）；`.nest("/parts/{part_id}", part_nested_router())` 一并删除。
pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/", get(list_part_files))
        .route("/{file_id}/url", get(get_part_file_url))
        .route("/{file_id}/content", get(get_part_file_content))
        .route("/{file_id}/delete", post(soft_delete_part_file))
}

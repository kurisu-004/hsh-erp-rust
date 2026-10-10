//! `files::part_file` 子模块 HTTP handler（2026-09-14 Phase 3 + 2026-09-15 takeover-fill
//! + followup-cleanup + 2026-09-16 M2-B 业务层 + 2026-09-18 upload-intents 删除
//! + 2026-09-22 对齐 iam 范式 + 2026-10-13 收编进 files 域）
//!
//! 对应 Python myERP/api/v1/part_file.py。
//!
//! ## 端点（6 条，挂在 `/api/v2/files/objects`，由 `files::handler::router()` 的
//! `.nest("/objects", …)` 桥接）
//!   - `POST /`                    —— 单文件上传（multipart：`data` JSON + `file` 二进制）
//!   - `GET  /`                    —— 列表查询 + 分页（`owner_kind` / `owner_id` / `kind` 过滤）
//!   - `POST /confirm`             —— STS 直传回执：把 tmp 对象绑定到 owner
//!   - `GET  /{file_id}/url`       —— 单条详情 + COS 预签下载 URL
//!   - `GET  /{file_id}/content`   —— 后端代理文件内容
//!   - `POST /{file_id}/delete`    —— 软删 + COS 异步清理
//!
//! ⚠️ 列表 / 上传端点在 wire 上是 `POST|GET /api/v2/files/objects`（**无尾斜杠**）：
//! axum 0.8 的 `nest` 把内层 `.route("/", …)` 注册成 `<prefix>` 本身。迁移前的
//! `GET /api/v2/part-files/` 同样是 404（行为未变），详见 `../../mod.rs` 模块 doc
//! 的尾斜杠一节。
//!
//! 2026-10-13：**自独立 `part_file` 域收编进 `files` 域**，URL 由
//! `/api/v2/part-files/*` 硬切到 `/api/v2/files/objects/*`（**无 alias，旧路径 404**）。
//!
//! ## 2026-10-13 PR1b：写路径搬进本子模块（上传四合一）
//!
//! 本子模块此前只提供**读 + 软删**，上传与 confirm 分散在 part / assembly 两个域。
//! 本轮把**五个**旧入口收敛成**两个**：
//!
//! | 旧 URL | 新 URL | 处置 |
//! |---|---|---|
//! | `POST /api/v2/parts/{part_id}/upload-drawing` | `POST /api/v2/files/objects` | 四合一 |
//! | `POST /api/v2/parts/{part_id}/upload-3d-model` | `POST /api/v2/files/objects` | 四合一 |
//! | `POST /api/v2/assemblies/{assembly_id}/files` | `POST /api/v2/files/objects` | 四合一 |
//! | `POST /api/v2/part-files/`（PR1a 下线，本轮复活） | `POST /api/v2/files/objects` | 四合一 |
//! | `POST /api/v2/parts/{part_id}/files/confirm` | `POST /api/v2/files/objects/confirm` | 搬家 + body 加 `owner_kind` / `owner_id` |
//! | `GET /api/v2/assemblies/{assembly_id}/files` | — | **删**（0 调用方，与 `GET /files/objects?owner_kind=ASSEMBLY&owner_id=X` 等价） |
//!
//! ### ⚠️ `POST /files/objects` 的契约是**复用**旧 `POST /part-files/`，不是新设计
//!
//! multipart 形态逐字沿用：`data` 文本字段（`UploadFileData` JSON：
//! `{ owner_kind, owner_id, kind }`）+ `file` 二进制字段。三个 part / assembly 旧端点
//! 只是把 `owner_kind` / `owner_id` / `kind` 从 path 段 + 硬编码搬进了这个 JSON。
//!
//! ### ⚠️ 响应形状变更（assembly 上传）
//!
//! 旧 `POST /assemblies/{id}/files` 回 `Vec<AssemblyFileRef>`
//! （`{id, original_filename, page_count}`），本端点回 `PartFileOut`（18 字段）。
//! `page_count` 键消失，但上传路径本就硬编码 `page_count: None`
//! （`assembly/service/lifecycle.rs` 的上传分支），**无信息损失**。
//! 另一处行为变更见 `service.rs::upload_file_for_owner` 的去重语义对照表。
//!
//! ### ⚠️ matchit 注册顺序
//!
//! `/confirm` 是 3 段（`files/objects/confirm`），与 `/{file_id}/url` 等 4 段 catch-all
//! 段数不同，**天然无冲突**；`/`（0 段内的 root）同样不与之竞争。
//!
//! ## 2026-10-13 同批下线的 8 条端点（移除记录，防后人重建）
//!
//! 原「零件维度嵌套」第二入口 `/part-files/parts/{part_id}/<file>...` 整段下线
//! （`part_nested_router()` 一并删除），已核实零前端调用方：
//!
//! | 旧 URL | 方法 | 处置 |
//! |---|---|---|
//! | `/part-files/parts/{part_id}/files` | `GET` | 删（等价于 `GET /files/objects?owner_kind=PART&owner_id=…`） |
//! | `/part-files/parts/{part_id}/cad-files` | `POST` | 删（等价于 `POST /files/objects` + `kind=CAD_2D`） |
//! | `/part-files/parts/{part_id}/cnc-programs` | `POST` + `GET` | 删（G_CODE 走 `POST /files/cnc/pairs`） |
//! | `/part-files/parts/{part_id}/setup-sheets` | `POST` + `GET` | 删（同上） |
//! | `/part-files/parts/{part_id}/cnc-pair` | `POST` | 删（与 `POST /files/cnc/pairs` 重复） |
//!
//! 连带删除的死代码：`read_part_file_multipart`（private helper，4 个调用方全删）、
//! `PartFilesQuery`。
//!
//! ⚠️ 上表里的 `POST /part-files/` **不在本表**：它在本轮（PR1b）以
//! `POST /files/objects` 的形态**复活并通用化**了，别按本表把它当已删端点。
//!
//! ## 事务边界（2026-09-22 重构对齐 iam 范式，2026-10-13 收编时不变）
//! handler 负责 `pool.begin()` / `tx.commit()` —— service 不知事务：
//! - ① **写 + post-commit 副作用**（soft_delete_part_file）：`pool.begin()` →
//!   `state.part_file_service.soft_delete_file(&mut tx, ...)` → 拿到 object_key 后
//!   `tx.commit()` → handler `tokio::spawn(cos.delete_object(...))`（review A2 约定：
//!   commit 在前，spawn 在后）。
//! - ② **读端点**（list_part_files / get_part_file_url / get_part_file_content）：
//!   `pool.begin()` → service 跑查询 → `tx.commit()`。
//! - ③ **写端点**（upload_part_file）：`pool.begin()` → service → `tx.commit()`。
//! - ④ **confirm 端点**（confirm_file_object）：**handler 不开 tx** —— service
//!   `bind_uploaded_file` 自管事务（head/copy 是外部 IO，不该包在事务里），
//!   handler 只在 service 返回**已 commit** 之后 spawn 兜底清理 tmp 对象。
//!
//! service 仅业务逻辑（方法签名 `<R: PartFileRepoTrait>(&self, mut repo: R, ...)`，
//! `PartFileRepoTrait` 已对 `&mut PgConnection` 实现），handler/service 借
//! `&mut *tx` / `&mut *conn` 喂给 trait。
//!
//! ## 约束
//! - 统一响应信封：`Result<Json<R<T>>, AppError>`。
//! - 权限在 service 层（`current.require_any_role(...)` 守卫）；handler 侧只保留
//!   「权限守卫先于 multipart 解析」这条顺序纪律（避免未授权请求触发内存分配）。
//! - WS 广播：本子模块不上报 WS 事件（文件资产是只读引用）。

use std::sync::Arc;

use axum::{
    Json, Router,
    body::Body,
    extract::{Multipart, Path, Query, State},
    http::{StatusCode, header},
    response::Response,
    routing::{get, post},
};

use crate::auth::rbac::CurrentUser;
use crate::modules::files::part_file::dto::{
    ConfirmFileIn, DeletePartFileRequest, PartFileListQuery, UploadFileData,
};
use crate::modules::files::part_file::service::PartFileService;
use crate::modules::files::part_file::vo::{PartFileListOut, PartFileOut, PartFileWithUrlOut};
use crate::shared::error::{AppError, code};
use crate::shared::response::R;
use crate::state::AppState;

/// `POST /api/v2/files/objects` → 201 Created（四合一上传）
///
/// body（multipart）：
/// - `data`：必填文本字段，`UploadFileData` 的 JSON —— `{"owner_kind":"PART",
///   "owner_id":"<雪花id字符串>","kind":"DRAWING"}`
/// - `file`：必填二进制字段（PDF / STEP / STL …）
///
/// ⚠️ multipart 契约逐字沿用 PR1a 下线的 `POST /api/v2/part-files/`，不是新设计。
/// 2026-10-13 PR1b 起它同时承接 part 侧两条（`upload-drawing` / `upload-3d-model`）
/// 与 assembly 侧一条（`POST /assemblies/{id}/files`）的上传，三者只差 `data` 里
/// 的 `owner_kind` / `owner_id` / `kind`。
///
/// 权限：`PartFileService::upload_file_for_owner` 的 3 角色
/// （Manager / Clerk / CncProgrammer）—— 与三条旧端点**合并后取并集**：
/// 旧 `upload-drawing` / `upload-3d-model` 是 M+C（`CRUD_PART_ROLES`）、旧
/// `assemblies/{id}/files` 是 M+C、旧 multipart 端点是 M+C+CNC，故白名单含 CNC。
/// ⚠️ 这是四合一带来的**唯一一处权限面放大**：能传 CNC 程序的人现在也能传图纸 /
/// 装配件 PDF（旧 multipart 端点已经是这个白名单，本轮只是让另三条与之看齐）。
///
/// 权限守卫在读 multipart **之前**（避免未授权请求触发整包内存分配），与 part 域
/// 旧 handler 的 Finding B 纪律一致；service 内再校验一次（双层守卫）。
pub async fn upload_part_file(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    mut multipart: Multipart,
) -> Result<(StatusCode, Json<R<PartFileOut>>), AppError> {
    current.require_any_role(&[
        crate::auth::rbac::Role::Manager,
        crate::auth::rbac::Role::Clerk,
        crate::auth::rbac::Role::CncProgrammer,
    ])?;
    let mut data_json: Option<String> = None;
    let mut file_bytes: Option<Vec<u8>> = None;
    let mut file_name: Option<String> = None;
    let mut file_content_type: Option<String> = None;

    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| AppError::validation(format!("multipart 解析失败: {e}")))?
    {
        match field.name().unwrap_or("") {
            "data" => {
                data_json = Some(
                    field
                        .text()
                        .await
                        .map_err(|e| AppError::validation(format!("data 字段读取失败: {e}")))?,
                );
            }
            "file" => {
                file_name = field.file_name().map(|s| s.to_string());
                file_content_type = field.content_type().map(|s| s.to_string());
                let bytes = field
                    .bytes()
                    .await
                    .map_err(|e| AppError::validation(format!("file 字段读取失败: {e}")))?;
                file_bytes = Some(bytes.to_vec());
            }
            _ => {
                let _ = field.bytes().await;
            }
        }
    }

    let data_json =
        data_json.ok_or_else(|| AppError::biz(code::BIZ_INVALID_VALUE, "缺少 data 字段"))?;
    let data: UploadFileData = serde_json::from_str(&data_json)
        .map_err(|e| AppError::biz(code::BIZ_INVALID_VALUE, format!("JSON 解析失败: {e}")))?;
    // ⚠️ 雪花 id 一律是字符串（`shared::types` 的 serde helper 约定，防 JS 精度截断）
    let owner_id: i64 = data.owner_id.parse().map_err(|_| {
        AppError::biz(
            code::BIZ_INVALID_VALUE,
            format!("owner_id 非法: {}", data.owner_id),
        )
    })?;
    let bytes =
        file_bytes.ok_or_else(|| AppError::biz(code::BIZ_INVALID_VALUE, "缺少 file 字段"))?;
    let filename = file_name.ok_or_else(|| AppError::biz(code::BIZ_INVALID_VALUE, "缺少文件名"))?;
    let content_type = file_content_type.unwrap_or_else(|| "application/octet-stream".to_string());

    let mut tx = state.pool.begin().await?;
    let out = state
        .part_file_service
        .upload_file_for_owner(
            &mut *tx,
            &state.config.cos.upload_prefix,
            &data.owner_kind,
            owner_id,
            &data.kind,
            &filename,
            &content_type,
            bytes,
            &current,
        )
        .await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(R::ok(out))))
}

/// `POST /api/v2/files/objects/confirm` → 200 OK（STS 直传回执）
///
/// 2026-10-13 PR1b 自 part 域 `POST /api/v2/parts/{part_id}/files/confirm` 搬来
/// （**硬切、无 alias，旧路径 404**）。owner 由 path 段改为 body 的
/// `owner_kind` + `owner_id`（`ConfirmFileIn`），与 `POST /files/objects` 的
/// multipart `data` 字段对称。
///
/// 流程（service `bind_uploaded_file`）：head 校验 tmp 对象 + size → copy 到 CAS key
/// → tx 内 soft_delete 旧活跃行 + INSERT → commit → **handler 在 commit 之后**
/// spawn 异步 `delete_object(tmp_key)` 兜底清理（与 `soft_delete_part_file` 同款
/// 「commit 在前、spawn 在后」纪律）。
///
/// 权限：service 内 M+C（`bind_uploaded_file` 的 `require_any_role`），与旧端点逐字一致。
pub async fn confirm_file_object(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Json(req): Json<ConfirmFileIn>,
) -> Result<Json<R<PartFileOut>>, AppError> {
    let owner_id: i64 = req.owner_id.parse().map_err(|_| {
        AppError::biz(
            code::BIZ_INVALID_VALUE,
            format!("owner_id 非法: {}", req.owner_id),
        )
    })?;
    let (out, tmp_key) = PartFileService::bind_uploaded_file(
        &state.pool,
        &state.snowflake,
        state.cos.clone(),
        &state.config.cos.upload_prefix,
        &state.config.cos.tmp_prefix,
        &req.owner_kind,
        owner_id,
        &req.kind,
        &req.tmp_key,
        &req.content_sha256,
        &req.original_filename,
        req.file_size,
        &req.content_type,
        &current,
    )
    .await?;
    // commit 已由 service 内 `tx.commit()` 完成；这里 spawn 异步清理 tmp
    let cos = state.cos.clone();
    tokio::spawn(async move {
        if let Err(e) = cos.delete_object(&tmp_key).await {
            tracing::warn!(
                tmp_key = %tmp_key,
                error = %e,
                "confirm_file_object COS tmp 异步清理失败（已绑定到 part_file，不影响 API 返回）"
            );
        }
    });
    Ok(Json(R::ok(out)))
}

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
/// 2026-10-13 PR1b：`POST /` 的上传端点复活并通用化（与 part / assembly 侧三条旧
/// 端点四合一），`POST /confirm` 自 part 域搬入；`.nest("/parts/{part_id}",
/// part_nested_router())` 仍是 PR1a 删除后不复存在的形态。
pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/", get(list_part_files).post(upload_part_file))
        .route("/confirm", post(confirm_file_object))
        .route("/{file_id}/url", get(get_part_file_url))
        .route("/{file_id}/content", get(get_part_file_content))
        .route("/{file_id}/delete", post(soft_delete_part_file))
}

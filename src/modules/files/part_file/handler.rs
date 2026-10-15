//! `files::part_file` 子模块 HTTP handler（2026-09-14 Phase 3 + 2026-09-15 takeover-fill
//! + followup-cleanup + 2026-09-16 M2-B 业务层 + 2026-09-22 对齐 iam 范式
//! + 2026-10-13 收编进 files 域 + 2026-10-15 上传链路统一）
//!
//! 对应 Python myERP/api/v1/part_file.py。
//!
//! ## 端点（7 条，挂在 `/api/v2/files/objects`，由 `files::handler::router()` 的
//! `.nest("/objects", …)` 桥接）
//!   - `POST /upload-intents`        —— **批量**签发预签 PUT URL（纯读，无字节）
//!   - `POST /confirm`               —— 直传回执登记：软删旧活跃行 + INSERT +（可选）CNC 配对
//!   - `GET  /`                      —— 列表查询 + 分页（`owner_kind` / `owner_id` / `kind` 过滤）
//!   - `GET  /{file_id}/url`         —— 单条详情 + COS 预签下载 URL
//!   - `GET  /{file_id}/content`     —— 后端代理文件内容
//!   - `POST /{file_id}/delete`      —— 软删 + COS 异步清理
//!   - `GET  /{file_id}/preview`     —— COS CI 图纸预览（逻辑全在 `preview` 子模块，
//!     本文件只做路由 + 一次取行 + `?size=view|thumb|full` 解析）
//!
//! ⚠️ 列表 / 两条写入口在 wire 上是 `POST|GET /api/v2/files/objects` 与
//! `/api/v2/files/objects/upload-intents`（**无尾斜杠**）：axum 0.8 的 `nest` 把
//! 内层 `.route("/", …)` 注册成 `<prefix>` 本身。旧前缀 `/api/v2/part-files/*`
//! 一律 404（无 alias），详见 `../../mod.rs` 模块 doc 的尾斜杠一节。
//!
//! ## ⚠️ 本域**没有任何收字节的端点**
//!
//! 收文件链路恒为「签发 URL → 前端直传 COS → 登记」三步：
//!
//! ```text
//! ① POST /objects/upload-intents  → grant_upload_intents（纯读 + 逐项签 URL）
//! ② 前端 XHR PUT 直传 COS（字节不过 rust 进程）
//! ③ POST /objects/confirm        → bind_uploaded_file（head 校验 + 软删 + INSERT）
//! ```
//!
//! 已下线的收字节入口（**无 alias、旧路径 404**）：
//!
//! | 旧 URL | 处置 |
//! |---|---|
//! | `POST /api/v2/files/objects`（multipart 代理） | 删 —— 由上面三步承接 |
//! | `POST /api/v2/files/sts-tmp-keys`（python STS 转发） | 删 —— 预签 PUT 改在本仓签发 |
//! | `POST /api/v2/files/cnc/pairs`（一次收两文件做配对） | 删 —— 配对改由 confirm 的 `pair_with_file_id` 承接 |
//! | `POST /api/v2/parts/{part_id}/upload-drawing` / `upload-3d-model` | 删 —— 2026-10-13 并入通用上传原语 |
//! | `POST /api/v2/assemblies/{id}/files` | 删 —— 同上 |
//! | `POST /api/v2/part-files/` | 删 —— 同上 |
//! | `POST /api/v2/parts/{part_id}/files/confirm` | 硬切到 `POST /objects/confirm` |
//!
//! 2026-10-13 同批下线的「零件维度嵌套列表」入口（`part_nested_router()` 一并删除，
//! 已核实零前端调用方）：`/part-files/parts/{id}/files`（`GET`）、
//! `cad-files`（`POST`）、`cnc-programs`（`POST`+`GET`）、`setup-sheets`（`POST`+`GET`）、
//! `cnc-pair`（`POST`）。连带删除的死代码：`read_part_file_multipart`、`PartFilesQuery`。
//!
//! ### ⚠️ matchit 注册顺序：**无冲突**
//!
//! | 路由 | 段数（相对 nest 根） | 与其它路由的关系 |
//! |---|---|---|
//! | `/` | 0 | 段数不同 |
//! | `/upload-intents` | 1 | 段数不同 |
//! | `/confirm` | 1 | 段数不同（与上一条同为静态段、互不竞争） |
//! | `/{file_id}/url` · `/{file_id}/content` · `/{file_id}/delete` · `/{file_id}/preview` | 2 | 同段数、**同为 `{file_id}` 捕获段 + 第 2 段静态**；matchit 同层静态段优先，逐一不争 |
//!
//! ⚠️ **别只靠读源码判断**（CLAUDE.md 路由声明规约第 9 条的老踩坑就是「同段数静态段
//! 被 catch-all 吞成 400」）：实测钉死在
//! `tests/files/part_file.rs::upload_intents_route_is_not_swallowed_by_old_catch_all`
//! 与 `tests/files/preview.rs::preview_route_is_not_swallowed_by_sibling_catch_alls`。
//!
//! ## 事务边界
//!
//! - ① **写 + post-commit 副作用**（`soft_delete_part_file`）：`pool.begin()` →
//!   `soft_delete_file(&mut tx, ...)` → 拿到 `object_key` 后 `tx.commit()` → handler
//!   `tokio::spawn(cos.delete_object(...))`（2026-09-15 review A2 约定：commit 在前，
//!   spawn 在后）。
//! - ② **读端点**（`list_part_files` / `get_part_file_url` / `get_part_file_content`）：
//!   `pool.begin()` → service 跑查询 → `tx.commit()`。
//! - ③ **`grant_upload_intents`**：`pool.begin()` → service（纯读 + 逐项本地签名）
//!   → `tx.commit()`。⚠️ 批量上限 200（`service::UPLOAD_INTENTS_MAX_ITEMS`）⇒
//!   签名最多 200 次、DB 往返最多 200 次；签名是本地计算不发网。
//! - ④ **`confirm_file_object` / `get_part_file_preview`**：**handler 不开 tx** ——
//!   前者 service `bind_uploaded_file` 自管事务（`head_object` 是外部 IO，不该包在
//!   事务里），后者是预览状态机要跨多次 python HTTP 往返（service 走
//!   `pool.acquire()`）。
//!
//! service 仅业务逻辑（方法签名 `<R: PartFileRepoTrait>(&self, mut repo: R, ...)`，
//! `PartFileRepoTrait` 已对 `&mut PgConnection` 实现），handler/service 借
//! `&mut *tx` / `&mut *conn` 喂给 trait。
//!
//! ## 约束
//! - 统一响应信封：`Result<Json<R<T>>, AppError>`（两条取字节端点除外，回裸字节）。
//! - 权限在 service 层（`current.require_any_role(...)` 守卫）。
//! - ⚠️ **body 反序列化失败不在信封内**：两条写入口用 axum `Json` 提取器，`file_size`
//!   走 `deserialize_i64`（**只收 JSON 字符串**）⇒ 裸 number / 缺字段在提取器层就
//!   返 **HTTP 422 纯文本**，前端拿不到 `{code,message}`。两条端点都**不接**
//!   `serde(default)`，故缺字段同样 422。
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
use crate::modules::files::part_file::dto::{
    ConfirmFileIn, DeletePartFileRequest, PartFileListQuery, UploadIntentsIn,
};
use crate::modules::files::part_file::policy;
use crate::modules::files::part_file::preview::dto::{PreviewOut, PreviewQuery, PreviewSize};
use crate::modules::files::part_file::vo::{
    PartFileListOut, PartFileOut, PartFileWithUrlOut, UploadIntentsOut,
};
use crate::shared::error::{AppError, code};
use crate::shared::response::R;
use crate::state::AppState;

/// `POST /api/v2/files/objects/upload-intents` → 200 OK（签发预签 PUT URL）
///
/// 2026-10-15 新增，统一上传链路的第 ② 步：业务提交成功后拿 URL，前端 XHR PUT
/// 直传 COS（字节不过 rust 进程），再走 `POST /objects/confirm` 登记。
///
/// 权限：service 内 `grant_upload_intents` 的 `policy::write_roles_for(owner_kind,
/// WriteEntry::Upload)` —— PART/ASSEMBLY 是 M+C+CNC；CUSTOMER（送货单打印模板）
/// 仅 Manager。
///
/// 事务边界：handler `pool.begin()` → service（纯读 + 逐项本地签名）→
/// `tx.commit()`。与 `confirm_file_object`（service 自管 tx）相反：本端点不碰
/// 外部 IO 的事务持有，只是沿用本文件「handler 管 tx」的统一范式。
///
/// ## ⚠️ 「纯读」不等于「一次签名」
///
/// 批量上限 **200** 项（`service::UPLOAD_INTENTS_MAX_ITEMS`），逐项处理 ⇒
/// **最多 200 次** `presigned_put_url`（本地计算、不发网络请求）与**最多 200 次**
/// `get_by_owner_kind_sha` DB 往返（索引点查）。CAS 命中的项不签名。
/// ⇒ 一次请求在极端批量下会占住一个连接跑 200 次点查；这是**有意接受的**取舍
/// （一次多选几百个文件是「建单」量级的场景，不是常态）。
///
/// ## ⚠️ body 反序列化失败**不在 `R<T>` 信封内**
///
/// `Json<UploadIntentsIn>` 提取器对 `items[].file_size` 走 `deserialize_i64`
/// ——**只收 JSON 字符串**。传裸 number（`"file_size": 2048`）或缺字段 ⇒
/// **HTTP 422 纯文本**，前端 `envelopeResponseInterceptor` 拿不到
/// `{code, message}`。`owner_id` 同理（解析失败才有信封 20104，那是 handler 里的
/// `parse::<i64>`，不是 serde）。
/// 完整字段清单与前端配套改名见 `part_file/mod.rs` 的「前端配套改动清单」。
pub async fn grant_upload_intents(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Json(req): Json<UploadIntentsIn>,
) -> Result<Json<R<UploadIntentsOut>>, AppError> {
    let owner_id: i64 = req.owner_id.parse().map_err(|_| {
        AppError::biz(
            code::BIZ_INVALID_VALUE,
            format!("owner_id 非法: {}", req.owner_id),
        )
    })?;
    let mut tx = state.pool.begin().await?;
    let out = state
        .part_file_service
        .grant_upload_intents(
            &mut *tx,
            &state.config.cos.upload_prefix,
            state.config.cos.max_file_size,
            state.config.cos.upload_url_expire_seconds,
            &req.owner_kind,
            owner_id,
            &req.items,
            &current,
        )
        .await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// `POST /api/v2/files/objects/confirm` → 200 OK（直传回执登记）
///
/// 统一上传链路的第 ④ 步：前端已拿 `POST /objects/upload-intents` 的 URL 把字节
/// PUT 到 COS 正式 CAS key，本端点做「校验 → 软删旧活跃行 → INSERT」并可选地
/// 完成 CNC 配对（`pair_with_file_id`）。**handler 不做任何后置异步清理** ——
/// 没有 tmp 对象可清（对象从一开始就在正式 key 上）。
///
/// 权限：service 内 `bind_uploaded_file` 的 `policy::confirm_roles_for(owner_kind,
/// kind)` —— CNC 配对两种 kind（G_CODE / SETUP_SHEET）是 M+CNC，其余逐字沿用
/// `write_roles_for(Confirm)`（PART/ASSEMBLY 是 M+C；CUSTOMER 送货单打印模板
/// **仅 Manager**，且本端点**不拒** CUSTOMER —— 打印模板的写入走的就是它）。
///
/// ⚠️ 事务边界不变：service 自管 tx（`head_object` 是外部 IO，不进事务）。
///
/// ⚠️ body 反序列化失败**不在 `R<T>` 信封内**：`file_size` 走 `deserialize_i64`
/// （**只收 JSON 字符串**）⇒ 裸 number / 缺字段（含被硬切掉的旧 `tmp_key` 字段名）
/// 在 `Json` 提取器层就返 **HTTP 422 纯文本**。同款坑在 delete 端点的 `version`
/// 上已登记过（CLAUDE.md OCC 铁律）。
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
    let pair_with_file_id: Option<i64> = req
        .pair_with_file_id
        .as_deref()
        .filter(|s| !s.is_empty())
        .map(|s| {
            s.parse::<i64>().map_err(|_| {
                AppError::biz(
                    code::BIZ_INVALID_VALUE,
                    format!("pair_with_file_id 非法: {s}"),
                )
            })
        })
        .transpose()?;
    let out = state
        .part_file_service
        .bind_uploaded_file(
            &state.pool,
            state.cos.clone(),
            &state.config.cos.upload_prefix,
            // 2026-10-13 review 第 1 轮 MAJOR-1：原先 service 内写死 300MB，
            // `COS_MAX_FILE_SIZE` 解析了从没被这条路径读过；改为配置下发。
            state.config.cos.max_file_size,
            &req.owner_kind,
            owner_id,
            &req.kind,
            &req.object_key,
            &req.content_sha256,
            &req.original_filename,
            req.file_size,
            &req.content_type,
            pair_with_file_id,
            &current,
        )
        .await?;
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
/// 权限：`PartFileService::get_file_with_url` 的守卫是**权限点**
/// `part.drawing.print`（2026-10-14 随 RBAC 面板迁移；默认矩阵 Manager / Clerk /
/// Inspector / CncProgrammer，见 `seeds/rbac.sql`）—— **默认矩阵刻意不含
/// `ShelfAccount`**。本端点回的是 COS 预签直链（有效期由 `COS_PRESIGN_EXPIRE` 下发，
/// 代码缺省 3600s），
/// 拿到即可脱离本后端直接访问、直链可外传；工控机账号的图纸预览走
/// `get_part_file_content`（后端代理、逐次过 RBAC）即可。
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
            // 2026-10-15：此前这里写死 3600s，`COS_PRESIGN_EXPIRE` 解析了从没被读。
            state.config.cos.presign_expire_seconds,
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
/// 权限：`PartFileService::get_file_content_of_checked_row` 的 5 角色（Manager / Clerk / Inspector /
/// CncProgrammer / **ShelfAccount**，2026-10-10 新增 ShelfAccount 供报工台工控机预览图纸）。
///
/// ⚠️ 与 `get_part_file_url` **角色集合相同但风险面不同**，`/url` 刻意不含 ShelfAccount：
/// 本端点是**后端代理**（每次都过 RBAC）；`/url` 回的是 **COS 预签直链（默认 1 小时有效）**，
/// 拿到即可脱离本后端直接访问、直链可外传 ⇒ 工控机账号不需要它（预览走 content 就够），
/// 不因「同一个 owner 的两个读端点」而一并放开。
///
/// ⚠️ **2026-10-16 起两条取字节端点共用同一张 kind 白名单**
/// （[`policy::is_file_content_kind`]，判据与 fail-safe 方向见那里的 doc；
/// 2026-10-16 review 第 1 轮 MAJOR-2 把 `/url` 也纳入了同一张表 ——
/// 原先只守 content，CLERK / INSPECTOR / CNC 拿预签直链完全绕得过去）。
/// 判 kind 排在取字节**之前** ⇒ 非法 kind 零外部 IO（不拉 COS）。客户送货单打印模板
/// 要取字节请打 `print_template` 域的 `GET /print-templates/{id}/content`（那是**打印**
/// 语义、权限点 `delivery_note.print`，刻意与本端点的 5 角色集合不同）。
///
/// ⚠️ 2026-10-16 review 第 1 轮 MINOR-5：kind 判定与取字节**共用同一行**（行由本
/// handler 取一次，判过之后交给 `get_file_content_for_row`）—— 原先这里是
/// 「handler 取一次行判 kind → service 又 `get_by_id` 一次」，同一行同一谓词查两遍。
///
/// 2026-09-29 扁平化：service 内部走 legacy→新模板 fallback（见
/// `resolve_effective_key`），handler 仅透传 `cfg.upload_prefix`。
pub async fn get_part_file_content(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(file_id): Path<i64>,
) -> Result<Response, AppError> {
    let mut tx = state.pool.begin().await?;
    // ⚠️ 先取行判 kind 再取字节：非法 kind 零外部 IO（不拉 COS）。
    let row = state
        .part_file_service
        .get_file_row(&mut *tx, file_id)
        .await?;
    if !policy::is_file_content_kind(&row.kind) {
        return Err(AppError::biz(
            code::BIZ_PART_FILE_BAD_TYPE,
            format!(
                "kind={:?} 不走本端点取字节（仅 DRAWING / 3D_MODEL / G_CODE / SETUP_SHEET / ASSEMBLY_MASTER / CAD_2D）",
                row.kind
            ),
        ));
    }
    // 角色闸门仍在 service（`get_file_content_of_checked_row`）—— 本 handler 不重复
    // 那张 5 角色表，保持「一个端点一层闸门、且那层闸门在 service」的域内约定。
    let out = state
        .part_file_service
        .get_file_content_of_checked_row(
            state.cos.as_ref(),
            &state.config.cos.upload_prefix,
            &row,
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
/// 权限：按 kind 派生角色（DRAWING / 3D_MODEL / CAD_2D / SETUP_SHEET /
/// **ASSEMBLY_MASTER** → M+C；G_CODE → M+CNC）。`ASSEMBLY_MASTER` 是
/// 2026-10-13 review 第 1 轮 MINOR-6 补进分派的，详见 `service.rs::soft_delete_file`。
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

/// `GET /api/v2/files/objects/{file_id}/preview` —— COS CI 图纸预览（惰性轮询）。
///
/// 2026-10-13 PR2 新增。**唯一端点**是本子模块 `preview` 子模块的入口，
/// 其状态机 / seed 推导 / 安全取舍见 `part_file::preview` 的模块 doc。
///
/// ## 入参（2026-10-12 新增 `size`）
///
/// `?size=view|thumb|full`（可选，缺省 `view`）：选择 `pages[]` 尾部拼哪一档
/// 图片即时处理参数（值来自 `CosConfig` 的 `preview_image_*_params`；`full` 档
/// 恒无参数）。**大小写敏感**，非法值 ⇒ 40001 / HTTP 422。
///
/// ⚠️ 解析顺序：`size` 必须先于**任何 DB 读 / CI 副作用**（非法值不应读到文件行、
/// 更不应登记 `t_file_preview`）。⚠️ `size` 在 `PreviewQuery` 里是 `Option<String>`
/// 而不是枚举 —— 枚举放提取器层会让非法值走 `QueryRejection` 纯文本 400，
/// 破坏「错误走 `R<T>` 信封」的契约。
///
/// ## 事务边界
///
/// **handler 不开事务**（与本文件 `confirm_file_object` 同款「service 自管」的
/// 例外）：预览状态机要跨多次 python HTTP 往返，把整条链包在 PG 事务里会让连接
/// 被外部 IO 长时间占住。service 走 `pool.acquire()` 借一个连接跑完整条链。
///
/// ## 错误码
///
/// | 码 | HTTP | 条件 |
/// |---|---|---|
/// | 40001 | 422 | `size` 非法（非 `view` / `thumb` / `full`） |
/// | 40300 | 403 | 角色不在 M / C / I / CNC / **SHELF** 内 |
/// | 21101 | 404 | `file_id` 不存在（或已软删） |
/// | 21116 | 400 | 该文件不可预览（kind 非图纸类 / `file_type ≠ PDF`） |
/// | 21117 | 502 | CI job 终态 `Failed` |
/// | 21118 | 502 | 打 python / CI 转发失败（含 `NoopPyBackend` 占位） |
///
/// `PENDING` 走 **200 + `state: "PENDING"`**（不是 202）：它是正常中间态而非错误，
/// 详见 `preview::dto::PreviewOut` 的三态契约表。
///
/// ## ⚠️ matchit 注册顺序：**无冲突**（实测钉死）
///
/// `/{file_id}/preview` 是 2 段，与本 router 内其它路由逐一比对：
///
/// | 路由 | 段数 | 与 `/preview` 的关系 |
/// |---|---|---|
/// | `/` | 0 | 段数不同 |
/// | `/upload-intents` · `/confirm` | 1 | 段数不同 |
/// | `/{file_id}/url` | 2 | 段数相同，第 2 段同为静态 `url` |
/// | `/{file_id}/content` | 2 | 段数相同，第 2 段同为静态 `content` |
/// | `/{file_id}/delete` | 2 | 段数相同，第 2 段同为静态 `delete` |
/// | **`/{file_id}/preview`** | **2** | **同段数、同为 `{file_id}` 捕获段** |
///
/// 与 `/confirm`（1 段静态）的历史踩坑不同：这里四条 2 段路由的第 2 段**全都是
/// 字面量**、第 1 段**全都是捕获段**，matchit 对「同层静态段 vs 捕获段」按静态
/// 优先匹配，**没有歧义**。
///
/// 实测证据见 `tests/files/preview.rs::preview_route_is_not_swallowed_by_sibling_catch_alls`
/// —— 它逐条打 `/objects/{id}/preview` 与 `/objects/{id}/url`，两条都必须命中
/// 各自的 handler（不是 400 / 404）。**别只靠读源码判断**（CLAUDE.md 路由声明
/// 规约第 9 条的老踩坑就是「同段数静态段被 catch-all 吞成 400」）。
pub async fn get_part_file_preview(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(file_id): Path<i64>,
    Query(q): Query<PreviewQuery>,
) -> Result<Json<R<PreviewOut>>, AppError> {
    // ⚠️ `size` 解析必须在读文件行 / 任何 DB / CI 副作用**之前**（非法输入零代价
    // 拒绝；也让「非法值不产生 t_file_preview 行」成为结构性事实而不是巧合）。
    let size = PreviewSize::parse(q.size.as_deref())?;
    let image_params = size.image_params(
        &state.config.cos.preview_image_view_params,
        &state.config.cos.preview_image_thumb_params,
    );

    let file = {
        let mut tx = state.pool.begin().await?;
        let file = state
            .part_file_service
            .get_file_row(&mut *tx, file_id)
            .await?;
        tx.commit().await?;
        file
    };
    let out = state
        .preview_service
        .get_preview(
            &state.pool,
            &state.py_backend,
            &state.config.cos.preview_prefix,
            &state.config.cos.preview_base_url,
            image_params,
            &file,
            &current,
        )
        .await?;
    Ok(Json(R::ok(out)))
}

/// `files::part_file` 子模块 router 注册的全部路由（`METHOD /path`，相对
/// `/api/v2/files/objects`）。
///
/// 2026-10-13 review 第 1 轮 NIT 新增。此前本域的端点数（7）分散在
/// `src/modules/files/mod.rs` 的端点表、`CLAUDE.md` 的子模块清单、本文件 doc
/// 三处**手工维护** —— 下加一条端点必然漂移（且漂移的表现是「文档说有、实际没有」
/// 或反之，不报错）。本常量是那三处的**唯一真源**，`mod tests` 逐条比对
/// `router()` 源码，加/删端点忘同步会当场红。
///
/// 计数口径与全仓一致：一条 `.route(path, m1)` 记 1 条；
/// `.route(path, get(h).post(h))` 记 2 条。
pub const ROUTES: &[&str] = &[
    "GET /",
    "POST /upload-intents",
    "POST /confirm",
    "GET /{file_id}/url",
    "GET /{file_id}/content",
    "POST /{file_id}/delete",
    "GET /{file_id}/preview",
];

/// `files::part_file` 子模块 axum 子路由（完整路径 `/api/v2/files/objects/*`）。
///
/// 2026-10-15：multipart 上传端点下线，本 router **不再有任何收字节的路由**；
/// 写路径恒为 `POST /upload-intents` + `POST /confirm` 两步（字节由前端直传 COS）。
///
/// ⚠️ `/{file_id}/preview` 与 `/{file_id}/url` 等**位置平行**而非「先于」——
/// matchit 对同层静态段本就静态优先，顺序无关（实测钉死见
/// `get_part_file_preview` 的 doc 与对应集成测试）。
pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/", get(list_part_files))
        .route("/upload-intents", post(grant_upload_intents))
        .route("/confirm", post(confirm_file_object))
        .route("/{file_id}/url", get(get_part_file_url))
        .route("/{file_id}/content", get(get_part_file_content))
        .route("/{file_id}/delete", post(soft_delete_part_file))
        .route("/{file_id}/preview", get(get_part_file_preview))
}

#[cfg(test)]
mod tests {
    use super::ROUTES;
    use crate::modules::files::route_guard::routes_in_router_source;

    /// `ROUTES` 必须与 `router()` 源码**逐条**一致（不多、不少、同序）。
    ///
    /// 解析规则在 `crate::modules::files::route_guard`（本域 `part_file` /
    /// `files` 两个 router + 域外的 `print_template` 共用同一份实现）。
    ///
    /// ⚠️ 本表的计数口径：`"/"` 只有 `get(h)`（multipart 上传端点 2026-10-15 下线）
    /// ⇒ **记 1 条**；本表 7 条 = 7 条 `.route` 语句。
    #[test]
    fn routes_declared_in_router() {
        let actual = routes_in_router_source(include_str!("handler.rs"), "router");
        assert_eq!(
            actual.len(),
            ROUTES.len(),
            "`router()` 注册了 {} 条路由，ROUTES 只列了 {} 条（新增端点忘了登记？）\n\
             router(): {:?}\nROUTES: {:?}",
            actual.len(),
            ROUTES.len(),
            actual,
            ROUTES,
        );
        for (got, want) in actual.iter().zip(ROUTES.iter()) {
            assert_eq!(
                got,
                want,
                "`router()` 与 ROUTES 的第 {} 条不一致",
                ROUTES.len()
            );
        }
    }

    // ⚠️ **kind 白名单没有 lib 单测钉在这里**，因为它现在**不在 handler 里**：
    // 2026-10-16 review 第 1 轮 MAJOR-2 把它上提到 `policy::is_file_content_kind`
    // （service 的 `get_file_with_url` 也要判它，不能让 service 反向依赖 handler），
    // 两处判定点分别是：
    // - 本文件 `get_part_file_content`（判完把行交给 `get_file_content_of_checked_row`）
    // - `service::get_file_with_url`（判完才 `head_object` + `presigned_get_url`）
    //
    // 语义（白名单含哪些 kind、不含 DELIVERY_TEMPLATE）由
    // `policy::tests::is_file_content_kind_excludes_delivery_template_and_unknowns` 钉；
    // **接线**（两条端点真的都拒）由集成测试从 HTTP 外部钉：
    // `tests/print_template/resolve.rs::clerk_cannot_get_delivery_template_presigned_url`
    // （含对照组：同一 token 打图纸行仍 200）与
    // `::shelf_account_cannot_get_delivery_template_bytes`。
    // ⚠️ 别写「数源码里出现几次 `policy::is_file_content_kind`」这种自证式单测 ——
    // 断言字符串自己就会出现在同一文件里，count 恒 ≥ 1 ⇒ 恒真绿（review 第 1 轮
    // MAJOR-4 同一类陷阱，本轮实际踩过一次后已改掉）。
}

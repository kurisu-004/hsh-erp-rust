//! assembly 域 HTTP handler
//!
//! 对应 Python myERP/api/v1/assembly.py（设计 §7 — assembly CRUD）。
//!
//! ## 端点（挂在 `/api/v2/assemblies`，由 `mod.rs::router()` 桥接）
//! - `GET  /`                              —— 列表查询 + 分页
//! - `POST /`                              —— 创建（multipart：`data` + `files`）
//! - `GET  /{assembly_id}`                 —— 详情（含 children + files 占位）
//! - `POST /{assembly_id}/update`          —— 字段可选 UPDATE（OCC）
//! - `POST /{assembly_id}/soft-delete`     —— Manager 软删（OCC）
//! - `POST /{assembly_id}/cancel`          —— Manager/Clerk 取消
//! - `POST /{assembly_id}/start`           —— PENDING → IN_PROCESS 状态机守卫
//! - `POST /{assembly_id}/children`        —— 2026-09-25 新增 D-07：追加单个子件（事务）
//!
//! ### 2026-10-13 PR1b：两条 `/files` 端点下线（移除记录，防后人重建）
//!
//! | 旧 URL | 方法 | 处置 |
//! |---|---|---|
//! | `/api/v2/assemblies/{assembly_id}/files` | `POST` | 删 → 上传并入 `POST /api/v2/files/objects`（四合一，`owner_kind=ASSEMBLY` / `kind=ASSEMBLY_MASTER`） |
//! | `/api/v2/assemblies/{assembly_id}/files` | `GET` | 删 → 等价于 `GET /api/v2/files/objects?owner_kind=ASSEMBLY&owner_id={id}`（已核实 0 调用方） |
//!
//! 两条旧路径**均无 alias**（整段 `{assembly_id}/files` 从 router 上删除）。
//! ⚠️ `GET /assemblies/{id}` 详情里的 `files[]`（`AssemblyDetail.files`，类型仍是
//! `AssemblyFileRef`）**不受影响** —— 它走 `service/crud.rs` 的
//! `list_part_files_by_owner`，与被删的那条 list 端点是两条路径。
//!
//! ## 约定（2026-09-22 Group D-3 重构对齐 iam 范本）
//! - 事务边界在 handler：`state.pool.begin()` → service（trait 注入式签名
//!   `<R: AssemblyRepoTrait>(&self, mut repo: R, ...)` 收 `&mut tx`）→ 显式
//!   `tx.commit()`；提前 return 时 `Transaction` 的 Drop 自动回滚。
//! - 统一响应信封：`Result<Json<R<T>>, AppError>`。
//! - 权限在 service 层（`current.require_role(...)` 守卫）。
//! - WS 广播在 commit 之后（对齐 Python 延迟广播模式）；service 不直接调 ws_hub。
//!
//! ## handler 三形态
//! - ① 纯写端点：`pool.begin() → service → commit`，再广播。
//! - ② 读端点（list / get）：`pool.acquire() → service`，不开事务。
//! - ③ multipart 上传（create / files）：`pool.begin() → service → commit`；commit 后
//!   广播（仅 create）。

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Multipart, Path, Query, State},
    http::StatusCode,
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::json;

use crate::auth::rbac::CurrentUser;
use crate::infra::ws_hub::WsEvent;
use crate::modules::assembly::dto::{
    AssemblyChildAddRequest, AssemblyCreateRequest, AssemblyListQuery, AssemblyUpdateRequest,
    ForceCompleteRequest,
};
use crate::modules::assembly::service::AssemblyService;
use crate::modules::assembly::vo::{
    AssemblyCreateResult, AssemblyDetail, AssemblyListOut, AssemblyOut,
};
use crate::modules::part::vo::PartListItem;
use crate::shared::error::{AppError, code};
use crate::shared::response::R;
use crate::state::AppState;

/// 软删 / 取消的乐观锁版本号 body。
#[derive(Debug, Deserialize)]
pub struct VersionBody {
    pub version: i32,
}

/// GET /api/v2/assemblies
///
/// 列表查询 + 分页（service 内已校验角色）。读端点，不开事务。
///
/// 2026-09-22 D-3：handler 直接走 `AssemblyService::list_assemblies` 的 ZST 静态
/// wrapper（接受 `&mut PgConnection`），与既存集成测试一致。
pub async fn list_assemblies(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Query(query): Query<AssemblyListQuery>,
) -> Result<Json<R<AssemblyListOut>>, AppError> {
    let mut conn = state.pool.acquire().await?;
    let out = AssemblyService::list_assemblies(&mut conn, &query, &current).await?;
    Ok(Json(R::ok(out)))
}

/// GET /api/v2/assemblies/{assembly_id}
///
/// 单条详情（assembly 行 + children parts + files 占位空数组）。读端点，不开事务。
pub async fn get_assembly(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(assembly_id): Path<i64>,
) -> Result<Json<R<AssemblyDetail>>, AppError> {
    let mut conn = state.pool.acquire().await?;
    let out = AssemblyService::get_assembly(&mut conn, assembly_id, &current).await?;
    Ok(Json(R::ok(out)))
}

/// POST /api/v2/assemblies → 201 Created
///
/// multipart body：
/// - `data`：必填，文本字段，序列化后的 `AssemblyCreateRequest` JSON；
/// - `files` / `file`：可选，多个 PDF 二进制字段（首份会被 service 用作页数校验）。
///   两个字段名等价，都收；PDF 在本端点**不入库**。
///
/// 行为：
/// - 业务流转：service `create_assembly`（叶子 customer 校验 + 子件上限 +
///   PDF 页数 == children.len()+1 + serial 派发 + children INSERT）。
/// - WS 广播：commit 后 `ASSEMBLY_CREATED`。
/// - 响应：`AssemblyCreateResult { assembly, created_children }`。
pub async fn create_assembly(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    mut multipart: Multipart,
) -> Result<(StatusCode, Json<R<AssemblyCreateResult>>), AppError> {
    let mut data_json: Option<String> = None;
    let mut pdf_files: Vec<Vec<u8>> = Vec::new();
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
            // `files` / `file` 等价：只认其中一个名字的话，另一种拼写会被下面的
            // `_` 分支静默吞掉，表现为「PDF 没生效」而没有任何报错。
            "files" | "file" => {
                let bytes = field
                    .bytes()
                    .await
                    .map_err(|e| AppError::validation(format!("files 字段读取失败: {e}")))?;
                pdf_files.push(bytes.to_vec());
            }
            // 其它字段一律丢弃（与 Python 行为对齐）
            _ => {
                let _ = field.bytes().await;
            }
        }
    }
    let data_json =
        data_json.ok_or_else(|| AppError::biz(code::BIZ_INVALID_VALUE, "缺少 data 字段"))?;
    let req: AssemblyCreateRequest = serde_json::from_str(&data_json)
        .map_err(|e| AppError::biz(code::BIZ_INVALID_VALUE, format!("JSON 解析失败: {e}")))?;

    let mut tx = state.pool.begin().await?;
    let out =
        AssemblyService::create_assembly(&mut tx, &state.snowflake, &req, pdf_files, &current)
            .await?;
    tx.commit().await?;
    // commit 之后广播（对齐 Python 延迟广播模式）
    state.ws_hub.broadcast(WsEvent::DashboardEvent {
        kind: "ASSEMBLY_CREATED".into(),
        payload: json!({ "assembly_id": out.assembly.id.to_string() }),
    });
    Ok((StatusCode::CREATED, Json(R::ok(out))))
}

/// POST /api/v2/assemblies/{assembly_id}/update
///
/// 字段可选 UPDATE（含 customer_id 三态校验 + 叶子校验）；OCC 通过 `req.version` 守。
///
/// 行为：
/// - 权限：Manager / Clerk（service 内守卫）
/// - WS 广播：commit 后 `ASSEMBLY_UPDATED`
/// - 响应：`AssemblyOut`
pub async fn update_assembly(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(assembly_id): Path<i64>,
    Json(req): Json<AssemblyUpdateRequest>,
) -> Result<Json<R<AssemblyOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = AssemblyService::update_assembly(&mut tx, assembly_id, &req, &current).await?;
    tx.commit().await?;
    state.ws_hub.broadcast(WsEvent::DashboardEvent {
        kind: "ASSEMBLY_UPDATED".into(),
        payload: json!({ "assembly_id": out.id.to_string() }),
    });
    Ok(Json(R::ok(out)))
}

/// POST /api/v2/assemblies/{assembly_id}/soft-delete
///
/// Manager 专属软删；OCC 守；commit 后广播 `ASSEMBLY_DELETED`。
pub async fn soft_delete_assembly(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(assembly_id): Path<i64>,
    Json(req): Json<VersionBody>,
) -> Result<Json<R<()>>, AppError> {
    let mut tx = state.pool.begin().await?;
    AssemblyService::soft_delete_assembly(&mut tx, assembly_id, req.version, &current).await?;
    tx.commit().await?;
    state.ws_hub.broadcast(WsEvent::DashboardEvent {
        kind: "ASSEMBLY_DELETED".into(),
        payload: json!({ "assembly_id": assembly_id.to_string() }),
    });
    Ok(Json(R::ok_empty()))
}

/// POST /api/v2/assemblies/{assembly_id}/cancel
///
/// Manager / Clerk；repo 按 `status NOT IN ('COMPLETED','CANCELLED')` 守卫，
/// 命中 0 行 → 终态禁 cancel（返回 `BIZ_INVALID_TRANSITION`）。
///
/// 行为：
/// - 权限：Manager / Clerk（service 内守卫）
/// - WS 广播：commit 后 `ASSEMBLY_CANCELLED`
/// - 响应：`AssemblyOut`
pub async fn cancel_assembly(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(assembly_id): Path<i64>,
) -> Result<Json<R<AssemblyOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = AssemblyService::cancel_assembly(&mut tx, assembly_id, &current).await?;
    tx.commit().await?;
    state.ws_hub.broadcast(WsEvent::DashboardEvent {
        kind: "ASSEMBLY_CANCELLED".into(),
        payload: json!({ "assembly_id": out.id.to_string() }),
    });
    Ok(Json(R::ok(out)))
}

/// `POST /api/v2/assemblies/{assembly_id}/start` → 200 OK
///
/// 2026-09-14 Phase 3（deferred #4）：PENDING → IN_PROCESS 状态机守卫。
pub async fn start_assembly(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(assembly_id): Path<i64>,
) -> Result<Json<R<AssemblyOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = AssemblyService::start_assembly(&mut tx, assembly_id, &current).await?;
    tx.commit().await?;
    state.ws_hub.broadcast(WsEvent::DashboardEvent {
        kind: "ASSEMBLY_UPDATED".into(),
        payload: json!({ "assembly_id": out.id.to_string() }),
    });
    Ok(Json(R::ok(out)))
}

/// `POST /api/v2/assemblies/{assembly_id}/children` → 200 OK
///
/// 2026-09-25 新增（D-07 api-drift-fix）：在已存在装配体下追加单个 part 子件。
///
/// 行为：
/// - 权限：Manager / Clerk（service 内 `require_any_role`）
/// - 事务：handler `pool.begin()` → service（trait 注入式）→ 显式 `tx.commit()`
///   同事务 INSERT `t_part` + INSERT 初始 `t_part_batch`
/// - 业务流转：service 层
///   1. 校验 `drawing_no` / `name` 非空，`quantity > 0`
///   2. 装配体存在性（→ 20301）
///   3. 子件继承父件 7 个共享字段；`planned_delivery_date` 子件入参优先，缺省继承
///   4. 子件 `customer_id` = 父件 `customer_id`；`serial_no = NULL`
/// - WS 广播：commit 后 `ASSEMBLY_UPDATED`（payload `{ assembly_id }`）
/// - 响应：`PartListItem`（含 `customer_name` / `l1_customer_name` 冗余）
pub async fn add_assembly_child(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(assembly_id): Path<i64>,
    Json(req): Json<AssemblyChildAddRequest>,
) -> Result<Json<R<PartListItem>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out =
        AssemblyService::add_assembly_child(&mut tx, &state.snowflake, assembly_id, &req, &current)
            .await?;
    tx.commit().await?;
    state.ws_hub.broadcast(WsEvent::DashboardEvent {
        kind: "ASSEMBLY_UPDATED".into(),
        payload: json!({ "assembly_id": assembly_id.to_string() }),
    });
    Ok(Json(R::ok(out)))
}

/// `POST /api/v2/prod/assemblies/{assembly_id}/force-complete` → 200 OK
///
/// 2026-10-11 新增。MANAGER **单角色**强推装配件 + 全部子件 + 全部非 CANCELLED
/// 批次为 `COMPLETED`（绕状态机），用于「实际早已送货、但没在系统录入」的工单收口。
///
/// 路径挂在 `/api/v2/prod/assemblies/*`（**不是** `/api/v2/assemblies/*`）：它是生产
/// 链路收口的逃生端点，与 `prod` 下其余批次 / 队列动作同属「生产执行」语义；装配件域
/// 本体的 8 条 CRUD / 状态机端点仍在 `modules::v2_router` 的顶层 nest。
/// 详见 `modules::prod::router()` 的挂载注释。
///
/// 行为：
/// - 事务：handler `pool.begin()` → service（`&mut *tx`）→ 显式 `tx.commit()`
/// - WS 广播：commit 之后 1 条 `ASSEMBLY_FORCE_COMPLETED` + 每个被改动的子件各 1 条
///   既有 `PART_FORCE_COMPLETED`（雪花 id 字符串化，雪花 ID 不能当 JSON 数字传）
/// - 响应：`AssemblyOut`
pub async fn force_complete_assembly(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(assembly_id): Path<i64>,
    Json(req): Json<ForceCompleteRequest>,
) -> Result<Json<R<AssemblyOut>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let (out, forced_part_ids) =
        AssemblyService::force_complete(&mut tx, &state.snowflake, assembly_id, &req, &current)
            .await?;
    tx.commit().await?;
    state.ws_hub.broadcast(WsEvent::DashboardEvent {
        kind: "ASSEMBLY_FORCE_COMPLETED".into(),
        payload: json!({ "assembly_id": assembly_id.to_string() }),
    });
    for part_id in forced_part_ids {
        state.ws_hub.broadcast(WsEvent::DashboardEvent {
            kind: "PART_FORCE_COMPLETED".into(),
            payload: json!({ "part_id": part_id.to_string() }),
        });
    }
    Ok(Json(R::ok(out)))
}

/// 装配件域**生产链路逃生端点**子路由（不含公共前缀；由 `mod.rs::force_complete_router()`
/// 转发、`prod::router()` 桥接到 `/prod/assemblies`）。
///
/// 与 [`router`] 是两个独立工厂：前者给装配件域本体的 8 条（顶层 `/api/v2/assemblies`），
/// 后者只含强制完成这一条（`/api/v2/prod/assemblies`）。前缀并存是刻意的端点级归属裁决。
pub fn force_complete_router() -> Router<Arc<AppState>> {
    Router::new().route(
        "/{assembly_id}/force-complete",
        post(force_complete_assembly),
    )
}

/// assembly 域 axum 子路由（不含公共前缀；由 `mod.rs::router()` 桥接到 `/assemblies`）。
pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/", get(list_assemblies).post(create_assembly))
        .route("/{assembly_id}", get(get_assembly))
        .route("/{assembly_id}/update", post(update_assembly))
        .route("/{assembly_id}/soft-delete", post(soft_delete_assembly))
        .route("/{assembly_id}/cancel", post(cancel_assembly))
        .route("/{assembly_id}/start", post(start_assembly))
        .route("/{assembly_id}/children", post(add_assembly_child))
}

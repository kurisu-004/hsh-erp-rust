//! delivery_note 域打印 handler（BFF 转发 + 套数注入）
//!
//! 2026-10-03：2 个端点改为**纯转发**——`POST /delivery-notes/{id}/print` 与
//! `POST /delivery-notes/{id}/print-labels` 只做「鉴权 + 闸门 + 转发」，渲染动作
//! （模板填表 / 标签生成）全在 python 侧执行。
//!
//! 2026-10-04：转发前**读本单批次算装配件可出货套数**并注入 body 的
//! `assembly_ids` / `merge_quantities` 两个键。套数算在 rust 侧是因为口径必须与
//! 详情只读字段（`line_items[].shippable_sets`）逐字一致，两处共用
//! `service::note_shippable_sets`；python 端只负责把套数填进 xlsx，不再自己推算。
//!
//! ## handler 语义
//! 1. `authenticate_middleware` 已强制 JWT 校验（未带 token → 40100）；
//! 2. `require_any_role` 限制 `MANAGER / CLERK / INSPECTOR`（对齐前端 `canPrint`
//!    闸门 + python 端历史 RBAC）；不通过 → 40300；
//! 3. clone `headers` 后注入 `X-Forwarded-User-Id: <CurrentUser.id>`；
//! 4. 读本单批次 → 算套数 → 注入转发 body（见下「注入的两个键」）；
//! 5. 调 `state.py_backend.forward_delivery_note_print{,_labels}(id, body, headers)`，
//!    拿 `(status, headers, body)` 三元组原样拼 `Response`。
//!
//! ## 注入的两个键（`body` 是 `Json<Value>`，handler 可自由改写后转发）
//! - `assembly_ids: [string]` —— 本单批次所属、且**能解析到**（未软删）的装配件 id。
//!   雪花 id > 2^53，一律序列化为 JSON **string**（JSON number 会丢精度）。
//!   解析不到的（软删 / 不存在）**不**放进去 —— python 侧 `assembly_map` 缺它，
//!   其子件按散件行打印，这是既有可达路径。
//! - `merge_quantities: { "<assembly_id>": <sets> }` —— 本单可出货套数。
//!   **值可以是 0**（0 = 该装配件凑不齐整套，其子件不进 xlsx）。
//!   key 是 string（同上），value 是普通 JSON number。
//!
//! ## 三条关键取舍
//! - **只读取数、不开 tx、不改 DB**：handler 内 `state.pool.acquire()` 拿连接
//!   （`handler/crud.rs` 读端点同款范式），跑只读查询即 drop —— 读数**仅**
//!   用于注入转发 body，不参与任何业务写入，故不开事务。**读失败一律
//!   `AppError::Database`（50001 / HTTP 500）fail-loud，不降级成「不注入」**：
//!   降级会让 python 端回落成每套默认 1（`_build_print_rows` 里
//!   `(merge_quantities or {}).get(asm_id, 1)`）⇒ 静默打出错标签。
//! - **不新增 404**：note 不存在 / 本单无批次 / 无装配件 / 装配件全软删时，
//!   **不注入任何键**、原样转发。404 仍由 python 侧 `BIZ_DELIVERY_NOTE_NOT_FOUND`
//!   兜，避免在 BFF 层新造一条与上游不一致的失败路径。
//! - **body 用 `Json<Value>` 透传**：不定义强类型 DTO、不解析前端字段。前端发的
//!   雪花 ID 是 string（> 2^53，JSON number 会丢精度），rust 侧解析只会引入
//!   一层无收益的转换；`custom_order` / `line_item_ids` 的语义仍由 python 端
//!   schema 负责（与 STS 转发同构）。`merge_quantities` 是**例外**：它由 rust 侧
//!   **总是覆盖**写入（键级整体替换，不是逐键 merge），不再接受前端的人工
//!   override。⚠️ 注入只在「本单有可解析装配件」时发生；没有装配件时前端自己
//!   发的 `merge_quantities` 原样透传（python 端也用不到它）。
//!
//! ## 鉴权头不透传给 python
//! `filter_request_headers`（`infra::py_backend`）剥掉 `Authorization` / `Cookie`，
//! python 端不反向依赖 rust 的 JWT。身份只通过 `X-Forwarded-User-Id` 单头传递。
//!
//! ## 响应头
//! 由 `filter_response_headers` 清洗：保留 `content-type` / `content-disposition`
//! （前端 `parseFilename` 靠后者取下载文件名）/ `cache-control`，`content-length`
//! 按实际 body 长度重算；hop-by-hop 与 `content-encoding` 剥除。

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use serde_json::{Map, Value, json};

use crate::auth::rbac::{CurrentUser, Role};
use crate::modules::assembly::repo::AssemblyRepo;
use crate::modules::delivery_note::dto::DeliveryNotePath;
use crate::modules::delivery_note::service::note_shippable_sets;
use crate::modules::part::model::TPart;
use crate::modules::part::repo::PartRepo;
use crate::modules::prod::batch::repo::PartBatchRepo;
use crate::shared::error::AppError;
use crate::state::AppState;

/// 打印允许的角色集：`MANAGER` / `CLERK` / `INSPECTOR`。
///
/// 与前端 `canPrint` 闸门、python 端历史 RBAC 三方对齐；`SHELF_ACCOUNT`
/// （货架终端）不放行——它只该扫码，不该开单打印。
fn require_print_role(current: &CurrentUser) -> Result<(), AppError> {
    current.require_any_role(&[Role::Manager, Role::Clerk, Role::Inspector])
}

/// 2026-10-03 新增：clone `headers` 并注入 `X-Forwarded-User-Id`。
///
/// 形参 `headers` 由 axum extractor 提供，**不可**直接 mutate（会污染共用同一
/// `HeaderMap` 的其它 extractor / middleware），故先 clone 一份副本再插入。
/// `filter_request_headers` 的 SKIP 列表不含此 header，不会被二次过滤。
fn forwarded_headers(headers: &HeaderMap, current: &CurrentUser) -> HeaderMap {
    let mut fwd = headers.clone();
    if let Ok(value) = current.id.to_string().parse() {
        fwd.insert("x-forwarded-user-id", value);
    }
    fwd
}

/// 2026-10-04 新增：转发前读本单批次，把装配件可出货套数注入 body。
///
/// 取数走 3 类既有 repo 方法（**不新增 `query!` 宏**，`.sqlx/` 离线缓存不必重生成）：
/// 1. `PartBatchRepo::list_with_part_by_delivery_note` —— 本单全部未删批次 × 工单；
/// 2. `AssemblyRepo::list_by_ids(..., include_deleted=false)` —— 装配件（软删的解析不到）；
/// 3. `PartRepo::list_children(..., include_deleted=false)` —— 每个装配件的**全部**子件。
///
/// ⚠️ 第 3 步是 `min` 的定义域（缺它就会把「A 交一半、C 一件没交」误判成 A 能撑的
/// 套数 ⇒ 印出物理上不存在的整套），本域按装配件逐个取：单单装配件通常 1~3 个，
/// N 次小查询可接受（先例 `service/scan/mod.rs:143,166`）。批量详情那条链路
/// （N 单 × M 装配件）改用 1 条 SQL 的 `PartRepo::list_children_by_assemblies`，
/// 两条路径同 `include_deleted=false` 口径，套数不会分叉。
///
/// 「不注入任何键、原样转发」的 4 种情形（本单无批次 / 无装配件 / 装配件全软删 /
/// `body` 不是 JSON object），都是 python 侧已能处理的输入，BFF 层不新造失败路径。
///
/// 2026-10-04 观察项：本函数**不做用户货架 scope 过滤**（delivery_note 域本就无
/// user-scope 校验，打印端点连单是否存在都不查）。当前不泄漏 —— 响应体来自 python
/// 端自己的单据查询，本函数算出的套数只写进转发 body。若将来任何响应回显套数，
/// 必须先补 scope 过滤。
async fn with_shippable_sets(
    state: &AppState,
    note_id: i64,
    body: Value,
) -> Result<Value, AppError> {
    let Some(obj) = body.as_object() else {
        // body 不是 JSON object（前端理论上不会这么发）→ 不注入，也不报错。
        // 先于取数短路：省掉 3 条无用的 DB 往返。
        return Ok(body);
    };
    // 读端点不开 tx：pool.acquire() → 只读查询 → drop（同 handler/crud.rs 范式）。
    let mut conn = state.pool.acquire().await?;
    let rows = PartBatchRepo::list_with_part_by_delivery_note(&mut *conn, note_id).await?;
    if rows.is_empty() {
        return Ok(body);
    }
    let asm_ids: Vec<i64> = rows
        .iter()
        .filter_map(|(_b, p)| p.assembly_id)
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    if asm_ids.is_empty() {
        return Ok(body);
    }
    // list_by_ids 带 `ORDER BY id ASC` ⇒ assembly_ids 顺序稳定，测试可逐字断言。
    let asms = AssemblyRepo::list_by_ids(&mut *conn, &asm_ids, false).await?;
    if asms.is_empty() {
        return Ok(body);
    }
    // 套数公式只需要装配件的 `quantity`（比例因子 + LEAST 收口上界），收成
    // `HashMap<i64, i32>` 免掉整行 clone。
    let asm_quantity: HashMap<i64, i32> = asms.iter().map(|a| (a.id, a.quantity)).collect();
    let mut children_by_asm: HashMap<i64, Vec<TPart>> = HashMap::with_capacity(asm_ids.len());
    for aid in &asm_ids {
        children_by_asm.insert(
            *aid,
            PartRepo::list_children(&mut *conn, *aid, false).await?,
        );
    }
    let sets = note_shippable_sets(&rows, &asm_quantity, &children_by_asm);

    // 雪花 id > 2^53 ⇒ id 一律 JSON string；套数是普通计数 ⇒ JSON number。
    let mut quantities: Map<String, Value> = Map::with_capacity(asms.len());
    for a in &asms {
        quantities.insert(
            a.id.to_string(),
            json!(sets.get(&a.id).copied().unwrap_or(0)),
        );
    }
    let mut out = obj.clone();
    out.insert(
        "assembly_ids".to_string(),
        Value::Array(
            asms.iter()
                .map(|a| Value::String(a.id.to_string()))
                .collect(),
        ),
    );
    out.insert("merge_quantities".to_string(), Value::Object(quantities));
    Ok(Value::Object(out))
}

/// `POST /api/v2/delivery-notes/{id}/print` —— 鉴权 + 注入套数 + 转发
///
/// 转发到 python `POST /api/v1/delivery-notes/{id}/print`（路径同名），
/// 拿回 xlsx 字节流原样返回。
pub async fn print_delivery_note(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(path): Path<DeliveryNotePath>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Response, AppError> {
    require_print_role(&current)?;
    let fwd_headers = forwarded_headers(&headers, &current);
    let note_id = path.id.to_string();
    let body = with_shippable_sets(&state, path.id, body).await?;
    let resp = state
        .py_backend
        .forward_delivery_note_print(&note_id, body, fwd_headers)
        .await?;
    Ok((resp.status, resp.headers, resp.body).into_response())
}

/// `POST /api/v2/delivery-notes/{id}/print-labels` —— 鉴权 + 注入套数 + 转发
///
/// 转发到 python `POST /api/v1/delivery-notes/{id}/print-labels`（路径同名）。
/// 注入逻辑与 `/print` **完全一致**（同一 helper）：两个端点的 xlsx 行构建在
/// python 端是同一份 `_prepare_print_rows`，套数必须同样口径。
pub async fn print_labels(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(path): Path<DeliveryNotePath>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Response, AppError> {
    require_print_role(&current)?;
    let fwd_headers = forwarded_headers(&headers, &current);
    let note_id = path.id.to_string();
    let body = with_shippable_sets(&state, path.id, body).await?;
    let resp = state
        .py_backend
        .forward_delivery_note_labels(&note_id, body, fwd_headers)
        .await?;
    Ok((resp.status, resp.headers, resp.body).into_response())
}

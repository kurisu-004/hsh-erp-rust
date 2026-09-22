# axum 最佳实践 10 条（A1-A10）

> 2026-09-22 PR1 沉淀自 `iam-steady-flurry.md`，与现有代码（`Cargo.toml` 锁定）1:1 对齐。
>
> **本仓实际版本**（`Cargo.toml` 2026-09-22 锁定）：
> `axum = "0.8"` / `tower = "0.5"` / `tower-http = "0.6"` /
> `sqlx = "0.9"` / `tokio = "1"` / `tokio-util = "0.7"` / `edition = "2024"`。

PR5+ 写 handler / router / extractor / 错误响应时按本表 10 条逐项对照；
code review 时把违反 A1-A10 的写法视为 blocker。

## A1 — Router 工厂 + `.nest()` + `.route_layer()` 统一鉴权

业务 REST 路由统一 `/api/v2`，WS 在 `/ws/dashboard`。每个域提供工厂函数：

```rust
pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .nest("/iam", iam::router())
        .nest("/parts", part::router())
        .route("/ws/dashboard", get(ws::ws_upgrade))
        // 鉴权层挂顶层（除公开端点用 .route_layer(auth) 覆盖）
        .route_layer(from_fn_with_state(Arc::clone(...), auth_middleware))
}
```

**禁止**：在 handler 内部手写 `match path` 分发；禁止在 `main.rs` 平铺所有 route。

## A2 — `State<Arc<AppState>>` 共享状态；service 不直接持有 AppState

handler 签名首参数永远是 `State<Arc<AppState>>`，service 通过 `state.account_service.xxx` 取
**专域 service 句柄**（已 `Arc<AccountService>` 形式挂在 `AppState`），不传 `Arc<AppState>`。

```rust
// ✅ 正确
let out = state.account_service.get_user(&mut *conn, id, &current).await?;

// ❌ 错误（service 不该见 AppState）
let out = state.service.get_user(&mut *conn, state, id, &current).await?;
```

理由：`Arc<AppState>` 跨域互相调用会造成隐式依赖；service 仅持 `Arc<SnowflakeIdGenerator>`
+ 跨域依赖，**不持** repo / pool / tx。

## A3 — extractor 顺序固定

handler 签名参数按下列顺序：

```text
State → 鉴权 (CurrentUser / Extension) → Path / Query → body extractor 最后
```

```rust
pub async fn get_user(
    State(state): State<Arc<AppState>>,        // 1. State
    current: CurrentUser,                       // 2. 鉴权
    Path(id): Path<i64>,                        // 3. Path
    Query(q): Query<UserListQuery>,             // 4. Query（如有）
    Json(req): Json<UserUpdateRequest>,         // 5. body 最后
) -> Result<Json<R<UserOut>>, AppError> { ... }
```

**禁止**：`Json<T>` 放在 `Path`/`Query` 之前——axum 0.8 会因 body 提前消费导致 422。
code review 时把 extractor 顺序错误视为 blocker。

## A4 — `Path<i64>` / `Query<T>` 用 newtype；path 占位符用 `{id}` 而非 `:id`

雪花 ID 在 path / query 上必须用 `Path<i64>` + `serialize_i64` 双端对称，
**禁止** path 直接传 `"123456789012345678"` 字符串再服务端 `parse::<i64>()`。

**⚠️ axum 0.8 path 占位符风格切换**：

```rust
// ✅ axum 0.8（matchit 风格）
Router::new()
    .route("/{id}", get(get_user))
    .route("/{id}/roles/{role_id}", post(add_role))

// ❌ axum 0.7 风格（已废弃）
Router::new()
    .route("/:id", get(get_user))   // 0.8 不再支持，编译错
    .route("/:id/roles/:role_id", post(add_role))
```

axum 0.8 底层切到 `matchit`，path 占位符必须用 `{name}`；`:name` 编译失败。
PR1 文档扫仓已确认 0 处 `:id` / `:role_id` 等残留。

## A5 — `IntoResponse for AppError` 统一错误响应；handler 全走 `R<T>` 信封

业务错误由 `AppError::biz(code::BIZ_..., "...")` 构造（已在 service 内部翻译），
handler 不直接返 tuple，全走 `R<T>` 信封：

```rust
pub async fn handler(...) -> Result<Json<R<UserOut>>, AppError> { ... }
pub async fn create(...) -> Result<(StatusCode, Json<R<UserOut>>), AppError> {
    Ok((StatusCode::CREATED, Json(R::ok(out))))
}
pub async fn delete(...) -> Result<Json<R<()>>, AppError> {
    Ok(Json(R::ok_empty()))
}
```

错误响应由 `AppError::into_response()` 自动装入 `R::err(...)` 信封，
**禁止** handler 自己 `Json(serde_json::json!({"code": 4xx, ...}))` 手搓响应。

## A6 — WS handler 独立 Router + `WebSocketUpgrade` extractor

WS 路由独立挂在 `/ws/dashboard`，**不**混入 REST `.nest()`：

```rust
async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
    user: CurrentUser,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, state, user))
}

pub fn router() -> Router<Arc<AppState>> {
    Router::new().route("/ws/dashboard", get(ws_handler))
}
```

WS 内部用 `axum::extract::ws::WebSocket` 处理消息，广播走 `state.ws_hub.broadcast(...)`。
**禁止**：把 WS 升级逻辑塞进 REST handler；禁止绕开 `WebSocketUpgrade` extractor 手写握手。

## A7 — multipart upload 用 `axum::extract::Multipart`

文件上传域（如 `files` / `upload_session`）走 `axum::extract::Multipart`（已开
`multipart` feature），**禁止**手写 `tokio::io::AsyncReadExt` + body parser：

```rust
pub async fn upload(
    State(state): State<Arc<AppState>>,
    user: CurrentUser,
    mut multipart: Multipart,
) -> Result<Json<R<FileOut>>, AppError> {
    while let Some(field) = multipart.next_field().await? {
        let name = field.name().unwrap_or("file");
        let bytes = field.bytes().await?;
        // ...
    }
    ...
}
```

## A8 — middleware 组合用 `tower::ServiceBuilder` `.layer(...)`，不开后置包装

```rust
use tower::ServiceBuilder;
use tower_http::trace::TraceLayer;
use tower_http::cors::CorsLayer;

let app = Router::new()
    .nest("/api/v2", v2_router())
    .layer(
        ServiceBuilder::new()
            .layer(TraceLayer::new_for_http())
            .layer(CorsLayer::permissive())
            .layer(from_fn_with_state(state.clone(), auth_middleware))
    );
```

**禁止**：在 handler 内手写 middleware 逻辑；禁止"后置响应包装"（A5 已统一信封）。

## A9 — 业务 404 用 `AppError::NotFound` 经 IntoResponse

业务资源不存在（零件 / 客户 / 角色 / menu 节点）走 `AppError::biz(code::NOT_FOUND, "...")`，
由 `into_response` 渲染为 404 JSON 响应，**与 axum 路由级 404（URL 不存在）保持一致语义**：

```rust
// ✅ service 内部
.map_err(|_| AppError::biz(code::NOT_FOUND, "user not found"))?;

// ❌ handler 自己
return Ok((StatusCode::NOT_FOUND, Json(R::err(40404, "user not found"))));
```

## A10 — tracing/macro 不改 axum 0.8 默认行为；repo trait 仍用 `#[async_trait]`

**⚠️ 重要**：

```text
Rust 2024 + axum 0.8 已支持原生 `async fn` in trait；
但本仓 repo trait 仍用 #[async_trait]（与 src/modules/iam/repo/mod.rs 一致），
保留 #[async_trait] 以保持统一。
```

```rust
// ✅ 本仓现行写法（保留 #[async_trait]）
#[cfg_attr(test, mockall::automock)]
#[async_trait]
pub trait IamRepo: Send {
    async fn get_user_by_id(&mut self, id: i64) -> Result<Option<User>, sqlx::Error>;
    // ...
}

// ❌ 不要切（破坏 mockall + 现有 14 域一致性）
pub trait IamRepo: Send {
    async fn get_user_by_id(&mut self, id: i64) -> Result<Option<User>, sqlx::Error>;
}
```

理由：

- mockall 当前对 native `async fn` in trait 支持仍需 nightly；
- 14 域都 `#[async_trait]`，切掉一个破坏一致性；
- 切 `async fn` in trait 涉及全部 14 域 repo + service 形参统一改造，应作为独立 PR 处理。

error/log 注入统一由 `IntoResponse for AppError` 兜底，**禁止** handler 内 `tracing::error!`
后吞错或重抛——会破坏错误码语义。

---

## 速查：违反条款与对应修复

| 违反条款 | 表现 | 修复 |
|---|---|---|
| A1 | handler 内 `match path` 分发 | 提取 `pub fn router()` + `.nest()` |
| A2 | service 持 `Arc<AppState>` | service 仅持 `Arc<SnowflakeIdGenerator>` + 跨域依赖 |
| A3 | `Json<T>` 在 `Path`/`Query` 前 | 调换 extractor 顺序 |
| A4 | path 用 `:id` | 改 `{id}`（axum 0.8 matchit 风格） |
| A5 | handler 返 tuple `(StatusCode, Json(...))` 直接构造 | 走 `Result<..., AppError>` + `AppError::into_response` |
| A6 | WS 升级塞进 REST handler | 独立 `Router::new().route("/ws/...", get(ws_handler))` |
| A7 | 手写 body parser | 用 `axum::extract::Multipart` |
| A8 | 后置响应包装 | `ServiceBuilder::new().layer(...)` 顶挂 |
| A9 | handler 自己返 404 tuple | service 内 `AppError::biz(NOT_FOUND, ...)` |
| A10 | 切 native `async fn` in trait | 保留 `#[async_trait]`（mockall 兼容） |
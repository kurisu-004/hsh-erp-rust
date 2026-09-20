//! 应用入口
//!
//! 组装流程：
//!   tracing init → 配置 → PgPool → 雪花 ID → WS 广播中枢 → COS 客户端（占位）
//!   → CancellationToken → AppState → 后台任务 spawn → Router nest (/api/v2 /api/mcp /ws)
//!   → axum::serve + graceful_shutdown (Ctrl-C → 取消后台任务)
//!
//! 2026-09-20 新增：5 个 tower / tower-http 中间件（外层 + nest 内层）：
//! - 外层：RequestId / PropagateRequestId / CatchPanic / Trace / CORS / Body limit
//! - 内层（仅 `/api/v2` nest）：Compression（gzip）/ Timeout（默认 30s）
//!   WS nest **不**挂 Compression 与 Timeout（前者会压 upgrade 响应，后者会杀长连接）。

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use axum::Json;
use axum::Router;
use axum::extract::Request as AxumRequest;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::response::Response;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use tower_http::catch_panic::CatchPanicLayer;
use tower_http::compression::CompressionLayer;
use tower_http::cors::CorsLayer;
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer};
use tower_http::timeout::TimeoutLayer;
use tower_http::trace::TraceLayer;
use tracing::info;
use tracing_subscriber::EnvFilter;

use hsh_erp_rust::auth::session::{NoopSessionStore, RedisSessionStore, SessionStore};
use hsh_erp_rust::infra::config::AppConfig;
use hsh_erp_rust::infra::cos::CosClient;
use hsh_erp_rust::infra::cos_opendal::build_cos_client;
use hsh_erp_rust::infra::db;
use hsh_erp_rust::infra::python_sts::{HttpPythonSts, NoopPythonSts, PythonSts};
use hsh_erp_rust::infra::redis;
use hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator;
use hsh_erp_rust::infra::ws_hub::WsHub;
use hsh_erp_rust::modules;
use hsh_erp_rust::modules::upload_session::repo::{
    NoopUploadSessionRepo, RedisUploadSessionRepo, UploadSessionRepo,
};
use hsh_erp_rust::state::AppState;
use hsh_erp_rust::task;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // 1. 初始化日志
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_target(true)
        .init();

    // 2. 加载配置
    let config = AppConfig::from_env(".env").context("加载配置失败")?;
    info!(listen = %config.listen_addr, "配置加载完成");

    // 3. 数据库连接池
    let pool = db::create_pool(&config)
        .await
        .context("创建数据库连接池失败")?;

    // 3.5 启动时执行 sqlx 迁移（编译期扫描 ./migrations/，缺失/版本落后则自动 apply）
    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .context("执行数据库迁移失败")?;

    // 4. 雪花 ID 生成器（位布局对齐 myERP Python，跨语言 ID 互解）
    let snowflake = Arc::new(SnowflakeIdGenerator::new(
        config.snowflake.epoch_ms,
        config.snowflake.instance,
    ));

    // 5. WebSocket 广播中枢
    let ws_hub = Arc::new(WsHub::new());

    // 6. COS 客户端（按 `COS_BACKEND` 二选一：`opendal` / `noop`）
    // 2026-09-11 修改：原二选一 `if config.cos.enabled` 构造 TencentCos / NoopCos。
    // 2026-09-20 spike：扩展为三选一（`build_cos_client` 内部按 `CosBackend` enum dispatch）。
    // 2026-09-20 迁移清理：删 `cos_sdk` 变体后回到二选一（`opendal` / `noop`），缺省
    // `opendal`。`OpenDalCos::new` 失败时 `?` 终止启动；`NoopOpenDal` 永不失败。
    let cos: Arc<dyn CosClient> = build_cos_client(&config.cos).context("构造 COS 客户端失败")?;

    // 6.4 Python STS 凭证转发客户端（2026-09-18 新增；替代原 TencentSts 直连）
    // 走 HTTP 转发到 python 后端内部端点 `/api/v1/files/sts-prefix-credentials`；
    // 本地 cargo run 时如不想启 python 后端，把 `PYTHON_BACKEND_BASE_URL` 设为空
    // 走 Noop 占位（仅供前端骨架调试，业务上不真上传）。
    //
    // 2026-09-18 review #5 修复：fail-fast —— COS_ENABLED=true 且 PYTHON_BACKEND_BASE_URL
    // 为空是典型的生产 misconfiguration（业务要真实上传但 STS 链路未就绪）。原代码
    // 静默回退到 NoopPythonSts 会让前端拿到"看上去合法"的占位 token 上传，触发
    // 一连串 403 / 头像丢失等下游问题；现改为直接 bail! 拒绝启动，强制 ops
    // 修复环境变量。dev / 测试场景显式 `COS_ENABLED=false` 仍走 Noop。
    let python_sts: Arc<dyn PythonSts> = if config.cos.enabled
        && !config.upload_session.python_backend_base_url.is_empty()
    {
        info!(
            base_url = %config.upload_session.python_backend_base_url,
            "PYTHON_BACKEND_BASE_URL 已配置，启用 HttpPythonSts（转发 python 后端签发 STS）"
        );
        Arc::new(
            HttpPythonSts::new(config.upload_session.python_backend_base_url.clone())
                .context("初始化 HttpPythonSts 失败")?,
        )
    } else if config.cos.enabled && config.upload_session.python_backend_base_url.is_empty() {
        // fail-fast：COS_ENABLED=true 但 PYTHON_BACKEND_BASE_URL 缺失 —— 不静默回退
        anyhow::bail!(
            "COS_ENABLED=true 但 PYTHON_BACKEND_BASE_URL 未配置；rust 上传会话域必须转发 \
             python 后端签发 STS。请在 .env 设置 PYTHON_BACKEND_BASE_URL=http://backend:8000 \
             （或显式 COS_ENABLED=false 走 Noop 占位）。这是 review #5 修复的 fail-fast \
             防 misconfiguration 静默启用。"
        );
    } else {
        info!(
            cos_enabled = config.cos.enabled,
            python_base_url = %config.upload_session.python_backend_base_url,
            "PYTHON_BACKEND_BASE_URL 未配置或 COS_ENABLED=false，使用 NoopPythonSts（占位，本地调试用）"
        );
        Arc::new(NoopPythonSts)
    };

    // 6.5 Redis 连接池 + 服务端 session 存储
    // 关掉后使用 NoopSessionStore（不连 Redis）；适用于 Rust 借 Python JWT 的过渡期
    let (session, upload_session_repo): (Arc<dyn SessionStore>, Arc<dyn UploadSessionRepo>) =
        if config.redis.session_check_enabled {
            let redis_pool = redis::create_pool(&config).context("创建 Redis 连接池失败")?;
            info!("Redis session 存储 + upload_session 存储已就绪");
            (
                Arc::new(RedisSessionStore::new(redis_pool.clone())),
                Arc::new(RedisUploadSessionRepo::new(redis_pool)),
            )
        } else {
            info!(
                "REDIS_SESSION_CHECK_ENABLED=false，跳过 Redis 连接，使用 NoopSessionStore + NoopUploadSessionRepo"
            );
            (
                Arc::new(NoopSessionStore::new()),
                Arc::new(NoopUploadSessionRepo),
            )
        };

    // 7. 优雅退出令牌
    let shutdown = CancellationToken::new();

    // 8. 组装 AppState
    // 2026-09-14 修复 Bug #1：移除 release profile 硬关 _e2e 的逻辑。
    // 单一控制点 = env `E2E_HOOKS_ENABLED`（缺省 true）。
    // docker compose / dev `cargo run` 走默认值（true）→ 启用；prod / staging 必须
    // 显式 `E2E_HOOKS_ENABLED=false`（ops 责任，不靠编译期二分）。
    // 2026-09-18：移除 `state.sts`（TencentSts）；改为 `state.python_sts`（HttpPythonSts /
    // NoopPythonSts）+ `state.upload_session_repo`（RedisUploadSessionRepo / NoopUploadSessionRepo）。
    let config = Arc::new(config);
    let state = Arc::new(AppState::new(
        pool,
        config.clone(),
        snowflake,
        ws_hub.clone(),
        cos,
        python_sts,
        shutdown.clone(),
        session,
        upload_session_repo,
    ));

    // 9. 启动后台任务
    let task_state = state.clone();
    let task_token = shutdown.clone();
    tokio::spawn(async move {
        task::auto_complete::run(task_state, task_token).await;
    });

    // 10. 路由组装
    // 2026-09-20 修复 review #1：axum `.layer()` 语义是「**后加的在外层**」
    // （tower Layer 的 Service wrapped 顺序：后调用的 Layer 包裹先调用的），
    // 因此下面按「**内→外**」顺序书写（最先 `.layer` 的是最内层），
    // request 实际执行顺序（外→内）为：
    //   BodyLimit → CORS → SetRequestId → PropagateRequestId → CatchPanic
    //     → TraceLayer(make_span_with) → router
    // 关键：SetRequestId 必须在 TraceLayer 外侧跑，否则 `make_request_span`
    // 读不到注入的 `x-request-id` header，所有 span 都退化成 `request_id = "-"`。
    //
    // 越外层越先看到 request / 后看到 response；越内层越先看到 response。
    //
    // nest 内层（仅 `/api/v2`）：Compression（gzip 响应压缩）+ Timeout（请求超时）。
    // **TimeoutLayer 是 nest 内最外层（覆盖 Compression）** —— 先超时判定、再 gzip 响应，
    // 即超时分支直接 408 plain 不会被压缩；超时与压缩均只对 `/api/v2` 子树生效。
    // WS nest **不**挂这两层：Compression 会压缩 WS upgrade 响应（破坏握手），
    // Timeout 会杀 WS 心跳长连接。两者都必须局限在 `/api/v2` 子树内。
    let max_body = state.config.max_request_body_size;
    let request_timeout = Duration::from_secs(state.config.request_timeout_seconds);
    let api_v2 = modules::v2_router(state.clone())
        .layer(CompressionLayer::new()) // gzip 响应压缩（nest 内层）
        .layer(TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            request_timeout,
        )); // 请求级超时（nest 内最外层，覆盖 Compression；不影响 WS）
    let app: Router = Router::new()
        .nest("/api/v2", api_v2)
        .nest("/ws", modules::ws_router())
        // ----- 安全 / 可观测层（按内→外书写：最先 .layer 的是最内层） -----
        // 1) Trace 定制：span 带 request_id/method/path，on_response 记 status+耗时
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(make_request_span)
                .on_response(trace_on_response),
        )
        // 2) Panic 兜底：handler panic → 统一 500 信封（不是断连）
        .layer(CatchPanicLayer::custom(handle_panic))
        // 3) 回写 x-request-id 到响应头
        .layer(PropagateRequestIdLayer::x_request_id())
        // 4) RequestId：生成或透传 x-request-id（必须在 TraceLayer 外侧）
        .layer(SetRequestIdLayer::x_request_id(MakeRequestUuid))
        // 5) CORS（现状保留）
        .layer(CorsLayer::permissive())
        // 6) Body limit（现状保留）
        .layer(RequestBodyLimitLayer::new(max_body))
        // 2026-09-20 修复 review #1：state 同时是 `Arc<AppState>`（中间件用：
        // `route_layer(from_fn_with_state(state.clone(), auth_middleware))`）
        // 与 `Router<S = Arc<AppState>>` 的 S（handler extractor 用）的同一 Arc，
        // `state.clone()` 两次仅 bump Arc 引用计数、不做深拷贝。
        .with_state(state.clone());

    // 11. 监听
    let listen_addr = state.config.listen_addr.clone();
    let addr: std::net::SocketAddr = listen_addr
        .parse()
        .with_context(|| format!("解析监听地址失败: {listen_addr}"))?;
    let listener = TcpListener::bind(&addr)
        .await
        .with_context(|| format!("绑定端口失败: {addr}"))?;
    info!(%addr, "服务已启动");

    // 12. 优雅退出：Ctrl-C 触发取消令牌
    let signal_token = state.shutdown.clone();
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                info!("收到 Ctrl-C，开始取消后台任务");
                signal_token.cancel();
            }
        })
        .await
        .context("axum::serve 失败")?;

    info!("服务退出");
    Ok(())
}

// ===========================================================================
// 2026-09-20 新增：tower_http 中间件回调 fn pointer（满足 Fn / FnMut / FnOnce
// + Clone + Send + Sync + 'static，详见 tower-http 文档对 TraceLayer /
// CatchPanicLayer 的 trait bound 要求）。
// ===========================================================================

/// Panic 兜底：handler panic → 统一 500 信封（与 AppError::into_response 同形）。
///
/// `code: 50000 / INTERNAL` 是系统错误的固定段，HTTP 500。fn pointer 形式满足
/// `CatchPanicHandler` trait bound（自动实现）。
///
/// 2026-09-20 修复 review #1：原实现 `_err` 直接吞掉，运维侧拿不到 panic payload
/// 难以定位事故现场。现 downcast 出 `&str`（最常见 panic 信息形式）后走
/// `tracing::error!` 落地日志；`String` payload 也兼容；其它类型退化为类型名。
/// panic 仍以 500 信封对外 —— 日志仅作运维排查依据，**不**泄漏到客户端。
#[allow(dead_code)]
fn handle_panic(err: Box<dyn std::any::Any + Send + 'static>) -> Response {
    // 2026-09-20 修复 review #1：downcast payload 落日志（运维排查）
    let payload = if let Some(s) = err.downcast_ref::<&'static str>() {
        (*s).to_string()
    } else if let Some(s) = err.downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string panic payload>".to_string()
    };
    tracing::error!(panic = %payload, "handler panic（被 CatchPanicLayer 兜底为 500）");
    let body = serde_json::json!({
        "code": hsh_erp_rust::shared::error::code::INTERNAL,
        "message": "internal server error",
        "data": serde_json::Value::Null,
    });
    (StatusCode::INTERNAL_SERVER_ERROR, Json(body)).into_response()
}

/// TraceLayer 的 span 工厂：每个 HTTP 请求一个 `http_request` span，
/// 含 method / path / request_id（来自 SetRequestIdLayer 注入的 header）。
#[allow(dead_code)]
fn make_request_span(req: &AxumRequest) -> tracing::Span {
    let request_id = req
        .headers()
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("-");
    tracing::info_span!(
        "http_request",
        method = %req.method(),
        path = %req.uri().path(),
        request_id = %request_id,
    )
}

/// TraceLayer 的响应回调。成功走 `info!`，4xx/5xx 走 `warn!`（CI 日志分级友好）。
#[allow(dead_code)]
fn trace_on_response(resp: &Response, latency: Duration, _span: &tracing::Span) {
    let status = resp.status();
    let latency_ms = latency.as_millis();
    if status.is_success() {
        tracing::info!(status = %status, latency_ms = %latency_ms, "http response");
    } else {
        tracing::warn!(status = %status, latency_ms = %latency_ms, "http response");
    }
}

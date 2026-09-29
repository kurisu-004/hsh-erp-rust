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

use hsh_erp_rust::auth::session::{RedisSessionStore, SessionStore};
use hsh_erp_rust::infra::config::AppConfig;
use hsh_erp_rust::infra::cos::CosClient;
use hsh_erp_rust::infra::cos_opendal::build_cos_client;
use hsh_erp_rust::infra::db;
use hsh_erp_rust::infra::py_backend::{PyBackendClient, build_py_backend};
use hsh_erp_rust::infra::seed;
use hsh_erp_rust::infra::redis;
use hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator;
use hsh_erp_rust::infra::ws_hub::WsHub;
use hsh_erp_rust::middleware::idempotency::{IdempotencyStore, RedisIdempotencyStore};
use hsh_erp_rust::modules;
use hsh_erp_rust::modules::wx::wecom_client::{WeComApiClient, build_wecom_client};
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

    // 3.6 应用 seeds（菜单等配置数据，幂等；2026-09-25 sqlx 接管后从 migration 抽出）
    seed::run_seeds(&pool, config.bootstrap_admin_enabled)
        .await
        .context("应用 seeds 失败")?;

    // 4. 雪花 ID 生成器（位布局对齐 myERP Python，跨语言 ID 互解）
    let snowflake = Arc::new(SnowflakeIdGenerator::new(
        config.snowflake.epoch_ms,
        config.snowflake.instance,
    ));

    // 5. WebSocket 广播中枢
    let ws_hub = Arc::new(WsHub::new());

    // 6. COS 客户端（按 `COS_BACKEND` 二选一：`opendal` / `noop`）
    let cos: Arc<dyn CosClient> = build_cos_client(&config.cos).context("构造 COS 客户端失败")?;

    // 6.6 python 后端转发客户端（薄壳鉴权转发到 python STS 端点）。
    // 2026-09-28 新增：修复 python `/api/v1/files/sts-tmp-keys` 裸开漏洞——本 rust 端
    // 是新的强制鉴权点（详见 plan `/Users/ren/.claude/plans/sts-session-uploader-sts-sts-sequential-globe.md`）。
    // `enabled=false`（未设 `PYTHON_BACKEND_BASE_URL`）→ `NoopPyBackend` 占位；
    // `enabled=true` → `HttpPyBackend` 走真实转发。
    let py_backend: Arc<dyn PyBackendClient> =
        build_py_backend(&config.python_backend).context("构造 python 后端转发客户端失败")?;

    // 6.5 Redis 连接池 + 服务端 session 存储（生产必走 Redis；NoopSessionStore 仅测试 fixture 用）
    // 2026-09-28 删除：相关上传会话域装配（Redis 共享 STS 凭证会话机制已下线）。
    let (session, idempotency_store, wecom): (
        Arc<dyn SessionStore>,
        Arc<dyn IdempotencyStore>,
        Arc<dyn WeComApiClient>,
    ) = {
        let redis_pool = redis::create_pool(&config).context("创建 Redis 连接池失败")?;
        info!("Redis session 存储 + idempotency 缓存已就绪");
        // 2026-09-29 新增：企业微信小程序登录客户端装线。
        // - `config.wecom.enabled == false`（WECOM_CORPID / WECOM_CORPSECRET 留空）
        //   → `NoopWeComClient`，wx-login 直接返 40109，**后端照常启动**（不 fail-fast）。
        // - `enabled == true` → `HttpWeComClient`，access_token 缓存与 session 共用
        //   同一个 redis_pool（key 前缀 `wecom:access_token:*`，与 `session:tok:*` 互不冲突）。
        //
        // ⚠️ 日志只打 enabled 布尔与 corpid 后 4 位——corpsecret 绝不出现在日志里。
        let wecom = build_wecom_client(&config.wecom, redis_pool.clone())
            .context("构造企业微信登录客户端失败")?;
        let corp_tail = config
            .wecom
            .corpid
            .chars()
            .rev()
            .take(4)
            .collect::<String>()
            .chars()
            .rev()
            .collect::<String>();
        info!(
            wecom_enabled = config.wecom.enabled,
            wecom_corpid_tail = %corp_tail,
            "企业微信登录客户端已就绪"
        );
        (
            Arc::new(RedisSessionStore::new(redis_pool.clone())),
            // 2026-09-23 新增 Idempotency 中间件：与 session 同池共享
            Arc::new(RedisIdempotencyStore::new(redis_pool)),
            wecom,
        )
    };

    // 7. 优雅退出令牌
    let shutdown = CancellationToken::new();

    // 8. 组装 AppState
    let config = Arc::new(config);
    let state = Arc::new(AppState::new(
        pool,
        config.clone(),
        snowflake,
        ws_hub.clone(),
        cos,
        // 2026-09-28 新增：rust → python 后端转发客户端。
        py_backend,
        shutdown.clone(),
        session,
        // 2026-09-23 新增 Idempotency 中间件存储
        idempotency_store.clone(),
        // 2026-09-29 新增：企业微信小程序登录客户端
        wecom,
    ));

    // 9. 启动后台任务
    let task_state = state.clone();
    let task_token = shutdown.clone();
    tokio::spawn(async move {
        task::auto_complete::run(task_state, task_token).await;
    });

    // 10. 路由组装
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

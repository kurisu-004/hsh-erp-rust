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
use tower_http::trace::TraceLayer;
use tracing::info;
use tracing_subscriber::EnvFilter;

use hsh_erp_rust::auth::session::{RedisSessionStore, SessionStore};
use hsh_erp_rust::infra::config::AppConfig;
use hsh_erp_rust::infra::cos::CosClient;
use hsh_erp_rust::infra::cos_opendal::build_cos_client;
use hsh_erp_rust::infra::db;
use hsh_erp_rust::infra::py_backend::{PyBackendClient, build_py_backend};
use hsh_erp_rust::infra::redis;
use hsh_erp_rust::infra::seed;
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
            // 2026-10-09：key 前缀从配置注入（生产缺省空串 = key 格式与历史逐字节一致；
            // 集成测试按进程填 `t{pid}:`）。session 与 idempotency 共用同一前缀。
            Arc::new(RedisSessionStore::new(
                redis_pool.clone(),
                config.redis.key_prefix.clone(),
            )),
            // 2026-09-23 新增 Idempotency 中间件：与 session 同池共享
            Arc::new(RedisIdempotencyStore::new(
                redis_pool,
                config.redis.key_prefix.clone(),
            )),
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
    // 2026-10-03 新增：请求级超时改为按路径分档（替换 `tower_http::timeout::TimeoutLayer`）。
    // 打印路径（送货单 print / print-labels、零件 print-drawing / print-drawing-batch）
    // 走长档 `print_request_timeout_seconds`（缺省 660s）——批量图纸打印由 python 端
    // 执行，合法耗时数分钟，原先统一 30s 会把批量打印必然打断成 408；
    // 其余路径仍走 `request_timeout_seconds`（缺省 30s，行为不变）。
    let api_v2 = modules::v2_router(state.clone())
        // 2026-10-03 新增：谓词 = tower-http 默认谓词 `and` 排除已压缩格式
        // （缘由见 `NoPrecompressedMime` 的 doc）。`compress_when` 是**替换**语义，
        // 不 `and` 在默认谓词之上就会连带丢掉「< 32 字节 / image/* / gRPC / SSE
        // 不压缩」这 4 条保护。
        .layer(CompressionLayer::new().compress_when(compression_predicate()))
        // 超时层仍是最外层（后调 = 外层 = 请求先经过），保证超时判定覆盖到
        // Compression 的整个响应写出过程；不影响 /ws 长连接（本层只在 nest 内）。
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            hsh_erp_rust::middleware::timeout::timeout_middleware,
        ));
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

/// 2026-10-03 新增：gzip 压缩谓词——跳过**已压缩**的响应格式。
///
/// ## 为什么需要它
/// tower-http 默认谓词（`DefaultPredicate`）只排除 `image/*` / gRPC / SSE / < 32 字节，
/// 于是 `application/pdf`（批量图纸打印的产物）与
/// `application/vnd.openxmlformats-officedocument.spreadsheetml.sheet`（xlsx 导出）
/// 都会被 gzip 一遍——这两者**本身就是压缩格式**，gzip 只会烧 CPU、几乎压不动体积。
/// 更贵的是路径本身：`nginx.conf` 是 `proxy_buffering off`，意味着要把整个多 MB 的
/// 批量 PDF 完整在请求路径上 gzip 一遍才能吐给前端。
///
/// ## 为什么自己实现而不复用 tower-http 的 `ContentType` 枚举
/// 直接按 mime 字符串比对：不依赖 tower-http 有没有 PDF / XLSX 的枚举变体，
/// 跨 tower-http 版本稳定。
///
/// ## 它只是**排除项**，必须 `and` 在默认谓词之上
/// `CompressionLayer::compress_when` 的语义是**替换**内置谓词，直接传本 struct 会把
/// 默认谓词的 4 条保护一起丢掉。本 struct 自身**不判断体积**（未知长度响应一律放行），
/// 体积门槛由 [`compression_predicate`] 里的 `DefaultPredicate` 负责。
#[derive(Clone, Copy)]
struct NoPrecompressedMime;

impl tower_http::compression::Predicate for NoPrecompressedMime {
    fn should_compress<B>(&self, response: &axum::http::Response<B>) -> bool
    where
        B: axum::body::HttpBody,
    {
        let raw = response
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok());
        match raw {
            // 无 content-type / 非法 header 值：交给默认行为（压缩）
            None => true,
            Some(v) => {
                // 只取 media type 本体，丢掉 `; charset=…` / `; boundary=…` 参数
                let mime = v
                    .split(';')
                    .next()
                    .unwrap_or("")
                    .trim()
                    .to_ascii_lowercase();
                !matches!(
                    mime.as_str(),
                    "application/pdf"
                        | "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
                )
            }
        }
    }
}

/// `/api/v2` 的完整压缩谓词。
///
/// 语义 = tower-http `DefaultPredicate`（`SizeAbove(32)` ∧ 非 gRPC ∧ 非 `image/*` ∧
/// 非 SSE）**且** [`NoPrecompressedMime`]（非 PDF / xlsx）。用 `.and()` 组合而不是
/// 自写一份，是因为 `compress_when` 会**替换**内置谓词：只写排除项会把体积门槛与
/// SSE / image 保护一并丢掉（SSE 被 gzip 会破坏流式推送，`image/*` 本来压不动）。
fn compression_predicate() -> impl tower_http::compression::Predicate {
    use tower_http::compression::Predicate as _;
    tower_http::compression::predicate::DefaultPredicate::new().and(NoPrecompressedMime)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Response;
    use tower_http::compression::Predicate;

    /// 造一个带 content-type 与**已知长度** body 的响应。
    ///
    /// body 必须非空且能给出 size_hint：默认谓词的 `SizeAbove(32)` 读的是
    /// `content-length` 或 `body.size_hint()`，`Body::empty()` 恒为 0 会被判「不压缩」。
    fn resp_with_content_type(ct: Option<&str>) -> Response<Body> {
        resp_with_body(ct, "x".repeat(1024).into_bytes())
    }

    fn resp_with_body(ct: Option<&str>, body: Vec<u8>) -> Response<Body> {
        let mut builder = Response::builder();
        if let Some(ct) = ct {
            builder = builder.header(axum::http::header::CONTENT_TYPE, ct);
        }
        builder.body(Body::from(body)).expect("构造响应")
    }

    /// 已压缩格式（pdf / xlsx）不压缩。
    #[test]
    fn no_compression_for_precompressed_mime() {
        let p = compression_predicate();
        assert!(
            !p.should_compress(&resp_with_content_type(Some("application/pdf"))),
            "application/pdf 已压缩，不应再 gzip"
        );
        assert!(
            !p.should_compress(&resp_with_content_type(Some(
                "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
            ))),
            "xlsx 已压缩，不应再 gzip"
        );
    }

    /// 其余 content-type 的 gzip 行为与 tower-http 默认谓词一致（含无 content-type）。
    #[test]
    fn compression_unchanged_for_other_mime() {
        let p = compression_predicate();
        for ct in [
            "application/json",
            "application/json; charset=utf-8",
            "text/plain",
            "application/octet-stream",
        ] {
            assert!(
                p.should_compress(&resp_with_content_type(Some(ct))),
                "非「已压缩格式」的响应应保持压缩行为：{ct}"
            );
        }
        assert!(
            p.should_compress(&resp_with_content_type(None)),
            "无 content-type 时保持默认行为（压缩）"
        );
    }

    /// 默认谓词的 4 条保护必须**同时**生效：`compress_when` 是替换语义，
    /// 谓词若只做「排除已压缩格式」就会把这些一起丢掉（SSE 被 gzip 会破坏流式推送）。
    #[test]
    fn default_predicate_protections_survive_composition() {
        let p = compression_predicate();
        // 保护 1：image/* 不压缩
        assert!(
            !p.should_compress(&resp_with_content_type(Some("image/png"))),
            "image/* 已压缩，不应再 gzip"
        );
        // 保护 2：SSE 不压缩
        assert!(
            !p.should_compress(&resp_with_content_type(Some("text/event-stream"))),
            "SSE 是流式推送，gzip 会破坏分帧"
        );
        // 保护 3：gRPC 不压缩
        assert!(
            !p.should_compress(&resp_with_content_type(Some("application/grpc"))),
            "gRPC 自带压缩，不应再 gzip"
        );
        // 保护 4：小于 32 字节的响应不压缩
        assert!(
            !p.should_compress(&resp_with_body(Some("application/json"), b"{}".to_vec())),
            "小于 32 字节的响应压不动，不应 gzip"
        );
        // 门槛之上仍压缩（防「谓词恒 false」这种把压缩层关掉的写法）
        assert!(
            p.should_compress(&resp_with_body(
                Some("application/json"),
                "y".repeat(1024).into_bytes()
            )),
            "超过 32 字节的 JSON 仍应压缩"
        );
    }

    /// 判定对大小写与 `; 参数` 不敏感（`Application/PDF`、`application/pdf; charset=…`
    /// 同样不该被 gzip）。
    #[test]
    fn mime_matching_is_case_insensitive_and_ignores_parameters() {
        let p = compression_predicate();
        assert!(!p.should_compress(&resp_with_content_type(Some("Application/PDF"))));
        assert!(!p.should_compress(&resp_with_content_type(Some(
            "application/pdf; charset=binary"
        ))));
        assert!(!p.should_compress(&resp_with_content_type(Some(
            "APPLICATION/VND.OPENXMLFORMATS-OFFICEDOCUMENT.SPREADSHEETML.SHEET"
        ))));
    }
}

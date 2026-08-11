//! 应用入口
//!
//! 组装流程：
//!   tracing init → 配置 → PgPool → 雪花 ID → WS 广播中枢 → COS 客户端（占位）
//!   → CancellationToken → AppState → 后台任务 spawn → Router nest (/api/v2 /api/mcp /ws)
//!   → axum::serve + graceful_shutdown (Ctrl-C → 取消后台任务)

use std::sync::Arc;

use anyhow::Context;
use axum::Router;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use tower_http::cors::CorsLayer;
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::trace::TraceLayer;
use tracing::info;
use tracing_subscriber::EnvFilter;

use hsh_erp_rust::infra::config::AppConfig;
use hsh_erp_rust::infra::cos::{CosClient, NoopCos};
use hsh_erp_rust::infra::db;
use hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator;
use hsh_erp_rust::infra::ws_hub::WsHub;
use hsh_erp_rust::modules;
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
    let config = AppConfig::from_env().context("加载配置失败")?;
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

    // 4. 雪花 ID 生成器
    let snowflake = Arc::new(SnowflakeIdGenerator::new(
        config.snowflake.epoch_ms,
        config.snowflake.instance,
        config.snowflake.seq,
    ));

    // 5. WebSocket 广播中枢
    let ws_hub = Arc::new(WsHub::new());

    // 6. COS 客户端（骨架阶段占位）
    let cos: Arc<dyn CosClient> = Arc::new(NoopCos);

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
        shutdown.clone(),
    ));

    // 9. 启动后台任务
    let task_state = state.clone();
    let task_token = shutdown.clone();
    tokio::spawn(async move {
        task::auto_complete::run(task_state, task_token).await;
    });

    // 10. 路由组装
    let max_body = state.config.max_request_body_size;
    let app: Router = Router::new()
        .nest("/api/v2", modules::v2_router())
        .nest("/ws", modules::ws_router())
        .layer(RequestBodyLimitLayer::new(max_body))
        .layer(TraceLayer::new_for_http())
        .layer(CorsLayer::permissive())
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
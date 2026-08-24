//! 全局应用状态
//!
//! 以 `Arc<AppState>` 作为 axum Router 的 state 类型，跨 handler 共享。
//! 跨域组件在构造时一次性初始化；CancellationToken 用于优雅退出后台任务。

use std::sync::Arc;

use sqlx::PgPool;
use tokio_util::sync::CancellationToken;

use crate::auth::session::SessionStore;
use crate::infra::config::AppConfig;
use crate::infra::cos::CosClient;
use crate::infra::snowflake::SnowflakeIdGenerator;
use crate::infra::ws_hub::WsHub;

pub struct AppState {
    pub pool: PgPool,
    pub config: Arc<AppConfig>,
    pub snowflake: Arc<SnowflakeIdGenerator>,
    pub ws_hub: Arc<WsHub>,
    pub cos: Arc<dyn CosClient>,
    pub shutdown: CancellationToken,
    /// Redis 服务端 session 真相源（access/refresh token 吊销）
    pub session: Arc<dyn SessionStore>,
}

impl AppState {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        pool: PgPool,
        config: Arc<AppConfig>,
        snowflake: Arc<SnowflakeIdGenerator>,
        ws_hub: Arc<WsHub>,
        cos: Arc<dyn CosClient>,
        shutdown: CancellationToken,
        session: Arc<dyn SessionStore>,
    ) -> Self {
        Self {
            pool,
            config,
            snowflake,
            ws_hub,
            cos,
            shutdown,
            session,
        }
    }
}
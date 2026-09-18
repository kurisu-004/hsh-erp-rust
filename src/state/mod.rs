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
use crate::infra::python_sts::PythonSts;
use crate::infra::snowflake::SnowflakeIdGenerator;
use crate::infra::ws_hub::WsHub;
use crate::modules::upload_session::repo::UploadSessionRepo;

pub struct AppState {
    pub pool: PgPool,
    pub config: Arc<AppConfig>,
    pub snowflake: Arc<SnowflakeIdGenerator>,
    pub ws_hub: Arc<WsHub>,
    pub cos: Arc<dyn CosClient>,
    /// 2026-09-18 新增：python STS 签发器（HttpPythonSts / NoopPythonSts）。
    ///
    /// 上传会话域 `get_or_create` / `renew` 通过本 trait 调 python 后端
    /// `POST /api/v1/files/sts-prefix-credentials` 签发 prefix-scoped STS。
    ///
    /// 注：原 `state.sts: Arc<dyn StsCredentialIssuer>`（`TencentSts` / `NoopSts`）
    /// 2026-09-18 已删除——rust 后端不再直连腾讯云 STS，改为转发 python 后端。
    pub python_sts: Arc<dyn PythonSts>,
    pub shutdown: CancellationToken,
    /// Redis 服务端 session 真相源（access/refresh token 吊销）
    pub session: Arc<dyn SessionStore>,
    /// 2026-09-18 新增：上传会话 Redis 存储（`upload_session:{user_id}:{scope}` 键）。
    /// 与 `session` 同池（共用 `state.redis_pool`），通过 `cfg.redis.session_check_enabled`
    /// 决定真实 Redis 实现还是 Noop 占位。
    pub upload_session_repo: Arc<dyn UploadSessionRepo>,
}

impl AppState {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        pool: PgPool,
        config: Arc<AppConfig>,
        snowflake: Arc<SnowflakeIdGenerator>,
        ws_hub: Arc<WsHub>,
        cos: Arc<dyn CosClient>,
        python_sts: Arc<dyn PythonSts>,
        shutdown: CancellationToken,
        session: Arc<dyn SessionStore>,
        upload_session_repo: Arc<dyn UploadSessionRepo>,
    ) -> Self {
        Self {
            pool,
            config,
            snowflake,
            ws_hub,
            cos,
            python_sts,
            shutdown,
            session,
            upload_session_repo,
        }
    }
}

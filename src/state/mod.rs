//! 全局应用状态
//!
//! 以 `Arc<AppState>` 作为 axum Router 的 state 类型，跨 handler 共享。
//! 跨域组件在构造时一次性初始化；CancellationToken 用于优雅退出后台任务。
//!
//! 2026-09-21 事务分层重构：iam 域不再注入 `IamUowProvider`（事务移交 handler 后，
//! service 仅持雪花 ID 生成器 / 配置 / session store / 跨域委托）。handler 自行
//! `state.pool.begin()` / `acquire()`。
//!
//! 2026-09-22 Group C 重构：`cnc_program_service` / `part_file_service` 装线。
//! 两个 service 字段仅 `Arc<SnowflakeIdGenerator>` + `Arc<dyn CosClient>`，事务由
//! handler 借 `&mut *tx` / `&mut *conn` 喂给 trait（`CncProgramRepoTrait` /
//! `PartFileRepoTrait`，均直接 `impl for &mut PgConnection`）。

use std::sync::Arc;

use sqlx::PgPool;
use tokio_util::sync::CancellationToken;

use crate::auth::session::SessionStore;
use crate::infra::config::AppConfig;
use crate::infra::cos::CosClient;
use crate::infra::python_sts::PythonSts;
use crate::infra::snowflake::SnowflakeIdGenerator;
use crate::infra::ws_hub::WsHub;
use crate::modules::cnc_program::service::CncProgramService;
use crate::modules::com::applicant::service::ApplicantService;
use crate::modules::com::customer::service::CustomerService;
use crate::modules::iam::service::{AccountService, SessionService};
use crate::modules::part_file::service::PartFileService;
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
    /// 2026-09-19 IAM 域合并：原 `user_service` 重命名为 `account_service`，承载
    /// 账号 CRUD + 角色管理 + 改密（service::AccountService，原 UserService）。
    /// 2026-09-21 事务分层重构：字段仅 `snowflake`；handler 借连接传入 repo。
    pub account_service: Arc<AccountService>,
    /// 2026-09-19 IAM 域合并：原 `auth_service` 重命名为 `session_service`，承载
    /// login / refresh / me / logout / change-password（service::SessionService，原 AuthService）。
    /// 2026-09-21 事务分层重构：字段去掉 `uow_provider`；handler 借连接传入 repo。
    pub session_service: Arc<SessionService>,
    /// 2026-09-22 新增：com/customer service（CRUD）。字段仅 `snowflake`；handler
    /// 借 `&mut *tx` / `&mut *conn` 喂给 `CustomerRepo` trait。
    pub customer_service: Arc<CustomerService>,
    /// 2026-09-22 新增：com/applicant service（CRUD）。字段仅 `snowflake`；handler
    /// 借 `&mut *tx` 喂两次 trait（`ApplicantRepo` + `CustomerRepo::lookup_names`）。
    pub applicant_service: Arc<ApplicantService>,
    /// 2026-09-22 Group C 新增：part_file service。字段 `Arc<SnowflakeIdGenerator>` +
    /// `Arc<dyn CosClient>`；handler 借 `&mut *tx` 喂给 `PartFileRepoTrait`。
    /// 6 个 handler 端点（upload/list/get_url/get_content/soft_delete）走 handler 管 tx
    /// 范式；`bind_uploaded_file` 是事务范式特例（part 域 caller 不开 tx，由 service 自管）。
    pub part_file_service: Arc<PartFileService>,
    /// 2026-09-22 Group C 新增：cnc_program service。字段 `Arc<SnowflakeIdGenerator>` +
    /// `Arc<dyn CosClient>`；handler 借 `&mut *tx` 喂给 `CncProgramRepoTrait`。
    /// 5 个 handler 端点（upload_cnc_pair / list_pairs_for_part / 3 个 alias）走 handler 管 tx 范式。
    /// 3 个 alias 端点（download-url / content / delete）由 handler 直接转发到
    /// `state.part_file_service.method(&mut *tx, ...)`，不抽 trait 到 part_file。
    pub cnc_program_service: Arc<CncProgramService>,
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
        // 装线（2026-09-21 事务分层重构后无 IamUowProvider）：
        // AccountService::new(snowflake) → SessionService::new(config, session, account_service)
        let account_service = Arc::new(AccountService::new(snowflake.clone()));
        let session_service = Arc::new(SessionService::new(
            config.clone(),
            session.clone(),
            account_service.clone(),
        ));
        // 2026-09-22 com/customer + com/applicant service 装线：仅需 snowflake。
        let customer_service = Arc::new(CustomerService::new(snowflake.clone()));
        let applicant_service = Arc::new(ApplicantService::new(snowflake.clone()));
        // 2026-09-22 Group C 装线：part_file + cnc_program service；字段含 snowflake + cos。
        let part_file_service = Arc::new(PartFileService::new(snowflake.clone(), cos.clone()));
        let cnc_program_service = Arc::new(CncProgramService::new(snowflake.clone(), cos.clone()));
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
            account_service,
            session_service,
            customer_service,
            applicant_service,
            part_file_service,
            cnc_program_service,
        }
    }
}
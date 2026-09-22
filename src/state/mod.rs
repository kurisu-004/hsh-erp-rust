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
use crate::modules::dashboard::service::DashboardService;
use crate::modules::delivery_note::service::{DeliveryGroupService, DeliveryNoteService};
use crate::modules::iam::service::{AccountService, SessionService};
use crate::modules::outsource::service::OutsourceService;
use crate::modules::part_file::service::PartFileService;
use crate::modules::prod::process::service::ProcessService;
use crate::modules::prod::process_chain::service::crud::ProcessChainService;
use crate::modules::prod::work_type::service::WorkTypeProcessService;
use crate::modules::prod::work_type::service::WorkTypeService;
use crate::modules::prod::worker::service::WorkerService;
use crate::modules::prod::worker_pool::service::WorkerPoolService;
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
    /// 与 `session` 同池（共用 `state.redis_pool`）；真实 Redis 实现；上传会话由 Redis TTL 约束。
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
    /// 2026-09-22 D-1 新增：prod/process_chain service（get / upsert）。字段仅
    /// `snowflake`；handler 借 `&mut *tx` / `&mut *conn` 喂给 `ProcessChainRepoTrait`
    /// （trait 已直接 `impl for &mut PgConnection`，2026-09-22 替代任何 `PgProcessChainRepo` 壳）。
    pub process_chain_service: Arc<ProcessChainService>,
    /// 2026-09-22 新增：outsource service（company / quote / shipment 16 端点）。
    /// 字段仅 `snowflake`；handler 借 `&mut *tx` / `&mut *conn` 喂给 `OutsourceRepoTrait`
    /// 胖 trait（impl on `&mut PgConnection`）。
    pub outsource_service: Arc<OutsourceService>,
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
    /// 2026-09-22 D-2-simple 新增：prod/worker service（CRUD + verify-badge）。
    /// 字段仅 `snowflake`；handler 借 `&mut *tx` 喂给 `WorkerRepoTrait`
    /// （trait 已直接 `impl for &mut PgConnection`）。
    pub worker_service: Arc<WorkerService>,
    /// 2026-09-22 D-2-simple 新增：prod/work_type service（CRUD）。
    /// 字段仅 `snowflake`；handler 借 `&mut *tx` 喂给 `WorkTypeRepoTrait`
    /// （胖 trait 已合并 t_work_type + t_work_type_process + 跨域 helper，单 trait 一次借位）。
    pub work_type_service: Arc<WorkTypeService>,
    /// 2026-09-22 D-2-simple 新增：prod/work_type process_mapping service（set / list）；
    /// 2026-09-22 PR6 起合并入 `prod/work_type/service.rs`（不再独立子模块）。
    /// 无字段；handler 借 `&mut *tx` 喂给 `WorkTypeRepoTrait`（胖 trait，
    /// process_mapping 4 方法已合并入 trait）。
    pub work_type_process_service: Arc<WorkTypeProcessService>,
    /// 2026-09-22 D-2-simple 新增：prod/process service（CRUD）。
    /// 字段仅 `snowflake`；handler 借 `&mut *tx` 喂给 `ProcessRepoTrait`
    /// （trait 已直接 `impl for &mut PgConnection`）。
    pub process_service: Arc<ProcessService>,
    /// 2026-09-22 D-6 新增：prod/worker_pool service 注入到 AppState。
    /// unit struct（无字段）；handler 借 `&mut *tx` / `&mut *conn` 喂给 service 静态方法。
    pub worker_pool_service: Arc<WorkerPoolService>,
/// 2026-09-22 Group E 新增：dashboard service（WS-only 大屏）。
    /// unit struct（无字段）；handler 借 `&mut *tx` 喂给 `DashboardRepoTrait` trait
    /// （trait 已直接 `impl for &mut PgConnection`，与 iam 2026-09-22 / shelf 同形）。
    /// dashboard 域只有 snapshot 业务（WS upgrade 拉一次 snapshot + 订阅 ws_hub.broadcast），
    /// handler 三形态 ①（snapshot 单次只读聚合：pool.begin → service → commit）。
    pub dashboard_service: Arc<DashboardService>,
    /// 2026-09-22 D-5 新增：delivery_note service（CRUD + 提交 / 拣货 / 扫码 /
    /// 打印 / 分组）注入到 AppState。字段仅 `snowflake`；handler 借 `&mut *tx`
    /// / `&mut *conn` 喂给 `DeliveryNoteRepoTrait`（trait 已直接 `impl for &mut
    /// PgConnection`）。与 iam / shelf / customer 严格范本对齐（review 第 1 轮
    /// D1 修正：service 形参由 `&mut PgConnection` 改 by-value trait）。
    pub delivery_note_service: Arc<DeliveryNoteService>,
    /// 2026-09-22 D-5 新增：delivery_note P1 分组 service（CRUD）注入到
    /// AppState。字段仅 `snowflake`；handler 借 `&mut *tx` / `&mut *conn`
    /// 喂给 `DeliveryNoteRepoTrait`。
    pub delivery_group_service: Arc<DeliveryGroupService>,
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
        // 2026-09-22 D-1 prod/process_chain service 装线：仅需 snowflake。
        let process_chain_service = Arc::new(ProcessChainService::new(snowflake.clone()));
        // 2026-09-22 outsource service 装线：仅需 snowflake（16 端点 company + quote + shipment）。
        let outsource_service = Arc::new(OutsourceService::new(snowflake.clone()));
        // 2026-09-22 Group C 装线：part_file + cnc_program service；字段含 snowflake + cos。
        let part_file_service = Arc::new(PartFileService::new(snowflake.clone(), cos.clone()));
        let cnc_program_service = Arc::new(CncProgramService::new(snowflake.clone(), cos.clone()));
        // 2026-09-22 D-2-simple prod/worker service 装线：仅需 snowflake。
        let worker_service = Arc::new(WorkerService::new(snowflake.clone()));
        // 2026-09-22 D-2-simple prod/work_type service 装线：仅需 snowflake。
        let work_type_service = Arc::new(WorkTypeService::new(snowflake.clone()));
        // 2026-09-22 D-2-simple prod/work_type process_mapping service 装线：无字段；
        // 2026-09-22 PR6 起合入 prod/work_type/service.rs，本行不变。
        let work_type_process_service = Arc::new(WorkTypeProcessService);
        // 2026-09-22 D-2-simple prod/process service 装线：仅需 snowflake。
        let process_service = Arc::new(ProcessService::new(snowflake.clone()));
        // 2026-09-22 prod/worker_pool service 是 unit struct，无字段依赖，
        // 无需装线到 AppState——handler 直接 `WorkerPoolService::method(&mut *tx, ...)`
        // 静态调用即可（与原 `WorkerPoolService` 调用形态一致，part 域
        // `part/handler/inspection.rs:298` 沿用）。
        let worker_pool_service = Arc::new(WorkerPoolService::new());
// 2026-09-22 Group E dashboard service 是 unit struct（WS-only，4 个聚合 trait call
        // 不需要任何字段依赖——雪花 ID 在 snapshot 里无新增，主键全部借用既有数据）。
        let dashboard_service = Arc::new(DashboardService);
        // 2026-09-22 D-5 delivery_note service 装线：仅需 snowflake（事务 / WS
        // 广播 / session 全部移交 handler；review 第 1 轮 D1 修正后 service
        // 装线是 by-value trait 形参的硬性前提）。
        let delivery_note_service = Arc::new(DeliveryNoteService::new(snowflake.clone()));
        let delivery_group_service = Arc::new(DeliveryGroupService::new(snowflake.clone()));
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
            process_chain_service,
            outsource_service,
            part_file_service,
            cnc_program_service,
            worker_service,
            work_type_service,
            work_type_process_service,
            process_service,
            worker_pool_service,
            dashboard_service,
            delivery_note_service,
            delivery_group_service,
        }
    }
}

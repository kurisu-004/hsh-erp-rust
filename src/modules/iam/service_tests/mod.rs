//! iam 域 service 共享测试 helper + 子模块声明
//!
//! 两个 `#[cfg(test)] mod` 共享本模块：
//! - `session_tests`（原 `auth/service_tests.rs` 30 例）
//! - `account_tests`（原 `user/service_tests.rs` 65 例）
//!
//! ## 拆分原因
//! `iam/service_tests.rs` 原本是单文件 ~1700 行；按 conventions §2（1000 行硬红线）
//! 拆为目录式子模块，与 `delivery_note/service/` 子目录正典一致。
//!
//! ## 共享形态（2026-09-21 事务分层重构后）
//! - 构造器：`test_config()` / `manager_current()` / `clerk_current()` /
//!   `sample_user(...)` / `sample_role_row(...)` / `sample_shelf(...)` / `sample_menu(...)`
//!   / `make_session_service(account_svc)` / `test_state(...)`
//! - `make_refresh_token(...)`：用本模块的 `test_config()` 签一对齐 secret/issuer 的 refresh token
//!
//! ## 与重构前的差异（plan v4 §3 V7 → §5.2）
//! - service 不再 commit/rollback ——`IamUowFlags` / `MockIamUnitOfWork` / `provider_returning`
//!   全部删除；测试改用 `MockIamRepo` 直接注入方法参数。
//! - `test_state` 不再构造 `SqlxIamUowProvider`；AccountService 直接装线（只持 snowflake）。

#![allow(clippy::needless_borrow, clippy::redundant_clone)]

use std::sync::Arc;

use chrono::NaiveDateTime;
use tokio_util::sync::CancellationToken;

use crate::auth::rbac::CurrentUser;
use crate::auth::session::SessionStore;
use crate::infra::config::{
    AppConfig, AutoCompleteConfig, CosBackend, CosConfig, JwtConfig, RedisConfig, SnowflakeConfig,
    UploadSessionConfig,
};
use crate::infra::cos::NoopCos;
use crate::infra::python_sts::NoopPythonSts;
use crate::infra::snowflake::SnowflakeIdGenerator;
use crate::infra::ws_hub::WsHub;
use crate::modules::iam::model::{Menu, Shelf, User};
use crate::modules::iam::repo::UserRoleRow;
use crate::modules::iam::service::{AccountService, SessionService};
use crate::modules::upload_session::repo::InMemoryUploadSessionRepo;
use crate::state::AppState;
use crate::modules::com::applicant::service::ApplicantService;
use crate::modules::com::customer::service::CustomerService;

#[cfg(test)]
mod session_tests;

#[cfg(test)]
mod account;

// ===========================================================================
// 共享 fixtures / helpers
// ===========================================================================

pub(crate) fn ts() -> NaiveDateTime {
    #[allow(deprecated)]
    NaiveDateTime::from_timestamp_opt(1_700_000_000, 0).unwrap()
}

pub(crate) fn test_config() -> Arc<AppConfig> {
    Arc::new(AppConfig {
        database_url: "postgres://test:test@localhost:1/test".into(),
        listen_addr: "0.0.0.0:3000".into(),
        jwt: JwtConfig {
            secret: "test-secret-32-chars-min-for-hs256-padding-AAA".into(),
            issuer: "test".into(),
            access_ttl_hours: 1,
            refresh_ttl_days: 7,
        },
        cos: CosConfig {
            backend: CosBackend::OpenDal,
            enabled: false,
            region: "ap-shanghai".into(),
            bucket: "test".into(),
            secret_id: String::new(),
            secret_key: String::new(),
            app_id: String::new(),
            endpoint: String::new(),
            scheme: "https".into(),
            upload_prefix: "uploads".into(),
            presign_expire_seconds: 3600,
            max_file_size: 1024,
            tmp_prefix: "tmp/".into(),
        },
        snowflake: SnowflakeConfig {
            instance: 1,
            epoch_ms: 1_735_689_600_000,
        },
        max_request_body_size: 1024 * 1024,
        auto_complete: AutoCompleteConfig {
            threshold_days: 7,
            interval_hours: 24,
        },
        redis: RedisConfig {
            url: "redis://localhost".into(),
            session_ttl_seconds: 3600,
            pool_max_size: 1,
            session_check_enabled: false,
        },
        delivery_note_template_dir: std::path::PathBuf::from("/tmp"),
        enable_e2e_hooks: false,
        ws_heartbeat_interval_seconds: 30,
        request_timeout_seconds: 30,
        upload_session: UploadSessionConfig {
            python_backend_base_url: "http://localhost:8000".into(),
            ttl_seconds: 86_400,
            sts_duration_seconds: 7_200,
            renew_threshold_seconds: 600,
        },
    })
}

/// 构造 `SessionService`（轻壳），供 service_tests/session_tests 用。
pub(crate) fn make_session_service(
    session: Arc<dyn SessionStore>,
    account_service: Arc<AccountService>,
) -> Arc<SessionService> {
    Arc::new(SessionService::new(
        test_config(),
        session,
        account_service,
    ))
}

/// 构造最小 `AppState`（懒连 pool，不触 DB）。service 方法的 `state` 形参几乎不用，仅满足类型签名。
pub(crate) fn test_state(
    account_service: Arc<AccountService>,
    session_service: Arc<SessionService>,
    session: Arc<dyn SessionStore>,
) -> Arc<AppState> {
    let pool = sqlx::Pool::<sqlx::Postgres>::connect_lazy("postgres://test:test@localhost:1/test")
        .unwrap();
    let snowflake = Arc::new(SnowflakeIdGenerator::new(1_735_689_600_000, 1));
    Arc::new(AppState {
        pool,
        config: test_config(),
        snowflake: snowflake.clone(),
        ws_hub: Arc::new(WsHub::new()),
        cos: Arc::new(NoopCos),
        python_sts: Arc::new(NoopPythonSts),
        shutdown: CancellationToken::new(),
        session,
        upload_session_repo: Arc::new(InMemoryUploadSessionRepo::new()),
        account_service,
        session_service,
        customer_service: Arc::new(CustomerService::new(snowflake.clone())),
        applicant_service: Arc::new(ApplicantService::new(snowflake.clone())),
        // 2026-09-22 Group C 新增：part_file + cnc_program service；unit tests 不
        // 直接走这两个 service（service_tests 仅覆盖 iam），传占位实例即可。
        part_file_service: Arc::new(
            crate::modules::part_file::service::PartFileService::new(
                snowflake.clone(),
                Arc::new(NoopCos),
            ),
        ),
        cnc_program_service: Arc::new(
            crate::modules::cnc_program::service::CncProgramService::new(
                snowflake.clone(),
                Arc::new(NoopCos),
            ),
        ),
    })
}

pub(crate) fn manager_current() -> CurrentUser {
    CurrentUser {
        id: 42,
        username: "alice".into(),
        roles: vec![crate::auth::rbac::Role::Manager],
        shelf_ids: vec![],
        shelf_wildcard: false,
    }
}

pub(crate) fn clerk_current() -> CurrentUser {
    CurrentUser {
        id: 99,
        username: "carol".into(),
        roles: vec![crate::auth::rbac::Role::Clerk],
        shelf_ids: vec![],
        shelf_wildcard: false,
    }
}

pub(crate) fn sample_user(id: i64, username: &str, hash: &str) -> User {
    User {
        id,
        username: username.into(),
        password_hash: hash.into(),
        full_name: "Full".into(),
        phone: None,
        is_active: true,
        last_login_at: None,
        refresh_token_version: 0,
        version: 0,
        created_at: ts(),
        created_by: None,
        updated_at: ts(),
        updated_by: None,
        deleted_at: None,
    }
}

pub(crate) fn sample_role_row(
    id: i64,
    user_id: i64,
    role: &str,
    scope_id: Option<i64>,
) -> UserRoleRow {
    UserRoleRow {
        id,
        user_id,
        role: role.into(),
        scope_type: if role == "SHELF_ACCOUNT" {
            Some("shelf".into())
        } else {
            None
        },
        scope_id,
        version: 0,
        created_at: ts(),
        created_by: None,
        updated_at: ts(),
        updated_by: None,
        deleted_at: None,
        shelf_code: None,
        shelf_name: None,
    }
}

pub(crate) fn sample_shelf(id: i64, code: &str, name: &str, zone: &str, is_active: bool) -> Shelf {
    Shelf {
        id,
        code: code.into(),
        name: name.into(),
        zone: zone.into(),
        location: None,
        is_active,
        display_order: 0,
        version: 0,
        created_at: ts(),
        created_by: None,
        updated_at: ts(),
        updated_by: None,
        deleted_at: None,
    }
}

pub(crate) fn sample_menu(id: i64, code: &str, title: &str) -> Menu {
    Menu {
        id,
        parent_id: None,
        code: code.into(),
        title: title.into(),
        path: None,
        icon: None,
        sort_order: 0,
        is_active: true,
        version: 0,
        created_at: ts(),
        created_by: None,
        updated_at: ts(),
        updated_by: None,
        deleted_at: None,
    }
}

/// 签发一个对应当前 secret/issuer 的 refresh token。
pub(crate) fn make_refresh_token(sub: i64, ver: i32) -> String {
    let cfg = test_config();
    crate::auth::jwt::encode_refresh(
        sub,
        ver,
        &cfg.jwt.secret,
        &cfg.jwt.issuer,
        cfg.jwt.refresh_ttl_days,
    )
    .unwrap()
    .0
}

//! 应用错误类型 + 错误码常量段
//! 对应 Python myERP/core/error_code.py + exception*.py
//!
//! ## 错误码分段契约（与 Python 前端保持兼容）
//! - `0`          成功
//! - `4xxxx`      HTTP 语义（40000 BAD_REQUEST、40001 VALIDATION、40100 UNAUTHORIZED、
//!   40101 BIZ_AUTH_INVALID、40102 TOKEN_EXPIRED、40103 REFRESH_INVALID、40104 OLD_PASSWORD_MISMATCH、
//!   40300 FORBIDDEN、40301 SHELF_MISMATCH、40400 NOT_FOUND、40901 VERSION_CONFLICT、41301 REQUEST_TOO_LARGE）
//!   Auth 业务码与通用 UNAUTHORIZED 的区别：业务码携带细分原因。
//! - `5xxxx`      系统错误（50000 INTERNAL、50001 DATABASE）
//! - `2xxxx`      业务域错误：200xx 用户、201xx 零件/客户、202xx 工人、203xx 装配体、
//!   204xx/211xx 文件、205xx 货架、206xx 账号、208xx 工序、209xx 工种、210xx 申请人、
//!   212xx 外协公司、213xx 外协报价、214xx 送货单、215xx 外协发货。
//! 业务实现阶段在域内自行定义并写入文档。

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use thiserror::Error;

use crate::shared::response::R;

/// 错误码常量（沿用 Python 数字契约）
pub mod code {
    pub const SUCCESS: i32 = 0;

    // HTTP 语义
    pub const BAD_REQUEST: i32 = 40000;
    pub const VALIDATION_ERROR: i32 = 40001;
    pub const UNAUTHORIZED: i32 = 40100;

    // Auth 域业务码（401xx，HTTP 语义层里的业务码，区别于通用 40100 UNAUTHORIZED）
    pub const BIZ_AUTH_INVALID: i32 = 40101;        // 登录：用户不存在/已删/已停用/密码错统一
    pub const TOKEN_EXPIRED: i32 = 40102;
    pub const REFRESH_INVALID: i32 = 40103;         // refresh token 失效/版本不匹配/用户停用
    pub const OLD_PASSWORD_MISMATCH: i32 = 40104;   // 修改密码时旧密码错误

    pub const FORBIDDEN: i32 = 40300;

    // 货架越权
    pub const SHELF_MISMATCH: i32 = 40301;

    pub const NOT_FOUND: i32 = 40400;
    pub const VERSION_CONFLICT: i32 = 40901;
    pub const REQUEST_TOO_LARGE: i32 = 41301;

    // User/Account 域业务码（206xx，详见 docs/architecture.md 错误码分段）
    pub const USER_NOT_FOUND: i32 = 20601;
    pub const DUPLICATE_USERNAME: i32 = 20602;
    pub const ROLE_DUPLICATE: i32 = 20604;
    pub const ROLE_NOT_FOUND: i32 = 20605;
    pub const NO_ROLE: i32 = 20606;

    // 系统错误
    pub const INTERNAL: i32 = 50000;
    pub const DATABASE: i32 = 50001;
}

#[derive(Debug, Error)]
pub enum AppError {
    /// 业务错误：自定义错误码 + 自定义 HTTP 状态码
    #[error("[{code}] {message}")]
    Biz {
        code: i32,
        message: String,
        http: StatusCode,
    },

    /// 校验失败（40001，HTTP 422）
    #[error("校验失败: {0}")]
    Validation(String),

    /// 未授权（40100，HTTP 401）
    #[error("未授权: {0}")]
    Unauthorized(String),

    /// 禁止访问（40300，HTTP 403）
    #[error("禁止访问")]
    Forbidden,

    /// JWT 错误（40100，HTTP 401）
    #[error("JWT: {0}")]
    Jwt(String),

    /// 数据库错误（50001，HTTP 500）
    #[error("数据库错误")]
    Database(#[from] sqlx::Error),

    /// 内部错误（50000，HTTP 500）
    #[error("内部错误: {0}")]
    Internal(String),
}

impl AppError {
    /// 构造业务错误（HTTP 状态码由 code 自动推导）
    pub fn biz(code: i32, message: impl Into<String>) -> Self {
        Self::Biz {
            code,
            message: message.into(),
            http: status_from_code(code),
        }
    }

    pub fn validation(msg: impl Into<String>) -> Self {
        Self::Validation(msg.into())
    }

    pub fn unauthorized(msg: impl Into<String>) -> Self {
        Self::Unauthorized(msg.into())
    }

    pub fn internal(msg: impl Into<String>) -> Self {
        Self::Internal(msg.into())
    }

    pub fn code(&self) -> i32 {
        match self {
            Self::Biz { code, .. } => *code,
            Self::Validation(_) => code::VALIDATION_ERROR,
            Self::Unauthorized(_) => code::UNAUTHORIZED,
            Self::Forbidden => code::FORBIDDEN,
            Self::Jwt(_) => code::UNAUTHORIZED,
            Self::Database(_) => code::DATABASE,
            Self::Internal(_) => code::INTERNAL,
        }
    }

    pub fn http_status(&self) -> StatusCode {
        match self {
            Self::Biz { http, .. } => *http,
            Self::Validation(_) => StatusCode::UNPROCESSABLE_ENTITY,
            Self::Unauthorized(_) | Self::Jwt(_) => StatusCode::UNAUTHORIZED,
            Self::Forbidden => StatusCode::FORBIDDEN,
            Self::Database(_) => StatusCode::INTERNAL_SERVER_ERROR,
            Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

/// 由错误码推导 HTTP 状态码（与 Python 错误码分段一致）
fn status_from_code(c: i32) -> StatusCode {
    match c {
        c if c == code::BAD_REQUEST => StatusCode::BAD_REQUEST,
        c if c == code::VALIDATION_ERROR => StatusCode::UNPROCESSABLE_ENTITY,
        c if c == code::UNAUTHORIZED => StatusCode::UNAUTHORIZED,
        c if c == code::TOKEN_EXPIRED => StatusCode::UNAUTHORIZED,
        c if c == code::BIZ_AUTH_INVALID
            || c == code::REFRESH_INVALID
            || c == code::OLD_PASSWORD_MISMATCH => StatusCode::UNAUTHORIZED,
        c if c == code::SHELF_MISMATCH => StatusCode::FORBIDDEN,
        c if c == code::USER_NOT_FOUND || c == code::ROLE_NOT_FOUND => StatusCode::NOT_FOUND,
        c if c == code::DUPLICATE_USERNAME || c == code::ROLE_DUPLICATE => StatusCode::CONFLICT,
        c if c == code::NO_ROLE => StatusCode::FORBIDDEN,
        c if c == code::FORBIDDEN => StatusCode::FORBIDDEN,
        c if c == code::NOT_FOUND => StatusCode::NOT_FOUND,
        c if c == code::VERSION_CONFLICT => StatusCode::CONFLICT,
        c if c == code::REQUEST_TOO_LARGE => StatusCode::PAYLOAD_TOO_LARGE,
        c if c == code::INTERNAL || c == code::DATABASE => StatusCode::INTERNAL_SERVER_ERROR,
        c if (40000..50000).contains(&c) => StatusCode::BAD_REQUEST,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let status = self.http_status();
        // 不向客户端泄露 sqlx 内部信息
        let message = match &self {
            AppError::Database(e) => {
                tracing::error!(error = %e, "数据库错误");
                "数据库错误".to_string()
            }
            other => other.to_string(),
        };
        let body = R::<()> {
            code: self.code(),
            message,
            data: None,
        };
        (status, Json(body)).into_response()
    }
}
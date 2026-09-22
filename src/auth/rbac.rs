//! RBAC 五角色 + 当前用户结构 + JWT AccessTokenClaims
//!
//! ## 角色
//! - `Manager`：超级权限（业务层自行判断是否豁免）
//! - `Clerk`：文员
//! - `Inspector`：品检员
//! - `CncProgrammer`：CNC 程序员
//! - `ShelfAccount`：货架一体机专用账号，必须 scope 到具体 `shelf_id`
//!
//! ## SHELF_ACCOUNT 货架范围
//! - `shelf_ids`：可访问的具体货架列表
//! - `shelf_wildcard`：是否对所有货架放行（仅 Manager 标志）
//!
//! ## JWT 字段（2026-09-22 重构）
//! - access token 业务字段（username/roles/shelf_ids/shelf_wildcard/ver）已从 JWT 中删除，
//!   全部改走 Redis session 校验 + 服务端缓存；handler/extractor 仍通过 `CurrentUser`
//!   拿到这些上下文。
//! - 标准字段（`sub/aud/iat/nbf/exp/iss/jti/typ`）按 RFC 7519 命名，Rust 结构体字段全词化
//!   （`subject` / `audience` / `issued_at` / `not_before` / `expires_at` / `issuer` / `jwt_id` /
//!   `token_type`），通过 `#[serde(rename = "...")]` 桥接 JSON 短码。

use serde::{Deserialize, Serialize};

use crate::shared::error::{AppError, code};

/// 把 DB / Redis 缓存里的大写 role 字符串转回 `Role` 枚举。
///
/// 仅识别 5 种已知值；未知值打 `tracing::warn!` 并返回 `None`，调用方自行决定是否跳过。
pub fn parse_role_string(s: &str) -> Option<Role> {
    Some(match s {
        "MANAGER" => Role::Manager,
        "CLERK" => Role::Clerk,
        "INSPECTOR" => Role::Inspector,
        "CNC_PROGRAMMER" => Role::CncProgrammer,
        "SHELF_ACCOUNT" => Role::ShelfAccount,
        _ => {
            tracing::warn!(role = %s, "未知 role 字符串，跳过");
            return None;
        }
    })
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum Role {
    #[serde(rename = "MANAGER")]
    Manager,
    #[serde(rename = "CLERK")]
    Clerk,
    #[serde(rename = "INSPECTOR")]
    Inspector,
    #[serde(rename = "CNC_PROGRAMMER")]
    CncProgrammer,
    #[serde(rename = "SHELF_ACCOUNT")]
    ShelfAccount,
}

/// access token 标准字段 + RFC 7519 claim set（业务字段已全部移到 Redis session）
///
/// 2026-09-22 重构：
/// - Rust 字段全词化（subject / audience / issued_at / not_before / expires_at / issuer /
///   jwt_id / token_type），JSON 字段名按 RFC 7519 用短码（sub / aud / iat / nbf / exp / iss /
///   jti / typ），通过 `#[serde(rename = "...")]` 桥接。
/// - 删除 `username` / `roles` / `shelf_ids` / `shelf_wildcard` / `ver` / `deserialize_sub_or_int` /
///   `alias = "type"`：Rust 自签 token 不再携带业务字段，业务上下文从 Redis session 取；
///   `sub` 直接用 `i64`，无须再兼容 Python v1 数字串。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccessTokenClaims {
    /// RFC 7519 `sub`：用户 snowflake id（i64）
    #[serde(rename = "sub")]
    pub subject: i64,
    /// RFC 7519 `aud`：受众；本服务统一用 `JwtConfig::audience`（默认 `hsh-erp-rust`）
    #[serde(rename = "aud")]
    pub audience: String,
    /// RFC 7519 `iat`：issued-at，由签发函数填入 unix 时间
    #[serde(rename = "iat")]
    pub issued_at: i64,
    /// RFC 7519 `nbf`：not-before；默认等于 `issued_at`（签发即生效）
    #[serde(rename = "nbf", default = "default_not_before")]
    pub not_before: i64,
    /// RFC 7519 `exp`：expiration
    #[serde(rename = "exp")]
    pub expires_at: i64,
    /// RFC 7519 `iss`：issuer，对齐 `JwtConfig::issuer`
    #[serde(rename = "iss")]
    pub issuer: String,
    /// RFC 7519 `jti`：JWT ID（UUID v4），防重放审计字段
    #[serde(rename = "jti", default = "default_jwt_id")]
    pub jwt_id: String,
    /// RFC 7519 `typ`：token type；access token 默认 `default_access_token_type`（`"access"`）
    #[serde(rename = "typ", default = "default_access_token_type")]
    pub token_type: String,
}

fn default_access_token_type() -> String {
    "access".into()
}

fn default_not_before() -> i64 {
    // 0 仅作占位；签发时由 encode_access / encode_refresh 覆写
    0
}

fn default_jwt_id() -> String {
    // 0 仅作占位；签发时由 encode_access / encode_refresh 填入 UUID v4
    String::new()
}

/// Handler 中可用的当前登录用户
#[derive(Debug, Clone)]
pub struct CurrentUser {
    pub id: i64,
    pub username: String,
    pub roles: Vec<Role>,
    pub shelf_ids: Vec<i64>,
    pub shelf_wildcard: bool,
}

impl CurrentUser {
    pub fn has_role(&self, role: Role) -> bool {
        self.roles.contains(&role)
    }

    pub fn require_role(&self, role: Role) -> Result<(), AppError> {
        if self.has_role(role) {
            Ok(())
        } else {
            Err(AppError::biz(code::FORBIDDEN, "无权限"))
        }
    }

    pub fn require_any_role(&self, roles: &[Role]) -> Result<(), AppError> {
        if roles.iter().any(|r| self.has_role(*r)) {
            Ok(())
        } else {
            Err(AppError::biz(code::FORBIDDEN, "无权限"))
        }
    }

    /// 货架一体机/有 shelf_ids 限制的用户：判断是否可访问特定 shelf
    pub fn can_access_shelf(&self, shelf_id: i64) -> bool {
        self.shelf_wildcard || self.shelf_ids.contains(&shelf_id) || self.has_role(Role::Manager)
    }
}
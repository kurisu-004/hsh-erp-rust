//! JWT 双 token 编解码（2026-09-22 重构）
//!
//! ## token 类型
//! - `access_token`：短 TTL（默认 12h），payload 仅含 RFC 7519 标准字段（sub/aud/iat/nbf/exp/iss/jti/typ）。
//!   **不携带**业务字段（username/roles/shelf_ids/...），全部改走 Redis session 校验 +
//!   服务端缓存。
//! - `refresh_token`：长 TTL（默认 7d），payload 在 access 字段基础上额外带 `refresh_version`
//!   （用户 DB 轮转字段，JSON 名 `ver`），每次 refresh 后 `t_user.refresh_token_version + 1`，
//!   旧 refresh 即时作废。
//!
//! ## 签发期自动字段
//! - `iat` / `nbf` = 当前 unix 时间（`Utc::now().timestamp()`）
//! - `jti` = `Uuid::new_v4().to_string()`
//! - `aud` = 签发时由 `audience` 参数传入
//! - `typ` = "access" / "refresh"（由 `encode_access` / `encode_refresh` 内部设置）
//!
//! ## 校验
//! - HS256 算法
//! - `iss` 校验：`Validation::set_issuer`
//! - `aud` 校验：`Validation::set_audience`（jsonwebtoken 10.x）
//! - `exp` 校验：默认 `validate_exp = true`，`leeway = 30s`
//! - 必填标准 claim（`Validation::set_required_spec_claims`）：
//!   - access token：`["exp", "aud", "iss"]`
//!   - refresh token：`["exp", "aud", "iss"]`
//!
//! ### 为什么 `aud` 必须列入 `set_required_spec_claims`
//!
//! `set_audience` 只设置"允许的 audience 列表"，**不**把 `aud` 加入
//! `required_spec_claims`。jsonwebtoken 10.x 在 `aud` 校验时：`validate_aud=true` +
//! `set_audience=[...]` 路径下，若 token **缺** `aud` 字段，match 会落到 `_ => {}`
//! 直接 Ok，**漏过校验**。`set_required_spec_claims` 把 `aud` 列为必填，杜绝此类
//! token 漏过校验。
//!
//! ### 为什么 `sub` **不**列入 `set_required_spec_claims`
//!
//! jsonwebtoken 10.x 内部 `ClaimsForValidation.sub` 类型是 `TryParse<Cow<'_, str>>`，
//! **要求 JSON 中 `sub` 字段是字符串**。本仓库 `AccessTokenClaims.subject` /
//! `RefreshTokenClaims.subject` 沿用 `i64`（雪花 ID 数值序列化更紧凑，与 Python 后端
//! 约定对齐），加进 `required_spec_claims` 会让所有合法 token 立刻被 `MissingRequiredClaim`
//! 拒掉。sub 字段的"必填"由 Rust 结构体非 `Option` 字段在 deserialize 阶段兜底：
//! 缺 `sub` → serde "missing field" → jsonwebtoken 错误包 → 40100 UNAUTHORIZED。

use chrono::Utc;
use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation, decode, encode};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::auth::rbac::AccessTokenClaims;
use crate::shared::error::{AppError, code};

/// Refresh token 标准字段 + 业务字段 `refresh_version`（JSON 名 `ver`）
///
/// 2026-09-22 重构：
/// - 字段全词化（subject / audience / issued_at / not_before / expires_at / issuer / jwt_id /
///   token_type / refresh_version），JSON 短码按 RFC 7519 桥接。
/// - Rust 字段 `refresh_version` 对应 JSON `ver`，保持与之前字段名 `ver` 的对外 JSON 形态
///   一致；`AccessTokenClaims` 不带此字段。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefreshTokenClaims {
    /// RFC 7519 `sub`：用户 snowflake id（i64）
    #[serde(rename = "sub")]
    pub subject: i64,
    /// RFC 7519 `aud`：受众；与 `JwtConfig::audience` 一致
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
    /// RFC 7519 `typ`：token type；refresh token 默认 `default_refresh_token_type`（`"refresh"`）
    #[serde(rename = "typ", default = "default_refresh_token_type")]
    pub token_type: String,
    /// 业务字段：用户在 DB 端的 refresh 版本号，refresh 时校验匹配。JSON 字段名仍为 `ver`
    /// （保持向后兼容）。
    #[serde(rename = "ver")]
    pub refresh_version: i32,
}

fn default_refresh_token_type() -> String {
    "refresh".into()
}

fn default_not_before() -> i64 {
    // 0 仅作占位；签发时由 encode_access / encode_refresh 覆写
    0
}

fn default_jwt_id() -> String {
    // 0 仅作占位；签发时由 encode_access / encode_refresh 填入 UUID v4
    String::new()
}

/// 双 token 签发结果
///
/// 2026-09-23 重构：新增 `access_jti` / `refresh_jti` 字段（UUID v4）。
/// jti 直接写入 JWT payload（`claims.jwt_id`），同时充当 Redis session cache key
/// 的后缀（`session:tok:<jti>`），与 sha256 派生 key 完全解耦。
#[derive(Debug, Clone)]
pub struct TokenPair {
    pub access_token: String,
    pub refresh_token: String,
    pub access_jti: String,
    pub refresh_jti: String,
    pub access_expires_at: i64,
    pub refresh_expires_at: i64,
}

/// 签发 access token：自动填 `iat` / `nbf` / `jti` / `aud` / `typ`；`expires_at` 按 ttl 算。
///
/// 2026-09-23 重构：返回 `(token, jti, exp)` 三元组。`jti` 即 JWT payload 中
/// `claims.jwt_id`（UUID v4），是 Redis session key `session:tok:<jti>` 的
/// 后缀来源，业务层据此写入 Redis 而无需对 token 做哈希。
pub fn encode_access(
    secret: &str,
    issuer: &str,
    audience: &str,
    subject: i64,
    ttl_hours: i64,
) -> Result<(String, String, i64), AppError> {
    let now = Utc::now().timestamp();
    let exp = now + ttl_hours * 3600;
    let jti = Uuid::new_v4().to_string();
    let claims = AccessTokenClaims {
        subject,
        audience: audience.to_string(),
        issued_at: now,
        not_before: now,
        expires_at: exp,
        issuer: issuer.to_string(),
        jwt_id: jti.clone(),
        token_type: "access".into(),
    };
    encode(
        &Header::new(Algorithm::HS256),
        &claims,
        &EncodingKey::from_secret(secret.as_bytes()),
    )
    .map(|t| (t, jti, exp))
    .map_err(|e| AppError::biz(code::INTERNAL, format!("jwt encode: {e}")))
}

/// 解码 access token
///
/// 2026-09-20 修改：`jsonwebtoken::errors::ErrorKind::ExpiredSignature` 单独映射
/// 到 `code::TOKEN_EXPIRED` (40102)，便于前端细分提示「请用 refresh token 续期」
/// vs 其它签名错误（40100 仍是「请重新登录」）。其余 ErrorKind 仍走 40100。
///
/// 2026-09-22 重构：增加 `audience` 参数，`Validation::set_audience` 强校验。
pub fn decode_access(
    token: &str,
    secret: &str,
    issuer: &str,
    audience: &str,
) -> Result<AccessTokenClaims, AppError> {
    let mut v = Validation::new(Algorithm::HS256);
    v.set_issuer(&[issuer]);
    v.set_audience(&[audience]);
    // 2026-09-22 重构：显式声明必填标准 claim，杜绝"缺 aud 的 token 漏过校验"；
    // `sub` 由 Rust 结构体非 Option 字段 deserialize 兜底（详见模块 docstring）。
    v.set_required_spec_claims(&["exp", "aud", "iss"]);
    // 30s leeway 容忍跨节点时钟漂移
    v.leeway = 30;
    decode::<AccessTokenClaims>(
        token,
        &DecodingKey::from_secret(secret.as_bytes()),
        &v,
    )
    .map(|d| d.claims)
    .map_err(|e| match e.kind() {
        jsonwebtoken::errors::ErrorKind::ExpiredSignature => {
            AppError::biz(code::TOKEN_EXPIRED, "token expired")
        }
        _ => AppError::biz(code::UNAUTHORIZED, format!("jwt: {e}")),
    })
}

/// 签发 refresh token：自动填 `iat` / `nbf` / `jti` / `aud` / `typ`；`expires_at` 按 ttl 算。
///
/// 2026-09-23 重构：返回 `(token, jti, exp)` 三元组，语义同 `encode_access`。
pub fn encode_refresh(
    secret: &str,
    issuer: &str,
    audience: &str,
    subject: i64,
    refresh_version: i32,
    ttl_days: i64,
) -> Result<(String, String, i64), AppError> {
    let now = Utc::now().timestamp();
    let exp = now + ttl_days * 86_400;
    let jti = Uuid::new_v4().to_string();
    let claims = RefreshTokenClaims {
        subject,
        audience: audience.to_string(),
        issued_at: now,
        not_before: now,
        expires_at: exp,
        issuer: issuer.to_string(),
        jwt_id: jti.clone(),
        token_type: "refresh".into(),
        refresh_version,
    };
    encode(
        &Header::new(Algorithm::HS256),
        &claims,
        &EncodingKey::from_secret(secret.as_bytes()),
    )
    .map(|t| (t, jti, exp))
    .map_err(|e| AppError::biz(code::INTERNAL, format!("refresh encode: {e}")))
}

/// 解码 refresh token
///
/// 2026-09-22 重构：增加 `audience` 参数，`Validation::set_audience` 强校验。
pub fn decode_refresh(
    token: &str,
    secret: &str,
    issuer: &str,
    audience: &str,
) -> Result<RefreshTokenClaims, AppError> {
    let mut v = Validation::new(Algorithm::HS256);
    v.set_issuer(&[issuer]);
    v.set_audience(&[audience]);
    // 2026-09-22 重构：refresh token 必填 exp/aud/iss；`sub` 由 Rust 结构体非 Option
    // 字段 deserialize 兜底（详见模块 docstring）。
    v.set_required_spec_claims(&["exp", "aud", "iss"]);
    v.leeway = 30;
    decode::<RefreshTokenClaims>(
        token,
        &DecodingKey::from_secret(secret.as_bytes()),
        &v,
    )
    .map(|d| d.claims)
    .map_err(|e| AppError::biz(code::UNAUTHORIZED, format!("refresh: {e}")))
}

/// 业务层调用此函数完成一次登录或 refresh：返回 access+refresh 配对
///
/// 2026-09-22 重构：参数从 10 个简化为 7 个——移除 `username/roles/shelf_ids/shelf_wildcard`
/// （业务字段不再带进 JWT）；新增 `audience`；增加 `refresh_version` 作为 refresh 校验字段。
///
/// 2026-09-23 重构：内部使用 `encode_access` / `encode_refresh` 的三元组返回值，
/// 把各自的 `jti` 写入 `TokenPair`，业务层据此分别去 Redis 创建 access / refresh
/// session。
pub fn issue_token_pair(
    subject: i64,
    refresh_version: i32,
    secret: &str,
    issuer: &str,
    audience: &str,
    access_ttl_hours: i64,
    refresh_ttl_days: i64,
) -> Result<TokenPair, AppError> {
    let (access_token, access_jti, access_exp) =
        encode_access(secret, issuer, audience, subject, access_ttl_hours)?;
    let (refresh_token, refresh_jti, refresh_exp) = encode_refresh(
        secret,
        issuer,
        audience,
        subject,
        refresh_version,
        refresh_ttl_days,
    )?;
    Ok(TokenPair {
        access_token,
        refresh_token,
        access_jti,
        refresh_jti,
        access_expires_at: access_exp,
        refresh_expires_at: refresh_exp,
    })
}

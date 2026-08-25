//! JWT 双 token 编解码
//!
// 对应 Python myERP/core/security.py：
// - `access_token`：短 TTL（默认 12h），payload 含 sub/username/roles/shelf_ids/type="access"
// - `refresh_token`：长 TTL（默认 7d），payload 只含 sub+ver+type="refresh"，每次 refresh 后
//!   `t_user.refresh_token_version + 1`，旧 refresh 即时作废。
//!
//! HS256 算法；`iss` 在校验时绑定到 `Jwt_ISSUER`。

use chrono::Utc;
use jsonwebtoken::{decode, encode, Algorithm, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};

use crate::auth::rbac::{Claims, Role};
use crate::shared::error::{code, AppError};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefreshClaims {
    pub sub: i64,
    pub ver: i32,
    #[serde(default = "default_refresh_type")]
    pub typ: String,
    pub iss: String,
    pub exp: i64,
}

fn default_refresh_type() -> String {
    "refresh".into()
}

/// 双 token 签发结果
#[derive(Debug, Clone)]
pub struct TokenPair {
    pub access_token: String,
    pub refresh_token: String,
    pub access_expires_at: i64,
    pub refresh_expires_at: i64,
}

/// 签发 access token（含 roles/shelf_ids 等业务字段）
pub fn encode_access(
    claims: &Claims,
    secret: &str,
    issuer: &str,
    ttl_hours: i64,
) -> Result<(String, i64), AppError> {
    let exp = Utc::now().timestamp() + ttl_hours * 3600;
    let mut c = claims.clone();
    c.exp = exp;
    c.iss = issuer.to_string();
    c.typ = "access".into();
    encode(
        &Header::new(Algorithm::HS256),
        &c,
        &EncodingKey::from_secret(secret.as_bytes()),
    )
    .map(|t| (t, exp))
    .map_err(|e| AppError::biz(code::INTERNAL, format!("jwt encode: {e}")))
}

/// 解码 access token
pub fn decode_access(token: &str, secret: &str, issuer: &str) -> Result<Claims, AppError> {
    let mut v = Validation::new(Algorithm::HS256);
    v.set_issuer(&[issuer]);
    decode::<Claims>(token, &DecodingKey::from_secret(secret.as_bytes()), &v)
        .map(|d| d.claims)
        .map_err(|e| AppError::biz(code::UNAUTHORIZED, format!("jwt: {e}")))
}

/// 签发 refresh token（仅含 sub + ver）
pub fn encode_refresh(
    sub: i64,
    ver: i32,
    secret: &str,
    issuer: &str,
    ttl_days: i64,
) -> Result<(String, i64), AppError> {
    let exp = Utc::now().timestamp() + ttl_days * 86_400;
    let c = RefreshClaims {
        sub,
        ver,
        typ: "refresh".into(),
        iss: issuer.into(),
        exp,
    };
    encode(
        &Header::new(Algorithm::HS256),
        &c,
        &EncodingKey::from_secret(secret.as_bytes()),
    )
    .map(|t| (t, exp))
    .map_err(|e| AppError::biz(code::INTERNAL, format!("refresh encode: {e}")))
}

/// 解码 refresh token
pub fn decode_refresh(
    token: &str,
    secret: &str,
    issuer: &str,
) -> Result<(i64, i32), AppError> {
    let mut v = Validation::new(Algorithm::HS256);
    v.set_issuer(&[issuer]);
    decode::<RefreshClaims>(token, &DecodingKey::from_secret(secret.as_bytes()), &v)
        .map(|d| (d.claims.sub, d.claims.ver))
        .map_err(|e| AppError::biz(code::UNAUTHORIZED, format!("refresh: {e}")))
}

/// 业务层调用此函数完成一次登录或 refresh：返回 access+refresh 配对
#[allow(clippy::too_many_arguments)]
pub fn issue_token_pair(
    sub: i64,
    username: &str,
    roles: &[Role],
    shelf_ids: &[i64],
    shelf_wildcard: bool,
    refresh_ver: i32,
    secret: &str,
    issuer: &str,
    access_ttl_hours: i64,
    refresh_ttl_days: i64,
) -> Result<TokenPair, AppError> {
    let claims = Claims {
        sub,
        username: username.to_string(),
        roles: roles.to_vec(),
        shelf_ids: shelf_ids.to_vec(),
        shelf_wildcard,
        ver: refresh_ver,
        typ: "access".into(),
        iss: issuer.into(),
        exp: 0, // encode_access 会覆写
    };
    let (access_token, access_exp) = encode_access(&claims, secret, issuer, access_ttl_hours)?;
    let (refresh_token, refresh_exp) =
        encode_refresh(sub, refresh_ver, secret, issuer, refresh_ttl_days)?;
    Ok(TokenPair {
        access_token,
        refresh_token,
        access_expires_at: access_exp,
        refresh_expires_at: refresh_exp,
    })
}
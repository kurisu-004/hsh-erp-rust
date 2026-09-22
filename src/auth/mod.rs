//! 横切认证授权模块
//!
//! 2026-09-22 重构：删 Python myERP JWT 兼容，全词化命名，access token 不再携带业务字段。
//! - `jwt`：access / refresh 双 token 编解码（HS256）；access 不携带业务字段，仅含
//!   RFC 7519 标准字段（sub/aud/iat/nbf/exp/iss/jti/typ）+ Rust 全词化字段名。
//! - `password`：bcrypt 散列/校验
//! - `rbac`：五角色定义、`CurrentUser`、`AccessTokenClaims`
//! - `extractor`：axum `FromRequestParts` 把 `CurrentUser` / `SessionJti` 从
//!   request extensions 里读出来（由 `middleware` 注入）
//! - `middleware`：axum `authenticate_middleware` —— Bearer JWT 验签 + Redis session 校验 +
//!   滑动 TTL 集中处理；公开路径白名单；写 `extensions` 给下游 extractor
//! - `session`：服务端 Redis session 真相源（access/refresh token 吊销）
//!
//! 2026-09-23 重构：Redis session key 从 `sha256(token)` 改为 JWT 自带 jti（UUID v4）；
//! `AuthenticatedTokenHash` → `SessionJti`（值类型 String 不变，语义从 sha256 hex 改 jti）。

pub mod extractor;
pub mod jwt;
pub mod middleware;
pub mod password;
pub mod rbac;
pub mod session;

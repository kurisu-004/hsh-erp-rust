//! 横切认证授权模块
//!
//! 对应 Python myERP/core/security.py + core/permission.py。
//! - `jwt`：access / refresh 双 token 编解码（HS256）
//! - `password`：bcrypt 散列/校验
//! - `rbac`：五角色定义、`CurrentUser`、`Claims`
//! - `extractor`：axum `FromRequestParts` 把 `CurrentUser` / `AuthTokenHash` 从
//!   request extensions 里读出来（由 `middleware` 注入）
//! - `middleware`：axum `auth_middleware` —— Bearer JWT 验签 + Redis session 校验 +
//!   滑动 TTL 集中处理；公开路径白名单；写 `extensions` 给下游 extractor
//! - `session`：服务端 Redis session 真相源（access/refresh token 吊销）

pub mod extractor;
pub mod jwt;
pub mod middleware;
pub mod password;
pub mod rbac;
pub mod session;

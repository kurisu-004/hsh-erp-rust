//! 横切认证授权模块
//!
//! 对应 Python myERP/core/security.py + core/permission.py。
//! - `jwt`：access / refresh 双 token 编解码（HS256）
//! - `password`：bcrypt 散列/校验
//! - `rbac`：五角色定义、`CurrentUser`、`Claims`
//! - `extractor`：axum `FromRequestParts` 把 Bearer JWT 解析成 `CurrentUser`
//! - `session`：服务端 Redis session 真相源（access/refresh token 吊销）

pub mod extractor;
pub mod jwt;
pub mod password;
pub mod rbac;
pub mod session;
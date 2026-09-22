//! iam 域入参 DTO（axum extractor 反序列化目标）
//!
//! 仅含 HTTP 请求侧的 input 类型；响应侧的 output 类型见 `super::vo`。
//! 按端点语义拆为 `login.rs`（session 端点）+ `account.rs`（account 端点）。

pub mod account;
pub mod login;

pub use account::{
    ChangePasswordRequest, UserAddRoleRequest, UserCreateRequest, UserListQuery,
    UserUpdateRequest,
};
pub use login::{LoginRequest, RefreshRequest};
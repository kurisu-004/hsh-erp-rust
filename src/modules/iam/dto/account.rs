//! iam 域 account 端点入参（账号 CRUD / 角色 / 改密）

use serde::Deserialize;

use crate::auth::rbac::Role;

/// POST /api/v2/iam/users 入参
#[derive(Debug, Clone, Deserialize)]
pub struct UserCreateRequest {
    pub username: String,
    pub password: String,
    pub full_name: String,
    #[serde(default)]
    pub phone: Option<String>,
}

/// 部分更新：字段为 `None` 表示不修改（与 Python `exclude_unset` 语义一致）
#[derive(Debug, Clone, Deserialize)]
pub struct UserUpdateRequest {
    #[serde(default)]
    pub full_name: Option<String>,
    #[serde(default)]
    pub phone: Option<String>,
    #[serde(default)]
    pub password: Option<String>,
    #[serde(default)]
    pub is_active: Option<bool>,
}

/// GET /api/v2/iam/users 列表查询参数
#[derive(Debug, Clone, Deserialize)]
pub struct UserListQuery {
    #[serde(default)]
    pub username_like: Option<String>,
    #[serde(default)]
    pub is_active: Option<bool>,
    #[serde(default)]
    pub limit: Option<i64>,
    #[serde(default)]
    pub offset: Option<i64>,
}

/// 添加角色入参。`role` 反序列化自大写字符串（MANAGER/CLERK/...，见 `auth::rbac::Role`）。
/// SHELF_ACCOUNT 必须带 `scope_type = "shelf"` + `scope_id`；其余角色两者必须为空。
#[derive(Debug, Clone, Deserialize)]
pub struct UserAddRoleRequest {
    pub role: Role,
    #[serde(default)]
    pub scope_type: Option<String>,
    #[serde(default)]
    pub scope_id: Option<i64>,
}

/// 自助/管理员改密入参（旧密码在自助改密时必填）
#[derive(Debug, Clone, Deserialize)]
pub struct ChangePasswordRequest {
    pub old_password: String,
    pub new_password: String,
}
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

/// `POST /api/v2/iam/users/{id}/wx-bind` 入参（2026-09-29 新增）
///
/// `wx_user_id` = 企业微信 userid（自建应用 `jscode2session` 返回的明文）。
/// `corp_id` 是**保留字段**：2026-09-29 起（review 第 1 轮 Y3）一律以后端配置的
/// `WECOM_CORPID` 为准，请求体传什么都不影响落库值——登录侧只认配置值，两侧
/// 必须对称，否则会写出永远登不进来的死行。配置也为空 → 40109。
#[derive(Debug, Clone, Deserialize)]
pub struct WxBindRequest {
    /// 企业微信 userid；service 层会 trim + 转小写（企微 userid 不区分大小写）
    pub wx_user_id: String,
    /// 保留字段，当前**被忽略**（与服务端 `WECOM_CORPID` 不符时打 warn）。
    /// 之所以留着不删：前端可能已经在传，直接删字段属于破坏性 API 变更。
    #[serde(default)]
    pub corp_id: Option<String>,
}

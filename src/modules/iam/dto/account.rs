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
///
/// 2026-10-10：`version` 改为**必填**（无 `#[serde(default)]`）—— OCC 锚点必须来自
/// 客户端。缺失时 axum `Json` 提取器直接返 HTTP 422 纯文本（不是业务信封），
/// 这是刻意的：让「忘了传 version」在联调期就炸，而不是被 service 读到的 DB 值
/// 悄悄顶替（那会让 30s 缓存快照下的并发改动静默成功）。
#[derive(Debug, Clone, Deserialize)]
pub struct UserUpdateRequest {
    /// `t_user.version` 的当前值（来自 `UserOut.version` / `GET /iam/users/{id}`）
    pub version: i32,
    #[serde(default)]
    pub full_name: Option<String>,
    #[serde(default)]
    pub phone: Option<String>,
    #[serde(default)]
    pub password: Option<String>,
    #[serde(default)]
    pub is_active: Option<bool>,
}

/// `POST /api/v2/iam/users/{id}/deactivate` 入参（2026-10-10 新增）
///
/// 停用 = 软删 `t_user`，是带 `version` 列的表上的写操作，故与 `update` 一样收
/// 必填 `version`。
#[derive(Debug, Clone, Deserialize)]
pub struct UserDeactivateRequest {
    /// `t_user.version` 的当前值
    pub version: i32,
}

/// `POST /api/v2/iam/users/{id}/roles/{role_id}/remove` 入参（2026-10-10 新增）
///
/// 撤的是 `t_user_role` 那一行自己的 `version`（来自 `UserRoleOut.version`）。
#[derive(Debug, Clone, Deserialize)]
pub struct UserRemoveRoleRequest {
    /// 被撤销的角色行的当前 `version`
    pub version: i32,
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

/// `POST /api/v2/iam/users/{id}/wx-bind` 入参
///
/// `wx_user_id` = 企业微信 userid（自建应用 `jscode2session` 返回的明文）。
///
/// 2026-10-10：删掉 `corp_id` 字段。它此前是「保留字段、一律忽略」（落库一律用后端
/// 配置的 `WECOM_CORPID`，因为登录侧只认配置值），留着只会让调用方误以为自己能
/// 指定企业。旧客户端若仍在传该字段，serde 默认忽略未知字段、不报错，落库行为不变。
#[derive(Debug, Clone, Deserialize)]
pub struct WxBindRequest {
    /// 企业微信 userid；service 层会 trim + 转小写（企微 userid 不区分大小写）
    pub wx_user_id: String,
}

/// `POST /api/v2/iam/users/{id}/wx-bind/unbind` 入参（2026-10-10 新增）
///
/// 解绑是 `t_wx_identity` 上的写操作（软删），带 `version` 列，故收必填 `version`。
#[derive(Debug, Clone, Deserialize)]
pub struct WxUnbindRequest {
    /// 绑定行的当前 `version`（来自 `GET /iam/users/{id}/wx-bind` 的 `data.version`）
    pub version: i32,
}

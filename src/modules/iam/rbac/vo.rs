//! RBAC 子模块响应 VO（HTTP 返回值隔离层）
//!
//! 仅含 handler 返回的 output 类型（仅 `Serialize`）；入参见 `super::dto`。
//!
//! ## id 一律 JSON string
//! 雪花 ID 是 19 位，超 JS `Number.MAX_SAFE_INTEGER`，故所有 id 字段走
//! `shared::types::serialize_i64` 序列化成字符串。
//!
//! ## `RoleDef` 与 `GET /iam/menus` 的字段纪律
//! `MenuNodeOut` 刻意**复用** `crate::modules::iam::vo::MenuNodeOut`（不在本文件
//! 复制一份）：`GET /iam/menus` 的出参必须与 `GET /iam/me` 的 `menus` 字段
//! **逐字段同形**，前端才能共用同一套渲染与成员判断（`meta.menuCode` 在这棵
//! 树上做匹配）。复制一份的代价是将来改一侧漏改另一侧、表现是「面板能显示
//! 但菜单高亮不亮」，且没有任何编译期信号。

use serde::Serialize;

use crate::shared::types::serialize_i64;

/// 角色定义（`GET /roles`、`POST /roles`、`POST /roles/{id}/update` 通用出参）。
#[derive(Debug, Clone, Serialize)]
pub struct RoleDef {
    #[serde(serialize_with = "serialize_i64")]
    pub id: i64,
    /// 角色码（大写）。自定义角色如 `QC_LEAD` 也在其中。
    pub code: String,
    pub name: String,
    pub description: Option<String>,
    /// 内置角色标记；`true` ⇒ 前端的 code 输入框与删除按钮应置灰。
    pub is_system: bool,
    pub is_active: bool,
    /// OCC 锚点：更新 / 停用 / grants 提交都要回传它。
    pub version: i32,
    /// 持有该角色的**在册**用户数（`t_user.is_active` 且授权行未软删）。
    /// 停用 / 删除角色的确认框靠它提示「会影响 N 个人」。
    pub user_count: i64,
}

/// 角色的完整授权包（`GET|POST /roles/{id}/grants` 通用出参）。
///
/// 整包语义：读到的就是全量，写回时**整包替换**（未出现在数组里的授权被软删）。
#[derive(Debug, Clone, Serialize)]
pub struct RoleGrants {
    #[serde(serialize_with = "serialize_i64")]
    pub role_id: i64,
    pub role_code: String,
    /// 内置角色标记（前端据此把 grants 表单置只读）。
    pub is_system: bool,
    /// `t_role.version` —— 整包替换的 OCC 锚点（与角色本身的 version 同一个，
    /// 因为「改授权」与「改角色」都是对同一行的乐观锁写）。
    pub version: i32,
    /// 已授权 / 待授权的菜单码（`t_role_menu`）。
    pub menu_codes: Vec<String>,
    /// 已授权 / 待授权的权限码（`t_role_permission`）。
    pub permission_codes: Vec<String>,
}

/// 权限点分组里的一项（`GET /permissions`）。
#[derive(Debug, Clone, Serialize)]
pub struct PermissionItem {
    pub code: String,
    pub name: String,
    pub description: Option<String>,
    pub is_active: bool,
}

/// 权限点分组（`GET /permissions`），按 `domain` 聚合。
#[derive(Debug, Clone, Serialize)]
pub struct PermissionGroup {
    pub domain: String,
    pub items: Vec<PermissionItem>,
}

/// `POST /roles/{id}/deactivate` 出参。
#[derive(Debug, Clone, Serialize)]
pub struct RoleDeactivateOut {
    /// **本次被踢下线的用户数**（= 被连带软删的 `t_user_role` 行数）。
    ///
    /// 只在 `delete=true` 时有意义（停用不连带删授权行，见端点契约）；
    /// 前端据此提示「N 个账号将失去该角色并被强制下线」。
    pub affected_user_count: i64,
}

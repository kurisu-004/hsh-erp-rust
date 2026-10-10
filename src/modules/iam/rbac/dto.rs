//! RBAC 子模块 DTO（HTTP 请求入参）
//!
//! 三类入参：建角色 / 改角色 / 停用角色，外加 grants 整包替换。
//!
//! ## `version` 一律必填（无 `#[serde(default)]`）
//! 按仓库 OCC 铁律，缺失走 axum `Json` 提取器 → **HTTP 422 纯文本**（不是业务
//! 信封）。客户端必须显式回传它看到的那一版。
//!
//! ## `code` 的形态
//! 统一大写（service 层 `to_uppercase` 归一后校验），长度 ≤ 20（与
//! `t_role.code` / `t_user_role.role` 的列宽一致 —— 同一个串要能在几张表里通用）。
//! 字符集限定 `[A-Z0-9_]`，避免空格 / 中文 / 引号进到 SQL 与前端菜单显示。

use serde::Deserialize;

use crate::shared::error::AppError;

/// `POST /api/v2/iam/roles`：新建自定义角色。
///
/// ⚠️ **不接受 `is_system` / `is_active` 入参**：内置角色由 `seeds/rbac.sql`
/// 声明，不许经 HTTP 创建；停用有专门的端点。
#[derive(Debug, Clone, Deserialize)]
pub struct RoleCreateRequest {
    pub code: String,
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
}

/// `POST /api/v2/iam/roles/{id}/update`：部分更新。
///
/// - `code` **省略 = 不改**（`Option<String>` 的 `None` 语义）；
/// - `name` 必填（角色名是面板主标识，没有「不传就沿用」的必要，省掉一个
///   「忘了传导致整次编辑白做」的分支）；
/// - `is_system=true` 的角色带 `code` → 400（系统角色 code 不可改）。
#[derive(Debug, Clone, Deserialize)]
pub struct RoleUpdateRequest {
    #[serde(default)]
    pub code: Option<String>,
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    pub version: i32,
}

/// `POST /api/v2/iam/roles/{id}/deactivate`：停用 / 软删。
///
/// `delete` 的三态语义：
/// - `false`：只停用（内置角色只允许这一支 —— 可停用、不可删）
/// - `true`：软删 `t_role` + 同事务连带软删该角色的 `t_user_role` /
///   `t_role_menu` / `t_role_permission`（仅 `is_system=false` 可用）
#[derive(Debug, Clone, Deserialize)]
pub struct RoleDeactivateRequest {
    pub version: i32,
    #[serde(default)]
    pub delete: bool,
}

/// `POST /api/v2/iam/roles/{id}/grants`：整包替换授权。
///
/// 两个数组都是**完整集合**而非增量：提交什么就等于什么，未出现在数组里的
/// 授权会被软删。这样前端不需要维护「先读差集、再删后插」的顺序。
///
/// `role_code == "ADMIN"` → 400（拒写元角色 grants）。
#[derive(Debug, Clone, Deserialize)]
pub struct RoleGrantsRequest {
    #[serde(default)]
    pub menu_codes: Vec<String>,
    #[serde(default)]
    pub permission_codes: Vec<String>,
    pub version: i32,
}

// ===========================================================================
// 入参校验（service 与单测共用，避免两处各写一遍规则）
// ===========================================================================

/// `t_role.code` / `t_user_role.role` 的共同列宽。
pub(crate) const CODE_MAX_LEN: usize = 20;
/// `t_role.name` 列宽。
pub(crate) const NAME_MAX_LEN: usize = 50;

/// 角色码形态：`[A-Z0-9_]`、1..=20 字符。**要求已是大写** —— 归一在
/// service 层做（`trim` + `to_uppercase`），本函数只校验结果。
pub(crate) fn validate_code_shape(code: &str) -> Result<(), AppError> {
    if code.is_empty() || code.len() > CODE_MAX_LEN {
        return Err(AppError::validation(format!(
            "code 长度须为 1..={CODE_MAX_LEN}，收到 '{}'",
            code.chars().count()
        )));
    }
    if !code
        .chars()
        .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
    {
        return Err(AppError::validation(
            "code 只允许大写字母 / 数字 / 下划线".to_string(),
        ));
    }
    Ok(())
}

/// 角色名：trim 后非空、≤ 50 字符。
pub(crate) fn validate_name(name: &str) -> Result<(), AppError> {
    let n = name.trim();
    if n.is_empty() {
        return Err(AppError::validation("name 不能为空".to_string()));
    }
    if n.chars().count() > NAME_MAX_LEN {
        return Err(AppError::validation(format!(
            "name 长度须 ≤ {NAME_MAX_LEN}"
        )));
    }
    Ok(())
}

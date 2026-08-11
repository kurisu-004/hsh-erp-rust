//! RBAC 五角色 + 当前用户结构 + JWT Claims
//!
//! 对应 Python myERP/model/enums.py `UserRole` + `CurrentUser` dataclass。
//!
//! ## 角色
//! - `Manager`：超级权限（业务层自行判断是否豁免）
//! - `Clerk`：文员
//! - `Inspector`：品检员
//! - `CncProgrammer`：CNC 程序员
//! - `ShelfAccount`：货架一体机专用账号，必须 scope 到具体 `shelf_id`
//!
//! ## SHELF_ACCOUNT 货架范围
//! - `shelf_ids`：可访问的具体货架列表
//! - `shelf_wildcard`：是否对所有货架放行（仅 Manager 标志）

use serde::{Deserialize, Serialize};

use crate::shared::error::{code, AppError};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum Role {
    #[serde(rename = "MANAGER")]
    Manager,
    #[serde(rename = "CLERK")]
    Clerk,
    #[serde(rename = "INSPECTOR")]
    Inspector,
    #[serde(rename = "CNC_PROGRAMMER")]
    CncProgrammer,
    #[serde(rename = "SHELF_ACCOUNT")]
    ShelfAccount,
}

/// access token 业务载荷（与 Python JWT payload 对齐）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claims {
    pub sub: i64,
    pub username: String,
    pub roles: Vec<Role>,
    pub shelf_ids: Vec<i64>,
    #[serde(default)]
    pub shelf_wildcard: bool,
    #[serde(default)]
    pub ver: i32,
    #[serde(default = "default_access_type")]
    pub typ: String,
    pub iss: String,
    pub exp: i64,
}

fn default_access_type() -> String {
    "access".into()
}

/// Handler 中可用的当前登录用户
#[derive(Debug, Clone)]
pub struct CurrentUser {
    pub id: i64,
    pub username: String,
    pub roles: Vec<Role>,
    pub shelf_ids: Vec<i64>,
    pub shelf_wildcard: bool,
}

impl CurrentUser {
    pub fn has_role(&self, role: Role) -> bool {
        self.roles.contains(&role)
    }

    pub fn require_role(&self, role: Role) -> Result<(), AppError> {
        if self.has_role(role) {
            Ok(())
        } else {
            Err(AppError::biz(code::FORBIDDEN, "无权限"))
        }
    }

    pub fn require_any_role(&self, roles: &[Role]) -> Result<(), AppError> {
        if roles.iter().any(|r| self.has_role(*r)) {
            Ok(())
        } else {
            Err(AppError::biz(code::FORBIDDEN, "无权限"))
        }
    }

    /// 货架一体机/有 shelf_ids 限制的用户：判断是否可访问特定 shelf
    pub fn can_access_shelf(&self, shelf_id: i64) -> bool {
        self.shelf_wildcard || self.shelf_ids.contains(&shelf_id) || self.has_role(Role::Manager)
    }
}
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
//!
//! ## v1/v2 JWT 兼容（2026 docker 编排 Phase 3）
//! - Python v1 token 把 `sub` 写成字符串（`str(user_id)`），Rust v2 历史期望 `i64`。
//!   `Claims.sub` 用 `deserialize_sub_or_int` 同时接受数字和数字串。
//! - Python v1 用 `type` 字段，Rust v2 历史用 `typ`。`Claims.typ` 加 `alias = "type"`
//!   让解码阶段吃下两种命名；编码仍发 `typ`，无破坏。

use serde::{Deserialize, Deserializer, Serialize, de::Visitor};

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
    /// Python v1 token 把 `sub` 写成 `str(user_id)`，Rust v2 历史上期望 `i64`；
    /// `deserialize_sub_or_int` 同时接受两种形式（数字 / 数字串）。
    #[serde(deserialize_with = "deserialize_sub_or_int")]
    pub sub: i64,
    pub username: String,
    pub roles: Vec<Role>,
    pub shelf_ids: Vec<i64>,
    #[serde(default)]
    pub shelf_wildcard: bool,
    #[serde(default)]
    pub ver: i32,
    /// Python v1 用 `type` 字段；加 `alias = "type"` 解码时兼容两种命名。
    /// 编码仍发 `typ`，Rust 自签 token 形态不变。
    #[serde(default = "default_access_type", alias = "type")]
    pub typ: String,
    pub iss: String,
    pub exp: i64,
}

fn default_access_type() -> String {
    "access".into()
}

/// `Claims.sub` 反序列化 helper：
/// - Rust 自签：`"sub": 123`（i64）
/// - Python v1：`"sub": "123"`（数字串）
/// 两种都解析为 `i64`。
fn deserialize_sub_or_int<'de, D>(deserializer: D) -> Result<i64, D::Error>
where
    D: Deserializer<'de>,
{
    struct SubVisitor;

    impl<'de> Visitor<'de> for SubVisitor {
        type Value = i64;

        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("i64 or numeric string for JWT sub")
        }

        fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<i64, E> {
            Ok(v)
        }

        fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<i64, E> {
            i64::try_from(v).map_err(|_| E::custom("u64 too large for i64 sub"))
        }

        fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<i64, E> {
            v.parse::<i64>()
                .map_err(|_| E::custom(format!("sub is not a numeric string: {v:?}")))
        }

        fn visit_string<E: serde::de::Error>(self, v: String) -> Result<i64, E> {
            self.visit_str(&v)
        }
    }

    deserializer.deserialize_any(SubVisitor)
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
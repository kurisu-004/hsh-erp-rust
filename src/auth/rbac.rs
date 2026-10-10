//! RBAC 六角色 + 当前用户结构 + JWT AccessTokenClaims
//!
//! ## 角色
//! - `Admin`：**元角色**，代码级通配（2026-10-14 新增，见下方「ADMIN 元角色」）
//! - `Manager`：超级权限（业务层自行判断是否豁免）
//! - `Clerk`：文员
//! - `Inspector`：品检员
//! - `CncProgrammer`：CNC 程序员
//! - `ShelfAccount`：货架一体机专用账号，必须 scope 到具体 `shelf_id`
//!
//! ## SHELF_ACCOUNT 货架范围
//! - `shelf_ids`：可访问的具体货架列表
//! - `shelf_wildcard`：是否对所有货架放行（仅 Manager / Admin 标志）
//!
//! ## Role ↔ 字符串的唯一真源
//! 2026-10-10 收敛：`Role` 的字符串表示此前散在 3 处（`auth::rbac::parse_role_string`
//! 反向映射、`iam::service::account::role_as_str` 正向映射、`iam::service::session`
//! 的私有 `parse_role` 复制品），改规则时极易漏改一处。现在两个方向都收在本文件：
//! `Role::as_str`（正向）与 `parse_role_string`（反向）。
//!
//! ## ADMIN 元角色（2026-10-14 新增）
//! ADMIN 是**代码级的通配角色**，不是一行数据授权：
//! - `has_role(任意角色)` 在用户持有 ADMIN 时恒返 `true`；四个鉴权原语
//!   （`require_role` / `require_any_role` / `require_permission` /
//!   `can_access_shelf`）因此自动放行；
//! - ADMIN 用户装载的 `permissions` **恒为空 vec** —— 通配在
//!   `require_permission` 内做短路，不靠数据。若改成往 `t_role_permission`
//!   里灌满行，通配语义就退化成「逐项勾选」，漏改一行即静默失效；
//! - 后端 `POST /iam/roles/{id}/grants` **拒写** ADMIN 的 grants（400），
//!   `GET` 则返回全量 active 菜单 / 权限点（面板要能整页渲染，不能返空）。
//!
//! ⚠️ 前端 `hasRole` 必须同步短路：后端加了通配而前端没有时，纯 ADMIN 账号
//!   会「后端 253 处全放行、前端 `isManager` / `canSplit` / 路由 `allowRoles`
//!   / 菜单树全关」，登录即全站无入口。
//!
//! ## 自定义角色不进本枚举（2026-10-14）
//! `t_user_role.role` 是无 FK 的 varchar，能存 `QC_LEAD` 这类自定义 code。
//! `parse_role_string` 对未知值**打 warn 并返回 `None`**（不 panic、不静默当作
//! 已知角色）——自定义角色不参与 `Role` 枚举的授权判定，但仍要出现在
//! `GET /iam/me` 的 `roles` 字段里（那里返的是**原始大写字符串数组**，见
//! `iam::service::session::build_current_user_out`），否则它们在角色面板 /
//! 用户详情里会全部消失且**零报错**。
//!
//! ## JWT 字段（2026-09-22 重构）
//! - access token 业务字段（username/roles/shelf_ids/shelf_wildcard/ver）已从 JWT 中删除，
//!   全部改走 Redis session 校验 + 服务端缓存；handler/extractor 仍通过 `CurrentUser`
//!   拿到这些上下文。
//! - 标准字段（`sub/aud/iat/nbf/exp/iss/jti/typ`）按 RFC 7519 命名，Rust 结构体字段全词化
//!   （`subject` / `audience` / `issued_at` / `not_before` / `expires_at` / `issuer` / `jwt_id` /
//!   `token_type`），通过 `#[serde(rename = "...")]` 桥接 JSON 短码。

use serde::{Deserialize, Serialize};

use crate::shared::error::{AppError, code};

/// 把 DB / Redis 缓存里的大写 role 字符串转回 `Role` 枚举（`Role::as_str` 的反向）。
///
/// 仅识别 6 种已知值；未知值打 `tracing::warn!` 并返回 `None`，调用方自行决定是否跳过。
///
/// ⚠️ 自定义角色（`t_role` 里 `is_system=false` 的行，如 `QC_LEAD`）走 `_` 分支：
///   返回 `None` + 一行 warn，**不 panic、不静默当成已知角色**。它们不参与
///   `Role` 枚举的授权判定，但仍原样出现在 `GET /iam/me` 的 `roles` 里。
pub fn parse_role_string(s: &str) -> Option<Role> {
    Some(match s {
        "ADMIN" => Role::Admin,
        "MANAGER" => Role::Manager,
        "CLERK" => Role::Clerk,
        "INSPECTOR" => Role::Inspector,
        "CNC_PROGRAMMER" => Role::CncProgrammer,
        "SHELF_ACCOUNT" => Role::ShelfAccount,
        _ => {
            tracing::warn!(role = %s, "未知 role 字符串，跳过");
            return None;
        }
    })
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum Role {
    /// 2026-10-14 新增：元角色，`has_role` 对任意角色恒真。
    #[serde(rename = "ADMIN")]
    Admin,
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

impl Role {
    /// `Role` → DB / JSON 中的大写字符串（与本枚举的 `serde(rename)` 逐字一致）。
    ///
    /// 改任一处的 `serde(rename)` 必须同步改这里，否则「序列化给前端」与「写进
    /// `t_user_role.role`」会分叉，而后者只有 restart 后重建的数据才对得上。
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Admin => "ADMIN",
            Role::Manager => "MANAGER",
            Role::Clerk => "CLERK",
            Role::Inspector => "INSPECTOR",
            Role::CncProgrammer => "CNC_PROGRAMMER",
            Role::ShelfAccount => "SHELF_ACCOUNT",
        }
    }
}

/// access token 标准字段 + RFC 7519 claim set（业务字段已全部移到 Redis session）
///
/// 2026-09-22 重构：
/// - Rust 字段全词化（subject / audience / issued_at / not_before / expires_at / issuer /
///   jwt_id / token_type），JSON 字段名按 RFC 7519 用短码（sub / aud / iat / nbf / exp / iss /
///   jti / typ），通过 `#[serde(rename = "...")]` 桥接。
/// - 删除 `username` / `roles` / `shelf_ids` / `shelf_wildcard` / `ver` / `deserialize_sub_or_int` /
///   `alias = "type"`：Rust 自签 token 不再携带业务字段，业务上下文从 Redis session 取；
///   `sub` 直接用 `i64`，无须再兼容 Python v1 数字串。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccessTokenClaims {
    /// RFC 7519 `sub`：用户 snowflake id（i64）
    #[serde(rename = "sub")]
    pub subject: i64,
    /// RFC 7519 `aud`：受众；本服务统一用 `JwtConfig::audience`（默认 `hsh-erp-rust`）
    #[serde(rename = "aud")]
    pub audience: String,
    /// RFC 7519 `iat`：issued-at，由签发函数填入 unix 时间
    #[serde(rename = "iat")]
    pub issued_at: i64,
    /// RFC 7519 `nbf`：not-before；默认等于 `issued_at`（签发即生效）
    #[serde(rename = "nbf", default = "default_not_before")]
    pub not_before: i64,
    /// RFC 7519 `exp`：expiration
    #[serde(rename = "exp")]
    pub expires_at: i64,
    /// RFC 7519 `iss`：issuer，对齐 `JwtConfig::issuer`
    #[serde(rename = "iss")]
    pub issuer: String,
    /// RFC 7519 `jti`：JWT ID（UUID v4），防重放审计字段
    #[serde(rename = "jti", default = "default_jwt_id")]
    pub jwt_id: String,
    /// RFC 7519 `typ`：token type；access token 默认 `default_access_token_type`（`"access"`）
    #[serde(rename = "typ", default = "default_access_token_type")]
    pub token_type: String,
}

fn default_access_token_type() -> String {
    "access".into()
}

fn default_not_before() -> i64 {
    // 0 仅作占位；签发时由 encode_access / encode_refresh 覆写
    0
}

fn default_jwt_id() -> String {
    // 0 仅作占位；签发时由 encode_access / encode_refresh 填入 UUID v4
    String::new()
}

/// Handler 中可用的当前登录用户
#[derive(Debug, Clone)]
pub struct CurrentUser {
    pub id: i64,
    pub username: String,
    pub roles: Vec<Role>,
    pub shelf_ids: Vec<i64>,
    pub shelf_wildcard: bool,
    /// 该用户经**全部**角色 code 联查 `t_role_permission` 得到的权限码集合
    ///（2026-10-14 新增）。
    ///
    /// 数据来源是 **Redis session 缓存**（`CachedUserProfile.permissions`），
    /// 每请求**不**查库。这么定的理由是「grants 变更后踢持有者下线」这条语义
    /// 依赖缓存能被**一次性作废**：权限装在 session 里，改 grants 时 handler 在
    /// 事务提交后 `delete_all_user_sessions`，所有持有者的下一跳即失效。若改成每
    /// 请求查库，改 grants 的提交与生效之间有一段「改了但仍按旧授权放行」的
    /// 窗口，且这段窗口没有任何机制能收敛。
    ///
    /// ⚠️ **ADMIN 用户的这个字段恒为空 vec**：ADMIN 是代码级通配
    ///   （`require_permission` 内短路），不靠数据。写成满行反而会让通配语义
    ///   悄悄降级成逐项勾选。
    pub permissions: Vec<String>,
}

impl CurrentUser {
    /// 是否持有某角色。**ADMIN 元角色通配**（2026-10-14 新增）：持有 ADMIN 时
    /// 对任意角色恒返 `true`。
    ///
    /// 这是全仓唯一一处角色判定入口 —— `require_role` / `require_any_role` /
    /// `can_access_shelf` 都走它，所以通配只需在这一处实现一次。
    pub fn has_role(&self, role: Role) -> bool {
        self.roles.contains(&Role::Admin) || self.roles.contains(&role)
    }

    pub fn require_role(&self, role: Role) -> Result<(), AppError> {
        if self.has_role(role) {
            Ok(())
        } else {
            Err(AppError::biz(code::FORBIDDEN, "无权限"))
        }
    }

    /// 角色集合闸门。**ADMIN 元角色短路**（2026-10-14 新增）：持有 ADMIN 时直接
    /// 放行，与 `require_role` 同语义。
    ///
    /// 短路放在循环之前而不是只靠 `has_role`：ADMIN 的通配要覆盖「空集合」这种
    /// 退化调用（`require_any_role(&[])` 走 `iter().any` 恒假，一个角色都不命中）。
    /// 该形态在产品代码里不该出现，但本原语是**授权闸门**，fail-open 的代价是
    /// 无授权放行、fail-closed 的代价只是让退化调用报错 —— 选后者。
    pub fn require_any_role(&self, roles: &[Role]) -> Result<(), AppError> {
        if self.has_role(Role::Admin) || roles.iter().any(|r| self.has_role(*r)) {
            Ok(())
        } else {
            Err(AppError::biz(code::FORBIDDEN, "无权限"))
        }
    }

    /// 权限点闸门（2026-10-14 新增）：`permissions` 里含 `code` 即放行。
    ///
    /// **ADMIN 元角色短路**：`has_role(Role::Admin)` 时直接放行，不查
    /// `permissions`（ADMIN 用户该字段恒空 vec，不靠数据授权）。
    pub fn require_permission(&self, code: &str) -> Result<(), AppError> {
        if self.has_role(Role::Admin) || self.permissions.iter().any(|p| p == code) {
            Ok(())
        } else {
            Err(AppError::biz(code::FORBIDDEN, "无权限"))
        }
    }

    /// 货架一体机/有 shelf_ids 限制的用户：判断是否可访问特定 shelf。
    /// ADMIN 元角色通配（与 `require_role` 同源，见 `has_role`）。
    pub fn can_access_shelf(&self, shelf_id: i64) -> bool {
        self.shelf_wildcard || self.shelf_ids.contains(&shelf_id) || self.has_role(Role::Manager)
    }
}

// ===========================================================================
// 纯函数单测（无 IO，按仓库惯例 inline 在本文件）
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::{CurrentUser, Role, parse_role_string};

    fn user_with(roles: Vec<Role>, permissions: &[&str]) -> CurrentUser {
        CurrentUser {
            id: 1,
            username: "u".into(),
            roles,
            shelf_ids: vec![],
            shelf_wildcard: false,
            permissions: permissions.iter().map(|s| s.to_string()).collect(),
        }
    }

    /// 2026-10-14 新增：ADMIN 是元角色，**四个鉴权原语**都要放行。
    ///
    /// 这条用例守的是「通配只实现了一次」这个前提：`has_role` 里那一句
    /// `roles.contains(&Role::Admin)` 若被删，`require_permission` 单独实现的
    /// ADMIN 分支仍会让本用例绿 —— 故本用例刻意**只**构造 ADMIN 用户跑四个原语，
    /// 通配的**唯一**实现点（`has_role`）由 `has_role_admin_wildcards_every_role`
    /// 钉住。
    #[test]
    fn admin_passes_all_primitives() {
        // ⚠️ permissions 恒空：ADMIN 的通配不靠数据
        let admin = user_with(vec![Role::Admin], &[]);

        // 原语 ①：require_role —— 对任意目标角色都放行
        for role in [
            Role::Manager,
            Role::Clerk,
            Role::Inspector,
            Role::CncProgrammer,
            Role::ShelfAccount,
        ] {
            admin.require_role(role).expect("ADMIN 应放行 require_role");
        }
        // 原语 ②：require_any_role —— 空集合也放行（ADMIN 无需命中任何一项）
        admin
            .require_any_role(&[])
            .expect("ADMIN 应放行 require_any_role");
        admin
            .require_any_role(&[Role::Clerk, Role::Inspector])
            .expect("ADMIN 应放行 require_any_role");
        // 原语 ③：require_permission —— permissions 为空仍放行
        admin
            .require_permission("part.drawing.print")
            .expect("ADMIN 应放行 require_permission");
        admin
            .require_permission("never.granted.anywhere")
            .expect("ADMIN 应放行 require_permission");
        // 原语 ④：can_access_shelf —— 通配到未在 shelf_ids 里的架
        assert!(admin.can_access_shelf(987654321));

        // parse_role_string 两条
        assert_eq!(parse_role_string("ADMIN"), Some(Role::Admin));
        // 自定义 code：不 panic、不误认，打 warn 后跳过
        assert_eq!(parse_role_string("QC_LEAD"), None);
    }

    /// ADMIN 通配的**唯一**实现点是 `has_role`（见上一条的说明）。
    #[test]
    fn has_role_admin_wildcards_every_role() {
        let admin = user_with(vec![Role::Admin], &[]);
        assert!(admin.has_role(Role::Manager));
        assert!(admin.has_role(Role::Clerk));
        assert!(admin.has_role(Role::ShelfAccount));
    }

    /// 非 ADMIN 用户的四个原语**不该**被通配影响（否则通配写错成恒真也绿）。
    #[test]
    fn non_admin_keeps_exact_role_matching() {
        let clerk = user_with(vec![Role::Clerk], &["queue.dispatch"]);
        assert!(clerk.has_role(Role::Clerk));
        assert!(!clerk.has_role(Role::Manager));
        assert!(clerk.require_permission("queue.dispatch").is_ok());
        assert!(clerk.require_permission("part.drawing.print").is_err());
        assert!(clerk.require_role(Role::Manager).is_err());
        assert!(!clerk.can_access_shelf(1));
    }
}

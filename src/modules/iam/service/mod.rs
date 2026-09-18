//! iam service 层入口
//!
//! 按职责拆为：
//! - `account` — AccountService（原 UserService）：账号 CRUD + 角色管理 + 改密
//! - `session` — SessionService（原 AuthService）：login / refresh / me / logout / change-password
//! - `menu`    — `build_menu_tree` 纯函数（菜单树组装）
//!
//! 对外 API（`handler.rs` 调用面）保持原方法名（方法名从 `UserService`/`AuthService`
//! 完整继承，只类型重命名），handler 通过 `iam::service::{AccountService, SessionService, role_as_str}` 引用。
//!
//! 2026-09-19 IAM 域合并：合并 `auth/service.rs` + `user/service.rs`，方法体零 diff，
//! 仅 `AuthService` → `SessionService`、`UserService` → `AccountService`。

mod account;
mod menu;
mod session;

pub use account::{AccountService, DEFAULT_RESET_PASSWORD, role_as_str};
pub use menu::build_menu_tree;
pub use session::SessionService;

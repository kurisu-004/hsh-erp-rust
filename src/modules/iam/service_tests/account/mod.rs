//! iam AccountService 单测子模块入口（plan v4 §3 V7 + §5.1）
//!
//! 按职责拆为：
//! - `helpers`     — 共享 fixtures / mocks（pub(crate) re-export 给兄弟模块）
//! - `crud_tests`  — list_users / get_user / create_user / update_user / deactivate_user（27 例）
//! - `password_tests` — change_own_password / admin_reset_password（10 例）
//! - `role_tests`  — list_user_roles / add_role / remove_role（16 例）
//! - `helper_tests` — menus_for_roles 助手 + 边界用例（8 例）
//!
//! 拆文件原因：`account_tests.rs` 原本 1589 行超出 IDE 跳闸舒适区（conventions §2 不计入，
//! 但 1000 行硬约束同样适用 service_tests）。
//!
//! 2026-09-22 删 `current_user_out`（死代码）→ helper_tests 由 10 例减为 8 例。

mod helpers;

#[cfg(test)]
mod crud_tests;

#[cfg(test)]
mod password_tests;

#[cfg(test)]
mod role_tests;

#[cfg(test)]
mod helper_tests;

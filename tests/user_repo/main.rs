//! user 域 repo 集成测试（PR13 Phase D 拆分）
//!
//! 1 个原 test binary（user_repo，1164 行）按主题拆为 3 sub-file，1 个 binary，
//! 入口 `main.rs`（cargo 1.98 auto-discover 约定；`mod.rs` 不被识别）。
//!
//! ## 拆分映射（原 1164 行 user_repo.rs → 3 文件）
//! - basic.rs   ← UserRepo 26 例（user CRUD + query + update + touch_login +
//!   refresh_token 轮转 + 密码轮转）
//! - role.rs    ← UserRoleRepo 16 + MenuRepo 4 + ShelfRepo 2 = 22 例
//!   （role/permission 相关）
//! - password.rs ← 多表组合事务 2 + 事务边界 2 + 3 个补充集成测试 = 7 例
//!   （密码 / 状态 / 软删过滤）
//!
//! 合计 26 + 22 + 7 = 55 例（拆分当时是 47 例，后续增补用例使各文件计数上移；
//! 本注释的 3 个数字与「拆分映射」里逐文件的数字是同一组，核对时一起改）。

#![allow(dead_code, clippy::await_holding_lock, unused_imports)]

mod basic;
mod password;
mod role;

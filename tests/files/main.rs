//! files 域集成测试（2026-10-13 新建 binary）
//!
//! cargo 1.98.1 **不识别** `tests/<dir>/mod.rs`，只识别 `tests/<dir>/main.rs`
//! （binary 名 = `<dir>`）。本 binary 由 3 个原 binary 合并而成，与「一域一份测试
//! binary」的归属约定一致：
//!
//! ## 拆分映射
//! - part_file.rs   ← 2026-10-13 自 `tests/part/file.rs` 迁入（原 `part_file_api.rs`）。
//!   `t_part_file` 的 service 直调用例 + 硬切 404 守卫。URL 自 `/api/v2/part-files/*`
//!   硬切到 `/api/v2/files/objects/*`（无 alias，旧路径 404）。
//! - cnc.rs         ← 2026-10-13 自 `tests/cnc_program_api.rs` 迁入。URL 自
//!   `POST /api/v2/cnc-programs/pairs` 硬切到 `POST /api/v2/files/cnc/pairs`。
//! - sts.rs         ← 2026-10-13 自 `tests/files_sts_tmp_keys.rs` 迁入。
//!   `POST /api/v2/files/sts-tmp-keys` 的 BFF 转发用例（URL 未变）。
//!
//! ⚠️ `sts.rs` 自建了一个最小 Router（`.nest("/files", files::router())` + iam nest），
//! 2026-10-13 起它会顺带挂上本域新的对象 / CNC 路由，但该文件一条都不打 —— 保留原样。
//!
//! 2026-10-13 随端点下线删除的用例：part_file 侧
//! `rbac_shelf_account_rejects_part_nested_lists`（三条零件维度嵌套列表端点已删）、
//! cnc 侧 `list_pairs_for_part` + 3 条 alias 用例（端点已删）。

#![allow(dead_code, clippy::await_holding_lock, unused_imports)]

mod cnc;
mod part_file;
mod sts;

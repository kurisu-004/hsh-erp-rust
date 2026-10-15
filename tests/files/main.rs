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
//! - upload.rs      ← **2026-10-15 重写**：`POST /files/objects/upload-intents`
//!   的签发 / 秒传 / 校验矩阵 / 角色矩阵用例（HTTP 层）。
//! - cnc.rs         ← **2026-10-15 重写**：CNC 配对（`POST /files/objects/confirm`
//!   的 `pair_with_file_id`）用例。原 multipart 端点 `POST /files/cnc/pairs` 已下线。
//! - preview.rs     ← **2026-10-13 PR2 新建**：COS CI 图纸预览
//!   `GET /api/v2/files/objects/{id}/preview`（惰性轮询、零后台任务）。⚠️ **34 个
//!   测试函数**（2026-10-13 新增轮询节流可配 1 条后逐函数点数）= 25 个编号函数
//!   （24 个编号条目，含 8b；其中 #18 覆盖 2 个）+ 9 条不编号护栏 —— 含 matchit
//!   冲突实测、`preview_urls` 纯函数、跨仓 CI 终态词表表驱动、
//!   **跨仓请求体契约**、认领谓词的确定性并发断言、**`size=view/thumb/full` 档位与
//!   非法值 422 零副作用**、**`PREVIEW_POLL_BACKOFF_SECONDS` 配 1 时 `retry_after_ms`
//!   精确 1000ms**。
//!
//! ## 2026-10-15 随端点下线整文件删除的 sub-file
//!
//! - `sts.rs`（`POST /api/v2/files/sts-tmp-keys` 的 python BFF 转发用例）：
//!   端点随统一上传链路上线而下线。⚠️ 硬切守卫在 `part_file.rs` 里新增了一条
//!   （`POST /api/v2/files/sts-tmp-keys` → 404），别把它当成「无人管过」。
//!
//! 2026-10-13 随端点下线删除的用例：part_file 侧
//! `rbac_shelf_account_rejects_part_nested_lists`（三条零件维度嵌套列表端点已删）、
//! cnc 侧 `list_pairs_for_part` + 3 条 alias 用例（端点已删）。
//!
//! ⚠️ 2026-10-13 PR1b：原 `tests/assembly/files_list.rs`（D-09
//! `GET /assemblies/{id}/files` 的 4 条用例）**整文件删除** —— 端点已下线，
//! 等价读端点的覆盖在 `part_file.rs`。
//!
//! ⚠️ 2026-10-15：`upload.rs` 的装配件 multipart 用例（`upload_file_for_owner`
//! service 直调 + 3 条手搓 multipart 请求）随 multipart 上传端点下线整段重写为
//! `POST /files/objects/upload-intents` 的 HTTP 用例；装配件文件本身仍能经
//! upload-intents + confirm 写入（见 `part_file.rs` 的
//! `list_assembly_owner_filters_by_kind_and_owner` 与
//! `soft_delete_assembly_master_happy_path`，二者走新链路）。

#![allow(dead_code, clippy::await_holding_lock, unused_imports)]

mod cnc;
mod part_file;
mod preview;
mod upload;

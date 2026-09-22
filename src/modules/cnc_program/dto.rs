//! cnc_program 域 DTO（2026-09-14 Phase 3 + 2026-09-22 PR4 拆分）
//!
//! 对应 Python myERP/schema/cnc_program.py。
//!
//! CNC 程序是「配对上传」：一次提交 G_CODE + SETUP_SHEET 两个文件，
//! 形成一对（`paired_file_id` 互相指向对方）。
//!
//! ## DTO/VO 边界（2026-09-22 PR4 重构）
//! 本文件仅含入参（Deserialize）；出参结构（`CncPairOut` / `CncFileRef` /
//! `CncPairListItem` / `CncPairListOut`）已迁移至 `super::vo`。

use serde::Deserialize;

/// 配对上传请求（multipart `data` JSON 字段）。
///
/// 上传时同时携带 G_CODE + SETUP_SHEET 两个文件，service 端在事务内写两条
/// `t_part_file` 行，`paired_file_id` 互指。允许多次上传形成多版本对，但
/// 简单版（Phase 3）只写一对。
#[derive(Debug, Clone, Deserialize)]
pub struct CncPairUploadRequest {
    pub part_id: String, // 雪花 id
    pub note: Option<String>,
}

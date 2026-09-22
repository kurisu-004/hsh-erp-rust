//! `scan_add` 第一步解析：ScanKind enum + `resolve_scan_kind` 纯函数（2026-09-22 D-5 拆出）
//!
//! Idempotency 假设（与 Python `service/delivery_note.py::pickup_scan` 一致，
//! 在 comment block 中固化说明）：
//! > part serial 与 assembly serial 由同一 `t_serial_counter`（per-prefix）
//! > 池子发放；part 表 `uk_t_part_serial_no` partial unique + assembly 表
//! > `uk_t_assembly_serial_no` partial unique 都在 `serial_no IS NOT NULL AND
//! > deleted_at IS NULL` 域内全局唯一，因此 **同一 serial 不可能既挂在 part
//! > 也挂在 assembly** —— 解析分支不会有歧义。

use crate::modules::assembly::model::TAssembly;
use crate::modules::delivery_note::model::TPart;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ScanKind {
    /// Part 命中，且 `part.assembly_id IS NULL` → 散件扫描。
    StandalonePart,
    /// Part 命中，但 `part.assembly_id IS NOT NULL` → 视作装配件整套。
    /// 取 `assembly_id` 加载装配件头 + 全部子件。
    PartOfAssembly(i64),
    /// Part 未命中但 Assembly 命中 → 装配件总图。
    Assembly,
    /// 两者都没命中 → 404 `BIZ_DELIVERY_SCAN_UNKNOWN_CODE`。
    Unknown,
}

/// 纯函数：根据 SQL 已装载的 part / assembly 行决定 scan 处理的形态。
/// 测试覆盖在 `mod scan_resolve_tests`。
pub(super) fn resolve_scan_kind(part: Option<&TPart>, assembly: Option<&TAssembly>) -> ScanKind {
    match (part, assembly) {
        (Some(p), _) => {
            if let Some(aid) = p.assembly_id {
                ScanKind::PartOfAssembly(aid)
            } else {
                ScanKind::StandalonePart
            }
        }
        (None, Some(_)) => ScanKind::Assembly,
        (None, None) => ScanKind::Unknown,
    }
}
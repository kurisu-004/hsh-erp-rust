//! com::union_list 域响应 VO
//!
//! 2026-09-29 新增：跨表合并视图（part UNION assembly）共用 `PartListOut` /
//! `PartListItem`（已含 `row_type` / `has_children` / `child_count` 三字段，
//! 2026-09-28 row-type-merge 期已就位）。本模块仅 re-export，不复制定义。

pub use crate::modules::part::vo::{PartListItem, PartListOut};

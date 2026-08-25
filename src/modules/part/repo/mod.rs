//! part 域数据访问（按表职责拆分）
//!
//! 4 个文件：
//! - `part.rs`：`t_part` 主表查询 + 状态机 UPDATE
//! - `batch.rs`：`t_part_batch` 批次查询 + 状态机 UPDATE
//! - `event.rs`：`t_part_event` 事件日志
//!
//! 调用方式保持兼容：`crate::modules::part::repo::PartRepo::xxx(...)`。

pub mod batch;
pub mod event;
pub mod part;

pub struct PartRepo;
//! 业务单号/序列号计数
//!
//! 对应 Python myERP/core/serial.py（除雪花外的部分）：
//! - 客户级序列号循环计数器（`<单字母前缀><4位数字>`，pool=9000）
//! - 送货单每日单号计数器
//!
//! 骨架阶段仅占位，业务实现阶段补充：
//! - SQL 计数表读写（truncate-and-restart-transaction）
//! - 高并发下的 row-level lock
//! - 时区归属（Asia/Shanghai 当日）

use crate::shared::error::AppError;

/// 客户级循环序列号。骨架阶段返回未实现错误。
pub fn next_customer_serial(_customer_id: i64, _prefix: &str) -> Result<String, AppError> {
    unimplemented!("实施阶段实现客户级序列号循环")
}

/// 送货单当日单号。骨架阶段返回未实现错误。
pub fn next_delivery_note_no(_customer_id: i64) -> Result<String, AppError> {
    unimplemented!("实施阶段实现送货单单号")
}
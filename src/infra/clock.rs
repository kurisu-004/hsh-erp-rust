//! 时区工具：统一 Asia/Shanghai
//!
//! 对应 Python myERP/core/time.py。DB 列 `datetime` 全部存 naive，应用层使用以下函数。

use chrono::{DateTime, FixedOffset, NaiveDateTime, Utc};

const SHANGHAI_OFFSET_SECONDS: i32 = 8 * 3600;

pub fn shanghai_tz() -> FixedOffset {
    FixedOffset::east_opt(SHANGHAI_OFFSET_SECONDS).expect("固定偏移合法")
}

/// 当前 Shanghai 时间（带时区）
pub fn now_shanghai() -> DateTime<FixedOffset> {
    Utc::now().with_timezone(&shanghai_tz())
}

/// 当前 Shanghai 时间（naive，用于 DB datetime 列写入）
pub fn now_naive() -> NaiveDateTime {
    now_shanghai().naive_local()
}

/// 当前 Shanghai 时间 ISO 字符串（用于 WebSocket 推送等）
pub fn now_shanghai_iso() -> String {
    now_shanghai().to_rfc3339()
}
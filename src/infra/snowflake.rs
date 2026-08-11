//! 雪花 ID 生成器
//!
//! 对应 Python myERP/core/serial.py 雪花部分。41 位时间戳 + 5 位 datacenter + 5 位 worker + 12 位 sequence。
//! DB 主键统一使用 `i64` 雪花 ID；JSON 序列化由 [`crate::shared::types`] 转为字符串避免 JS 精度问题。

use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

pub struct SnowflakeIdGenerator {
    inner: Mutex<Inner>,
}

struct Inner {
    epoch_ms: u64,
    datacenter_id: u16,
    worker_id: u16,
    last_ms: u64,
    sequence: u16,
}

impl SnowflakeIdGenerator {
    pub fn new(epoch_ms: u64, datacenter_id: u16, worker_id: u16) -> Self {
        Self {
            inner: Mutex::new(Inner {
                epoch_ms,
                datacenter_id,
                worker_id,
                last_ms: 0,
                sequence: 0,
            }),
        }
    }

    /// 生成下一个雪花 ID
    pub fn next_id(&self) -> i64 {
        let mut g = self.inner.lock().expect("snowflake mutex poisoned");
        let now = now_ms_since(g.epoch_ms);

        if now > g.last_ms {
            g.last_ms = now;
            g.sequence = 0;
            return compose(g.last_ms, g.datacenter_id, g.worker_id, 0);
        }

        // now == last_ms：递增 sequence
        let seq = g.sequence.wrapping_add(1) & 0xFFF; // 12 bits
        if seq == 0 {
            // 4096/ms 用尽：等待下一毫秒后重试
            drop(g);
            std::thread::sleep(std::time::Duration::from_millis(1));
            return self.next_id();
        }
        g.sequence = seq;
        compose(g.last_ms, g.datacenter_id, g.worker_id, seq)
    }
}

fn now_ms_since(epoch_ms: u64) -> u64 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("系统时间早于 Unix 纪元")
        .as_millis() as u64;
    now.saturating_sub(epoch_ms)
}

fn compose(ts_ms: u64, datacenter: u16, worker: u16, sequence: u16) -> i64 {
    (((ts_ms & 0x1FFFFFFFFFF) << 17)
        | ((datacenter as u64 & 0x1F) << 12)
        | ((worker as u64 & 0x1F) << 7)
        | (sequence as u64 & 0xFFF)) as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snowflake_basic() {
        let g = SnowflakeIdGenerator::new(0, 1, 1);
        let id1 = g.next_id();
        let id2 = g.next_id();
        assert!(id1 > 0);
        assert!(id2 > id1);
    }
}
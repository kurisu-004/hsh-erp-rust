//! 雪花 ID 生成器（41 位时间戳 + 10 位 instance + 12 位 sequence，共 63 位）。
//!
//! 位布局与 myERP Python 完全一致：`ts << 22 | instance << 12 | seq`，
//! 保证跨语言 ID 互解。Python 参考：
//! `/Users/ren/Code/myERP/utils/id_gen.py` → `snowflake-id` PyPI 包（v1.0.2，
//! `site-packages/snowflake/snowflake.py`）。
//!
//! DB 主键统一使用 `i64` 雪花 ID；JSON 序列化由 [`crate::shared::types`] 转为
//! 字符串避免 JS 精度问题。

use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// 10 位 instance 上界（含），对齐 Python `MAX_INSTANCE = 0x3FF`。
const MAX_INSTANCE: u16 = 0x3FF;
/// 12 位 sequence 上界（含），对齐 Python `MAX_SEQ = 0xFFF`。
const MAX_SEQUENCE: u16 = 0xFFF;
/// 41 位 timestamp 上界（含），对齐 Python `MAX_TS`。
const MAX_TIMESTAMP: u64 = 0x1FFFFFFFFFF;

pub struct SnowflakeIdGenerator {
    inner: Mutex<Inner>,
}

struct Inner {
    epoch_ms: u64,
    instance_id: u16,
    last_ms: u64,
    sequence: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SnowflakeParts {
    /// 距 epoch 的毫秒数（41 位）。
    pub timestamp_ms_since_epoch: u64,
    /// 节点实例号（10 位）。
    pub instance: u16,
    /// 同毫秒内的递增序列号（12 位）。
    pub sequence: u16,
}

impl SnowflakeIdGenerator {
    /// 构造雪花 ID 生成器。
    ///
    /// `instance_id` 必须 ≤ 1023（10 位），否则 panic —— 构造期失败比运行时
    /// 静默错位 ID 更安全，对齐 Python `SnowflakeGenerator.__init__` 的 raise 行为。
    pub fn new(epoch_ms: u64, instance_id: u16) -> Self {
        assert!(
            instance_id <= MAX_INSTANCE,
            "snowflake instance_id {instance_id} 超出 10 位范围 0..={MAX_INSTANCE}"
        );
        Self {
            inner: Mutex::new(Inner {
                epoch_ms,
                instance_id,
                last_ms: 0,
                sequence: 0,
            }),
        }
    }

    /// 生成下一个雪花 ID。
    ///
    /// 与 Python `SnowflakeGenerator.__next__` 的差异：本实现在同一毫秒用尽
    /// 4096 个 sequence 后会 `sleep(1ms)` 并重试，而 Python 返回 `None`。
    /// 这是为了让 Rust 端业务调用方不必处理 `Option`。
    pub fn next_id(&self) -> i64 {
        let mut g = self.inner.lock().expect("snowflake mutex poisoned");
        let now = now_ms_since(g.epoch_ms);

        if now > g.last_ms {
            g.last_ms = now;
            g.sequence = 0;
            return compose(g.last_ms, g.instance_id, 0);
        }

        // now == last_ms：递增 sequence，并显式 mask 保证 ≤ 0xFFF。
        let seq = g.sequence.wrapping_add(1) & MAX_SEQUENCE;
        if seq == 0 {
            // 4096/ms 用尽：释放锁后等待下一毫秒，再递归重试。
            drop(g);
            std::thread::sleep(std::time::Duration::from_millis(1));
            return self.next_id();
        }
        g.sequence = seq;
        compose(g.last_ms, g.instance_id, seq)
    }

    /// 反解雪花 ID 的三个字段（位布局验证用 + 跨语言互解验证）。
    ///
    /// 仅按位解析，不校验 timestamp 段是否合理（极端小 / 极端大）。
    pub fn parse(&self, id: i64) -> SnowflakeParts {
        let v = id as u64;
        SnowflakeParts {
            timestamp_ms_since_epoch: (v >> 22) & MAX_TIMESTAMP,
            instance: ((v >> 12) & MAX_INSTANCE as u64) as u16,
            sequence: (v & MAX_SEQUENCE as u64) as u16,
        }
    }
}

fn now_ms_since(epoch_ms: u64) -> u64 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("系统时间早于 Unix 纪元")
        .as_millis() as u64;
    now.saturating_sub(epoch_ms)
}

fn compose(ts_ms: u64, instance: u16, sequence: u16) -> i64 {
    (((ts_ms & MAX_TIMESTAMP) << 22)
        | ((instance as u64 & MAX_INSTANCE as u64) << 12)
        | (sequence as u64 & MAX_SEQUENCE as u64)) as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snowflake_basic() {
        let g = SnowflakeIdGenerator::new(0, 1);
        let id1 = g.next_id();
        let id2 = g.next_id();
        assert!(id1 > 0);
        assert!(id2 > id1);
    }

    /// 与 myERP Python `snowflake-id` 包的位布局公式一致：
    /// `ts << 22 | instance << 12 | seq`。
    #[test]
    fn compose_matches_python_formula() {
        let cases = [
            (0u64, 0u16, 0u16),
            (1u64, 0u16, 0u16),
            (123_456u64, 42u16, 7u16),
            (MAX_TIMESTAMP, MAX_INSTANCE, MAX_SEQUENCE),
            (MAX_TIMESTAMP, 1, 1),
        ];
        for (ts, instance, seq) in cases {
            let expected: i64 = ((ts as u128) << 22 | (instance as u128) << 12 | seq as u128) as i64;
            assert_eq!(
                compose(ts, instance, seq),
                expected,
                "ts={ts} instance={instance} seq={seq}"
            );
        }
    }

    /// 三段互不重叠：每段都能被正确 mask 回原值。
    #[test]
    fn bits_dont_overlap() {
        // 抽样而非全量（41 位空间太大），覆盖边界 + 任意组合。
        let mut instances = vec![0u16, 1, 31, 32, 511, 512, 1023];
        let sequences: [u16; 5] = [0, 1, 0x7FF, 0x800, 0xFFF];
        let timestamps = [
            0u64,
            1,
            MAX_TIMESTAMP / 2,
            MAX_TIMESTAMP - 1,
            MAX_TIMESTAMP,
        ];
        for &ts in &timestamps {
            for &ins in &instances {
                for &seq in &sequences {
                    let id = compose(ts, ins, seq);
                    let back_ts = ((id as u64) >> 22) & MAX_TIMESTAMP;
                    let back_ins = ((id as u64) >> 12) & (MAX_INSTANCE as u64);
                    let back_seq = id as u64 & (MAX_SEQUENCE as u64);
                    assert_eq!(back_ts, ts, "ts mismatch ts={ts} ins={ins} seq={seq}");
                    assert_eq!(back_ins, ins as u64, "instance mismatch ts={ts} ins={ins} seq={seq}");
                    assert_eq!(back_seq, seq as u64, "seq mismatch ts={ts} ins={ins} seq={seq}");
                }
            }
        }
        instances.clear();
    }

    #[test]
    fn parse_round_trip() {
        let g = SnowflakeIdGenerator::new(0, 7);
        let parts = SnowflakeParts {
            timestamp_ms_since_epoch: 123_456_789,
            instance: 42,
            sequence: 4095,
        };
        let id = compose(
            parts.timestamp_ms_since_epoch,
            parts.instance,
            parts.sequence,
        );
        let back = g.parse(id);
        assert_eq!(back, parts);
    }

    #[test]
    #[should_panic(expected = "超出 10 位范围")]
    fn instance_overflow_panics() {
        let _ = SnowflakeIdGenerator::new(0, MAX_INSTANCE + 1);
    }

    #[test]
    fn instance_boundary_ok() {
        // MAX_INSTANCE 上界（含）必须可构造。
        let _ = SnowflakeIdGenerator::new(0, MAX_INSTANCE);
    }

    /// 同一毫秒内连续生成：sequence 单调递增，不溢出到 instance 段。
    #[test]
    fn sequence_monotonic_in_same_ms() {
        let g = SnowflakeIdGenerator::new(1_735_689_600_000, 5);
        let a = g.next_id();
        let b = g.next_id();
        let c = g.next_id();
        // 极可能在同一毫秒生成；a/b/c 的 instance 段应一致。
        let parts_a = g.parse(a);
        let parts_b = g.parse(b);
        let parts_c = g.parse(c);
        assert_eq!(parts_a.instance, 5);
        assert_eq!(parts_b.instance, 5);
        assert_eq!(parts_c.instance, 5);
        assert!(parts_b.sequence > parts_a.sequence || parts_b.timestamp_ms_since_epoch > parts_a.timestamp_ms_since_epoch);
        assert!(parts_c.sequence > parts_b.sequence || parts_c.timestamp_ms_since_epoch > parts_b.timestamp_ms_since_epoch);
    }

    /// epoch=0 烟雾：连续 100 次生成的 ID 必须单调递增、全部 > 0。
    #[test]
    fn epoch_zero_smoke() {
        let g = SnowflakeIdGenerator::new(0, 0);
        let mut prev = 0i64;
        for _ in 0..100 {
            let id = g.next_id();
            assert!(id > 0);
            assert!(id > prev, "snowflake id 未单调递增：prev={prev} id={id}");
            prev = id;
        }
    }

    /// 跨语言互解固定值：与 Python `Snowflake.parse` 的语义一致。
    ///
    /// Python 在 epoch=1735689600000、instance=7、seq=100 时：
    ///   value = (0 << 22) | (7 << 12) | 100 = 7*4096 + 100 = 28772
    ///   parse(28772) → timestamp=0, instance=7, seq=100
    /// 这里固定一个 epoch 偏移后的 ID 验证。
    #[test]
    fn parse_python_compat_hardcoded() {
        // ts=1000, instance=7, seq=100
        // value = (1000 << 22) | (7 << 12) | 100
        let id: i64 = ((1000u128 << 22) | (7u128 << 12) | 100u128) as i64;
        let g = SnowflakeIdGenerator::new(0, 0);
        let parts = g.parse(id);
        assert_eq!(parts.timestamp_ms_since_epoch, 1000);
        assert_eq!(parts.instance, 7);
        assert_eq!(parts.sequence, 100);
    }

    /// 真实 myERP Python `snowflake-id==1.0.2` 包在同一 ms 连产三条 ID，
    /// 验证 Rust 端 `parse()` 反解字段与 Python `Snowflake.parse` 完全一致。
    ///
    /// Python fixture（instance=7, epoch=1735689600000）：
    ///   ts=52027689321 instance=7 seq=1  → 218219945429856257
    ///   ts=52027689321 instance=7 seq=2  → 218219945429856258
    ///   ts=52027689321 instance=7 seq=3  → 218219945429856259
    #[test]
    fn parse_python_compat_real_ids() {
        let cases: [(i64, u64, u16, u16); 3] = [
            (218_219_945_429_856_257, 52_027_689_321, 7, 1),
            (218_219_945_429_856_258, 52_027_689_321, 7, 2),
            (218_219_945_429_856_259, 52_027_689_321, 7, 3),
        ];
        let g = SnowflakeIdGenerator::new(0, 0);
        for (id, ts, ins, seq) in cases {
            assert_eq!(
                g.parse(id),
                SnowflakeParts {
                    timestamp_ms_since_epoch: ts,
                    instance: ins,
                    sequence: seq,
                },
                "跨语言互解失败：id={id}"
            );
        }
    }
}
//! `src/` 单元测试专用的**进程内唯一**雪花 ID 源（`#[cfg(test)]` 项，不进 lib 产物）。
//!
//! ## 为什么不能直接用 test-support 的同名函数（2026-10-09 踩坑记录）
//! `hsh-erp-test-support` 是 `[dev-dependencies]`（见 `Cargo.toml`），而它自己又
//! path-depends 回主 crate —— 这构成 dev-dependency 环。于是编译 lib 单测目标
//! （`cargo test --lib` / nextest 的 lib profile）时，**同一个二进制里会被链进两份
//! `hsh_erp_rust`**：一份是带 `cfg(test)` 的 lib 测试构建（单元测试用的就是这份），
//! 一份是 test-support 依赖的、不带 `cfg(test)` 的普通构建。两份是**各自独立的
//! crate 实例**，故 `hsh_erp_test_support::shared_test_snowflake()` 返回的
//! `Arc<SnowflakeIdGenerator>` 与 `crate::infra::snowflake::SnowflakeIdGenerator`
//! **是两个不同的类型**，直接传参即 `error[E0308]`（并附
//! "there are multiple different versions of crate `hsh_erp_rust` in the dependency
//! graph"）。
//!
//! test-support 里那些**返回外部 crate 类型**的入口（`sqlx::PgPool`、redis URL 等）
//! 不受影响，可以照常用 —— `test_pool()` 仍是 lib 单测建库的唯一入口。
//!
//! ## 两个进程域各有一个 generator，互不干扰（这已经够了）
//! - **lib 单测**（`--lib`）：只走本模块的 generator。单元测试**不使用**
//!   `test_support::state::{test_app, test_state, test_ws_app}`（它们内部才用
//!   test-support 的 generator），所以本进程内没有任何第二条 id 流。
//! - **集成测试**（`tests/**` 各自 binary）：每个 binary 只链一份 `hsh_erp_rust`
//!   （无 `cfg(test)` 的普通构建），故它们走 test-support 的
//!   `shared_test_snowflake()`，进程内唯一性由那个对象保证。
//!
//! 二者是**不同进程**（`cargo test` 里 lib 单测与每个 integration test binary 各起
//! 各的进程），所以「同一个 generator 对象」这个要求在进程之间并不需要成立；
//! in-process 唯一即可，而每个测试又有 `test_pool()` 现建的独立数据库。
//!
//! ## 根因回顾（与 test-support `shared_test_snowflake` 的 doc 同源）
//! 位布局 `ts << 22 | instance << 12 | seq`，其中 `last_ms` / `sequence` 属于
//! generator 的**实例私有**字段，而 `SnowflakeIdGenerator::new()` 一律从
//! `last_ms=0, sequence=0` 起步。于是**任意两个 instance 相同、但对象不同**的
//! generator，只要在同一毫秒各自取到第 j 个号，就发出**逐字节相同**的 id，撞
//! `t_*_pkey` 报 `23505` —— 碰撞判据不是「同一个 helper 调几次」，而是
//! **两个不同 helper 各调一次**（2026-10-09 本轮改造前，
//! `prod::queue::service::dispatch` 的 7 个 helper 就是实例全为 7、各自现建
//! generator 的典型）。
//!
//! 故「进程内唯一」的正确保证点是**对象共享**，不是 instance 编号。

use std::sync::{Arc, OnceLock};

use crate::infra::snowflake::SnowflakeIdGenerator;

/// 进程内唯一的测试 generator（`Arc` 是因为 `SnowflakeIdGenerator` 内部是
/// `Mutex<Inner>` 且**没有** `Clone`，不包一层就没法把同一对象交给多个调用点）。
static SHARED_TEST_SNOWFLAKE: OnceLock<Arc<SnowflakeIdGenerator>> = OnceLock::new();

/// 全进程唯一的测试雪花 ID 源（`src/` 单元测试专用，见模块 doc）。
///
/// 调用方：`.next_id()` 取号；或 `.clone()` 拿 `Arc` 传给收
/// `&SnowflakeIdGenerator` / `Arc<SnowflakeIdGenerator>` 形参的 service 方法
/// （本仓两者都有，故 helper 保持原调用形态即可）。
pub(crate) fn shared_test_snowflake() -> &'static Arc<SnowflakeIdGenerator> {
    SHARED_TEST_SNOWFLAKE.get_or_init(|| {
        Arc::new(SnowflakeIdGenerator::new(
            1_577_836_800_000,
            test_snowflake_instance(),
        ))
    })
}

/// per-process snowflake instance：`pid ⊕ 启动纳秒低位 → 0-1023`。
///
/// 职责**只有跨进程**这 10 bit（1024 槽）的区分，进程内唯一性由「共享同一个
/// generator 对象」保证（见模块 doc 与 `test_snowflake_instance` 的根因说明）。
/// 刻意**不**写死 `instance = 7`：那正是本轮改造前 `dispatch.rs` 7 个 helper 的
/// 写法，写死的字面量还有 1/1024 概率与别的进程撞上。
fn test_snowflake_instance() -> u16 {
    static INSTANCE: OnceLock<u16> = OnceLock::new();
    *INSTANCE.get_or_init(|| {
        let pid = std::process::id() as u64;
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos() as u64;
        ((pid ^ nanos) % 1024) as u16
    })
}

/// 2026-10-09：迁移前后的取号来源对照（供 review 逐行核对，确认零断言改动）。
///
/// - 迁移前 `dispatch.rs`：7 个 helper + 7 个 `#[tokio::test]` 各写
///   `SnowflakeIdGenerator::new(1_577_836_800_000, 7)`；
/// - 迁移后：一律 `crate::shared::test_snowflake::shared_test_snowflake()`。
#[cfg(test)]
mod tests {
    use super::*;

    /// 同一 generator 连发两个号必然不同 —— 这条不变式是整套迁移成立的前提
    /// （迁移前的两个 instance=7 独立对象在同一毫秒会发出**逐字节相同**的号）。
    #[test]
    fn shared_generator_never_repeats_an_id() {
        let a = shared_test_snowflake().next_id();
        let b = shared_test_snowflake().next_id();
        assert_ne!(a, b);
    }

    /// instance 落在 10 bit 内（位段不被溢出截断到 seq 段 ⇒ 跨进程区分有效）。
    #[test]
    fn instance_stays_within_10_bits() {
        assert!(test_snowflake_instance() < 1024);
    }

    /// 并发取号仍必须互不相同：lib 单测是多线程跑的，若发号不串行，两个线程在同一
    /// 毫秒各自取到第 0 号就会撞 `t_*_pkey`（串行由 `SnowflakeIdGenerator` 内部的
    /// `Mutex<Inner>` 保证，本模块只负责让全进程拿到同一个对象）。
    #[test]
    fn concurrent_callers_get_distinct_ids() {
        let mut handles = Vec::new();
        for _ in 0..4 {
            handles.push(std::thread::spawn(|| {
                (0..16)
                    .map(|_| shared_test_snowflake().next_id())
                    .collect::<Vec<i64>>()
            }));
        }
        let mut all: Vec<i64> = handles
            .into_iter()
            .flat_map(|h| h.join().expect("取号线程不应 panic"))
            .collect();
        assert_eq!(all.len(), 64);
        all.sort_unstable();
        let before = all.len();
        all.dedup();
        assert_eq!(before, all.len(), "共享 generator 不应发出重复 id");
    }
}

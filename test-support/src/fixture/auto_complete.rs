//! 集成测试 fixture（PR13 Phase I 引入，2026-09-24）：auto_complete 域预制 fixture
//!
//! `tests/auto_complete_api.rs`（207 行）单文件使用。覆盖
//! `task::auto_complete::run_once` 的 3 个核心场景：
//! - DELIVERED + 早于阈值 → 翻 COMPLETED
//! - DELIVERED + 在阈值内 → 不动
//! - commit 后 ws_hub 收到 PART_COMPLETED 事件
//!
//! ## 字段按域需求聚合
//! - `t_customer` ×2 —— L1（fx_auto_complete_l1，prefix='F'）+ L2（fx_auto_complete_l2）
//!
//! 不预置 `t_part` / `t_part_batch` / `t_part_event`：每个用例现场通过
//! `seed_delivered_batch` helper 创建专属数据（不同 placed_days_ago），
//! 避免 fixture 占用 serial_no 与测试现场字面冲突（uk_t_part_serial_no_active）。

use sqlx::PgPool;

/// `fixtures/auto_complete.sql` 加载产物：常量 ID 句柄供测试函数直接使用。
#[allow(dead_code)]
pub struct AutoCompleteFixture {
    /// baseline L1 customer id（fx_auto_complete_l1，对应 t_customer_id=180）
    pub l1_customer_id: i64,
    /// baseline L2 customer id（fx_auto_complete_l2，对应 t_customer_id=181）
    pub l2_customer_id: i64,
}

impl AutoCompleteFixture {
    pub const L1_CUSTOMER_ID: i64 = 9_000_000_000_000_000_180;
    pub const L2_CUSTOMER_ID: i64 = 9_000_000_000_000_000_181;
}

impl Default for AutoCompleteFixture {
    fn default() -> Self {
        Self {
            l1_customer_id: AutoCompleteFixture::L1_CUSTOMER_ID,
            l2_customer_id: AutoCompleteFixture::L2_CUSTOMER_ID,
        }
    }
}

/// 加载 auto_complete fixture。
///
/// SQL 走 `include_str!` 编译期嵌入本 crate，路径相对本 crate
/// `CARGO_MANIFEST_DIR`（= `test-support/`），即 `fixtures/auto_complete.sql`。
///
/// SQL 内 INSERT 全部走常量 ID（不依赖运行时雪花 ID），跨测试并行 / 跨进程
/// 重跑都不会撞 ID。
///
/// ## 加载顺序
/// 1. 直接 `sqlx::raw_sql(auto_complete.sql)` —— 加载 2 行（1 L1 + 1 L2）。
/// 2. **不**调 [`load_part_fixture`](super::part::load_part_fixture)——
///    auto_complete 测试不需要 part 域基线。
#[allow(dead_code)]
pub async fn load_auto_complete_fixture(pool: &PgPool) -> AutoCompleteFixture {
    let sql = include_str!("../../fixtures/auto_complete.sql");
    sqlx::raw_sql(sql)
        .execute(pool)
        .await
        .expect("load_auto_complete_fixture: insert fixture rows");
    AutoCompleteFixture::default()
}
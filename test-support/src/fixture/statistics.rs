//! 集成测试 fixture（按域拆分）：加载 `fixtures/statistics.sql` 提供强类型句柄
//!
//! 2026-09-24 PR13 Phase H 新增：statistics 域预制 fixture。
//!
//! 与 [`production`](super::production) / [`assembly`](super::assembly) /
//! [`shelf`](super::shelf) 子模块同结构：
//! 1. SQL 落到 `test-support/fixtures/statistics.sql`，本域自身仅 1 行
//!    （1 工种 baseline），ID 段 90（process_chain 1-9 / part 10-49 /
//!    delivery 50-52 / production 60-69 / assembly 70+ / shelf 80+）；
//! 2. 本文件定义 `StatisticsFixture` struct + 常量
//!    + `load_statistics_fixture(pool)` 函数；
//! 3. 测试 binary `use hsh_erp_test_support::fixture::*;` 直接拿到句柄。
//!
//! ## 复用 part 域基线
//! statistics 域测试走 service 层直调（不开 HTTP、不经 JWT / Redis），按需构造
//! `CurrentUser { roles: vec![Role::Manager], .. }` 或直接调 `StatisticsService::*` /
//! `statistics::repo::sql::*`。fixture 复用 part 域基线（ID 段 10-49 在 part.sql 内）
//! 是为保持跨域 fixture 形态一致 + 未来若加 HTTP 契约测试可直接复用 fx_part_manager。
//!
//! ## 为什么 fixture 极简（仅 1 工种）
//! statistics 域 10 个场景（api.rs 7 + event_driven.rs 3）全部走 service 层直调，
//! 每个用例都要按需造不同 prefix 的 L1 + 不同 status / event_type 的 part /
//! event / 不同 badge 的 worker：
//! - `uq_t_customer_root_prefix` 唯一索引要求 L1 prefix 全局唯一，测试自建
//!   L1(prefix='F')，fixture 预置会冲突；
//! - 状态机不允许 part 从 IN_PROCESS / COMPLETED 回退 PENDING，预置 PENDING 行
//!   会让 `count_in_process_at` 等「期望空库」断言失败；
//! - part_event / pickup_skip_event 都是测试现场按 event_type / worker_id /
//!   part_id / created_at 构造，fixture 不预置避免污染；
//! - **t_worker 不预置**：worker_stats 端点按日期范围列出全部工人，api.rs
//!   `workers_stats_happy_path` 断言 `out.items.len() == 2`，预置 1 个
//!   worker 会让 list 出现第 3 行破坏断言；各 sub-file 用本地
//!   `insert_worker(&pool, badge, name, Some(wt))` 按需造 badge / 名称 / 工种。
//!
//! fixture 仅放 1 工种 baseline（提供强类型 `fx.work_type_id` 句柄），给
//! 「service 层直调时可选引用」的共享点；其余行由各 sub-file 走本地 helper
//! 自建。

use sqlx::PgPool;

/// `fixtures/statistics.sql` 加载产物：常量 ID 句柄供测试函数直接使用。
///
/// 字段名采用「原 tests/statistics/ 内曾出现过的字段名」最小集，避免与 fixture
/// 字段名重构产生 churn；常量 ID 与 SQL INSERT 字面值逐字对应。
///
/// **不预置 t_customer / t_part / t_part_batch / t_part_event /
/// t_pickup_skip_event / t_worker**：fixture 只放「不可变共享」基线（1 工种）。
/// statistics 域测试需要按需造不同 L1 prefix / 不同 part status / 不同
/// event_type / 不同 badge 的 worker，状态机不允许从 IN_PROCESS 回退 PENDING，
/// worker_stats 端点按日期范围列全部工人不能预置 worker；预置行会让
/// count_in_process_at / workers_stats_happy_path 等「期望空库 / 期望严格
/// 行数」断言失败。各 sub-file 按需用 sqlx::query 直插 customer / part /
/// batch / event / pickup_skip_event / worker。
#[allow(dead_code)]
pub struct StatisticsFixture {
    /// 复用 PartFixture：L1 客户 id（带 serial_prefix='P'，statistics 域测试如
    /// 需走 part 域基线 user 登录时使用）
    pub part_customer_l1_id: i64,
    /// 复用 PartFixture：L2 客户 id（挂在 L1 下，prefix=NULL）
    pub part_customer_l2_id: i64,
    /// 复用 PartFixture：INHOUSE 工序 FX-PROC-A id
    pub part_process_id: i64,
    /// 复用 PartFixture：工种 FX-WT-A id
    pub part_work_type_id: i64,
    /// 复用 PartFixture：INSPECTION 货架 id
    pub part_inspection_shelf_id: i64,
    /// 复用 PartFixture：PRODUCTION 货架 id
    pub part_production_shelf_id: i64,
    /// 复用 PartFixture：MANAGER 用户 id（statistics 当前 service-direct，
    /// 未来若加 HTTP 契约测试可复用此用户登录）
    pub part_manager_user_id: i64,
    /// 复用 PartFixture：MANAGER 用户名
    pub part_manager_username: String,
    /// statistics 自有：工种 FX-WT-STAT id（baseline 共享，worker_stats /
    /// pickup_skip_summary 端点按 worker 聚合，工种存在不引入额外工人）
    pub work_type_id: i64,
}

impl StatisticsFixture {
    /// fixture 内用户共用的明文密码（bcrypt 哈希嵌入 part.sql）。
    /// 改此处必须同步更新 fixtures/part.sql 的 password_hash 字面值。
    pub const PASSWORD: &'static str = "changeme";

    pub const WORK_TYPE_ID: i64 = 9_000_000_000_000_000_090;
}

impl Default for StatisticsFixture {
    fn default() -> Self {
        use super::part::PartFixture;
        Self {
            part_customer_l1_id: PartFixture::CUSTOMER_L1_ID,
            part_customer_l2_id: PartFixture::CUSTOMER_L2_ID,
            part_process_id: PartFixture::PROCESS_ID,
            part_work_type_id: PartFixture::WORK_TYPE_ID,
            part_inspection_shelf_id: PartFixture::INSPECTION_SHELF_ID,
            part_production_shelf_id: PartFixture::PRODUCTION_SHELF_ID,
            part_manager_user_id: PartFixture::MANAGER_USER_ID,
            part_manager_username: PartFixture::MANAGER_USERNAME.to_string(),
            work_type_id: StatisticsFixture::WORK_TYPE_ID,
        }
    }
}

/// 加载 statistics fixture。
///
/// SQL 走 `include_str!` 编译期嵌入本 crate，路径相对本 crate
/// `CARGO_MANIFEST_DIR`（= `test-support/`），即 `fixtures/statistics.sql`。
///
/// ## 加载顺序
/// 1. 先 `load_part_fixture(pool)` —— 复用 part 域基线（customer / process /
///    shelf / user / role / 映射），不创建 `PartFixture` 字段，只为基线行；
/// 2. 再 `sqlx::raw_sql(statistics.sql)` —— 加载 statistics 自有 1 行
///    （1 工种 FX-WT-STAT baseline）。
///
/// ## 复用 vs 重复
/// 选择「复用 part fixture」而非「把 part 域基线 SQL 复制到 statistics.sql」，
/// 避免 part 域基线（13 行）散落到多个 SQL 文件里造成更新不同步。part fixture
/// 已经是「不可变共享基线」，statistics 直接调用即可。
#[allow(dead_code)]
pub async fn load_statistics_fixture(pool: &PgPool) -> StatisticsFixture {
    crate::fixture::load_part_fixture(pool).await;
    let sql = include_str!("../../fixtures/statistics.sql");
    sqlx::raw_sql(sql)
        .execute(pool)
        .await
        .expect("load_statistics_fixture: insert fixture rows");
    StatisticsFixture::default()
}
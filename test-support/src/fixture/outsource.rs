//! 集成测试 fixture（按域拆分）：加载 `fixtures/outsource.sql` 提供强类型句柄
//!
//! 2026-09-24 PR13 Phase H 新增：outsource 域预制 fixture。
//!
//! 与 [`production`](super::production) / [`assembly`](super::assembly) /
//! [`shelf`](super::shelf) / [`statistics`](super::statistics) 子模块同结构：
//! 1. SQL 落到 `test-support/fixtures/outsource.sql`，常量 ID 走
//!    9_000_000_000_000_000_100+ 区段（process_chain 1-9 / part 10-49 /
//!    delivery 50-52 / production 60-69 / assembly 70+ / shelf 80+ /
//!    statistics 90+，物理不相交）；
//! 2. 本文件定义 `OutsourceFixture` struct + 常量 ID const
//!    + `load_outsource_fixture(pool)` 函数；
//! 3. 测试 binary `use hsh_erp_test_support::fixture::*;` 直接拿到句柄。
//!
//! ## 复用 part 域基线
//! outsource 测试需要 customer / user / role / shelf / 映射等 part 域已经
//! 预制的「不可变共享基线」行（MANAGER 用户 fx_part_manager 用于登录，
//! quote.rs CLERK 守卫测试用 fx_outsource_clerk）。
//! `load_outsource_fixture` 内部先调 [`load_part_fixture`](super::part::load_part_fixture)
//! 复用 part 域基线（ID 段 10-49 在 `part.sql` 内），再 raw_sql outsource
//! 自有行（ID 段 100+）。
//!
//! ## 当前域
//! - `outsource`：1 OUTSOURCE 类别工序（FX-OPROC-A）+ 1 outsource 公司
//!   （FX-OC-001 active baseline）+ 1 CLERK 用户（fx_outsource_clerk）+ 1 CLERK role。
//!   不预置 t_outsource_quote / t_outsource_shipment / t_part / t_part_batch /
//!   t_part_process_chain / t_process_chain_step：各 sub-file 按需用 sqlx::query
//!   直插（quote 需 part_id + 状态机不允许 part 从 OUTSOURCE 回退 PENDING；预置
//!   会污染 list / count 等「期望空库」断言）。

use sqlx::PgPool;

/// `fixtures/outsource.sql` 加载产物：常量 ID 句柄供测试函数直接使用。
///
/// 字段名采用「原 tests/outsource/ 内曾出现过的字段名」最小集，避免与 fixture
/// 字段名重构产生 churn；常量 ID 与 SQL INSERT 字面值逐字对应。
///
/// **不预置 t_outsource_quote / t_outsource_shipment / t_part / t_part_batch /
/// t_part_process_chain / t_process_chain_step**：fixture 只放「不可变共享」
/// 基线（1 OUTSOURCE 工序 + 1 公司 + 1 CLERK 用户）。outsource 域测试需要
/// 按需造不同 customer prefix / 不同 part status / 不同 quote 状态机 / 不同
/// shipment 数量，状态机不允许从 OUTSOURCE / APPROVED 回退 PENDING；预置行
/// 会让 list / count / 「期望空库」测试断言失败。各 sub-file 按需用 sqlx::query
/// 直插 part / batch / chain / step / quote / shipment，保留本地 helper：
///
/// - `seed_outsource_process` —— OUTSOURCE 类别工序（绕开 fixtures::seed_process
///   category=INHOUSE）
/// - `insert_outsource_company` —— 外协公司（自定义 name / is_active）
/// - `insert_approved_quote` —— APPROVED 状态 quote（绕开 DRAFT→SUBMITTED→APPROVED）
/// - `insert_l1_customer` / `insert_part` / `insert_batch` —— 客户 / 零件 / 批次
/// - `create_chain_for_part` / `create_step` —— 工艺链 + step
///   （PR-3 批次 step 化要求 part 已绑定工艺链）
#[allow(dead_code)]
pub struct OutsourceFixture {
    /// 复用 PartFixture：L1 客户 id（带 serial_prefix='P'，outsource 域测试如
    /// 需走 part 域基线 user 登录时使用）
    pub part_customer_l1_id: i64,
    /// 复用 PartFixture：MANAGER 用户 id
    pub part_manager_user_id: i64,
    /// 复用 PartFixture：MANAGER 用户名（fixture 内含 MANAGER role，用于登录；
    /// outsource 域绝大多数测试以 MANAGER 身份跑）
    pub part_manager_username: String,
    /// outsource 自有：OUTSOURCE 类别工序 FX-OPROC-A id（baseline 共享；
    /// 各 sub-file 测试需要不同 process 时仍走本地 `seed_outsource_process`
    /// 自建额外行）
    pub outsource_process_id: i64,
    /// outsource 自有：t_outsource_company FX-OC-001 id（active baseline；
    /// 各 sub-file 测试需要不同 company 时仍走本地 `insert_outsource_company`
    /// 自建额外行）
    pub outsource_company_id: i64,
    /// outsource 自有：CLERK 用户 id（quote.rs CLERK 守卫测试专用）
    pub clerk_user_id: i64,
    /// outsource 自有：CLERK 用户名（fixture 内含 CLERK role，无 scope）
    pub clerk_username: String,
}

impl OutsourceFixture {
    /// fixture 内用户共用的明文密码（bcrypt 哈希嵌入 outsource.sql）。
    /// 改此处必须同步更新 fixtures/outsource.sql 的 password_hash 字面值。
    pub const PASSWORD: &'static str = "changeme";

    pub const OUTSOURCE_PROCESS_ID: i64 = 9_000_000_000_000_000_100;
    pub const OUTSOURCE_COMPANY_ID: i64 = 9_000_000_000_000_000_101;
    pub const CLERK_USER_ID: i64 = 9_000_000_000_000_000_103;
    pub const CLERK_ROLE_ID: i64 = 9_000_000_000_000_000_104;

    /// fixture 内 CLERK 用户的 username（与 fixtures/outsource.sql 第 56 行
    /// INSERT 字面对齐）
    pub const CLERK_USERNAME: &'static str = "fx_outsource_clerk";
}

impl Default for OutsourceFixture {
    fn default() -> Self {
        use super::part::PartFixture;
        Self {
            part_customer_l1_id: PartFixture::CUSTOMER_L1_ID,
            part_manager_user_id: PartFixture::MANAGER_USER_ID,
            part_manager_username: PartFixture::MANAGER_USERNAME.to_string(),
            outsource_process_id: OutsourceFixture::OUTSOURCE_PROCESS_ID,
            outsource_company_id: OutsourceFixture::OUTSOURCE_COMPANY_ID,
            clerk_user_id: OutsourceFixture::CLERK_USER_ID,
            clerk_username: OutsourceFixture::CLERK_USERNAME.to_string(),
        }
    }
}

/// 加载 outsource fixture。
///
/// SQL 走 `include_str!` 编译期嵌入本 crate，路径相对本 crate
/// `CARGO_MANIFEST_DIR`（= `test-support/`），即 `fixtures/outsource.sql`。
///
/// SQL 内 INSERT 全部走常量 ID（不依赖运行时雪花 ID），跨测试并行 / 跨进程
/// 重跑都不会撞 ID。
///
/// ## 加载顺序
/// 1. 先 `load_part_fixture(pool)` —— 复用 part 域基线（customer / process /
///    shelf / user / role / 映射），不创建 `PartFixture` 字段，只为基线行；
/// 2. 再 `sqlx::raw_sql(outsource.sql)` —— 加载 outsource 自有 4 行
///    （1 OUTSOURCE 工序 + 1 outsource 公司 + 1 CLERK 用户 + 1 CLERK role）。
///
/// ## 复用 vs 重复
/// 选择「复用 part fixture」而非「把 part 域基线 SQL 复制到 outsource.sql」，
/// 避免 part 域基线（13 行）散落到多个 SQL 文件里造成更新不同步。part fixture
/// 已经是「不可变共享基线」，outsource 直接调用即可。
#[allow(dead_code)]
pub async fn load_outsource_fixture(pool: &PgPool) -> OutsourceFixture {
    crate::fixture::load_part_fixture(pool).await;
    let sql = include_str!("../../fixtures/outsource.sql");
    sqlx::raw_sql(sql)
        .execute(pool)
        .await
        .expect("load_outsource_fixture: insert fixture rows");
    OutsourceFixture::default()
}

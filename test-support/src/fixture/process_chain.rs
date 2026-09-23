//! 集成测试 fixture（按域拆分）：加载 `fixtures/<domain>.sql` 提供强类型句柄
//!
//! 2026-09-23 PR13 Phase F 引入。从 `tests/production/process_chain.rs` 范本
//! 抽出，未来其它 27+ integration test binary 改造时按同模式复用：
//!
//! 1. SQL 落到 `test-support/fixtures/<domain>.sql`，常量 ID 走
//!    9_000_000_000_000_000_001+ 区段；
//! 2. bcrypt 哈希预生成嵌入 SQL，省 ~250ms×N 现场 hash 开销；
//! 3. 本文件定义 `<Domain>Fixture` struct + 常量 ID const
//!    + `load_<domain>_fixture(pool)` 函数；
//! 4. 测试 binary `use hsh_erp_test_support::fixture::*;` 直接拿到句柄。
//!
//! ## 与 [`fixtures`](super::fixtures) 模块的分工
//! - `fixtures`：动态 helper（`insert_user_with_password` / `add_role` 等），
//!   每个测试即时造 1～N 行；适合"参数化差异"场景（不同 username / 不同 role）。
//! - `fixture`（本模块）：预制 SQL 静态行集合，一次 INSERT 9 行；适合"批量差异
//!   跨域共享"场景（process_chain 测试同时用 customer / process / part / user / role）。
//!
//! ## 当前域
//! - `process_chain`：1 客户 + 2 工序 + 2 part（PENDING / IN_PROCESS）+ 2 用户
//!   （MANAGER / CLERK，对应 bcrypt cost=12 哈希嵌入 SQL）

use sqlx::PgPool;

/// `fixtures/process_chain.sql` 加载产物：常量 ID 句柄供测试函数直接使用。
///
/// 字段名采用「原 process_chain.rs 内曾出现过的字段名」最小集，避免与 fixture
/// 字段名重构产生 churn；常量 ID 与 SQL INSERT 字面值逐字对应。
#[allow(dead_code)]
pub struct ProcessChainFixture {
    /// L1 客户 id
    pub customer_id: i64,
    /// 工序 PROC-A id
    pub proc_a: i64,
    /// 工序 PROC-B id
    pub proc_b: i64,
    /// PENDING 状态的 part id（happy / 校验路径用）
    pub part_pending: i64,
    /// IN_PROCESS 状态的 part id（20705 BIZ_PROCESS_CHAIN_PART_NOT_PENDING 守卫用）
    pub part_in_process: i64,
    /// MANAGER 用户名（fixture 内含 MANAGER role）
    pub manager_username: String,
    /// CLERK 用户名（fixture 内含 CLERK role）
    pub clerk_username: String,
}

impl ProcessChainFixture {
    /// fixture 内两个用户共用的明文密码（bcrypt 哈希嵌入 SQL）。
    /// 改此处必须同步更新 fixtures/process_chain.sql 的两个 password_hash 字面值。
    pub const PASSWORD: &'static str = "changeme";

    pub const CUSTOMER_ID: i64 = 9_000_000_000_000_000_001;
    pub const PROC_A: i64 = 9_000_000_000_000_000_002;
    pub const PROC_B: i64 = 9_000_000_000_000_000_003;
    pub const PART_PENDING: i64 = 9_000_000_000_000_000_004;
    pub const PART_IN_PROCESS: i64 = 9_000_000_000_000_000_005;
    pub const MANAGER_USER_ID: i64 = 9_000_000_000_000_000_006;
    pub const CLERK_USER_ID: i64 = 9_000_000_000_000_000_007;
    pub const MANAGER_ROLE_ID: i64 = 9_000_000_000_000_000_008;
    pub const CLERK_ROLE_ID: i64 = 9_000_000_000_000_000_009;

    /// fixture 内 MANAGER 用户的 username（与 fixtures/process_chain.sql 第 41 行 INSERT 字面对齐）
    pub const MANAGER_USERNAME: &'static str = "fx_manager";
    /// fixture 内 CLERK 用户的 username（与 fixtures/process_chain.sql 第 42 行 INSERT 字面对齐）
    pub const CLERK_USERNAME: &'static str = "fx_clerk";
}

impl Default for ProcessChainFixture {
    fn default() -> Self {
        Self {
            customer_id: ProcessChainFixture::CUSTOMER_ID,
            proc_a: ProcessChainFixture::PROC_A,
            proc_b: ProcessChainFixture::PROC_B,
            part_pending: ProcessChainFixture::PART_PENDING,
            part_in_process: ProcessChainFixture::PART_IN_PROCESS,
            manager_username: ProcessChainFixture::MANAGER_USERNAME.to_string(),
            clerk_username: ProcessChainFixture::CLERK_USERNAME.to_string(),
        }
    }
}

/// 加载 process_chain fixture。
///
/// SQL 走 `include_str!` 编译期嵌入本 crate，路径相对本 crate
/// `CARGO_MANIFEST_DIR`（= `test-support/`），即 `fixtures/process_chain.sql`。
///
/// SQL 内 INSERT 全部走常量 ID（不依赖运行时雪花 ID），跨测试并行 / 跨进程
/// 重跑都不会撞 ID。bcrypt 哈希预生成嵌入 SQL，省每测试 ~250ms 现场 hash 开销。
#[allow(dead_code)]
pub async fn load_process_chain_fixture(pool: &PgPool) -> ProcessChainFixture {
    let sql = include_str!("../../fixtures/process_chain.sql");
    sqlx::raw_sql(sql)
        .execute(pool)
        .await
        .expect("load_process_chain_fixture: insert fixture rows");
    ProcessChainFixture::default()
}
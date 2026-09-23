//! 集成测试 fixture（按域拆分）：加载 `fixtures/<domain>.sql` 提供强类型句柄
//!
//! 2026-09-23 PR13 Phase G 新增：part 域预制 fixture。
//!
//! 与 [`process_chain`](super::process_chain) 子模块同结构：
//! 1. SQL 落到 `test-support/fixtures/part.sql`，常量 ID 走
//!    9_000_000_000_000_000_010+ 区段；
//! 2. bcrypt 哈希预生成嵌入 SQL，省 ~250ms×N 现场 hash 开销；
//! 3. 本文件定义 `PartFixture` struct + 常量 ID const
//!    + `load_part_fixture(pool)` 函数；
//! 4. 测试 binary `use hsh_erp_test_support::fixture::*;` 直接拿到句柄。
//!
//! ## 与 [`fixtures`](super::fixtures) 模块的分工
//! - `fixtures`：动态 helper（`insert_user_with_password` / `add_role` 等），
//!   每个测试即时造 1～N 行；适合"参数化差异"场景（不同 username / 不同 role）。
//! - `fixture`（本模块）：预制 SQL 静态行集合，一次 INSERT 13 行；适合"批量差异
//!   跨域共享"场景（part 测试同时用 customer / process / shelf / part / user / role）。
//!
//! ## 当前域
//! - `part`：2 客户（L1 + L2）+ 1 INHOUSE 工序 + 1 工种 + 2 货架（检验 + 生产）
//!   + 2 映射（work_type_process / shelf_process）+ 2 part（PENDING + IN_PROCESS）
//!   + 2 批次 + 4 用户（MANAGER / INSPECTOR / CLERK / SHELF_ACCOUNT）+ 4 role
//!   对应 bcrypt cost=12 哈希嵌入 SQL。

use sqlx::PgPool;

/// `fixtures/part.sql` 加载产物：常量 ID 句柄供测试函数直接使用。
///
/// 字段名采用「原 tests/part/ 内曾出现过的字段名」最小集，避免与 fixture
/// 字段名重构产生 churn；常量 ID 与 SQL INSERT 字面值逐字对应。
///
/// **不预置 part / batch**：fixture 只放「不可变共享」基线（customer / process /
/// shelf / user / role / 映射）。多数 part 域测试需要特定 status（READY_TO_SHIP /
/// DELIVERED / REPAIRING / COMPLETED 等），状态机不允许从这些状态转回 PENDING；
/// 预置 PENDING/IN_PROCESS 行会让「期望空库」测试失败。各 sub-file 按需用
/// `sqlx::query` 直插 part / batch（PR-C 末统一迁 test-support）。
#[allow(dead_code)]
pub struct PartFixture {
    /// L1 客户 id（带 serial_prefix='P'）
    pub customer_l1_id: i64,
    /// L2 客户 id（挂在 L1 下，prefix=NULL）
    pub customer_l2_id: i64,
    /// INHOUSE 工序 FX-PROC-A id（生产架已绑到该工序）
    pub process_id: i64,
    /// 工种 FX-WT-A id
    pub work_type_id: i64,
    /// INSPECTION 货架 id（zone=INSPECTION，SHELF_ACCOUNT scope target）
    pub inspection_shelf_id: i64,
    /// PRODUCTION 货架 id（zone=PRODUCTION，已绑到 process_id）
    pub production_shelf_id: i64,
    /// MANAGER 用户 id
    pub manager_user_id: i64,
    /// INSPECTOR 用户 id
    pub inspector_user_id: i64,
    /// CLERK 用户 id
    pub clerk_user_id: i64,
    /// SHELF_ACCOUNT 用户 id（合法登录但用于越权守卫测试）
    pub shelf_account_user_id: i64,
    /// MANAGER 用户名（fixture 内含 MANAGER role）
    pub manager_username: String,
    /// INSPECTOR 用户名（fixture 内含 INSPECTOR role）
    pub inspector_username: String,
    /// CLERK 用户名（fixture 内含 CLERK role）
    pub clerk_username: String,
    /// SHELF_ACCOUNT 用户名（fixture 内含 SHELF_ACCOUNT role，scope=inspection_shelf_id）
    pub shelf_account_username: String,
}

impl PartFixture {
    /// fixture 内 4 个用户共用的明文密码（bcrypt 哈希嵌入 SQL）。
    /// 改此处必须同步更新 fixtures/part.sql 的 4 个 password_hash 字面值。
    pub const PASSWORD: &'static str = "changeme";

    pub const CUSTOMER_L1_ID: i64 = 9_000_000_000_000_000_010;
    pub const CUSTOMER_L2_ID: i64 = 9_000_000_000_000_000_011;
    pub const PROCESS_ID: i64 = 9_000_000_000_000_000_012;
    pub const WORK_TYPE_ID: i64 = 9_000_000_000_000_000_013;
    pub const INSPECTION_SHELF_ID: i64 = 9_000_000_000_000_000_014;
    pub const PRODUCTION_SHELF_ID: i64 = 9_000_000_000_000_000_015;
    pub const MANAGER_USER_ID: i64 = 9_000_000_000_000_000_016;
    pub const INSPECTOR_USER_ID: i64 = 9_000_000_000_000_000_017;
    pub const CLERK_USER_ID: i64 = 9_000_000_000_000_000_018;
    pub const SHELF_ACCOUNT_USER_ID: i64 = 9_000_000_000_000_000_019;

    /// fixture 内 MANAGER 用户的 username（与 fixtures/part.sql 第 80 行 INSERT 字面对齐）
    pub const MANAGER_USERNAME: &'static str = "fx_part_manager";
    /// fixture 内 INSPECTOR 用户的 username
    pub const INSPECTOR_USERNAME: &'static str = "fx_part_inspector";
    /// fixture 内 CLERK 用户的 username
    pub const CLERK_USERNAME: &'static str = "fx_part_clerk";
    /// fixture 内 SHELF_ACCOUNT 用户的 username（scope 到 INSPECTION_SHELF）
    pub const SHELF_ACCOUNT_USERNAME: &'static str = "fx_part_shelf";
}

impl Default for PartFixture {
    fn default() -> Self {
        Self {
            customer_l1_id: PartFixture::CUSTOMER_L1_ID,
            customer_l2_id: PartFixture::CUSTOMER_L2_ID,
            process_id: PartFixture::PROCESS_ID,
            work_type_id: PartFixture::WORK_TYPE_ID,
            inspection_shelf_id: PartFixture::INSPECTION_SHELF_ID,
            production_shelf_id: PartFixture::PRODUCTION_SHELF_ID,
            manager_user_id: PartFixture::MANAGER_USER_ID,
            inspector_user_id: PartFixture::INSPECTOR_USER_ID,
            clerk_user_id: PartFixture::CLERK_USER_ID,
            shelf_account_user_id: PartFixture::SHELF_ACCOUNT_USER_ID,
            manager_username: PartFixture::MANAGER_USERNAME.to_string(),
            inspector_username: PartFixture::INSPECTOR_USERNAME.to_string(),
            clerk_username: PartFixture::CLERK_USERNAME.to_string(),
            shelf_account_username: PartFixture::SHELF_ACCOUNT_USERNAME.to_string(),
        }
    }
}

/// 加载 part fixture。
///
/// SQL 走 `include_str!` 编译期嵌入本 crate，路径相对本 crate
/// `CARGO_MANIFEST_DIR`（= `test-support/`），即 `fixtures/part.sql`。
///
/// SQL 内 INSERT 全部走常量 ID（不依赖运行时雪花 ID），跨测试并行 / 跨进程
/// 重跑都不会撞 ID。bcrypt 哈希预生成嵌入 SQL，省每测试 ~250ms 现场 hash 开销。
#[allow(dead_code)]
pub async fn load_part_fixture(pool: &PgPool) -> PartFixture {
    let sql = include_str!("../../fixtures/part.sql");
    sqlx::raw_sql(sql)
        .execute(pool)
        .await
        .expect("load_part_fixture: insert fixture rows");
    PartFixture::default()
}
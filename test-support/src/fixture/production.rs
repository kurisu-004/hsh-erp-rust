//! 集成测试 fixture（按域拆分）：加载 `fixtures/production.sql` 提供强类型句柄
//!
//! 2026-09-24 PR13 Phase H 新增：production 域预制 fixture。
//!
//! 与 [`part`](super::part) / [`delivery`](super::delivery) 子模块同结构：
//! 1. SQL 落到 `test-support/fixtures/production.sql`，常量 ID 走
//!    9_000_000_000_000_000_060+ 区段（process_chain 1-9 / part 10-49 /
//!    delivery 50-52 占下层）；
//! 2. 本文件定义 `ProductionFixture` struct + 常量 ID const
//!    + `load_production_fixture(pool)` 函数；
//! 3. 测试 binary `use hsh_erp_test_support::fixture::*;` 直接拿到句柄。
//!
//! ## 复用 part 域基线
//! production 测试需要 customer / user / role / shelf / 映射等 part 域已经
//! 预制的「不可变共享基线」行（MANAGER 用户 fx_part_manager 用于登录）。
//! `load_production_fixture` 内部先调 [`load_part_fixture`](super::part::load_part_fixture)
//! 复用 part 域基线（ID 段 10-49 在 `part.sql` 内），再 raw_sql production
//! 自有行（ID 段 60+）。
//!
//! ## 当前域
//! - `production`：2 INHOUSE 工序（FX-PROC-A / FX-PROC-B）+ 2 工种
//!   （FX-WT-A / FX-WT-B）+ 2 work_type_process 映射。不预置 t_part /
//!   t_part_batch / t_part_process_chain / t_process_chain_step / t_worker
//!   行：各 sub-file 按需用 sqlx::query 直插（process / work_type / worker
//!   域测试常需要特定 code / status / work_type_id / 数量，状态机不允许从这些
//!   状态回退；预置行会让「期望空库」测试断言失败）。

use sqlx::PgPool;

/// `fixtures/production.sql` 加载产物：常量 ID 句柄供测试函数直接使用。
///
/// 字段名采用「原 tests/production/ 内曾出现过的字段名」最小集，避免与 fixture
/// 字段名重构产生 churn；常量 ID 与 SQL INSERT 字面值逐字对应。
///
/// **不预置 part / batch / chain / step / worker**：fixture 只放「不可变共享」
/// 基线（process / work_type / 映射）。多数 production 域测试需要特定 code /
/// status / quantity（如 worker_pool 候选池要求 batch 持有 current_process_step_id），
/// 状态机不允许从这些状态回退；预置行会让 list / count 等「期望空库」断言失败。
/// 各 sub-file 按需用 sqlx::query 直插 part / batch / chain / step / worker。
#[allow(dead_code)]
pub struct ProductionFixture {
    /// 复用 PartFixture：L1 客户 id（带 serial_prefix='P'，production 域送货单 /
    /// 分组 / 装配件都挂在它下）
    pub part_customer_l1_id: i64,
    /// 复用 PartFixture：MANAGER 用户 id
    pub part_manager_user_id: i64,
    /// 复用 PartFixture：MANAGER 用户名（fixture 内含 MANAGER role，用于登录）
    pub part_manager_username: String,
    /// 复用 PartFixture：SHELF_ACCOUNT 用户 id（合法登录但用于越权守卫测试）
    pub part_shelf_account_user_id: i64,
    /// 复用 PartFixture：SHELF_ACCOUNT 用户名（fixture 内含 SHELF_ACCOUNT role，
    /// scope=inspection_shelf_id）
    pub part_shelf_account_username: String,
    /// production 自有：INHOUSE 工序 FX-NA id
    pub process_a_id: i64,
    /// production 自有：INHOUSE 工序 FX-NB id
    pub process_b_id: i64,
    /// production 自有：工种 FX-WTA id
    pub work_type_a_id: i64,
    /// production 自有：工种 FX-WTB id
    pub work_type_b_id: i64,
    /// production 自有：work_type_A ↔ process_A 映射 id
    pub work_type_process_a_id: i64,
    /// production 自有：work_type_B ↔ process_B 映射 id
    pub work_type_process_b_id: i64,
}

impl ProductionFixture {
    /// fixture 内用户共用的明文密码（bcrypt 哈希嵌入 part.sql）。
    /// 改此处必须同步更新 fixtures/part.sql 的 password_hash 字面值。
    pub const PASSWORD: &'static str = "changeme";

    pub const PROCESS_A_ID: i64 = 9_000_000_000_000_000_060;
    pub const PROCESS_B_ID: i64 = 9_000_000_000_000_000_061;
    pub const WORK_TYPE_A_ID: i64 = 9_000_000_000_000_000_062;
    pub const WORK_TYPE_B_ID: i64 = 9_000_000_000_000_000_063;
    pub const WORK_TYPE_PROCESS_A_ID: i64 = 9_000_000_000_000_000_064;
    pub const WORK_TYPE_PROCESS_B_ID: i64 = 9_000_000_000_000_000_065;
}

impl Default for ProductionFixture {
    fn default() -> Self {
        use super::part::PartFixture;
        Self {
            part_customer_l1_id: PartFixture::CUSTOMER_L1_ID,
            part_manager_user_id: PartFixture::MANAGER_USER_ID,
            part_manager_username: PartFixture::MANAGER_USERNAME.to_string(),
            part_shelf_account_user_id: PartFixture::SHELF_ACCOUNT_USER_ID,
            part_shelf_account_username: PartFixture::SHELF_ACCOUNT_USERNAME.to_string(),
            process_a_id: ProductionFixture::PROCESS_A_ID,
            process_b_id: ProductionFixture::PROCESS_B_ID,
            work_type_a_id: ProductionFixture::WORK_TYPE_A_ID,
            work_type_b_id: ProductionFixture::WORK_TYPE_B_ID,
            work_type_process_a_id: ProductionFixture::WORK_TYPE_PROCESS_A_ID,
            work_type_process_b_id: ProductionFixture::WORK_TYPE_PROCESS_B_ID,
        }
    }
}

/// 加载 production fixture。
///
/// SQL 走 `include_str!` 编译期嵌入本 crate，路径相对本 crate
/// `CARGO_MANIFEST_DIR`（= `test-support/`），即 `fixtures/production.sql`。
///
/// SQL 内 INSERT 全部走常量 ID（不依赖运行时雪花 ID），跨测试并行 / 跨进程
/// 重跑都不会撞 ID。
///
/// ## 加载顺序
/// 1. 先 `load_part_fixture(pool)` —— 复用 part 域基线（customer / process /
///    shelf / user / role / 映射），不创建 `PartFixture` 字段，只为基线行；
/// 2. 再 `sqlx::raw_sql(production.sql)` —— 加载 production 自有 5 行
///    （2 INHOUSE 工序 + 2 工种 + 2 work_type_process 映射）。
///
/// ## 复用 vs 重复
/// 选择「复用 part fixture」而非「把 part 域基线 SQL 复制到 production.sql」，
/// 避免 part 域基线（13 行）散落到多个 SQL 文件里造成更新不同步。part fixture
/// 已经是「不可变共享基线」，production 直接调用即可。
#[allow(dead_code)]
pub async fn load_production_fixture(pool: &PgPool) -> ProductionFixture {
    crate::fixture::load_part_fixture(pool).await;
    let sql = include_str!("../../fixtures/production.sql");
    sqlx::raw_sql(sql)
        .execute(pool)
        .await
        .expect("load_production_fixture: insert fixture rows");
    ProductionFixture::default()
}
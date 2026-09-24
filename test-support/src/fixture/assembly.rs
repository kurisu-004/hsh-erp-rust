//! 集成测试 fixture（按域拆分）：加载 `fixtures/assembly.sql` 提供强类型句柄
//!
//! 2026-09-24 PR13 Phase H 新增：assembly 域预制 fixture。
//!
//! 与 [`production`](super::production) / [`delivery`](super::delivery) /
//! [`part`](super::part) 子模块同结构：
//! 1. SQL 落到 `test-support/fixtures/assembly.sql`，本域自身仅 1 行
//!    `t_serial_counter(prefix='F', counter=0)`（PK `prefix varchar(1)`，无
//!    snowflake ID，不占用 70+ 段数值）；
//! 2. 本文件定义 `AssemblyFixture` struct + 常量
//!    + `load_assembly_fixture(pool)` 函数；
//! 3. 测试 binary `use hsh_erp_test_support::fixture::*;` 直接拿到句柄。
//!
//! ## 复用 part 域基线
//! assembly 测试主要走 service 层（不开 HTTP、不经 JWT / Redis），构造
//! `CurrentUser { roles: vec![Role::Manager], .. }` 直调 `AssemblyService::*`。
//! fixture 仅提供 canonical t_serial_counter('F', 0)，复用 part 域基线 user
//! 用于越权守卫测试。
//!
//! ## 为什么 fixture 不预置 t_customer
//! `t_customer.serial_prefix` 有唯一索引 `uq_t_customer_root_prefix`
//! (parent_id IS NULL AND deleted_at IS NULL AND serial_prefix IS NOT NULL)：
//! 全局活跃根客户的 prefix 必须唯一。assembly 测试 18/20 用 'F' prefix、
//! 1/20 用 'X' prefix、每用例都动态插自己的 L1 + L2；预置 L1(prefix='F') 会
//! 与测试自建 L1(prefix='F') 撞唯一约束。fixture 故不预置 t_customer，由各
//! sub-file 按需通过本地 `insert_l1_customer` / `insert_l2_customer` 自建。
//!
//! ## 当前域
//! - `assembly`：1 t_serial_counter（prefix='F', counter=0）。不预置
//!   t_customer / t_assembly / t_part / t_part_batch：assembly 域测试需要
//!   按需造不同 prefix / 不同状态 / 不同 child count 组合，状态机不允许从
//!   IN_PROCESS / COMPLETED 等回退 PENDING，预置会污染 list / count 等
//!   「期望空库」断言。

use sqlx::PgPool;

/// `fixtures/assembly.sql` 加载产物：常量句柄供测试函数直接使用。
///
/// 字段名采用「原 tests/assembly/ 内曾出现过的字段名」最小集，避免与 fixture
/// 字段名重构产生 churn；常量与 SQL INSERT 字面值逐字对应。
///
/// **不预置 t_customer / t_assembly / t_part / t_part_batch**：fixture 只放
/// canonical t_serial_counter(prefix='F', counter=0)（PK 是 varchar(1) 无 snowflake
/// id）。t_customer 因 `uq_t_customer_root_prefix` 唯一索引与测试自建 L1 冲突；
/// t_assembly 等因状态机不允许回退 PENDING 且测试需要不同 child count，预置会
/// 污染「期望空库」断言。各 sub-file 按需走 `AssemblyService::create_assembly`
/// 服务层创建，并通过 `insert_l1_customer` / `insert_l2_customer` 本地 helper
/// 造不同 prefix 的根客户。
#[allow(dead_code)]
pub struct AssemblyFixture {
    /// 复用 PartFixture：L1 客户 id（带 serial_prefix='P'，assembly 测试如需
    /// 走 part 域基线 user 登录时使用）
    pub part_customer_l1_id: i64,
    /// 复用 PartFixture：MANAGER 用户 id
    pub part_manager_user_id: i64,
    /// 复用 PartFixture：MANAGER 用户名（fixture 内含 MANAGER role，用于登录）
    pub part_manager_username: String,
}

impl AssemblyFixture {
    /// fixture 内用户共用的明文密码（bcrypt 哈希嵌入 part.sql）。
    /// 改此处必须同步更新 fixtures/part.sql 的 password_hash 字面值。
    pub const PASSWORD: &'static str = "changeme";

    /// canonical t_serial_counter.prefix（PK 是 varchar(1) 无数值 ID）。
    /// 18/20 assembly 测试用 'F' prefix；'X' 测试本地保留 insert_serial_counter。
    pub const SERIAL_COUNTER_PREFIX: &'static str = "F";
}

impl Default for AssemblyFixture {
    fn default() -> Self {
        use super::part::PartFixture;
        Self {
            part_customer_l1_id: PartFixture::CUSTOMER_L1_ID,
            part_manager_user_id: PartFixture::MANAGER_USER_ID,
            part_manager_username: PartFixture::MANAGER_USERNAME.to_string(),
        }
    }
}

/// 加载 assembly fixture。
///
/// SQL 走 `include_str!` 编译期嵌入本 crate，路径相对本 crate
/// `CARGO_MANIFEST_DIR`（= `test-support/`），即 `fixtures/assembly.sql`。
///
/// ## 加载顺序
/// 1. 先 `load_part_fixture(pool)` —— 复用 part 域基线（customer / process /
///    shelf / user / role / 映射），不创建 `PartFixture` 字段，只为基线行；
/// 2. 再 `sqlx::raw_sql(assembly.sql)` —— 加载 assembly 自有 1 行
///    `t_serial_counter(prefix='F', counter=0)`。
///
/// ## 复用 vs 重复
/// 选择「复用 part fixture」而非「把 part 域基线 SQL 复制到 assembly.sql」，
/// 避免 part 域基线（13 行）散落到多个 SQL 文件里造成更新不同步。part fixture
/// 已经是「不可变共享基线」，assembly 直接调用即可。
#[allow(dead_code)]
pub async fn load_assembly_fixture(pool: &PgPool) -> AssemblyFixture {
    crate::fixture::load_part_fixture(pool).await;
    let sql = include_str!("../../fixtures/assembly.sql");
    sqlx::raw_sql(sql)
        .execute(pool)
        .await
        .expect("load_assembly_fixture: insert fixture rows");
    AssemblyFixture::default()
}
//! 集成测试 fixture（按域拆分）：加载 `fixtures/assembly.sql` 提供强类型句柄
//!
//! 2026-09-24 PR13 Phase H 新增：assembly 域预制 fixture。
//!
//! 与 [`production`](super::production) / [`delivery`](super::delivery) /
//! [`part`](super::part) 子模块同结构：
//! 1. SQL 落到 `test-support/fixtures/assembly.sql`，常量 ID 走
//!    9_000_000_000_000_000_070+ 区段（process_chain 1-9 / part 10-49 /
//!    delivery 50-52 / production 60-69 占下层）；
//! 2. 本文件定义 `AssemblyFixture` struct + 常量 ID const
//!    + `load_assembly_fixture(pool)` 函数；
//! 3. 测试 binary `use hsh_erp_test_support::fixture::*;` 直接拿到句柄。
//!
//! ## 复用 part 域基线
//! assembly 测试主要走 service 层（不开 HTTP、不经 JWT / Redis），构造
//! `CurrentUser { roles: vec![Role::Manager], .. }` 直调 `AssemblyService::*`。
//! fixture 主要提供「canonical L1+L2 配对」+ 「canonical t_serial_counter('F', 0)」，
//! 让走 'F' prefix 的测试可省去 `insert_l1_customer` / `insert_l2_customer` /
//! `insert_serial_counter` 三连样板（18/20 测试命中）。少数走 'X' prefix 的测试
//! （如 list_with_filters_and_l1_expansion）仍保留本地 helper 按需自建。
//!
//! ## 当前域
//! - `assembly`：1 L1 + 1 L2 + 1 serial_counter（prefix='F'）。不预置 t_assembly /
//!   t_part / t_part_batch：各 sub-file 按需通过 `AssemblyService::create_assembly`
//!   服务层直接调用，状态 / drawing_no / customer_id / quantity / PDF 等由测试
//!   现场控制；状态机不允许从 IN_PROCESS / COMPLETED 等回退 PENDING，预置会污染
//!   「期望空库」断言。

use sqlx::PgPool;

/// `fixtures/assembly.sql` 加载产物：常量 ID 句柄供测试函数直接使用。
///
/// 字段名采用「原 tests/assembly/ 内曾出现过的字段名」最小集，避免与 fixture
/// 字段名重构产生 churn；常量 ID 与 SQL INSERT 字面值逐字对应。
///
/// **不预置 t_assembly / t_part / t_part_batch**：fixture 只放「不可变共享」
/// 基线（canonical L1+L2 customer + canonical 'F' serial_counter）。assembly 域
/// 测试需要按需造不同 drawing_no / customer_id / 带不带 PDF 等组合，状态机不
/// 允许从 IN_PROCESS / COMPLETED 等回退 PENDING；预置会污染 list / count 等
/// 「期望空库」断言。各 sub-file 按需走 `AssemblyService::create_assembly`
/// 服务层创建。
#[allow(dead_code)]
pub struct AssemblyFixture {
    /// 复用 PartFixture：L1 客户 id（带 serial_prefix='P'，assembly 测试如需
    /// 走 part 域基线 user 登录时使用）
    pub part_customer_l1_id: i64,
    /// 复用 PartFixture：MANAGER 用户 id
    pub part_manager_user_id: i64,
    /// 复用 PartFixture：MANAGER 用户名（fixture 内含 MANAGER role，用于登录）
    pub part_manager_username: String,
    /// assembly 自有：canonical L1 客户 id（带 serial_prefix='F'）
    pub assembly_l1_id: i64,
    /// assembly 自有：canonical L2 客户 id（挂在 L1 下，prefix=NULL，可直接用作
    /// assembly.customer_id）
    pub assembly_l2_id: i64,
}

impl AssemblyFixture {
    /// fixture 内用户共用的明文密码（bcrypt 哈希嵌入 part.sql）。
    /// 改此处必须同步更新 fixtures/part.sql 的 password_hash 字面值。
    pub const PASSWORD: &'static str = "changeme";

    pub const CUSTOMER_L1_ID: i64 = 9_000_000_000_000_000_070;
    pub const CUSTOMER_L2_ID: i64 = 9_000_000_000_000_000_071;

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
            assembly_l1_id: AssemblyFixture::CUSTOMER_L1_ID,
            assembly_l2_id: AssemblyFixture::CUSTOMER_L2_ID,
        }
    }
}

/// 加载 assembly fixture。
///
/// SQL 走 `include_str!` 编译期嵌入本 crate，路径相对本 crate
/// `CARGO_MANIFEST_DIR`（= `test-support/`），即 `fixtures/assembly.sql`。
///
/// SQL 内 INSERT 全部走常量 ID（不依赖运行时雪花 ID），跨测试并行 / 跨进程
/// 重跑都不会撞 ID。
///
/// ## 加载顺序
/// 1. 先 `load_part_fixture(pool)` —— 复用 part 域基线（customer / process /
///    shelf / user / role / 映射），不创建 `PartFixture` 字段，只为基线行；
/// 2. 再 `sqlx::raw_sql(assembly.sql)` —— 加载 assembly 自有 3 行
///    （1 canonical L1 + 1 canonical L2 + 1 canonical t_serial_counter('F', 0)）。
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
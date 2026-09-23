//! 集成测试 fixture（按域拆分）：加载 `fixtures/delivery.sql` 提供强类型句柄
//!
//! 2026-09-23 PR13 Phase G 新增：delivery 域预制 fixture。
//!
//! 与 [`part`](super::part) 子模块同结构：
//! 1. SQL 落到 `test-support/fixtures/delivery.sql`，常量 ID 走
//!    9_000_000_000_000_000_050+ 区段（part 占 10-49，process_chain 占 1-9）；
//! 2. 本文件定义 `DeliveryFixture` struct + 常量 ID const
//!    + `load_delivery_fixture(pool)` 函数；
//! 3. 测试 binary `use hsh_erp_test_support::fixture::*;` 直接拿到句柄。
//!
//! ## 与 [`fixtures`](super::fixtures) 模块的分工
//! - `fixtures`：动态 helper（`insert_user_with_password` / `add_role` 等），
//!   每个测试即时造 1～N 行；适合"参数化差异"场景（不同 username / 不同 role）。
//! - `fixture`（本模块）：预制 SQL 静态行集合，一次 INSERT 3 行；适合"批量差异
//!   跨域共享"场景（delivery 测试同时需要 part 域基线 customer / user / role）。
//!
//! ## 复用 part 域基线
//! delivery 测试需要 customer / process / shelf / user / role / 映射等 part
//! 域已经预制的「不可变共享基线」行。`load_delivery_fixture` 内部先调
//! [`load_part_fixture`](super::part::load_part_fixture) 复用 part 域基线
//! （ID 段 10-49 在 `part.sql` 内），再 raw_sql delivery 自有行（ID 段 50+）。
//! 这样既避免 SQL 重复（part 域基线 13 行已够稳定，不重复 INSERT），也保证
//! 测试能用 fx_part_manager 用户登录拿 token。
//!
//! ## 当前域
//! - `delivery`：1 装配件（FX-ASM-001）+ 1 送货分组（FX-Group-1）+ 1 分组成员
//!   （L2 = CUSTOMER_L2_ID），全部依赖 part 域基线的 CUSTOMER_L1_ID=10。
//!   **不**预置 t_delivery_note —— 多数测试用「customer_id 限定 + status
//!   过滤」查单，预置 DRAFT 草稿会让 `total` 计数包含本行，破坏
//!   `list_with_filters_status_and_pagination` 等「期望仅 N 条」断言。

use sqlx::PgPool;

/// `fixtures/delivery.sql` 加载产物：常量 ID 句柄供测试函数直接使用。
///
/// 字段名采用「原 tests/delivery/ 内曾出现过的字段名」最小集，避免与 fixture
/// 字段名重构产生 churn；常量 ID 与 SQL INSERT 字面值逐字对应。
///
/// **不预置 part / batch / note / scan_code 等**：fixture 只放「不可变共享」
/// 基线（assembly / group / group_member）。多数 delivery 域测试需要特定
/// part.status / batch.status / part.serial_no（如扫码测试需要可控 serial_no
/// 与不同 batch 状态），状态机不允许从这些状态回退；预置 PENDING/IN_PROCESS
/// 行会让「期望空库」测试失败。各 sub-file 按需用 sqlx::query 直插 part /
/// batch，或走 POST /delivery-notes 建草稿。
#[allow(dead_code)]
pub struct DeliveryFixture {
    /// 复用 PartFixture：L1 客户 id（带 serial_prefix='P'，delivery 域送货单 /
    /// 分组 / 装配件都挂在它下）
    pub part_customer_l1_id: i64,
    /// 复用 PartFixture：MANAGER 用户 id
    pub part_manager_user_id: i64,
    /// 复用 PartFixture：MANAGER 用户名（fixture 内含 MANAGER role，用于登录）
    pub part_manager_username: String,
    /// delivery 自有：t_assembly.id
    pub assembly_id: i64,
    /// delivery 自有：t_delivery_group.id
    pub delivery_group_id: i64,
    /// delivery 自有：t_delivery_group_member.id
    pub delivery_group_member_id: i64,
}

impl DeliveryFixture {
    /// fixture 内用户共用的明文密码（bcrypt 哈希嵌入 part.sql）。
    /// 改此处必须同步更新 fixtures/part.sql 的 password_hash 字面值。
    pub const PASSWORD: &'static str = "changeme";

    pub const ASSEMBLY_ID: i64 = 9_000_000_000_000_000_050;
    pub const DELIVERY_GROUP_ID: i64 = 9_000_000_000_000_000_051;
    pub const DELIVERY_GROUP_MEMBER_ID: i64 = 9_000_000_000_000_000_052;
}

impl Default for DeliveryFixture {
    fn default() -> Self {
        use super::part::PartFixture;
        Self {
            part_customer_l1_id: PartFixture::CUSTOMER_L1_ID,
            part_manager_user_id: PartFixture::MANAGER_USER_ID,
            part_manager_username: PartFixture::MANAGER_USERNAME.to_string(),
            assembly_id: DeliveryFixture::ASSEMBLY_ID,
            delivery_group_id: DeliveryFixture::DELIVERY_GROUP_ID,
            delivery_group_member_id: DeliveryFixture::DELIVERY_GROUP_MEMBER_ID,
        }
    }
}

/// 加载 delivery fixture。
///
/// SQL 走 `include_str!` 编译期嵌入本 crate，路径相对本 crate
/// `CARGO_MANIFEST_DIR`（= `test-support/`），即 `fixtures/delivery.sql`。
///
/// SQL 内 INSERT 全部走常量 ID（不依赖运行时雪花 ID），跨测试并行 / 跨进程
/// 重跑都不会撞 ID。
///
/// ## 加载顺序
/// 1. 先 `load_part_fixture(pool)` —— 复用 part 域基线（customer / process /
///    shelf / user / role / 映射），不创建 `PartFixture` 字段，只为基线行；
/// 2. 再 `sqlx::raw_sql(delivery.sql)` —— 加载 delivery 自有 3 行
///    （assembly / group / group_member；不预置 note）。
///
/// ## 复用 vs 重复
/// 选择「复用 part fixture」而非「把 part 域基线 SQL 复制到 delivery.sql」，
/// 避免 part 域基线（13 行）散落到多个 SQL 文件里造成更新不同步。part fixture
/// 已经是「不可变共享基线」，delivery 直接调用即可。
#[allow(dead_code)]
pub async fn load_delivery_fixture(pool: &PgPool) -> DeliveryFixture {
    crate::fixture::load_part_fixture(pool).await;
    let sql = include_str!("../../fixtures/delivery.sql");
    sqlx::raw_sql(sql)
        .execute(pool)
        .await
        .expect("load_delivery_fixture: insert fixture rows");
    DeliveryFixture::default()
}

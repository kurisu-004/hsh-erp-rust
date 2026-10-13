//! 集成测试 fixture：客户树**任意深度**（2026-10-17 新增）
//!
//! 客户层级由固定两层放开为任意深度后，需要一组**深度 ≥3**的树供「送货分组成员、
//! 送货入单叶子闸门、`l1_of` 上溯根、`customer_path` 拼接」这些用例复用。
//!
//! ## 为什么单独一个 fixture（不并进 `part.sql` / `customer.sql`）
//! 既有 fixture 的 id / name 是大量集成测试的公共基线（`PartFixture::CUSTOMER_L1_ID`
//! = 10 / `CUSTOMER_L2_ID` = 11 被 10+ 个 sub-file 引用）。往既有 SQL 里加节点会让
//! ①「列表返回 N 条」类断言凭空多 1 行；②「整棵树」类断言的根数变化。本模块用
//! **独立 id 段 60-65**（与 part 10-49 / delivery 50-52 物理不相交）+ 独立加载函数，
//! 对既有 fixture **零影响** —— 只有显式调 [`load_customer_tree_fixture`] 的用例才会
//! 看到这些行。
//!
//! ## 树形
//!
//! ```text
//! FX 深层集团(L1, serial_prefix='Q')            tree_l1_id
//! └─ FX 深层事业群(L2)                          tree_l2_id      ← 中间层（非叶子）
//!    ├─ FX 深层事业部(L3)                       tree_l3_a_id    ← 中间层（非叶子）
//!    │  ├─ FX 深层车间(L4)                      tree_l4_a_id    ← 叶子
//!    │  └─ FX 深层班组(L4)                      tree_l4_b_id    ← 叶子（兄弟）
//!    └─ FX 深层分部(L3)                         tree_l3_b_id    ← 叶子（兄弟）
//! ```
//!
//! 对应的 `customer_path`（第 2 层到叶子，**不含**根）：
//! `FX 深层集团` / `FX 深层事业群` / `FX 深层事业群 / FX 深层事业部` /
//! `FX 深层事业群 / FX 深层事业部 / FX 深层车间`。

use sqlx::PgPool;

/// `fixtures/customer_tree.sql` 加载产物：常量 ID 句柄。
#[allow(dead_code)]
pub struct CustomerTreeFixture {
    /// 根（L1 集团，`serial_prefix = 'Q'`）
    pub tree_l1_id: i64,
    /// 第 2 层（**中间层**，有子节点 ⇒ 非叶子）
    pub tree_l2_id: i64,
    /// 第 3 层 A（**中间层**，有子节点 ⇒ 非叶子）
    pub tree_l3_a_id: i64,
    /// 第 4 层 A（叶子）
    pub tree_l4_a_id: i64,
    /// 第 4 层 B（叶子；`tree_l4_a_id` 的兄弟）
    pub tree_l4_b_id: i64,
    /// 第 3 层 B（叶子；`tree_l3_a_id` 的兄弟）
    pub tree_l3_b_id: i64,
}

impl CustomerTreeFixture {
    pub const TREE_L1_ID: i64 = 9_000_000_000_000_000_060;
    pub const TREE_L2_ID: i64 = 9_000_000_000_000_000_061;
    pub const TREE_L3_A_ID: i64 = 9_000_000_000_000_000_062;
    pub const TREE_L4_A_ID: i64 = 9_000_000_000_000_000_063;
    pub const TREE_L4_B_ID: i64 = 9_000_000_000_000_000_064;
    pub const TREE_L3_B_ID: i64 = 9_000_000_000_000_000_065;

    /// 根的 `serial_prefix`（建单派号用；`t_serial_counter` 里已播同名一行）
    pub const SERIAL_PREFIX: &'static str = "Q";
}

impl Default for CustomerTreeFixture {
    fn default() -> Self {
        Self {
            tree_l1_id: CustomerTreeFixture::TREE_L1_ID,
            tree_l2_id: CustomerTreeFixture::TREE_L2_ID,
            tree_l3_a_id: CustomerTreeFixture::TREE_L3_A_ID,
            tree_l4_a_id: CustomerTreeFixture::TREE_L4_A_ID,
            tree_l4_b_id: CustomerTreeFixture::TREE_L4_B_ID,
            tree_l3_b_id: CustomerTreeFixture::TREE_L3_B_ID,
        }
    }
}

/// 加载客户深树 fixture（6 个客户 + 1 行序列号计数器）。
///
/// ⚠️ **不**调 `load_part_fixture` / `load_iam_fixture`：本 fixture 只提供客户节点，
/// 需要 MANAGER 账号的用例自己叠加对应 fixture，避免 fixture 互相污染。
#[allow(dead_code)]
pub async fn load_customer_tree_fixture(pool: &PgPool) -> CustomerTreeFixture {
    let sql = include_str!("../../fixtures/customer_tree.sql");
    sqlx::raw_sql(sql)
        .execute(pool)
        .await
        .expect("load_customer_tree_fixture: insert fixture rows");
    CustomerTreeFixture::default()
}

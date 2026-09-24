//! 集成测试 fixture（按域拆分）：加载 `fixtures/shelf.sql` 提供强类型句柄
//!
//! 2026-09-24 PR13 Phase H 新增：shelf 域预制 fixture。
//!
//! 与 [`production`](super::production) / [`assembly`](super::assembly) /
//! [`delivery`](super::delivery) / [`part`](super::part) 子模块同结构：
//! 1. SQL 落到 `test-support/fixtures/shelf.sql`，本域自身仅 3 行
//!    （1 INHOUSE 工序 + 1 INSPECTION 货架 + 1 t_shelf_process 映射），
//!    ID 段 80-82（process_chain 1-9 / part 10-49 / delivery 50-52 /
//!    production 60-69 / assembly 70+ 占位）；
//! 2. 本文件定义 `ShelfFixture` struct + 常量
//!    + `load_shelf_fixture(pool)` 函数；
//! 3. 测试 binary `use hsh_erp_test_support::fixture::*;` 直接拿到句柄。
//!
//! ## 复用 part 域基线
//! shelf 域测试需要 MANAGER 用户（fx_part_manager）登录（POST /shelves 写路径
//! 要求 M-only，按设计 §6.1 用 M 即可）。fixture 不创建新用户，直接复用 part
//! 域基线 MANAGER（密码 "changeme"，bcrypt cost=12 哈希已在 part.sql 内预
//! 生成）。复用而非复制 part 域基线（13 行）避免散落到多个 SQL 文件造成更新
//! 不同步。
//!
//! ## 为什么 fixture 预置 1 货架 + 1 工序 + 1 映射
//! shelf 域 API（POST /shelves、POST /shelves/{id}/processes 等）允许现场
//! 创建，但预置 1 货架 + 1 映射给「已有映射时替换 / list 时已存在 1 行」等场景
//! 提供 fixture 起点；测试仍按需用 POST /shelves 创建更多货架。
//!
//! ## 当前域
//! - `shelf`：1 INHOUSE 工序（FX-SHP）加 1 INSPECTION 货架（FX-SH-NEW1）加
//!   1 t_shelf_process 映射（FX-SH-NEW1 → FX-SHP）。不预置 t_customer /
//!   t_part / t_part_batch / t_worker：deactivate / api 测试走 service 层 /
//!   直插 t_part_batch 路径，需要按需造不同 L1 prefix / 不同 customer_name
//!   / 不同 status / 不同 worker_badge，预置会污染「期望空库」断言；
//!   t_customer 上 `uq_t_customer_root_prefix` 唯一索引也要求测试自建 L1。

use sqlx::PgPool;

/// `fixtures/shelf.sql` 加载产物：常量句柄供测试函数直接使用。
///
/// 字段名采用「原 tests/shelf/ 内曾出现过的字段名」最小集，避免与 fixture
/// 字段名重构产生 churn；常量与 SQL INSERT 字面值逐字对应。
///
/// **不预置 t_customer / t_part / t_part_batch / t_worker**：fixture 只放
/// 「不可变共享」基线（process / shelf / 映射）+ 复用 part 域 MANAGER 用户。
/// 各 sub-file 按需走本地 helper：
/// - `api.rs::insert_part_held_by_shelf`：直插 t_part + t_part_batch
/// - `api.rs::insert_test_process`：直插 INHOUSE t_process
/// - `deactivate.rs::insert_l2_customer / insert_part / insert_batch /
///   insert_worker_min`：直插各自域行（不同 prefix / 不同 status / 不同
///   worker_badge）
#[allow(dead_code)]
pub struct ShelfFixture {
    /// 复用 PartFixture：L1 客户 id（带 serial_prefix='P'，仅 shelf 域测试如
    /// 需走 part 域基线 user 登录时使用）
    pub part_customer_l1_id: i64,
    /// 复用 PartFixture：L2 客户 id（挂在 L1 下，prefix=NULL）
    pub part_customer_l2_id: i64,
    /// 复用 PartFixture：INHOUSE 工序 FX-PROC-A id
    pub part_process_id: i64,
    /// 复用 PartFixture：INSPECTION 货架 id（SHELF_ACCOUNT scope target）
    pub part_inspection_shelf_id: i64,
    /// 复用 PartFixture：PRODUCTION 货架 id
    pub part_production_shelf_id: i64,
    /// 复用 PartFixture：MANAGER 用户 id
    pub part_manager_user_id: i64,
    /// 复用 PartFixture：MANAGER 用户名（fixture 内含 MANAGER role，用于登录）
    pub part_manager_username: String,
    /// shelf 自有：INHOUSE 工序 FX-SHP id（shelf 映射目标 process）
    pub shelf_process_id: i64,
    /// shelf 自有：INSPECTION 货架 FX-SH-NEW1 id
    pub shelf_id: i64,
    /// shelf 自有：FX-SH-NEW1 ↔ FX-SHP 映射 id
    pub shelf_process_mapping_id: i64,
}

impl ShelfFixture {
    /// fixture 内用户共用的明文密码（bcrypt 哈希嵌入 part.sql）。
    /// 改此处必须同步更新 fixtures/part.sql 的 password_hash 字面值。
    pub const PASSWORD: &'static str = "changeme";

    pub const SHELF_PROCESS_ID: i64 = 9_000_000_000_000_000_080;
    pub const SHELF_ID: i64 = 9_000_000_000_000_000_081;
    pub const SHELF_PROCESS_MAPPING_ID: i64 = 9_000_000_000_000_000_082;
}

impl Default for ShelfFixture {
    fn default() -> Self {
        use super::part::PartFixture;
        Self {
            part_customer_l1_id: PartFixture::CUSTOMER_L1_ID,
            part_customer_l2_id: PartFixture::CUSTOMER_L2_ID,
            part_process_id: PartFixture::PROCESS_ID,
            part_inspection_shelf_id: PartFixture::INSPECTION_SHELF_ID,
            part_production_shelf_id: PartFixture::PRODUCTION_SHELF_ID,
            part_manager_user_id: PartFixture::MANAGER_USER_ID,
            part_manager_username: PartFixture::MANAGER_USERNAME.to_string(),
            shelf_process_id: ShelfFixture::SHELF_PROCESS_ID,
            shelf_id: ShelfFixture::SHELF_ID,
            shelf_process_mapping_id: ShelfFixture::SHELF_PROCESS_MAPPING_ID,
        }
    }
}

/// 加载 shelf fixture。
///
/// SQL 走 `include_str!` 编译期嵌入本 crate，路径相对本 crate
/// `CARGO_MANIFEST_DIR`（= `test-support/`），即 `fixtures/shelf.sql`。
///
/// ## 加载顺序
/// 1. 先 `load_part_fixture(pool)` —— 复用 part 域基线（customer / process /
///    shelf / user / role / 映射），不创建 `PartFixture` 字段，只为基线行；
/// 2. 再 `sqlx::raw_sql(shelf.sql)` —— 加载 shelf 自有 3 行
///    （1 INHOUSE 工序 FX-SHP + 1 INSPECTION 货架 FX-SH-NEW1 + 1 映射）。
///
/// ## 复用 vs 重复
/// 选择「复用 part fixture」而非「把 part 域基线 SQL 复制到 shelf.sql」，
/// 避免 part 域基线（13 行）散落到多个 SQL 文件里造成更新不同步。part fixture
/// 已经是「不可变共享基线」，shelf 直接调用即可。
#[allow(dead_code)]
pub async fn load_shelf_fixture(pool: &PgPool) -> ShelfFixture {
    crate::fixture::load_part_fixture(pool).await;
    let sql = include_str!("../../fixtures/shelf.sql");
    sqlx::raw_sql(sql)
        .execute(pool)
        .await
        .expect("load_shelf_fixture: insert fixture rows");
    ShelfFixture::default()
}
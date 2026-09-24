//! 集成测试 fixture（PR13 Phase I 引入，2026-09-24）：customer 域预制 fixture
//!
//! `tests/customer_api.rs`（242 行）单文件使用。customer 域端到端集成测试
//! 覆盖 create L1/L2 + soft-delete in-use 守卫 + serial_prefix 唯一约束。
//!
//! ## 字段按域需求聚合
//! - `t_user` ×1 —— fx_customer_manager（密码 "changeme"，MANAGER role）
//! - `t_user_role` ×1 —— baseline MANAGER role
//!
//! 不预置 `t_customer` / `t_part`：测试现场创建 L1/L2 customer（避免 fixture
//! 占用 serial_prefix 与测试现场字面冲突）+ 软删 in-use 时插 part。

use sqlx::PgPool;

/// `fixtures/customer.sql` 加载产物：常量 ID 句柄供测试函数直接使用。
#[allow(dead_code)]
pub struct CustomerFixture {
    /// baseline MANAGER user id（fx_customer_manager，对应 t_user_id=140）
    pub manager_user_id: i64,
    /// baseline MANAGER role id（对应 t_user_role_id=141）
    pub manager_role_id: i64,
}

impl CustomerFixture {
    /// fixture 内 baseline MANAGER 用户的明文密码（bcrypt 哈希嵌入 customer.sql）。
    /// 改此处必须同步更新 fixtures/customer.sql 的 password_hash 字面值。
    pub const PASSWORD: &'static str = "changeme";

    pub const MANAGER_USER_ID: i64 = 9_000_000_000_000_000_140;
    pub const MANAGER_ROLE_ID: i64 = 9_000_000_000_000_000_141;

    /// fixture 内 baseline MANAGER 用户的 username（与 fixtures/customer.sql 字面对齐）
    pub const MANAGER_USERNAME: &'static str = "fx_customer_manager";
}

impl Default for CustomerFixture {
    fn default() -> Self {
        Self {
            manager_user_id: CustomerFixture::MANAGER_USER_ID,
            manager_role_id: CustomerFixture::MANAGER_ROLE_ID,
        }
    }
}

/// 加载 customer fixture。
///
/// SQL 走 `include_str!` 编译期嵌入本 crate，路径相对本 crate
/// `CARGO_MANIFEST_DIR`（= `test-support/`），即 `fixtures/customer.sql`。
///
/// SQL 内 INSERT 全部走常量 ID（不依赖运行时雪花 ID），跨测试并行 / 跨进程
/// 重跑都不会撞 ID。
///
/// ## 加载顺序
/// 1. 直接 `sqlx::raw_sql(customer.sql)` —— 加载 2 行（1 user + 1 role）。
/// 2. **不**调 [`load_iam_fixture`](super::iam::load_iam_fixture)——
///    customer 测试不需要 IAM 5 用户基线，避免 fixture 散落到多文件。
#[allow(dead_code)]
pub async fn load_customer_fixture(pool: &PgPool) -> CustomerFixture {
    let sql = include_str!("../../fixtures/customer.sql");
    sqlx::raw_sql(sql)
        .execute(pool)
        .await
        .expect("load_customer_fixture: insert fixture rows");
    CustomerFixture::default()
}
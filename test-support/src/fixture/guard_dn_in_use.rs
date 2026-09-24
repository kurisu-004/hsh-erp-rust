//! 集成测试 fixture（PR13 Phase I 引入，2026-09-24）：guard_dn_in_use 域预制 fixture
//!
//! `tests/guard_dn_in_use_api.rs`（255 行）单文件使用。覆盖 PR-2
//! `t_part.delivery_note_id` 列删除后的守卫回归点（part cancel + part
//! soft-delete + assembly soft-delete，3 个测试）。
//!
//! ## 字段按域需求聚合
//! - `t_user` ×1 —— fx_guard_dn_manager（密码 "changeme"，MANAGER role）
//! - `t_user_role` ×1 —— baseline MANAGER role
//!
//! 不预置 `t_part` / `t_customer` / `t_part_batch` / `t_assembly` /
//! `t_delivery_note`：本测试保留 `mod helpers;`（依赖 `tests/part/helpers.rs`
//! 的 `insert_l1` / `insert_l2` / `insert_part_with_status` / `insert_batch` /
//! `login_manager`），不在本 fixture 重复 —— helpers 走 snowflake 运行时 ID
//! （避免 uk_t_part_* 唯一约束撞车）。fixture 仅提供 baseline MANAGER user / role。

use sqlx::PgPool;

/// `fixtures/guard_dn_in_use.sql` 加载产物：常量 ID 句柄供测试函数直接使用。
#[allow(dead_code)]
pub struct GuardDnFixture {
    /// baseline MANAGER user id（fx_guard_dn_manager，对应 t_user_id=190）
    pub manager_user_id: i64,
    /// baseline MANAGER role id（对应 t_user_role_id=191）
    pub manager_role_id: i64,
}

impl GuardDnFixture {
    /// fixture 内 baseline MANAGER 用户的明文密码（bcrypt 哈希嵌入 guard_dn_in_use.sql）。
    /// 改此处必须同步更新 fixtures/guard_dn_in_use.sql 的 password_hash 字面值。
    pub const PASSWORD: &'static str = "changeme";

    pub const MANAGER_USER_ID: i64 = 9_000_000_000_000_000_190;
    pub const MANAGER_ROLE_ID: i64 = 9_000_000_000_000_000_191;

    /// fixture 内 baseline MANAGER 用户的 username（与 fixtures/guard_dn_in_use.sql 字面对齐）
    pub const MANAGER_USERNAME: &'static str = "fx_guard_dn_manager";
}

impl Default for GuardDnFixture {
    fn default() -> Self {
        Self {
            manager_user_id: GuardDnFixture::MANAGER_USER_ID,
            manager_role_id: GuardDnFixture::MANAGER_ROLE_ID,
        }
    }
}

/// 加载 guard_dn_in_use fixture。
///
/// SQL 走 `include_str!` 编译期嵌入本 crate，路径相对本 crate
/// `CARGO_MANIFEST_DIR`（= `test-support/`），即 `fixtures/guard_dn_in_use.sql`。
///
/// SQL 内 INSERT 全部走常量 ID（不依赖运行时雪花 ID），跨测试并行 / 跨进程
/// 重跑都不会撞 ID。
///
/// ## 加载顺序
/// 1. 直接 `sqlx::raw_sql(guard_dn_in_use.sql)` —— 加载 2 行（1 user + 1 role）。
/// 2. **不**调 [`load_part_fixture`](super::part::load_part_fixture)——
///    guard_dn_in_use 测试通过 helpers 自建 part / customer / batch，避免
///    fixture 与 helpers 数据重复。
#[allow(dead_code)]
pub async fn load_guard_dn_in_use_fixture(pool: &PgPool) -> GuardDnFixture {
    let sql = include_str!("../../fixtures/guard_dn_in_use.sql");
    sqlx::raw_sql(sql)
        .execute(pool)
        .await
        .expect("load_guard_dn_in_use_fixture: insert fixture rows");
    GuardDnFixture::default()
}
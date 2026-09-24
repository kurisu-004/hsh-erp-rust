//! 集成测试 fixture（PR13 Phase I 引入，2026-09-24）：user_repo 域预制 fixture
//!
//! 4 个 sub-file（main / basic / role / password）共用一份 user_repo.sql。
//! user_repo 已有 local seed_* helper（seed_user / seed_role / seed_menu /
//! link_role_menu / seed_shelf），这些 helper 不走 fixtures.rs，本任务保留
//! 本地。本任务核心是**统一 bootstrap 模式** + 提供 fixture baseline 减少
//! 重复 INSERT。
//!
//! 与 [`iam`](super::iam) 子模块同结构：
//! 1. SQL 落到 `test-support/fixtures/user_repo.sql`，常量 ID 走
//!    9_000_000_000_000_000_120+ 区段（process_chain 1-9 / part 10-49 /
//!    delivery 50-52 / production 60-69 / assembly 70+ / shelf 80+ /
//!    statistics 90+ / outsource 100-104 / iam 110-118，物理不相交）；
//! 2. 本文件定义 `UserRepoFixture` struct + 常量 ID const + `load_user_repo_fixture(pool)` 函数；
//! 3. 测试 binary `use hsh_erp_test_support::fixture::*;` 直接拿到句柄。
//!
//! ## 独立加载（不复用 iam / part 域基线）
//! user_repo 测试不需要 customer / process / work_type / part fixture；
//! `load_user_repo_fixture` 内部不调 [`load_iam_fixture`](super::iam::load_iam_fixture)
//! 或 [`load_part_fixture`](super::part::load_part_fixture)，直接
//! `raw_sql(user_repo.sql)` 加载 3 行（1 user + 1 role + 1 menu）。
//!
//! ## 字段按域需求聚合
//! - `t_user` ×1 —— fx_user_repo_baseline（密码 "changeme"，active）
//! - `t_user_role` ×1 —— MANAGER 角色（属于 baseline user，无 scope）
//! - `t_menu` ×1 —— baseline menu（不挂 t_role_menu）
//!
//! 不预置 `t_shelf` / `t_role_menu`：role.rs 内 ShelfRepo 测试用 seed_shelf
//! 自建不同 code（避免 uk_t_shelf_code 撞），MenuRepo 测试现场 link_role_menu
//! 自建关系（避免预置污染「无角色菜单 list」断言）。
//!
//! ## 当前域
//! - `user_repo`：1 用户 + 1 角色 + 1 菜单（共 3 行静态 INSERT）

use sqlx::PgPool;

/// `fixtures/user_repo.sql` 加载产物：常量 ID 句柄供测试函数直接使用。
///
/// 字段名采用「原 tests/user_repo/ 内曾出现过的字段名」最小集，避免与 fixture
/// 字段名重构产生 churn；常量 ID 与 SQL INSERT 字面值逐字对应。
#[allow(dead_code)]
pub struct UserRepoFixture {
    /// baseline user id（fx_user_repo_baseline，对应 t_user_id=120）
    pub baseline_user_id: i64,
    /// baseline username（fx_user_repo_baseline）
    pub baseline_username: String,
    /// baseline MANAGER role id（属于 baseline user，无 scope，对应 t_user_role.id=121）
    pub baseline_role_id: i64,
    /// baseline menu id（对应 t_menu.id=122；不挂 t_role_menu）
    pub baseline_menu_id: i64,
}

impl UserRepoFixture {
    /// fixture 内 baseline 用户的明文密码（bcrypt 哈希嵌入 user_repo.sql）。
    /// 改此处必须同步更新 fixtures/user_repo.sql 的 password_hash 字面值。
    pub const PASSWORD: &'static str = "changeme";

    pub const BASELINE_USER_ID: i64 = 9_000_000_000_000_000_120;
    pub const BASELINE_ROLE_ID: i64 = 9_000_000_000_000_000_121;
    pub const BASELINE_MENU_ID: i64 = 9_000_000_000_000_000_122;

    /// fixture 内 baseline 用户的 username（与 fixtures/user_repo.sql INSERT 字面对齐）
    pub const BASELINE_USERNAME: &'static str = "fx_user_repo_baseline";
}

impl Default for UserRepoFixture {
    fn default() -> Self {
        Self {
            baseline_user_id: UserRepoFixture::BASELINE_USER_ID,
            baseline_username: UserRepoFixture::BASELINE_USERNAME.to_string(),
            baseline_role_id: UserRepoFixture::BASELINE_ROLE_ID,
            baseline_menu_id: UserRepoFixture::BASELINE_MENU_ID,
        }
    }
}

/// 加载 user_repo fixture。
///
/// SQL 走 `include_str!` 编译期嵌入本 crate，路径相对本 crate
/// `CARGO_MANIFEST_DIR`（= `test-support/`），即 `fixtures/user_repo.sql`。
///
/// SQL 内 INSERT 全部走常量 ID（不依赖运行时雪花 ID），跨测试并行 / 跨进程
/// 重跑都不会撞 ID。
///
/// ## 加载顺序
/// 1. 直接 `sqlx::raw_sql(user_repo.sql)` —— 加载 3 行（1 user + 1 role + 1 menu）。
/// 2. **不**调 [`load_iam_fixture`](super::iam::load_iam_fixture) 或
///    [`load_part_fixture`](super::part::load_part_fixture) —— user_repo 测试
///    不需要 customer / process / work_type / part fixture。
///
/// ## 复用 vs 重复
/// 选择「独立 fixture」而非「在 user_repo.sql 内复制 iam 域基线 SQL」—— user_repo
/// 测试不依赖 customer / shelf_process / work_type_process，避免把 iam 域基线
/// （9 行）散落到多个 SQL 文件造成更新不同步。
///
/// ## 本地 helper 保留（不走 fixtures.rs）
/// `tests/user_repo/{basic,role,password}.rs` 内 seed_user / seed_role /
/// seed_menu / link_role_menu / seed_shelf 是 user_repo 域独享 helper，本来
/// 就不调 `test-support::fixtures`，本 fixture 只提供 baseline ID 起点；多数
/// 测试仍走本地 helper 创建专属测试数据（特定 username / 多用户 / 多角色组合）。
#[allow(dead_code)]
pub async fn load_user_repo_fixture(pool: &PgPool) -> UserRepoFixture {
    let sql = include_str!("../../fixtures/user_repo.sql");
    sqlx::raw_sql(sql)
        .execute(pool)
        .await
        .expect("load_user_repo_fixture: insert fixture rows");
    UserRepoFixture::default()
}
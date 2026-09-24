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
//! ## 2026-09-24 PR13 Phase C.Final：移除 baseline user / role
//! PR13 Phase C.2 引入的 `fx_user_repo_baseline`（t_user_id=120）+ baseline
//! MANAGER role（t_user_role_id=120）破坏 4 个 user_repo 测试（count /
//! list_with_filters_* 系列）。C.Final 移除这两行 INSERT，fixture 仅保留
//! 1 个 baseline menu（id=121）。多数测试用本地 seed_user / seed_role
//! 创建专属测试数据。
//!
//! ## 字段按域需求聚合
//! - `t_menu` ×1 —— baseline menu（不挂 t_role_menu）
//!
//! 不预置 `t_user` / `t_user_role` / `t_shelf` / `t_role_menu`：baseline
//! user/role 删除后 fixture 仅提供 menu baseline；其余由各 sub-file 用
//! seed_user / seed_role / seed_shelf / link_role_menu 自建。
//!
//! ## 当前域
//! - `user_repo`：1 菜单（共 1 行静态 INSERT）

use sqlx::PgPool;

/// `fixtures/user_repo.sql` 加载产物：常量 ID 句柄供测试函数直接使用。
///
/// 字段名采用「原 tests/user_repo/ 内曾出现过的字段名」最小集，避免与 fixture
/// 字段名重构产生 churn；常量 ID 与 SQL INSERT 字面值逐字对应。
#[allow(dead_code)]
pub struct UserRepoFixture {
    /// baseline menu id（对应 t_menu.id=121；不挂 t_role_menu）
    pub baseline_menu_id: i64,
}

impl UserRepoFixture {
    pub const BASELINE_MENU_ID: i64 = 9_000_000_000_000_000_121;
}

impl Default for UserRepoFixture {
    fn default() -> Self {
        Self {
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
/// 1. 直接 `sqlx::raw_sql(user_repo.sql)` —— 加载 1 行 baseline menu。
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
/// 就不调 `test-support::fixtures`，本 fixture 只提供 baseline menu；多数
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
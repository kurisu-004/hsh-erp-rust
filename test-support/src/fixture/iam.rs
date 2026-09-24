//! 集成测试 fixture（PR13 Phase I 引入，2026-09-24）：iam 域预制 fixture
//!
//! 3 个 sub-file（main / api / middleware）共用一份 iam.sql。iam/api.rs 是
//! `test-support::fixtures` 最后重度用户（`add_role` / `insert_user_with_password` /
//! `insert_inactive_user` / `insert_menu` / `add_role_menu` / `get_refresh_token_version` /
//! `clean_db` / `clean_redis` / `test_redis_pool`），本 fixture 替代其依赖。
//!
//! 与 [`outsource`](super::outsource) 子模块同结构：
//! 1. SQL 落到 `test-support/fixtures/iam.sql`，常量 ID 走
//!    9_000_000_000_000_000_110+ 区段（process_chain 1-9 / part 10-49 /
//!    delivery 50-52 / production 60-69 / assembly 70+ / shelf 80+ /
//!    statistics 90+ / outsource 100-104，物理不相交）；
//! 2. 本文件定义 `IamFixture` struct + 常量 ID const + `load_iam_fixture(pool)` 函数；
//! 3. 测试 binary `use hsh_erp_test_support::fixture::*;` 直接拿到句柄。
//!
//! ## 独立加载（不复用 part 域基线）
//! iam 测试不需要 customer / process / work_type / part fixture；
//! `load_iam_fixture` 内部不调 [`load_part_fixture`](super::part::load_part_fixture)，
//! 直接 `raw_sql(iam.sql)` 加载 9 行（5 users + 2 roles + 2 shelves）。
//!
//! ## 字段按域需求聚合
//! - `t_user` ×5 —— manager / clerk / lonely / target / inactive
//! - `t_user_role` ×2 —— MANAGER (manager) + CLERK (clerk)，均无 scope
//! - `t_shelf` ×2 —— FX-SH-A1 (PRODUCTION) + FX-SH-B1 (INSPECTION)
//!
//! 不预置 `t_menu` / `t_role_menu`：原 api.rs 的 `insert_menu` / `add_role_menu`
//! 仅在 `_unused_silencer` 压 unused 警告时被提，本fixture 不引入菜单占位。
//!
//! ## 当前域
//! - `iam`：5 用户 + 2 角色 + 2 货架（共 9 行静态 INSERT）

use sqlx::PgPool;

/// `fixtures/iam.sql` 加载产物：常量 ID 句柄供测试函数直接使用。
///
/// 字段名采用「原 tests/iam/ 内曾出现过的字段名」最小集，避免与 fixture
/// 字段名重构产生 churn；常量 ID 与 SQL INSERT 字面值逐字对应。
#[allow(dead_code)]
pub struct IamFixture {
    /// MANAGER 用户 id（fx_iam_manager，对应 t_user_id=110）
    pub manager_user_id: i64,
    /// MANAGER 用户名
    pub manager_username: String,
    /// CLERK 用户 id（fx_iam_clerk，对应 t_user_id=111）
    pub clerk_user_id: i64,
    /// CLERK 用户名
    pub clerk_username: String,
    /// 无角色用户 id（fx_iam_lonely，对应 t_user_id=112；用于"无 role 登录 → 403"测试）
    pub lonely_user_id: i64,
    /// 无角色用户名
    pub lonely_username: String,
    /// 目标用户 id（fx_iam_target，对应 t_user_id=113；用于"添加 SHELF_ACCOUNT role"测试）
    pub target_user_id: i64,
    /// 目标用户名
    pub target_username: String,
    /// 已停用用户 id（fx_iam_inactive，对应 t_user_id=114，is_active=false）
    pub inactive_user_id: i64,
    /// 已停用用户名
    pub inactive_username: String,
    /// MANAGER role id（fx_iam_manager 持有，对应 t_user_role.id=115）
    pub manager_role_id: i64,
    /// CLERK role id（fx_iam_clerk 持有，对应 t_user_role.id=116）
    pub clerk_role_id: i64,
    /// 货架 FX-SH-A1 id（PRODUCTION zone，对应 t_shelf.id=117）
    pub shelf_a_id: i64,
    /// 货架 FX-SH-B1 id（INSPECTION zone，对应 t_shelf.id=118）
    pub shelf_b_id: i64,
}

impl IamFixture {
    /// fixture 内 5 用户共用的明文密码（bcrypt 哈希嵌入 iam.sql）。
    /// 改此处必须同步更新 fixtures/iam.sql 的 5 个 password_hash 字面值。
    pub const PASSWORD: &'static str = "changeme";

    pub const MANAGER_USER_ID: i64 = 9_000_000_000_000_000_110;
    pub const CLERK_USER_ID: i64 = 9_000_000_000_000_000_111;
    pub const LONELY_USER_ID: i64 = 9_000_000_000_000_000_112;
    pub const TARGET_USER_ID: i64 = 9_000_000_000_000_000_113;
    pub const INACTIVE_USER_ID: i64 = 9_000_000_000_000_000_114;
    pub const MANAGER_ROLE_ID: i64 = 9_000_000_000_000_000_115;
    pub const CLERK_ROLE_ID: i64 = 9_000_000_000_000_000_116;
    pub const SHELF_A_ID: i64 = 9_000_000_000_000_000_117;
    pub const SHELF_B_ID: i64 = 9_000_000_000_000_000_118;

    /// fixture 内 MANAGER 用户的 username（与 fixtures/iam.sql INSERT 字面对齐）
    pub const MANAGER_USERNAME: &'static str = "fx_iam_manager";
    pub const CLERK_USERNAME: &'static str = "fx_iam_clerk";
    pub const LONELY_USERNAME: &'static str = "fx_iam_lonely";
    pub const TARGET_USERNAME: &'static str = "fx_iam_target";
    pub const INACTIVE_USERNAME: &'static str = "fx_iam_inactive";
}

impl Default for IamFixture {
    fn default() -> Self {
        Self {
            manager_user_id: IamFixture::MANAGER_USER_ID,
            manager_username: IamFixture::MANAGER_USERNAME.to_string(),
            clerk_user_id: IamFixture::CLERK_USER_ID,
            clerk_username: IamFixture::CLERK_USERNAME.to_string(),
            lonely_user_id: IamFixture::LONELY_USER_ID,
            lonely_username: IamFixture::LONELY_USERNAME.to_string(),
            target_user_id: IamFixture::TARGET_USER_ID,
            target_username: IamFixture::TARGET_USERNAME.to_string(),
            inactive_user_id: IamFixture::INACTIVE_USER_ID,
            inactive_username: IamFixture::INACTIVE_USERNAME.to_string(),
            manager_role_id: IamFixture::MANAGER_ROLE_ID,
            clerk_role_id: IamFixture::CLERK_ROLE_ID,
            shelf_a_id: IamFixture::SHELF_A_ID,
            shelf_b_id: IamFixture::SHELF_B_ID,
        }
    }
}

/// 加载 iam fixture。
///
/// SQL 走 `include_str!` 编译期嵌入本 crate，路径相对本 crate
/// `CARGO_MANIFEST_DIR`（= `test-support/`），即 `fixtures/iam.sql`。
///
/// SQL 内 INSERT 全部走常量 ID（不依赖运行时雪花 ID），跨测试并行 / 跨进程
/// 重跑都不会撞 ID。
///
/// ## 加载顺序
/// 1. 直接 `sqlx::raw_sql(iam.sql)` —— 加载 9 行（5 users + 2 roles + 2 shelves）。
/// 2. **不**调 [`load_part_fixture`](super::part::load_part_fixture)——
///    iam 测试不需要 customer / process / work_type / part fixture。
///
/// ## 复用 vs 重复
/// 选择「独立 fixture」而非「在 iam.sql 内复制 part 域基线 SQL」—— iam 测试
/// 完全不依赖 customer / process / shelf_process / work_type_process，避免把
/// part 域基线（10 行）散落到多个 SQL 文件造成更新不同步。
#[allow(dead_code)]
pub async fn load_iam_fixture(pool: &PgPool) -> IamFixture {
    let sql = include_str!("../../fixtures/iam.sql");
    sqlx::raw_sql(sql)
        .execute(pool)
        .await
        .expect("load_iam_fixture: insert fixture rows");
    IamFixture::default()
}
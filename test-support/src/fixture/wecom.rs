//! 集成测试 fixture（2026-09-29 新增）：wecom（企业微信小程序登录）域预制 fixture
//!
//! `tests/wecom_login.rs` + `tests/iam/wx_bind.rs` 使用。预置 `t_wx_identity`
//! 映射行，把企业微信 userid 绑到 [`IamFixture`](super::iam::IamFixture) 的 5 个
//! 常量用户上，覆盖 wx-login 的 200 / 20606 / 40101 三类结果 + 管理端点复用。
//!
//! ## 加载顺序（**必须**先 iam 后 wecom）
//! `t_wx_identity.user_id` 无物理外键（对齐 `migrations/README.md`），但逻辑上
//! 依赖 iam fixture 的用户行；先 iam 后 wecom 可保证任何时刻数据自洽。
//!
//! ## 与 [iam](super::iam) 的 ID 段划分
//! iam 占 110-118，本 fixture 占 120-124（物理不相交）。

use sqlx::PgPool;

/// `fixtures/wecom.sql` 加载产物：常量 ID 句柄供测试函数直接使用。
#[allow(dead_code)]
pub struct WecomFixture {
    /// MANAGER 绑定行 id（fx_wx_manager → fx_iam_manager）
    pub manager_bind_id: i64,
    /// CLERK 绑定行 id（fx_wx_clerk → fx_iam_clerk）
    pub clerk_bind_id: i64,
    /// 无角色用户绑定行 id（wx-login 期望 20606）
    pub lonely_bind_id: i64,
    /// 已停用用户绑定行 id（wx-login 期望 40101）
    pub inactive_bind_id: i64,
    /// 目标用户绑定行 id（管理端点「解绑后重绑」测试用）
    pub target_bind_id: i64,
}

impl WecomFixture {
    pub const MANAGER_BIND_ID: i64 = 9_000_000_000_000_000_120;
    pub const CLERK_BIND_ID: i64 = 9_000_000_000_000_000_121;
    pub const LONELY_BIND_ID: i64 = 9_000_000_000_000_000_122;
    pub const INACTIVE_BIND_ID: i64 = 9_000_000_000_000_000_123;
    pub const TARGET_BIND_ID: i64 = 9_000_000_000_000_000_124;

    /// fixture 内全部绑定的 `corp_id`（与 `fixtures/wecom.sql` 字面对齐）。
    ///
    /// 构造测试 state 时须把它传给 `test_state_with_wecom`，否则 wx-login 的
    /// corpid 比对会失败（返 40107）。
    pub const CORP_ID: &'static str = "ww-fixture-corp";

    /// 「corpid 与配置不符」场景用的对照企业 ID。
    pub const OTHER_CORP_ID: &'static str = "ww-other-corp";

    /// fixture 内 MANAGER 绑定行的 userid（wx-login 成功路径）
    pub const MANAGER_WX_USER_ID: &'static str = "fx_wx_manager";
    /// fixture 内 CLERK 绑定行的 userid
    pub const CLERK_WX_USER_ID: &'static str = "fx_wx_clerk";
    /// fixture 内无角色用户绑定行的 userid（wx-login → 20606）
    pub const LONELY_WX_USER_ID: &'static str = "fx_wx_lonely";
    /// fixture 内已停用用户绑定行的 userid（wx-login → 40101）
    pub const INACTIVE_WX_USER_ID: &'static str = "fx_wx_inactive";
    /// fixture 内目标用户绑定行的 userid（管理端点用）
    pub const TARGET_WX_USER_ID: &'static str = "fx_wx_target";
    /// 任何未出现在表里的 userid（wx-login → 40107）
    pub const UNBOUND_WX_USER_ID: &'static str = "fx_wx_unbound";
}

impl Default for WecomFixture {
    fn default() -> Self {
        Self {
            manager_bind_id: WecomFixture::MANAGER_BIND_ID,
            clerk_bind_id: WecomFixture::CLERK_BIND_ID,
            lonely_bind_id: WecomFixture::LONELY_BIND_ID,
            inactive_bind_id: WecomFixture::INACTIVE_BIND_ID,
            target_bind_id: WecomFixture::TARGET_BIND_ID,
        }
    }
}

/// 加载 wecom fixture。
///
/// SQL 走 `include_str!` 编译期嵌入本 crate，路径相对本 crate
/// `CARGO_MANIFEST_DIR`（= `test-support/`），即 `fixtures/wecom.sql`。
///
/// 前置：先 `load_iam_fixture(&pool)`（本 fixture 的 `user_id` 指向那 5 个用户）。
#[allow(dead_code)]
pub async fn load_wecom_fixture(pool: &PgPool) -> WecomFixture {
    let sql = include_str!("../../fixtures/wecom.sql");
    sqlx::raw_sql(sql)
        .execute(pool)
        .await
        .expect("load_wecom_fixture: insert fixture rows");
    WecomFixture::default()
}

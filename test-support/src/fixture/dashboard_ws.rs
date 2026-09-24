//! 集成测试 fixture（PR13 Phase I 引入，2026-09-24）：dashboard_ws 域预制 fixture
//!
//! `tests/dashboard_ws_api.rs`（413 行）单文件使用。覆盖：
//! - service 层 `DashboardService::build_snapshot_with_workers` JSON shape
//! - WsHub 事件 / snapshot 订阅通路
//! - 真实 socket E2E（WS 握手 / heartbeat / snapshot）
//!
//! ## 字段按域需求聚合
//! - `t_user` ×1 —— fx_dashboard_ws_user（密码 "changeme"，active；WS 真实 socket
//!   E2E 验签 token 用）
//!
//! 不预置 `t_customer` / `t_part` / `t_part_batch` / `t_shelf`：每个用例现场插
//! 数据，避免 fixture 占用 shelf code / customer prefix 与测试现场字面冲突
//! （snapshot 按 shelf.code 查找）。

use sqlx::PgPool;

/// `fixtures/dashboard_ws.sql` 加载产物：常量 ID 句柄供测试函数直接使用。
#[allow(dead_code)]
pub struct DashboardWsFixture {
    /// baseline WS 验签 user id（fx_dashboard_ws_user，对应 t_user_id=150）
    pub ws_user_id: i64,
}

impl DashboardWsFixture {
    /// fixture 内 baseline WS 用户的明文密码（bcrypt 哈希嵌入 dashboard_ws.sql）。
    /// 改此处必须同步更新 fixtures/dashboard_ws.sql 的 password_hash 字面值。
    pub const PASSWORD: &'static str = "changeme";

    pub const WS_USER_ID: i64 = 9_000_000_000_000_000_150;

    /// fixture 内 baseline WS 用户的 username（与 fixtures/dashboard_ws.sql 字面对齐）
    pub const WS_USERNAME: &'static str = "fx_dashboard_ws_user";
}

impl Default for DashboardWsFixture {
    fn default() -> Self {
        Self {
            ws_user_id: DashboardWsFixture::WS_USER_ID,
        }
    }
}

/// 加载 dashboard_ws fixture。
///
/// SQL 走 `include_str!` 编译期嵌入本 crate，路径相对本 crate
/// `CARGO_MANIFEST_DIR`（= `test-support/`），即 `fixtures/dashboard_ws.sql`。
///
/// SQL 内 INSERT 全部走常量 ID（不依赖运行时雪花 ID），跨测试并行 / 跨进程
/// 重跑都不会撞 ID。
///
/// ## 加载顺序
/// 1. 直接 `sqlx::raw_sql(dashboard_ws.sql)` —— 加载 1 行（1 user）。
/// 2. **不**调 [`load_iam_fixture`](super::iam::load_iam_fixture)——
///    dashboard_ws 测试不需要 IAM 5 用户基线。
#[allow(dead_code)]
pub async fn load_dashboard_ws_fixture(pool: &PgPool) -> DashboardWsFixture {
    let sql = include_str!("../../fixtures/dashboard_ws.sql");
    sqlx::raw_sql(sql)
        .execute(pool)
        .await
        .expect("load_dashboard_ws_fixture: insert fixture rows");
    DashboardWsFixture::default()
}
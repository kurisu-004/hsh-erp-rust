//! 集成测试 fixture（PR13 Phase I 引入，2026-09-24）：_e2e 域预制 fixture
//!
//! `tests/_e2e_api.rs`（627 行）单文件使用。覆盖 `/_e2e/probe` / `reset` /
//! `seed/*` / `revoke-session` / `hard-delete` / `e2e_guard_disabled` 端点。
//!
//! ## 字段按域需求聚合
//! - `t_process` ×1 —— fx_e2e_process（hard-delete 引用检测：被
//! `t_outsource_company_process` 引用 → 触发 21205 守卫）
//!
//! 不预置 `t_user` / `t_customer` / `t_outsource_company`：所有 9 个测试均走
//! `/_e2e/seed/*` 端点自建数据；fixture 仅提供 baseline t_process 行
//! （用于 `hard_delete_outsource_company_referenced_returns_409` 的引用检测）。

use sqlx::PgPool;

/// `fixtures/_e2e.sql` 加载产物：常量 ID 句柄供测试函数直接使用。
#[allow(dead_code)]
pub struct E2eFixture {
    /// baseline t_process row id（fx_e2e_process，对应 t_process_id=160）
    pub process_id: i64,
}

impl E2eFixture {
    pub const PROCESS_ID: i64 = 9_000_000_000_000_000_160;

    /// fixture 内 baseline t_process code（与 fixtures/_e2e.sql 字面对齐）
    pub const PROCESS_CODE: &'static str = "FX_E2E_PROC";
}

impl Default for E2eFixture {
    fn default() -> Self {
        Self {
            process_id: E2eFixture::PROCESS_ID,
        }
    }
}

/// 加载 _e2e fixture。
///
/// SQL 走 `include_str!` 编译期嵌入本 crate，路径相对本 crate
/// `CARGO_MANIFEST_DIR`（= `test-support/`），即 `fixtures/_e2e.sql`。
///
/// SQL 内 INSERT 全部走常量 ID（不依赖运行时雪花 ID），跨测试并行 / 跨进程
/// 重跑都不会撞 ID。
///
/// ## 加载顺序
/// 1. 直接 `sqlx::raw_sql(_e2e.sql)` —— 加载 1 行（1 process）。
/// 2. **不**调 [`load_iam_fixture`](super::iam::load_iam_fixture)——
///    _e2e 测试不需要 IAM 用户基线。
#[allow(dead_code)]
pub async fn load_e2e_fixture(pool: &PgPool) -> E2eFixture {
    let sql = include_str!("../../fixtures/_e2e.sql");
    sqlx::raw_sql(sql)
        .execute(pool)
        .await
        .expect("load_e2e_fixture: insert fixture rows");
    E2eFixture::default()
}
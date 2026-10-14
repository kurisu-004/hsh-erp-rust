//! `serial` 域集成测试（**2026-10-19 新建 binary**，与新域同批）
//!
//! 覆盖 `POST /api/v2/serials/release-terminal` 一条端点的 HTTP 契约：dry_run
//! 零写入 / 实跑两表清空 + 子件归档事件 / 幂等 / MANAGER 单角色守卫 / 明细截断
//! 与计数解耦。
//!
//! ## ⚠️ 入口**必须是 `main.rs`**
//!
//! cargo 1.98.1 **不识别** `tests/<dir>/mod.rs`，只识别 `tests/<dir>/main.rs`
//! （binary 名 = `<dir>`，即 `serial`）。
//!
//! ## 拆分映射（1 个 sub-file）
//! - `release.rs` —— 端点 1 的全部用例
//!
//! ## 前置条件
//! - PG：`scripts/test_nextest.sh` 自带 session 容器与 per-test fresh database。
//! - **Redis**：走 `/iam/login` 拿 token ⇒ 本 binary 需要 Redis（同全仓其它
//!   HTTP 契约测试同款前提）。跨 worktree 用 `TEST_REDIS_URL` 覆盖。
//!
//! ## fixture 口径
//!
//! 全部基建取自 `hsh_erp_test_support::*`，本 binary **不重复声明**任何一条
//! `send` / `json_request` / `login_token` / `test_pool` / `test_state`；取号一律走
//! 仓库登记的唯一源 `shared_test_snowflake()`（**绝不** `SnowflakeIdGenerator::new`
//! —— 两个 fresh generator 会撞 `t_part_event.id` / `t_part.id` 的 pkey，
//! 23505 会让整条事务回滚）。
//!
//! ⚠️ **别把底部的 `insert_part_with_serial` / `insert_assembly_with_serial` /
//! `shared_id` 当成违反 CLAUDE.md「禁止在测试文件内重新声明本地 `insert_*` / snowflake
//! 取号入口」的复制**（2026-10-19 review 第 2 轮 MINOR-9 澄清）：它们是**共享入口的
//! 薄壳**——`shared_id()` 逐字转调 `hsh_erp_test_support::shared_test_snowflake()`
//! （全仓唯一那把 generator，与 `test_state` 里的 `AppState.snowflake` 是同一把，
//! **没有**第二个 generator，也就没有 23505 风险）；两个 `insert_*_with_serial`
//! 是本域**特有形态**的直插 SQL（要造出「终态 + 持号」的行，建单端点造不出来，
//! 见各自 doc），不是任何通用 fixture 的副本。要判合规性请按「有没有新建
//! generator / 有没有复制 `send` 这类通用入口」这两条实际判据看，不是按函数名匹配。

#![allow(dead_code)]

mod release;

use serde_json::Value;
use sqlx::PgPool;

use hsh_erp_rust::infra::clock::now_naive;
use hsh_erp_test_support::{
    PartFixture, load_part_fixture, login_token, test_app, test_pool, test_state,
};

/// 一个用例的公共上下文：fresh database + fixture + 两个角色的 token。
///
/// ⚠️ 两个 token 是本域的**硬需求**：守卫是 MANAGER **单角色**，少一个 token 就
/// 钉不到「CLERK 被拒」这条（用例 4 的全部价值就在它）。
pub(crate) struct Ctx {
    pub pool: PgPool,
    pub app: axum::Router,
    pub manager_token: String,
    pub clerk_token: String,
    /// 一个未软删客户（装配件 / 零件的 `customer_id` 外键用）。
    pub customer_id: i64,
}

/// 起一套上下文：连库、灌 fixture、起 app、登录两个角色。
pub(crate) async fn bootstrap() -> Ctx {
    let pool = test_pool().await;
    let fx = load_part_fixture(&pool).await;
    let app = test_app(test_state(pool.clone()).await);

    let manager_token = login_token(&app, &fx.manager_username, PartFixture::PASSWORD).await;
    let clerk_token = login_token(&app, &fx.clerk_username, PartFixture::PASSWORD).await;

    Ctx {
        pool,
        app,
        manager_token,
        clerk_token,
        customer_id: fx.customer_l2_id,
    }
}

/// 断言 HTTP 状态 + 信封业务码。
///
/// 成功时 `want_code = 0`（`R::ok` 的信封码）、失败时传 `code::FORBIDDEN` 等。
/// **不放行 `data` 的断言** —— 各用例自己对 `data` 逐字段断言，那才是本 binary 的
/// 验收核心。
pub(crate) fn assert_envelope(
    status: axum::http::StatusCode,
    env: &Value,
    want_status: axum::http::StatusCode,
    want_code: i32,
) {
    assert_eq!(status, want_status, "HTTP 状态不符：{env}");
    assert_eq!(
        env["code"].as_i64(),
        Some(want_code as i64),
        "信封业务码不符（want {want_code}）：{env}"
    );
}

/// 一行工单的 `(id, serial_no, version)` 快照（断言「没动」用）。
pub(crate) async fn snapshot_part(pool: &PgPool, id: i64) -> (Option<String>, i32) {
    sqlx::query_as("SELECT serial_no, version FROM t_part WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .expect("查 t_part 快照")
}

/// 一行工单的 `(id, serial_no, version)` 快照（断言「没动」用）。
pub(crate) async fn snapshot_assembly(pool: &PgPool, id: i64) -> (Option<String>, i32) {
    sqlx::query_as("SELECT serial_no, version FROM t_assembly WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .expect("查 t_assembly 快照")
}

/// 插一行**持号**的零件 / 子件 → id。
///
/// 刻意直插 SQL 而不是走建单端点：本域要验的是「给定的 `(status, serial_no)` 形态
/// 被识别 / 被清」，走建单端点只能造出 `PENDING` + 自动派号，再逐个 UPDATE 改状态 ——
/// 多三步样板，且那条 UPDATE 本身可能带派生副作用。
///
/// ⚠️ `version` 显式给非默认值：**用例要断言「实跑后恰好 +1」**，初值必须是可辨认的。
pub(crate) async fn insert_part_with_serial(
    ctx: &Ctx,
    status: &str,
    serial_no: &str,
    drawing_no: &str,
) -> i64 {
    let id = shared_id();
    let now = now_naive();
    let today = now.date();
    sqlx::query(
        "INSERT INTO t_part (id, serial_no, name, drawing_no, applicant_name, quantity, \
         unit_price, total_price, request_date, planned_delivery_date, system_delivery_date, \
         status, is_urgent, customer_id, version, created_at, updated_at) \
         VALUES ($1, $2, 'serial 测试件', $3, 'tester', 1, 0, 0, $4, $4, $4, $5, false, $6, 7, $7, $7)",
    )
    .bind(id)
    .bind(serial_no)
    .bind(drawing_no)
    .bind(today)
    .bind(status)
    .bind(ctx.customer_id)
    .bind(now)
    .execute(&ctx.pool)
    .await
    .expect("插一行持号零件");
    id
}

/// 插一行**持号**的装配件 → id（`version` 同上，初值 7）。
pub(crate) async fn insert_assembly_with_serial(
    ctx: &Ctx,
    status: &str,
    serial_no: &str,
    drawing_no: &str,
) -> i64 {
    let id = shared_id();
    let now = now_naive();
    let today = now.date();
    sqlx::query(
        "INSERT INTO t_assembly (id, serial_no, drawing_no, name, applicant_name, quantity, \
         unit_price, total_price, request_date, planned_delivery_date, system_delivery_date, \
         status, is_urgent, customer_id, version, created_at, updated_at) \
         VALUES ($1, $2, $3, 'serial 测试装配件', 'tester', 1, 0, 0, $4, $4, $4, $5, false, \
         $6, 7, $7, $7)",
    )
    .bind(id)
    .bind(serial_no)
    .bind(drawing_no)
    .bind(today)
    .bind(status)
    .bind(ctx.customer_id)
    .bind(now)
    .execute(&ctx.pool)
    .await
    .expect("插一行持号装配件");
    id
}

/// 取一个测试雪花 id（**唯一真源** `shared_test_snowflake`，绝不在本文件
/// `SnowflakeIdGenerator::new` —— 见 `main.rs` 头部的 fixture 口径）。
pub(crate) fn shared_id() -> i64 {
    hsh_erp_test_support::shared_test_snowflake().next_id()
}

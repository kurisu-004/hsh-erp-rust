//! 集成测试 fixture（PR13 Phase I 引入，2026-09-24）：idempotency 域预制 fixture
//!
//! `tests/idempotency_api.rs`（590 行）单文件使用。覆盖 9 个中间件层用例
//! （含 1 个公开路径闸门）：同 key 缓存 / handler 单次调用 / 无 header 透传 /
//! 跨方法同 key 撞车 / GET/DELETE 跳过 / header missing 路径 / TTL 过期 /
//! login 闸门）。
//!
//! ## 字段按域需求聚合
//! （空 stub）
//!
//! 不预置任何表：idempotency_api 用 `make_test_app(state, counter)` 自建 mini
//! router + `__test/post` 等路由 + `Arc<AtomicUsize>` 计数器，走
//! `idempotency_middleware` 单测；所有用例用 UUID 唯一 idem-key 防止并行
//! 测试间 cache 撞车。本 fixture 是空 stub，仅占位 ID 段 200-209。

use sqlx::PgPool;

/// `fixtures/idempotency.sql` 加载产物：常量 ID 句柄（本 fixture 为空 stub）。
#[allow(dead_code)]
#[derive(Default)]
pub struct IdempotencyFixture {}

impl IdempotencyFixture {
    /// fixture ID 段起点（与 fixtures/idempotency.sql 占位 SELECT 1 对齐）。
    /// 实际无 INSERT 行，调用方不应依赖此值。
    pub const STUB_ID: i64 = 9_000_000_000_000_000_200;
}

/// 加载 idempotency fixture。
///
/// SQL 走 `include_str!` 编译期嵌入本 crate，路径相对本 crate
/// `CARGO_MANIFEST_DIR`（= `test-support/`），即 `fixtures/idempotency.sql`。
///
/// SQL 为 `SELECT 1;` 占位语句（idempotency_api 不依赖任何 DB 数据）。
/// load 函数仍保留以保持与其它 fixture 一致的「`load_<binary>_fixture`」
/// 调用约定，便于 PR-C.Final 替换 fixtures.rs 时模式统一。
#[allow(dead_code)]
pub async fn load_idempotency_fixture(pool: &PgPool) -> IdempotencyFixture {
    let sql = include_str!("../../fixtures/idempotency.sql");
    sqlx::raw_sql(sql)
        .execute(pool)
        .await
        .expect("load_idempotency_fixture: insert fixture rows");
    IdempotencyFixture::default()
}
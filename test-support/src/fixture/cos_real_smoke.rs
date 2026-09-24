//! 集成测试 fixture（PR13 Phase I 引入，2026-09-24）：cos_real_smoke 域预制 fixture
//!
//! `tests/cos_real_smoke.rs`（187 行）单文件使用。opt-in 真 COS 烟雾测试
//! （`#[ignore]`，需 `RUN_REAL_COS_TESTS=1`），用 OpenDalCos 直连腾讯云
//! COS 凭据从环境变量读取。
//!
//! ## 字段按域需求聚合
//! （空 stub）
//!
//! 不预置任何表：cos_real_smoke 不依赖 PG / Redis。本 fixture 是空 stub，
//! 仅占位 ID 段 220-229。

use sqlx::PgPool;

/// `fixtures/cos_real_smoke.sql` 加载产物：常量 ID 句柄（本 fixture 为空 stub）。
#[allow(dead_code)]
pub struct CosRealSmokeFixture {}

impl CosRealSmokeFixture {
    /// fixture ID 段起点（与 fixtures/cos_real_smoke.sql 占位 SELECT 1 对齐）。
    /// 实际无 INSERT 行，调用方不应依赖此值。
    pub const STUB_ID: i64 = 9_000_000_000_000_000_220;
}

impl Default for CosRealSmokeFixture {
    fn default() -> Self {
        Self {}
    }
}

/// 加载 cos_real_smoke fixture。
///
/// SQL 走 `include_str!` 编译期嵌入本 crate，路径相对本 crate
/// `CARGO_MANIFEST_DIR`（= `test-support/`），即 `fixtures/cos_real_smoke.sql`。
///
/// SQL 为 `SELECT 1;` 占位语句（cos_real_smoke 不依赖任何 DB 数据）。
/// load 函数仍保留以保持与其它 fixture 一致的「`load_<binary>_fixture`」
/// 调用约定，便于 PR-C.Final 替换 fixtures.rs 时模式统一。
#[allow(dead_code)]
pub async fn load_cos_real_smoke_fixture(pool: &PgPool) -> CosRealSmokeFixture {
    let sql = include_str!("../../fixtures/cos_real_smoke.sql");
    sqlx::raw_sql(sql)
        .execute(pool)
        .await
        .expect("load_cos_real_smoke_fixture: insert fixture rows");
    CosRealSmokeFixture::default()
}
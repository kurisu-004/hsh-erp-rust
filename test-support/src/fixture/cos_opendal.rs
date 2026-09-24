//! 集成测试 fixture（PR13 Phase I 引入，2026-09-24）：cos_opendal 域预制 fixture
//!
//! `tests/cos_opendal_api.rs`（465 行）单文件使用。覆盖 OpenDAL `Operator`
//! 适配 `CosClient` trait 的 6 个 method + 配置 / backend 选择路径
//! （NoopOpenDal Memory backend + 临时 RSA PEM 文件）。
//!
//! ## 字段按域需求聚合
//! （空 stub）
//!
//! 不预置任何表：cos_opendal_api 是 COS 客户端单测 + AppConfig 解析路径测试，
//! 不依赖 PG / Redis。本 fixture 是空 stub，仅占位 ID 段 210-219。

use sqlx::PgPool;

/// `fixtures/cos_opendal.sql` 加载产物：常量 ID 句柄（本 fixture 为空 stub）。
#[allow(dead_code)]
pub struct CosOpendalFixture {}

impl CosOpendalFixture {
    /// fixture ID 段起点（与 fixtures/cos_opendal.sql 占位 SELECT 1 对齐）。
    /// 实际无 INSERT 行，调用方不应依赖此值。
    pub const STUB_ID: i64 = 9_000_000_000_000_000_210;
}

impl Default for CosOpendalFixture {
    fn default() -> Self {
        Self {}
    }
}

/// 加载 cos_opendal fixture。
///
/// SQL 走 `include_str!` 编译期嵌入本 crate，路径相对本 crate
/// `CARGO_MANIFEST_DIR`（= `test-support/`），即 `fixtures/cos_opendal.sql`。
///
/// SQL 为 `SELECT 1;` 占位语句（cos_opendal_api 不依赖任何 DB 数据）。
/// load 函数仍保留以保持与其它 fixture 一致的「`load_<binary>_fixture`」
/// 调用约定，便于 PR-C.Final 替换 fixtures.rs 时模式统一。
#[allow(dead_code)]
pub async fn load_cos_opendal_fixture(pool: &PgPool) -> CosOpendalFixture {
    let sql = include_str!("../../fixtures/cos_opendal.sql");
    sqlx::raw_sql(sql)
        .execute(pool)
        .await
        .expect("load_cos_opendal_fixture: insert fixture rows");
    CosOpendalFixture::default()
}
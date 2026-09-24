//! 集成测试 fixture（PR13 Phase I 引入，2026-09-24）：cnc_program 域预制 fixture
//!
//! `tests/cnc_program_api.rs`（404 行）单文件使用。覆盖 CncProgramService
//! upload_cnc_pair / list_pairs_for_part + PartFileService alias 端点
//! （download-url / content / delete）。
//!
//! ## 字段按域需求聚合
//! - `t_customer` ×2 —— L1（fx_cnc_l1，prefix='C'）+ L2（fx_cnc_l2，parent=L1）
//! - `t_part` ×1 —— fx_cnc_part（PENDING 状态，upload_cnc_pair 测试用）
//!
//! 不预置 `t_user`：cnc_program_api 是 service 层单测，用
//! `test_current_user(vec![Role::Manager])` 构造 CurrentUser（不依赖 DB user 行）。

use sqlx::PgPool;

/// `fixtures/cnc_program.sql` 加载产物：常量 ID 句柄供测试函数直接使用。
#[allow(dead_code)]
pub struct CncProgramFixture {
    /// baseline L1 customer id（fx_cnc_l1，对应 t_customer_id=170）
    pub l1_customer_id: i64,
    /// baseline L2 customer id（fx_cnc_l2，对应 t_customer_id=171）
    pub l2_customer_id: i64,
    /// baseline part id（fx_cnc_part，PENDING 状态，对应 t_part_id=172）
    pub part_id: i64,
}

impl CncProgramFixture {
    pub const L1_CUSTOMER_ID: i64 = 9_000_000_000_000_000_170;
    pub const L2_CUSTOMER_ID: i64 = 9_000_000_000_000_000_171;
    pub const PART_ID: i64 = 9_000_000_000_000_000_172;
}

impl Default for CncProgramFixture {
    fn default() -> Self {
        Self {
            l1_customer_id: CncProgramFixture::L1_CUSTOMER_ID,
            l2_customer_id: CncProgramFixture::L2_CUSTOMER_ID,
            part_id: CncProgramFixture::PART_ID,
        }
    }
}

/// 加载 cnc_program fixture。
///
/// SQL 走 `include_str!` 编译期嵌入本 crate，路径相对本 crate
/// `CARGO_MANIFEST_DIR`（= `test-support/`），即 `fixtures/cnc_program.sql`。
///
/// SQL 内 INSERT 全部走常量 ID（不依赖运行时雪花 ID），跨测试并行 / 跨进程
/// 重跑都不会撞 ID。
///
/// ## 加载顺序
/// 1. 直接 `sqlx::raw_sql(cnc_program.sql)` —— 加载 3 行（1 L1 + 1 L2 + 1 part）。
/// 2. **不**调 [`load_part_fixture`](super::part::load_part_fixture)——
///    cnc_program 测试不需要 part 域基线的 9 行（user / role / shelf / 多个 part）。
#[allow(dead_code)]
pub async fn load_cnc_program_fixture(pool: &PgPool) -> CncProgramFixture {
    let sql = include_str!("../../fixtures/cnc_program.sql");
    sqlx::raw_sql(sql)
        .execute(pool)
        .await
        .expect("load_cnc_program_fixture: insert fixture rows");
    CncProgramFixture::default()
}
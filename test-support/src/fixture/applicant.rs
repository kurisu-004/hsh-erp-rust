//! 集成测试 fixture（PR13 Phase I 引入，2026-09-24）：applicant 域预制 fixture
//!
//! `tests/applicant_api.rs`（466 行）单文件使用。applicant 域 6 端点集成测试
//! 覆盖 list / create / get / update / soft-delete / in-use 校验 / OCC。
//!
//! ## 字段按域需求聚合
//! - `t_user` ×1 —— fx_applicant_manager（密码 "changeme"，MANAGER role）
//! - `t_user_role` ×1 —— baseline MANAGER role（无 scope）
//! - `t_customer` ×2 —— L1（fx_applicant_l1，prefix='A'）+ L2（fx_applicant_l2，parent=L1）
//! - `t_applicant` ×1 —— baseline applicant 行（happy path 引用）
//! - `t_part` ×1 —— 引用 baseline applicant + L1 customer 的 part 行（in-use 测试用）

use sqlx::PgPool;

/// `fixtures/applicant.sql` 加载产物：常量 ID 句柄供测试函数直接使用。
#[allow(dead_code)]
pub struct ApplicantFixture {
    /// baseline MANAGER user id（fx_applicant_manager，对应 t_user_id=130）
    pub manager_user_id: i64,
    /// baseline MANAGER user id（t_user_role，对应 131）
    pub manager_role_id: i64,
    /// baseline L1 customer id（fx_applicant_l1，对应 t_customer_id=132）
    pub l1_customer_id: i64,
    /// baseline L2 customer id（fx_applicant_l2，对应 t_customer_id=133）
    pub l2_customer_id: i64,
    /// baseline applicant row id（fx-baseline-applicant，对应 t_applicant_id=134）
    pub sample_applicant_id: i64,
    /// baseline t_part row id（fx-part-ref-applicant，对应 t_part_id=135）
    pub part_ref_id: i64,
}

impl ApplicantFixture {
    /// fixture 内 baseline MANAGER 用户的明文密码（bcrypt 哈希嵌入 applicant.sql）。
    /// 改此处必须同步更新 fixtures/applicant.sql 的 password_hash 字面值。
    pub const PASSWORD: &'static str = "changeme";

    pub const MANAGER_USER_ID: i64 = 9_000_000_000_000_000_130;
    pub const MANAGER_ROLE_ID: i64 = 9_000_000_000_000_000_131;
    pub const L1_CUSTOMER_ID: i64 = 9_000_000_000_000_000_132;
    pub const L2_CUSTOMER_ID: i64 = 9_000_000_000_000_000_133;
    pub const SAMPLE_APPLICANT_ID: i64 = 9_000_000_000_000_000_134;
    pub const PART_REF_ID: i64 = 9_000_000_000_000_000_135;

    /// fixture 内 baseline MANAGER 用户的 username（与 fixtures/applicant.sql 字面对齐）
    pub const MANAGER_USERNAME: &'static str = "fx_applicant_manager";
    /// fixture 内 baseline applicant name（t_applicant.name）
    pub const SAMPLE_APPLICANT_NAME: &'static str = "fx-baseline-applicant";
}

impl Default for ApplicantFixture {
    fn default() -> Self {
        Self {
            manager_user_id: ApplicantFixture::MANAGER_USER_ID,
            manager_role_id: ApplicantFixture::MANAGER_ROLE_ID,
            l1_customer_id: ApplicantFixture::L1_CUSTOMER_ID,
            l2_customer_id: ApplicantFixture::L2_CUSTOMER_ID,
            sample_applicant_id: ApplicantFixture::SAMPLE_APPLICANT_ID,
            part_ref_id: ApplicantFixture::PART_REF_ID,
        }
    }
}

/// 加载 applicant fixture。
///
/// SQL 走 `include_str!` 编译期嵌入本 crate，路径相对本 crate
/// `CARGO_MANIFEST_DIR`（= `test-support/`），即 `fixtures/applicant.sql`。
///
/// SQL 内 INSERT 全部走常量 ID（不依赖运行时雪花 ID），跨测试并行 / 跨进程
/// 重跑都不会撞 ID。bcrypt 哈希预生成嵌入 SQL，省每测试 ~250ms 现场 hash 开销。
///
/// ## 加载顺序
/// 1. 直接 `sqlx::raw_sql(applicant.sql)` —— 加载 6 行（1 user + 1 role + 2 customers + 1 applicant + 1 part）。
/// 2. **不**调 [`load_iam_fixture`](super::iam::load_iam_fixture)——
///    applicant 测试不需要 IAM 5 用户基线。
///
/// ## 与 `tests/applicant_api.rs` 的对应
/// - 测试内「建 applicant」/「in-use 校验」/「OCC 并发 update」可直接用 fixture
///   提供的 L1 customer_id + applicant_name，避免每测试现场 INSERT。
#[allow(dead_code)]
pub async fn load_applicant_fixture(pool: &PgPool) -> ApplicantFixture {
    let sql = include_str!("../../fixtures/applicant.sql");
    sqlx::raw_sql(sql)
        .execute(pool)
        .await
        .expect("load_applicant_fixture: insert fixture rows");
    ApplicantFixture::default()
}
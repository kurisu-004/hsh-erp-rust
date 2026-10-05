//! 集成测试 fixture（按域拆分）：加载 `fixtures/inspection.sql` 提供强类型句柄
//!
//! 2026-10-05 新增：prod::inspection 子模块扫码端点
//! `GET /api/v2/prod/inspection/scan/{serial_no}`（装配件 → 全部子件 → 全部批次
//! 三层树）预制 fixture，结构照 [`ProcessDesignFixture`](super::process_design::ProcessDesignFixture)
//! 范本：
//!
//! 1. SQL 落到 `test-support/fixtures/inspection.sql`，常量 ID 走
//!    9_000_000_000_000_000_261+ 区段（与 `test-support/fixtures/` 下其余全部
//!    fixture 声明的 ID 段物理不相交，核对清单见该 SQL 头注释）；
//! 2. bcrypt 哈希预生成嵌入 SQL，省 ~250ms×N 现场 hash 开销；
//! 3. 本文件定义 `InspectionFixture` struct + 常量 ID `const` + `Default` 实现
//!    + `load_inspection_fixture(pool)` 函数；
//! 4. 测试 binary（`tests/production/inspection.rs`）直接拿句柄。
//!
//! ⚠️ 本文件的常量与 `fixtures/inspection.sql` 的 INSERT 字面值是**同一批字面量**，
//! 任何一边改 ID 段都必须同步另一边，否则集成测试会静默查空行。
//!
//! ## 为什么要 5 个角色账号
//! 角色守卫场景要逐个登录 2 个白名单角色（MANAGER / INSPECTOR）并用 3 个越权角色
//! （CLERK / CNC_PROGRAMMER / SHELF_ACCOUNT）验证 40300，故账号预置进 fixture 而非
//! 测试内临时建号 —— 临时建号要现场 bcrypt（~250ms×5）且 hash 值不在 SQL 里可审。
//!
//! ## 为什么要预置全套批次状态
//! 扫码树的口径是「读**全部**批次、不按状态过滤」（含 `COMPLETED` / `CANCELLED`
//! 等终态），而这条口径只能靠 fixture 把 8 种状态一次摆齐来证伪 —— 测试内按场景
//! 临时插批次就只能覆盖当场景那一种。故「基线零件行 + 全状态批次 + 反例（软删）行」
//! 一并预置进 SQL，测试只按需追加自己造的差异行。
//!
//! ## 当前域
//! - `inspection`：2 客户（L1 根 + L2 子） + 2 货架（品检架 / 生产架） + 2 工序 +
//!   1 条货架↔工序映射 + 1 装配件 + 1 独立件 + 4 子件（含 1 软删） +
//!   9 批次（含 1 软删、1 返修中）+ 5 角色用户

use sqlx::PgPool;

/// `fixtures/inspection.sql` 加载产物：常量句柄供测试函数直接使用。
///
/// 字段名与 SQL INSERT 字面值逐字对应。常量 ID 全部走 9_000_000_000_000_000_261+
/// 区段，与运行时雪花 ID 及 `test-support/fixtures/` 下其余 fixture 均物理不相交。
#[allow(dead_code)]
pub struct InspectionFixture {
    /// L1 根客户 id
    pub l1_customer_id: i64,
    /// L2 叶子客户 id（零件行的 `customer_id` 指向它）
    pub l2_customer_id: i64,
    /// 品检架 id（`zone='INSPECTION'`；INSPECTION 批次的 holder）
    pub inspection_shelf_id: i64,
    /// 生产架 id（`zone='PRODUCTION'`；IN_PROCESS 批次的 holder）
    pub production_shelf_id: i64,
    /// 工序 A id（独立件 IN_PROCESS 批次的 `current_process_id`）
    pub process_a_id: i64,
    /// 工序 B id（返修中批次的 `current_process_id`）
    pub process_b_id: i64,
    /// 货架 ↔ 工序映射行 id（生产架 ↔ 工序 A）
    pub shelf_process_id: i64,
    /// 装配件 id（4 个子件的 `assembly_id` 都指向它）
    pub assembly_id: i64,
    /// 独立件 id（`assembly_id IS NULL`），挂 8 个批次
    pub part_standalone: i64,
    /// 子件 #1 id（★ 扫码树核心回归：扫中的就是它）
    pub part_child_1: i64,
    /// 子件 #2 id（**无批次**，验 children 是「全部子件」）
    pub part_child_2: i64,
    /// 子件 #3 id（**无批次**）
    pub part_child_3: i64,
    /// 子件 #4 id，`deleted_at` 非空（软删闸门）
    pub part_child_deleted: i64,
    /// 独立件批次：`INSPECTION`（★ version=3 ≠ 零件 version=1）
    pub batch_inspection: i64,
    /// 独立件批次：`PENDING`
    pub batch_pending: i64,
    /// 独立件批次：`IN_PROCESS` + 工序 A（`process_name` 取到真值的唯一形态）
    pub batch_in_process: i64,
    /// 独立件批次：`READY_TO_SHIP`（`current_process_id` 恒 NULL）
    pub batch_ready_to_ship: i64,
    /// 独立件批次：`COMPLETED` 终态
    pub batch_completed: i64,
    /// 独立件批次：`CANCELLED` 终态
    pub batch_cancelled: i64,
    /// 独立件批次：`IN_PROCESS` + `is_repairing = true` + 工序 B
    pub batch_repairing: i64,
    /// 独立件批次：`deleted_at` 非空（软删闸门）
    pub batch_soft_deleted: i64,
    /// 子件 #1 的唯一批次：`INSPECTION`（version=7 ≠ 零件 version=4）
    pub batch_child_inspection: i64,
    /// MANAGER 用户名（白名单角色）
    pub manager_username: String,
    /// CLERK 用户名（越权角色）
    pub clerk_username: String,
    /// INSPECTOR 用户名（白名单角色）
    pub inspector_username: String,
    /// CNC_PROGRAMMER 用户名（越权角色）
    pub cnc_username: String,
    /// SHELF_ACCOUNT 用户名（越权角色）
    pub shelf_username: String,
}

impl InspectionFixture {
    /// fixture 内 5 个用户共用的明文密码（bcrypt 哈希嵌入 SQL）。
    /// 改此处必须同步更新 fixtures/inspection.sql 的 password_hash 字面值。
    pub const PASSWORD: &'static str = "changeme";

    pub const L1_CUSTOMER_ID: i64 = 9_000_000_000_000_000_261;
    pub const L2_CUSTOMER_ID: i64 = 9_000_000_000_000_000_262;
    pub const INSPECTION_SHELF_ID: i64 = 9_000_000_000_000_000_263;
    pub const PRODUCTION_SHELF_ID: i64 = 9_000_000_000_000_000_264;
    pub const PROCESS_A_ID: i64 = 9_000_000_000_000_000_265;
    pub const PROCESS_B_ID: i64 = 9_000_000_000_000_000_266;
    pub const SHELF_PROCESS_ID: i64 = 9_000_000_000_000_000_267;
    pub const MANAGER_USER_ID: i64 = 9_000_000_000_000_000_268;
    pub const CLERK_USER_ID: i64 = 9_000_000_000_000_000_269;
    pub const INSPECTOR_USER_ID: i64 = 9_000_000_000_000_000_270;
    pub const CNC_USER_ID: i64 = 9_000_000_000_000_000_271;
    pub const SHELF_USER_ID: i64 = 9_000_000_000_000_000_272;
    pub const MANAGER_ROLE_ID: i64 = 9_000_000_000_000_000_273;
    pub const CLERK_ROLE_ID: i64 = 9_000_000_000_000_000_274;
    pub const INSPECTOR_ROLE_ID: i64 = 9_000_000_000_000_000_275;
    pub const CNC_ROLE_ID: i64 = 9_000_000_000_000_000_276;
    pub const SHELF_ROLE_ID: i64 = 9_000_000_000_000_000_277;
    pub const ASSEMBLY_ID: i64 = 9_000_000_000_000_000_278;
    pub const PART_STANDALONE: i64 = 9_000_000_000_000_000_279;
    /// ★ 被扫中的子件行 id（扫码树核心回归：扫子件 → 返回整棵装配件树）
    pub const PART_CHILD_1: i64 = 9_000_000_000_000_000_280;
    pub const PART_CHILD_2: i64 = 9_000_000_000_000_000_281;
    pub const PART_CHILD_3: i64 = 9_000_000_000_000_000_282;
    pub const PART_CHILD_DELETED: i64 = 9_000_000_000_000_000_283;
    pub const BATCH_INSPECTION: i64 = 9_000_000_000_000_000_290;
    pub const BATCH_PENDING: i64 = 9_000_000_000_000_000_291;
    pub const BATCH_IN_PROCESS: i64 = 9_000_000_000_000_000_292;
    pub const BATCH_READY_TO_SHIP: i64 = 9_000_000_000_000_000_293;
    pub const BATCH_COMPLETED: i64 = 9_000_000_000_000_000_294;
    pub const BATCH_CANCELLED: i64 = 9_000_000_000_000_000_295;
    pub const BATCH_REPAIRING: i64 = 9_000_000_000_000_000_296;
    pub const BATCH_SOFT_DELETED: i64 = 9_000_000_000_000_000_297;
    pub const BATCH_CHILD_INSPECTION: i64 = 9_000_000_000_000_000_300;

    /// 独立件序列号（扫它 → `assembly = null` 的独立件树）
    pub const STANDALONE_SERIAL_NO: &'static str = "SI-S1001";
    /// 装配件序列号（扫它 → `hit_kind = "ASSEMBLY"` 的装配件树）
    pub const ASSEMBLY_SERIAL_NO: &'static str = "SI-ASM";
    /// 子件 #1 序列号（扫它 → `hit_kind = "PART"` + `assembly` 有值的装配件树）
    pub const CHILD_1_SERIAL_NO: &'static str = "SI-ASM-01";
    /// 子件 #2 序列号
    pub const CHILD_2_SERIAL_NO: &'static str = "SI-ASM-02";
    /// 子件 #3 序列号
    pub const CHILD_3_SERIAL_NO: &'static str = "SI-ASM-03";
    /// 软删子件 #4 序列号（扫它 → 404）
    pub const CHILD_DELETED_SERIAL_NO: &'static str = "SI-ASM-04";

    /// 独立件行的 `t_part.version`（fixture 写死 1）
    ///
    /// 与 [`Self::BATCH_INSPECTION`] 对应的批次 version（写死 3）**不同** ——
    /// 两者不等是本 fixture 的刻意设计，用来证明扫码树的批次 `version` 取自
    /// `t_part_batch.version` 而非 `t_part.version`。
    pub const PART_VERSION: i32 = 1;
    /// `INSPECTION` 批次的 `t_part_batch.version`（fixture 写死 3）
    pub const BATCH_INSPECTION_VERSION: i32 = 3;
    /// 子件 #1 行的 `t_part.version`（fixture 写死 4）
    pub const CHILD_1_VERSION: i32 = 4;
    /// 子件 #1 的 `INSPECTION` 批次的 `t_part_batch.version`（fixture 写死 7）
    pub const BATCH_CHILD_VERSION: i32 = 7;

    /// 工序 A 名（`process_name` 取到真值时的断言目标）
    pub const PROCESS_A_NAME: &'static str = "SI process A";
    /// 工序 B 名（返修中批次的 `process_name` 断言目标）
    pub const PROCESS_B_NAME: &'static str = "SI process B";

    /// fixture 内 MANAGER 用户的 username（与 fixtures/inspection.sql 的 INSERT 字面对齐）
    pub const MANAGER_USERNAME: &'static str = "fx_si_manager";
    /// fixture 内 CLERK 用户的 username（越权角色）
    pub const CLERK_USERNAME: &'static str = "fx_si_clerk";
    /// fixture 内 INSPECTOR 用户的 username（与 MANAGER 同为白名单角色）
    pub const INSPECTOR_USERNAME: &'static str = "fx_si_inspector";
    /// fixture 内 CNC_PROGRAMMER 用户的 username（越权角色）
    pub const CNC_USERNAME: &'static str = "fx_si_cnc";
    /// fixture 内 SHELF_ACCOUNT 用户的 username（越权角色）
    pub const SHELF_USERNAME: &'static str = "fx_si_shelf";
}

impl Default for InspectionFixture {
    fn default() -> Self {
        Self {
            l1_customer_id: InspectionFixture::L1_CUSTOMER_ID,
            l2_customer_id: InspectionFixture::L2_CUSTOMER_ID,
            inspection_shelf_id: InspectionFixture::INSPECTION_SHELF_ID,
            production_shelf_id: InspectionFixture::PRODUCTION_SHELF_ID,
            process_a_id: InspectionFixture::PROCESS_A_ID,
            process_b_id: InspectionFixture::PROCESS_B_ID,
            shelf_process_id: InspectionFixture::SHELF_PROCESS_ID,
            assembly_id: InspectionFixture::ASSEMBLY_ID,
            part_standalone: InspectionFixture::PART_STANDALONE,
            part_child_1: InspectionFixture::PART_CHILD_1,
            part_child_2: InspectionFixture::PART_CHILD_2,
            part_child_3: InspectionFixture::PART_CHILD_3,
            part_child_deleted: InspectionFixture::PART_CHILD_DELETED,
            batch_inspection: InspectionFixture::BATCH_INSPECTION,
            batch_pending: InspectionFixture::BATCH_PENDING,
            batch_in_process: InspectionFixture::BATCH_IN_PROCESS,
            batch_ready_to_ship: InspectionFixture::BATCH_READY_TO_SHIP,
            batch_completed: InspectionFixture::BATCH_COMPLETED,
            batch_cancelled: InspectionFixture::BATCH_CANCELLED,
            batch_repairing: InspectionFixture::BATCH_REPAIRING,
            batch_soft_deleted: InspectionFixture::BATCH_SOFT_DELETED,
            batch_child_inspection: InspectionFixture::BATCH_CHILD_INSPECTION,
            manager_username: InspectionFixture::MANAGER_USERNAME.to_string(),
            clerk_username: InspectionFixture::CLERK_USERNAME.to_string(),
            inspector_username: InspectionFixture::INSPECTOR_USERNAME.to_string(),
            cnc_username: InspectionFixture::CNC_USERNAME.to_string(),
            shelf_username: InspectionFixture::SHELF_USERNAME.to_string(),
        }
    }
}

/// 加载 inspection fixture。
///
/// SQL 走 `include_str!` 编译期嵌入本 crate，路径相对本 crate
/// `CARGO_MANIFEST_DIR`（= `test-support/`），即 `fixtures/inspection.sql`。
///
/// SQL 内 INSERT 全部走常量 ID（不依赖运行时雪花 ID），跨测试并行 / 跨进程
/// 重跑都不会撞 ID。bcrypt 哈希预生成嵌入 SQL，省每测试 ~250ms 现场 hash 开销。
#[allow(dead_code)]
pub async fn load_inspection_fixture(pool: &PgPool) -> InspectionFixture {
    let sql = include_str!("../../fixtures/inspection.sql");
    sqlx::raw_sql(sql)
        .execute(pool)
        .await
        .expect("load_inspection_fixture: insert fixture rows");
    InspectionFixture::default()
}

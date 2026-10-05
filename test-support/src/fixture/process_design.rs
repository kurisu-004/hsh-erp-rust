//! 集成测试 fixture（按域拆分）：加载 `fixtures/process_design.sql` 提供强类型句柄
//!
//! 2026-10-05 新增：prod::process_design 子模块（制定工序页零件列表 1 端点）预制
//! fixture，结构照 [`ProcessChainFixture`](super::process_chain::ProcessChainFixture)
//! 范本：
//!
//! 1. SQL 落到 `test-support/fixtures/process_design.sql`，常量 ID 走
//!    9_000_000_000_000_000_001+ 区段；
//! 2. bcrypt 哈希预生成嵌入 SQL，省 ~250ms×N 现场 hash 开销；
//! 3. 本文件定义 `ProcessDesignFixture` struct + 常量 ID `const` + `Default` 实现
//!    + `load_process_design_fixture(pool)` 函数；
//! 4. 测试 binary（`tests/production/process_design.rs`）直接拿句柄。
//!
//! ## 为什么要 5 个角色账号
//! 角色守卫场景要逐个登录 4 个白名单角色（MANAGER / CLERK / INSPECTOR /
//! CNC_PROGRAMMER）并用 1 个越权角色（SHELF_ACCOUNT）验证 40300，故账号预置进
//! fixture 而非测试内临时建号 —— 临时建号要现场 bcrypt（~250ms×5）且 hash 值不在
//! SQL 里可审。
//!
//! ## 为什么不预置 t_part
//! 本域谓词是 `status = 'PENDING' AND deleted_at IS NULL`，而闸门场景需要
//! IN_PROCESS / PROGRAMMING / COMPLETED / CANCELLED / 软删五类反例行 —— 这些行
//! 只能在 SQL 里显式写死（状态机不允许它们回退 PENDING）。故「基线零件行 + 反例行」
//! 一并预置进 SQL，测试只按需追加自己造的差异行。
//!
//! ## 当前域
//! - `process_design`：1 客户 + 1 货架（SHELF_ACCOUNT 的 scope 锚点） + 1 工艺链 +
//!   1 装配件 + 6 个 PENDING 零件（含 1 子件、1 无序列号、1 已挂链）+ 4 个非 PENDING
//!   反例零件 + 1 个软删零件 + 5 角色用户

use sqlx::PgPool;

/// `fixtures/process_design.sql` 加载产物：常量句柄供测试函数直接使用。
///
/// 字段名与 SQL INSERT 字面值逐字对应。常量 ID 全部走 9_000_000_000_000_000_001+
/// 区段，与运行时雪花 ID 物理不相交。
#[allow(dead_code)]
pub struct ProcessDesignFixture {
    /// L1 客户 id（`t_part.customer_id` NOT NULL 指向它）
    pub customer_id: i64,
    /// 检验架 id（`fx_pd_shelf` 的 `scope_id`）
    pub shelf_id: i64,
    /// 工艺链 id（`PART_CHAINED` 挂它）
    pub chain_id: i64,
    /// 装配件 id（`PART_CHILD` 的 `assembly_id`）
    pub assembly_id: i64,
    /// PENDING 基线零件 #1（有序列号）
    pub part_base_1: i64,
    /// PENDING 基线零件 #2（有序列号）
    pub part_base_2: i64,
    /// PENDING 但 `serial_no IS NULL`（手工工单，验 NULLS LAST）
    pub part_no_serial: i64,
    /// PENDING 且已挂 `chain_id`（验 `process_chain_id` 非 null）
    pub part_chained: i64,
    /// PENDING 装配件**子件**（`assembly_id` 指向 [`Self::assembly_id`]）★ 核心回归行
    pub part_child: i64,
    /// PENDING 装配件主表行（独立行形态，`assembly_id IS NULL`）
    pub part_assembly: i64,
    /// 反例：`status = 'IN_PROCESS'`
    pub part_in_process: i64,
    /// 反例：`status = 'PROGRAMMING'`
    pub part_programming: i64,
    /// 反例：`status = 'COMPLETED'`
    pub part_completed: i64,
    /// 反例：`status = 'CANCELLED'`
    pub part_cancelled: i64,
    /// 反例：PENDING 但 `deleted_at` 非空
    pub part_soft_deleted: i64,
    /// MANAGER 用户名
    pub manager_username: String,
    /// CLERK 用户名
    pub clerk_username: String,
    /// INSPECTOR 用户名
    pub inspector_username: String,
    /// CNC_PROGRAMMER 用户名
    pub cnc_username: String,
    /// SHELF_ACCOUNT 用户名（越权角色）
    pub shelf_username: String,
}

impl ProcessDesignFixture {
    /// fixture 内 5 个用户共用的明文密码（bcrypt 哈希嵌入 SQL）。
    /// 改此处必须同步更新 fixtures/process_design.sql 的 password_hash 字面值。
    pub const PASSWORD: &'static str = "changeme";

    pub const CUSTOMER_ID: i64 = 9_000_000_000_000_000_001;
    pub const SHELF_ID: i64 = 9_000_000_000_000_000_002;
    pub const CHAIN_ID: i64 = 9_000_000_000_000_000_003;
    pub const MANAGER_USER_ID: i64 = 9_000_000_000_000_000_004;
    pub const CLERK_USER_ID: i64 = 9_000_000_000_000_000_005;
    pub const INSPECTOR_USER_ID: i64 = 9_000_000_000_000_000_006;
    pub const CNC_USER_ID: i64 = 9_000_000_000_000_000_007;
    pub const SHELF_USER_ID: i64 = 9_000_000_000_000_000_008;
    pub const MANAGER_ROLE_ID: i64 = 9_000_000_000_000_000_009;
    pub const CLERK_ROLE_ID: i64 = 9_000_000_000_000_000_010;
    pub const INSPECTOR_ROLE_ID: i64 = 9_000_000_000_000_000_011;
    pub const CNC_ROLE_ID: i64 = 9_000_000_000_000_000_012;
    pub const SHELF_ROLE_ID: i64 = 9_000_000_000_000_000_013;
    pub const ASSEMBLY_ID: i64 = 9_000_000_000_000_000_014;
    pub const PART_BASE_1: i64 = 9_000_000_000_000_000_015;
    pub const PART_BASE_2: i64 = 9_000_000_000_000_000_016;
    pub const PART_NO_SERIAL: i64 = 9_000_000_000_000_000_017;
    pub const PART_CHAINED: i64 = 9_000_000_000_000_000_018;
    /// ★ 装配件子件行 id（本域核心回归：子件必须在列表里可见）
    pub const PART_CHILD: i64 = 9_000_000_000_000_000_019;
    pub const PART_ASSEMBLY: i64 = 9_000_000_000_000_000_020;
    pub const PART_IN_PROCESS: i64 = 9_000_000_000_000_000_021;
    pub const PART_PROGRAMMING: i64 = 9_000_000_000_000_000_022;
    pub const PART_COMPLETED: i64 = 9_000_000_000_000_000_023;
    pub const PART_CANCELLED: i64 = 9_000_000_000_000_000_024;
    pub const PART_SOFT_DELETED: i64 = 9_000_000_000_000_000_025;

    /// fixture 内 MANAGER 用户的 username（与 fixtures/process_design.sql 的 INSERT 字面对齐）
    pub const MANAGER_USERNAME: &'static str = "fx_pd_manager";
    /// fixture 内 CLERK 用户的 username
    pub const CLERK_USERNAME: &'static str = "fx_pd_clerk";
    /// fixture 内 INSPECTOR 用户的 username
    pub const INSPECTOR_USERNAME: &'static str = "fx_pd_inspector";
    /// fixture 内 CNC_PROGRAMMER 用户的 username
    pub const CNC_USERNAME: &'static str = "fx_pd_cnc";
    /// fixture 内 SHELF_ACCOUNT 用户的 username（越权角色）
    pub const SHELF_USERNAME: &'static str = "fx_pd_shelf";
}

impl Default for ProcessDesignFixture {
    fn default() -> Self {
        Self {
            customer_id: ProcessDesignFixture::CUSTOMER_ID,
            shelf_id: ProcessDesignFixture::SHELF_ID,
            chain_id: ProcessDesignFixture::CHAIN_ID,
            assembly_id: ProcessDesignFixture::ASSEMBLY_ID,
            part_base_1: ProcessDesignFixture::PART_BASE_1,
            part_base_2: ProcessDesignFixture::PART_BASE_2,
            part_no_serial: ProcessDesignFixture::PART_NO_SERIAL,
            part_chained: ProcessDesignFixture::PART_CHAINED,
            part_child: ProcessDesignFixture::PART_CHILD,
            part_assembly: ProcessDesignFixture::PART_ASSEMBLY,
            part_in_process: ProcessDesignFixture::PART_IN_PROCESS,
            part_programming: ProcessDesignFixture::PART_PROGRAMMING,
            part_completed: ProcessDesignFixture::PART_COMPLETED,
            part_cancelled: ProcessDesignFixture::PART_CANCELLED,
            part_soft_deleted: ProcessDesignFixture::PART_SOFT_DELETED,
            manager_username: ProcessDesignFixture::MANAGER_USERNAME.to_string(),
            clerk_username: ProcessDesignFixture::CLERK_USERNAME.to_string(),
            inspector_username: ProcessDesignFixture::INSPECTOR_USERNAME.to_string(),
            cnc_username: ProcessDesignFixture::CNC_USERNAME.to_string(),
            shelf_username: ProcessDesignFixture::SHELF_USERNAME.to_string(),
        }
    }
}

/// 加载 process_design fixture。
///
/// SQL 走 `include_str!` 编译期嵌入本 crate，路径相对本 crate
/// `CARGO_MANIFEST_DIR`（= `test-support/`），即 `fixtures/process_design.sql`。
///
/// SQL 内 INSERT 全部走常量 ID（不依赖运行时雪花 ID），跨测试并行 / 跨进程
/// 重跑都不会撞 ID。bcrypt 哈希预生成嵌入 SQL，省每测试 ~250ms 现场 hash 开销。
#[allow(dead_code)]
pub async fn load_process_design_fixture(pool: &PgPool) -> ProcessDesignFixture {
    let sql = include_str!("../../fixtures/process_design.sql");
    sqlx::raw_sql(sql)
        .execute(pool)
        .await
        .expect("load_process_design_fixture: insert fixture rows");
    ProcessDesignFixture::default()
}

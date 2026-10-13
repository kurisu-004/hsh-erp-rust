//! assembly 域业务逻辑门面
//!
//! 对应 Python myERP/service/assembly_service.py（及 _<d>_*.py helper）。
//!
//! ## 事务边界（2026-09-22 Group D-3 重构对齐 iam 范本）
//! 事务移交 handler（与 20 个 handler 文件现状对齐）：service 仅业务逻辑，所有
//! 跨 repo 操作经 `repo: R`（by-value；`R: AssemblyRepoTrait`）参数传入——handler/service
//! 借 `&mut *tx` / `&mut *conn` 喂给 trait（trait 已直接 `impl for &mut PgConnection`）。
//! service 不知事务——handler `pool.begin()` + `tx.commit()` 包外。
//!
//! `AssemblyService` 是 unit struct（无字段依赖，iam 范本 §6）；trait 注入式方法签名
//! `<R: AssemblyRepoTrait>(&self, mut repo: R, ...)`。
//!
//! ## 子模块拆分（2026-09-22，原 service.rs 1088 行超 1000 行上限）
//! - `crud.rs` —— 列表 / 详情 / 创建 / 更新 / 软删 / 取消（6 端点）
//! - `lifecycle.rs` —— start（状态机 PENDING → IN_PROCESS）/ files 上传
//! - `sync_from_part.rs` —— 被 part 反向触发的回调（`sync_from_part_change_by_id` /
//!   `sync_assembly_status`）
//!
//! ## 跨域调用（2026-09-22 D-3 决策）
//! 所有跨域 SQL（含 t_customer / t_part / t_part_file / t_part_batch）通过
//! `AssemblyRepoTrait` 跨域 helper 收口——service 不直接调 `PartRepo::xxx` /
//! `PartFileRepo::xxx` ZST 静态方法。
//!
//! 反向：part → assembly 由 `AssemblyService::sync_from_part_change_by_id`（mod.rs 内
//! ZST 静态入口）暴露，供 `shared::batch::status` 的 ZST 调用点使用。
//!
//! ## 兼容旧测试的 ZST 静态 wrapper（2026-09-22 D-3 决策）
//! 既存集成测试（`tests/assembly_api.rs` / `tests/assembly_files_api.rs` /
//! `tests/assembly_status_sync.rs` 等）以 `AssemblyService::create_assembly(&mut tx, ...)`
//! ZST 静态调用直调 service。本任务不修改测试代码（避免破坏契约），故本 mod.rs 暴露一组
//! ZST 静态 wrapper（接收 `&mut PgConnection`），内部一行委托真正的 trait 方法
//! `AssemblyService.xxx_trait(conn, ...)`（trait 注入式）。
//!
//! ## WS 广播（2026-09-22 D-3 决策）
//! 严格走 handler commit 后（`state.ws_hub.broadcast(WsEvent::DashboardEvent{...})`），
//! service 仅返回 "事件已发生" 标志（如 `AssemblyOut`）或不返回（写端点返回 unit）。
//! service 内不允许直接持有 `Arc<WsHub>`。

pub mod crud;
pub mod lifecycle;
pub mod sync_from_part;

use crate::auth::rbac::CurrentUser;
use crate::infra::snowflake::SnowflakeIdGenerator;
use crate::modules::assembly::dto::{
    AssemblyChildAddRequest, AssemblyCreateRequest, AssemblyListQuery, AssemblyUpdateRequest,
    ForceCompleteRequest,
};
use crate::modules::assembly::model::TAssembly;
use crate::modules::assembly::vo::{
    AssemblyCreateResult, AssemblyDetail, AssemblyListOut, AssemblyOut,
};
use crate::modules::part::vo::PartListItem;
use crate::shared::error::AppError;

/// `TAssembly` → `AssemblyOut`（详情 / 更新 / 取消 / start / force-complete 返回）。
///
/// 2026-10-13：原先 `crud.rs` 与 `lifecycle.rs` 各存一份**全字段逐字同形**的
/// `AssemblyOut` 副本，2026-09-22 D-3 给的理由是「保留两份以避免跨子模块
/// `pub(crate)` 暴露」—— 该理由
/// 在 Rust 里不成立（同 crate 内 `pub(crate)` 即可，且本函数本就只在同 crate 内被两个
/// 子模块调用）。两份副本改由本函数单点承载：VO 加/删字段只改一处，不会出现
/// 「crud 返新字段、lifecycle 返旧字段」这种静默分叉。
///
/// 2026-09-16 PR-2 瘦身：删 `actual_delivery_date` 字段。
pub(crate) fn render_assembly_out(asm: TAssembly) -> AssemblyOut {
    AssemblyOut {
        id: asm.id,
        drawing_no: asm.drawing_no,
        name: asm.name,
        applicant_name: asm.applicant_name,
        customer_id: asm.customer_id,
        request_date: asm.request_date,
        planned_delivery_date: asm.planned_delivery_date,
        is_urgent: asm.is_urgent,
        status: asm.status,
        version: asm.version,
        serial_no: asm.serial_no,
        quantity: asm.quantity,
        unit_price: asm.unit_price,
        total_price: asm.total_price,
        order_no: asm.order_no,
        system_delivery_date: asm.system_delivery_date,
        note: asm.note,
        created_at: asm.created_at,
        updated_at: asm.updated_at,
    }
}

/// 2026-10-13：`SyncOutcome` 的**定义**已上提到
/// `crate::shared::batch::status`（本模块的 `RollupOutcome` 与整条派生链都在
/// shared 那一层，而 `SyncOutcome` 表达的是「派生有没有真写库」，是派生链自身的
/// 返回类型）。此处保留 `pub use` 重导出，使
/// `crate::modules::assembly::service::SyncOutcome` 这条老路径继续可解析
/// （现存的调用点是 `admin::service` / `prod::scan::service::transfer` /
/// `tests/assembly/status_sync.rs`）。
pub use crate::shared::batch::status::SyncOutcome;

/// assembly 域业务门面。handler 经 `state.pool.begin()` 开 tx 后透传 `&mut tx`。
///
/// 2026-09-22 Group D-3 重构：unit struct 保持不变（无字段依赖）；trait 注入式方法签名
/// `<R: AssemblyRepoTrait>(&self, mut repo: R, ...)`，by-value 收 trait 对象。
///
/// 本 mod.rs 同时暴露 ZST 静态 wrapper（接收 `&mut PgConnection`），作为兼容旧测试的
/// 调用入口；handler 内部已全部改用 trait 注入式（`AssemblyService.xxx(&mut *tx, ...)`）。
pub struct AssemblyService;

// =============================================================================
// ZST 静态 wrapper —— 兼容旧测试（2026-09-22 D-3 决策）
// =============================================================================
//
// 既存集成测试以 `AssemblyService::create_assembly(&mut tx, ...)` 形式直调 service。
// trait 注入式新签名 `<R: AssemblyRepoTrait>(&self, mut repo: R, ...)` 要求 caller
// 写 `AssemblyService.xxx(&mut *tx, ...)`，与旧测试不兼容。本任务"不修改测试代码"，
// 故以下 ZST 静态 wrapper 保留旧签名，内部一行委托到 trait 方法。
//
// 2026-09-22 D-3 决策：保留 wrapper 而非把 service 改成旧签名，原因是新签名是 iam 范本
// 要求（`<R: AssemblyRepoTrait>`）。wrapper 仅 ~10 行 / 方法，不破坏任何未来 trait 注入。

impl AssemblyService {
    /// 兼容旧 ZST 静态调用：列表查询。
    pub async fn list_assemblies(
        conn: &mut sqlx::PgConnection,
        query: &AssemblyListQuery,
        current: &CurrentUser,
    ) -> Result<AssemblyListOut, AppError> {
        crud::list_assemblies_dispatch(conn, query, current).await
    }

    /// 兼容旧 ZST 静态调用：详情。
    pub async fn get_assembly(
        conn: &mut sqlx::PgConnection,
        assembly_id: i64,
        current: &CurrentUser,
    ) -> Result<AssemblyDetail, AppError> {
        crud::get_assembly_dispatch(conn, assembly_id, current).await
    }

    /// 兼容旧 ZST 静态调用：创建。
    pub async fn create_assembly(
        conn: &mut sqlx::PgConnection,
        snowflake: &SnowflakeIdGenerator,
        req: &AssemblyCreateRequest,
        pdf_files: Vec<Vec<u8>>,
        current: &CurrentUser,
    ) -> Result<AssemblyCreateResult, AppError> {
        crud::create_assembly_dispatch(conn, snowflake, req, pdf_files, current).await
    }

    /// 兼容旧 ZST 静态调用：更新。
    pub async fn update_assembly(
        conn: &mut sqlx::PgConnection,
        assembly_id: i64,
        req: &AssemblyUpdateRequest,
        current: &CurrentUser,
    ) -> Result<AssemblyOut, AppError> {
        crud::update_assembly_dispatch(conn, assembly_id, req, current).await
    }

    /// 兼容旧 ZST 静态调用：软删。
    pub async fn soft_delete_assembly(
        conn: &mut sqlx::PgConnection,
        assembly_id: i64,
        expected_version: i32,
        current: &CurrentUser,
    ) -> Result<(), AppError> {
        crud::soft_delete_assembly_dispatch(conn, assembly_id, expected_version, current).await
    }

    /// 兼容旧 ZST 静态调用：取消。
    pub async fn cancel_assembly(
        conn: &mut sqlx::PgConnection,
        assembly_id: i64,
        current: &CurrentUser,
    ) -> Result<AssemblyOut, AppError> {
        crud::cancel_assembly_dispatch(conn, assembly_id, current).await
    }

    /// 兼容旧 ZST 静态调用：start。
    pub async fn start_assembly(
        conn: &mut sqlx::PgConnection,
        assembly_id: i64,
        current: &CurrentUser,
    ) -> Result<AssemblyOut, AppError> {
        lifecycle::start_assembly_dispatch(conn, assembly_id, current).await
    }

    /// 2026-09-25 新增：单 part 子件追加（`POST /assemblies/{id}/children`）。
    pub async fn add_assembly_child(
        conn: &mut sqlx::PgConnection,
        snowflake: &SnowflakeIdGenerator,
        assembly_id: i64,
        req: &AssemblyChildAddRequest,
        current: &CurrentUser,
    ) -> Result<PartListItem, AppError> {
        crud::add_assembly_child_dispatch(conn, snowflake, assembly_id, req, current).await
    }

    /// 2026-10-01 新增：只收 `updated_by` 的单 part → assembly sync 钩子。
    ///
    /// **唯一**调用方是 `shared::batch::status`（batch → part → assembly 单一写
    /// 入口）—— 该链路上只有 `updated_by`，没有登录用户上下文。
    ///
    /// 2026-10-13：原先还有一个收 `&CurrentUser` 的 `sync_from_part_change` 与一个
    /// 批量变体 `sync_from_part_changes`，生产零调用（派生链自 2026-10-01 起就只走
    /// 本方法），唯一真实调用方是集成测试，已随死代码清理一并删除。理由与影响见
    /// [`sync_from_part`] 的模块 doc「2026-10-13 死代码清理」一节。
    pub async fn sync_from_part_change_by_id(
        conn: &mut sqlx::PgConnection,
        part_id: i64,
        updated_by: i64,
    ) -> Result<SyncOutcome, AppError> {
        sync_from_part::sync_from_part_change_by_id(&Self {}, conn, part_id, updated_by).await
    }

    /// 2026-10-11 新增：装配件级强制完成
    /// （`POST /api/v2/prod/assemblies/{assembly_id}/force-complete`）。
    ///
    /// 返回 `(AssemblyOut, 被改动的子件 id 列表)`：handler 用后者在 commit 之后逐个
    /// 广播 `PART_FORCE_COMPLETED`，响应体取前者。
    pub async fn force_complete(
        conn: &mut sqlx::PgConnection,
        snowflake: &SnowflakeIdGenerator,
        assembly_id: i64,
        req: &ForceCompleteRequest,
        current: &CurrentUser,
    ) -> Result<(AssemblyOut, Vec<i64>), AppError> {
        AssemblyService
            .force_complete_inner(conn, snowflake, assembly_id, req.clone(), current)
            .await
    }
}

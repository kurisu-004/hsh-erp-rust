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
//! - `sync_from_part.rs` —— 被 part 反向触发的回调（`sync_from_part_change[es]` /
//!   `sync_assembly_status`）
//!
//! ## 跨域调用（2026-09-22 D-3 决策）
//! 所有跨域 SQL（含 t_customer / t_part / t_part_file / t_part_batch）通过
//! `AssemblyRepoTrait` 跨域 helper 收口——service 不直接调 `PartRepo::xxx` /
//! `PartFileRepo::xxx` ZST 静态方法。
//!
//! 反向：part → assembly 由 `AssemblyService::sync_from_part_change`（mod.rs 内
//! ZST 静态入口）暴露，兼容 `part/service/rollup.rs:140` 的 ZST 调用点。
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

use std::sync::Arc;

use crate::auth::rbac::CurrentUser;
use crate::infra::cos::CosClient;
use crate::infra::snowflake::SnowflakeIdGenerator;
use crate::modules::assembly::dto::{
    AssemblyCreateRequest, AssemblyCreateResult, AssemblyDetail, AssemblyListOut,
    AssemblyListQuery, AssemblyOut, AssemblyUpdateRequest,
};
use crate::shared::error::AppError;

/// Sync hook result（service::sync_from_part 回调返回；handler 据此发 ASSEMBLY_UPDATED）。
///
/// ## 变体
/// - `NoChange` —— 父装配件已为终态 / 无子件 / 目标 == 当前 → 不写库；handler 不广播。
/// - `Changed(assembly_id)` —— 实际更新了 t_assembly.status；handler 发 ASSEMBLY_UPDATED。
///
/// 设计意图：把"是否需要广播"的判断推给 handler，service 只表达"是否实际写库"。
/// 2026-09-22 D-3 与 `SyncOutcome` 命名统一（part/service/rollup.rs 也有同名 enum）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncOutcome {
    /// 父装配件已为终态 / 无子件 / 目标 == 当前 → 不写库
    NoChange,
    /// 实际更新了 t_assembly.status；handler 据此发 ASSEMBLY_UPDATED
    Changed(i64),
}

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

    /// 兼容旧 ZST 静态调用：files 上传。
    pub async fn upload_assembly_files(
        conn: &mut sqlx::PgConnection,
        snowflake: &SnowflakeIdGenerator,
        cos: Arc<dyn CosClient>,
        assembly_id: i64,
        files: Vec<(Vec<u8>, String, String)>,
        current: &CurrentUser,
    ) -> Result<Vec<crate::modules::assembly::dto::AssemblyFileRef>, AppError> {
        lifecycle::upload_assembly_files_dispatch(conn, snowflake, cos, assembly_id, files, current)
            .await
    }

    /// 兼容旧 ZST 静态调用：单 part → assembly sync 钩子。
    ///
    /// `part/service/rollup.rs:140` 仍走 ZST 静态调用。详见 [`sync_from_part::sync_from_part_change`]。
    pub async fn sync_from_part_change(
        conn: &mut sqlx::PgConnection,
        part_id: i64,
        current: &CurrentUser,
    ) -> Result<SyncOutcome, AppError> {
        sync_from_part::sync_from_part_change(&Self {}, conn, part_id, current).await
    }

    /// 兼容旧 ZST 静态调用：批量 part → assembly sync 钩子。
    pub async fn sync_from_part_changes(
        conn: &mut sqlx::PgConnection,
        part_ids: &[i64],
        current: &CurrentUser,
    ) -> Result<Vec<SyncOutcome>, AppError> {
        sync_from_part::sync_from_part_changes_dispatch(conn, part_ids, current).await
    }
}

//! assembly 域 part → assembly 反向同步 hook（2026-09-22 Group D-3 拆自原 service.rs）
//!
//! 当 part 状态变化时（worker_scan / pick-up / to-* / complete / 返修闭环 / 报工），
//! `PartService::sync_from_batch_change` 在同一事务内调用
//! `AssemblyService::sync_from_part_change(conn, part_id, current)` 闭合
//! batch → part → assembly 链路。本文件承载该反向钩子的 3 个方法。
//!
//! ## 核心流程（per-assembly）
//! 1. 反查 `part.assembly_id`（None → NoChange）
//! 2. 父已是 COMPLETED/CANCELLED → NoChange（Python 短路 L92）
//! 3. 拉子件 status → `compute_assembly_target` → Some(target)
//! 4. 取父当前 version + status；target == 当前 → NoChange
//! 5. `update_status_if_not_terminal`；0 行 → VERSION_CONFLICT（事务回滚）
//! 6. 返回 `Changed(assembly_id)` —— handler 据此发 ASSEMBLY_UPDATED
//!
//! ## 事务边界（2026-09-22 重构对齐 iam 范本）
//! 事务移交 caller（part/service/rollup.rs 在自己的事务里调本方法）。service 不知事务——
//! 通过 `repo: R: AssemblyRepoTrait` by-value 收 trait，借 `&mut *conn` 喂连接。
//!
//! ## 与 `mod.rs::sync_from_part_change` 静态 wrapper 的关系
//! 2026-09-22 D-3 决策：本模块暴露 `pub async fn sync_from_part_change(self, ...)`（收
//! `&self` + `repo: R`）；`mod.rs::AssemblyService::sync_from_part_change` 是 ZST 静态
//! 入口（兼容 `part/service/rollup.rs:140` 的旧调用点），内部一行委托本函数。
//! 这样既保留 trait 注入式新路径（单测用），又不破坏生产跨模块 ZST 调用。

use crate::auth::rbac::CurrentUser;
use crate::modules::assembly::repo::AssemblyRepoTrait;
use crate::modules::assembly::statemachine::{AssemblyStatus, compute_assembly_target};
use crate::shared::error::{AppError, code};

use super::{AssemblyService, SyncOutcome};

impl AssemblyService {
    /// 从单个 part 的状态变更回流到父装配件（同事务调用）。
    /// 1. 反查 `part.assembly_id`（None → NoChange）
    /// 2. 父已是 COMPLETED/CANCELLED → NoChange（Python 短路 L92）
    /// 3. 拉子件 status → `compute_assembly_target` → Some(target)
    /// 4. 取父当前 version + status；target == 当前 → NoChange
    /// 5. `update_status_if_not_terminal`；0 行 → VERSION_CONFLICT（事务回滚）
    /// 6. 返回 `Changed(assembly_id)`
    ///
    /// 2026-09-22 D-3 决策：方法签名为 `(&self, repo: R, part_id, current)`；
    /// service 不再持 `&mut PgConnection`，所有 SQL 经 `repo` 调用。`SyncOutcome::Changed(assembly_id)`
    /// 由 handler 据此发 WS 广播。
    pub async fn sync_from_part_change_inner<R: AssemblyRepoTrait>(
        &self,
        mut repo: R,
        part_id: i64,
        current: &CurrentUser,
    ) -> Result<SyncOutcome, AppError> {
        // 反查 part 的 assembly_id（跨域 SQL 也走 trait 收口：`fetch_part_assembly_id`）。
        let row = repo.fetch_part_assembly_id(part_id).await?;
        let Some(Some(assembly_id)) = row else {
            return Ok(SyncOutcome::NoChange);
        };
        sync_assembly_status(&mut repo, assembly_id, current).await
    }

    /// 批量版本：传入本次批量成功的 part_id 列表；
    /// 用单条 SQL `SELECT DISTINCT assembly_id` 去重，再逐个 sync。
    pub async fn sync_from_part_changes_inner<R: AssemblyRepoTrait>(
        &self,
        mut repo: R,
        part_ids: &[i64],
        current: &CurrentUser,
    ) -> Result<Vec<SyncOutcome>, AppError> {
        if part_ids.is_empty() {
            return Ok(Vec::new());
        }
        let assembly_ids = repo
            .fetch_distinct_assembly_ids_by_part_ids(part_ids)
            .await
            .map_err(AppError::from)?;
        let mut out = Vec::with_capacity(assembly_ids.len());
        for aid in assembly_ids {
            out.push(sync_assembly_status(&mut repo, aid, current).await?);
        }
        Ok(out)
    }
}

/// 实际聚合 + 翻转的核心；`sync_from_part_change_inner` / `sync_from_part_changes` 共用。
///
/// 2026-09-22 D-3 决策：保持 module-private 自由函数（不挂在 `impl AssemblyService`），避免
/// 跨 impl 块调用路径复杂化；签名与 `crud.rs::fetch_current_batch_ids` 同形。
async fn sync_assembly_status<R: AssemblyRepoTrait>(
    repo: &mut R,
    assembly_id: i64,
    current: &CurrentUser,
) -> Result<SyncOutcome, AppError> {
    // 父存在性 + 终态短路
    let asm = repo
        .get_by_id(assembly_id, false)
        .await
        .map_err(AppError::from)?
        .ok_or_else(|| {
            AppError::biz(
                code::BIZ_ASSEMBLY_NOT_FOUND,
                format!("assembly {assembly_id} 不存在"),
            )
        })?;
    let current_status = AssemblyStatus::from_str(&asm.status).ok_or_else(|| {
        AppError::biz(
            code::BIZ_INVALID_VALUE,
            format!("未知 assembly status: {}", asm.status),
        )
    })?;
    if matches!(
        current_status,
        AssemblyStatus::COMPLETED | AssemblyStatus::CANCELLED
    ) {
        return Ok(SyncOutcome::NoChange);
    }

    // 聚合子件
    let children_statuses = repo
        .aggregate_children_status(assembly_id)
        .await
        .map_err(AppError::from)?;
    let Some(target) = compute_assembly_target(children_statuses.iter().map(|s| s.as_str()))
    else {
        return Ok(SyncOutcome::NoChange);
    };

    // target == current → NoChange
    if target == current_status {
        return Ok(SyncOutcome::NoChange);
    }

    // OCC 翻转
    let affected = repo
        .update_status_if_not_terminal(assembly_id, asm.version, target.as_str(), current.id)
        .await
        .map_err(AppError::from)?;
    if affected == 0 {
        return Err(AppError::biz(
            code::VERSION_CONFLICT,
            format!(
                "assembly {assembly_id} version {} 已变化或已终态",
                asm.version
            ),
        ));
    }
    Ok(SyncOutcome::Changed(assembly_id))
}

// ---------- ZST 静态入口的兼容 wrapper ----------
//
// 2026-09-22 D-3 决策：`mod.rs::AssemblyService::sync_from_part_change` 是 ZST 静态入口
// （兼容 `part/service/rollup.rs:140` 的旧调用点），内部一行委托本文件的
// `sync_from_part_change_inner`。本文件暴露 `pub async fn sync_from_part_change`
// 作为 `mod.rs` wrapper 的直接实现点（避免 mod.rs 仅做 4 行 wrapper 而失去内聚性）。
pub async fn sync_from_part_change(
    self_svc: &AssemblyService,
    conn: &mut sqlx::PgConnection,
    part_id: i64,
    current: &CurrentUser,
) -> Result<SyncOutcome, AppError> {
    // 生产路径：R = &mut PgConnection
    self_svc
        .sync_from_part_change_inner::<&mut sqlx::PgConnection>(conn, part_id, current)
        .await
}

/// 兼容旧 ZST 静态调用（`AssemblyService::sync_from_part_changes(&mut tx, ...)`）。
///
/// 详见 [`AssemblyService::sync_from_part_changes`]（impl 块定义）。
pub(crate) async fn sync_from_part_changes_dispatch(
    conn: &mut sqlx::PgConnection,
    part_ids: &[i64],
    current: &CurrentUser,
) -> Result<Vec<SyncOutcome>, AppError> {
    AssemblyService
        .sync_from_part_changes_inner::<&mut sqlx::PgConnection>(conn, part_ids, current)
        .await
}

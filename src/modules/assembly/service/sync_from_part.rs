//! assembly 域 part → assembly 反向同步 hook（2026-09-22 Group D-3 拆自原 service.rs）
//!
//! 当 part 状态变化时（worker_scan / pick-up / to-* / complete / 返修闭环 / 报工），
//! `PartService::sync_from_batch_change` 在同一事务内调用
//! `AssemblyService::sync_from_part_change(conn, part_id, current)` 闭合
//! batch → part → assembly 链路。本文件承载该反向钩子的 3 个 `_inner` 方法与
//! 4 个 ZST 静态 wrapper；聚合 + 翻转的**实体实现**在
//! `crate::shared::batch::status::sync_assembly_status`（2026-10-13 自本文件搬走）。
//!
//! ## 核心流程（per-assembly，实体实现在 `shared::batch::status`）
//! 1. 反查 `part.assembly_id`（None → NoChange）—— **本文件**的 `_inner` 做
//! 2. 父已是 COMPLETED/CANCELLED → NoChange（Python 短路 L92）—— shared
//! 3. 拉子件 status → `compute_assembly_target` → Some(target) —— shared
//! 4. 取父当前 version + status；target == 当前 → NoChange —— shared
//! 5. `update_status_if_not_terminal`；0 行 → **降级为 `NoChange` + `tracing::warn!`**
//!    （**派生层不得否决主操作**，2026-10-01 review 第 1 轮）—— shared
//! 6. 返回 `Changed(assembly_id)` —— handler 据此发 ASSEMBLY_UPDATED —— shared
//!
//! ⚠️ 第 5 步在 2026-10-01 之前是 `Err(VERSION_CONFLICT)`（事务回滚）。那会把用户
//! 真正请求的批次状态变更一起回滚、还返回一个指向无关实体的 409。现在只 warn。
//!
//! ## 事务边界（2026-09-22 重构对齐 iam 范本）
//! 事务移交 caller（part/service/rollup.rs 在自己的事务里调本方法）。service 不知事务——
//! 通过 `repo: R: AssemblyRepoTrait` by-value 收 trait，借 `&mut *conn` 喂连接。
//!
//! ## 与 `mod.rs::sync_from_part_change` 静态 wrapper 的关系
//! 2026-09-22 D-3 决策：本模块暴露 `pub async fn sync_from_part_change(self, ...)`（收
//! `&self` + `repo: R`）；`mod.rs::AssemblyService::sync_from_part_change` 是 ZST 静态
//! 入口（供 `shared::batch::status` 的 ZST 调用点使用），内部一行委托本函数。
//! 这样既保留 trait 注入式新路径（单测用），又不破坏生产跨模块 ZST 调用。

use crate::auth::rbac::CurrentUser;
use crate::modules::assembly::repo::AssemblyRepoTrait;
use crate::shared::batch::status::sync_assembly_status;
use crate::shared::error::AppError;

use super::{AssemblyService, SyncOutcome};

impl AssemblyService {
    /// 从单个 part 的状态变更回流到父装配件（同事务调用）。
    /// 1. 反查 `part.assembly_id`（None → NoChange）
    /// 2. 父已是 COMPLETED/CANCELLED → NoChange（Python 短路 L92）
    /// 3. 拉子件 status → `compute_assembly_target` → Some(target)
    /// 4. 取父当前 version + status；target == 当前 → NoChange
    /// 5. `update_status_if_not_terminal`；0 行 → **降级为 `NoChange`**（不抛错、
    ///    不回滚，2026-10-01，见下方「派生层冲突降级」段）
    /// 6. 返回 `Changed(assembly_id)`
    ///
    /// 2026-09-22 D-3 决策：方法签名为 `(&self, repo: R, part_id, current)`；
    /// service 不再持 `&mut PgConnection`，所有 SQL 经 `repo` 调用。`SyncOutcome::Changed(assembly_id)`
    /// 由 handler 据此发 WS 广播。
    ///
    /// 2026-10-01：本函数只把 `current.id` 往下传（见 `sync_assembly_status` 签名），
    /// 形参保留 `&CurrentUser` 是为了不打穿 20+ 个调用点。
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
        sync_assembly_status(&mut repo, assembly_id, current.id).await
    }

    /// 2026-10-01 新增：只收 `updated_by` 的反向同步入口（无登录用户上下文）。
    ///
    /// 存在的理由：新的 batch → part → assembly 单一写入口
    /// （`shared::batch::status`）在整条派生链路上只有 `updated_by`（一个 i64），
    /// 没有完整的 `CurrentUser`。本函数把 `sync_assembly_status` 直接暴露出来，
    /// 而**不是**在 shared::batch::status 里伪造一个 `CurrentUser`（伪造身份迟早会被
    /// 下游的 `username` / `roles` 依赖带出真实 bug）。
    pub async fn sync_from_part_change_by_id_inner<R: AssemblyRepoTrait>(
        &self,
        mut repo: R,
        part_id: i64,
        updated_by: i64,
    ) -> Result<SyncOutcome, AppError> {
        let row = repo.fetch_part_assembly_id(part_id).await?;
        let Some(Some(assembly_id)) = row else {
            return Ok(SyncOutcome::NoChange);
        };
        sync_assembly_status(&mut repo, assembly_id, updated_by).await
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
            out.push(sync_assembly_status(&mut repo, aid, current.id).await?);
        }
        Ok(out)
    }
}

// ⚠️ 2026-10-13：聚合 + 翻转的**实体实现**已搬到
// `crate::shared::batch::status::sync_assembly_status`（派生链最后一跳，与
// `rollup_part_derived` / `rollup_assembly_derived` 同属一条链、放在同一层）。
// 本文件保留 4 个 wrapper（`sync_from_part_change` / `sync_from_part_changes` /
// `sync_from_part_change_by_id` / `recompute_assembly_status_by_id`）与其 3 个
// `_inner` 方法，它们只做「反查 assembly_id → 调 shared 那份」。
//
// 保留 wrapper 而不是让 shared 反过来调这里的原因：本文件的入口签名收
// `&CurrentUser`，而 `shared::batch::status` 的派生链手上只有 `updated_by`
// （一个 i64）—— 在 shared 里伪造 `CurrentUser` 迟早会被下游的
// `username` / `roles` 依赖带出真实 bug。

// ---------- ZST 静态入口的兼容 wrapper ----------
//
// 2026-09-22 D-3 决策：`mod.rs::AssemblyService::sync_from_part_change` 是 ZST 静态入口
// （供 `shared::batch::status` 的 ZST 调用点使用），内部一行委托本文件的
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

/// 2026-10-01 新增：只收 `updated_by` 的 ZST 静态入口（`shared::batch::status` 用）。
///
/// 见 `AssemblyService::sync_from_part_change_by_id_inner` 的 rationale。
pub async fn sync_from_part_change_by_id(
    self_svc: &AssemblyService,
    conn: &mut sqlx::PgConnection,
    part_id: i64,
    updated_by: i64,
) -> Result<SyncOutcome, AppError> {
    self_svc
        .sync_from_part_change_by_id_inner::<&mut sqlx::PgConnection>(conn, part_id, updated_by)
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

/// 2026-10-01 新增：按 `assembly_id` 直接重算一次聚合状态（admin 对账逃生口用）。
///
/// 暴露（自 2026-10-13 起在 `shared::batch::status` 的）[`sync_assembly_status`]
/// 的唯一理由是
/// `POST /api/v2/admin/recompute-rollup` 需要「**从装配件出发**」跑一次聚合 ——
/// 正常业务流永远是从 part 出发的（`sync_from_part_change*`），而对账场景要修的
/// 恰恰是「part 侧派生已正确、但父装配件没跟上」这种反向漂移。
///
/// 仍然走同一段聚合实现（`compute_assembly_target` + OCC + 终态守卫），因此
/// 对账端点与业务流**不可能**算出两个结果。
pub async fn recompute_assembly_status_by_id(
    conn: &mut sqlx::PgConnection,
    assembly_id: i64,
    updated_by: i64,
) -> Result<SyncOutcome, AppError> {
    // `sync_assembly_status` 收 `&mut R`（trait 注入式），生产路径的 R 是
    // `&mut PgConnection`；这里借用一次以凑出那个 `&mut R` 借位。
    let mut repo: &mut sqlx::PgConnection = conn;
    sync_assembly_status::<&mut sqlx::PgConnection>(&mut repo, assembly_id, updated_by).await
}

//! assembly 域 part → assembly 反向同步 hook（2026-09-22 Group D-3 拆自原 service.rs）
//!
//! 当 part 状态变化时（worker_scan / pick-up / to-* / complete / 返修闭环 / 报工），
//! `shared::batch::status` 的 batch → part → assembly 派生链在同一事务内调用
//! `AssemblyService::sync_from_part_change_by_id(conn, part_id, updated_by)` 闭合
//! batch → part → assembly 链路。本文件承载该反向钩子的 1 个 `_inner` 方法与
//! 2 个 ZST 静态 wrapper；聚合 + 翻转的**实体实现**在
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
//! 事务移交 caller（`shared::batch::status` 在自己的事务里调本方法）。service 不知
//! 事务——通过 `repo: R: AssemblyRepoTrait` by-value 收 trait，借 `&mut *conn` 喂连接。
//!
//! ## 2026-10-13 死代码清理：只留 `_by_id` 一条腿
//!
//! 本文件原先有 4 个入口：`sync_from_part_change`（收 `&CurrentUser`）/
//! `sync_from_part_changes`（批量）/ 各自的 `_inner`，以及两个 `_by_id` 变体。
//! 复核结论：**生产的派生链自 2026-10-01 起只走 `sync_from_part_change_by_id`**
//! （`shared::batch::status` 那条链上只有 `updated_by` 一个 i64，没有
//! `CurrentUser`）。收 `&CurrentUser` 的那两个变体生产零调用，唯一真实调用方是
//! `tests/assembly/status_sync.rs`，而那两个用例守的算法（`ALL_CANCELLED → CANCELLED`
//! 分支、批量去重）已由「`sync_from_part_change_by_id` 直调 + mock 单测」覆盖。
//! 故一并删除，**不留**「兼容旧 ZST 调用」的壳 —— 仓内没有第二个调用方。
//!
//! 连带删掉 `AssemblyRepoTrait::fetch_distinct_assembly_ids_by_part_ids`（批量去重
//! 查询）：它唯一的调用点就是被删的 `sync_from_part_changes_inner`。

use crate::modules::assembly::repo::AssemblyRepoTrait;
use crate::shared::batch::status::sync_assembly_status;
use crate::shared::error::AppError;

use super::{AssemblyService, SyncOutcome};

impl AssemblyService {
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
}

// ⚠️ 2026-10-13：聚合 + 翻转的**实体实现**已搬到
// `crate::shared::batch::status::sync_assembly_status`（派生链最后一跳，与
// `rollup_part_derived` / `rollup_assembly_derived` 同属一条链、放在同一层）。
// 本文件保留 1 个 wrapper（`sync_from_part_change_by_id`）与它的 1 个 `_inner`
// 方法，它们只做「反查 assembly_id → 调 shared 那份」。
//
// 保留 wrapper 而不是让 shared 反过来调这里的原因：本文件历史的入口签名收
// `&CurrentUser`，而 `shared::batch::status` 的派生链手上只有 `updated_by`
// （一个 i64）—— 在 shared 里伪造 `CurrentUser` 迟早会被下游的
// `username` / `roles` 依赖带出真实 bug。
//
// 2026-10-13 随 admin 域下线一并删除的还有 `recompute_assembly_status_by_id`
// ——「从装配件出发反查一次聚合」的逃生入口，它的唯一调用方就是
// `POST /api/v2/admin/recompute-rollup`。算法本体（`sync_assembly_status`）
// **完全不动**、仍由 `sync_from_part_change_by_id` 走；消失的只是那个反向入口。

// ---------- ZST 静态入口的兼容 wrapper ----------
//
// 2026-09-22 D-3 决策：`mod.rs::AssemblyService::sync_from_part_change_by_id` 是 ZST
// 静态入口，内部一行委托本文件的 `sync_from_part_change_by_id_inner`。本文件暴露
// `pub async fn sync_from_part_change_by_id` 作为 mod.rs wrapper 的直接实现点
// （避免 mod.rs 仅做 4 行 wrapper 而失去内聚性）。
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

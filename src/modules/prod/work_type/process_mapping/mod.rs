//! work_type ↔ process 映射（`t_work_type_process`）子模块
//!
//! 7 个 work_type 端点中，mapping 相关 2 个端点（`GET /work-types/{id}/processes` /
//! `POST /work-types/{id}/processes`）的本域职责抽到本目录，避免 `service.rs` 超过 1000 行
//! 硬上限（conventions.md §2）。
//!
//! ## 子模块结构（2026-09-22 D-2-simple 重构）
//! - `sql.rs`：`t_work_type_process` SQL 真源（ZST struct `WorkTypeProcessRepo` + 4
//!   固有静态方法）+ `NewWorkTypeProcessRow` 输入结构。SQL 与方法签名零 diff。
//! - `mod.rs`（本文件）：`WorkTypeProcessService`（2 个 service 方法）+ 重导出
//!   `WorkTypeProcessRepo` / `NewWorkTypeProcessRow`（保留旧调用路径）。
//!
//! ## 与 `work_type/repo` 的协作
//! `t_work_type_process` 的 4 个方法**也**声明在胖 trait `WorkTypeRepoTrait`
//! （`crate::modules::prod::work_type::repo::WorkTypeRepoTrait`），trait impl 一行
//! 委托到本文件的 `WorkTypeProcessRepo` 静态方法。决策方案 A（推荐，与 shelf 同形）
//! —— 单一胖 trait，单 service 签名 `<R: WorkTypeRepoTrait>`，单 mock，
//! handler 借 `&mut *tx` 一次喂给 service。
//!
//! 不沿用「独立 `WorkTypeProcessRepoTrait` + service 收双 trait」方案的原因：
//! 跨模块调用方（同任务未覆盖域）暂未出现需要 process_mapping trait helper 的场景；
//! 双 trait 让 service 收 `<R, M>` 两个泛型 + handler 需两次 `&mut *tx` reborrow，
//! 在 `&mut PgConnection` 同一作用域只能借一次的语义下会撞借用窗口。
//! 后续若需拆分，按需再开 trait 即可。
//!
//! ## 整组替换语义
//! `set_work_type_processes` 是「整组替换」：先软删该 work_type 的全部旧 mapping，再
//! INSERT 新列表；事务由 caller 保证（handler 层 `state.pool.begin()` + commit）。
//!
//! ## 错误码
//! - 20901 `BIZ_WORK_TYPE_NOT_FOUND`
//! - 20801 `BIZ_PROCESS_NOT_FOUND` —— items 里有 process_id 不存在
//! - 20902 `BIZ_WORK_TYPE_DUPLICATE_CODE` —— uk_t_work_type_process 撞（理论不该发生，service 已去重）

use crate::auth::rbac::CurrentUser;
use crate::infra::snowflake::SnowflakeIdGenerator;
use crate::modules::prod::work_type::vo::{WorkTypeProcessMappingItem, WorkTypeProcessMappingOut};
use crate::modules::prod::work_type::repo::WorkTypeRepoTrait;
use crate::shared::error::{AppError, code};

pub mod sql;

// 重导出 sql.rs 中的 ZST struct + 输入结构，让旧调用路径
// `use crate::modules::prod::work_type::process_mapping::WorkTypeProcessRepo;`
// 继续可用。
pub use sql::{NewWorkTypeProcessRow, WorkTypeProcessRepo};

// ===========================================================================
// WorkTypeProcessService
// ===========================================================================

pub struct WorkTypeProcessService;

impl WorkTypeProcessService {
    /// 设置指定 work_type 的工序映射 —— **整组替换**语义：
    ///
    /// 1. 校验 work_type 存在（`repo.get_by_id`）
    /// 2. 校验 items 内的所有 process_id 存在（`repo.process_list_by_ids` 跨域 helper）
    /// 3. 软删该 work_type 的全部旧 mapping（`repo.worktypeproc_soft_delete_all_for_work_type`）
    /// 4. INSERT 新 mapping（`repo.worktypeproc_bulk_insert`，按 sort_order）
    ///
    /// 整组事务由 caller 保证（handler 层 `state.pool.begin()` + commit）。
    ///
    /// 错误码：
    /// - 20901 `BIZ_WORK_TYPE_NOT_FOUND`
    /// - 20801 `BIZ_PROCESS_NOT_FOUND` —— items 里有 process_id 不存在
    #[allow(clippy::too_many_arguments)]
    pub async fn set_work_type_processes<R: WorkTypeRepoTrait>(
        &self,
        mut repo: R,
        snowflake: &SnowflakeIdGenerator,
        work_type_id: i64,
        items: &[crate::modules::prod::work_type::dto::SetWorkTypeProcessesItem],
        current: &CurrentUser,
    ) -> Result<(), AppError> {
        // 1. work_type 存在性 + 软删校验（已软删 → 404）
        let wt = repo
            .get_by_id(work_type_id)
            .await?
            .ok_or_else(|| {
                AppError::biz(
                    code::BIZ_WORK_TYPE_NOT_FOUND,
                    format!("work_type {work_type_id} 不存在"),
                )
            })?;

        // 2. 解析 + 校验所有 process_id 存在
        let mut process_ids: Vec<i64> = Vec::with_capacity(items.len());
        for it in items {
            let pid = it.process_id.parse::<i64>().map_err(|_| {
                AppError::biz(code::BIZ_INVALID_VALUE, "process_id 必须为雪花 ID 字符串")
            })?;
            process_ids.push(pid);
        }
        if !process_ids.is_empty() {
            // 一次性批量查 process —— 防 N+1（跨域 helper 内部走 prod ProcessRepo）
            let existing = repo.process_list_by_ids(&process_ids).await?;
            if existing.len() != process_ids.len() {
                // 找出缺失的 id（用 Vec 差集；批量小，开销可忽略）
                let existing_ids: std::collections::HashSet<i64> =
                    existing.iter().map(|p| p.id).collect();
                let missing: Vec<i64> = process_ids
                    .iter()
                    .filter(|p| !existing_ids.contains(p))
                    .copied()
                    .collect();
                return Err(AppError::biz(
                    code::BIZ_PROCESS_NOT_FOUND,
                    format!("process 不存在: {:?}", missing),
                ));
            }
        }

        // 3. 软删旧 mapping（事务内）
        repo.worktypeproc_soft_delete_all_for_work_type(work_type_id)
            .await?;

        // 4. 批量 INSERT 新 mapping（空 items = 清空映射；无行写）
        let new_rows: Vec<NewWorkTypeProcessRow> = items
            .iter()
            .zip(process_ids.iter())
            .map(|(it, &pid)| NewWorkTypeProcessRow {
                work_type_id: wt.id,
                process_id: pid,
                sort_order: it.sort_order,
            })
            .collect();
        repo.worktypeproc_bulk_insert(&new_rows, snowflake, current.id)
            .await?;

        Ok(())
    }

    /// 列出指定 work_type 的所有 active mapping（按 sort_order ASC）。
    pub async fn list_work_type_processes<R: WorkTypeRepoTrait>(
        &self,
        mut repo: R,
        work_type_id: i64,
        current: &CurrentUser,
    ) -> Result<WorkTypeProcessMappingOut, AppError> {
        // 权限：与 list_work_types 一致（任意已登录）
        current.require_any_role(&[
            crate::auth::rbac::Role::Manager,
            crate::auth::rbac::Role::Clerk,
            crate::auth::rbac::Role::CncProgrammer,
            crate::auth::rbac::Role::ShelfAccount,
            crate::auth::rbac::Role::Inspector,
        ])?;

        // work_type 存在性校验
        let wt = repo
            .get_by_id(work_type_id)
            .await?
            .ok_or_else(|| {
                AppError::biz(
                    code::BIZ_WORK_TYPE_NOT_FOUND,
                    format!("work_type {work_type_id} 不存在"),
                )
            })?;

        let rows = repo.worktypeproc_list_by_work_type(wt.id).await?;
        let items = rows
            .into_iter()
            .map(
                |(pid, sort_order, process_code)| WorkTypeProcessMappingItem {
                    work_type_id: wt.id,
                    process_id: pid,
                    process_code,
                    sort_order,
                },
            )
            .collect();
        Ok(WorkTypeProcessMappingOut { items })
    }
}

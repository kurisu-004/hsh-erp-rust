//! shelf ↔ process 映射（`t_shelf_process`）子模块
//!
//! 11 个 shelf 端点中，mapping 相关 3 个端点（`GET /shelves/{id}/processes` /
//! `POST /shelves/{id}/processes` / `GET /shelves/processes`）的本域职责
//! 抽到本文件，避免主 service 超过 1000 行硬上限（conventions.md §2）。
//!
//! ## 子模块结构（2026-09-22 重构）
//! - `sql.rs`：`t_shelf_process` SQL 真源（ZST struct `ShelfProcessRepo` + 4
//!   固有静态方法）+ `NewShelfProcessRow` 输入结构。SQL 与方法签名零 diff。
//! - `mod.rs`（本文件）：`ShelfProcessService`（2 个 service 方法）+ `ShelfProcessRepo`
//!   与 `NewShelfProcessRow` 的 re-export（让外部继续 `use crate::modules::shelf::process_mapping::ShelfProcessRepo;`）。
//!
//! ## 与 `shelf/repo` 的协作
//! `ShelfProcessService` 通过胖 trait `ShelfRepoTrait`（`crate::modules::shelf::repo::ShelfRepoTrait`）
//! 调 `t_shelf` 的方法（`get_by_id`），并通过胖 trait 的 `proc_*` 系列方法调
//! `t_shelf_process` 的方法（trait impl 一行委托到 `sql::ShelfProcessRepo`）。
//! 跨域校验 `process_id` 是否存在走 trait helper `proc_list_existing_process_ids`。
//! 决策方案 A（推荐）—— 单一胖 trait，单 service 签名 `<R: ShelfRepoTrait>`，单 mock。
//!
//! ## 整组替换语义
//! `set_shelf_processes` 是「整组替换」：先软删该 shelf 的全部旧 mapping，再
//! INSERT 新列表；事务由 caller 保证（handler 层 `state.pool.begin()` + commit）。
//!
//! ## 错误码
//! - 20501 `BIZ_SHELF_NOT_FOUND`
//! - 20504 `BIZ_SHELF_PROCESS_SHELF_NOT_FOUND`
//! - 20505 `BIZ_SHELF_PROCESS_PROCESS_NOT_FOUND` —— items 里有 process_id 不存在
//! - 20502 `BIZ_SHELF_DUPLICATE_CODE` —— uk_t_shelf_process 撞（理论不该发生，service 已去重）

use crate::auth::rbac::CurrentUser;
use crate::infra::snowflake::SnowflakeIdGenerator;
use crate::modules::shelf::repo::ShelfRepoTrait;
use crate::modules::shelf::vo::{ShelfProcessMappingItem, ShelfProcessMappingOut};
use crate::shared::error::{AppError, code};

pub mod sql;

// 重导出 sql.rs 中的 ZST + 输入结构，让旧调用路径
// `use crate::modules::shelf::process_mapping::ShelfProcessRepo;` 继续可用。
pub use sql::{NewShelfProcessRow, ShelfProcessRepo};

// ===========================================================================
// ShelfProcessService
// ===========================================================================

pub struct ShelfProcessService;

impl ShelfProcessService {
    /// 设置指定 shelf 的工序映射 —— **整组替换**语义：
    ///
    /// 1. 校验 shelf 存在 + active（`repo.get_by_id`）
    /// 2. 校验 items 内的所有 process_id 存在（`repo.proc_list_existing_process_ids` 跨域 helper）
    /// 3. 软删该 shelf 的全部旧 mapping（`repo.proc_soft_delete_all_for_shelf`）
    /// 4. INSERT 新 mapping（`repo.proc_bulk_insert`，按 sort_order）
    ///
    /// 整组事务由 caller 保证（handler 层 `state.pool.begin()` + commit）。
    ///
    /// 错误码：
    /// - 20501 `BIZ_SHELF_NOT_FOUND`
    /// - 20505 `BIZ_SHELF_PROCESS_PROCESS_NOT_FOUND` —— items 里有 process_id 不存在
    /// - 20502 `BIZ_SHELF_DUPLICATE_CODE` —— uk_t_shelf_process 撞（理论不该发生，service 已去重）
    #[allow(clippy::too_many_arguments)]
    pub async fn set_shelf_processes<R: ShelfRepoTrait>(
        &self,
        mut repo: R,
        snowflake: &SnowflakeIdGenerator,
        shelf_id: i64,
        items: &[crate::modules::shelf::dto::SetShelfProcessesItem],
        current: &CurrentUser,
    ) -> Result<(), AppError> {
        // 1. shelf 存在性 + 软删校验（已软删 → 404）
        let shelf = repo
            .get_by_id(shelf_id)
            .await?
            .ok_or_else(|| {
                AppError::biz(
                    code::BIZ_SHELF_NOT_FOUND,
                    format!("shelf {shelf_id} 不存在"),
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
            let existing_ids = repo
                .proc_list_existing_process_ids(&process_ids)
                .await
                .map_err(AppError::from)?;
            if existing_ids.len() != process_ids.len() {
                // 找出缺失的 id（用 Vec 差集；批量小，开销可忽略）
                let missing: Vec<i64> = process_ids
                    .iter()
                    .filter(|p| !existing_ids.contains(p))
                    .copied()
                    .collect();
                return Err(AppError::biz(
                    code::BIZ_SHELF_PROCESS_PROCESS_NOT_FOUND,
                    format!("process 不存在: {:?}", missing),
                ));
            }
        }

        // 3. 软删旧 mapping（事务内）
        repo.proc_soft_delete_all_for_shelf(shelf_id).await?;

        // 4. 批量 INSERT 新 mapping（空 items = 清空映射；无行写）
        let new_rows: Vec<NewShelfProcessRow> = items
            .iter()
            .zip(process_ids.iter())
            .map(|(it, &pid)| NewShelfProcessRow {
                shelf_id: shelf.id,
                process_id: pid,
                sort_order: it.sort_order,
            })
            .collect();
        repo.proc_bulk_insert(&new_rows, snowflake, current.id).await?;

        Ok(())
    }

    /// 列出指定 shelf 的所有 active mapping（按 sort_order ASC）。
    pub async fn list_shelf_processes<R: ShelfRepoTrait>(
        &self,
        mut repo: R,
        shelf_id: i64,
        current: &CurrentUser,
    ) -> Result<ShelfProcessMappingOut, AppError> {
        // 权限：与 list_shelves 一致（任意已登录）
        current.require_any_role(&[
            crate::auth::rbac::Role::Manager,
            crate::auth::rbac::Role::Clerk,
            crate::auth::rbac::Role::CncProgrammer,
            crate::auth::rbac::Role::ShelfAccount,
            crate::auth::rbac::Role::Inspector,
        ])?;

        // shelf 存在性 / scope 校验
        let shelf = repo
            .get_by_id(shelf_id)
            .await?
            .ok_or_else(|| {
                AppError::biz(
                    code::BIZ_SHELF_NOT_FOUND,
                    format!("shelf {shelf_id} 不存在"),
                )
            })?;
        if !current.can_access_shelf(shelf.id) {
            return Err(AppError::biz(
                code::SHELF_MISMATCH,
                format!("无权访问 shelf {shelf_id}"),
            ));
        }

        let rows = repo.proc_list_by_shelf(shelf.id).await?;
        let items = rows
            .into_iter()
            .map(
                |(sid, pid, sort_order, shelf_code, process_code)| ShelfProcessMappingItem {
                    shelf_id: sid,
                    shelf_code,
                    process_id: pid,
                    process_code,
                    sort_order,
                },
            )
            .collect();
        Ok(ShelfProcessMappingOut { items })
    }
}
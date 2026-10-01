//! prod::shelf_process 子模块 service —— 货架 ↔ 工序映射读写
//!
//! 2026-10-02 域归属反转：自 `src/modules/shelf/process_mapping/mod.rs` 平移
//! （业务逻辑零 diff，仅把「胖 trait 调用」换成「ZST 静态方法直调」）。
//!
//! ## 依赖方向
//! 迁移前：shelf 域 service 经胖 trait `ShelfRepoTrait` 的 2 个跨域 helper
//! （`proc_check_process_exists` / `proc_list_existing_process_ids`）反向依赖
//! `prod::process::ProcessRepo`；`t_shelf_process` 的 4 个方法也并入 shelf 的胖
//! trait。迁移后：
//! - `t_shelf_process` SQL 只在本文件 + 同目录 `repo.rs` 内
//! - 唯一跨域调用是**读** `shelf::repo::ShelfRepo::get_by_id`（校验 shelf 存在 +
//!   scope），方向由 shelf→prod 翻转为 prod→shelf
//! - process 存在性校验改调**同域** `prod::process::repo::ProcessRepo::list_by_ids`
//!   （零改动直接调），shelf 侧的 2 个反向 helper 已删
//!
//! ## 整组替换语义
//! `set_shelf_processes` 是「整组替换」：先软删该 shelf 的全部旧 mapping，再
//! INSERT 新列表；事务由 caller 保证（handler 层 `state.pool.begin()` + commit）。
//!
//! ## 错误码（数字不动，仅改归属说明；20504~20508 是已发布契约）
//! - 20501 `BIZ_SHELF_NOT_FOUND` —— 数字留在 shelf 段（货架本体）
//! - 20504 `BIZ_SHELF_PROCESS_SHELF_NOT_FOUND`
//! - 20505 `BIZ_SHELF_PROCESS_PROCESS_NOT_FOUND` —— items 里有 process_id 不存在
//! - 20507 `BIZ_SHELF_PROCESS_NOT_MAPPED`（worker_pool move 反向校验复用）
//! - 20508 `BIZ_SHELF_PROCESS_NOT_FOUND`（prod::batch dispatch 解析货架复用）
//! - 20502 `BIZ_SHELF_DUPLICATE_CODE` —— uk_t_shelf_process 撞（理论不该发生，service 已去重）

use sqlx::PgConnection;

use crate::auth::rbac::CurrentUser;
use crate::infra::snowflake::SnowflakeIdGenerator;
use crate::modules::prod::process::repo::ProcessRepo;
use crate::modules::shelf::repo::ShelfRepo;
use crate::shared::error::{AppError, code};

use super::dto::SetShelfProcessesItem;
use super::repo::{NewShelfProcessRow, ShelfProcessRepo};
use super::vo::{
    AllShelfProcessMappingItem, AllShelfProcessMappingOut, ShelfProcessMappingItem,
    ShelfProcessMappingOut,
};

// ===========================================================================
// ShelfProcessService
// ===========================================================================

pub struct ShelfProcessService;

impl ShelfProcessService {
    /// 设置指定 shelf 的工序映射 —— **整组替换**语义：
    ///
    /// 1. 校验 shelf 存在 + active（`ShelfRepo::get_by_id`，跨域只读）
    /// 2. 校验 items 内的所有 process_id 存在（`ProcessRepo::list_by_ids`，同域）
    /// 3. 软删该 shelf 的全部旧 mapping（`ShelfProcessRepo::soft_delete_all_for_shelf`）
    /// 4. INSERT 新 mapping（`ShelfProcessRepo::bulk_insert`，按 sort_order）
    ///
    /// 整组事务由 caller 保证（handler 层 `state.pool.begin()` + commit）。
    ///
    /// 错误码：
    /// - 20501 `BIZ_SHELF_NOT_FOUND`
    /// - 20505 `BIZ_SHELF_PROCESS_PROCESS_NOT_FOUND` —— items 里有 process_id 不存在
    /// - 20502 `BIZ_SHELF_DUPLICATE_CODE` —— uk_t_shelf_process 撞（理论不该发生，service 已去重）
    pub async fn set_shelf_processes(
        &self,
        conn: &mut PgConnection,
        snowflake: &SnowflakeIdGenerator,
        shelf_id: i64,
        items: &[SetShelfProcessesItem],
        current: &CurrentUser,
    ) -> Result<(), AppError> {
        // 1. shelf 存在性 + 软删校验（已软删 → 404）
        let shelf = ShelfRepo::get_by_id(&mut *conn, shelf_id)
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
            // 一次性批量查 process —— 防 N+1（同域 ProcessRepo::list_by_ids）
            let existing_ids = ProcessRepo::list_by_ids(&mut *conn, &process_ids).await?;
            if existing_ids.len() != process_ids.len() {
                // 找出缺失的 id（用 Vec 差集；批量小，开销可忽略）
                let existing_set: std::collections::HashSet<i64> =
                    existing_ids.iter().map(|p| p.id).collect();
                let missing: Vec<i64> = process_ids
                    .iter()
                    .filter(|p| !existing_set.contains(p))
                    .copied()
                    .collect();
                return Err(AppError::biz(
                    code::BIZ_SHELF_PROCESS_PROCESS_NOT_FOUND,
                    format!("process 不存在: {:?}", missing),
                ));
            }
        }

        // 3. 软删旧 mapping（事务内）
        ShelfProcessRepo::soft_delete_all_for_shelf(&mut *conn, shelf_id).await?;

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
        ShelfProcessRepo::bulk_insert(&mut *conn, &new_rows, snowflake, current.id).await?;

        Ok(())
    }

    /// 列出所有 active shelf 的全部 mapping（`GET /prod/shelf-processes`）。
    ///
    /// 2026-10-02 自 `shelf::service::ShelfService::list_all_process_mappings`
    /// 平移（原端点 `GET /shelves/processes` 已硬切到 `/api/v2/prod/shelf-processes`）。
    /// 单条 SQL JOIN（防 N+1）。任意已登录可调。
    pub async fn list_all_mappings(
        &self,
        conn: &mut PgConnection,
        current: &CurrentUser,
    ) -> Result<AllShelfProcessMappingOut, AppError> {
        current.require_any_role(&[
            crate::auth::rbac::Role::Manager,
            crate::auth::rbac::Role::Clerk,
            crate::auth::rbac::Role::CncProgrammer,
            crate::auth::rbac::Role::ShelfAccount,
            crate::auth::rbac::Role::Inspector,
        ])?;

        let rows = ShelfProcessRepo::list_all_active_mappings(&mut *conn).await?;

        // SHELF_ACCOUNT scope：统一走 `can_access_shelf`（Manager / wildcard 已短路
        // 返回 true ⇒ 见全集；其他按 user.shelf_ids 收窄）。
        let items: Vec<AllShelfProcessMappingItem> = rows
            .into_iter()
            .filter(|(sid, _, _, _)| current.can_access_shelf(*sid))
            .map(|(sid, pid, sc, pc)| AllShelfProcessMappingItem {
                shelf_id: sid,
                shelf_code: sc,
                process_id: pid,
                process_code: pc,
            })
            .collect();

        Ok(AllShelfProcessMappingOut { items })
    }

    /// 列出指定 shelf 的所有 active mapping（按 sort_order ASC）。
    pub async fn list_shelf_processes(
        &self,
        conn: &mut PgConnection,
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
        let shelf = ShelfRepo::get_by_id(&mut *conn, shelf_id)
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

        let rows = ShelfProcessRepo::list_by_shelf(&mut *conn, shelf.id).await?;
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

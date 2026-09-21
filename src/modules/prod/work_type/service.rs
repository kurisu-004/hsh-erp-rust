//! work_type 域 CRUD service
//!
//! 列表 / 详情 / 创建 / 更新 / 软删 —— 共 5 个端点（外加 `process_mapping.rs` 子模块
//! 维护的 2 个 mapping 端点）。
//!
//! ## 业务约束（service 层 enforce）
//! - `code` 业务唯一键，update 不允许改（20104）
//! - `max_held_batches` 三态编码：None = 不动；Some(None) = 清空；Some(Some(v)) = 改值，v ≥ 1
//! - 软删前查 `t_worker.work_type_id` + `t_work_type_process` 引用，>0 ⇒ 20903 拒
//!
//! ## mapping 端点
//! 见 `crate::modules::prod::work_type::process_mapping`（set / list per-work_type）。
//!
//! ## 事务边界（2026-09-22 D-2-simple 重构对齐 iam / shelf / customer 范本）
//! 事务移交 handler：handler 显式 `pool.begin()` / `commit()`，service 仅业务逻辑。
//! 所有跨 repo 操作经 `repo: R`（by-value；`R: WorkTypeRepoTrait`）参数传入——
//! handler/service 借 `&mut *tx` / `&mut *conn` 喂给 trait（trait 已直接
//! `impl for &mut PgConnection`）。
//!
//! `WorkTypeService` 字段仅 `snowflake: Arc<SnowflakeIdGenerator>`（雪花 ID 在
//! `create_work_type` 用）。实例为轻壳，可直接 `Arc<WorkTypeService>` 存 `AppState`。
//! `process_ids` 批量补齐走 trait 方法 `worktypeproc_list_by_work_types_batch`。
//!
//! `WorkTypeProcessService`（`process_mapping/mod.rs`）也只收
//! `<R: WorkTypeRepoTrait>`——胖 trait 已合并 `t_work_type_process` 的 4 个方法，
//! service 单 trait 一次收下即可。

use std::sync::Arc;

use crate::auth::rbac::{CurrentUser, Role};
use crate::infra::snowflake::SnowflakeIdGenerator;
use crate::modules::prod::work_type::dto::*;
use crate::modules::prod::work_type::model::TWorkType;
use crate::modules::prod::work_type::repo::WorkTypeRepoTrait;
use crate::shared::error::{AppError, code};

const DEFAULT_LIMIT: i64 = 50;
const MAX_LIMIT: i64 = 500;

fn work_type_not_found() -> AppError {
    AppError::biz(code::BIZ_WORK_TYPE_NOT_FOUND, "工种不存在")
}

fn version_conflict() -> AppError {
    AppError::biz(code::VERSION_CONFLICT, "数据已被他人修改，请刷新后重试")
}

/// 把 service 的 `TWorkType` 转 `WorkTypeOut`。`process_ids` 由 caller 在 list / get 时
/// 用 `repo.worktypeproc_list_by_work_types_batch` 批量补齐（防 N+1）。
fn to_work_type_out(wt: TWorkType, process_ids: Vec<String>) -> WorkTypeOut {
    WorkTypeOut {
        id: wt.id,
        code: wt.code,
        name: wt.name,
        description: wt.description,
        sort_order: wt.sort_order,
        max_held_batches: wt.max_held_batches,
        process_ids,
        version: wt.version,
        created_at: wt.created_at,
        updated_at: wt.updated_at,
    }
}

/// 把 i64 转 String；按 `process_ids` JSON 形态输出（前端不需要 i64 防精度截断）。
fn pid_to_string(pid: i64) -> String {
    pid.to_string()
}

/// work_type 域 service（2026-09-22 D-2-simple 重构后）
///
/// 字段仅 `snowflake`（事务已移交 handler）。实例为轻壳，可直接
/// `Arc<WorkTypeService>` 存 `AppState`；方法签名收 `mut repo: R`（by-value；
/// 生产 `R = &mut PgConnection`，单测 `R = MockWorkTypeRepo`），单测用 `MockWorkTypeRepo`
/// 直接注入。
pub struct WorkTypeService {
    snowflake: Arc<SnowflakeIdGenerator>,
}

impl WorkTypeService {
    /// 构造：仅需雪花 ID 生成器。
    pub fn new(snowflake: Arc<SnowflakeIdGenerator>) -> Self {
        Self { snowflake }
    }

    // =======================================================================
    // 列表 / 详情
    // =======================================================================

    pub async fn list_work_types<R: WorkTypeRepoTrait>(
        &self,
        mut repo: R,
        query: &WorkTypeListQuery,
        current: &CurrentUser,
    ) -> Result<WorkTypeListOut, AppError> {
        current.require_any_role(&[
            Role::Manager,
            Role::Clerk,
            Role::CncProgrammer,
            Role::ShelfAccount,
            Role::Inspector,
        ])?;

        let limit = query.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
        let offset = query.offset.unwrap_or(0).max(0);
        let code_like = query
            .code_like
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());

        let items = repo
            .list_with_filters(code_like, limit, offset)
            .await?;
        let total = repo.count_with_filters(code_like).await?;

        // process_ids 单条 SQL 批量算（防 N+1）—— 走胖 trait 方法
        let ids: Vec<i64> = items.iter().map(|w| w.id).collect();
        let mapping_rows = repo.worktypeproc_list_by_work_types_batch(&ids).await?;
        let mut mapping_map: std::collections::HashMap<i64, Vec<i64>> =
            std::collections::HashMap::new();
        for (wt_id, pid) in mapping_rows {
            mapping_map.entry(wt_id).or_default().push(pid);
        }

        let out_items = items
            .into_iter()
            .map(|wt| {
                let process_ids = mapping_map
                    .remove(&wt.id)
                    .unwrap_or_default()
                    .into_iter()
                    .map(pid_to_string)
                    .collect();
                to_work_type_out(wt, process_ids)
            })
            .collect();

        Ok(WorkTypeListOut {
            items: out_items,
            total,
            limit,
            offset,
        })
    }

    pub async fn get_work_type<R: WorkTypeRepoTrait>(
        &self,
        mut repo: R,
        id: i64,
        current: &CurrentUser,
    ) -> Result<WorkTypeOut, AppError> {
        current.require_any_role(&[
            Role::Manager,
            Role::Clerk,
            Role::CncProgrammer,
            Role::ShelfAccount,
            Role::Inspector,
        ])?;

        let wt = repo
            .get_by_id(id)
            .await?
            .ok_or_else(work_type_not_found)?;

        // process_ids 单条批量查（防 N+1：get 不必有 mapping 时也走同一函数）
        let mapping_rows = repo.worktypeproc_list_by_work_types_batch(&[wt.id]).await?;
        let process_ids: Vec<String> = mapping_rows
            .into_iter()
            .map(|(_, pid)| pid_to_string(pid))
            .collect();

        Ok(to_work_type_out(wt, process_ids))
    }

    // =======================================================================
    // 创建 / 更新 / 软删（MANAGER-only）
    // =======================================================================

    pub async fn create_work_type<R: WorkTypeRepoTrait>(
        &self,
        mut repo: R,
        req: &WorkTypeCreateRequest,
        current: &CurrentUser,
    ) -> Result<WorkTypeOut, AppError> {
        current.require_role(Role::Manager)?;

        let code = req.code.trim();
        if code.is_empty() {
            return Err(AppError::biz(code::BIZ_INVALID_VALUE, "code 不能为空"));
        }
        let name = req.name.trim();
        if name.is_empty() {
            return Err(AppError::biz(code::BIZ_INVALID_VALUE, "name 不能为空"));
        }
        let description = req
            .description
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let sort_order = req.sort_order.unwrap_or(0);
        let max_held_batches = req.max_held_batches;
        if let Some(v) = max_held_batches
            && v < 1
        {
            return Err(AppError::biz(
                code::BIZ_INVALID_VALUE,
                "max_held_batches 必须 ≥ 1",
            ));
        }

        let id = self.snowflake.next_id();
        let wt = repo
            .create(
                id,
                code,
                name,
                description,
                sort_order,
                max_held_batches,
                current.id,
            )
            .await
            .map_err(
                |e| match e.as_database_error().and_then(|d| d.code()).as_deref() {
                    // uk_t_work_type_code：活跃行唯一
                    Some("23505") => AppError::biz(
                        code::BIZ_WORK_TYPE_DUPLICATE_CODE,
                        format!("code '{code}' 已被占用"),
                    ),
                    _ => AppError::from(e),
                },
            )?;

        Ok(to_work_type_out(wt, Vec::new()))
    }

    pub async fn update_work_type<R: WorkTypeRepoTrait>(
        &self,
        mut repo: R,
        id: i64,
        req: &WorkTypeUpdateRequest,
        current: &CurrentUser,
    ) -> Result<WorkTypeOut, AppError> {
        current.require_role(Role::Manager)?;

        // code 业务唯一键不可变 —— 不论传值都拒（含空串、null 之外的任何值）
        if req.code.is_some() {
            return Err(AppError::biz(
                code::BIZ_INVALID_VALUE,
                "本接口不支持修改 code（业务唯一键不可变）",
            ));
        }

        let existing = repo
            .get_by_id(id)
            .await?
            .ok_or_else(work_type_not_found)?;

        // name: Some("") ⇒ 显式拒；None ⇒ 不修改
        let name_update: Option<&str> = match req.name.as_deref() {
            Some(s) => {
                let t = s.trim();
                if t.is_empty() {
                    return Err(AppError::biz(code::BIZ_INVALID_VALUE, "name 不能为空"));
                }
                Some(t)
            }
            None => None,
        };

        // description 三态：None ⇒ 不改；Some(None) ⇒ 清空；Some(Some(v)) ⇒ 改值
        let new_desc_owned: Option<String>;
        let desc_update: Option<Option<&str>> = match &req.description {
            None => None,
            Some(None) => Some(None),
            Some(Some(s)) => {
                let trimmed = s.trim();
                new_desc_owned = Some(trimmed.to_string());
                Some(Some(new_desc_owned.as_deref().unwrap()))
            }
        };

        // max_held_batches 三态：None ⇒ 不改；Some(None) ⇒ 清空（SET NULL，NULL=不限）；
        // Some(Some(v)) ⇒ 改值，校验 v ≥ 1
        let new_mhb_owned: Option<i32>;
        let mhb_update: Option<Option<i32>> = match &req.max_held_batches {
            None => None,
            Some(None) => Some(None),
            Some(Some(v)) => {
                if *v < 1 {
                    return Err(AppError::biz(
                        code::BIZ_INVALID_VALUE,
                        "max_held_batches 必须 ≥ 1",
                    ));
                }
                new_mhb_owned = Some(*v);
                Some(new_mhb_owned)
            }
        };

        let affected = repo
            .update(
                id,
                existing.version,
                name_update,
                desc_update,
                req.sort_order,
                mhb_update,
                current.id,
            )
            .await?;
        if affected == 0 {
            return Err(version_conflict());
        }

        // 回读最新行 + process_ids
        Self::get_work_type(self, repo, id, current).await
    }

    /// 软删前查引用：`t_worker.work_type_id` + `t_work_type_process` 任一 > 0 ⇒ 20903 拒。
    pub async fn soft_delete_work_type<R: WorkTypeRepoTrait>(
        &self,
        mut repo: R,
        id: i64,
        current: &CurrentUser,
    ) -> Result<(), AppError> {
        current.require_role(Role::Manager)?;

        let wt = repo
            .get_by_id(id)
            .await?
            .ok_or_else(work_type_not_found)?;

        let ref_count = repo.count_work_type_references(id).await?;
        if ref_count > 0 {
            return Err(AppError::biz(
                code::BIZ_WORK_TYPE_IN_USE,
                format!(
                    "工种 {} (code={}) 仍被 {ref_count} 处引用（worker 或 process mapping），无法软删",
                    wt.id, wt.code
                ),
            ));
        }

        let affected = repo.soft_delete(id, wt.version, current.id).await?;
        if affected == 0 {
            return Err(version_conflict());
        }
        Ok(())
    }
}

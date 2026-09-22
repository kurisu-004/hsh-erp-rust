//! work_type 域 CRUD service
//!
//! 列表 / 详情 / 创建 / 更新 / 软删 —— 共 5 个端点（外加本文件末尾合并的
//! `WorkTypeProcessService` 2 个 mapping 端点）。
//!
//! ## 业务约束（service 层 enforce）
//! - `code` 业务唯一键，update 不允许改（20104）
//! - `max_held_batches` 三态编码：None = 不动；Some(None) = 清空；Some(Some(v)) = 改值，v ≥ 1
//! - 软删前查 `t_worker.work_type_id` + `t_work_type_process` 引用，>0 ⇒ 20903 拒
//!
//! ## mapping 端点
//! 见本文件末尾 `WorkTypeProcessService`（2026-09-22 PR6 由原
//! `prod/work_type/process_mapping/{mod.rs, sql.rs}` 合并而来；set / list per-work_type）。
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
//! `WorkTypeProcessService`（本文件末尾）也只收 `<R: WorkTypeRepoTrait>`——
//! 胖 trait 已合并 `t_work_type_process` 的 4 个方法，service 单 trait 一次收下即可。

use std::sync::Arc;

use sqlx::PgExecutor;

use crate::auth::rbac::{CurrentUser, Role};
use crate::infra::snowflake::SnowflakeIdGenerator;
use crate::modules::prod::work_type::dto::{
    WorkTypeCreateRequest, WorkTypeListQuery, WorkTypeUpdateRequest,
};
use crate::modules::prod::work_type::model::TWorkType;
use crate::modules::prod::work_type::repo::WorkTypeRepoTrait;
use crate::modules::prod::work_type::vo::{
    WorkTypeListOut, WorkTypeOut, WorkTypeProcessMappingItem, WorkTypeProcessMappingOut,
};
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

// ===========================================================================
// 2026-09-22 PR6 合并入：原 `prod/work_type/process_mapping/{sql.rs, mod.rs}` 内容
// （work_type ↔ process 映射：`t_work_type_process` SQL 真源 +
// `WorkTypeProcessService` 2 个 service 方法）。本节内容一字未改，仅去掉原
// 子目录内的 `pub mod sql;` / `pub use sql::{...};` 重导出句。
// ===========================================================================

// ---- 原 process_mapping/sql.rs 内容开始 ----
//（2026-09-22 PR6 合并：原文件级 `//!` inner doc 不再合法，删去；
// 原 `use sqlx::PgExecutor;` / `use crate::infra::snowflake::...;` 已在本文件
// 顶部 imports 声明，本节内不再重复。）

/// 新 mapping 行的输入结构（service 层用，喂给 `WorkTypeProcessRepo::bulk_insert`）。
#[derive(Debug, Clone)]
pub struct NewWorkTypeProcessRow {
    pub work_type_id: i64,
    pub process_id: i64,
    pub sort_order: i32,
}

// ---------------------------------------------------------------------------
// WorkTypeProcessRepo（t_work_type_process，4 方法）
// ---------------------------------------------------------------------------

pub struct WorkTypeProcessRepo;

impl WorkTypeProcessRepo {
    /// 按 `work_type_id` 取所有 active mapping（按 sort_order ASC）。
    /// `t_work_type_process` 无业务软删（mapping 表通常保留历史），不筛 `deleted_at`。
    pub async fn list_by_work_type<'e, E: PgExecutor<'e>>(
        executor: E,
        work_type_id: i64,
    ) -> Result<Vec<(i64, i32, String)>, sqlx::Error> {
        // 返回 (process_id, sort_order, process_code) —— 单 JOIN
        sqlx::query_as(
            r#"
            SELECT wtp.process_id, wtp.sort_order, p.code AS process_code
            FROM t_work_type_process wtp
            JOIN t_process p ON p.id = wtp.process_id AND p.deleted_at IS NULL
            WHERE wtp.work_type_id = $1
            ORDER BY wtp.sort_order ASC, wtp.id ASC
            "#,
        )
        .bind(work_type_id)
        .fetch_all(executor)
        .await
    }

    /// 批量取一组工种的全部 process_id 列表（防 N+1）：
    /// 单条 SQL 返回 `[work_type_id, process_id, process_code, sort_order]`
    /// 用于 `WorkTypeService::list_work_types` 一次性补齐 `process_ids` 字段。
    pub async fn list_by_work_types_batch<'e, E: PgExecutor<'e>>(
        executor: E,
        work_type_ids: &[i64],
    ) -> Result<Vec<(i64, i64)>, sqlx::Error> {
        if work_type_ids.is_empty() {
            return Ok(Vec::new());
        }
        let rows: Vec<(i64, i64)> = sqlx::query_as(
            r#"
            SELECT work_type_id, process_id
            FROM t_work_type_process
            WHERE work_type_id = ANY($1)
            ORDER BY work_type_id, sort_order, id
            "#,
        )
        .bind(work_type_ids)
        .fetch_all(executor)
        .await?;
        Ok(rows)
    }

    /// 软删一个 work_type 的全部 active mapping（同事务内与 INSERT 配对）。
    pub async fn soft_delete_all_for_work_type<'e, E: PgExecutor<'e>>(
        executor: E,
        work_type_id: i64,
    ) -> Result<u64, sqlx::Error> {
        sqlx::query(
            r#"
            UPDATE t_work_type_process
            SET deleted_at = now()
            WHERE work_type_id = $1 AND deleted_at IS NULL
            "#,
        )
        .bind(work_type_id)
        .execute(executor)
        .await
        .map(|r| r.rows_affected())
    }

    /// 批量插入新 mapping：单条 INSERT ... VALUES (...), (...), (...)。
    ///
    /// 空切片短路返回 0 行；service 层若要清空映射仍应走 set_work_type_processes + 空 items。
    #[allow(clippy::too_many_arguments)]
    pub async fn bulk_insert<'e, E: PgExecutor<'e>>(
        executor: E,
        rows: &[NewWorkTypeProcessRow],
        snowflake: &SnowflakeIdGenerator,
        created_by: i64,
    ) -> Result<u64, sqlx::Error> {
        if rows.is_empty() {
            return Ok(0);
        }

        // sqlx::QueryBuilder 拼 INSERT ... VALUES (...), (...), ...；
        // 单条往返即可写入全部行（防 N+1）。
        use sqlx::QueryBuilder;
        let mut qb: QueryBuilder<sqlx::Postgres> = QueryBuilder::new(
            "INSERT INTO t_work_type_process (id, work_type_id, process_id, sort_order, created_by, updated_by) ",
        );
        qb.push_values(rows.iter(), |mut b, row| {
            let id = snowflake.next_id();
            b.push_bind(id)
                .push_bind(row.work_type_id)
                .push_bind(row.process_id)
                .push_bind(row.sort_order)
                .push_bind(created_by)
                .push_bind(created_by);
        });
        qb.build()
            .execute(executor)
            .await
            .map(|r| r.rows_affected())
    }
}

// ---- 原 process_mapping/sql.rs 内容结束 ----

// ---- 原 process_mapping/mod.rs 内容开始（去掉 `pub mod sql;` / `pub use sql::{...};`） ----
//（2026-09-22 PR6 合并：原文件级 `//!` inner doc 描述已删除的子目录结构，删去；
// 原 `use ...` 块已在本文件顶部 imports 声明，本节内不再重复。）

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

// ---- 原 process_mapping/mod.rs 内容结束 ----

//! worker 域业务逻辑
//!
//! 对应 Python myERP/service/worker.py + repository/worker_repository.py。
//! 实施约定：方法签名接收 `&mut PgConnection`，由 handler 开 tx 并 commit。
//!
//! ## 业务约束（service 层 enforce）
//! - `verify_badge` 命中但 `is_active=false` → 20202 `BIZ_WORKER_INACTIVE`（HTTP 400）；
//!   未命中 → 20201 `BIZ_WORKER_NOT_FOUND`（HTTP 404）。
//! - `deactivate` 前查 `t_part.current_holder_id = worker_id` 且
//!   `status IN ('IN_PROCESS','INSPECTION','REPAIRING','RETURNED')` 引用，>0 ⇒ 20203 拒
//! - `create_worker` 时 `work_type_id` 校验（如果非空）—— `WorkTypeRepo::get_by_id`
//! - `id_card_no` 部分唯一索引由 DB 兜底，捕获 `UniqueViolation`（23505）→ 40901
//!   `VERSION_CONFLICT`（与 Python 一致；统一用 40901 而非单独的 duplicate 业务码）
//! - `badge_code` 业务唯一键，撞 → 40901 `VERSION_CONFLICT`
//!
//! ## 权限（RBAC）
//! - `verify_badge`：**任意已登录用户**（含 SHELF_ACCOUNT）。service 层 `require_auth`。
//! - 其它 6 端点：MANAGER-only。service 层 `require_role(Role::Manager)`。

use sqlx::PgConnection;

use crate::auth::rbac::{CurrentUser, Role};
use crate::infra::snowflake::SnowflakeIdGenerator;
use crate::modules::worker::dto::*;
use crate::modules::worker::model::TWorker;
use crate::modules::worker::repo::WorkerRepo;
use crate::modules::work_type::repo::WorkTypeRepo;
use crate::shared::error::{code, AppError};

const DEFAULT_LIMIT: i64 = 50;
const MAX_LIMIT: i64 = 500;

fn worker_not_found() -> AppError {
    AppError::biz(code::BIZ_WORKER_NOT_FOUND, "工人不存在")
}

fn worker_inactive() -> AppError {
    AppError::biz(code::BIZ_WORKER_INACTIVE, "工人已停用")
}

fn version_conflict() -> AppError {
    AppError::biz(code::VERSION_CONFLICT, "数据已被他人修改，请刷新后重试")
}

fn work_type_not_found() -> AppError {
    AppError::biz(code::BIZ_WORK_TYPE_NOT_FOUND, "工种不存在")
}

/// 把 service 的 `TWorker` 转 `WorkerOut`。`work_type_name` 由 caller 在 list 时
/// 一次性批量补全；单条 get 与 verify_badge 不补 name（保持响应轻量）。
fn to_worker_out(w: TWorker, work_type_name: Option<String>) -> WorkerOut {
    WorkerOut {
        id: w.id,
        badge_code: w.badge_code,
        name: w.name,
        id_card_no: w.id_card_no,
        phone: w.phone,
        is_active: w.is_active,
        work_type_id: w.work_type_id.map(|v| v.to_string()),
        work_type_name,
        version: w.version,
        created_at: w.created_at,
        updated_at: w.updated_at,
    }
}

/// 把 `Some("")` / `Some("   ")` 视作 None；保留 trim 后的非空 owned String。
fn normalize_optional_str(s: Option<&str>) -> Option<String> {
    s.map(str::trim).filter(|t| !t.is_empty()).map(str::to_string)
}

/// 校验 `work_type_id` 字符串并解析为 i64，校验工种是否存在。
/// `None` / 空串 → `None`（未分配工种，合法）。
async fn resolve_work_type_id(
    conn: &mut PgConnection,
    raw: Option<&str>,
) -> Result<Option<i64>, AppError> {
    let trimmed = match raw.map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => s,
        None => return Ok(None),
    };
    let id = trimmed
        .parse::<i64>()
        .map_err(|_| AppError::biz(code::BIZ_INVALID_VALUE, "work_type_id 非整数"))?;
    let wt = WorkTypeRepo::get_by_id(&mut *conn, id)
        .await?
        .ok_or_else(work_type_not_found)?;
    Ok(Some(wt.id))
}

pub struct WorkerService;

impl WorkerService {
    // =======================================================================
    // 扫码校验（任意已登录用户）
    // =======================================================================

    pub async fn verify_badge(
        conn: &mut PgConnection,
        badge_code: &str,
        _user: &CurrentUser,
    ) -> Result<WorkerOut, AppError> {
        let code = badge_code.trim();
        if code.is_empty() {
            return Err(worker_not_found());
        }
        // include_deleted=true：以区分「不存在 (20201)」与「存在但停用 (20202)」。
        // Python `_d_get_by_badge_code` 也走 `include_deleted=True` 实现此语义。
        let w = WorkerRepo::get_by_badge_code(&mut *conn, code, true)
            .await?
            .ok_or_else(worker_not_found)?;
        if !w.is_active {
            return Err(worker_inactive());
        }
        // verify_badge 不补 work_type_name（响应轻量）；如需由前端再调 get_worker。
        Ok(to_worker_out(w, None))
    }

    // =======================================================================
    // 列表 / 详情（MANAGER-only）
    // =======================================================================

    pub async fn list_workers(
        conn: &mut PgConnection,
        query: &WorkerListQuery,
        user: &CurrentUser,
    ) -> Result<WorkerListOut, AppError> {
        user.require_role(Role::Manager)?;

        let limit = query.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
        let offset = query.offset.unwrap_or(0).max(0);
        let name_like = query
            .name_like
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());

        let items =
            WorkerRepo::list_with_filters(&mut *conn, name_like, query.is_active, limit, offset)
                .await?;
        let total =
            WorkerRepo::count_with_filters(&mut *conn, name_like, query.is_active).await?;

        // work_type_name 一次性批量补全（防 N+1）
        let wt_ids: Vec<i64> = items.iter().filter_map(|w| w.work_type_id).collect();
        let wt_rows = WorkTypeRepo::list_by_ids(&mut *conn, &wt_ids).await?;
        let wt_map: std::collections::HashMap<i64, String> =
            wt_rows.into_iter().map(|wt| (wt.id, wt.name)).collect();

        let out_items = items
            .into_iter()
            .map(|w| {
                let name = w.work_type_id.and_then(|id| wt_map.get(&id).cloned());
                to_worker_out(w, name)
            })
            .collect();

        Ok(WorkerListOut {
            items: out_items,
            total,
            limit,
            offset,
        })
    }

    pub async fn get_worker(
        conn: &mut PgConnection,
        id: i64,
        user: &CurrentUser,
    ) -> Result<WorkerOut, AppError> {
        user.require_role(Role::Manager)?;

        let w = WorkerRepo::get_by_id(&mut *conn, id, false)
            .await?
            .ok_or_else(worker_not_found)?;

        let work_type_name = match w.work_type_id {
            Some(wt_id) => WorkTypeRepo::get_by_id(&mut *conn, wt_id)
                .await?
                .map(|wt| wt.name),
            None => None,
        };
        Ok(to_worker_out(w, work_type_name))
    }

    // =======================================================================
    // 创建 / 更新 / 停用 / 重启（MANAGER-only）
    // =======================================================================

    pub async fn create_worker(
        conn: &mut PgConnection,
        snowflake: &SnowflakeIdGenerator,
        req: &WorkerCreateRequest,
        user: &CurrentUser,
    ) -> Result<WorkerOut, AppError> {
        user.require_role(Role::Manager)?;

        let badge_code = req.badge_code.trim();
        if badge_code.is_empty() {
            return Err(AppError::biz(code::BIZ_INVALID_VALUE, "badge_code 不能为空"));
        }
        let name = req.name.trim();
        if name.is_empty() {
            return Err(AppError::biz(code::BIZ_INVALID_VALUE, "name 不能为空"));
        }

        // 服务端再查一次唯一性（业务即时反馈）；DB uk_t_worker_badge_code 兜底
        if WorkerRepo::get_by_badge_code(&mut *conn, badge_code, false)
            .await?
            .is_some()
        {
            return Err(AppError::biz(
                code::VERSION_CONFLICT,
                format!("badge_code '{badge_code}' 已被占用"),
            ));
        }

        let id_card_no_owned = normalize_optional_str(req.id_card_no.as_deref());
        let phone_owned = normalize_optional_str(req.phone.as_deref());
        let work_type_id =
            resolve_work_type_id(&mut *conn, req.work_type_id.as_deref()).await?;

        let id = snowflake.next_id();
        let w = WorkerRepo::create(
            &mut *conn,
            id,
            badge_code,
            name,
            id_card_no_owned.as_deref(),
            phone_owned.as_deref(),
            work_type_id,
            user.id,
        )
        .await
        .map_err(|e| match e.as_database_error().and_then(|d| d.code()).as_deref() {
            // uk_t_worker_badge_code（23505）或 uk_t_worker_id_card_no（23505）
            // 统一映射到 40901（与 Python / brief 决策一致）。
            Some("23505") => {
                AppError::biz(code::VERSION_CONFLICT, "badge_code 或 id_card_no 已存在")
            }
            _ => AppError::from(e),
        })?;

        // 回读时再补 work_type_name（与 list 一致）
        let work_type_name = match w.work_type_id {
            Some(wt_id) => WorkTypeRepo::get_by_id(&mut *conn, wt_id)
                .await?
                .map(|wt| wt.name),
            None => None,
        };
        Ok(to_worker_out(w, work_type_name))
    }

    pub async fn update_worker(
        conn: &mut PgConnection,
        id: i64,
        req: &WorkerUpdateRequest,
        user: &CurrentUser,
    ) -> Result<WorkerOut, AppError> {
        user.require_role(Role::Manager)?;

        let current = WorkerRepo::get_by_id(&mut *conn, id, false)
            .await?
            .ok_or_else(worker_not_found)?;

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

        // badge_code: Some("") ⇒ 显式拒；None ⇒ 不修改；Some(non-empty) ⇒ 改值
        // 唯一性走 DB uk_t_worker_badge_code + service 预查 23505 兜底
        let badge_code_update: Option<&str> = match req.badge_code.as_deref() {
            Some(s) => {
                let t = s.trim();
                if t.is_empty() {
                    return Err(AppError::biz(
                        code::BIZ_INVALID_VALUE,
                        "badge_code 不能为空",
                    ));
                }
                Some(t)
            }
            None => None,
        };

        // id_card_no 三态：None ⇒ 不改；Some(None) ⇒ 清空；Some(Some(v)) ⇒ 改值
        let new_id_card_no_owned: Option<String>;
        let id_card_no_update: Option<Option<&str>> = match &req.id_card_no {
            None => None,
            Some(None) => Some(None),
            Some(Some(s)) => {
                let trimmed = s.trim();
                new_id_card_no_owned = Some(trimmed.to_string());
                Some(Some(new_id_card_no_owned.as_deref().unwrap()))
            }
        };

        // phone 三态：同 id_card_no
        let new_phone_owned: Option<String>;
        let phone_update: Option<Option<&str>> = match &req.phone {
            None => None,
            Some(None) => Some(None),
            Some(Some(s)) => {
                let trimmed = s.trim();
                new_phone_owned = Some(trimmed.to_string());
                Some(Some(new_phone_owned.as_deref().unwrap()))
            }
        };

        // work_type_id 三态：None ⇒ 不改；Some(None) ⇒ 清空；Some(Some(v)) ⇒ 改值
        // 改值时校验工种存在性
        let new_wt_owned: Option<i64>;
        let work_type_id_update: Option<Option<i64>> = match &req.work_type_id {
            None => None,
            Some(None) => Some(None),
            Some(Some(s)) => {
                let parsed = resolve_work_type_id(&mut *conn, Some(s.as_str())).await?;
                // resolve_work_type_id 在 raw=None/empty 时返回 Ok(None)，但这里
                // raw 一定是 Some(non-empty) 否则被 trim 后成 None 视作清空分支。
                // 为了保险显式 unwrap_or 返回错误：
                new_wt_owned = Some(parsed.ok_or_else(|| {
                    AppError::biz(code::BIZ_INVALID_VALUE, "work_type_id 非整数")
                })?);
                Some(new_wt_owned)
            }
        };

        let affected = WorkerRepo::update(
            &mut *conn,
            id,
            current.version,
            name_update,
            badge_code_update,
            id_card_no_update,
            phone_update,
            work_type_id_update,
            user.id,
        )
        .await
        .map_err(|e| match e.as_database_error().and_then(|d| d.code()).as_deref() {
            Some("23505") => {
                AppError::biz(code::VERSION_CONFLICT, "badge_code 或 id_card_no 已存在")
            }
            _ => AppError::from(e),
        })?;
        if affected == 0 {
            return Err(version_conflict());
        }

        // 回读最新行 + work_type_name
        Self::get_worker(conn, id, user).await
    }

    /// 停用：`is_active=false` 同时 `deleted_at=now()`。
    /// 停用前查 `t_part.current_holder_id = worker_id` 且
    /// `status IN ('IN_PROCESS','INSPECTION','REPAIRING','RETURNED')` 引用，
    /// >0 ⇒ 20203 `BIZ_WORKER_IN_USE` 拒。
    pub async fn deactivate_worker(
        conn: &mut PgConnection,
        id: i64,
        user: &CurrentUser,
    ) -> Result<(), AppError> {
        user.require_role(Role::Manager)?;

        let current = WorkerRepo::get_by_id(&mut *conn, id, false)
            .await?
            .ok_or_else(worker_not_found)?;

        let in_use = WorkerRepo::count_in_use_parts(&mut *conn, id).await?;
        if in_use > 0 {
            return Err(AppError::biz(
                code::BIZ_WORKER_IN_USE,
                format!(
                    "工人 {} (badge={}) 仍被 {in_use} 个 IN_PROCESS/INSPECTION/REPAIRING/RETURNED 零件引用，无法停用",
                    current.id, current.badge_code
                ),
            ));
        }

        let affected = WorkerRepo::deactivate(&mut *conn, id, current.version, user.id).await?;
        if affected == 0 {
            return Err(version_conflict());
        }
        Ok(())
    }

    /// 重启：`is_active=true` 同时 `deleted_at=NULL`。
    /// 行必须存在（含已 soft-delete）；OCC 校验。
    pub async fn reactivate_worker(
        conn: &mut PgConnection,
        id: i64,
        user: &CurrentUser,
    ) -> Result<WorkerOut, AppError> {
        user.require_role(Role::Manager)?;

        let current = WorkerRepo::get_by_id(&mut *conn, id, true)
            .await?
            .ok_or_else(worker_not_found)?;

        let affected = WorkerRepo::reactivate(&mut *conn, id, current.version, user.id).await?;
        if affected == 0 {
            // 已激活（deleted_at IS NULL）⇒ 无变化
            return Err(AppError::biz(
                code::BIZ_INVALID_VALUE,
                "工人已是激活状态，无需重启",
            ));
        }

        Self::get_worker(conn, id, user).await
    }
}

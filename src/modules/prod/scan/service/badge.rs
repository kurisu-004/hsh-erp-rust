//! prod::scan 的扫工牌用例：`POST /api/v2/prod/scan/verify-badge`
//!
//! 2026-10-10 自 `prod::worker::service::verify_badge` 搬来。业务逻辑逐字不变
//! （trim 空码 → 20201；`include_deleted=true` 区分「不存在」与「停用」；停用
//! → 20202），**只有出参收敛**成 [`ScanWorkerBrief`]（4 字段）。
//!
//! ## 权限
//! **任意已登录用户**（含 `SHELF_ACCOUNT`）：本端点是报工台三页的入口，工人自己
//! 用工牌号开机，故不设角色白名单。鉴权由 handler 的 `CurrentUser` extractor
//! 承担（未登录 → 401），service 层不重复校验。

use sqlx::PgConnection;

use crate::auth::rbac::CurrentUser;
use crate::modules::prod::scan::vo::ScanWorkerBrief;
use crate::modules::prod::worker::repo::WorkerRepo;
use crate::shared::error::{AppError, code};

use super::ScanService;

fn worker_not_found() -> AppError {
    AppError::biz(code::BIZ_WORKER_NOT_FOUND, "工人不存在")
}

fn worker_inactive() -> AppError {
    AppError::biz(code::BIZ_WORKER_INACTIVE, "工人已停用")
}

impl ScanService {
    /// 扫工牌：命中且活跃则返回报工台用的 4 字段最小投影。
    ///
    /// - `badge_code` 先 trim，trim 后为空 → 20201 `BIZ_WORKER_NOT_FOUND`
    /// - `include_deleted=true`：以区分「不存在 (20201)」与「存在但停用 (20202)」
    /// - 停用 → 20202 `BIZ_WORKER_INACTIVE`（HTTP 400）
    pub async fn verify_badge(
        conn: &mut PgConnection,
        badge_code: &str,
        _user: &CurrentUser,
    ) -> Result<ScanWorkerBrief, AppError> {
        let code = badge_code.trim();
        if code.is_empty() {
            return Err(worker_not_found());
        }
        let w = WorkerRepo::get_by_badge_code(conn, code, true)
            .await?
            .ok_or_else(worker_not_found)?;
        if !w.is_active {
            return Err(worker_inactive());
        }
        Ok(ScanWorkerBrief {
            id: w.id,
            badge_code: w.badge_code,
            name: w.name,
            work_type_id: w.work_type_id,
        })
    }
}

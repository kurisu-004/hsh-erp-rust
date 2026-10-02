//! prod::programming 子模块 service 层 —— 业务逻辑
//!
//! 2026-10-01 新增：单个方法 [`ProgrammingService::list_pending`]。
//!
//! ## 角色守卫
//! 下沉到 service（沿 `prod::batch::BatchService::list_pending` 范本），service
//! 入口第一行 `current.require_any_role(...)`；handler 仅做权限分发。
//!
//! ⚠️ `Role::CncProgrammer` **必须**在白名单内：前端「待编程一览」页由 CNC 编程员
//! 账号进入，漏掉该角色会直接 403。
//!
//! ## 事务边界
//! 读端点不开事务（handler `pool.acquire()` 借 `&mut PgConnection`），与
//! `prod::batch` 的 pending 端点一致。

use sqlx::PgConnection;

use crate::auth::rbac::{CurrentUser, Role};
use crate::modules::prod::programming::repo::{
    ProgrammingFilters, ProgrammingRepo, ProgrammingRow,
};
use crate::modules::prod::programming::vo::{ProgrammingItemOut, ProgrammingListOut};
use crate::shared::error::AppError;

use super::dto::ProgrammingListQuery;

/// 默认分页大小（与 part 域 `pending-programming` 一致）。
const DEFAULT_LIMIT: i64 = 50;

/// `prod::programming` service（ZST，与 `prod::batch` 范本一致）。
pub struct ProgrammingService;

impl ProgrammingService {
    /// `GET /api/v2/prod/programming/pending` 业务逻辑。
    ///
    /// 流程：角色守卫 → limit/offset clamp → 规范化 filters（trim + 空串→None）
    /// → `list` + `count` 两条 SQL → row → vo 投影。
    pub async fn list_pending(
        conn: &mut PgConnection,
        current: &CurrentUser,
        q: ProgrammingListQuery,
    ) -> Result<ProgrammingListOut, AppError> {
        current.require_any_role(&[
            Role::Manager,
            Role::Clerk,
            Role::Inspector,
            Role::CncProgrammer,
        ])?;

        // 防御：limit / offset 边界（limit=0 → 1；offset 负数 → 0）
        let limit = q.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, 500);
        let offset = q.offset.unwrap_or(0).max(0);

        let f = ProgrammingFilters {
            keyword: normalize(q.keyword),
            serial_no: normalize(q.serial_no),
            sort_by: q.sort_by,
            sort_dir: q.sort_dir,
            limit,
            offset,
            has_cnc_program: q.has_cnc_program,
        };

        let rows = ProgrammingRepo::list(&mut *conn, &f).await?;
        let total = ProgrammingRepo::count(&mut *conn, &f).await?;
        let items = rows.into_iter().map(row_to_item).collect();

        Ok(ProgrammingListOut {
            items,
            total,
            limit,
            offset,
        })
    }
}

/// 入参字符串规范化：trim 后空串收敛成 `None`（避免 `keyword= ` 变成 `ILIKE '%%'`
/// 全表扫）。
fn normalize(raw: Option<String>) -> Option<String> {
    raw.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// row → vo 投影（日期走 `String`，NULL 兜底 `"1970-01-01"`）。
fn row_to_item(r: ProgrammingRow) -> ProgrammingItemOut {
    ProgrammingItemOut {
        id: r.id,
        version: r.version,
        serial_no: r.serial_no,
        name: r.name,
        drawing_no: r.drawing_no,
        quantity: r.quantity,
        status: r.status,
        is_urgent: r.is_urgent,
        planned_delivery_date: r
            .planned_delivery_date
            .map(|d| d.to_string())
            .unwrap_or_else(|| "1970-01-01".to_string()),
        system_delivery_date: r.system_delivery_date,
        customer_name: r.customer_name,
        parent_customer_name: r.parent_customer_name,
        has_cnc_program: r.has_cnc_program,
        // 2026-10-03 新增：PROGRAMMING 活跃批次锚点（无该状态批次 → 两个都是 None，
        // 前端据此禁用「下发」按钮）。与上面的 part 级 `version` 严格区分。
        batch_id: r.batch_id,
        batch_version: r.batch_version,
    }
}

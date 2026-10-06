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

/// 排序列白名单（`sort_by` → ORDER BY 列名）。
///
/// 映射放在 service 层而不是 repo：`order_col` 会被拼进 SQL 文本，只有经过这张
/// 映射表的 `sort_by` 才能到达 repo —— 外部输入不可能直接成为 SQL 片段
/// （范式同 `prod::batch::service::list` 的同名 helper）。
///
/// 未命中 / 缺省 → `p.planned_delivery_date`；非法 `sort_by` **不报错**（前端切表头
/// 时不会因为拼错参数拿到 5xx），一律退化为默认列。
fn resolve_order_col(sort_by: Option<&str>) -> &'static str {
    match sort_by {
        Some("CREATED_AT") => "p.created_at",
        Some("UPDATED_AT") => "p.updated_at",
        Some("PLANNED_DELIVERY_DATE") => "p.planned_delivery_date",
        Some("REQUEST_DATE") => "p.request_date",
        Some("SERIAL_NO") => "p.serial_no",
        Some("DRAWING_NO") => "p.drawing_no",
        Some("NAME") => "p.name",
        _ => "p.planned_delivery_date",
    }
}

/// 排序方向：仅 `DESC`（忽略大小写）被接受，其余（含缺省）→ `ASC`。
fn resolve_order_dir(sort_dir: Option<&str>) -> &'static str {
    match sort_dir {
        Some(d) if d.eq_ignore_ascii_case("DESC") => "DESC",
        _ => "ASC",
    }
}

/// `prod::programming` service（ZST，与 `prod::batch` 范本一致）。
pub struct ProgrammingService;

impl ProgrammingService {
    /// `GET /api/v2/prod/programming/pending` 业务逻辑。
    ///
    /// 流程：角色守卫 → limit/offset clamp → 规范化 filters（trim + 空串→None +
    /// 排序白名单映射）→ `list` + `count` 两条 SQL → row → vo 投影。
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
            order_col: resolve_order_col(q.sort_by.as_deref()),
            order_dir: resolve_order_dir(q.sort_dir.as_deref()),
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

#[cfg(test)]
mod tests {
    //! 排序白名单的映射表 + 兜底口径在这里锁住：repo 侧拿到的是本层映射出的列名
    //! 字面量，映射表一旦被改宽，外部输入就会直接变成 SQL 片段。集成测试
    //! `tests/production/pending_programming.rs::sorting_default_desc_and_invalid_sort_by`
    //! 锁端到端行为，本组锁映射表本身（含注入串退化）。

    use super::{normalize, resolve_order_col, resolve_order_dir};

    #[test]
    fn order_col_whitelist_maps_and_degrades() {
        for (raw, expect) in [
            ("CREATED_AT", "p.created_at"),
            ("UPDATED_AT", "p.updated_at"),
            ("PLANNED_DELIVERY_DATE", "p.planned_delivery_date"),
            ("REQUEST_DATE", "p.request_date"),
            ("SERIAL_NO", "p.serial_no"),
            ("DRAWING_NO", "p.drawing_no"),
            ("NAME", "p.name"),
        ] {
            assert_eq!(
                resolve_order_col(Some(raw)),
                expect,
                "sort_by={raw} 未命中白名单"
            );
        }
        // 缺省 / 非法值 / 注入串 → 计划交期（绝不 500、绝不进 SQL 片段）
        assert_eq!(resolve_order_col(None), "p.planned_delivery_date");
        assert_eq!(
            resolve_order_col(Some("p.serial_no; DROP TABLE t_part")),
            "p.planned_delivery_date"
        );
        // 列名只认全大写：与下方 order_dir 的忽略大小写不对称（口径登记在 dto 字段 doc）
        assert_eq!(
            resolve_order_col(Some("created_at")),
            "p.planned_delivery_date"
        );
        assert_eq!(
            resolve_order_col(Some("serial_no")),
            "p.planned_delivery_date"
        );
    }

    #[test]
    fn order_dir_only_accepts_desc() {
        assert_eq!(resolve_order_dir(Some("DESC")), "DESC");
        assert_eq!(resolve_order_dir(Some("desc")), "DESC");
        assert_eq!(resolve_order_dir(Some("ASC")), "ASC");
        assert_eq!(resolve_order_dir(Some("ASC; DROP TABLE t_part")), "ASC");
        assert_eq!(resolve_order_dir(None), "ASC");
    }

    /// 入参规范化：trim + 空串 → `None`（`keyword= ` 不得变成 `ILIKE '%%'` 全表扫）。
    #[test]
    fn normalize_trims_and_drops_blank() {
        assert_eq!(
            normalize(Some("  ABC  ".to_string())).as_deref(),
            Some("ABC")
        );
        assert_eq!(normalize(Some("   ".to_string())), None);
        assert_eq!(normalize(Some(String::new())), None);
        assert_eq!(normalize(None), None);
    }
}

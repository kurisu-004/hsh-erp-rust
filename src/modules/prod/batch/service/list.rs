//! prod::batch 的 INSPECTION 状态批次集合读
//!
//! `GET /api/v2/prod/batches/inspection` —— 返回 `status='INSPECTION'` 的全部活跃
//! 批次，出参 `InspectionQueueListOut` 严格对齐前端待品检页的 7 个数据列
//! （序列号 / 图号 / 名称 / 批次 / 数量 / 系统交期 / 客户）+ 操作列锚点。
//! `GET /repair` 与 `GET /repairing` 是另两条判据 + 另一套宽 VO，见 `repair.rs`。
//!
//! - 权限：Manager + Inspector
//! - 限流：`limit ∈ [1, 200]`，默认 200；`offset` 默认 0
//! - `customer_id`：单值 → `expand_customer_id` 展开为 L1+L2 ids（与 `list_parts` 同逻辑）
//! - `drawing_no` / `name` / `serial_no`：表头筛选各一个独立 ILIKE 参数，service 层拼
//!   `%...%` 加通配符；**拒绝** `%` / `_` / `\` 等通配符（含任一 → VALIDATION_ERROR
//!   40001）。拒通配符是**语义**约束 —— 防止用户输入的 `%…%` 被 PG 当通配符放大成
//!   跨全表的 ILIKE 扫描（表头筛选框输个 `%` 就能把全表捞出来）；**注入面**由 repo
//!   侧 `QueryBuilder::push_bind` 参数化保证，与本校验无关。
//! - `system_delivery_date_from/to`：筛**系统交期**（页面已不显示计划交期）
//! - `sort_by` / `sort_dir`：服务端排序，白名单映射在本层完成，repo 只收列名字面量
//!
//! 2026-10-03 VO 收口：改用 `repo/list.rs` 的 3-JOIN 窄投影 + 表头独立筛选 +
//! 服务端排序；`is_urgent` **不再**参与服务端排序（仅作展示字段供前端标红），
//! 白名单映射在本层完成。

use crate::auth::rbac::{CurrentUser, Role};
use crate::modules::part::repo::PartRepoTrait;
use crate::modules::part::service::crud::expand_customer_id;
use crate::modules::prod::batch::dto::InspectionQueueQuery;
use crate::modules::prod::batch::repo::PartBatchRepo;
use crate::modules::prod::batch::repo::list::InspectionQueueFilters;
use crate::modules::prod::batch::vo::{InspectionQueueItemOut, InspectionQueueListOut};
use crate::shared::error::AppError;

use super::BatchService;

/// 排序列白名单（`sort_by` → ORDER BY 列名）。
///
/// 映射放在 service 层而不是 repo：`order_col` 会被拼进 SQL 文本，只有经过这张
/// 映射表的 `sort_by` 才能到达 repo —— 外部输入不可能直接成为 SQL 片段
/// （范式同 `part/repo/sql/part_sql.rs`，那里把同样式放在 repo）。
///
/// 2026-10-03 与前端表头 7 列一一对应。
fn resolve_order_col(sort_by: Option<&str>) -> &'static str {
    match sort_by {
        Some("SERIAL_NO") => "p.serial_no",
        Some("DRAWING_NO") => "p.drawing_no",
        Some("NAME") => "p.name",
        Some("BATCH_NO") => "pb.batch_no",
        Some("QUANTITY") => "pb.quantity",
        Some("CUSTOMER_NAME") => "c.name",
        // SYSTEM_DELIVERY_DATE + 缺省 + 非法值统一退化到系统交期
        // （待品检页默认按交期近优先排）。
        _ => "p.system_delivery_date",
    }
}

/// 排序方向：仅 `DESC`（忽略大小写）被接受，其余（含缺省）→ `ASC`。
fn resolve_order_dir(sort_dir: Option<&str>) -> &'static str {
    match sort_dir {
        Some(d) if d.eq_ignore_ascii_case("DESC") => "DESC",
        _ => "ASC",
    }
}

/// 文本筛选参数 → ILIKE pattern：拒绝 `%` / `_` / `\` 后拼 `%...%`。
///
/// 拒绝通配符是**语义**约束，不是注入防护：注入面由 repo 侧 `push_bind` 参数化保证。
/// 拒它的理由是 `%…%` 会被 PG 当通配符放大 —— 表头筛选框只输一个 `%` 就能把整张
/// 表捞出来，1 次请求退化成全表 ILIKE 扫描。空白串视为「不筛选」（筛选框清空态
/// 传空串比传缺省更常见）。
fn to_ilike_pat(field: &str, raw: Option<&str>) -> Result<Option<String>, AppError> {
    let Some(v) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    if v.contains(['%', '_', '\\']) {
        return Err(AppError::validation(format!(
            "{field} 不能包含通配符 % _ \\"
        )));
    }
    Ok(Some(format!("%{v}%")))
}

impl BatchService {
    /// `GET /prod/batches/inspection` 待品检队列列表。
    ///
    /// 权限：Manager + Inspector。
    /// 限流：`limit ∈ [1, 200]`，默认 200；`offset` 默认 0。
    /// customer_id：单值 → `expand_customer_id` 展开为 L1+L2 ids（与 `list_parts` 同逻辑）。
    /// drawing_no / name / serial_no：各一个 ILIKE 独立筛选（`%` / `_` / `\\` → 40001）。
    /// 排序：见文件头 `resolve_order_col` 白名单；非法 `sort_by` 退化为系统交期，
    /// 非法 `sort_dir` 退化为 ASC（不报错，前端表头切列不会拿到 5xx）。
    pub async fn list_inspection_batches<R: PartRepoTrait>(
        mut repo: R,
        query: &InspectionQueueQuery,
        current: &CurrentUser,
    ) -> Result<InspectionQueueListOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Inspector])?;

        let limit = query.limit.unwrap_or(200).clamp(1, 200);
        let offset = query.offset.unwrap_or(0).max(0);

        // customer_id 展开：单值 → [L1, 所有 L2]；None → 空切片（不过滤）
        let customer_ids_owned: Vec<i64>;
        let customer_ids: &[i64] = if let Some(cid) = query.customer_id {
            customer_ids_owned = expand_customer_id(repo.conn_mut(), cid).await?;
            &customer_ids_owned
        } else {
            &[]
        };

        let drawing_no_pat = to_ilike_pat("drawing_no", query.drawing_no.as_deref())?;
        let name_pat = to_ilike_pat("name", query.name.as_deref())?;
        let serial_no_pat = to_ilike_pat("serial_no", query.serial_no.as_deref())?;

        let filters = InspectionQueueFilters {
            customer_ids,
            drawing_no_pat: drawing_no_pat.as_deref(),
            name_pat: name_pat.as_deref(),
            serial_no_pat: serial_no_pat.as_deref(),
            date_from: query.system_delivery_date_from,
            date_to: query.system_delivery_date_to,
            order_col: resolve_order_col(query.sort_by.as_deref()),
            order_dir: resolve_order_dir(query.sort_dir.as_deref()),
            limit,
            offset,
        };

        let rows = PartBatchRepo::list_inspection_queue(repo.conn_mut(), &filters).await?;
        let total = PartBatchRepo::count_inspection_queue(repo.conn_mut(), &filters).await?;

        Ok(InspectionQueueListOut {
            items: rows.into_iter().map(InspectionQueueItemOut::from).collect(),
            total,
            limit,
            offset,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{resolve_order_col, resolve_order_dir, to_ilike_pat};

    #[test]
    fn order_col_whitelist_maps_and_degrades() {
        assert_eq!(resolve_order_col(Some("SERIAL_NO")), "p.serial_no");
        assert_eq!(resolve_order_col(Some("DRAWING_NO")), "p.drawing_no");
        assert_eq!(resolve_order_col(Some("NAME")), "p.name");
        assert_eq!(resolve_order_col(Some("BATCH_NO")), "pb.batch_no");
        assert_eq!(resolve_order_col(Some("QUANTITY")), "pb.quantity");
        assert_eq!(
            resolve_order_col(Some("SYSTEM_DELIVERY_DATE")),
            "p.system_delivery_date"
        );
        assert_eq!(resolve_order_col(Some("CUSTOMER_NAME")), "c.name");
        // 缺省 / 非法值 → 系统交期（绝不 500）
        assert_eq!(resolve_order_col(None), "p.system_delivery_date");
        assert_eq!(
            resolve_order_col(Some("p.serial_no; DROP TABLE t_part_batch")),
            "p.system_delivery_date"
        );
    }

    #[test]
    fn order_dir_only_accepts_desc() {
        assert_eq!(resolve_order_dir(Some("DESC")), "DESC");
        assert_eq!(resolve_order_dir(Some("desc")), "DESC");
        assert_eq!(resolve_order_dir(Some("ASC")), "ASC");
        assert_eq!(
            resolve_order_dir(Some("ASC;DROP TABLE t_part_batch")),
            "ASC"
        );
        assert_eq!(resolve_order_dir(None), "ASC");
    }

    #[test]
    fn ilike_pat_rejects_wildcards_and_blanks_out_empty() {
        assert_eq!(
            to_ilike_pat("name", Some("ABC")).unwrap().as_deref(),
            Some("%ABC%")
        );
        assert_eq!(to_ilike_pat("name", Some("  ")).unwrap(), None);
        assert_eq!(to_ilike_pat("name", None).unwrap(), None);
        for bad in ["A%B", "A_B", "A\\B"] {
            assert!(
                to_ilike_pat("name", Some(bad)).is_err(),
                "含通配符应被拒：{bad}"
            );
        }
    }
}

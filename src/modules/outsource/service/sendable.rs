//! outsource 域 service — `GET /outsource-sendable` 子模块
//!
//! 2026-10-03 新增。可发送外协的一览：一行 = 一个（活跃批次 × 该批次所在货架上、
//! 且在该零件工艺链内的 OUTSOURCE 工序）组合。
//!
//! 取代 part 域旧 `list_outsource_sendable`（后者返回通用 `PartListItem`，与前端
//! 外协域字段需求完全不匹配 → 页面全灰）。旧端点已删除（`/parts/outsource-sendable`
//! 实际返回 400 —— part 域 `/{part_id}` `Path<i64>` catch-all 兜底，非 404，
//! 见 `docs/api/inconsistencies.md` § 9.2），无 alias。
//!
//! ## 判定逻辑全在 SQL
//! `APPROVAL` / `DIRECT` 的判定、`quote_id` 的选取、`company_options` 的
//! `array_agg` 全部在 `OutsourceSendableRepo`（`repo/sql.rs`）一条 SQL 内完成；
//! service 只做 VO 映射（含 `company_options` 的 JSON → 结构体解码）。**禁止**
//! 在这里循环查公司（会退化成 N+1）。
//!
//! ## 事务边界
//! 读端点：handler `pool.acquire()` 不开事务，service 借 `&mut *conn`。

use crate::auth::rbac::{CurrentUser, Role};
use crate::modules::outsource::dto::OutsourceSendableListQuery;
use crate::modules::outsource::repo::OutsourceRepoTrait;
use crate::modules::outsource::vo::{
    OutsourceCompanyOption, OutsourceSendableItem, OutsourceSendableListOut,
};
use crate::shared::error::AppError;

use super::{DEFAULT_LIMIT, LIST_MAX_LIMIT, OutsourceService, join_customer_path, keyword_pattern};

/// 把 SQL `array_agg(json_build_object(...))` 的结果解成 `Vec<OutsourceCompanyOption>`。
///
/// 解不出来（理论上不会：SQL 侧已 `COALESCE` 成 `ARRAY[]::json[]`）时**降级为空数组**
/// 而不是让整个 list 端点 500 —— 少一个下拉选项远好过整页不可用。
fn decode_company_options(raw: serde_json::Value) -> Vec<OutsourceCompanyOption> {
    serde_json::from_value(raw).unwrap_or_default()
}

impl OutsourceService {
    /// `GET /outsource-sendable`（2026-10-03 新增）
    ///
    /// 角色守卫与旧 `list_outsource_sendable` 一致（多给 Inspector 只读）。
    pub async fn list_sendable<R: OutsourceRepoTrait>(
        &self,
        mut repo: R,
        query: &OutsourceSendableListQuery,
        current: &CurrentUser,
    ) -> Result<OutsourceSendableListOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk, Role::Inspector])?;
        let limit = query
            .limit
            .unwrap_or(DEFAULT_LIMIT)
            .clamp(1, LIST_MAX_LIMIT);
        let offset = query.offset.unwrap_or(0).max(0);
        let pat = keyword_pattern(query.keyword.as_deref());

        let rows = repo
            .sendable_list(pat.as_deref(), query.customer_id, limit, offset)
            .await?;
        let total = repo
            .sendable_count(pat.as_deref(), query.customer_id)
            .await?;

        let items = rows
            .into_iter()
            .map(|r| {
                // 命中 APPROVED 报价 → APPROVAL 模式（quote_id / company / price 三件套
                // 来自报价）；未命中 → DIRECT（company_options 已由 SQL 填好，
                // APPROVAL 行走 SQL 的 CASE 分支恒为空数组）。
                let send_mode = if r.quote_id.is_some() {
                    "APPROVAL"
                } else {
                    "DIRECT"
                };
                OutsourceSendableItem {
                    version: r.batch_version,
                    send_mode: send_mode.to_string(),
                    source_status: r.source_status,
                    part_id: r.part_id,
                    part_serial_no: r.part_serial_no,
                    part_drawing_no: r.part_drawing_no,
                    part_name: r.part_name,
                    quantity: r.batch_quantity,
                    batch_id: r.batch_id,
                    batch_no: r.batch_no,
                    batch_quantity: r.batch_quantity,
                    planned_delivery_date: r.planned_delivery_date,
                    is_urgent: r.is_urgent,
                    customer_path: join_customer_path(
                        r.parent_customer_name.as_deref(),
                        r.customer_name.as_deref(),
                    ),
                    next_process_id: r.next_process_id,
                    next_process_name: Some(r.next_process_name),
                    shelf_code: Some(r.shelf_code),
                    outsource_company_id: r.outsource_company_id,
                    outsource_company_name: r.outsource_company_name,
                    quote_id: r.quote_id,
                    company_options: decode_company_options(r.company_options),
                    price: r.price,
                    status_label: "sendable".to_string(),
                }
            })
            .collect();

        Ok(OutsourceSendableListOut {
            items,
            total,
            limit,
            offset,
        })
    }
}

#[cfg(test)]
mod tests {
    //! 2026-10-03 新增：send_mode 判定 + company_options 解码的纯函数回归。
    use super::decode_company_options;

    #[test]
    fn company_options_empty_array_decodes_to_empty_vec() {
        let v: serde_json::Value = serde_json::json!([]);
        assert!(decode_company_options(v).is_empty());
    }

    #[test]
    fn company_options_decodes_id_as_i64() {
        let v: serde_json::Value = serde_json::json!([
            { "id": 9_000_000_000_000_000_101i64, "name": "Co A" }
        ]);
        let opts = decode_company_options(v);
        assert_eq!(opts.len(), 1);
        assert_eq!(opts[0].id, 9_000_000_000_000_000_101);
        assert_eq!(opts[0].name, "Co A");
    }

    #[test]
    fn company_options_malformed_degrades_to_empty_vec() {
        // 不 panic、不冒泡 —— 单行降级不该让整页 list 500
        let opts = decode_company_options(serde_json::json!("not-an-array"));
        assert!(opts.is_empty());
    }
}

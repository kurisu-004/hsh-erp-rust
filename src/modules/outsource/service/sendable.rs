//! outsource 域 service — `GET /outsource-sendable` 子模块
//!
//! 2026-10-03 新增，同日改判据：可发送外协的一览，一行 = 一个活跃批次（判据是
//! `t_part_batch.current_process_id` 指向一道 OUTSOURCE 工序，不再要求零件绑了
//! 工艺链 —— 生产库里绝大多数零件没有链，原谓词导致端点恒空，见
//! `repo/sql.rs::SENDABLE_INNER_X_SQL`）。
//!
//! 取代 part 域旧 `list_outsource_sendable`（后者返回通用 `PartListItem`，与前端
//! 外协域字段需求完全不匹配 → 页面全灰）。旧端点已删除（`/parts/outsource-sendable`
//! 实际返回 400 —— part 域 `/{part_id}` `Path<i64>` catch-all 兜底，非 404，
//! 成因与取舍见 `src/modules/part/mod.rs` 的模块 doc），无 alias。
//!
//! ## 判定逻辑全在 SQL
//! 哪些批次出行、`send_mode` 的审批闸门、`quote_id` 的选取、`company_options` 的
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

/// 候选行的 `send_mode` 判定（`service/pool.rs` 的看板候选列复用本函数）。
///
/// 2026-10-03 起语义由「有没有命中已批准报价」改为「该外协工序是否需要审批」：
/// - `requires_approval = false` → `DIRECT`（免审批直发）。此时候选报价字段
///   （`quote_id` / `price` / 公司）被 SQL 层的 `AND pr.requires_approval` 短路成
///   `null`，即便历史上恰好存在一条 APPROVED 报价也不参与判定 —— 免审批直发的
///   价由写侧 `resolve_direct_quote_id` 现建占位报价，用旧报价的价发货才是错的。
/// - `requires_approval = true` → `APPROVAL`。SQL 的 EXISTS 闸门已保证存在已批准
///   报价，故 `has_approved_quote` 恒为 `true`；两个入参都取仍写成「与」是为了把
///   「APPROVAL 必有 quote_id」这条不变量守在派生点，而不是依赖 SQL 的隐式保证。
///
/// ## `(true, false)` 分支是双重闸门下的兜底，构造上不可达（保留而非改 fail-closed）
/// 2026-10-03 review 第 1 轮复核过是否该把它改成报错/不出行，结论是**不改**：
/// 1. **不可达**：`requires_approval=true` 的行要出行必须过 SQL 的 EXISTS 闸门，
///    而同一份内层 `x` 的 LEFT JOIN 谓词与之等价（都要求命中一条真实审批报价，
///    2026-10-03 起两处都带 `is_direct = false`）⇒ `quote_id IS NULL` 的行根本进不了
///    结果集；写侧 `send_to_outsource` 也已拒 `requires_approval && direct`（20104）。
/// 2. **改 fail-closed 会破坏 VO 契约**：`OutsourceSendableItem.quote_id` 的契约是
///    「APPROVAL 有值 / DIRECT `null`」。让本函数返回 APPROVAL 之外的第三种结果（或
///    抛错）都要求 service 层开始丢弃行，于是「列表行数 ≠ count」这类对账事故重新
///    出现；返回 `"APPROVAL"` + `quote_id = null` 则直接违反上面那条 VO 契约，前端
///    拿 `null` 去调 `send-to-outsource` 会吃 400。
/// 3. **保留的代价可控**：真走到这个分支时，该行的 `company_options` 由 SQL 的
///    `CASE WHEN q.id IS NOT NULL` 短路成 `[]`（`q.id` 为 NULL）⇒ 前端
///    `canSend()`（要求 APPROVAL 或 `company_options.length >= 1`）把该行置灰，
///    不会出现「用 0 元占位价发货」。
pub(super) fn send_mode_of(requires_approval: bool, has_approved_quote: bool) -> &'static str {
    if requires_approval && has_approved_quote {
        "APPROVAL"
    } else {
        "DIRECT"
    }
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
                // send_mode 由「该外协工序是否需要审批」决定（`requires_approval`），
                // 命中已批准报价的免审批工序仍判 DIRECT —— 见 `send_mode_of`。
                let send_mode = send_mode_of(r.requires_approval, r.quote_id.is_some());
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
                    current_process_id: r.current_process_id,
                    current_process_name: Some(r.current_process_name),
                    shelf_code: r.shelf_code,
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
    //! 2026-10-03 新增：`send_mode` 判定 + `company_options` 解码的纯函数回归。
    use super::{decode_company_options, send_mode_of};

    /// 免审批工序 → DIRECT，**即便**历史上有已批准报价（SQL 已把它短路成 null）。
    #[test]
    fn send_mode_direct_when_approval_not_required() {
        assert_eq!(send_mode_of(false, false), "DIRECT");
        assert_eq!(send_mode_of(false, true), "DIRECT");
    }

    /// 需审批 + 命中已批准报价 → APPROVAL。
    #[test]
    fn send_mode_approval_when_required_and_quoted() {
        assert_eq!(send_mode_of(true, true), "APPROVAL");
    }

    /// 需审批但没命中报价（SQL 的 EXISTS 闸门理论上已排除）→ 降级 DIRECT，
    /// 代价是 `company_options` 为空时前端把该行置灰，而不是拿一个没有 quote_id
    /// 的 APPROVAL 行去发。
    ///
    /// 2026-10-03 review 第 1 轮：保留本用例是为了把「兜底而非 fail-closed」这个
    /// 决策钉在测试里 —— 改成报错或返回 APPROVAL 会违反 VO 契约，理由见
    /// `send_mode_of` 的 doc「`(true, false)` 分支是双重闸门下的兜底」。
    #[test]
    fn send_mode_falls_back_to_direct_when_quote_missing() {
        assert_eq!(send_mode_of(true, false), "DIRECT");
    }

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

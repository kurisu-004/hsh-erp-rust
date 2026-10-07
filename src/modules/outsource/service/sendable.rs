//! outsource 域 — 候选侧三个共用纯函数（2026-10-03 新增，2026-10-09 端点下线后
//! 只剩「共用纯函数」这一个身份）
//!
//! 本文件原本是 `GET /outsource-sendable` 的实现（service + 该端点的判定入口）。该端点
//! 于 2026-10-09 硬切下线：它的行就是 `GET /outsource-queue/processes/{id}` 候选列的
//! **分页子集**（看板不分页），分页 + 关键字 + 客户过滤那套入参与 VO 一并删除。
//!
//! ## 为什么这三个函数留在这里而不是删掉 / 搬去看板侧
//!
//! | 函数 | 谁消费 | 判的是什么 |
//! |---|---|---|
//! | `send_mode_of` | 看板候选列 | APPROVAL / DIRECT 双模式 |
//! | `can_send_of` | 看板候选列（卡片是否置灰） | 该行能不能发 |
//! | `decode_company_options` | 看板候选列 | SQL `array_agg` JSON → 结构体 |
//!
//! 候选侧的**谓词 SQL**（`repo/sql.rs::SENDABLE_INNER_X_SQL` + `DISTINCT ON` 收敛层）
//! 与这三个纯函数是**同一份判定**的三个出口。判定与 `company_options` 是一组，搬到
//! `board/` 会让 `repo/sql.rs` 的谓词真源与它的消费者分居两个目录（`board/` 受
//! 「固定 SQL 条数」源码护栏圈住，判定函数混进去就不再是纯聚合）。留在本模块并
//! `pub(crate)` 放开，比在看板侧复制粘贴一份更便宜 —— 两边各写一遍时，「tab 内行数
//! ≠ 一览行数」这类对账事故是**静默**的。
//!
//! ## 事务边界
//! 无：本文件只剩纯函数，不再有端点实现（看板两读是 handler `pool.acquire()` 不开
//! 事务，见 `super::board`）。

use crate::modules::outsource::vo::OutsourceCompanyOption;

/// 把 SQL `array_agg(json_build_object(...))` 的结果解成 `Vec<OutsourceCompanyOption>`。
///
/// 解不出来（理论上不会：SQL 侧已 `COALESCE` 成 `ARRAY[]::json[]`）时**降级为空数组**
/// 而不是让整个 list 端点 500 —— 少一个下拉选项远好过整页不可用。
pub(crate) fn decode_company_options(raw: serde_json::Value) -> Vec<OutsourceCompanyOption> {
    serde_json::from_value(raw).unwrap_or_default()
}

/// 候选行的 `send_mode` 判定（看板候选列 `board/service.rs::to_candidate` 消费）。
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
///    结果集；写侧 `service/move.rs` 也已拒 `requires_approval && direct`（20104）。
/// 2. **改 fail-closed 会破坏 VO 契约**：`OutsourceSendableItem.quote_id` 的契约是
///    「APPROVAL 有值 / DIRECT `null`」。让本函数返回 APPROVAL 之外的第三种结果（或
///    抛错）都要求 service 层开始丢弃行，于是「列表行数 ≠ count」这类对账事故重新
///    出现；返回 `"APPROVAL"` + `quote_id = null` 则直接违反上面那条 VO 契约，前端
///    拿 `null` 去调 `send-to-outsource` 会吃 400。
/// 3. **保留的代价可控**：真走到这个分支时，该行的 `company_options` 由 SQL 的
///    `CASE WHEN q.id IS NOT NULL` 短路成 `[]`（`q.id` 为 NULL）⇒ 前端
///    `canSend()`（要求 APPROVAL 或 `company_options.length >= 1`）把该行置灰，
///    不会出现「用 0 元占位价发货」。
pub(crate) fn send_mode_of(requires_approval: bool, has_approved_quote: bool) -> &'static str {
    if requires_approval && has_approved_quote {
        "APPROVAL"
    } else {
        "DIRECT"
    }
}

/// 该候选行能否发送：`APPROVAL` 恒可发（报价已批），`DIRECT` 需至少一个候选公司。
///
/// 提成独立函数是为了能单测 —— 这条判定同时驱动「卡片是否置灰」和「拖到公司列后能否
/// 真发出去」，两处口径漂移的代价是用户点了没反应。
///
/// 消费方是看板候选列（`board/service.rs::to_candidate` 把它装进 `can_send`）。
/// 放在本模块而不是看板侧，是因为判定与 `send_mode_of` / `company_options` 是一组，
/// 而这三个函数的真源都在这里。
pub(crate) fn can_send_of(send_mode: &str, company_options: &[OutsourceCompanyOption]) -> bool {
    send_mode == "APPROVAL" || !company_options.is_empty()
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

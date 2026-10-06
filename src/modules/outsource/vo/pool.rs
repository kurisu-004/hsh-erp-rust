//! outsource 域 `GET /outsource-pool/*` 三个端点的响应 VO（2026-10-03 新增）
//!
//! ## 为什么独立顶层前缀
//! 看板要按「外协工序」切 tab：左边一份「可发送候选批次」、右边若干列「外协公司」。
//! 既有 `GET /outsource-sendable` / `GET /outsource-shipments/in-flight` 都**不接受
//! `process_id` 且分页**，无法支撑按工序切 tab；本域形态刻意照抄
//! `prod::pool`（`GET /api/v2/prod/pool/{state,counts,/{process_id}}`），让前端
//! 复用同一套 queryKey / 失效编排 / 测试范本。
//!
//! ## 与既有只读端点的一致性约束
//! - 候选侧（`counts.sendable_count` / `items`）与 `GET /outsource-sendable`
//!   **同源 SQL**（见 `repo/sql.rs` 的 `SENDABLE_INNER_X_SQL`），逐字段一致。
//! - `OutsourcePoolCandidate` 刻意**不复用** `OutsourceSendableItem`：后者带
//!   `current_process_id` / `current_process_name`（分页一览里「这一行是哪道工序」
//!   是必须的），而看板视角工序已提到顶层 `process_id`，重复出现会让前端 Zod
//!   守门时出现两个真相源。
//!
//! ## 雪花 ID / Decimal 序列化口径
//! - i64 雪花 ID 一律 `serialize_i64` ⇒ JSON **字符串**（避免 JS 精度截断）。
//! - Decimal 一律 `::text` 取成字符串。
//! - `receive_next_process_id` 走 **0 兜底**（后端 i64 + `serialize_i64`，NULL
//!   走 `.unwrap_or(0)`）—— 与 `prod::queue::vo::queue::PendingBatchItem.
//!   current_process_step_id` 同一口径，JSON 里非 nullable，语义为字符串 `"0"`
//!   = 「工序链缺失或指针漂移，接收时需人工填下一道工序」。

use serde::Serialize;

use crate::shared::types::{serialize_i64, serialize_i64_opt};

use super::sendable::OutsourceCompanyOption;

// ===========================================================================
//  GET /outsource-pool/counts
// ===========================================================================

/// `counts[]` 单条：一道外协工序的「可发 / 在途」双徽标。
#[derive(Debug, Clone, Serialize)]
pub struct OutsourcePoolProcessCount {
    #[serde(serialize_with = "serialize_i64")]
    pub process_id: i64,
    pub process_code: String,
    pub process_name: String,
    /// 可发送候选批次数。行粒度与 `GET /outsource-pool/{process_id}` 的 `items`
    /// **完全一致**（同源 SQL，含 DIRECT 且 `company_options` 为空的行）。
    pub sendable_count: i64,
    /// 在该工序在外协的批次数（`status='OUTSOURCE' AND
    /// location='OUTSOURCE_COMPANY' AND current_process_id = process_id`）。
    pub in_flight_count: i64,
}

/// `GET /outsource-pool/counts` 顶层响应。
///
/// 前端用它渲染 tab 标题徽标，不需要每个 tab 的详情端点已加载（避免 N+1 轮询
/// per-process 端点）。
#[derive(Debug, Clone, Serialize)]
pub struct OutsourcePoolCountsOut {
    /// 只含 `sendable_count + in_flight_count > 0` 的工序，按 `process_id ASC` 稳定排序。
    pub counts: Vec<OutsourcePoolProcessCount>,
    pub sendable_total: i64,
    pub in_flight_total: i64,
    /// `sendable_total + in_flight_total`。
    pub total: i64,
}

// ===========================================================================
//  GET /outsource-pool/{process_id}
// ===========================================================================

/// `companies[]` 单条：看板右列的一列。
///
/// **无在途批次的公司也在列内**（`held_count = 0`）—— 前端要渲染空公司列当
/// 拖拽目标。
#[derive(Debug, Clone, Serialize)]
pub struct OutsourcePoolCompanyOut {
    #[serde(serialize_with = "serialize_i64")]
    pub company_id: i64,
    pub name: String,
    pub held_count: i64,
}

/// 看板左列的一个候选批次卡片。
#[derive(Debug, Clone, Serialize)]
pub struct OutsourcePoolCandidate {
    /// `t_part_batch.version`（批次级 OCC）。前端发送时原样回传。
    pub version: i32,
    /// `"APPROVAL"`（该外协工序 `requires_approval = true` 且已命中已批准报价）/
    /// `"DIRECT"`（`requires_approval = false`，免审批直发）。
    pub send_mode: String,
    /// 批次来源状态：`"PENDING"` / `"IN_PROCESS"`。
    pub source_status: String,
    #[serde(serialize_with = "serialize_i64")]
    pub part_id: i64,
    pub part_serial_no: Option<String>,
    pub part_drawing_no: Option<String>,
    pub part_name: Option<String>,
    /// 可发送数量（行 = 批次，恒等于 `batch_quantity`）。
    pub quantity: i32,
    #[serde(serialize_with = "serialize_i64")]
    pub batch_id: i64,
    pub batch_no: i32,
    pub batch_quantity: i32,
    /// `YYYY-MM-DD`；DB 侧 `t_part.planned_delivery_date` 是 `NOT NULL`，
    /// 这里保留 `?` 只为与既有 `OutsourceSendableItem` 逐字一致。
    pub planned_delivery_date: Option<String>,
    pub is_urgent: bool,
    /// 有 L1 拼 `L1 / L2`，仅 L2 时给 L2 名，无客户 `null`。
    pub customer_path: Option<String>,
    /// 批次所在货架 code。`PENDING` 且未上架的批次没有 holder，故为 `null`。
    pub shelf_code: Option<String>,
    /// APPROVAL 有值（取报价的公司）/ DIRECT `null`。
    #[serde(serialize_with = "serialize_i64_opt")]
    pub outsource_company_id: Option<i64>,
    pub outsource_company_name: Option<String>,
    /// 命中的 APPROVED 报价 id。APPROVAL 有值 / DIRECT `null`。
    #[serde(serialize_with = "serialize_i64_opt")]
    pub quote_id: Option<i64>,
    /// DIRECT 列出该公司工序映射的全部活跃公司；APPROVAL 恒为 `[]`。
    /// DIRECT 且该工序未映射任何活跃公司时为 `[]` —— **该行仍返回**
    /// （`can_send = false` 把它置灰），不要在 SQL 里滤掉。
    pub company_options: Vec<OutsourceCompanyOption>,
    /// APPROVAL 报价的 Decimal 字符串 / DIRECT `null`。
    pub price: Option<String>,
    /// `send_mode == "APPROVAL" || !company_options.is_empty()`。
    /// 服务端算好给前端，避免每个视图各写一遍判定。
    pub can_send: bool,
    /// 恒为 `"sendable"`。
    pub status_label: String,
}

/// `GET /outsource-pool/{process_id}` 顶层响应（一个 tab 的全部内容）。
#[derive(Debug, Clone, Serialize)]
pub struct OutsourcePoolDetailOut {
    #[serde(serialize_with = "serialize_i64")]
    pub process_id: i64,
    pub process_code: String,
    pub process_name: String,
    /// 该工序映射的**全部活跃外协公司**（含 `held_count = 0` 的空列）。
    pub companies: Vec<OutsourcePoolCompanyOut>,
    /// `items.len()`（不分页）。
    pub total: i64,
    pub items: Vec<OutsourcePoolCandidate>,
}

// ===========================================================================
//  GET /outsource-pool/state
// ===========================================================================

/// 看板右列（某公司在某工序在外协）的一个批次卡片。
#[derive(Debug, Clone, Serialize)]
pub struct OutsourceHeldBatchItem {
    #[serde(serialize_with = "serialize_i64")]
    pub batch_id: i64,
    #[serde(serialize_with = "serialize_i64")]
    pub part_id: i64,
    pub batch_no: i32,
    /// **当前余量**（`t_part_batch.quantity`），不是 shipment.quantity ——
    /// 前端拿它做「部分接收」输入框的 max 值。
    pub quantity: i32,
    pub serial_no: Option<String>,
    pub drawing_no: String,
    pub name: String,
    pub system_delivery_date: Option<chrono::NaiveDate>,
    pub planned_delivery_date: Option<chrono::NaiveDate>,
    pub is_urgent: bool,
    /// L2 叶子客户名。
    pub customer_name: Option<String>,
    /// L1 一级集团名。
    pub parent_customer_name: Option<String>,
    pub applicant_name: Option<String>,
    /// 恒为 `"OUTSOURCE_COMPANY"`。
    pub location: String,
    pub note: Option<String>,
    /// `t_part_batch.version` —— 前端拿它当 `receive-from-outsource` 的 OCC 锚。
    pub version: i32,
    /// `t_outsource_shipment.sent_at`（ISO 字符串 / `null`）。
    ///
    /// 正常流**恒有值**：`uq_t_outsource_shipment_open_batch` 保证一个批次最多
    /// 一张开口 shipment，而 `send_to_outsource` 在同一事务里 INSERT 它，所以
    /// `status='OUTSOURCE' AND location='OUTSOURCE_COMPANY'` 的批次必然对应一张
    /// `OUTSOURCING` shipment。之所以仍声明成可空，是因为驱动 SQL 按契约用
    /// `LEFT JOIN`（防御历史脏数据 / 手工改库），不想给一个可能不存在的值编一个
    /// 假的替身（见 `price` 字段的同类说明）。
    pub sent_at: Option<chrono::NaiveDateTime>,
    /// `t_outsource_shipment.unit_price` 的 Decimal 字符串 / `null`。
    ///
    /// 与 `sent_at` 同理：正常流恒有值，可空是 `LEFT JOIN` 的诚实映射。
    /// **刻意不用空串兜底** —— 空串会被前端当成「0 元 / 格式错误的数」渲染，
    /// 比 `null` 难排查。
    pub price: Option<String>,
    /// 接收后批次的下一道工序 id；**无下一 step 时为 `"0"`**（0 兜底口径，
    /// 非 nullable）。前端据此判断要不要弹对话框让用户填工序。
    #[serde(serialize_with = "serialize_i64")]
    pub receive_next_process_id: i64,
    pub receive_next_process_name: Option<String>,
    /// `receive_next_process_id != 0`。
    ///
    /// 业务含义：`true` ⇒ 工序链已知，前端可免填「下一道工序」；
    /// `false` ⇒ 工序链缺失或指针漂移，前端必须让用户填。
    pub chain_resolvable: bool,
}

/// `GET /outsource-pool/state?outsource_company_id=&process_id=` 顶层响应。
#[derive(Debug, Clone, Serialize)]
pub struct OutsourcePoolStateOut {
    #[serde(serialize_with = "serialize_i64")]
    pub outsource_company_id: i64,
    /// 公司不存在 / 已软删时为 `null`（本端点不做存在性拒绝，见文档「端点契约要点」）。
    pub outsource_company_name: Option<String>,
    #[serde(serialize_with = "serialize_i64")]
    pub process_id: i64,
    /// `items.len()`。
    pub current_held: i64,
    pub items: Vec<OutsourceHeldBatchItem>,
}

#[cfg(test)]
mod tests {
    //! 2026-10-03 新增：`receive_next_process_id` 的 0 兜底序列化口径守卫。
    //!
    //! 这条不变量一旦破掉，前端 Zod 的 `z.string()` 守门会以
    //! 「`expected string, received number`」的形式炸在整页渲染上，而症状离
    //! 根因很远，故锁在单测里。
    use super::*;

    fn item(next_process_id: i64) -> OutsourceHeldBatchItem {
        OutsourceHeldBatchItem {
            batch_id: 9_000_000_000_000_000_501,
            part_id: 9_000_000_000_000_000_502,
            batch_no: 1,
            quantity: 5,
            serial_no: None,
            drawing_no: "DWG-1".into(),
            name: "NAME-1".into(),
            system_delivery_date: None,
            planned_delivery_date: None,
            is_urgent: false,
            customer_name: None,
            parent_customer_name: None,
            applicant_name: None,
            location: "OUTSOURCE_COMPANY".into(),
            note: None,
            version: 3,
            sent_at: None,
            price: Some("12.50".into()),
            receive_next_process_id: next_process_id,
            receive_next_process_name: None,
            chain_resolvable: next_process_id != 0,
        }
    }

    /// 无下一 step ⇒ 序列化成字符串 `"0"`（不是数字 `0`，也不是 `null`）。
    #[test]
    fn receive_next_process_id_zero_serializes_as_string() {
        let v = serde_json::to_value(item(0)).unwrap();
        assert_eq!(
            v["receive_next_process_id"],
            serde_json::Value::String("0".into())
        );
    }

    /// 有下一 step ⇒ 十进制字符串，且与入参一致。
    #[test]
    fn receive_next_process_id_serializes_as_string() {
        let v = serde_json::to_value(item(9_000_000_000_000_000_503)).unwrap();
        assert_eq!(
            v["receive_next_process_id"],
            serde_json::Value::String("9000000000000000503".into())
        );
    }

    /// 雪花字段不得出现裸数字。
    #[test]
    fn snowflake_ids_are_strings() {
        let v = serde_json::to_value(item(1)).unwrap();
        assert!(v["batch_id"].is_string(), "{v}");
        assert!(v["part_id"].is_string(), "{v}");
    }
}

//! delivery_note 域 P1 送货分组 端点响应 VO

use serde::Serialize;

/// 组成员出参（id 序列化为字符串，避免 JS 精度截断）
#[derive(Debug, Clone, Serialize)]
pub struct DeliveryGroupMemberOut {
    #[serde(serialize_with = "crate::shared::types::serialize_i64")]
    pub customer_id: i64,
    pub customer_name: String,
}

/// 分组头出参
///
/// 2026-10-08 删掉 3 个零读字段：`customer_id`（前端从请求入参就知道自己在哪个 L1
/// 下分组）/ `created_at` / `updated_at`（时间戳无展示位）。OCC 锚 `version` 保留。
#[derive(Debug, Clone, Serialize)]
pub struct DeliveryGroupOut {
    #[serde(serialize_with = "crate::shared::types::serialize_i64")]
    pub id: i64,
    pub name: String,
    pub members: Vec<DeliveryGroupMemberOut>,
    pub version: i32,
}

/// 组外 L2 出参（按设计 §6.1：所有未入组的 L2）
#[derive(Debug, Clone, Serialize)]
pub struct UngroupedCustomerOut {
    #[serde(serialize_with = "crate::shared::types::serialize_i64")]
    pub id: i64,
    pub name: String,
}

/// 分组列表出参（GET /api/v2/com/delivery/group）
#[derive(Debug, Clone, Serialize)]
pub struct DeliveryGroupListOut {
    pub groups: Vec<DeliveryGroupOut>,
    pub ungrouped_customers: Vec<UngroupedCustomerOut>,
}

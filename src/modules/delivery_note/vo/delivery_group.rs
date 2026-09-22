//! delivery_note 域 P1 送货分组 端点响应 VO

use chrono::NaiveDateTime;
use serde::Serialize;

/// 组成员出参（id 序列化为字符串，避免 JS 精度截断）
#[derive(Debug, Clone, Serialize)]
pub struct DeliveryGroupMemberOut {
    #[serde(serialize_with = "crate::shared::types::serialize_i64")]
    pub customer_id: i64,
    pub customer_name: String,
}

/// 分组头出参
#[derive(Debug, Clone, Serialize)]
pub struct DeliveryGroupOut {
    #[serde(serialize_with = "crate::shared::types::serialize_i64")]
    pub id: i64,
    #[serde(serialize_with = "crate::shared::types::serialize_i64")]
    pub customer_id: i64,
    pub name: String,
    pub members: Vec<DeliveryGroupMemberOut>,
    pub version: i32,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
}

/// 组外 L2 出参（按设计 §6.1：所有未入组的 L2）
#[derive(Debug, Clone, Serialize)]
pub struct UngroupedCustomerOut {
    #[serde(serialize_with = "crate::shared::types::serialize_i64")]
    pub id: i64,
    pub name: String,
}

/// 分组列表出参（GET /delivery-groups）
#[derive(Debug, Clone, Serialize)]
pub struct DeliveryGroupListOut {
    pub groups: Vec<DeliveryGroupOut>,
    pub ungrouped_customers: Vec<UngroupedCustomerOut>,
}
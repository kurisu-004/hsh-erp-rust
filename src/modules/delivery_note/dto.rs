//! delivery_note 域 DTO
//!
//! 对应 Python myERP/schema/delivery_note.py。命名约定：
//! - `CreateXxxRequest` / `UpdateXxxRequest`：写操作入参
//! - `XxxOut`：单条详情出参（id 字段用 `#[serde(serialize_with = "shared::types::serialize_i64")]`）
//! - `XxxListItem` / `XxxListOut`：列表分页
//!
//! ## Phase P1 范围
//! 本期只实装「送货分组」（§6.1）一组：
//! `DeliveryGroupOut` / `DeliveryGroupListOut` / `CreateDeliveryGroupRequest` /
//! `UpdateDeliveryGroupRequest` / `DeliveryGroupIdRequest` /
//! `DeliveryGroupMemberOut` / `UngroupedCustomerOut`。
//! 送货单生命周期 + 扫码入单的 DTO 留到 P2 / P3。

use chrono::NaiveDateTime;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
//  出参
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
//  入参
// ---------------------------------------------------------------------------

/// 创建分组入参（POST /delivery-groups）
///
/// `member_customer_ids` 是**初始成员集合**，新增分组时一次性写入。
/// `name` 长度 1..=100（与 DB 列 `varchar(100)` 对齐），空白字符串 trim 后为空则拒。
#[derive(Debug, Clone, Deserialize)]
pub struct CreateDeliveryGroupRequest {
    /// L1 客户的雪花 id（请求 JSON 字符串）
    #[serde(deserialize_with = "crate::shared::types::deserialize_i64")]
    pub customer_id: i64,
    /// 分组名（trim 后 1..=100）
    pub name: String,
    /// 成员 L2 客户 id 列表（字符串形式；空 Vec 表示创建时无成员）
    #[serde(deserialize_with = "crate::shared::types::deserialize_i64_vec")]
    pub member_customer_ids: Vec<i64>,
}

/// 更新分组入参（POST /delivery-groups/{id}/update）
///
/// 字段语义：
/// - `version`：必填，用于乐观锁（req 与 DB 当前 version 不一致 → 409 / VERSION_CONFLICT）
/// - `name`：None = 不改；Some(trim 后空) = 400；Some(>100 字符) = 400
/// - `member_customer_ids`：None = 不改；Some(vec) = **全量替换**
///   （缺失成员软删、新增成员插入；同 tx 内校验成员冲突 21415）
#[derive(Debug, Clone, Deserialize)]
pub struct UpdateDeliveryGroupRequest {
    pub version: i32,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub member_customer_ids: Option<Vec<i64>>,
}

/// 软删除分组入参（POST /delivery-groups/{id}/soft-delete）
#[derive(Debug, Clone, Deserialize)]
pub struct DeliveryGroupIdRequest {
    pub version: i32,
}
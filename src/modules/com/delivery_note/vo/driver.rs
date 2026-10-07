//! 送货司机候选出参（`GET /api/v2/com/delivery/drivers`）
//!
//! 只有 3 个字段：`id` / `name` / `badge_code`。刻意**不复用**
//! `prod::worker::vo::WorkerOut` —— 后者 11 个字段（`id_card_no` / `phone` /
//! `version` / `created_at` …）对「选一个送货司机」这个用途全是多余载荷，且
//! `id_card_no` / `phone` 属个人信息，不该因为一个下拉框就发到前端。
//!
//! 2026-10-08 新增。

use serde::Serialize;

/// `GET /api/v2/com/delivery/drivers` 响应体。
#[derive(Debug, Clone, Serialize)]
pub struct DeliveryDriverListOut {
    pub items: Vec<DeliveryDriverOption>,
}

/// 一个司机候选（前端下拉框的一项）。
#[derive(Debug, Clone, Serialize)]
pub struct DeliveryDriverOption {
    /// `t_worker.id`（雪花 id，JSON **string**）—— 回传给 `POST /{id}/driver` 的
    /// `driver_worker_id`。
    #[serde(serialize_with = "crate::shared::types::serialize_i64")]
    pub id: i64,
    pub name: String,
    /// 工牌号（司机核销时手输的那串；`t_worker.badge_code` 非空列）。
    pub badge_code: String,
}

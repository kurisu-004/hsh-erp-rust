//! prod::inspection 子模块 DTO —— 入参（Query string 反序列化侧）
//!
//! 队列读 `GET /api/v2/prod/inspection/queue` 有 Query string 入参，故本域的入参
//! DTO 载体就是这一个文件；**扫码端点 `GET /scan/{serial_no}` 无任何入参**
//! （`serial_no` 走 path），故本域**没有**第二个 DTO（形态同
//! `prod::programming::dto`）。
//!
//! i64 反序列化兜底走 `shared::types::deserialize_i64_opt`（前端允许 数字 / 字符串
//! 两种形态，雪花 ID 一律 string 避免 JS `Number.MAX_SAFE_INTEGER` 精度截断）。

use serde::Deserialize;

use crate::shared::types::deserialize_i64_opt;

/// `GET /api/v2/prod/inspection/queue` 查询参数。
///
/// 2026-10-07 自 `prod::batch` 迁入（路由由 `GET /api/v2/prod/batches/inspection`
/// 改为本域 `GET /api/v2/prod/inspection/queue`，**破坏性变更、无 alias**，出参 JSON
/// 逐字不变）。筛选收敛到表头 7 列，每列一个独立参数（图号 / 名称 / 序列号各一个
/// ILIKE），日期区间筛系统交期（页面已不显示计划交期）。
/// `sort_by` 白名单（SERIAL_NO / DRAWING_NO / NAME / BATCH_NO / QUANTITY /
/// SYSTEM_DELIVERY_DATE / CUSTOMER_NAME），非法值退化为 SYSTEM_DELIVERY_DATE；
/// `sort_dir` 非法退化为 ASC。二者都在 service 层做白名单映射（见
/// `service.rs::resolve_order_col` / `resolve_order_dir`），**不报错**。
/// 3 个文本筛选值含 `%` / `_` / `\` 时在 service 层拒（40001）—— 那是防 `%…%`
/// 被当通配符放大成全表扫描的**语义**约束，注入面由 repo 的 `push_bind` 保证。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct InspectionQueueQuery {
    #[serde(default)]
    pub drawing_no: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub serial_no: Option<String>,
    #[serde(default, deserialize_with = "deserialize_i64_opt")]
    pub customer_id: Option<i64>,
    #[serde(default)]
    pub system_delivery_date_from: Option<chrono::NaiveDate>,
    #[serde(default)]
    pub system_delivery_date_to: Option<chrono::NaiveDate>,
    #[serde(default)]
    pub sort_by: Option<String>,
    #[serde(default)]
    pub sort_dir: Option<String>,
    #[serde(default, deserialize_with = "deserialize_i64_opt")]
    pub limit: Option<i64>,
    #[serde(default, deserialize_with = "deserialize_i64_opt")]
    pub offset: Option<i64>,
}

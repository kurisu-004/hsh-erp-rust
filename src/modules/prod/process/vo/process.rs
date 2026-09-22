//! process 域 端点响应 VO（2026-09-22 PR4 重构）

use chrono::NaiveDateTime;
use serde::Serialize;

/// 工序详情出参。
#[derive(Debug, Clone, Serialize)]
pub struct ProcessOut {
    #[serde(serialize_with = "crate::shared::types::serialize_i64")]
    pub id: i64,
    pub code: String,
    pub name: String,
    pub category: String,
    pub sort_order: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub requires_approval: bool,
    /// 前端工序卡片颜色（`#RRGGBBAA`，9 字符含 alpha）。NULL = 未设置。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    pub version: i32,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
}

/// 工序列表出参。
#[derive(Debug, Clone, Serialize)]
pub struct ProcessListOut {
    pub items: Vec<ProcessOut>,
    pub total: i64,
    pub limit: i64,
    pub offset: i64,
}

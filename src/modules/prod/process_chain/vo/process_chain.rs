//! process_chain 域 端点响应 VO（2026-09-22 PR4 重构）

use chrono::NaiveDateTime;
use serde::Serialize;

/// 工艺链步骤（链内按 sort_order 升序）。
#[derive(Debug, Clone, Serialize)]
pub struct ProcessChainStepOut {
    #[serde(serialize_with = "crate::shared::types::serialize_i64")]
    pub id: i64,
    pub sort_order: i32,
    #[serde(serialize_with = "crate::shared::types::serialize_i64")]
    pub process_id: i64,
    pub estimated_minutes: i32,
    /// 单步备注（车间操作员参考）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    pub version: i32,
}

/// 工艺链详情出参（header + steps）。
///
/// 2026-09-16 FK 翻转（migration 026）：移除 `part_id` 字段 —— 归属关系改由
/// `t_part.process_chain_id` 承载，前端从 part 列表/详情读取。
#[derive(Debug, Clone, Serialize)]
pub struct ProcessChainOut {
    #[serde(serialize_with = "crate::shared::types::serialize_i64")]
    pub id: i64,
    pub name: String,
    pub note: Option<String>,
    pub version: i32,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
    pub steps: Vec<ProcessChainStepOut>,
}

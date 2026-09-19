//! process_chain 域 DTO
//!
//! id 序列化约定：`serialize_i64`（雪花 ID → JSON 字符串，防 JS 精度截断）

use chrono::NaiveDateTime;
use serde::{Deserialize, Serialize};

use crate::shared::types::serialize_i64;

// ---------------------------------------------------------------------------
// 出参
// ---------------------------------------------------------------------------

/// 工艺链步骤（链内按 sort_order 升序）。
#[derive(Debug, Clone, Serialize)]
pub struct ProcessChainStepOut {
    #[serde(serialize_with = "serialize_i64")]
    pub id: i64,
    pub sort_order: i32,
    #[serde(serialize_with = "serialize_i64")]
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
    #[serde(serialize_with = "serialize_i64")]
    pub id: i64,
    pub name: String,
    pub note: Option<String>,
    pub version: i32,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
    pub steps: Vec<ProcessChainStepOut>,
}

// ---------------------------------------------------------------------------
// 入参
// ---------------------------------------------------------------------------

/// 单步 upsert 输入。
///
/// `process_id` 以字符串形式入参（雪花 ID 防 JS 精度截断约定），service 层
/// `parse::<i64>()`；`estimated_minutes` 必须 ≥ 0；`sort_order` 由前端维护
///（稀疏 10/20/30，DB 部分唯一索引兜底重复）。`note` 可选；空串视作 None。
#[derive(Debug, Clone, Deserialize)]
pub struct UpsertChainStep {
    pub sort_order: i32,
    #[serde(default)]
    pub process_id: String,
    pub estimated_minutes: i32,
    #[serde(default)]
    pub note: Option<String>,
}

/// 整组 upsert 请求：替换语义（不存在的 part 自动建链；存在的整组覆盖）。
///
/// `steps` 可为空数组（=保留 header 但清空所有步骤；sort_order 全删）。
#[derive(Debug, Clone, Deserialize)]
pub struct UpsertChainRequest {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub note: Option<String>,
    pub steps: Vec<UpsertChainStep>,
}

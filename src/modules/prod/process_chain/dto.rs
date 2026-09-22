//! process_chain 域 DTO（入参 + 校验）
//!
//! ## DTO/VO 边界（2026-09-22 PR4 重构）
//! 出参结构（`ProcessChainOut` / `ProcessChainStepOut`）已抽离至 `super::vo`。
//! 本文件仅含入参（Deserialize）。

use serde::Deserialize;

/// 单步 upsert 输入。
#[derive(Debug, Clone, Deserialize)]
pub struct UpsertChainStep {
    pub sort_order: i32,
    #[serde(default)]
    pub process_id: String,
    pub estimated_minutes: i32,
    #[serde(default)]
    pub note: Option<String>,
}

/// 整组 upsert 请求：替换语义。
#[derive(Debug, Clone, Deserialize)]
pub struct UpsertChainRequest {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub note: Option<String>,
    pub steps: Vec<UpsertChainStep>,
}

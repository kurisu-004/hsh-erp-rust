//! `OutsourceCompanyOption` —— DIRECT 模式下的候选外协公司
//! （`t_outsource_company_process` ∩ active 公司）
//!
//! 同一个类型既作 VO（Serialize，id 转字符串）又作 repo 行解码目标
//! （Deserialize，从 SQL `array_agg(json_build_object(...))` 解出）—— 故双派生。
//!
//! ## 2026-10-09：`GET /outsource-sendable` 的两个 VO 已删除
//! 该端点硬切下线（看板 `/outsource-queue/processes/{id}` 的候选列是不分页的同一批
//! 行），`OutsourceSendableListOut` / `OutsourceSendableItem` 随之删除。本类型保留并
//! 继续被看板候选卡消费（`vo/queue.rs::OutsourceQueueCandidate::company_options`）——
//! 它同时是 repo 层 `array_agg` 的解码目标，搬走会让 `board/repo.rs` 反向依赖
//! `vo/sendable.rs` 的路径。

use serde::{Deserialize, Serialize};

use crate::shared::types::serialize_i64;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutsourceCompanyOption {
    #[serde(serialize_with = "serialize_i64")]
    pub id: i64,
    pub name: String,
}

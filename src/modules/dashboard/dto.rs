//! dashboard 域 DTO
//!
//! 本文件承载 **HTTP 入参** 类型（`GET /api/v2/dashboard/snapshot` 的 query 口径枚举）。
//! dashboard 域无 HTTP request body 入参（鉴权走 query `WsQuery` 字段 token + JWT 验签，
//! 定义在 handler.rs）。`SnapshotQuery` 结构体本身也在 handler.rs（与 `WsQuery` 同处，
//! 便于对照两个入口的入参面），本文件只放需要被 repo / service 层引用的口径枚举。
//!
//! ## 与 `super::vo` 的边界
//! 出参 VO（snapshot 子结构 / WS 消息外壳）全部位于 `super::vo`；本文件只承载入参侧
//! 类型，且入参侧只有枚举——不带字段的结构体留在 handler.rs。
//!
//! ## `DeliveryBasis`（2026-10-04 新增）
//! `upcoming_delivery[]` 分桶的交期口径：`planned` 走 `t_part.planned_delivery_date`，
//! `system` 走 `t_part.system_delivery_date`。默认 `planned`（`Default` impl），
//! service 层 `unwrap_or_default()` 收敛成唯一默认值决策点。
//!
//! 非法取值（如 `?basis=xxx`）由 axum `Query` 反序列化直接 4xx，不在本层自写校验
//! 返回业务错误码（与 part 域 DTO 行为对齐）。
//!
//! `Copy + PartialEq + Eq` 是刻意约束：repo 层形参收值传递（不做二次兜底），
//! `Copy` 让调用点无需克隆。

use serde::Deserialize;

/// 形参 `basis` 语义见模块 doc `DeliveryBasis` 章节。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DeliveryBasis {
    /// 计划交期 `t_part.planned_delivery_date`（缺省口径，与 WS 路径一致）。
    #[default]
    Planned,
    /// 系统交期 `t_part.system_delivery_date`。
    System,
}
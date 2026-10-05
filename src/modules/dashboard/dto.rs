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

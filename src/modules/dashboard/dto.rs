use serde::Deserialize;

/// 交期口径（柱状图分桶 / 下钻抽屉二选一）。
///
/// 2026-10-07 缺省从 `Planned` 改为 `System`：前端默认显示系统交期，后端缺省
/// 与之保持一致（口径不一致时前端会出现「切了口径数字没变」的现象）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DeliveryBasis {
    /// 计划交期 `t_part.planned_delivery_date`
    Planned,
    /// 系统交期 `t_part.system_delivery_date`（缺省口径）
    #[default]
    System,
}

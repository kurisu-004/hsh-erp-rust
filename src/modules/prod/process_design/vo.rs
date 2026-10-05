//! prod::process_design 子模块 VO —— 出参（handler 响应序列化层）
//!
//! 2026-10-05 新增：与 `prod::programming` 同形 VO 模块，仅 `Serialize`
//! 不 `Deserialize`（禁止出现在 axum extractor 反序列化侧）。
//!
//! i64 一律走 `serialize_i64` / `serialize_i64_opt` → JSON string（雪花 ID
//! > 2^53，JS `Number` 会丢精度，参见 `shared::types` 模块 doc）。
//!
//! 字段集**刻意收窄到 7 个**（零件标识 + 展示 + 工序链 / 装配件归属两个闸门标记），
//! 不加客户 / 交期 / 数量 / `status` 等字段 —— 本页只做「选零件 → 定工序」，
//! 展示信息够用即可，且与 part 域 `GET /parts` 的 20 余字段出参逐字不同会让前端
//! 多一层类型适配。

use serde::Serialize;

use crate::shared::types::{serialize_i64, serialize_i64_opt};

/// `GET /api/v2/prod/process-design/parts` 单条结构。
///
/// 字段顺序按业务语义分组：标识 → 展示字段 → 两个闸门标记。
#[derive(Debug, Clone, Serialize)]
pub struct ProcessDesignPartItemOut {
    #[serde(serialize_with = "serialize_i64")]
    pub id: i64,
    /// 乐观锁版本号（`t_part.version`）。
    ///
    /// ⚠️ 当前**无消费方**（本页暂不在本列表上改 part）：预留给未来「在本页改零件」
    /// 的 OCC 回传。与 part 域 `PartOut.version` 是同一列、同一语义。
    pub version: i32,
    /// 零件序列号（`varchar(15)`，手工工单可空 → 排序时 `NULLS LAST`）
    pub serial_no: Option<String>,
    pub name: String,
    pub drawing_no: String,
    /// 已绑定的工艺链 id（`serialize_i64_opt` → JSON string / null）；
    /// `null` = **尚未制定工序**，本页的主闸门标记（前端据此显示「待制定」）。
    #[serde(serialize_with = "serialize_i64_opt")]
    pub process_chain_id: Option<i64>,
    /// 所属装配件 id（`serialize_i64_opt` → JSON string / null）；
    /// `null` = 独立零件，非 `null` = 装配件的子零件。
    ///
    /// ⚠️ 本端点**刻意不**按该列过滤（不设 `AND assembly_id IS NULL`）——
    /// 见 [`super`] 模块 doc 的警示段。该字段的作用是让前端能**标注**子件归属，
    /// 而不是把它从列表里剔除。
    #[serde(serialize_with = "serialize_i64_opt")]
    pub assembly_id: Option<i64>,
}

/// `GET /api/v2/prod/process-design/parts` 顶层响应。
///
/// ⚠️ `total` / `limit` / `offset` 是**分页计数类 `i64`，故意不走
/// `serialize_i64`**：它们是行数 / 偏移量，远小于 `2^53`，不存在 JS 精度截断风险；
/// 序列化形态与 `prod::programming` 的 `ProgrammingListOut`（以及 part 域
/// `PartListOut`）**逐字一致**，前端从 `GET /parts` 切到本端点时该层无需改动。
/// 只有雪花 ID 字段（`ProcessDesignPartItemOut::id` / `process_chain_id` /
/// `assembly_id`）需要 `serialize_i64` → JSON string。
#[derive(Debug, Clone, Serialize)]
pub struct ProcessDesignPartListOut {
    pub items: Vec<ProcessDesignPartItemOut>,
    pub total: i64,
    pub limit: i64,
    pub offset: i64,
}

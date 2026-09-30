//! prod::programming 子模块 VO —— 出参（handler 响应序列化层）
//!
//! 2026-10-01 新增：与 `prod::batch` / `worker_pool` 同形 VO 模块，仅 `Serialize`
//! 不 `Deserialize`（禁止出现在 axum extractor 反序列化侧）。
//!
//! i64 一律走 `serialize_i64` → JSON string（雪花 ID > 2^53，JS `Number` 会丢
//! 精度，参见 `shared::types` 模块 doc）。
//!
//! 字段集**刻意收窄到 13 个**（工单标识 + 展示 + 交期 + 客户 + CNC 程序标记），
//! 不加 `match_reason` 之类诊断字段——本端点只做「筛选 + 列表」，命中原因由规则
//! 语义（链含 CNC / 批次在 CNC 工序）表达，前端不需要逐行归因。

use chrono::NaiveDate;
use serde::Serialize;

use crate::shared::types::serialize_i64;

/// `GET /api/v2/prod/programming/pending` 单条结构。
///
/// 字段顺序按业务语义分组：工单标识 → 展示字段 → 交期 → 客户 → CNC 程序标记。
#[derive(Debug, Clone, Serialize)]
pub struct ProgrammingItemOut {
    #[serde(serialize_with = "serialize_i64")]
    pub id: i64,
    /// 乐观锁版本号（前端行内操作回传用）。
    pub version: i32,
    /// 工单序列号（手工工单可空）
    pub serial_no: Option<String>,
    pub name: String,
    pub drawing_no: String,
    pub quantity: i32,
    pub status: String,
    pub is_urgent: bool,
    /// 计划交期（`String` 而非 `NaiveDate`；NULL 走 `"1970-01-01"` 兜底，
    /// DB `NOT NULL` 已保证非空，此为防御性）。
    pub planned_delivery_date: String,
    /// 系统交期（`NaiveDate` 原生序列化；NULL → JSON `null`）。
    pub system_delivery_date: Option<NaiveDate>,
    /// L2 叶子客户名
    pub customer_name: Option<String>,
    /// L1 一级集团名；L2 未挂 parent 时为 None
    pub parent_customer_name: Option<String>,
    /// 是否已上传 G_CODE（真相源 `EXISTS t_part_file kind='G_CODE' AND
    /// deleted_at IS NULL`，与 part 域旧端点 / worker_pool 候选池同源）。
    pub has_cnc_program: bool,
}

/// `GET /api/v2/prod/programming/pending` 顶层响应。
///
/// ⚠️ `total` / `limit` / `offset` 是**分页计数类 `i64`，故意不走
/// `serialize_i64`**（2026-10-01 review 第 1 轮 C 项确认）：它们是行数 / 偏移量，
/// 远小于 `2^53`，不存在 JS 精度截断风险；序列化形态与
/// `part/vo/part.rs::PartListOut`（以及被替换的 part 域旧端点）**逐字一致**，
/// 前端从旧端点切到本端点时该层无需改动。只有雪花 ID 字段（`ProgrammingItemOut::id`）
/// 需要 `serialize_i64` → JSON string。
#[derive(Debug, Clone, Serialize)]
pub struct ProgrammingListOut {
    pub items: Vec<ProgrammingItemOut>,
    pub total: i64,
    pub limit: i64,
    pub offset: i64,
}

//! wx::part_list 子模块 model 层 —— `FromRow` 行结构
//!
//! 2026-10-11 新增。
//!
//! ## ⚠️ 刻意**不** derive `Serialize`
//! `model.rs` 是「SQL 投影的原始快照」，不是响应结构。JSON 契约的唯一真源是
//! [`super::vo`]：由 service 层显式投影（字段名 camelCase、雪花 id 字符串化、
//! 无 `drawingUrl`…）。给 model 加 `Serialize` 等于开一条「绕过 service 直接把
//! 行结构当响应」的旁路，那条旁路上没有任何 serde 属性，字段名全是 snake_case ——
//! 与 `docs/api/wx.md` §2 的契约逐字不符。

use chrono::NaiveDate;
use sqlx::FromRow;

/// `t_part` 的一行（+ 客户名 + 当前活跃批次号 + 已交件数）。
///
/// 字段与 [`super::repo::SELECT_COLS`] 的列**一一对应**（alias 名逐字对齐字段名），
/// `FromRow` 才能取到值。
#[derive(Debug, Clone, FromRow)]
pub struct PartListRow {
    /// `t_part.id`（雪花 ID）
    pub id: i64,
    /// `t_part.serial_no`（可为 NULL —— 手工工单没序列号）
    pub serial_no: Option<String>,
    /// `t_part.name`
    pub name: String,
    /// `t_part.drawing_no`（前端卡片里的 `code`）
    pub drawing_no: String,
    /// `t_part.quantity`（工单总件数 → 前端 `totalQty` / `batchQty`）
    pub quantity: i32,
    /// `t_part.status`（DB 原值；VO 里会被折叠成 7 类 tab 值）
    pub status: String,
    /// `t_part.system_delivery_date`（**可空**：无交期工单是 NULL）
    ///
    /// 2026-10-12 变更：原字段取 `p.planned_delivery_date`（NOT NULL），本次
    /// 随筛选谓词一起改打 `system_delivery_date` —— 小程序 `date-nav-bar` 展示的
    /// 是「系统交期」，用它当谓词才能让日期栏真正生效。VO 的 `dueDate` 随之改成
    /// `Option<String>`（`noSystemDate` tab 的行必须是 JSON `null`）。
    pub system_delivery_date: Option<NaiveDate>,
    /// `t_customer.name`（`LEFT JOIN`，可为 NULL）
    pub customer_name: Option<String>,
    /// `t_part.assembly_id`（非空 = 装配件子件 ⇒ 卡片按 `kind=batch` 呈现）
    pub assembly_id: Option<i64>,
    /// 当前活跃批次的 `t_part_batch.id`。
    ///
    /// ⚠️ **进 model 不进 VO**（2026-10-11 字段级移除，见 `docs/api/wx.md` §6）。
    /// 保留是为让 model 保持「SQL 投影完整快照」，省得将来要用时再改 SQL。
    pub current_batch_id: Option<i64>,
    /// 当前活跃批次的 `t_part_batch.batch_no`（`kind=batch` 时进 VO 的 `batchNo`；
    /// 无活跃批次时 NULL）。⚠️ 前端 `BatchPartCard.batchNo` 声明为 `string`，转换在
    /// 小程序侧映射层，类型差登记在 `docs/api/wx.md` §8.11
    pub current_batch_no: Option<i32>,
    /// 已交件数（`SUM(t_part_batch.quantity)` where `status IN
    /// ('DELIVERED','COMPLETED')` 且未软删，COALESCE 0）
    pub delivered_qty: i32,
}

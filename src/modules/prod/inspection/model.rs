//! prod::inspection 子模块 行模型 —— repo 的 SQL 投影行结构
//!
//! 2026-10-05 新增：3 个行结构（`ScanPartRow` / `ScanAssemblyRow` / `ScanBatchRow`），
//! 字段与 [`super::vo`] 的同名出参结构**一一对应**。
//!
//! 2026-10-07 新增：[`InspectionQueueRow`]（待品检队列读的 repo ↔ service 边界类型，
//! 随 `GET /api/v2/prod/inspection/queue` 自 `prod::batch` 迁入）。
//!
//! ## 为什么行结构与 VO 分开两层
//! 行结构是 SQL 的投影（列名 + DB 原生类型 + 可能的 `LEFT JOIN` 空值），
//! VO 是响应契约（`i64` 走 `serialize_i64`、批次挂 `children` 树）。两者字段名
//! 刻意保持一致，让 service 里的 row→vo 投影是逐字段直传、无重命名。
//!
//! ## 字段集刻意收窄
//! - [`ScanPartRow`] 只读 11 列（不含 `applicant_name` / `note` / 价格三列 /
//!   `process_chain_id` / `next_process_id`），扫码树只渲染「标识 + 展示 + 两个
//!   归属闸门」
//! - [`ScanAssemblyRow`] 只读 9 列 —— 装配件节点**没有批次**（`t_assembly` 无
//!   `t_part_batch` 行），故不需要 `version`（OCC 锚在批次上）
//! - [`ScanBatchRow`] 只读 10 列 + `part_id` 供 service 内存分组（见
//!   `t_part_batch` **无序列号列**这一事实）
//! - [`InspectionQueueRow`] 读 13 列（3-JOIN 窄投影），字段与
//!   `vo::InspectionQueueItemOut` 逐字同形；SQL 侧列别名直接取语义名
//!   （`pb.id AS batch_id` 等），repo 层 1:1 搬运，service 只做形状转换
//!
//! 四个行结构里三个 `#[derive(sqlx::FromRow)]` —— `query_as!(Struct, …)` 宏需要它
//! 才能把 DB 行搬进结构体，且宏在编译期按列名 + 列类型逐字段校验。
//! [`InspectionQueueRow`] 的 SQL 由 `QueryBuilder` 动态拼装（动态 `ORDER BY` +
//! 可选过滤，宏无法固化），故同样手写 `FromRow`（列名由常量 SELECT 里的别名承接）。

use chrono::NaiveDate;
use sqlx::FromRow;

/// `t_part` 窄投影行（11 列）+ `LEFT JOIN t_customer` 的客户名。
///
/// 命中查询与装配件子件列表查询**共用同一组列**（SQL 字面量各写一份，`query_as!`
/// 宏要求字面量、不能插常量，见 [`super::repo`] 模块 doc）。
#[derive(Debug, Clone, FromRow)]
pub struct ScanPartRow {
    pub id: i64,
    /// `varchar(15)` 可空（手工工单可能没序列号）。
    pub serial_no: Option<String>,
    pub name: String,
    pub drawing_no: String,
    /// `t_part.status` 原文（8 态），前端按原文决定行内按钮显隐。
    pub status: String,
    pub quantity: i32,
    pub is_urgent: bool,
    pub system_delivery_date: Option<NaiveDate>,
    /// L2 叶子客户名；`LEFT JOIN` 命中不到（客户软删 / 悬空 id）时为 `None`。
    pub customer_name: Option<String>,
    /// `t_part.version` —— **仅展示**，不参与任何批次写操作的 OCC（见
    /// [`ScanBatchRow::version`]）。
    pub version: i32,
    /// 所属装配件 id；`None` = 独立零件。非 `None` 时本端点返回整棵装配件树。
    pub assembly_id: Option<i64>,
}

/// `t_assembly` 窄投影行（9 列）+ `LEFT JOIN t_customer` 的客户名。
///
/// 装配件节点**本身没有批次**（`t_assembly` 在 `t_part_batch` 里没有行），所以
/// 本结构不含 `version` —— 本端点不提供任何以装配件为锚的写操作。
#[derive(Debug, Clone, FromRow)]
pub struct ScanAssemblyRow {
    pub id: i64,
    /// `varchar(15)` 可空（老数据可能没派发序列号）。
    pub serial_no: Option<String>,
    pub name: String,
    pub drawing_no: String,
    /// `t_assembly.status` 原文（7 态，无 `OUTSOURCE`）。
    pub status: String,
    pub quantity: i32,
    pub is_urgent: bool,
    pub system_delivery_date: Option<NaiveDate>,
    pub customer_name: Option<String>,
}

/// `t_part_batch` 窄投影行（10 列 + 1 列分组键）。
///
/// 批次是本端点唯一的**写操作锚**（前端的 `to-ship` / `to-process` /
/// `to-inspection` 都以 `ScanBatchOut::id` 作路径参数、以
/// [`ScanBatchRow::version`] 作 OCC 锚），故 `version` 必须是
/// `t_part_batch.version`。
#[derive(Debug, Clone, FromRow)]
pub struct ScanBatchRow {
    pub id: i64,
    /// 分组键：批次归属的零件 id。`t_part_batch` **没有序列号列**，
    /// 「这个批次是不是被扫中的那个零件的」只能靠 service 在内存里比对
    /// `batch.part_id == 命中 part.id` 得出（见 `service.rs` 的 `is_scanned`）。
    pub part_id: i64,
    pub batch_no: i32,
    pub quantity: i32,
    /// `t_part_batch.status` 原文（8 态）。
    pub status: String,
    /// `t_part_batch.version` —— 前端作 OCC 锚回传，**不是** `t_part.version`。
    pub version: i32,
    /// 返修中标记（`REPAIRING` 降级后的 boolean 列）；前端据此禁用「指定工序」。
    pub is_repairing: bool,
    /// 位置（`PRODUCTION_SHELF` / `WORKER` / `INSPECTION_SHELF` / `OFFICE` …）。
    pub location: Option<String>,
    /// 当前位置持有者名（`t_shelf` / `t_worker` / `t_outsource_company` 三表
    /// `COALESCE`，口径见 [`super::repo`] 模块 doc 的已知缺陷段）。
    pub current_holder_display: Option<String>,
    /// 批次当前工序名，来自 `current_process_id` → `t_process.name`。
    ///
    /// ⚠️ `INSPECTION` / `DELIVERED` 批次**恒为 `None`**：这两态按「出池清
    /// `current_process_id`」不变式把该列置 NULL，这是**正确**结果不是缺陷。
    pub process_name: Option<String>,
}

// ===== 待品检队列（`GET /queue`） =====

/// `GET /api/v2/prod/inspection/queue` 单行中间结构（repo ↔ service 边界类型）。
///
/// 2026-10-07 自 `prod::batch::model` 迁入（域迁移，字段与投影逐字不变）。
/// 待品检页只渲染 7 个数据列（序列号 / 图号 / 名称 / 批次 / 数量 / 系统交期 /
/// 客户），故只投 13 列 —— 不投 holder / 工序 / 送货单。
/// `l1_customer_name` 的派生在 repo 层完成（原料列 `c.parent_id` / `pc.name`）。
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct InspectionQueueRow {
    pub batch_id: i64,
    pub part_id: i64,
    pub batch_no: i32,
    pub quantity: i32,
    /// OCC 锚 `t_part_batch.version`（不是 `t_part.version`）。
    pub version: i32,
    pub serial_no: Option<String>,
    pub drawing_no: String,
    pub name: String,
    /// 系统交期（页面已不显示计划交期，日期筛选改筛本列）。
    pub system_delivery_date: Option<NaiveDate>,
    pub is_urgent: bool,
    pub customer_id: i64,
    pub customer_name: Option<String>,
    /// L1 集团名（由 `c.parent_id` 是否为空派生，口径见 [`super::repo`]）。
    pub l1_customer_name: Option<String>,
}

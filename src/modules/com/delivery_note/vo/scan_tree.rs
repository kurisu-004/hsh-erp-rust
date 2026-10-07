//! 扫码三层树出参（`GET /api/v2/com/delivery/note/scan/{serial_no}`）
//!
//! ## 树形状
//! ```text
//! DeliveryScanTreeOut
//! ├─ hit_kind: string                  "ASSEMBLY" | "PART"
//! ├─ scanned_serial_no: string         trim 后的回显
//! ├─ draft: DeliveryScanDraftOut|null  ★ 该 L1 现有的 DRAFT（无则 null，本端点不建单）
//! ├─ assembly: DeliveryScanAssemblyOut|null
//! └─ children: Vec<DeliveryScanPartOut>   装配件树 = 全部子件；独立件树 = [该件]
//!    └─ children: Vec<DeliveryScanBatchOut>
//! ```
//! 与 `prod::inspection` 的 `GET /scan/{serial_no}` **同形**（同一个前端树组件可以
//! 直接复用），但字段集不同：本域额外需要 `draft`（入单落点）、`customer_id`（L1
//! 一致性校验）、`entry_max_quantity` / `entry_max_sets`（可入单量闸门）与
//! `occupied_by_note_no`（批次占用提示）。
//!
//! ## VO 规约（2026-10-08 新增，域内自用）
//! 本文件的字段类型**只能**来自 `delivery_note/vo/` 或 `chrono`。跨域数据
//! （`TPart` / `TAssembly` / `TCustomer` / `TPartBatch`）必须在 service 层摊平成
//! 域内标量后再装配 —— VO 里不出现他域行模型，也不 `pub use` / `pub type` 别名
//! 指向他域类型。

use chrono::NaiveDate;
use serde::Serialize;

/// `GET /api/v2/com/delivery/note/scan/{serial_no}` 顶层响应（扫码树）。
#[derive(Debug, Clone, Serialize)]
pub struct DeliveryScanTreeOut {
    /// 命中来源：`"ASSEMBLY"` = 扫到装配件条码；`"PART"` = 扫到独立件或装配件子件
    /// 的条码。
    ///
    /// 判定顺序固定「先 `t_part` 后 `t_assembly`」：`t_part` 在前意味着子件码永远
    /// 不会被误判成装配件码。
    pub hit_kind: String,
    /// 回显 trim 后的扫码串，便于前端把扫码结果与历史记录对齐。
    pub scanned_serial_no: String,
    /// 该 L1 名下现有的 DRAFT 送货单（`POST /scan` 的落点）。无则 `null`。
    ///
    /// ⚠️ **本端点是纯读，绝不建单**。`null` 的含义是「这次扫码会新建一张草稿」，
    /// 不是「扫码失败」。
    pub draft: Option<DeliveryScanDraftOut>,
    /// 装配件节点。`hit_kind == "ASSEMBLY"` 或扫中的零件是某个装配件的子件时有值；
    /// 独立件树（含「父装配件已软删」的退化情形）为 `null`。
    ///
    /// ⚠️ 装配件节点**没有批次**：`t_assembly` 在 `t_part_batch` 里没有行，批次只挂
    /// 在 `DeliveryScanPartOut::children` 上。
    pub assembly: Option<DeliveryScanAssemblyOut>,
    /// 顶层零件节点。装配件树 = 该装配件的**全部**子件（不止被扫中的那个）；
    /// 独立件树 = `[被扫中的那个 part]`。
    ///
    /// 恒为数组（装配件无活跃子件时是空数组，不返回 `null`）—— 少一层 `?? []`。
    pub children: Vec<DeliveryScanPartOut>,
}

/// 现有 DRAFT 送货单概要（`draft` 字段）。
#[derive(Debug, Clone, Serialize)]
pub struct DeliveryScanDraftOut {
    #[serde(serialize_with = "crate::shared::types::serialize_i64")]
    pub note_id: i64,
    pub note_no: String,
    /// OCC 锚：客户端把它原样回传给 `POST /scan` 的 `note_version`。
    pub version: i32,
    pub status: String,
}

/// 装配件节点（无批次）。
#[derive(Debug, Clone, Serialize)]
pub struct DeliveryScanAssemblyOut {
    #[serde(serialize_with = "crate::shared::types::serialize_i64")]
    pub id: i64,
    pub serial_no: Option<String>,
    pub name: String,
    pub drawing_no: String,
    /// `t_assembly.status` 原文（7 态）。
    pub status: String,
    /// 工单总套数（`t_assembly.quantity`）—— 「送 N 套」里的 N 的上界。
    pub quantity: i32,
    pub is_urgent: bool,
    pub system_delivery_date: Option<NaiveDate>,
    /// L2 叶子客户名（`LEFT JOIN t_customer`，客户软删时退化为 `null`）。
    pub customer_name: Option<String>,
    /// `t_assembly.customer_id`（L2 客户 id）。
    ///
    /// 用途：前端展示 + `POST /scan` 的 L1 一致性校验（服务端另有一道 21416 闸门，
    /// 本字段不是信任边界）。
    #[serde(serialize_with = "crate::shared::types::serialize_i64")]
    pub customer_id: i64,
    /// **可入单套数上限**：该装配件当前**全部子件**的「可入单」批次
    /// （`READY_TO_SHIP` + 未占用）按 `shippable_sets` 公式算出的 `min(per_set)`，
    /// 并以 `assembly.quantity` 收口。
    ///
    /// `POST /scan` 里 `sets > entry_max_sets` ⇒ 21405。不传 `sets`（= 全部）时
    /// 前端用它做上限提示。
    pub entry_max_sets: i32,
    /// 每套各子件应入单的数量：`per_set_quantity = part.quantity /
    /// assembly.quantity`（**整数除法，向零截断**）。
    ///
    /// 装配序按 `part.id ASC`。前端用它把「送 N 套」翻译成每个子件的件数；
    /// 服务端在 `POST /scan` 里按同一公式重算，不信任客户端传来的值。
    pub per_set_parts: Vec<DeliveryScanPerSetPartOut>,
}

/// 装配件「每套用量」一项。
#[derive(Debug, Clone, Serialize)]
pub struct DeliveryScanPerSetPartOut {
    #[serde(serialize_with = "crate::shared::types::serialize_i64")]
    pub part_id: i64,
    pub per_set_quantity: i32,
}

/// 零件节点（其 `children` 是该零件的全部批次）。
#[derive(Debug, Clone, Serialize)]
pub struct DeliveryScanPartOut {
    #[serde(serialize_with = "crate::shared::types::serialize_i64")]
    pub id: i64,
    pub serial_no: Option<String>,
    pub name: String,
    pub drawing_no: String,
    /// `t_part.status` 原文（8 态）。
    pub status: String,
    /// 工单总件数（`t_part.quantity`）。
    pub quantity: i32,
    pub is_urgent: bool,
    pub system_delivery_date: Option<NaiveDate>,
    pub customer_name: Option<String>,
    /// `t_part.version` —— **仅展示**。任何批次写动作的 OCC 锚都是
    /// [`DeliveryScanBatchOut::version`]（批次版本），本字段不参与。
    pub version: i32,
    /// `t_part.customer_id`（L2 客户 id），用途同
    /// [`DeliveryScanAssemblyOut::customer_id`]。
    #[serde(serialize_with = "crate::shared::types::serialize_i64")]
    pub customer_id: i64,
    /// **可入单件数上限**：该零件当前「可入单」批次（`READY_TO_SHIP` +
    /// `delivery_note_id IS NULL`）的 `quantity` 合计。
    ///
    /// `POST /scan` 里 `quantity > entry_max_quantity` ⇒ DP 分配不可行 ⇒ 21405。
    pub entry_max_quantity: i32,
    /// 该零件的全部批次（**不按状态过滤**，含 `COMPLETED` / `CANCELLED` 等终态），
    /// 按 `batch_no ASC, id ASC` 排序。
    ///
    /// ⚠️ 不过滤的理由与 `prod::inspection` 同：扫码弹窗要回答「这批货总共分了
    /// 几批、每批现在什么状态」，砍掉终态就答不了；**状态闸门在前端**。
    pub children: Vec<DeliveryScanBatchOut>,
}

/// 批次节点（树里唯一的写动作锚）。
#[derive(Debug, Clone, Serialize)]
pub struct DeliveryScanBatchOut {
    /// `t_part_batch.id`。
    #[serde(serialize_with = "crate::shared::types::serialize_i64")]
    pub id: i64,
    pub batch_no: i32,
    pub quantity: i32,
    /// `t_part_batch.status` 原文。**后端不过滤、不改写**，前端据此显示 / 禁用按钮。
    pub status: String,
    /// `t_part_batch.version` —— 批次乐观锁版本（**不是** `t_part.version`）。
    pub version: i32,
    pub is_repairing: bool,
    /// 位置（`PRODUCTION_SHELF` / `WORKER` / `INSPECTION_SHELF` / `OFFICE` …）。
    pub location: Option<String>,
    /// 当前位置持有者名（货架 / 工人 / 外协公司三表 `COALESCE`）。
    pub current_holder_display: Option<String>,
    /// 批次当前工序名（`current_process_id` → `t_process.name`）。
    ///
    /// ⚠️ `INSPECTION` / `DELIVERED` 批次恒为 `null`（出池清 `current_process_id`
    /// 不变式的正确结果）。
    pub process_name: Option<String>,
    /// 该批次所属零件就是被扫中的那个 → 前端高亮。装配件条码命中时全为 `false`
    /// （装配件码没有零件身份）。
    pub is_scanned: bool,
    /// 已被哪张送货单占用（`t_part_batch.delivery_note_id` → 单号）。
    ///
    /// `null` = 未被占用（可入单）；非空即「已被 DN-20260110-0007 占用」，前端
    /// 应禁用该批次的勾选。⚠️ JOIN 带 `dn.deleted_at IS NULL`：被**软删**的单占用
    /// 的批次视为未占用（`POST /scan` 侧也只把 `DRAFT` / `SUBMITTED` 单的占用算
    /// 冲突）。
    pub occupied_by_note_no: Option<String>,
}

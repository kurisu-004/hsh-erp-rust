//! delivery_note 域 P2 送货单 CRUD 端点响应 VO

use chrono::{NaiveDate, NaiveDateTime};
use serde::Serialize;

/// 送货单概要（list + 大部分接口的公共响应）。
///
/// ## 2026-10-08 字段裁剪（15 个）
///
/// **范围 5 个**：`delivery_group_id` / `delivery_group_name` /
/// `leaf_customer_id` / `leaf_customer_name` / `scope_label`。范围三态判定已下线、
/// 建单判定键收敛为 `(customer_id, DRAFT)` 单键 ⇒ 「这张单属于哪个分组 / 哪个单厂」
/// 不再是单据属性，展示口径统一退化为「L1 客户名 + `customer_path`」。
///
/// **零读 6 个**（后端真在发但前端从不读）：`parent_customer_name`（前端统一读
/// `customer_path`）/ `submitted_by` / `picked_up_by`（只有 id 没有姓名，页面无从
/// 展示）/ `driver_worker_id`（用 `driver_worker_name` 判空即可）/ `updated_at`
/// （只有 `created_at` 有展示位）。
///
/// **`created_at` 保留**：一带的单据列表 / 详情都显示创建时间。
///
/// 保留字段的共同点是**前端 Zod schema 与页面都在用**；裁剪的依据是「前端仓零读」
/// + 「没有姓名佐证的裸 id 无法展示」两条。
#[derive(Debug, Clone, Serialize)]
pub struct DeliveryNoteOut {
    #[serde(serialize_with = "crate::shared::types::serialize_i64")]
    pub id: i64,
    pub version: i32,
    pub delivery_note_no: String,
    #[serde(serialize_with = "crate::shared::types::serialize_i64")]
    pub customer_id: i64,
    pub customer_name: Option<String>,
    /// 「L1 / L2」完整路径（L1 自指时只给 L1 名）。前端统一读这个字段展示客户，
    /// 不再单独读 `parent_customer_name`（2026-10-08 已删）。
    pub customer_path: Option<String>,
    pub status: String,
    pub submitted_at: Option<NaiveDateTime>,
    pub picked_up_at: Option<NaiveDateTime>,
    /// 指定司机的姓名（`t_worker.name`，随 `driver_worker_id` 派生）。
    ///
    /// 前端判断「是否已指定司机」读**本字段判空**即可（打印对话框的「导出」按钮
    /// disabled 逻辑就是它）—— 不需要 `driver_worker_id`。
    pub driver_worker_name: Option<String>,
    pub part_count: i64,
    pub note: Option<String>,
    pub delivery_date: Option<NaiveDate>,
    pub created_at: NaiveDateTime,
}

/// 送货单下一行零件的投影（行 = 批次；`id` = batch_id）
///
/// 2026-10-08 删掉 3 个恒定值字段：`is_urgent`（两处装配都硬编码 `false`，注「TPart
/// 当前投影不含该列」——`t_part` 明明有 `is_urgent`，是投影漏了，补上会与扫码树的
/// `DeliveryScanPartOut::is_urgent` 语义重复，故删 VO 而不是补数据）；
/// `is_scanned` 与 `scanned`（成对的「兼容字段」，两处都硬编码 `false`，前端两个都
/// 不读）。
#[derive(Debug, Clone, Serialize)]
pub struct DeliveryNoteLineItem {
    /// 批次 id（行身份）
    #[serde(serialize_with = "crate::shared::types::serialize_i64")]
    pub id: i64,
    /// 工单 id
    #[serde(serialize_with = "crate::shared::types::serialize_i64")]
    pub part_id: i64,
    pub batch_no: i32,
    pub batch_label: String,
    pub serial_no: String,
    pub drawing_no: String,
    pub name: String,
    pub quantity: i32,
    pub status: String,
    pub applicant_name: Option<String>,
    pub request_date: Option<NaiveDate>,
    pub planned_delivery_date: Option<NaiveDate>,
    pub system_delivery_date: Option<NaiveDate>,
    pub order_no: Option<String>,
    pub note: Option<String>,
    pub customer_name: Option<String>,
    pub parent_customer_name: Option<String>,
    pub customer_path: Option<String>,
    /// L2 叶子客户 id（`t_part.customer_id`）—— 打印分组时按它查
    /// `t_delivery_group_member` 定位分组。
    ///
    /// 2026-10-08 新增。⚠️ 分组键**必须是 id 而不是 `customer_name`**：
    /// `t_customer.name` 只有**非唯一** btree 索引，同名 L2 会被并进同一张 sheet，
    /// 而打印产物是客户签字的收货凭证 —— 收货单位归属错了是业务事故。
    #[serde(serialize_with = "crate::shared::types::serialize_i64")]
    pub customer_id: i64,
    /// 装配件父行字段（仅子件行填；散件 None）
    #[serde(
        serialize_with = "crate::shared::types::serialize_i64_opt",
        skip_serializing_if = "Option::is_none"
    )]
    pub assembly_id: Option<i64>,
    pub assembly_serial_no: Option<String>,
    pub assembly_drawing_no: Option<String>,
    pub assembly_name: Option<String>,
    pub assembly_order_no: Option<String>,
    /// 2026-10-04 新增：装配件工单总套数（`t_assembly.quantity`）。
    ///
    /// 仅子件行填；散件为 `None`。`Option<i32>` 走普通 serde（不需要
    /// `serialize_i64_opt` —— 那套是给 > 2^53 的雪花 id 用的）。
    pub assembly_quantity: Option<i32>,
    /// 本单可出货套数（**只统计本单** `READY_TO_SHIP` 批次，口径见
    /// `service::shippable_sets`）。
    ///
    /// `min` 的定义域是「该装配件的**全部**子件」：本单没交批次的子件以 0 参与
    /// ⇒ 凑不齐整套就是 0。
    ///
    /// ⚠️ 必须与扫码树 `DeliveryScanAssemblyOut::entry_max_sets` **逐字同值** ——
    /// 两者走同一份 `shippable_sets` 公式、同一子件集、同一分子过滤（2026-10-08
    /// 打印链路下线后，这个约束从「详情 vs 打印预览」变成「详情 vs 扫码弹窗」）。
    ///
    /// 仅子件行填；散件为 `None`。0 = 凑不齐整套。装配件被软删 / 不存在时同样是
    /// `None`（与 `assembly_id` 同口径：都取「解析到的装配件」）。
    pub shippable_sets: Option<i32>,
}

/// 送货单详情（head + line_items）。
///
/// 2026-10-08 删掉恒为空的 `scanned_serials`（两处装配都是 `vec![]`；送货台
/// `POST /{id}/pickup-scan` 端点删除后前端零读）。
#[derive(Debug, Clone, Serialize)]
pub struct DeliveryNoteDetailOut {
    #[serde(flatten)]
    pub head: DeliveryNoteOut,
    pub line_items: Vec<DeliveryNoteLineItem>,
}

/// `GET /api/v2/com/delivery/note/batch-detail?ids=...` 响应载体。
///
/// 仅作为 `items: [DeliveryNoteDetailOut]` 的轻量封装，避免 schema 顶层直接
/// 给出数组（信封 `data` 不能是裸数组）。`DeliveryNoteDetailOut` 自身已
/// `#[serde(flatten)] head: DeliveryNoteOut`，因此每个 item 在 wire 上仍是
/// head + `line_items` 的扁平结构。
#[derive(Debug, Clone, Serialize)]
pub struct BatchDeliveryDetailData {
    pub items: Vec<DeliveryNoteDetailOut>,
}

/// 一览响应（GET /api/v2/com/delivery/note；含分页总计）。
#[derive(Debug, Clone, Serialize)]
pub struct DeliveryNoteListOut {
    pub items: Vec<DeliveryNoteOut>,
    pub total: i64,
    pub limit: i64,
    pub offset: i64,
}

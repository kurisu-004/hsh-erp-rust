//! dashboard 域交期工单 VO（2026-10-07 新增）
//!
//! 字段集按前端实际渲染反推推导，契约见 `docs/api/dashboard.md`。
//!
//! ## id 序列化约定
//! `SystemDeliveryOrder.id` / `DeliveryOrderDetail.id` 是**雪花 ID 的字符串形态**
//! （防 JS 精度截断，与 `shared::types::serialize_i64` 的输出逐字等价）；而
//! `DeliveryOrderDetailOut.total` 保持**裸 i64 JSON number** —— 它是普通计数而非
//! 标识符，先例见 `part::vo::PartListOut`（`total` / `limit` / `offset` 裸 number，
//! 只有 `items[].id` 字符串化）。

use serde::Serialize;

/// 最紧急工单 / 部分已交 面板行（前端 SystemDeliveryOrdersPanel 消费的字段集 + 锚点）
#[derive(Debug, Clone, Serialize)]
pub struct SystemDeliveryOrder {
    /// 雪花 ID 字符串形态（前端 PartPreviewDialog 锚点）
    pub id: String,
    pub serial_no: Option<String>,
    pub name: String,
    pub quantity: i32,
    /// OrderStatus 字面量
    pub status: String,
    pub system_delivery_date: Option<String>,
    /// 二级客户名
    pub customer_name: Option<String>,
    pub is_urgent: bool,
    /// 已送数量（0 = 未交过）。服务端分桶判据就是它 > 0
    pub delivered_quantity: i32,
}

/// 两桶结果（2026-10-07：窗口 / 判定 / 截断全部下沉服务端，
/// 前端不再跑 splitForDashboard）
#[derive(Debug, Clone, Serialize)]
pub struct SystemDeliveryOrders {
    /// 未交过（delivered_quantity == 0），按 system_delivery_date ASC
    pub urgent: Vec<SystemDeliveryOrder>,
    /// 已交过一部分（delivered_quantity > 0），同上
    pub partial: Vec<SystemDeliveryOrder>,
}

/// 交期柱状图下钻抽屉行（前端 UpcomingDeliveryListDrawer 消费）
#[derive(Debug, Clone, Serialize)]
pub struct DeliveryOrderDetail {
    /// 雪花 ID 字符串形态（前端零件预览锚点）
    pub id: String,
    pub serial_no: Option<String>,
    pub drawing_no: String,
    pub name: String,
    pub l1_customer_name: Option<String>,
    pub customer_name: Option<String>,
    pub status: String,
    /// NOT NULL 列
    pub planned_delivery_date: String,
    pub system_delivery_date: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DeliveryOrderDetailOut {
    /// 回显请求的日期
    pub date: String,
    /// 回显请求的口径
    pub basis: String,
    /// 匹配总数（不受 items 截断影响），前端据此显示「共 N 件」
    pub total: i64,
    /// 最多 `DELIVERY_DETAIL_LIMIT` 行
    pub items: Vec<DeliveryOrderDetail>,
    pub ts: String,
}

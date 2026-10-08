//! dashboard 域交期工单 VO（2026-10-07 新增；2026-10-10 交期面板拆三桶）
//!
//! 字段集按前端实际渲染反推推导，契约见 `docs/api/dashboard.md`。
//!
//! ## id 序列化约定
//! `SystemDeliveryOrder.id` / `DeliveryOrderDetail.id` 是**雪花 ID 的字符串形态**
//! （防 JS 精度截断，与 `shared::types::serialize_i64` 的输出逐字等价）；而
//! `DeliveryOrderDetailOut.total` / `DeliveryBucket.total` 保持**裸 i64 JSON
//! number** —— 它们是普通计数而非标识符，先例见 `part::vo::PartListOut`（`total` /
//! `limit` / `offset` 裸 number，只有 `items[].id` 字符串化）。
//!
//! ## 三桶（2026-10-10）
//! 三桶**全部是工单级**，装配件算 1 条、子件不单独出行（子件只作为装配件已交量的
//! 计算中间量出现）。判据 / 窗口 / 截断全在服务端 SQL，前端不再自己过滤：
//!
//! | 桶 | 交期窗口 | 已交判据 | 排序 |
//! |---|---|---|---|
//! | `upcoming` | `system_delivery_date >= today` | 一件没交过 | `sdd ASC` |
//! | `overdue` | `system_delivery_date < today` | 一件没交过 | `sdd ASC` |
//! | `partial` | **无窗口** | 交过一部分 | `sdd ASC NULLS LAST` |
//!
//! `partial` 无窗口 ⇒ 它与 `overdue` 在时间范围上**必然重叠**（这是产品决议，不是缺陷；
//! 登记见 `docs/api/dashboard.md` §8.4）。

use serde::Serialize;

/// 交期面板行（前端 SystemDeliveryOrdersPanel 消费的字段集 + 锚点）
#[derive(Debug, Clone, Serialize)]
pub struct SystemDeliveryOrder {
    /// 雪花 ID 字符串形态（前端 PartPreviewDialog 锚点）
    pub id: String,
    pub serial_no: Option<String>,
    pub name: String,
    pub quantity: i32,
    /// OrderStatus / AssemblyStatus 字面量（两侧共用同一份 6 态白名单，`t_assembly`
    /// 侧天然只落 4 态）
    pub status: String,
    pub system_delivery_date: Option<String>,
    /// 二级客户名
    pub customer_name: Option<String>,
    pub is_urgent: bool,
    /// 件级：已交**件数**（`t_part` 侧 `SUM(t_part_batch.quantity)`）；
    /// 装配件级：已交**套数**（min 公式，见 `repo/delivery.rs::fetch_delivered_sets`）。
    /// 一行的语义随 `row_type` 变，前端展示单位必须跟着换。
    pub delivered_quantity: i32,
    /// 行来源：`"PART"`（`t_part`，`assembly_id IS NULL`）| `"ASSEMBLY"`（`t_assembly`）。
    /// SQL 里用字面量直接投影，前端据此选择「件 / 套」单位与下钻目标。
    pub row_type: &'static str,
}

/// 单桶结果：最多 `DELIVERY_BUCKET_LIMIT` 行 + **不受该截断影响**的匹配总数
/// （`COUNT(*) OVER ()`，SQL 侧窗口函数在 `LIMIT` 之前求值）。
#[derive(Debug, Clone, Serialize)]
pub struct DeliveryBucket {
    pub items: Vec<SystemDeliveryOrder>,
    /// 匹配总数（裸 JSON number，与 `overdue_count` 口径一致）
    pub total: i64,
}

/// 三桶结果（2026-10-10：交期面板拆 upcoming / overdue / partial 三块）
#[derive(Debug, Clone, Serialize)]
pub struct SystemDeliveryOrders {
    /// 交期在今天及以后 + 一件没交过，按 `system_delivery_date ASC`
    pub upcoming: DeliveryBucket,
    /// 交期早于今天 + 一件没交过，按 `system_delivery_date ASC`
    pub overdue: DeliveryBucket,
    /// 已交过一部分，**不限时间范围**，按 `system_delivery_date ASC NULLS LAST`
    pub partial: DeliveryBucket,
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

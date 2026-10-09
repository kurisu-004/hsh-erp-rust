//! wx::part_list 子模块出参 VO 层（仅 `Serialize`）
//!
//! 2026-10-11 新增。**逐字对齐前端卡片模型**（`wx-app/miniprogram/mock/parts.ts`
//! 的 `WorkOrderPartCard` / `BatchPartCard` / `CountsByStatus`）——**camelCase 字段名**，
//! 与本仓其它域（`part::vo` / `iam::vo` 的 snake_case）刻意不同。
//!
//! ## 为什么本域用 camelCase 而别域用 snake_case
//! 小程序侧 `part-card` 组件是 wx-js 手写模板，**没有**一层「后端 snake → 前端
//! camel」的映射（Web 端有 `services/*.ts` 的映射层）。逐字对齐后前端可直接把响应
//! 塞进 `PartCardItem`，省掉一整个映射文件与它的漂移面。
//!
//! ⚠️ 对照：`wx::login` 的 VO **仍**是 snake_case（`refresh_token` / `full_name`）——
//! 那里前端 `applyLoginResponse` 逐字读那几个键，改名即打断登录态。两处口径**刻意
//! 不同**，不要互相"对齐"。
//!
//! ## ❌ 没有 `drawingUrl`（2026-10-11 登记为已知有意缺口）
//! 前端 `BasePartCard` 有 `drawingUrl`，但 **`t_part` 无图纸列**，后端没有可信数据源
//! 可填。**本轮刻意不产出该字段**（也不加恒 `null` 的占位），小程序侧在自己的映射
//! 层用 `/asset/drawing/{code}.png` 本地兜底。等 COS 文件服务接入后单独 PR 补。
//!
//! ## `kind` 判定口径（既有行为，本次**未改**）
//! `assembly_id.is_some() → "batch"`，否则 `"workOrder"`。即**装配件的子件按批次
//! 卡片呈现**（`batchNo` / `batchQty` 取代 `customer` / `deliveredQty` /
//! `totalQty` / `status`）。该口径来自旧 `repo.rs::row_to_wx_part`，本次只是搬位置。
//!
//! ## ⚠️ 4 类 `status` 折叠的静默兜底
//! 前端只有 4 个 tab 值，`status` 字段也只可能取这 4 个之一。但 DB 的
//! `PROGRAMMING` / `OUTSOURCE` / `COMPLETED` / `CANCELLED` 不映射到任何 tab。**它们
//! 在 `list[].status` 里一律填 `"pendingProduction"`**（与旧前端 `mapStatus` 的
//! catch-all 分支逐字对齐，避免小程序渲染行为突变）——这是**静默兜底**：这类工单
//! 只计入 `counts.all`，却不会出现在任何单个 tab 的列表里。详见
//! [`docs/api/wx.md`](../../../../docs/api/wx.md) §8 已知偏差登记。

use serde::Serialize;

use crate::shared::types::serialize_i64;

// =============================================================================
// 卡片（判别联合）
// =============================================================================

/// 工单卡片判别联合：`#[serde(tag = "kind")]` ⇒ JSON 顶层多一个 `"kind"` 键。
///
/// ⚠️ 两个变体的 5 个公共字段（`id` / `serialNo` / `name` / `code` / `dueDate`）
/// **刻意各写一份**而不用 `#[serde(flatten)]` + 共享 base struct：flatten 与
/// internally-tagged 枚举的序列化组合是 serde 的边角行为（`TaggedSerializer`
/// 遇到 `FlatMapSerializer` 的行为没有文档保证），5 个字段的重复远小于这个风险。
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind")]
pub enum PartCardOut {
    /// 普通工单卡片（`assembly_id IS NULL`）。JSON `{"kind":"workOrder", …}`
    #[serde(rename = "workOrder")]
    WorkOrder(WorkOrderCard),
    /// 装配件子件卡片（`assembly_id` 非空，按批次维度呈现）。
    /// JSON `{"kind":"batch", …}`
    #[serde(rename = "batch")]
    Batch(BatchCard),
}

/// `kind = "workOrder"` 变体：对应前端 `WorkOrderPartCard`。
///
/// 字段顺序即 JSON 键序（serde 按声明序输出），与 `docs/api/wx.md` §2 的示例逐字
/// 一致。
#[derive(Debug, Clone, Serialize)]
pub struct WorkOrderCard {
    /// `t_part.id`（雪花 ID → **JSON string**，防 JS 精度截断）
    #[serde(rename = "id", serialize_with = "serialize_i64")]
    pub id: i64,
    /// `t_part.serial_no`（可为 null —— 手工工单没序列号）
    #[serde(rename = "serialNo")]
    pub serial_no: Option<String>,
    /// `t_part.name`
    pub name: String,
    /// `t_part.drawing_no`（前端卡片里的「图号」，叫 `code`）
    #[serde(rename = "code")]
    pub code: String,
    /// `t_part.planned_delivery_date`，格式恒为 `YYYY-MM-DD`（该列 NOT NULL）
    #[serde(rename = "dueDate")]
    pub due_date: String,
    /// `t_customer.name`（`LEFT JOIN t_customer`，可为 null）
    pub customer: Option<String>,
    /// **已交付件数**：`SUM(t_part_batch.quantity)` where 批次
    /// `status IN ('DELIVERED','COMPLETED')` 且未软删（无已交批次时为 `0`）
    #[serde(rename = "deliveredQty")]
    pub delivered_qty: i32,
    /// `t_part.quantity`（工单总件数）
    #[serde(rename = "totalQty")]
    pub total_qty: i32,
    /// **4 类 tab 值之一**：`pendingProduction` / `inProduction` /
    /// `pendingInspection` / `delivered`。DB 里其余状态静默折叠成
    /// `pendingProduction`（见模块 doc 的「4 类折叠的静默兜底」）
    pub status: String,
}

/// `kind = "batch"` 变体：对应前端 `BatchPartCard`。
///
/// ⚠️ 前端 `BatchPartCard` **没有** `status` 字段（前端类型注释明确写了
/// 「batch 变体的 status 语义由生产域自定义，复用 BasePartCard 会冲突」）。故本变体
/// **不含** `status`，小程序在批次卡片上拿不到 tab 归属。
#[derive(Debug, Clone, Serialize)]
pub struct BatchCard {
    /// `t_part.id`（子件 id；雪花 ID → **JSON string**）
    #[serde(rename = "id", serialize_with = "serialize_i64")]
    pub id: i64,
    /// `t_part.serial_no`
    #[serde(rename = "serialNo")]
    pub serial_no: Option<String>,
    /// `t_part.name`
    pub name: String,
    /// `t_part.drawing_no`
    #[serde(rename = "code")]
    pub code: String,
    /// `t_part.planned_delivery_date`，格式恒为 `YYYY-MM-DD`
    #[serde(rename = "dueDate")]
    pub due_date: String,
    /// 当前活跃批次的 `batch_no`（`LEFT JOIN LATERAL`，无可活跃批次时 null）。
    ///
    /// ⚠️ **JSON number / 可空**（2026-10-11 review 第 1 轮登记的与前端 TS 模型的
    /// 类型差）：前端 `BatchPartCard.batchNo` 声明为 **`string`**，但小程序侧有映射
    /// 层 `services/parts.ts::toPartCard` 做 `String(it.current_batch_no ?? 1)
    /// .padStart(2, '0')` —— 注意 `?? 1` 这个**兜底默认值 1**：本字段返 `null` 时
    /// 前端渲染成 `01` 而不是空白。这是既有前端行为，本轮不改，只登记以免后人把
    /// `Option<i32>` 当成「VO 逐字对齐前端模型」的证据。详见 `docs/api/wx.md` §8.11
    /// （端点 4/5 的同名字段是**非空** number，两域刻意一致）。
    #[serde(rename = "batchNo")]
    pub batch_no: Option<i32>,
    /// `t_part.quantity`（前端 `BatchPartCard.batchQty`；⚠️ **不是**当前批次的
    /// `t_part_batch.quantity`，口径是工单总件数，与前端映射层原实现一致）
    #[serde(rename = "batchQty")]
    pub batch_qty: i32,
}

// =============================================================================
// 计数
// =============================================================================

/// 4 个 tab 的计数 + 全部。对应前端 `CountsByStatus`（`Record<'all' | PartStatus, number>`）。
///
/// ⚠️ 归桶口径与 [`PartListHomeOut::counts`] 的 `status` 过滤**同源于 service 层
/// 的映射表**（`service::status_to_db_statuses` / `service::map_counts_by_status`），
/// 这是 2026-10-11 修掉的既有 bug：旧实现角标按 `READY_TO_SHIP + DELIVERED`
/// （实测 72 + 126 = 198）算，列表却只收单值 `DELIVERED`（最多 126），两边对不上。
#[derive(Debug, Clone, Serialize)]
pub struct PartCountsOut {
    /// 全部未软删工单数（**任何** DB 状态都计入，含 `CANCELLED`）
    pub all: i64,
    /// `t_part.status = 'PENDING'` 的行数
    #[serde(rename = "pendingProduction")]
    pub pending_production: i64,
    /// `t_part.status = 'IN_PROCESS'` 的行数
    #[serde(rename = "inProduction")]
    pub in_production: i64,
    /// `t_part.status = 'INSPECTION'` 的行数
    #[serde(rename = "pendingInspection")]
    pub pending_inspection: i64,
    /// `t_part.status IN ('READY_TO_SHIP','DELIVERED')` 的行数（**两个**状态合并）
    pub delivered: i64,
}

// =============================================================================
// 分页外壳
// =============================================================================

/// `GET /api/v2/wx/part-list` 出参：4 tab 角标 + 第 1 页卡片（**首屏聚合**）。
#[derive(Debug, Clone, Serialize)]
pub struct PartListHomeOut {
    /// 4 个 tab 的计数（**不带 `status` 过滤** —— 角标恒是全局口径）
    pub counts: PartCountsOut,
    /// 当前页卡片（按 `?status=` 过滤、按 `?page=` 翻页）
    pub list: Vec<PartCardOut>,
    /// 是否还有下一页（算法见 `docs/api/wx.md` §3：取 `size + 1` 条判超）
    #[serde(rename = "hasMore")]
    pub has_more: bool,
}

/// `GET /api/v2/wx/part-list/page` 出参：纯增量（上拉加载后续页）。
///
/// 与 [`PartListHomeOut`] 的**唯一**差别是**不带** `counts` —— 角标只在首屏聚合端点
/// 算一次，`/page` 每次上拉都重算 4 个 COUNT 是纯浪费。
#[derive(Debug, Clone, Serialize)]
pub struct PartListPageOut {
    /// 当前页卡片（与首屏端点同一查询路径，`?page=2` 时内容逐字相同）
    pub list: Vec<PartCardOut>,
    /// 是否还有下一页（算法同上）
    #[serde(rename = "hasMore")]
    pub has_more: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// `kind = "workOrder"` 的 JSON 必须**逐字**长成前端 `WorkOrderPartCard` 形态。
    #[test]
    fn work_order_card_serializes_to_frontend_shape() {
        let card = PartCardOut::WorkOrder(WorkOrderCard {
            id: 2256,
            serial_no: Some("F2256".into()),
            name: "壳体".into(),
            code: "E4201".into(),
            due_date: "2026-08-04".into(),
            customer: Some("六厂".into()),
            delivered_qty: 3,
            total_qty: 8,
            status: "inProduction".into(),
        });
        let v = serde_json::to_value(&card).expect("serialize workOrder card");
        assert_eq!(
            v,
            json!({
                "kind": "workOrder",
                "id": "2256",
                "serialNo": "F2256",
                "name": "壳体",
                "code": "E4201",
                "dueDate": "2026-08-04",
                "customer": "六厂",
                "deliveredQty": 3,
                "totalQty": 8,
                "status": "inProduction"
            })
        );
    }

    /// `kind = "batch"` 的 JSON 必须逐字长成前端 `BatchPartCard` 形态。
    #[test]
    fn batch_card_serializes_to_frontend_shape() {
        let card = PartCardOut::Batch(BatchCard {
            id: 2257,
            serial_no: None,
            name: "法兰".into(),
            code: "FL-DN80".into(),
            due_date: "2026-09-01".into(),
            batch_no: Some(2),
            batch_qty: 4,
        });
        let v = serde_json::to_value(&card).expect("serialize batch card");
        assert_eq!(
            v,
            json!({
                "kind": "batch",
                "id": "2257",
                "serialNo": null,
                "name": "法兰",
                "code": "FL-DN80",
                "dueDate": "2026-09-01",
                "batchNo": 2,
                "batchQty": 4
            })
        );
        // batch 变体**不得**带 status / customer / deliveredQty / totalQty / drawingUrl
        let obj = v.as_object().expect("object");
        for banned in [
            "status",
            "customer",
            "deliveredQty",
            "totalQty",
            "drawingUrl",
        ] {
            assert!(!obj.contains_key(banned), "batch 变体不该有 {banned} 键");
        }
    }

    /// 两个变体都**不得**产出 `drawingUrl`（已知有意缺口，见模块 doc）。
    #[test]
    fn no_card_variant_emits_drawing_url() {
        let wo = WorkOrderCard {
            id: 1,
            serial_no: None,
            name: "n".into(),
            code: "c".into(),
            due_date: "2026-01-01".into(),
            customer: None,
            delivered_qty: 0,
            total_qty: 1,
            status: "pendingProduction".into(),
        };
        let v = serde_json::to_value(PartCardOut::WorkOrder(wo)).unwrap();
        assert!(!v.as_object().unwrap().contains_key("drawingUrl"));
    }

    /// `PartCountsOut` 的 5 个键名必须与前端 tab 值逐字一致（camelCase）。
    #[test]
    fn counts_keys_are_camel_case_tab_values() {
        let c = PartCountsOut {
            all: 1901,
            pending_production: 300,
            in_production: 900,
            pending_inspection: 500,
            delivered: 198,
        };
        let v = serde_json::to_value(c).unwrap();
        assert_eq!(
            v,
            json!({
                "all": 1901,
                "pendingProduction": 300,
                "inProduction": 900,
                "pendingInspection": 500,
                "delivered": 198
            })
        );
    }

    /// `has_more` 必须序列化成 camelCase 的 `hasMore`。
    #[test]
    fn has_more_serializes_as_has_more() {
        let home = PartListHomeOut {
            counts: PartCountsOut {
                all: 0,
                pending_production: 0,
                in_production: 0,
                pending_inspection: 0,
                delivered: 0,
            },
            list: Vec::new(),
            has_more: true,
        };
        let v = serde_json::to_value(home).unwrap();
        assert_eq!(v["hasMore"], json!(true));
        assert!(!v.as_object().unwrap().contains_key("has_more"));
    }
}

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
//! ## ⚠️ 7 类 `status` 折叠的 catch-all 兜底（2026-10-12 起**已不可达**）
//! DB 的 `PROGRAMMING` / `COMPLETED` / `CANCELLED` 三个状态不落在任何 tab 的
//! 6 状态白名单内，2026-10-12 起**根本进不了列表**（过滤谓词就是白名单）。它们
//! 在 `list[].status` 里填 `"pendingProduction"` 的那枚 `_` 臂因此**已是纯防御**
//! （与旧前端 `services/parts.ts::mapStatus` 的 catch-all 逐字对齐的那份兜底，
//! 在本轮收口）。留着的唯一理由是「将来 DB 新增状态时卡片不至于渲染成空白」。
//! 详见 [`docs/api/wx.md`](../../../../docs/api/wx.md) §8 已知偏差登记。

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
    /// `t_part.system_delivery_date`，格式恒为 `YYYY-MM-DD`；**可空** —— 无交期工单
    /// 是 `null`（2026-10-12：数据源由 NOT NULL 的 `planned_delivery_date` 改成
    /// 可空的 `system_delivery_date`，与日期筛选谓词同源）
    #[serde(rename = "dueDate")]
    pub due_date: Option<String>,
    /// `t_customer.name`（`LEFT JOIN t_customer`，可为 null）
    pub customer: Option<String>,
    /// **已交付件数**：`SUM(t_part_batch.quantity)` where 批次
    /// `status IN ('DELIVERED','COMPLETED')` 且未软删（无已交批次时为 `0`）
    #[serde(rename = "deliveredQty")]
    pub delivered_qty: i32,
    /// `t_part.quantity`（工单总件数）
    #[serde(rename = "totalQty")]
    pub total_qty: i32,
    /// **6 类 tab 值之一**：`pendingProduction` / `inProduction` / `outsource` /
    /// `inspecting` / `delivered`。DB 里 6 状态白名单以外的状态（`PROGRAMMING` /
    /// `COMPLETED` / `CANCELLED`）**不会出现在列表里**；`display_status` 的
    /// catch-all 只作纯防御（见模块 doc）
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
    /// `t_part.system_delivery_date`，格式恒为 `YYYY-MM-DD`；**可空**（同上）
    #[serde(rename = "dueDate")]
    pub due_date: Option<String>,
    /// 当前活跃批次的 `batch_no`（`LEFT JOIN LATERAL`，无可活跃批次时 null）。
    ///
    /// ⚠️ **JSON number / 可空**（2026-10-11 review 第 1 轮登记的与前端 TS 模型的
    /// 类型差）：前端 `BatchPartCard.batchNo` 声明为 **`string`**，但小程序侧有映射
    /// 层 `services/parts.ts::toPartCardItem` 做 `String(it.batchNo ?? 1)
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

/// 7 个 tab 的计数。对应前端 `CountsByStatus`（`Record<TabValue, number>`）。
///
/// ⚠️ **角标不随 `?status=` 变，但随 `?date=` 变**（2026-10-12）：小程序 7 个 tab
/// 的数字是固定的（切 tab 不会让角标塌成 0），但日期导航条一变，7 个数字就整体
/// 换一批数 —— 那是日期作用域，不是 tab 作用域。
///
/// ⚠️ 归桶口径与 [`PartListHomeOut::counts`] 的 `status` 过滤**同源于 service 层
/// 的映射表**（`service::status_to_db_statuses` / `service::map_counts_by_status`），
/// 这是 2026-10-11 修掉的既有 bug：旧实现角标按 `READY_TO_SHIP + DELIVERED`
/// （实测 72 + 126 = 198）算，列表却只收单值 `DELIVERED`（最多 126），两边对不上。
///
/// ★ **不变量（lib 单测钉死）**：
/// `all == pendingProduction + inProduction + outsource + inspecting + delivered`。
/// 它成立的前提是 6 个 DB 状态与 5 个 dated tab 严格一一归属、无重叠无遗漏
/// （`INSPECTION` / `READY_TO_SHIP` 同归 `inspecting`，其余各一）。`noSystemDate`
/// 是**第 7 个**桶，**不进**这条等式 —— 它是「NULL 日期」的横切口径，与状态无关。
#[derive(Debug, Clone, Serialize)]
pub struct PartCountsOut {
    /// 6 状态白名单里、`system_delivery_date = ?date` 的行数（`?date` 缺省时 =
    /// 6 状态白名单的**全部**行，含 NULL 日期）。**排除** `PROGRAMMING` /
    /// `COMPLETED` / `CANCELLED`（2026-10-12 语义变更）
    pub all: i64,
    /// `t_part.status = 'PENDING'` 且 `system_delivery_date = ?date` 的行数
    #[serde(rename = "pendingProduction")]
    pub pending_production: i64,
    /// `t_part.status = 'IN_PROCESS'` 且 `system_delivery_date = ?date` 的行数
    #[serde(rename = "inProduction")]
    pub in_production: i64,
    /// `t_part.status = 'OUTSOURCE'` 且 `system_delivery_date = ?date` 的行数
    #[serde(rename = "outsource")]
    pub outsource: i64,
    /// `t_part.status IN ('INSPECTION','READY_TO_SHIP')` 且 `system_delivery_date = ?date`
    /// 的行数（**两个**状态合并）
    #[serde(rename = "inspecting")]
    pub inspecting: i64,
    /// `t_part.status = 'DELIVERED'` 且 `system_delivery_date = ?date` 的行数
    pub delivered: i64,
    /// `system_delivery_date IS NULL` 的行数（6 状态白名单内），**与 `?date=` 无关**
    #[serde(rename = "noSystemDate")]
    pub no_system_date: i64,
}

// =============================================================================
// 分页外壳
// =============================================================================

/// `GET /api/v2/wx/part-list` 出参：7 tab 角标 + 第 1 页卡片（**首屏聚合**）。
#[derive(Debug, Clone, Serialize)]
pub struct PartListHomeOut {
    /// 7 个 tab 的计数（**不带 `status` 过滤** —— 角标恒是 6 状态白名单口径；
    /// 但**带** `date` 作用域）
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
            due_date: Some("2026-08-04".into()),
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
            due_date: Some("2026-09-01".into()),
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
            due_date: Some("2026-01-01".into()),
            customer: None,
            delivered_qty: 0,
            total_qty: 1,
            status: "pendingProduction".into(),
        };
        let v = serde_json::to_value(PartCardOut::WorkOrder(wo)).unwrap();
        assert!(!v.as_object().unwrap().contains_key("drawingUrl"));
    }

    /// 2026-10-12 新增：两个变体的 `dueDate` 在「无交期」工单上必须是
    /// **JSON `null`**（`system_delivery_date IS NULL`），不能是空串、不能缺键 ——
    /// 小程序按 `item.dueDate` 直接渲染，`null` 才不会被兜底成 1970 之类的怪值。
    #[test]
    fn due_date_serializes_as_json_null_when_system_delivery_date_is_null() {
        let wo = PartCardOut::WorkOrder(WorkOrderCard {
            id: 7,
            serial_no: None,
            name: "n".into(),
            code: "c".into(),
            due_date: None,
            customer: None,
            delivered_qty: 0,
            total_qty: 1,
            status: "pendingProduction".into(),
        });
        let v = serde_json::to_value(&wo).unwrap();
        assert_eq!(v["dueDate"], json!(null));
        assert!(
            v.as_object().unwrap().contains_key("dueDate"),
            "键必须在位（值是 null），不能整个省略"
        );

        let batch = PartCardOut::Batch(BatchCard {
            id: 8,
            serial_no: None,
            name: "n".into(),
            code: "c".into(),
            due_date: None,
            batch_no: None,
            batch_qty: 1,
        });
        let v = serde_json::to_value(&batch).unwrap();
        assert_eq!(v["dueDate"], json!(null));
        assert!(v.as_object().unwrap().contains_key("dueDate"));
    }

    /// `PartCountsOut` 的 7 个键名必须与前端 tab 值逐字一致（camelCase）。
    #[test]
    fn counts_keys_are_camel_case_tab_values() {
        let c = PartCountsOut {
            all: 1901,
            pending_production: 300,
            in_production: 900,
            outsource: 120,
            inspecting: 500,
            delivered: 198,
            no_system_date: 42,
        };
        let v = serde_json::to_value(c).unwrap();
        assert_eq!(
            v,
            json!({
                "all": 1901,
                "pendingProduction": 300,
                "inProduction": 900,
                "outsource": 120,
                "inspecting": 500,
                "delivered": 198,
                "noSystemDate": 42
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
                outsource: 0,
                inspecting: 0,
                delivered: 0,
                no_system_date: 0,
            },
            list: Vec::new(),
            has_more: true,
        };
        let v = serde_json::to_value(home).unwrap();
        assert_eq!(v["hasMore"], json!(true));
        assert!(!v.as_object().unwrap().contains_key("has_more"));
    }
}

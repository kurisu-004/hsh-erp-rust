//! wx::production 子模块出参 VO 层（仅 `Serialize`）
//!
//! 2026-10-11 新增。**逐字对齐前端卡片模型** —— `wx-app/miniprogram/mock/
//! production.ts` 的 `ProductionBatchCardData extends BatchPartCard`、
//! `WorkerInfo`、`MonthlyStats`、`BatchCounts` —— **camelCase 字段名**，与本仓其它
//! 域（`part::vo` / `iam::vo` 的 snake_case）刻意不同。
//!
//! ## 为什么本域用 camelCase 而别域用 snake_case
//! 小程序侧 `part-card` / `production-stats` 组件是 wx-js 手写模板，**没有**一层
//! 「后端 snake → 前端 camel」的映射（Web 端有 `services/*.ts` 的映射层）。逐字对齐
//! 后前端可直接把响应塞进 `ProductionBatchCardData`，省掉一整个映射文件与它的漂移
//! 面。做法与 `wx::part_list::vo` 一致。
//!
//! ⚠️ **一个例外**：`batchNo` 的**字段名**对齐了前端模型，但**类型**没对齐（前端
//! `string` / 后端 JSON number）—— 该字段仍需经小程序侧映射层的
//! `String(...).padStart(2, '0')`。详见 [`ProductionBatchCardOut::batch_no`] 的逐字段
//! doc 与 `docs/api/wx.md` §8.11。
//!
//! ⚠️ 对照：`wx::login` 的 VO **仍**是 snake_case（`refresh_token` / `full_name`）——
//! 那里前端 `applyLoginResponse` 逐字读那几个键。三处口径**刻意**不同，不要互相
//! 「对齐」。
//!
//! ## ❌ 没有 `drawingUrl`（2026-10-11 登记为已知有意缺口）
//! 前端 `BasePartCard` 有 `drawingUrl`，但 **`t_part` 无图纸列**，后端没有可信数据
//! 源可填。**本轮刻意不产出该字段**（也不加恒 `null` 的占位），小程序侧在自己的
//! 映射层用 `/asset/drawing/{code}.png` 本地兜底。等 COS 文件服务接入后单独 PR 补。
//!
//! ⚠️ **与旧实现不同**：旧 `WxBatchSummary` 带一个**恒 `null`** 的 `drawing_url`
//! 字段（旧 `repo.rs` 里 `drawing_url: None, // 本 PR 不拉图`）。本次**连占位字段
//! 一起删掉** —— 与 `wx::part_list` 的处置统一，见 `docs/api/wx.md` §8.2。
//!
//! ## ❌ 没有 `part_id`
//! 前端 `ProductionBatchCardData` 声明了 `part_id` 但**从不读取**（小程序没有
//! 「跳零件详情」的跳转），删掉。

use serde::Serialize;

use crate::shared::types::serialize_i64;

// =============================================================================
//  工人 / 统计（首屏专属）
// =============================================================================

/// 当前登录账号绑定的工人。对应前端 `WorkerInfo`
/// （`wx-app/miniprogram/components/production-stats/production-stats.ts`）。
///
/// ⚠️ 外层是 `Option<WorkerOut>`：`t_user.worker_id` 未绑定时整块为 `null`
/// （见 [`ProductionHomeOut::worker`]）。
#[derive(Debug, Clone, Serialize)]
pub struct WorkerOut {
    /// `t_worker.name`（NOT NULL）
    pub name: String,
    /// `t_work_type.name`（经 `t_worker.work_type_id` → `t_work_type.id`）。
    ///
    /// ⚠️ **未绑定工种 / 工种行已软删时是空串 `""`，不是 `null`**。前端模板直接
    /// `{{worker.workType}}` 渲染（`production-stats.wxml`），空串渲染成空白、
    /// `null` 渲染成字面量 `null`；且前端 `WorkerInfo.workType` 的 TS 类型是
    /// `string`（非可选），空串不破坏类型。口径登记在 `docs/api/wx.md` §8.7。
    #[serde(rename = "workType")]
    pub work_type: String,
    /// 头像 URL。**恒 `null`**：`t_worker` **无头像列**，后端无数据源。
    ///
    /// ⚠️ **刻意保留该字段而不是删掉**：小程序组件 `production-stats` 读它并写
    /// `worker?.avatar || ''` 兜底（observer 里据此决定渲染 `t-avatar` 还是
    /// `t-icon` 用户占位）。恒 `null` ⇒ 组件走占位分支，行为正确；删掉字段则
    /// `avatar` 恒 `undefined`，`|| ''` 同样兜住 —— 但那会让「将来接头像服务」
    /// 这件事在前端和后端同时缺一个已声明的契约位。等头像服务接入后单独 PR 补。
    /// 登记在 `docs/api/wx.md` §8.7。
    pub avatar: Option<String>,
}

/// 工人当月工作量统计。对应前端 `MonthlyStats`（camelCase）。
#[derive(Debug, Clone, Serialize)]
pub struct WorkerStatsOut {
    /// 该工人当月发生过事件的**不同批次**数
    #[serde(rename = "batchCount")]
    pub batch_count: i64,
    /// 该工人当月 `PICKED_UP + RETURNED` 事件的 `SUM(quantity)`。
    ///
    /// ⚠️ **工作量估算**，不是真实工时（DB schema 无 `work_hours` 列）。前端
    /// `production-stats.wxml` 把它渲染成「累计工时 (h)」。
    #[serde(rename = "workHours")]
    pub work_hours: f64,
}

// =============================================================================
//  计数
// =============================================================================

/// 批次 tab 角标。对应前端 `BatchCounts`
/// （`wx-app/miniprogram/mock/production.ts`：`{ in_progress: number;
///
/// done: number }`）。
///
/// ⚠️⚠️ **两个键刻意保持 snake_case（`in_progress` / `done`），不转 camelCase。**
/// 它们是**前端的 tab 名**（前端 `production.ts:124` 声明为
/// `Record<BatchStatus, number>`，`BatchStatus = 'in_progress' | 'done'`），
/// 不是卡片字段——和 `worker` / `stats` / `list` 这些 camelCase 字段的语义不同。
/// 转成 `inProgress` 会直接打断前端角标渲染。
#[derive(Debug, Clone, Serialize)]
pub struct BatchCountsOut {
    /// 当月 `IN_PROCESS` 且 `updated_at` 落在当月的批次数
    pub in_progress: i64,
    /// 当月存在 `DELIVERED` 事件、且批次 `status IN ('DELIVERED','COMPLETED')`
    /// 的批次数
    pub done: i64,
}

// =============================================================================
//  卡片
// =============================================================================

/// 批次卡片。对应前端 `ProductionBatchCardData extends BatchPartCard`。
///
/// 字段顺序即 JSON 键序（serde 按声明序输出），与 `docs/api/wx.md` §2 的示例逐字
/// 一致。
#[derive(Debug, Clone, Serialize)]
pub struct ProductionBatchCardOut {
    /// `t_part_batch.id`（雪花 ID → **JSON string**，防 JS 精度截断）。
    ///
    /// ⚠️ 是**批次** id 不是零件 id（与 `wx::part_list` 的卡片 `id` 不同源）。
    #[serde(rename = "id", serialize_with = "serialize_i64")]
    pub id: i64,
    /// `t_part.serial_no`（可为 null —— 手工工单没序列号）。
    ///
    /// 前端原映射层有 `?? ''` 兜底；后端**如实返 null**，兜底留在前端。
    #[serde(rename = "serialNo")]
    pub serial_no: Option<String>,
    /// `t_part.name`
    pub name: String,
    /// `t_part.drawing_no`（前端卡片里的「图号」，叫 `code`；旧 VO 里叫
    /// `drawing_no`（snake_case），本次逐字对齐前端改名）
    #[serde(rename = "code")]
    pub code: String,
    /// `t_part.planned_delivery_date`，格式恒为 `YYYY-MM-DD`（该列 NOT NULL）
    #[serde(rename = "dueDate")]
    pub due_date: String,
    /// `t_part_batch.batch_no`，**JSON number**。
    ///
    /// ⚠️⚠️ **与前端 TS 模型的类型差（2026-10-11 review 第 1 轮登记）**：
    /// 前端 `BatchPartCard.batchNo` 的 TS 类型是 **`string`**
    /// （`wx-app/miniprogram/mock/parts.ts`），但**这不是本 VO 的 bug**：小程序侧
    /// 有一层映射把 number 变成字符串 —— `services/parts.ts::toPartCardItem` 写的是
    /// `String(it.batchNo ?? 1).padStart(2, '0')`、`services/production.ts::toBatchCard`
    /// 写的是 `String(it.batchNo).padStart(2, '0')`，
    /// `padStart` 是对 `String(...)` 的结果
    /// 调的（不是对 number 直接调）；`<part-card>` 组件的模板
    /// （`part-card.wxml`）只直接渲染 `{{item.batchNo}}`，**本身不做转换**。
    ///
    /// ⇒ 「前端可直接把响应塞进 `ProductionBatchCardData`」这句话**只对字段名成立、
    /// 对 `batchNo` 不成立**：数值类型必须过前端那层映射（且该映射是**既有**的，
    /// 新旧 URL 切换不改变它）。若将来要让 wx 响应直连卡片模型，得改的是**前端**
    /// 映射层或这条 TS 类型声明，**不是**把本字段序列化成字符串（DB 侧
    /// `t_part_batch.batch_no` 是 `int`，且 `wx::part_list` 端点 2/3 也返 number，
    /// 两域刻意一致）。
    ///
    /// 登记在 `docs/api/wx.md` §8.11。
    #[serde(rename = "batchNo")]
    pub batch_no: i32,
    /// `t_part_batch.quantity`（**本批次**件数）。
    ///
    /// ⚠️⚠️ **口径陷阱**：`wx::part_list` 卡片里同名的 `batchQty` 取的是
    /// `t_part.quantity`（**工单总**件数）。两个同名不同义，是既有行为的延续，
    /// **本次不改**。登记在 `docs/api/wx.md` §8.8，别后人「顺手统一」成同一个值。
    #[serde(rename = "batchQty")]
    pub batch_qty: i32,
    /// **折叠后**的 2 类 tab 值：`in_progress` / `done`。
    ///
    /// DB 的 `IN_PROCESS` → `in_progress`；`DELIVERED` / `COMPLETED` → `done`。
    /// 前端 `mapBatchStatus` 原本在客户端做同样的折叠，本次移到后端（前端映射层
    /// 下线时同步删）。
    pub status: String,
    /// 持有人名 `t_worker.name`；批次挂在货架上时为 `null`
    /// （SQL 的 `location = 'WORKER'` 闸门，见 `super::repo::FROM_SQL`）
    #[serde(rename = "assignedTo")]
    pub assigned_to: Option<String>,
    /// 该批次 `PICKED_UP + RETURNED` 事件的 `SUM(quantity)`。
    ///
    /// ⚠️ **无值时是 `null`，不是 `0`**：前端 `<part-card>` 用
    /// `wx:if="{{item.workHours != null}}"` 守门决定是否渲染工时行，给 `0` 会把
    /// 「本月没干过活」渲染成「干了 0 小时」。SQL 侧对应的 `COALESCE` 已删
    /// （旧实现有 `COALESCE(SUM(quantity), 0)`），登记在 `docs/api/wx.md` §8.6。
    #[serde(rename = "workHours")]
    pub work_hours: Option<f64>,
    /// 最近一次 `DELIVERED` 事件的日期 `YYYY-MM-DD`；从未送车过则 `null`。
    /// 前端用它按月份过滤 `done` 列表（`finishedDate.startsWith(period)`）。
    #[serde(rename = "finishedDate")]
    pub finished_date: Option<String>,
}

// =============================================================================
//  分页外壳
// =============================================================================

/// `GET /api/v2/wx/production` 出参：工人 + 统计 + tab 角标 + 第 1 页卡片
/// （**首屏聚合**）。
#[derive(Debug, Clone, Serialize)]
pub struct ProductionHomeOut {
    /// 当前账号绑定的工人；`t_user.worker_id` 未绑定时是 `null`。
    ///
    /// ⚠️ **未绑定不报错**（HTTP 仍 200）：绝大多数系统账号（admin / 系统管理员 /
    /// `hmi-*` 等非工人账号）本就没有对应工人。回填脚本
    /// `scripts/sql/20261011_backfill_t_user_worker_id.sql` 需人工确认后执行，
    /// 在那之前绝大多数账号都会命中这一支。
    pub worker: Option<WorkerOut>,
    /// 当月工作量统计。`worker` 为 `null` 时恒为 `{ batchCount: 0, workHours: 0 }`
    /// （与「已绑定但当月零工作量」**不可区分** —— 这是登记在案的取舍，见
    /// [`WorkerOut`] 的 `avatar` 与 `docs/api/wx.md` §8.7）
    pub stats: WorkerStatsOut,
    /// 2 个 tab 的角标（**不带 `?tab=` 过滤** —— 切 tab 时角标固定不变；但**带
    /// `?period=` 作用域**，与 `list` 侧的 period 闸门逐字一致。2026-10-12 口径
    /// 订正：原注释的「角标恒是全局口径」与代码相反，见 docs/api/wx.md §3.8）
    pub counts: BatchCountsOut,
    /// 当前页卡片（按 `?tab=` 过滤、按 `?page=` 翻页），**至多 `size` 条**
    pub list: Vec<ProductionBatchCardOut>,
    /// 是否还有下一页（算法：取 `size + 1` 条判超，见 `docs/api/wx.md` §3.4）
    #[serde(rename = "hasMore")]
    pub has_more: bool,
}

/// `GET /api/v2/wx/production/page` 出参：纯增量（上拉加载后续页）。
///
/// 与 [`ProductionHomeOut`] 的**唯一**差别是**不带** `worker` / `stats` /
/// `counts`：角标与工人信息只在首屏聚合端点算一次，`/page` 每次上拉都重查
/// `t_user` / `t_part_event` 聚合是纯浪费。
#[derive(Debug, Clone, Serialize)]
pub struct ProductionPageOut {
    /// 当前页卡片（与首屏端点同一查询路径，`?page=2` 时内容逐字相同）
    pub list: Vec<ProductionBatchCardOut>,
    /// 是否还有下一页（算法同上）
    #[serde(rename = "hasMore")]
    pub has_more: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    /// `ProductionBatchCardOut` 的 JSON 必须**逐字**长成前端
    /// `ProductionBatchCardData` 的形态（camelCase + 雪花 id 字符串化 + 日期串）。
    #[test]
    fn batch_card_serializes_to_frontend_shape() {
        let card = ProductionBatchCardOut {
            id: 2256,
            serial_no: Some("F2256-01".into()),
            name: "法兰盘 DN80".into(),
            code: "FL-DN80-A2".into(),
            due_date: "2026-10-20".into(),
            batch_no: 3,
            batch_qty: 7,
            status: "in_progress".into(),
            assigned_to: Some("李伟".into()),
            work_hours: Some(12.5),
            finished_date: None,
        };
        let v = serde_json::to_value(&card).expect("serialize card");
        assert_eq!(
            v,
            json!({
                "id": "2256",
                "serialNo": "F2256-01",
                "name": "法兰盘 DN80",
                "code": "FL-DN80-A2",
                "dueDate": "2026-10-20",
                "batchNo": 3,
                "batchQty": 7,
                "status": "in_progress",
                "assignedTo": "李伟",
                "workHours": 12.5,
                "finishedDate": null
            })
        );
        // 禁列：part_id / drawing_url / drawingUrl 都不该出现
        let obj = v.as_object().expect("object");
        for banned in ["part_id", "partId", "drawing_url", "drawingUrl"] {
            assert!(!obj.contains_key(banned), "卡片不该有 {banned} 键");
        }
    }

    /// 无值字段必须是 `null` 而不是 `0` / `""` / `""`（前端 `!= null` 守门依赖）。
    #[test]
    fn null_fields_serialize_as_null_not_zero() {
        let card = ProductionBatchCardOut {
            id: 1,
            serial_no: None,
            name: "n".into(),
            code: "c".into(),
            due_date: "2026-10-20".into(),
            batch_no: 1,
            batch_qty: 4,
            status: "done".into(),
            assigned_to: None,
            work_hours: None,
            finished_date: None,
        };
        let v = serde_json::to_value(card).unwrap();
        assert_eq!(v["serialNo"], Value::Null);
        assert_eq!(v["assignedTo"], Value::Null);
        // ★ 关键：不能是 0
        assert_eq!(v["workHours"], Value::Null, "workHours 无值时必须是 null");
        assert_eq!(v["finishedDate"], Value::Null);
        assert!(
            !v.as_object().unwrap().contains_key("work_hours"),
            "work_hours 不得以 snake_case 泄漏"
        );
    }

    /// `counts` 的两个键必须**保持 snake_case**（它们是前端 tab 名，不是卡片字段）。
    #[test]
    fn counts_keys_stay_snake_case_tab_names() {
        let c = BatchCountsOut {
            in_progress: 41,
            done: 12,
        };
        assert_eq!(
            serde_json::to_value(c).unwrap(),
            json!({"in_progress": 41, "done": 12}),
            "counts 是 Record<BatchStatus, number>，键必须是 in_progress / done"
        );
    }

    /// `worker` / `stats` / `hasMore` 的 camelCase 键名逐字钉死。
    #[test]
    fn home_out_keys_are_camel_case() {
        let home = ProductionHomeOut {
            worker: Some(WorkerOut {
                name: "李伟".into(),
                work_type: "CNC 车工".into(),
                avatar: None,
            }),
            stats: WorkerStatsOut {
                batch_count: 12,
                work_hours: 34.5,
            },
            counts: BatchCountsOut {
                in_progress: 41,
                done: 0,
            },
            list: Vec::new(),
            has_more: true,
        };
        let v = serde_json::to_value(home).unwrap();
        let obj = v.as_object().unwrap();
        let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec!["counts", "hasMore", "list", "stats", "worker"],
            "首屏响应恰好 5 个顶层键"
        );
        assert_eq!(v["worker"]["workType"], json!("CNC 车工"));
        assert_eq!(v["worker"]["avatar"], Value::Null);
        assert_eq!(v["stats"], json!({"batchCount": 12, "workHours": 34.5}));

        // 增量端点恰好 2 个键（⚠️ serde_json 默认**不**保序，故排序后比集合）
        let page = ProductionPageOut {
            list: Vec::new(),
            has_more: false,
        };
        let pv = serde_json::to_value(page).unwrap();
        let mut pkeys: Vec<&str> = pv.as_object().unwrap().keys().map(String::as_str).collect();
        pkeys.sort_unstable();
        assert_eq!(pkeys, vec!["hasMore", "list"]);
    }

    /// `worker: null`（未绑定）时响应仍是合法 JSON、且 `stats` 为零值。
    #[test]
    fn unbound_worker_serializes_as_null_with_zero_stats() {
        let home = ProductionHomeOut {
            worker: None,
            stats: WorkerStatsOut {
                batch_count: 0,
                work_hours: 0.0,
            },
            counts: BatchCountsOut {
                in_progress: 0,
                done: 0,
            },
            list: Vec::new(),
            has_more: false,
        };
        let v = serde_json::to_value(home).unwrap();
        assert_eq!(v["worker"], Value::Null);
        assert_eq!(v["stats"], json!({"batchCount": 0, "workHours": 0.0}));
    }
}

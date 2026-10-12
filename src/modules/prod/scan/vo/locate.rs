//! `prod::scan` 扫码找件 · 诊断端点（`GET /scan/locate`）的出参 VO
//!
//! 2026-10-15 新增（第 6 条端点）。工人扫图纸序列号 → 返回该 part **全部未软删
//! 批次**（**含终态**）的位置事实 + 针对指定工人的可行动作 `action` / 失败原因
//! `disabled_reason`。这是「扫码找件 · 自助恢复」三件套的第一件（S1 只读诊断）。
//! **2026-10-15 同日 S2 / S3 均已落地**（[`LocateAction::TakeOver`] 对应的
//! `POST /scan/take-over`、[`LocateAction::ReceiveFromOutsource`] 对应的
//! `POST /scan/outsource-receive`）。
//!
//! ## 与 `ScanListItem`（`listing.rs`）的关系：刻意不复用
//!
//! 两者都是「批次级行」，但契约不同：
//! - `ScanListItem` 是**列表分页行**（`/pickable` / `/held` 共用，17 字段），行集被
//!   状态 / 位置 / 持有人过滤；
//! - `LocateBatchItem` 是**诊断行**（16 字段），覆盖该 part 的**全部**批次（含
//!   `CANCELLED` / `COMPLETED` 等终态 —— 找件场景里「这批被取消了」本身就是要回答
//!   的事实），且多出 action 判定相关的原始事实（`holder_display` / `company_name` /
//!   `sent_at` / `disabled_reason`）。
//!
//! 两处字段形态也有意分叉：
//! - `ScanListItem.chain_next_process_id` 是**非空 + `"0"` 兜底**（列表页沿用本仓
//!   `COALESCE(..., 0)` 口径）；本行 `next_process_id` 是**可空字符串** —— 诊断语义
//!   里「没有下一道」应该显式是 `null`，而不是让前端去猜 `"0"` 是不是一个合法工序
//!   id（雪花 id 不含 0）。
//! - `ScanListItem.location` 恒 `null`（那两条端点不做 batch enrichment）；本行
//!   `location` 是**真实值**，它就是「件在哪个位置」的答案本身。
//!
//! ## 序列化约定
//!
//! 雪花 id（`LocatePartBrief.id` / `LocateBatchItem.batch_id` / `next_process_id` /
//! `LocateProcessBriefOut.process_id`）
//! 一律 `serialize_i64` / `serialize_i64_opt` → JSON **string**（19 位 id 在 JS
//! `Number` 下会丢精度）；`batch_no` / `quantity` / `version` 是裸数字；
//! `chain_state` 复用 [`ScanChainState`]（`"NONE"` / `"NEXT"` / `"TAIL"` 三个大写
//! 字面量）；`action` / `disabled_reason` 为 `null` 或大写蛇形字面量（枚举
//! `rename_all = "SCREAMING_SNAKE_CASE"`，逐字清单见两个枚举的 doc）。
//!
//! ## 目标工序 picker：2026-10-16 新增 `worker_processes`
//!
//! `LocateOut` 多一个**顶层** `worker_processes`（不在批次行里）—— 它是「这个工人
//! 能做哪几道工序」的**工人维度**事实，与 batch 维度无关，且同一响应内所有批次行
//! 共用同一份。写端点「接管 / 外协收回」的「单候选自动推导 / 多候选必填」用的
//! 是同一份工种映射，读侧只是把它显式发给前端当 picker 的选项集。
//!
//! ⚠️ **键集是网络契约**：`scan_list_item_keys_are_pinned` 的同款单测
//! （`locate_keys_are_pinned`）钉死本模块四个结构的键集合与雪花 string 形态 ——
//! 加 / 删字段必须先改它，并同步前端 schema。

use chrono::{NaiveDate, NaiveDateTime};
use serde::Serialize;

use crate::shared::types::{serialize_i64, serialize_i64_opt};

use super::listing::ScanChainState;

/// 诊断出的**可行动作**（`LocateBatchItem.action` 的非空取值）。
///
/// 三件套的分工：`action` 只回答「**这个工人**、**这一批**，现在能做什么」；
/// 它**不是**写侧守卫本身 —— 写侧还会做各自的 OCC / 存在性校验，且本端点刻意
/// 与写侧守卫同源（会成功的绝不显示 `NONE`，会拒绝的绝不显示可点按钮），
/// 对应关系见 `docs/api/scan.md` 的「与写侧守卫对照表」。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum LocateAction {
    /// `PENDING` 或 `IN_PROCESS + PRODUCTION_SHELF` → 该工人可领取（走
    /// `POST /scan/batches/{id}/pick-up` 的对应分支；PENDING 的手填下一道流见
    /// `docs/api/scan.md` §8 的残余失败组合登记）。
    PickUp,
    /// `IN_PROCESS + WORKER` 且持有人就是该工人 —— 件已在自己手上（放回 / 送检流）。
    /// **不受任何闸门影响**（容量触顶 / 工种未绑定都不改变「这是我手上的件」）。
    Mine,
    /// `IN_PROCESS + WORKER` 且持有人是**别的**工人 → 接手（S2 写端点
    /// `POST /scan/take-over`，**2026-10-15 已落地**；写侧判据与 `take_over_gate`
    /// 同源）。⚠️ holder 为 NULL 的脏形态**不在此列**（落 `UNKNOWN`，写侧同形态拒）。
    TakeOver,
    /// `OUTSOURCE + OUTSOURCE_COMPANY` 且 holder 非空 → 外协收回（S3 写端点
    /// `POST /scan/outsource-receive`，**2026-10-15 已落地**；holder 为 NULL 的脏
    /// 形态**不在此列**，落 `UNKNOWN` —— 写侧同形态拒 `20122`）。
    ReceiveFromOutsource,
}

/// 「不能点」的原因（`LocateBatchItem.disabled_reason` 的非空取值，9 个字面量钉死）。
///
/// 分组（与 service 的判定优先级同序：状态类 → 工种未绑定 → 工序 ∉ 工种 → 容量）：
/// - 状态类（**写侧没有任何按钮可点**，前端只做指引）：`PROGRAMMING`（待编程，
///   指引找 CNC 编程）/ `INSPECTION`（在品检，指引找品检）/ `TERMINAL`（终态）/
///   `UNKNOWN`（无法归类的状态 / 位置组合）；
/// - 资格类：`WORK_TYPE_NOT_SET`（工人未绑工种）→ `PROCESS_NOT_IN_WORK_TYPE`
///   （链首 / 推导出的下一道不在该工种的活跃工序映射里）→
///   `PROCESS_CHAIN_STEP_NOT_FOUND`（有链但链内无活跃 step，写侧无处可落）。
///   ⚠️ 原第 4 个资格类 `PROCESS_CHAIN_REQUIRED` 已于 **2026-10-15 S3 删除**：
///   外协收回写端点支持「前端手填 `next_process_id`」，把「有链但推不出下一道」
///   判成不可点是**假阻挡**（详见 `listing/service.rs::outsource_gate` 的 doc）；
/// - 容量类：`WORK_TYPE_MAX_HELD_NOT_SET`（工种未配 `max_held_batches` ⇒ 该工种
///   不可取件）→ `WORKER_HOLD_LIMIT_EXCEEDED`（已达 `max_held_batches`）。
///
/// ⚠️ 字面量是**新 wire 面**，前端按字符串分支；`UNKNOWN` 是「后端没见过的组合」
/// 的保守出口 —— 出现它说明有状态 / 位置形态没被决策表覆盖，应同时补表。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum LocateDisabledReason {
    /// 状态 `PROGRAMMING`：待编程，指引找 CNC 编程。
    Programming,
    /// 状态 `INSPECTION`：在品检，指引找品检。
    Inspection,
    /// `READY_TO_SHIP` / `DELIVERED` / `COMPLETED` / `CANCELLED`：终态，无可行动作。
    Terminal,
    /// 工人 `work_type_id IS NULL`（除 `MINE` 外一切动作都不可行）。
    WorkTypeNotSet,
    /// 链首（PENDING）/ 推导出的下一道（TAKE_OVER / 外协收回）不命中该工种的
    /// `t_work_type_process` 活跃映射。
    ///
    /// ⚠️ **2026-10-16 起有两种成因**（前端文案可共用，但排障时别混）：① 链推得出
    /// 目标工序、而它不在工种映射里；② 链推不出目标工序（无链 / 指针漂移 / 链尾 /
    /// 链内歧义），而该工种**一条工序都没映射**（对位写侧 `20104`）。后者是
    /// 「这个人根本没有可执行的工序」，与「这道工序不归他管」是两回事。
    ProcessNotInWorkType,
    /// 有链但链内没有未软删 step ⇒ 写侧没有可落的链首（对位写侧 `20702`）。
    ProcessChainStepNotFound,
    /// 该工人持有的批次已达工种上限 `max_held_batches`（对位写侧 `20204`）。
    WorkerHoldLimitExceeded,
    /// 工种未配 `max_held_batches`（`NULL` = 未配置 ⇒ 不可取件，对位写侧 `20904`）。
    WorkTypeMaxHeldNotSet,
    /// 状态 / 位置组合不在决策表内（保守出口，出现即需补表）。
    Unknown,
}

/// `GET /api/v2/prod/scan/locate` 的 data（`part` + 全部批次 + 工种候选工序）。
#[derive(Debug, Clone, Serialize)]
pub struct LocateOut {
    /// 命中的工单（只读序列号，**不回退 `t_assembly`**，见 `docs/api/scan.md` §8）。
    pub part: LocatePartBrief,
    /// 该 part **全部未软删批次**（含终态），按 `batch_no ASC, pb.id ASC` 排序。
    /// 顺序即「件数递增 + 拆批兜底」，与前端展示顺序一致，不必二次排序。
    pub batches: Vec<LocateBatchItem>,
    /// 该工人工种可执行的工序（`t_work_type_process` 活跃映射 ∩ 未软删
    /// `t_process`），按 `sort_order ASC, id ASC`。未传 `worker_id` / 工人无工种 /
    /// 工种未映射任何工序 ⇒ 空数组。
    ///
    /// **2026-10-16 新增**：前端把「接管 / 外协收回」的目标工序 picker 收窄到这个
    /// 集合（不再展示全部工序），且**单候选时直接提交、不弹框**（写端点会自行推导）。
    /// ⚠️ **本列表比写侧判据窄**（2026-10-16 review 第 1 轮 MINOR-1）：**闸门**
    /// （`locate` 的 `disabled_reason` 判定）用的是**宽**集合（全部活跃映射，含工序
    /// 已软删的，与写侧 `WorkTypeRepo::list_process_ids` 逐字同源），本**出参**只留
    /// 工序未软删的行（软删工序没有可展示的名字）。故「闸门放行 + 本数组为空」是
    /// 刻意且正常的形态（工种只映射了已软删工序），前端有 0 候选兜底提示。
    pub worker_processes: Vec<LocateProcessBriefOut>,
}

/// 目标工序 picker 的一行（`LocateOut.worker_processes` 的元素，4 字段）。
#[derive(Debug, Clone, Serialize)]
pub struct LocateProcessBriefOut {
    /// `t_process.id`（`serialize_i64` → JSON string；写端点的
    /// `next_process_id` 入参吃同一种字符串形态）。
    #[serde(serialize_with = "serialize_i64")]
    pub process_id: i64,
    pub code: String,
    pub name: String,
    /// `t_process.category`：`"INHOUSE"` / `"OUTSOURCE"`（与前端 `ProcessCategory`
    /// 对齐）。
    pub category: String,
}

/// 诊断结果的工单侧最小投影（7 字段）。
#[derive(Debug, Clone, Serialize)]
pub struct LocatePartBrief {
    /// `t_part.id`（`serialize_i64` → JSON string）。
    #[serde(serialize_with = "serialize_i64")]
    pub id: i64,
    /// 扫进来的序列号原样回显（`t_part.serial_no` 可空 —— 手工工单无序列号，
    /// 但本端点按序列号命中，故命中行必有值；仍按 `Option` 收以对齐列可空性）。
    pub serial_no: Option<String>,
    pub name: String,
    pub drawing_no: String,
    pub is_urgent: bool,
    /// `t_part.planned_delivery_date`（**真实值**，非占位）。
    pub planned_delivery_date: NaiveDate,
    /// `t_part.system_delivery_date` 是可空列（`date` 无 NOT NULL）。
    pub system_delivery_date: Option<NaiveDate>,
}

/// 诊断结果的一行批次（16 字段，**键名即网络契约**）。
///
/// 行内含 action 判定的**全部原始事实**（`status` / `location` / `holder_display` /
/// `chain_state` / `next_process_id` / `company_name` / `sent_at`），前端可以在
/// `action == null` 时自己解释 `disabled_reason`，也可以在 `action != null` 时
/// 用 `version` 直接发对应写请求（OCC 锚）。
#[derive(Debug, Clone, Serialize)]
pub struct LocateBatchItem {
    /// `t_part_batch.id`（`serialize_i64` → JSON string）。写请求的批次锚。
    #[serde(serialize_with = "serialize_i64")]
    pub batch_id: i64,
    /// `t_part_batch.batch_no`（零件内递增；拆批产生的同号行由 `batch_id` 兜底）。
    pub batch_no: i32,
    /// `t_part_batch.quantity`（批次数量，不是工单数量）。
    pub quantity: i32,
    /// `t_part_batch.status` 原文透出（8 态 + 可能的脏值）—— **不在这里枚举化**：
    /// 状态分类的结论在 `action` / `disabled_reason`，原文只作展示与排查。
    pub status: String,
    /// `t_part_batch.version`（OCC 锚，写请求原样回传）。
    pub version: i32,
    /// `t_part_batch.location` 真实值（`PRODUCTION_SHELF` / `WORKER` /
    /// `OUTSOURCE_COMPANY` / `INSPECTION_SHELF` / `OFFICE` / `NULL`）。
    pub location: Option<String>,
    /// 持有人显示名：`COALESCE(t_shelf.name, t_worker.name, t_outsource_company.name)`
    /// 三表 JOIN（详见 `listing/repo.rs` 的 SQL 与 §8 的多态歧义登记）。
    pub holder_display: Option<String>,
    /// 批次当前工序名（`t_process.name`，走 `current_process_id` 权威列；工序被软删
    /// 时 `null`）。`INSPECTION` / `DELIVERED` 批次按「出池清 `current_process_id`」
    /// 不变式恒 `null`。
    pub current_process_name: Option<String>,
    /// 链位置三值（`"NONE"` / `"NEXT"` / `"TAIL"`，复用 [`ScanChainState`]）。
    /// 与 `/held` 的 `chain_state` **同源**（`shared::batch::chain::
    /// CHAIN_POSITION_LATERAL_SQL`），语义逐字相同。
    pub chain_state: ScanChainState,
    /// 链内推导出的下一道工序 id（`serialize_i64_opt` → JSON string|null）。
    /// 仅 `chain_state == "NEXT"` 时非空；`TAIL` / `NONE` 恒 `null`。
    /// **不是** `"0"` 兜底 —— 诊断行用 `null` 表达「没有下一道」。
    #[serde(serialize_with = "serialize_i64_opt")]
    pub next_process_id: Option<i64>,
    /// 下一道工序名（`t_process.name`）；下一道工序被软删时为 `null`（id 仍有值）。
    pub next_process_name: Option<String>,
    /// `t_part_batch.updated_at`（位置事实的新鲜度；前端可据此提示「数据截止」）。
    pub updated_at: NaiveDateTime,
    /// 外协批次（`status='OUTSOURCE'`）的**在途公司名**（`t_outsource_company.name`，
    /// 即 `current_holder_id` 解析出的公司行）；非外协批次恒 `null`。
    /// 与 `holder_display` 的关系见 `docs/api/scan.md` §8 的登记。
    pub company_name: Option<String>,
    /// 外协批次的**开口 shipment 发出时间**（`t_outsource_shipment` 里
    /// `status='OUTSOURCING'` 的最新一条，`LEFT JOIN LATERAL … LIMIT 1`）；
    /// 非外协批次 / 找不到开口 shipment 时 `null`。
    pub sent_at: Option<NaiveDateTime>,
    /// 针对 query 里那个工人的可行动作；未传 `worker_id` 时恒 `null`（纯位置事实）。
    pub action: Option<LocateAction>,
    /// 不能点 `action` 的原因；`action` 非空或未传 `worker_id` 时恒 `null`。
    pub disabled_reason: Option<LocateDisabledReason>,
}

#[cfg(test)]
mod tests {
    //! 键集与字面量护栏（同 `listing.rs::scan_list_item_keys_are_pinned` 的口径）。
    //!
    //! 本端点的行 VO 是**诊断用**的：前端按 `action` / `disabled_reason` 渲染按钮，
    //! 字段漂移（悄悄加一个没人读的键 / 删一个前端在用的键）不会让任何编译或集成
    //! 测试变红，故在单测里把键集合与两个枚举的 wire 字面量逐字钉死。
    use super::*;

    fn sample_part() -> LocatePartBrief {
        LocatePartBrief {
            id: 1,
            serial_no: Some("SN-1".into()),
            name: "n".into(),
            drawing_no: "d".into(),
            is_urgent: false,
            planned_delivery_date: NaiveDate::from_ymd_opt(2026, 1, 1).unwrap(),
            system_delivery_date: None,
        }
    }

    fn sample_item() -> LocateBatchItem {
        LocateBatchItem {
            batch_id: 2,
            batch_no: 1,
            quantity: 3,
            status: "IN_PROCESS".into(),
            version: 4,
            location: Some("WORKER".into()),
            holder_display: None,
            current_process_name: None,
            chain_state: ScanChainState::None,
            next_process_id: None,
            next_process_name: None,
            updated_at: NaiveDate::from_ymd_opt(2026, 1, 1)
                .unwrap()
                .and_hms_opt(0, 0, 0)
                .unwrap(),
            company_name: None,
            sent_at: None,
            action: None,
            disabled_reason: None,
        }
    }

    /// `LocatePartBrief` / `LocateBatchItem` / `LocateOut` / `LocateProcessBriefOut`
    /// 的键集合逐字钉死，并锁住雪花 id 的 string 形态（19 位 id 在 JS `Number`
    /// 下会丢精度）。
    #[test]
    fn locate_keys_are_pinned() {
        let part = serde_json::to_value(sample_part()).expect("serialize LocatePartBrief");
        let mut part_keys: Vec<&str> = part
            .as_object()
            .expect("LocatePartBrief 序列化成对象")
            .keys()
            .map(String::as_str)
            .collect();
        part_keys.sort_unstable();
        assert_eq!(
            part_keys,
            [
                "drawing_no",
                "id",
                "is_urgent",
                "name",
                "planned_delivery_date",
                "serial_no",
                "system_delivery_date",
            ],
            "LocatePartBrief 的字段集变了：加 / 删字段要同步前端 schema 与本文档"
        );
        assert_eq!(part["id"], serde_json::json!("1"));

        let item = serde_json::to_value(sample_item()).expect("serialize LocateBatchItem");
        let mut item_keys: Vec<&str> = item
            .as_object()
            .expect("LocateBatchItem 序列化成对象")
            .keys()
            .map(String::as_str)
            .collect();
        item_keys.sort_unstable();
        assert_eq!(
            item_keys,
            [
                "action",
                "batch_id",
                "batch_no",
                "chain_state",
                "company_name",
                "current_process_name",
                "disabled_reason",
                "holder_display",
                "location",
                "next_process_id",
                "next_process_name",
                "quantity",
                "sent_at",
                "status",
                "updated_at",
                "version",
            ],
            "LocateBatchItem 的字段集变了：加 / 删字段要同步前端 schema 与本文档"
        );
        assert_eq!(item["batch_id"], serde_json::json!("2"));
        assert_eq!(item["next_process_id"], serde_json::Value::Null);
        assert_eq!(item["batch_no"], serde_json::json!(1));
        assert_eq!(item["quantity"], serde_json::json!(3));
        assert_eq!(item["version"], serde_json::json!(4));
        assert_eq!(item["action"], serde_json::Value::Null);
        assert_eq!(item["disabled_reason"], serde_json::Value::Null);

        let out = serde_json::to_value(LocateOut {
            part: sample_part(),
            batches: vec![sample_item()],
            worker_processes: vec![LocateProcessBriefOut {
                process_id: 5,
                code: "P".into(),
                name: "n".into(),
                category: "INHOUSE".into(),
            }],
        })
        .expect("serialize LocateOut");
        let mut out_keys: Vec<&str> = out
            .as_object()
            .expect("LocateOut 序列化成对象")
            .keys()
            .map(String::as_str)
            .collect();
        out_keys.sort_unstable();
        assert_eq!(out_keys, ["batches", "part", "worker_processes"]);

        let brief = serde_json::to_value(LocateProcessBriefOut {
            process_id: 5,
            code: "P".into(),
            name: "n".into(),
            category: "INHOUSE".into(),
        })
        .expect("serialize LocateProcessBriefOut");
        let mut brief_keys: Vec<&str> = brief
            .as_object()
            .expect("LocateProcessBriefOut 序列化成对象")
            .keys()
            .map(String::as_str)
            .collect();
        brief_keys.sort_unstable();
        assert_eq!(
            brief_keys,
            ["category", "code", "name", "process_id"],
            "LocateProcessBriefOut 的字段集变了：加 / 删字段要同步前端 schema 与本文档"
        );
        assert_eq!(brief["process_id"], serde_json::json!("5"));
    }

    /// `action` 的 4 个 wire 字面量逐字钉死（前端按字符串分支）。
    #[test]
    fn locate_action_literals_are_pinned() {
        for (action, want) in [
            (LocateAction::PickUp, "\"PICK_UP\""),
            (LocateAction::Mine, "\"MINE\""),
            (LocateAction::TakeOver, "\"TAKE_OVER\""),
            (
                LocateAction::ReceiveFromOutsource,
                "\"RECEIVE_FROM_OUTSOURCE\"",
            ),
        ] {
            let json = serde_json::to_string(&action).expect("serialize LocateAction");
            assert_eq!(json, want);
        }
    }

    /// `disabled_reason` 的 **9 个** wire 字面量逐字钉死。
    ///
    /// 少一个都会让前端落到 `else` 分支静默显示错误文案；`SCREAMING_SNAKE_CASE`
    /// 的大小写本身也是契约（与 `chain_state` 的 `UPPERCASE` 同款纪律）。
    ///
    /// 2026-10-15 S3：`PROCESS_CHAIN_REQUIRED` 随外协收回写端点落地被删除
    /// （假阻挡，见 `listing/service.rs::outsource_gate`），10 → 9。
    #[test]
    fn locate_disabled_reason_literals_are_pinned() {
        for (reason, want) in [
            (LocateDisabledReason::Programming, "\"PROGRAMMING\""),
            (LocateDisabledReason::Inspection, "\"INSPECTION\""),
            (LocateDisabledReason::Terminal, "\"TERMINAL\""),
            (
                LocateDisabledReason::WorkTypeNotSet,
                "\"WORK_TYPE_NOT_SET\"",
            ),
            (
                LocateDisabledReason::ProcessNotInWorkType,
                "\"PROCESS_NOT_IN_WORK_TYPE\"",
            ),
            (
                LocateDisabledReason::ProcessChainStepNotFound,
                "\"PROCESS_CHAIN_STEP_NOT_FOUND\"",
            ),
            (
                LocateDisabledReason::WorkerHoldLimitExceeded,
                "\"WORKER_HOLD_LIMIT_EXCEEDED\"",
            ),
            (
                LocateDisabledReason::WorkTypeMaxHeldNotSet,
                "\"WORK_TYPE_MAX_HELD_NOT_SET\"",
            ),
            (LocateDisabledReason::Unknown, "\"UNKNOWN\""),
        ] {
            let json = serde_json::to_string(&reason).expect("serialize LocateDisabledReason");
            assert_eq!(json, want);
        }
    }
}

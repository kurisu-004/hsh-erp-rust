//! `prod::scan` 扫码找件 · 诊断端点（`GET /scan/locate`）的出参 VO
//!
//! 2026-10-15 新增（第 6 条端点）。工人扫图纸序列号 → 返回该 part **全部未软删
//! 批次**（**含终态**）的位置事实 + 针对指定工人的可行动作 `action` / 失败原因
//! `disabled_reason`。这是「扫码找件 · 自助恢复」三件套的第一件（S1 只读诊断），
//! 后续 S2 / S3 会再加 take-over / outsource-receive 两个写端点。
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
//! 雪花 id（`LocatePartBrief.id` / `LocateBatchItem.batch_id` / `next_process_id`）
//! 一律 `serialize_i64` / `serialize_i64_opt` → JSON **string**（19 位 id 在 JS
//! `Number` 下会丢精度）；`batch_no` / `quantity` / `version` 是裸数字；
//! `chain_state` 复用 [`ScanChainState`]（`"NONE"` / `"NEXT"` / `"TAIL"` 三个大写
//! 字面量）；`action` / `disabled_reason` 为 `null` 或大写蛇形字面量（枚举
//! `rename_all = "SCREAMING_SNAKE_CASE"`，逐字清单见两个枚举的 doc）。
//!
//! ⚠️ **键集是网络契约**：`scan_list_item_keys_are_pinned` 的同款单测
//! （`locate_keys_are_pinned`）钉死本模块三个结构的键集合与雪花 string 形态 ——
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
    /// `IN_PROCESS + WORKER` 且持有人是**别的**工人 → 接手（S2 写端点，尚未落地）。
    TakeOver,
    /// `OUTSOURCE + OUTSOURCE_COMPANY` → 外协收回（S3 写端点，尚未落地）。
    ReceiveFromOutsource,
}

/// 「不能点」的原因（`LocateBatchItem.disabled_reason` 的非空取值，10 个字面量钉死）。
///
/// 分组（与 service 的判定优先级同序：状态类 → 工种未绑定 → 工序 ∉ 工种 → 容量）：
/// - 状态类（**写侧没有任何按钮可点**，前端只做指引）：`PROGRAMMING`（待编程，
///   指引找 CNC 编程）/ `INSPECTION`（在品检，指引找品检）/ `TERMINAL`（终态）/
///   `UNKNOWN`（无法归类的状态 / 位置组合）；
/// - 资格类：`WORK_TYPE_NOT_SET`（工人未绑工种）→ `PROCESS_NOT_IN_WORK_TYPE`
///   （链首 / 推导出的下一道不在该工种的活跃工序映射里）→
///   `PROCESS_CHAIN_STEP_NOT_FOUND`（有链但链内无活跃 step，写侧无处可落）→
///   `PROCESS_CHAIN_REQUIRED`（有链但推不出下一道，写侧 20706）；
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
    ProcessNotInWorkType,
    /// 有链但链内没有未软删 step ⇒ 写侧没有可落的链首（对位写侧 `20702`）。
    ProcessChainStepNotFound,
    /// 有链但推不出下一道（指针漂移 / 当前工序不在链内等）⇒ 对位写侧 `20706`。
    ProcessChainRequired,
    /// 该工人持有的批次已达工种上限 `max_held_batches`（对位写侧 `20204`）。
    WorkerHoldLimitExceeded,
    /// 工种未配 `max_held_batches`（`NULL` = 未配置 ⇒ 不可取件，对位写侧 `20904`）。
    WorkTypeMaxHeldNotSet,
    /// 状态 / 位置组合不在决策表内（保守出口，出现即需补表）。
    Unknown,
}

/// `GET /api/v2/prod/scan/locate` 的 data（`part` + 全部批次）。
#[derive(Debug, Clone, Serialize)]
pub struct LocateOut {
    /// 命中的工单（只读序列号，**不回退 `t_assembly`**，见 `docs/api/scan.md` §8）。
    pub part: LocatePartBrief,
    /// 该 part **全部未软删批次**（含终态），按 `batch_no ASC, pb.id ASC` 排序。
    /// 顺序即「件数递增 + 拆批兜底」，与前端展示顺序一致，不必二次排序。
    pub batches: Vec<LocateBatchItem>,
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

    /// `LocatePartBrief` / `LocateBatchItem` / `LocateOut` 的键集合逐字钉死，
    /// 并锁住雪花 id 的 string 形态（19 位 id 在 JS `Number` 下会丢精度）。
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
        })
        .expect("serialize LocateOut");
        let mut out_keys: Vec<&str> = out
            .as_object()
            .expect("LocateOut 序列化成对象")
            .keys()
            .map(String::as_str)
            .collect();
        out_keys.sort_unstable();
        assert_eq!(out_keys, ["batches", "part"]);
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

    /// `disabled_reason` 的 **10 个** wire 字面量逐字钉死。
    ///
    /// 少一个都会让前端落到 `else` 分支静默显示错误文案；`SCREAMING_SNAKE_CASE`
    /// 的大小写本身也是契约（与 `chain_state` 的 `UPPERCASE` 同款纪律）。
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
                LocateDisabledReason::ProcessChainRequired,
                "\"PROCESS_CHAIN_REQUIRED\"",
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

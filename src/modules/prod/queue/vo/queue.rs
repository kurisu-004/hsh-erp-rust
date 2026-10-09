//! prod::queue 的**下发流**出参（pending / dispatch / auto-dispatch / recall）
//!
//! 2026-10-08 自 `prod::batch::vo` 搬入。搬入理由同 `service/dispatch.rs`：
//! 这 5 类出参只服务下发流一条链，唯一消费方是队列页，与 batch 域的流转 /
//! 返修 / 外协用例无关。
//!
//! 入参见 [`super::dto`]。

use chrono::NaiveDate;
use serde::Serialize;

use crate::shared::types::{serialize_i64, serialize_i64_opt};

/// `GET /api/v2/prod/queue/pending` 单条结构（车间 PENDING 批次 + 工单 + 客户
/// 解析 JOIN 后扁平投影）。
///
/// 字段顺序按业务语义分组：batch 标识 → 工单展示 → 客户 / 申请人 → 派生元数据
/// （is_urgent / version / step_id）。`planned_delivery_date` 是 `String` 而非
/// `NaiveDate` —— 即使 `t_part.planned_delivery_date` 为 NULL，service 也用
/// `"1970-01-01"` 兜底（与既有 list 端点惯例一致；DB `NOT NULL DEFAULT` 已保证
/// 字段非空，此兜底为防御性）。
#[derive(Debug, Clone, Serialize)]
pub struct PendingBatchItem {
    // —— batch 标识 ——
    #[serde(serialize_with = "serialize_i64")]
    pub batch_id: i64,
    #[serde(serialize_with = "serialize_i64")]
    pub part_id: i64,
    pub batch_no: i32,
    pub quantity: i32,
    /// 工单序列号（手工工单可空）
    pub serial_no: Option<String>,
    pub name: String,
    pub drawing_no: String,
    /// 计划交期（`String` 而非 `NaiveDate`，NULL 走 `"1970-01-01"` 兜底）。
    pub planned_delivery_date: String,
    /// 系统交期（`NaiveDate` 原生序列化；NULL → JSON `null`）。
    pub system_delivery_date: Option<NaiveDate>,
    // —— 客户 / 申请人 ——
    /// L2 客户名（叶子）
    pub customer_name: Option<String>,
    /// L1 客户名（一级集团），L2.parent_id 为空时为 None
    pub parent_customer_name: Option<String>,
    /// 申请人字符串（来自 `t_part.applicant_name` LEFT JOIN `t_applicant.name`，
    /// applicant 软删 / 不存在时为 None）。
    pub applicant_name: Option<String>,
    // —— 派生元数据 ——
    pub is_urgent: bool,
    /// 工单级备注（`t_part.note`）
    pub note: Option<String>,
    pub version: i32,
    /// `t_part_batch.current_process_step_id`（链内位置指针）的原样投影。
    ///
    /// **NULL 兜底语义**：DB 列 NULL 时 row → vo 投影为 0（`Option<i64> → i64`
    /// 走 `.unwrap_or(0)`）；前端按 `0 == "未设 step"`、`> 0 == "已设 step"`
    /// 区分，故 JSON 是字符串 `"0"`、**不是** `null`。
    ///
    /// **取值范围**（2026-10-10 review 第 1 轮订正）：本列表闸门是
    /// `status IN ('PENDING','PROGRAMMING')`（`repo/dispatch.rs::list_pending_batches`），
    /// 而 dispatch 一落笔就写 `IN_PROCESS` ⇒ **下发过的批次不会留在本列表**，
    /// dispatch 对该列的两种写入（有链写链首 step / 无链清 NULL，见
    /// `QueueDispatchRepo::update_batch_dispatched`）在本列表里都观察不到。
    /// 常规取值因此是 `"0"`，但 DB 层**没有任何约束**保证待下发行的该列恒为 NULL
    /// —— `allowed_from` 之外的旁路写点、手工 SQL、历史脏数据都能破坏它，前端不要
    /// 假定它恒为 `"0"`，按 `> 0` 分支走即可。
    ///
    /// 注：本字段未走 `Option<i64>` 是为了对齐本 VO 整体扁平数字风格（与
    /// `process_chain_id` 同形态）。
    #[serde(serialize_with = "serialize_i64")]
    pub current_process_step_id: i64,
    /// `t_part.process_chain_id`（PR-3 step 化后新字段；PENDING 列表透传
    /// 给前端做 UI 关联）。
    #[serde(serialize_with = "serialize_i64")]
    pub process_chain_id: i64,
}

/// `GET /api/v2/prod/queue/pending` 顶层响应。
#[derive(Debug, Clone, Serialize)]
pub struct PendingBatchListOut {
    pub items: Vec<PendingBatchItem>,
    pub total: i64,
    pub limit: i64,
    pub offset: i64,
}

// ===== dispatch result =====

/// `POST /api/v2/prod/queue/dispatch` 单条结果（bulk-only：单条下发即
/// `succeeded.len() == 1`）。
///
/// 2026-09-30 重构：原 `DispatchResult`（单条）+ `BulkDispatchResult`（succeeded/failed）
/// 合并为统一 bulk 形态 `DispatchResult { succeeded }`：
/// - 单条下发 = 1 元素 succeeded
/// - 多批下发 = N 元素 succeeded（全成功）或 service 抛 AppError（任一硬错误全回滚，
///   响应为顶层 4xx/5xx，failed 数组废弃）
///
/// 当前实现走「任一失败 → 全回滚」语义；`failed` 字段保留为 `Vec<DispatchFailureItem>`
/// 是为未来启用 partial commit 时向前兼容，**当前总是空**。
///
/// `current_process_step_id` 是 `Option<i64>`：有链工单（`t_part.process_chain_id
/// IS NOT NULL`）为链内第一道未软删 step 的 id，无链工单为 `None`（无链侧是
/// **显式清空**该列，不采用「保留原值」写法 —— 那要论证「无链 ⇒ step 恒 NULL」
/// 这条无任何约束保证的不变式）—— 2026-10-09 起不再「恒 `None`」。
/// `current_process_id`（2026-09-30 新增）是 `Option<i64>`：有链工单下它等于
/// **链首 step 的工序**（= 实际下发到的那道）—— 池归属的权威依据；有链时请求里的
/// `target_process_id` 被忽略，且**出参的 `target_process_id` 也被 service 用同一个
/// 值覆盖**（见两个字段各自的 doc），只有无链工单它才等于请求值。
#[derive(Debug, Clone, Serialize)]
pub struct DispatchResult {
    /// 成功下发的 batch 列表（顺序与 req.targets 一致）。
    pub succeeded: Vec<DispatchSuccessItem>,
    /// 失败明细（当前总为空；预留 partial commit 启用）
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failed: Vec<DispatchFailureItem>,
}

/// `DispatchResult.succeeded` 单条（每条 target 对应一个）。
///
/// 2026-09-30 新增 `current_process_id`（工序池归属权威依据）。
/// 2026-10-09：`current_process_step_id` 由「恒 `null`（dispatch 不解析 step）」
/// 改为**真实写入值** —— 有链工单按下发链首工序，指针落链首 step。
#[derive(Debug, Clone, Serialize)]
pub struct DispatchSuccessItem {
    #[serde(serialize_with = "serialize_i64")]
    pub batch_id: i64,
    /// 下发后 `t_part_batch.current_process_step_id` 的值（逻辑 FK →
    /// `t_process_chain_step.id`）。JSON 里是**字符串**（`serialize_i64_opt`，
    /// 与本 VO 里 `batch_id` 等 id 字段同形态），`None` → JSON `null`。
    ///
    /// - 有链工单（`t_part.process_chain_id IS NOT NULL`）→ 链内第一道未软删 step 的
    ///   id（口径与端点 5 的 `first_process_id` 同源）；
    /// - 无链工单 → `null`（无链侧由 dispatch **显式清空**该列；不采用「保留原值」
    ///   写法 —— 那要论证「无链 ⇒ step 恒 NULL」这条无任何约束保证的不变式，
    ///   见 `repo/dispatch.rs::update_batch_dispatched` 的 doc 与
    ///   `docs/api/queue.md` §3.1）。
    ///
    /// ⚠️ **不能落成 JSON number**（2026-10-10 补）：step id 是雪花 id，量级
    /// 8.7×10¹⁷，远超 JS 的 `Number.MAX_SAFE_INTEGER`（2^53 ≈ 9.007×10¹⁵），
    /// number 进 JS 即被舍入 —— 前端即便把 Zod 改成收 number，拿到的也是错值。
    /// f9f98886 把本字段从「恒 `null`」改成真实写入值时漏加了字符串化器，
    /// 现场表现为「批次已下发成功、车间却看到下发失败」（Zod 抛在 HTTP 200 之后）。
    #[serde(serialize_with = "serialize_i64_opt")]
    pub current_process_step_id: Option<i64>,
    /// 下发后写入 `t_part_batch.current_process_id` 的值（逻辑 FK → `t_process.id`）。
    ///
    /// ⚠️ **有链工单下它等于链首 step 的工序**，而不是请求里那道工序（请求里的
    /// `target_process_id` 此时被忽略，见 `DispatchTarget::target_process_id` 的 doc）。
    /// 前端要展示「实际下发到哪道工序」读本字段即可；不过有链时出参的
    /// `target_process_id` 也被 service 覆盖成同一个值，两者读哪个结果一致
    /// （口径差异见下字段的 doc）。
    ///
    /// **Option 语义**：当前 dispatch 路径恒为 `Some(..)`；保留 `Option` 是为了与
    /// `current_process_step_id` 对齐并为将来「工序落空」的分支留出 `null` 表达。
    /// None → JSON `null`，避免前端拿 `"0"` 误判为合法工序 id。
    #[serde(serialize_with = "serialize_i64_opt")]
    pub current_process_id: Option<i64>,
    /// **不是请求字段的回声**（2026-10-10 登记）：service 解析链首时用
    /// `let (target_process_id, …)` 遮蔽了同名形参，并把遮蔽后的值同时填进
    /// `current_process_id` 与本字段 ⇒ **有链工单下本字段已被覆盖为链首 step 的
    /// 工序，与 `current_process_id` 同值**；只有无链工单才等于请求里传的那道
    /// （回落值）。
    ///
    /// 前端读它也能拿到「实际下发到哪道工序」，但**不能**用它复现「用户当时传了
    /// 什么」—— 那在有链工单下已经丢失。口径表见 `docs/api/queue.md` §3.1。
    #[serde(serialize_with = "serialize_i64")]
    pub target_process_id: i64,
    #[serde(serialize_with = "serialize_i64")]
    pub shelf_id: i64,
    pub version: i32,
}

/// `DispatchResult.failed` 单条（当前总为空；为 partial commit 启用预留）。
///
/// 注：本类型当前未被任何 service 代码生成，但保留作为 VO schema 的稳定部分；
/// 未来 partial commit 启用时，service 在每条失败处 push `DispatchFailureItem` 而非抛错。
#[derive(Debug, Clone, Serialize)]
pub struct DispatchFailureItem {
    #[serde(serialize_with = "serialize_i64")]
    pub batch_id: i64,
    pub code: i32,
    pub message: String,
}

// ===== auto-dispatch preview (2026-09-30 重构为只读查询) =====

/// `POST /api/v2/prod/queue/auto-dispatch` 单条预览项。
///
/// 2026-09-30 重构：原 `auto_dispatch` 改为只读 `auto_dispatch_preview`，
/// 不再真正下发批次，仅返回每个 batch 的「首道工序 + 首货架」+ skip_reason。
/// 实际下发仍走 `POST /api/v2/prod/queue/dispatch`，caller 据此构造
/// `targets: [{batch_id, target_process_id}]` 发起真正下发。
///
/// 字段语义：
/// - `batch_id` / `part_id` —— 必填
/// - `process_chain_id` / `first_process_id` / `first_process_code` / `first_process_name`
///   —— 当 `skip_reason = NO_PROCESS_CHAIN / NO_PROCESS_STEP` 时为 None
/// - `first_shelf_id` —— 当首道工序未映射货架时为 None（skip_reason=NO_SHELF）
/// - `skip_reason` —— NOT_FOUND / NO_PROCESS_CHAIN / NO_PROCESS_STEP / NO_SHELF 之一
///   或 None（一切就绪可下发）
#[derive(Debug, Clone, Serialize)]
pub struct AutoDispatchItem {
    #[serde(serialize_with = "serialize_i64")]
    pub batch_id: i64,
    #[serde(serialize_with = "serialize_i64")]
    pub part_id: i64,
    /// `t_part.process_chain_id`（PR-3 step 化后新字段）
    ///
    /// **Option 语义**：当 `skip_reason = NO_PROCESS_CHAIN` 时为 None；其余情形透传 part 实际
    /// 值（即便其它上层查不到也保持原值不动——避免误导 frontend）。None → JSON `null`，避免
    /// 前端拿 `"0"` 误判为合法 chain。
    ///
    /// 2026-09-30 review 第 1 轮：原为 `i64 + serialize_i64` + service `unwrap_or(0)` 兜底，
    /// 导致 NO_PROCESS_CHAIN 时输出 `"process_chain_id": "0"`；改为 Option 与 plan §3.2 对齐。
    #[serde(serialize_with = "serialize_i64_opt")]
    pub process_chain_id: Option<i64>,
    /// 首道工序 id（`t_process_chain_step` sort_order=1 行的 process_id）
    ///
    /// **Option 语义**：当 `skip_reason = NO_PROCESS_CHAIN / NO_PROCESS_STEP` 时为 None；
    /// OK 时为 Some(process_id)；NO_SHELF 时仍 Some（首道工序存在但未映射货架）。
    /// None → JSON `null`。
    #[serde(serialize_with = "serialize_i64_opt")]
    pub first_process_id: Option<i64>,
    pub first_process_code: String,
    pub first_process_name: String,
    /// 首道工序对应的候选货架（按 `t_shelf_process.sort_order ASC`）
    ///
    /// **Option 语义**：当 `skip_reason = NO_PROCESS_CHAIN / NO_PROCESS_STEP / NO_SHELF`
    /// 时为 None；其余情形为 Some(shelf_id)。None → JSON `null`。
    #[serde(serialize_with = "serialize_i64_opt")]
    pub first_shelf_id: Option<i64>,
    /// 取不到任一上游数据时的原因：NOT_FOUND / NO_PROCESS_CHAIN / NO_PROCESS_STEP / NO_SHELF
    /// （OK 时为 None）
    pub skip_reason: Option<String>,
}

/// `POST /api/v2/prod/queue/auto-dispatch` 顶层响应。
#[derive(Debug, Clone, Serialize)]
pub struct AutoDispatchResult {
    pub items: Vec<AutoDispatchItem>,
}

/// `POST /api/v2/prod/queue/recall` 出参。
///
/// 2026-10-08 契约变更：原 `POST /prod/batches/{batch_id}/recall-to-pending`
/// 返 `part::vo::PartOut`（工单全量投影），本端点改返本 VO —— 召回的语义锚点是
/// **批次**，返工单投影会让前端为拿 `part_id` 而解析一个上百字段的对象，且批次
/// 自己的 `version`（OCC 锚）根本没在里面，前端下一次操作拿不到正确的版本号。
#[derive(Debug, Clone, Serialize)]
pub struct RecallOut {
    /// 雪花 ID 字符串化
    pub batch_id: String,
    pub part_id: String,
    /// 批次 `version + 1`（写入后）。前端下一次对本批次的操作必须带这个值做 OCC。
    pub version: i32,
}

#[cfg(test)]
mod tests {
    //! `DispatchSuccessItem` 的 5 个 id 字段的 **wire 形态**守卫。
    //!
    //! 防的是「VO 层少一个 `serialize_with`」这种漂移。前端
    //! `productionQueueSchema.ts` 对这些字段声明的是 `z.string().nullable()`，一旦
    //! 后端某个 id 裸序列化落成 JSON **number**，Zod 就抛
    //! `expected string, received number` —— 而这发生在 HTTP 200 **之后**：批次
    //! 真的提交了，车间看到的却是「下发失败」，现场与根因隔了三层，谁也不会往
    //! 「一个 serde 属性」上想。2026-10-10 这次漂移能一路溜到生产，正是因为 VO 层
    //! 当时没有任何序列化形态断言，集成测试也只按 number 写（错误地固化了下来）。
    //!
    //! 用 19 位雪花 id 而非小整数：小 id 在 JS 里当 number 也不丢精度，这条断言才
    //! 真的在测「字符串化器在不在」，而不是碰巧相等。
    use super::DispatchSuccessItem;

    /// 5 个 id 字段全部填成 `Some(…)`。
    fn dispatch_success_item_some_ids() -> DispatchSuccessItem {
        DispatchSuccessItem {
            batch_id: 1590000000000000002,
            current_process_step_id: Some(1590000000000000001),
            current_process_id: Some(1590000000000000003),
            target_process_id: 1590000000000000004,
            shelf_id: 1590000000000000005,
            version: 2,
        }
    }

    /// 有值时 5 个 id 字段一律 JSON **字符串**，且十进制内容与传入值逐字一致。
    #[test]
    fn dispatch_success_item_ids_serialize_as_strings() {
        let value = serde_json::to_value(dispatch_success_item_some_ids())
            .expect("serialize DispatchSuccessItem");
        for (key, raw) in [
            ("batch_id", 1590000000000000002i64),
            ("current_process_step_id", 1590000000000000001),
            ("current_process_id", 1590000000000000003),
            ("target_process_id", 1590000000000000004),
            ("shelf_id", 1590000000000000005),
        ] {
            assert_eq!(
                value[key],
                serde_json::Value::String(raw.to_string()),
                "{key} 必须是字符串形态的雪花 id（漏了 serialize_with 就退成 number，\
                 前端 Zod 会炸在 HTTP 200 之后）"
            );
        }
    }

    /// `Option` 字段的 `None` → JSON `null`（不是 `"0"`、也不能整个键消失：
    /// 前端 `z.string().nullable()` 两条都只认前者）。非 Option 的
    /// `target_process_id` / `shelf_id` 恒有值，此处一并钉住它们的键不许被
    /// `skip_serializing_if` 摘掉。
    #[test]
    fn dispatch_success_item_none_ids_serialize_as_null() {
        let value = serde_json::to_value(DispatchSuccessItem {
            current_process_step_id: None,
            current_process_id: None,
            ..dispatch_success_item_some_ids()
        })
        .expect("serialize DispatchSuccessItem");
        let object = value
            .as_object()
            .expect("DispatchSuccessItem 应序列化为 JSON object");
        for key in ["current_process_step_id", "current_process_id"] {
            assert_eq!(
                object.get(key),
                Some(&serde_json::Value::Null),
                "{key} 的 None 必须是 JSON null（键必须出现，\
                 消失或退成 \"0\" 都会被前端 z.string().nullable() 拒收）"
            );
        }
        for key in ["batch_id", "target_process_id", "shelf_id"] {
            assert!(object.contains_key(key), "{key} 恒有值，键不许消失");
        }
    }
}

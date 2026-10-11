//! `prod::scan` 扫码接管端点（`POST /scan/take-over`）的出参 VO
//!
//! 2026-10-15 新增（第 7 条端点，S2）。`TakeOverOut` 是「扫码找件 · 自助恢复」
//! 第二件写端点的响应体：接管完成后，前端据此知道「这一批从谁转到了谁、被推进到
//! 哪道工序、新的 OCC 锚是多少」。
//!
//! ## 与 `LocateBatchItem`（`locate.rs`）的衔接
//! `locate` 诊断出的 `TAKE_OVER` 行动作，前端发起写请求时用到的两个事实
//! （`batch_id` / `version`）逐字来自诊断行的同名列；本 VO 回传的新 `version`
//! 是**接管后**的批次锚（`旧 version + 1`），前端继续用它做下一次动作的 OCC。
//!
//! ## 内部管道字段
//! `synced_assembly_id` 与 `WorkerScanCoreOut` 的 `work_type_id` / `badge_code`
//! 同类：**不进 JSON**（`#[serde(skip)]`），只服务 handler 在 commit 之后补发
//! `ASSEMBLY_UPDATED`。字段存在但键集仍严格等于下面这 7 个 —— 键集护栏单测
//! `take_over_keys_are_pinned` 会连内部字段一起挡在 JSON 之外。

use serde::Serialize;

use crate::shared::types::{serialize_i64, serialize_i64_opt};

/// `POST /api/v2/prod/scan/take-over` 的 data（7 个 JSON 键）。
#[derive(Debug, Clone, Serialize)]
pub struct TakeOverOut {
    /// `t_part_batch.id`（`serialize_i64` → JSON string）。被接管的批次。
    #[serde(serialize_with = "serialize_i64")]
    pub batch_id: i64,
    /// 所属工单（批次行反查，`serialize_i64` → JSON string）。
    #[serde(serialize_with = "serialize_i64")]
    pub part_id: i64,
    /// **旧**持有人（`t_worker.id`，接管前批次行上的 `current_holder_id`）。
    /// ⚠️ 服务端层面恒非空：`location='WORKER'` 但无持有人的行按数据异常拒收
    /// （`20103`），不存在「从 Nobody 手上接管」的分支。
    #[serde(serialize_with = "serialize_i64")]
    pub from_worker_id: i64,
    /// **新**持有人（扫码的接管人工牌反查出的 `t_worker.id`）。
    #[serde(serialize_with = "serialize_i64")]
    pub to_worker_id: i64,
    /// 接管后批次落到哪道工序（链上推导或前端显式指定，判据见 service doc）。
    #[serde(serialize_with = "serialize_i64")]
    pub next_process_id: i64,
    /// 目标工序在链内的 step id；**无链时可空**（`serialize_i64_opt` →
    /// JSON string|null）。有链时由 `optional_step_id` 解析（链内定位不到 → `20702`，
    /// 不会走到这里）。
    #[serde(serialize_with = "serialize_i64_opt")]
    pub next_process_step_id: Option<i64>,
    /// 接管**后**的 `t_part_batch.version`（= 客户端传值 + 1，本仓约定
    /// `version = version + 1`）。前端继续拿它做下一次动作的 OCC 锚。
    pub version: i32,
    /// 内部管道字段：`sync_from_batch_change` 的派生结果（父装配件真变了才有值），
    /// handler 据此在 commit 之后补发 `ASSEMBLY_UPDATED`。**不进 JSON** —— 与本
    /// 结构体的 7 个键无关，`take_over_keys_are_pinned` 钉住它不出现。
    #[serde(skip)]
    pub synced_assembly_id: Option<i64>,
}

#[cfg(test)]
mod tests {
    //! 键集 / 雪花 string 形态护栏（同 `locate.rs::locate_keys_are_pinned` 的口径）：
    //! 加 / 删字段、或误把内部管道字段序列化出去，都必须先改本测试并同步前端 schema。
    use super::*;

    fn sample() -> TakeOverOut {
        TakeOverOut {
            batch_id: 1,
            part_id: 2,
            from_worker_id: 3,
            to_worker_id: 4,
            next_process_id: 5,
            next_process_step_id: None,
            version: 7,
            synced_assembly_id: Some(9),
        }
    }

    /// 7 个 JSON 键逐字钉死 + 雪花 id 的 string 形态 + `version` 是裸 number +
    /// `next_process_step_id` 可空 + 内部字段 `synced_assembly_id` 不进 JSON。
    #[test]
    fn take_over_keys_are_pinned() {
        let out = serde_json::to_value(sample()).expect("serialize TakeOverOut");
        let mut keys: Vec<&str> = out
            .as_object()
            .expect("TakeOverOut 序列化成对象")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "batch_id",
                "from_worker_id",
                "next_process_id",
                "next_process_step_id",
                "part_id",
                "to_worker_id",
                "version",
            ],
            "TakeOverOut 的字段集变了：加 / 删字段要同步前端 schema 与 docs/api/scan.md"
        );
        // 雪花 id 一律 string（19 位在 JS Number 下会丢精度）
        assert_eq!(out["batch_id"], serde_json::json!("1"));
        assert_eq!(out["part_id"], serde_json::json!("2"));
        assert_eq!(out["from_worker_id"], serde_json::json!("3"));
        assert_eq!(out["to_worker_id"], serde_json::json!("4"));
        assert_eq!(out["next_process_id"], serde_json::json!("5"));
        // 无链时 step 为 null（**不是** "0" 兜底 —— 与 locate 的 next_process_id 同款口径）
        assert_eq!(out["next_process_step_id"], serde_json::Value::Null);
        // version 是裸 number
        assert_eq!(out["version"], serde_json::json!(7));
        // 内部管道字段不得进 JSON
        assert_eq!(
            out.get("synced_assembly_id"),
            None,
            "synced_assembly_id 是 handler 内部管道字段，必须 #[serde(skip)]"
        );
    }

    /// 有链时 `next_process_step_id` 序列化为 JSON string。
    #[test]
    fn take_over_step_id_serializes_as_string_when_some() {
        let out = TakeOverOut {
            next_process_step_id: Some(6),
            ..sample()
        };
        let json = serde_json::to_value(out).expect("serialize");
        assert_eq!(json["next_process_step_id"], serde_json::json!("6"));
    }
}

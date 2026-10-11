//! `prod::scan` 外协确认收回端点（`POST /scan/outsource-receive`）的出参 VO
//!
//! 2026-10-15 新增（第 8 条端点，S3）。`OutsourceReceiveOut` 是「扫码找件 · 自助恢复」
//! 第三件写端点的响应体：任务完成后，前端据此知道「哪一批从哪家公司收回、交到了哪个
//! 工人、落在哪道工序、新的 OCC 锚是多少」。
//!
//! ## 与 `LocateBatchItem`（`locate.rs`）的衔接
//! `locate` 诊断出的 `RECEIVE_FROM_OUTSOURCE` 行动作，前端发起写请求时用到的两个事实
//! （`batch_id` / `version`）逐字来自诊断行的同名列；本 VO 回传的新 `version` 是
//! **收回 + 交人两段写之后**的批次锚（内部由库回答、不手算），前端继续用它做下一次
//! 动作的 OCC。
//!
//! ## 内部管道字段
//! `picked_quantity` 与 `WorkerScanOut` 的 `split` 同类：**不进 JSON**
//! （`#[serde(skip)]`），只服务 handler 在 commit 之后补发 `PART_PICKED_UP`
//! （payload 与 pick-up 端点逐字同形）。字段存在但键集严格等于下面这 6 个 ——
//! 键集护栏单测 `outsource_receive_keys_are_pinned` 会连内部字段一起挡在 JSON 之外。

use serde::Serialize;

use crate::shared::types::serialize_i64;

/// `POST /api/v2/prod/scan/outsource-receive` 的 data（6 个 JSON 键）。
#[derive(Debug, Clone, Serialize)]
pub struct OutsourceReceiveOut {
    /// `t_part_batch.id`（`serialize_i64` → JSON string）。被收回的批次。
    #[serde(serialize_with = "serialize_i64")]
    pub batch_id: i64,
    /// 所属工单（批次行反查，`serialize_i64` → JSON string）。
    #[serde(serialize_with = "serialize_i64")]
    pub part_id: i64,
    /// **新**持有人（扫码的收回人工牌反查出的 `t_worker.id`）。
    #[serde(serialize_with = "serialize_i64")]
    pub worker_id: i64,
    /// **收回前**的在途公司（`t_part_batch.current_holder_id` 收回前的值）。
    /// ⚠️ 服务端层面恒非空：`location='OUTSOURCE_COMPANY'` 但无持有人的行按数据
    /// 异常拒收（`20122`），不存在「从 Nobody 手上收回」的分支。
    #[serde(serialize_with = "serialize_i64")]
    pub company_id: i64,
    /// 收回后批次落到哪道工序（链上推导或前端显式指定，判据见 service doc）。
    #[serde(serialize_with = "serialize_i64")]
    pub next_process_id: i64,
    /// 收回 + 交人**两段写之后**的 `t_part_batch.version`（由库写后读回，
    /// **不是**请求的 `version + 2`）。前端继续拿它做下一次动作的 OCC 锚。
    pub version: i32,
    /// 内部管道字段：本次实际交到工人手上的数量（= 批次余量），供 handler 在
    /// commit 之后补发 `PART_PICKED_UP`（payload 与 `pick-up` 端点逐字同形）。
    /// **不进 JSON** —— `outsource_receive_keys_are_pinned` 钉住它不出现。
    #[serde(skip)]
    pub picked_quantity: i32,
}

#[cfg(test)]
mod tests {
    //! 键集 / 雪花 string 形态护栏（同 `take_over.rs::take_over_keys_are_pinned` 的口径）：
    //! 加 / 删字段、或误把内部管道字段序列化出去，都必须先改本测试并同步前端 schema。
    use super::*;

    fn sample() -> OutsourceReceiveOut {
        OutsourceReceiveOut {
            batch_id: 1,
            part_id: 2,
            worker_id: 3,
            company_id: 4,
            next_process_id: 5,
            version: 7,
            picked_quantity: 9,
        }
    }

    /// 6 个 JSON 键逐字钉死 + 雪花 id 的 string 形态 + `version` 是裸 number +
    /// 内部字段 `picked_quantity` 不进 JSON。
    #[test]
    fn outsource_receive_keys_are_pinned() {
        let out = serde_json::to_value(sample()).expect("serialize OutsourceReceiveOut");
        let mut keys: Vec<&str> = out
            .as_object()
            .expect("OutsourceReceiveOut 序列化成对象")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "batch_id",
                "company_id",
                "next_process_id",
                "part_id",
                "version",
                "worker_id",
            ],
            "OutsourceReceiveOut 的字段集变了：加 / 删字段要同步前端 schema 与 docs/api/scan.md"
        );
        // 雪花 id 一律 string（19 位在 JS Number 下会丢精度）
        assert_eq!(out["batch_id"], serde_json::json!("1"));
        assert_eq!(out["part_id"], serde_json::json!("2"));
        assert_eq!(out["worker_id"], serde_json::json!("3"));
        assert_eq!(out["company_id"], serde_json::json!("4"));
        assert_eq!(out["next_process_id"], serde_json::json!("5"));
        // version 是裸 number
        assert_eq!(out["version"], serde_json::json!(7));
        // 内部管道字段不得进 JSON
        assert_eq!(
            out.get("picked_quantity"),
            None,
            "picked_quantity 是 handler 内部管道字段，必须 #[serde(skip)]"
        );
    }
}

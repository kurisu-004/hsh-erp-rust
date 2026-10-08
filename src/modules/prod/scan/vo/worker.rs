//! prod::scan 报工台**工人**出参 VO（`POST /scan/verify-badge`）

use serde::Serialize;

use crate::shared::types::serialize_i64;

/// `POST /api/v2/prod/scan/verify-badge` 出参（4 字段）。
///
/// 2026-10-10 自 `prod::worker::vo::WorkerOut`（12 字段）收敛。报工台扫工牌后只读
/// 4 个字段，其余 8 个是纯 wire 兼容负载：
///
/// | 字段 | 报工台的用途 |
/// |---|---|
/// | `id` | 发 `GET /scan/held?worker_id=` |
/// | `badge_code` | 顶栏显示 + worker-scan 的 `badge_code` 入参 |
/// | `name` | 顶栏显示 |
/// | `work_type_id` | 发 `GET /scan/pickable?work_type_id=` |
///
/// 被砍掉的 8 个：`id_card_no` / `phone` / `is_active` / `work_type_name` / `version`
/// / `created_at` / `updated_at`。其中 `work_type_name` 在 `verify_badge` 路径上
/// 本就恒 `null`（service 不做工种名回填，只 `GET /workers/{id}` 与列表端点才填）。
///
/// ⚠️ **`WorkerOut` 不删**：`GET /api/v2/prod/workers/{id}` 与 worker 列表端点仍在
/// 用它，只是不再被 `verify_badge` 引用。
#[derive(Debug, Clone, Serialize)]
pub struct ScanWorkerBrief {
    #[serde(serialize_with = "serialize_i64")]
    pub id: i64,
    pub badge_code: String,
    pub name: String,
    /// 未分配工种的工人为 `None`（`t_worker.work_type_id` 可空）⇒ JSON `null`。
    #[serde(serialize_with = "crate::shared::types::serialize_i64_opt")]
    pub work_type_id: Option<i64>,
}

#[cfg(test)]
mod tests {
    //! 出参形状守卫：键集合逐字钉死，前端多消费一个字段时立刻能看出来。
    //!
    //! `ScanWorkerBrief` 只有 4 个键 —— 前端 `views/scan/` 四个页面合计只读这 4 个
    //! （`worker?.id` / `worker?.badge_code` / `worker?.name` / `worker?.work_type_id`）。
    //! 将来真要加字段（例如顶栏要显示工种名），改这里 + 加消费方 + 同步前端类型。
    use super::ScanWorkerBrief;

    #[test]
    fn keys_are_exactly_four() {
        let brief = ScanWorkerBrief {
            id: 1_590_000_000_000_000_001,
            badge_code: "B001".into(),
            name: "张三".into(),
            work_type_id: Some(1_590_000_000_000_000_002),
        };
        let json = serde_json::to_value(&brief).expect("serialize ScanWorkerBrief");
        let mut keys: Vec<&str> = json
            .as_object()
            .expect("ScanWorkerBrief 序列化成对象")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            ["badge_code", "id", "name", "work_type_id"],
            "ScanWorkerBrief 的字段集变了：加字段要同步前端 Worker 类型与 views/scan 的消费方"
        );
        // 雪花 id 走 JSON string
        assert_eq!(json["id"], serde_json::json!("1590000000000000001"));
        assert_eq!(
            json["work_type_id"],
            serde_json::json!("1590000000000000002")
        );
    }

    /// 未分配工种的工人：`work_type_id` 为 `null`（键仍在）。
    #[test]
    fn work_type_id_null_keeps_its_key() {
        let brief = ScanWorkerBrief {
            id: 1,
            badge_code: "B001".into(),
            name: "张三".into(),
            work_type_id: None,
        };
        let json = serde_json::to_value(&brief).expect("serialize ScanWorkerBrief");
        assert_eq!(json["work_type_id"], serde_json::Value::Null);
        assert!(json.as_object().unwrap().contains_key("work_type_id"));
    }
}

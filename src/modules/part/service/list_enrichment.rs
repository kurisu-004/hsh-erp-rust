//! part 列表「位置 / 持有人」派生层（2026-09-22 review 第 2 轮从 `crud.rs` 抽出）
//!
//! 2026-09-16 PR-2 瘦身（migration 027）：t_part 删 `location` /
//! `current_holder_id`（已删列），列表页需要的「位置 / 持有人」展示由
//! service 层在 list_parts 内按 min-progress 活跃批次派生。
//!
//! M1 重构：原 1054 行超限 `crud.rs` 抽出本文件（独立单文件 < 200 行），
//! 让 `crud.rs` 重回 1000 行上限内。函数本身仍是 service 层的「跨三表
//! 派生」职责，签名 `<R: PartRepoTrait>` 收胖 trait（trait impl for
//! `&mut PgConnection` 与其它域一致）。

use std::collections::{HashMap, HashSet};

use crate::modules::part::repo::PartRepoTrait;
use crate::modules::part_batch::model::TPartBatch;
use crate::modules::part_batch::repo::PartBatchRepo;
use crate::shared::error::AppError;

/// 列表页「位置 / 持有人」派生（按 min-progress 活跃批次，跨 t_shelf /
/// t_worker / t_outsource_company 三表解析 holder 名）。
///
/// 输入：分页内的 part ids（去重）。
/// 输出：`HashMap<part_id, (Option<location>, Option<holder_name>)>`；
/// `part_id` 不在结果中 → caller 走 `(None, None)` 默认值（视为无活跃批次）。
///
/// 派生规则：
/// - min-progress 活跃批次选择：与 `compute_part_target` 一致 —— 排除
///   `CANCELLED`；非空时再排除 `COMPLETED`；剩余取 `part_status_progress`
///   最小者；多批 progress 相等时取首条（与 rollup 行为对齐）。
/// - holder_name 解析：按目标批次 `location` 分桶：
///   - `PRODUCTION_SHELF` / `INSPECTION_SHELF` → `t_shelf.code`
///   - `WORKER` → `t_worker.name`
///   - `OUTSOURCE_COMPANY` → `t_outsource_company.name`
///   - `OFFICE` / `None` / 无活跃批次 → `None`
///
/// SQL 数：4 条（与页大小 N 无关）：
/// 1. 一次性拉所有 part 的活跃批次（`list_active_by_part_ids`）
///    2-4. t_shelf / t_worker / t_outsource_company 各 1 条 `WHERE id = ANY(...)`
pub(super) async fn enrich_part_list_with_location_and_holder<R: PartRepoTrait>(
    repo: &mut R,
    part_ids: &[i64],
) -> Result<HashMap<i64, (Option<String>, Option<String>)>, AppError> {
    let mut out: HashMap<i64, (Option<String>, Option<String>)> = HashMap::new();
    if part_ids.is_empty() {
        return Ok(out);
    }

    // 1. 拉所有 part 的活跃批次（O(1) SQL）。
    let batches = PartBatchRepo::list_active_by_part_ids(repo.conn_mut(), part_ids).await?;

    // 2. 按 part_id 分桶 + Rust 内 min-progress 选目标批次。
    let mut per_part: HashMap<i64, Vec<&TPartBatch>> = HashMap::new();
    for b in &batches {
        per_part.entry(b.part_id).or_default().push(b);
    }
    let mut target_per_part: HashMap<i64, &TPartBatch> = HashMap::new();
    for (part_id, bs) in per_part {
        // 排除 CANCELLED。
        let non_cancelled: Vec<&&TPartBatch> =
            bs.iter().filter(|b| b.status != "CANCELLED").collect();
        let candidates: Vec<&&TPartBatch> = if !non_cancelled.is_empty() {
            // 非空时排除 COMPLETED。
            let non_terminal: Vec<&&TPartBatch> = non_cancelled
                .iter()
                .copied()
                .filter(|b| b.status != "COMPLETED")
                .collect();
            if !non_terminal.is_empty() {
                non_terminal
            } else {
                non_cancelled
            }
        } else {
            bs.iter().collect()
        };
        // min progress（与 statemachine::part_status_progress 对齐）。
        if let Some(min) = candidates
            .iter()
            .min_by_key(|b| part_status_progress_inline(&b.status))
            .copied()
        {
            target_per_part.insert(part_id, min);
        }
    }

    // 3. 把目标批次的 current_holder_id 按 location 分桶。
    let mut shelf_ids: HashSet<i64> = HashSet::new();
    let mut worker_ids: HashSet<i64> = HashSet::new();
    let mut outsource_ids: HashSet<i64> = HashSet::new();
    for b in target_per_part.values() {
        if let Some(hid) = b.current_holder_id {
            match b.location.as_deref() {
                Some("PRODUCTION_SHELF") | Some("INSPECTION_SHELF") => {
                    shelf_ids.insert(hid);
                }
                Some("WORKER") => {
                    worker_ids.insert(hid);
                }
                Some("OUTSOURCE_COMPANY") => {
                    outsource_ids.insert(hid);
                }
                _ => {}
            }
        }
    }

    // 4. 解析名称（每桶 1 条 SQL）。每次现调 `conn_mut()` 取得 fresh reborrow，
    //    避免 `&mut PgConnection` 一次移动 / 多次借用冲突。
    let mut shelf_names: HashMap<i64, String> = HashMap::new();
    if !shelf_ids.is_empty() {
        let ids: Vec<i64> = shelf_ids.iter().copied().collect();
        let rows: Vec<(i64, String)> = sqlx::query_as(
            "SELECT id, code FROM t_shelf WHERE id = ANY($1) AND deleted_at IS NULL",
        )
        .bind(&ids)
        .fetch_all(repo.conn_mut())
        .await?;
        for (id, code) in rows {
            shelf_names.insert(id, code);
        }
    }
    let mut worker_names: HashMap<i64, String> = HashMap::new();
    if !worker_ids.is_empty() {
        let ids: Vec<i64> = worker_ids.iter().copied().collect();
        let rows: Vec<(i64, String)> = sqlx::query_as(
            "SELECT id, name FROM t_worker WHERE id = ANY($1) AND deleted_at IS NULL",
        )
        .bind(&ids)
        .fetch_all(repo.conn_mut())
        .await?;
        for (id, name) in rows {
            worker_names.insert(id, name);
        }
    }
    let mut outsource_names: HashMap<i64, String> = HashMap::new();
    if !outsource_ids.is_empty() {
        let ids: Vec<i64> = outsource_ids.iter().copied().collect();
        let rows: Vec<(i64, String)> = sqlx::query_as(
            "SELECT id, name FROM t_outsource_company WHERE id = ANY($1) AND deleted_at IS NULL",
        )
        .bind(&ids)
        .fetch_all(repo.conn_mut())
        .await?;
        for (id, name) in rows {
            outsource_names.insert(id, name);
        }
    }

    // 5. 组装结果。
    for (part_id, b) in target_per_part {
        let location = b.location.clone();
        let holder_name = b
            .current_holder_id
            .and_then(|hid| match b.location.as_deref() {
                Some("PRODUCTION_SHELF") | Some("INSPECTION_SHELF") => {
                    shelf_names.get(&hid).cloned()
                }
                Some("WORKER") => worker_names.get(&hid).cloned(),
                Some("OUTSOURCE_COMPANY") => outsource_names.get(&hid).cloned(),
                _ => None,
            });
        out.insert(part_id, (location, holder_name));
    }
    Ok(out)
}

/// 与 `crate::modules::part::statemachine::part_status_progress` 同逻辑的
/// 内联副本（避免在 service 层引一圈 statemachine 依赖）。PR-2 增列同步。
fn part_status_progress_inline(s: &str) -> u8 {
    match s {
        "PENDING" => 0,
        "PROGRAMMING" => 1,
        "IN_PROCESS" | "REPAIRING" => 2,
        "OUTSOURCE" => 3,
        "INSPECTION" => 4,
        "READY_TO_SHIP" => 5,
        "DELIVERED" => 6,
        _ => 2,
    }
}

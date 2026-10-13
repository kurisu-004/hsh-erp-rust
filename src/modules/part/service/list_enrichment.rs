//! part 列表派生层（2026-09-22 review 第 2 轮从 `crud.rs` 抽出）
//!
//! 本文件承载两族「一次性批量聚合」helper，都是列表行上的派生值、都以单条聚合
//! SQL 完成（防 N+1 往返），且被 `part::service::crud`（`GET /parts`）与
//! `com::union_list::service::crud`（`GET /api/v2/com/union-list` 四种 row_type
//! 模式）共同消费，故统一放 part 域、`pub(crate)` 供跨域 import：
//!
//! 1. 「位置 / 持有人」（2026-09-16 PR-2 瘦身后新增，见下）：按 min-progress
//!    活跃批次跨 `t_shelf` / `t_worker` / `t_outsource_company` 解析。
//! 2. 「已送数量」（2026-10-03 新增）：PART 行取已交批次 `quantity` 之和，
//!    ASSEMBLY 行取可凑齐的套数（min 公式）。
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
use crate::modules::prod::batch::repo::PartBatchRepo;
use crate::shared::batch::TPartBatch;
// 2026-10-13：原先本文件另存一份 `part_status_progress_inline`（与状态机
// 8 个 match 臂逐字同形），已删改为直调 `shared::batch::status_machine` 的那一份。
// 理由：原注释「避免在 service 层引一圈 statemachine 依赖」已失效 —— 同域的
// `service/lifecycle.rs` 早就在 import `PartStatus`（同一子模块树内）。
use crate::shared::batch::status_machine::part_status_progress;
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
///
/// 2026-09-29 升 `pub(crate)`：com::union_list 域 ALL 模式 PART 段 enrichment 复用。
/// 函数体零变化；只是把可见性从 part 域内公开到 crate 内，避免重复实现 4 条 SQL 的派生逻辑。
pub(crate) async fn enrich_part_list_with_location_and_holder<R: PartRepoTrait>(
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
        // min progress（口径见 `shared::batch::status_machine::part_status_progress`）。
        if let Some(min) = candidates
            .iter()
            .min_by_key(|b| part_status_progress(&b.status))
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

// ---------------------------------------------------------------------------
// 已交数量 / 已交套数两条批量聚合（2026-10-13 搬往 `shared::analytics::delivered`）
// ---------------------------------------------------------------------------
//
// 原先本文件持有这两个函数的**定义**，dashboard 域另有一份逐字副本（被它的域隔离
// 护栏挡着不能 import 本域）。落到 shared 之后副本消失、护栏强度不降
// （`assert_no_foreign_domain` 只扫 `src/modules/dashboard/**/*.rs`、认的是
// 「域根段 + 他域路径」前缀失配，`shared::analytics` 不属于任何域）。
//
// 口径与那 62 行边界处理 doc 一并搬走，见 `shared::analytics::delivered`。
// 本文件与 `com::union_list::service::crud` 现在都改从 shared 取。
pub(crate) use crate::shared::analytics::delivered::{
    fetch_delivered_quantities, fetch_delivered_sets,
};

//! part 列表派生层（2026-09-22 review 第 2 轮从 `crud.rs` 抽出）
//!
//! 本文件承载两族「一次性批量聚合」helper，都是列表行上的派生值、都以单条聚合
//! SQL 完成（防 N+1 往返），且被 `part::service::crud`（`GET /parts`）与
//! `com::union_list::service::crud`（`GET /api/v2/com/union-list` 三种 row_type
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

use sqlx::PgConnection;

use crate::modules::part::repo::PartRepoTrait;
use crate::modules::prod::batch::model::TPartBatch;
use crate::modules::prod::batch::repo::PartBatchRepo;
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

/// 一批 part 的「已送数量」：未软删批次中 `status ∈ ('DELIVERED', 'COMPLETED')`
/// 的 `quantity` 之和。
///
/// 真相源是 `t_part_batch.status`（批次级「已交」的唯一依据，**不**从派生缓存
/// `t_part.status` 反推 —— 后者在 min-progress 规则下只有全部活跃批次都 DELIVERED
/// 才等于 DELIVERED，会把「部分已交」一律压成 0）。
///
/// 空 ids → 返回空 HashMap（不发起 SQL）。caller 侧用 `.copied().unwrap_or(0)`
/// 兜底零批次行。
///
/// SQL 条数：1 条，与页大小 N 无关（防 N+1 往返）；但**扫描量是 O(本页零件的批次
/// 总数)**，走 `ix_t_part_batch_part_id` 索引探测。不改写成 CTE hash 聚合
/// —— 收益不可测、风险大于收益。
pub(crate) async fn fetch_delivered_quantities(
    conn: &mut PgConnection,
    part_ids: &[i64],
) -> Result<HashMap<i64, i32>, AppError> {
    let mut out: HashMap<i64, i32> = HashMap::new();
    if part_ids.is_empty() {
        return Ok(out);
    }
    // `::int` cast 不可省：PG 的 `SUM(int4)` 返回 int8，直接绑 i32 解码会报类型不匹配
    // （整页 500）。`COALESCE(..., 0)` 不可省：空集合 SUM 返 NULL。
    // status 字面量全大写 —— `t_part_batch.status` 是 varchar（非 DB enum）。
    let rows: Vec<(i64, i32)> = sqlx::query_as(
        "SELECT part_id, COALESCE(SUM(quantity), 0)::int \
         FROM t_part_batch \
         WHERE part_id = ANY($1) AND deleted_at IS NULL \
           AND status IN ('DELIVERED', 'COMPLETED') \
         GROUP BY part_id",
    )
    .bind(part_ids)
    .fetch_all(&mut *conn)
    .await?;
    for (part_id, qty) in rows {
        out.insert(part_id, qty);
    }
    Ok(out)
}

/// 一批装配件的「已送套数」：能凑齐几套。
///
/// 口径：每套需要的子零件数 = `子件总量 / 套数`（用户在「新建装配件」时指定装配件
/// 套数与每个子零件的总数）。故某子件能支撑的套数 =
/// `子件已送件数 × 装配件套数 / 子件总量`，父装配件的可交套数取所有子件的 **min**
/// （PG 整数除法截断，凑不满整套就按 0 记），再对装配件总套数 `a.quantity` 收口
/// （`LEAST`）—— 不可能交付超过工单总套数的套数。
///
/// 边界处理：
/// - `COALESCE(SUM(...), 0)` 不可省 —— 未交任何批次的子件若贡献 NULL 会被 `MIN` 忽略，
///   那样「子件 A 交一半、子件 B 一件没交」会误判成 A 能撑的套数；
/// - `NULLIF(子件总量, 0)` —— 总量为 0 的子件让该项为 NULL 从而被 `MIN` 忽略
///   （不参与），既不整除零出错也不拖累 min；
/// - `LEAST(COALESCE(MIN(...), 0), a.quantity)` —— 收口到工单总套数，同时兜住
///   子件超交（子件已送 > 子件总量时按比例会算出超过总套数的值，UI 会出现
///   「20 / 10 套」）。**`COALESCE` 必须在 `LEAST` 里面**：PG 的 `LEAST` 会忽略
///   NULL 实参（与 `MIN` 聚合同语义），写成 `COALESCE(LEAST(MIN(...), a.quantity), 0)`
///   会在「子件总量全为 0、`MIN` 为 NULL」时返回 `a.quantity`（整套全交），
///   与「子件全零 → 0 套」的口径正好相反；
/// - `LEAST` 顺带消除 int8→int4 收窄溢出：`子件已送 × 装配件套数` 是 int8 乘积
///   （子件总量为 1 时等于乘积本身），`1e6 × 1e6 = 1e12` 超 int4 会让整页 500；
///   收口后上界是 `a.quantity`（int4），`::int` 不再可能溢出。
///   PART 侧的 `fetch_delivered_quantities` 保持纯 `::int` 不加钳制：那边只是
///   `SUM(quantity)` 不放大，无真实溢出路径，加钳制反而会掩盖「已交量 > 总量」。
///
/// 前提：假设 `t_assembly.quantity` / `t_part.quantity` 恒非负。三列都是
/// `integer NOT NULL` 且**无 CHECK**（全仓唯一 quantity CHECK 在 outsource 域），
/// `AssemblyCreateRequest.quantity` 是 `Option<i32>` + `unwrap_or(1)`、create 路径无
/// `>0` 校验，但业务不会建负量工单；PG 整数除法对负数是**向零截断**（`-1 / 2 = 0`）
/// 会让套数偏大，而 `NULLIF` 只挡 0 不挡负。
///
/// 无子件的装配件不产生结果行（SQL 以子件表为驱动表），caller 侧
/// `.copied().unwrap_or(0)` 兜 0。
///
/// SQL 条数：1 条，与页大小 N 无关（防 N+1 往返）；但**扫描量是 O(本页装配件的子件
/// 总数)**，每个子件一次 `ix_t_part_batch_part_id` 索引探测。不改写成 CTE hash 聚合
/// —— 收益不可测、风险大于收益。
pub(crate) async fn fetch_delivered_sets(
    conn: &mut PgConnection,
    asm_ids: &[i64],
) -> Result<HashMap<i64, i32>, AppError> {
    let mut out: HashMap<i64, i32> = HashMap::new();
    if asm_ids.is_empty() {
        return Ok(out);
    }
    // 以 `t_part`（子件）为驱动表，走 `(assembly_id, ...)` 前缀索引，因此整段仍只 1 条
    // SQL。不写死索引名：`ix_t_part_assembly_id_status` 与 `ix_t_part_assembly_id` 同
    // 前缀，planner 可能选后者，钉死名字必过期。
    // `GROUP BY c.assembly_id, a.quantity`：套数是表达式的一部分，必须进 GROUP BY。
    // ⚠️ `c.id` / `c.quantity` **未**进 GROUP BY 却被 SELECT 表达式引用，靠的是
    // 「相关标量子查询的外层引用不受 grouping 检查」这一 PG 行为 —— 标准 SQL 应拒绝
    // （PG 的函数依赖放宽只在 GROUP BY 含表主键时生效，`c.assembly_id` 不是 `t_part`
    // 的主键）。**把相关子查询改写成 LEFT JOIN 或改用窗口函数会立刻报**
    // `column "c.id" must appear in the GROUP BY clause`，改写前务必先跑
    // `tests/com/union_list.rs` 的 `delivered_quantity_*` 用例。
    let rows: Vec<(i64, i32)> = sqlx::query_as(
        "SELECT c.assembly_id, \
                LEAST(COALESCE(MIN( \
                    (COALESCE((SELECT SUM(b.quantity) FROM t_part_batch b \
                               WHERE b.part_id = c.id AND b.deleted_at IS NULL \
                                 AND b.status IN ('DELIVERED', 'COMPLETED')), 0) \
                     * a.quantity) / NULLIF(c.quantity, 0) \
                ), 0), a.quantity)::int AS delivered_sets \
         FROM t_part c \
         JOIN t_assembly a ON a.id = c.assembly_id AND a.deleted_at IS NULL \
         WHERE c.assembly_id = ANY($1) AND c.deleted_at IS NULL \
         GROUP BY c.assembly_id, a.quantity",
    )
    .bind(asm_ids)
    .fetch_all(&mut *conn)
    .await?;
    for (asm_id, sets) in rows {
        out.insert(asm_id, sets);
    }
    Ok(out)
}

/// 与 `crate::modules::part::statemachine::part_status_progress` 同逻辑的
/// 内联副本（避免在 service 层引一圈 statemachine 依赖）。PR-2 增列同步。
///
/// 2026-10-01：删掉 `"IN_PROCESS" | "REPAIRING" => 2` 的合并臂，改回单值
/// `"IN_PROCESS" => 2`。REPAIRING 降级为 `t_part_batch.is_repairing` 标记列
/// 后，`t_part.status` 里不会再出现该字面量（migration 006 已把存量洗白，
/// status_gate 也永不写它）。
///
/// ⚠️ 存量兼容：migration 006 未 apply 的环境里仍可能读到 `'REPAIRING'`，此时
/// 落到 `_ => 2` 兜底臂 —— 恰好与原 REPAIRING 档位（2）相同，故本函数对
/// 两种数据形态**返回值一致**（与 `statemachine::part_status_progress` 的
/// 同款兜底同构）。
fn part_status_progress_inline(s: &str) -> u8 {
    match s {
        "PENDING" => 0,
        "PROGRAMMING" => 1,
        "IN_PROCESS" => 2,
        "OUTSOURCE" => 3,
        "INSPECTION" => 4,
        "READY_TO_SHIP" => 5,
        "DELIVERED" => 6,
        _ => 2,
    }
}

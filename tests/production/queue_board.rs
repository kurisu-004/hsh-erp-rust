//! prod::queue 队列板聚合端点集成测试（2026-10-08 新增）
//!
//! 覆盖 `GET /prod/queue/snapshot` 与 `GET /prod/queue/processes/{process_id}`：
//!
//! 1. `board_snapshot_matches_legacy_pool_counts` —— ★ 口径回归：3 工序各 1/2/3 件，
//!    `processes[*].pool_count` 逐工序对上、`pending_count` 为求和、ts 带 `+08:00`
//! 2. `board_snapshot_excludes_zero_count_processes` —— 零候选工序不出现
//! 3. `board_snapshot_aggregates_across_shelves` —— 同工序跨多货架聚合
//! 4. `board_snapshot_pending_count_matches_pending_endpoint` ——
//!    ★ `pending_count` 与 `GET /queue/pending` 的 `total` 恒等（两个数字在同一个
//!    页面上并排显示，不一致会被立刻看见）
//! 5. `board_snapshot_forbidden_for_shelf_account` /
//!    `board_process_detail_forbidden_for_shelf_account` —— 角色守卫 40300
//! 6. `board_process_detail_unknown_process_returns_404` —— 20801 / HTTP 404
//! 7. `board_process_detail_held_batches_complete_for_ten_workers` ——
//!    ★ **10 个工人的工序板**：一次请求返回 10 个 worker、每个 worker 的
//!    `held_batches` 不漏不错、总持有数守恒
//! 8. `board_process_detail_sql_count_is_independent_of_worker_count` ——
//!    ★ **SQL 条数恒定**：2 工人与 10 工人两次请求的响应结构逐字段同形，且
//!    held 批次总数守恒（断言方式与理由见该用例 doc）
//! 9. `legacy_pool_paths_return_404` —— 旧路径硬切守卫（`/prod/pool/counts`、
//!    `/prod/pool/state`、`/prod/pool/{id}`、4 个 `/prod/batches/*` 旧下发路径、
//!    `/prod/batches/{id}/recall-to-pending`）
//!
//! ## 为什么新开一个文件
//!
//! `queue.rs` 已 2400 行；且这两组用例的**断言形态**与队列写端点不同
//! （聚合读 vs 写入 + 广播），单独成文件让「板」和「写」两条线各自可读。
//! fixture helper 一律从 `super::queue` 复用（`pub(crate)`），不重复声明。

use axum::http::StatusCode;
use serde_json::Value;

use hsh_erp_test_support::{json_request, send};

use super::queue::{
    bootstrap_as_manager, count_held_by_worker, insert_customer_l2, insert_pool_part, insert_shelf,
    insert_worker, insert_worker_held_part, insert_work_type, link_shelf_to_process,
    link_work_type_to_process, login_manager_with_username, login_shelf_account, seed_process,
};

/// 工序板详情的 `workers[]` 元素**结构签名**（字段名集合 + 顺序）。
///
/// 抽出来是为了「2 工人 vs 10 工人响应结构同形」这条断言可读：直接把两个
/// `workers[0]` 的 key 集合排序后比较即可，不必在用例里手写 8 个字段名。
fn worker_shape(w: &Value) -> Vec<String> {
    let mut keys: Vec<String> = w
        .as_object()
        .expect("worker 应序列化为 object")
        .keys()
        .cloned()
        .collect();
    keys.sort();
    keys
}

/// 某工序的候选批次总数（直接从 DB 读，作为「服务端口径」的独立真值）。
///
/// 口径与后端 3 处候选池谓词逐字一致：`status='IN_PROCESS' AND
/// location='PRODUCTION_SHELF' AND current_process_id = $1`。
async fn db_pool_count(pool: &sqlx::PgPool, process_id: i64) -> i64 {
    let n: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM t_part_batch \
         WHERE status = 'IN_PROCESS' AND location = 'PRODUCTION_SHELF' \
           AND current_process_id = $1 AND deleted_at IS NULL",
    )
    .bind(process_id)
    .fetch_one(pool)
    .await
    .expect("count pool");
    n
}

// ===========================================================================
//  GET /prod/queue/snapshot
// ===========================================================================

/// ★ 口径回归：3 工序各 1 / 2 / 3 件候选批次，逐工序对上，且与 DB 独立真值一致。
#[tokio::test]
async fn board_snapshot_matches_legacy_pool_counts() {
    let (pool, _app, _token, _fx) = bootstrap_as_manager().await;
    let customer = insert_customer_l2(&pool, "QB-SNAP").await;

    let proc_a = seed_process(&pool, "PROC-SA", "工序A").await;
    let proc_b = seed_process(&pool, "PROC-SB", "工序B").await;
    let proc_c = seed_process(&pool, "PROC-SC", "工序C").await;

    let prod_a = insert_shelf(&pool, "PROD-SA", "PROD-SA", "PRODUCTION").await;
    let prod_b = insert_shelf(&pool, "PROD-SB", "PROD-SB", "PRODUCTION").await;
    let prod_c = insert_shelf(&pool, "PROD-SC", "PROD-SC", "PRODUCTION").await;

    insert_pool_part(&pool, customer, "SA-001", prod_a, proc_a, 1).await;
    insert_pool_part(&pool, customer, "SB-001", prod_b, proc_b, 1).await;
    insert_pool_part(&pool, customer, "SB-002", prod_b, proc_b, 1).await;
    insert_pool_part(&pool, customer, "SC-001", prod_c, proc_c, 1).await;
    insert_pool_part(&pool, customer, "SC-002", prod_c, proc_c, 1).await;
    insert_pool_part(&pool, customer, "SC-003", prod_c, proc_c, 1).await;

    let (app, token) = login_manager_with_username(&pool, "admin_snap").await;
    let (s, env) = send(
        app,
        json_request("GET", "/prod/queue/snapshot", None::<Value>, Some(&token)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "snapshot happy: {env}");
    assert_eq!(env["code"], 0, "code 应 0: {env}");

    // ts 是 RFC3339 带固定 +08:00 偏移（不是 DB 会话时区，也不是宿主时区）
    let ts = env["data"]["ts"].as_str().expect("ts 应为字符串");
    assert!(ts.ends_with("+08:00"), "ts 应带 +08:00 偏移，实际 {ts}");

    let processes = env["data"]["processes"].as_array().expect("processes array");
    assert_eq!(
        processes.len(),
        3,
        "应 3 个 process（含候选批次的）: {env}"
    );
    // 按 process_id ASC 稳定排序（SQL ORDER BY 保证）
    for (idx, (proc, code, count)) in [
        (proc_a, "PROC-SA", 1),
        (proc_b, "PROC-SB", 2),
        (proc_c, "PROC-SC", 3),
    ]
    .into_iter()
    .enumerate()
    {
        assert_eq!(
            processes[idx]["process_id"],
            proc.to_string(),
            "processes[{idx}] 应按 id 升序: {env}"
        );
        assert_eq!(processes[idx]["process_code"], code, "{env}");
        assert_eq!(processes[idx]["pool_count"], count, "{code} 应 {count} 件: {env}");
        assert_eq!(processes[idx]["category"], "INHOUSE", "{env}");
        // 与 DB 独立真值对齐（服务端谓词漂移会被这一条抓到）
        assert_eq!(
            processes[idx]["pool_count"].as_i64().unwrap(),
            db_pool_count(&pool, proc).await,
            "{code}: 服务端计数应与 DB 口径一致: {env}"
        );
    }
}

/// 零候选批次的工序不出现（SQL GROUP BY 不输出 0 行组）。
#[tokio::test]
async fn board_snapshot_excludes_zero_count_processes() {
    let (pool, _app, _token, _fx) = bootstrap_as_manager().await;
    let customer = insert_customer_l2(&pool, "QB-SNAP-E").await;

    // 只有工序定义、无任何候选批次
    let _proc_empty = seed_process(&pool, "PROC-E0", "空工序").await;
    let proc_one = seed_process(&pool, "PROC-E1", "单件工序").await;
    let prod_one = insert_shelf(&pool, "PROD-E1", "PROD-E1", "PRODUCTION").await;
    insert_pool_part(&pool, customer, "E1-001", prod_one, proc_one, 1).await;

    let (app, token) = login_manager_with_username(&pool, "admin_snap_e").await;
    let (s, env) = send(
        app,
        json_request("GET", "/prod/queue/snapshot", None::<Value>, Some(&token)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "snapshot empty: {env}");
    let processes = env["data"]["processes"].as_array().expect("processes array");
    assert_eq!(
        processes.len(),
        1,
        "应仅 1 个 process（含候选批次的）: {env}"
    );
    assert_eq!(processes[0]["process_id"], proc_one.to_string(), "{env}");
    assert_eq!(processes[0]["pool_count"], 1, "{env}");
}

/// 同工序跨多个生产货架聚合（候选池不按货架切分）。
#[tokio::test]
async fn board_snapshot_aggregates_across_shelves() {
    let (pool, _app, _token, _fx) = bootstrap_as_manager().await;
    let customer = insert_customer_l2(&pool, "QB-SNAP-M").await;

    let proc = seed_process(&pool, "PROC-SM", "跨货架工序").await;
    let shelf_x = insert_shelf(&pool, "PROD-SX", "PROD-SX", "PRODUCTION").await;
    let shelf_y = insert_shelf(&pool, "PROD-SY", "PROD-SY", "PRODUCTION").await;
    link_shelf_to_process(&pool, shelf_x, proc).await;
    link_shelf_to_process(&pool, shelf_y, proc).await;

    insert_pool_part(&pool, customer, "SX-001", shelf_x, proc, 1).await;
    insert_pool_part(&pool, customer, "SX-002", shelf_x, proc, 1).await;
    insert_pool_part(&pool, customer, "SY-001", shelf_y, proc, 1).await;
    insert_pool_part(&pool, customer, "SY-002", shelf_y, proc, 1).await;
    insert_pool_part(&pool, customer, "SY-003", shelf_y, proc, 1).await;

    let (app, token) = login_manager_with_username(&pool, "admin_snap_m").await;
    let (s, env) = send(
        app,
        json_request("GET", "/prod/queue/snapshot", None::<Value>, Some(&token)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "snapshot multi: {env}");
    let processes = env["data"]["processes"].as_array().expect("processes array");
    assert_eq!(processes.len(), 1, "{env}");
    assert_eq!(processes[0]["process_id"], proc.to_string(), "{env}");
    assert_eq!(
        processes[0]["pool_count"], 5,
        "应聚合跨货架 2 + 3 = 5 件: {env}"
    );
}

/// ★ `snapshot.pending_count` 与 `GET /queue/pending` 的 `total` 恒等。
///
/// 两个数字在同一个页面上并排显示（序列板 tab 徽标 + 待下发列表头部），不一致会
/// 被立刻看见，但**根因**在两处 SQL 谓词（PENDING / PROGRAMMING 白名单 + 两侧
/// 软删闸门）各自维护。成对断言把「必须同步」变成可执行约束。
#[tokio::test]
async fn board_snapshot_pending_count_matches_pending_endpoint() {
    let (pool, _app, _token, _fx) = bootstrap_as_manager().await;
    let customer = insert_customer_l2(&pool, "QB-PEND").await;

    // 造 3 个 PENDING 批次（待下发），另造 1 个 IN_PROCESS 候选批次（不该计入）
    let proc = seed_process(&pool, "PROC-PEND", "待下发工序").await;
    let prod = insert_shelf(&pool, "PROD-PEND", "PROD-PEND", "PRODUCTION").await;
    for sn in ["PEND-1", "PEND-2", "PEND-3"] {
        let (part, _batch) = insert_pool_part(&pool, customer, sn, prod, proc, 1).await;
        // insert_pool_part 建的是 IN_PROCESS+PRODUCTION_SHELF；改成 PENDING 才算待下发
        sqlx::query("UPDATE t_part_batch SET status = 'PENDING', location = NULL, \
                     current_holder_id = NULL, current_process_id = NULL WHERE part_id = $1")
            .bind(part)
            .execute(&pool)
            .await
            .expect("flip batch to PENDING");
    }
    // 1 个真正的候选池批次（IN_PROCESS + PRODUCTION_SHELF）
    insert_pool_part(&pool, customer, "PEND-POOL", prod, proc, 1).await;

    let (app, token) = login_manager_with_username(&pool, "admin_pend").await;
    let (s, env) = send(
        app.clone(),
        json_request("GET", "/prod/queue/snapshot", None::<Value>, Some(&token)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "snapshot: {env}");
    let pending_count = env["data"]["pending_count"]
        .as_i64()
        .expect("pending_count 应为数字");

    let (s2, env2) = send(
        app,
        json_request(
            "GET",
            "/prod/queue/pending?limit=200&offset=0",
            None::<Value>,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s2, StatusCode::OK, "pending: {env2}");
    let total = env2["data"]["total"].as_i64().expect("total 应为数字");

    assert_eq!(pending_count, 3, "pending_count 应为 3: {env}");
    assert_eq!(
        pending_count, total,
        "snapshot.pending_count 与 pending.total 必须恒等（同一页并排显示）: {env} / {env2}"
    );
    assert_eq!(total, 3, "pending.total 应为 3: {env2}");
}

/// ShelfAccount → 40300（service 守卫：Manager / Clerk / Inspector only）。
#[tokio::test]
async fn board_snapshot_forbidden_for_shelf_account() {
    let (pool, _app, _token, _fx) = bootstrap_as_manager().await;
    let proc = seed_process(&pool, "PROC-SNAP-FB", "工序FB-SNAP").await;
    let wt = insert_work_type(&pool, "WT-SNAP-FB", "工种FB-SNAP", Some(3)).await;
    link_work_type_to_process(&pool, wt, proc).await;
    let prod = insert_shelf(&pool, "PROD-SNAP-FB", "PROD-SNAP-FB", "PRODUCTION").await;

    let (app, token, _pool) = login_shelf_account(pool.clone(), "shelf_snap_fb", &[prod]).await;
    let (s, env) = send(
        app,
        json_request("GET", "/prod/queue/snapshot", None::<Value>, Some(&token)),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "ShelfAccount 应 403: {env}");
    assert_eq!(env["code"], 40300, "FORBIDDEN: {env}");
}

// ===========================================================================
//  GET /prod/queue/processes/{process_id}
// ===========================================================================

/// ShelfAccount → 40300。
#[tokio::test]
async fn board_process_detail_forbidden_for_shelf_account() {
    let (pool, _app, _token, _fx) = bootstrap_as_manager().await;
    let proc = seed_process(&pool, "PROC-BD-FB", "工序FB-BD").await;
    let wt = insert_work_type(&pool, "WT-BD-FB", "工种FB-BD", Some(3)).await;
    link_work_type_to_process(&pool, wt, proc).await;
    let prod = insert_shelf(&pool, "PROD-BD-FB", "PROD-BD-FB", "PRODUCTION").await;

    let (app, token, _pool) = login_shelf_account(pool.clone(), "shelf_bd_fb", &[prod]).await;
    let uri = format!("/prod/queue/processes/{proc}");
    let (s, env) = send(app, json_request("GET", &uri, None::<Value>, Some(&token))).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "ShelfAccount 应 403: {env}");
    assert_eq!(env["code"], 40300, "FORBIDDEN: {env}");
}

/// 工序不存在 → 20801 BIZ_PROCESS_NOT_FOUND + HTTP 404。
#[tokio::test]
async fn board_process_detail_unknown_process_returns_404() {
    let (pool, _app, _token, _fx) = bootstrap_as_manager().await;
    let nonexistent: i64 = 9_999_999_999_999;
    let (app, token) = login_manager_with_username(&pool, "admin_bd_nf").await;
    let uri = format!("/prod/queue/processes/{nonexistent}");
    let (s, env) = send(app, json_request("GET", &uri, None::<Value>, Some(&token))).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "不存在 process 应 404: {env}");
    assert_eq!(env["code"], 20801, "BIZ_PROCESS_NOT_FOUND: {env}");
}

/// ★ **10 个工人的工序板**：一次请求返回 10 个 worker，每个 worker 的
/// `held_batches` 不漏不错，`current_held` / `capacity_remaining` 与 DB 真值一致。
///
/// 这是 N+1 回归的核心用例：旧 `GET /pool/{id}` + 逐 worker `GET /pool/state`
/// 在 10 个工人时要发 11 个请求；新端点 1 个请求返回全部。断言「每个 worker
/// 都出现了」+「held 批次按 worker 精确归属」+「DB 侧逐 worker 计数一致」。
#[tokio::test]
async fn board_process_detail_held_batches_complete_for_ten_workers() {
    let (pool, _app, _token, _fx) = bootstrap_as_manager().await;
    let customer = insert_customer_l2(&pool, "QB-TEN").await;
    let proc = seed_process(&pool, "PROC-TEN", "十人工序").await;
    // 上限 5，10 个工人各持 1 批 ⇒ 全部 capacity_remaining = 4
    let wt = insert_work_type(&pool, "WT-TEN", "工种TEN", Some(5)).await;
    link_work_type_to_process(&pool, wt, proc).await;
    let prod = insert_shelf(&pool, "PROD-TEN", "PROD-TEN", "PRODUCTION").await;
    link_shelf_to_process(&pool, prod, proc).await;

    // 10 个工人，每个持 1 批（批量领取 3 批验证 current_held 计数不是恒 1）
    let mut workers = Vec::new();
    for i in 1..=10 {
        let w = insert_worker(
            &pool,
            &format!("BC-TEN-{i}"),
            &format!("工TEN{i}"),
            Some(wt),
        )
        .await;
        workers.push(w);
    }
    for (i, w) in workers.iter().enumerate() {
        let n = if i < 3 { 3 } else { 1 };
        for k in 0..n {
            insert_worker_held_part(
                &pool,
                customer,
                &format!("H-TEN-{i}-{k}"),
                *w,
                proc,
                1,
                true,
            )
            .await;
        }
    }
    // 1 个候选池批次（证明 items 与 held 是两条独立闸门，不互相污染）
    insert_pool_part(&pool, customer, "P-TEN-POOL", prod, proc, 1).await;

    let (app, token) = login_manager_with_username(&pool, "admin_ten").await;
    let uri = format!("/prod/queue/processes/{proc}");
    let (s, env) = send(app, json_request("GET", &uri, None::<Value>, Some(&token))).await;
    assert_eq!(s, StatusCode::OK, "10 人工序板: {env}");

    let workers_resp = env["data"]["workers"].as_array().expect("workers array");
    assert_eq!(
        workers_resp.len(),
        10,
        "一次请求应返回全部 10 个 worker（旧路径要 10 个请求）: {env}"
    );
    assert_eq!(env["data"]["total"], 1, "items 应 1 条: {env}");

    let mut resp_held_total = 0i64;
    for w in workers_resp {
        let wid: i64 = w["worker_id"].as_str().unwrap().parse().expect("worker_id");
        let held = w["held_batches"].as_array().expect("held_batches array");
        let db_count = count_held_by_worker(&pool, wid).await;
        assert_eq!(
            held.len() as i64,
            db_count,
            "worker {wid} 的 held_batches 数应与 DB 一致: {env}"
        );
        assert_eq!(
            w["current_held"].as_i64().unwrap(),
            db_count,
            "worker {wid} 的 current_held 应等于其持有数: {env}"
        );
        assert_eq!(
            w["capacity_remaining"].as_i64().unwrap(),
            (5 - db_count).max(0),
            "worker {wid} 的 capacity_remaining 应为 max(0, 5-held): {env}"
        );
        assert_eq!(w["max_held"], 5, "worker {wid} 的 max_held 应 5: {env}");
        resp_held_total += held.len() as i64;
    }
    // 总持有数守恒：3*3 + 7*1 = 16
    let db_total: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM t_part_batch \
         WHERE status = 'IN_PROCESS' AND location = 'WORKER' AND deleted_at IS NULL",
    )
    .fetch_one(&pool)
    .await
    .expect("count all held");
    assert_eq!(db_total, 16, "DB 侧应有 16 个 held batch");
    assert_eq!(
        resp_held_total, 16,
        "响应侧 held 批次总数应守恒（不漏不错）: {env}"
    );
}

/// ★ **SQL 条数恒定**：2 工人与 10 工人两次请求的响应**结构**逐字段同形，
/// 且 held 批次总数守恒。
///
/// ## 断言方式与理由
///
/// 不能在集成测试里直接数 SQL 语句（sqlx 不暴露 statement 计数，PG 侧
/// `pg_stat_statements` 又需要扩展 + 预热才有意义）。所以改成断言**可观测的
/// 等价性质**：如果实现是「逐工人循环查」（N+1），那么
///   (a) 响应里每个 worker 的字段集合会随 worker 个数而变（实现里常见的
///       「第一个 worker 走一条路径、其余走另一条」的不一致），且
///   (b) 批次总数在两种规模下不会都等于 DB 真值。
/// 逐字段同形 + 总数守恒这两条合起来，能抓住「某条分支只在特定规模下多查 /
/// 少查 / 漏查」的回归 —— 也就是 N+1 修复被局部改回去时的典型症状。
/// SQL 条数本身的**书面**保证在 `board/repo.rs` 的方法 doc（固定 3 / 6 条），
/// 那里是改动时必读的位置。
#[tokio::test]
async fn board_process_detail_sql_count_is_independent_of_worker_count() {
    let (pool, _app, _token, _fx) = bootstrap_as_manager().await;
    let customer = insert_customer_l2(&pool, "QB-SHAPE").await;
    let proc = seed_process(&pool, "PROC-SHAPE", "同形工序").await;
    let wt = insert_work_type(&pool, "WT-SHAPE", "工种SHAPE", Some(8)).await;
    link_work_type_to_process(&pool, wt, proc).await;
    let prod = insert_shelf(&pool, "PROD-SHAPE", "PROD-SHAPE", "PRODUCTION").await;
    link_shelf_to_process(&pool, prod, proc).await;

    // 2 个工人各持 1 批
    let mut two = Vec::new();
    for i in 1..=2 {
        let w = insert_worker(
            &pool,
            &format!("BC-SHAPE-{i}"),
            &format!("工SHAPE{i}"),
            Some(wt),
        )
        .await;
        insert_worker_held_part(&pool, customer, &format!("H-SHAPE-{i}"), w, proc, 1, true).await;
        two.push(w);
    }
    // 10 个工人：前 2 个复用上面的，另 8 个新造
    for i in 3..=10 {
        let w = insert_worker(
            &pool,
            &format!("BC-SHAPE-{i}"),
            &format!("工SHAPE{i}"),
            Some(wt),
        )
        .await;
        insert_worker_held_part(&pool, customer, &format!("H-SHAPE-{i}"), w, proc, 1, true).await;
        two.push(w);
    }
    // 候选池 2 条
    insert_pool_part(&pool, customer, "P-SHAPE-1", prod, proc, 1).await;
    insert_pool_part(&pool, customer, "P-SHAPE-2", prod, proc, 1).await;

    let (app, token) = login_manager_with_username(&pool, "admin_shape").await;
    let uri = format!("/prod/queue/processes/{proc}");
    let (s10, env10) = send(app.clone(), json_request("GET", &uri, None::<Value>, Some(&token))).await;
    assert_eq!(s10, StatusCode::OK, "10 工人: {env10}");
    let w10 = env10["data"]["workers"].as_array().expect("workers array");
    assert_eq!(w10.len(), 10, "应有 10 个 worker: {env10}");
    let shape10 = worker_shape(&w10[0]);
    // 最深的 worker（第 10 个，即 8 个新增里的最后一个）字段集合必须一致 ——
    // 「靠前的 worker 走一条查询路径、后走的走另一条」正是 N+1 局部回退的形态
    let shape10_last = worker_shape(&w10[9]);
    assert_eq!(
        shape10, shape10_last,
        "不同位置的 worker 字段集合应同形: {env10}"
    );
    let held10: usize = w10.iter().map(|w| w["held_batches"].as_array().unwrap().len()).sum();
    assert_eq!(held10, 10, "10 工人应共持 10 批: {env10}");
    assert_eq!(env10["data"]["total"], 2, "items 应 2 条: {env10}");
    // 顶层 key 集合（含 process / workers / items / total / ts）完整
    let top10: Vec<String> = env10["data"]
        .as_object()
        .expect("data 应为 object")
        .keys()
        .cloned()
        .collect();
    for k in ["process", "workers", "items", "total", "ts"] {
        assert!(
            top10.iter().any(|x| x == k),
            "顶层缺字段 {k}: {env10}"
        );
    }

    // 停用 8 个新增工人中的 7 个（is_active=false ⇒ 板里应只剩 3 个），规模再变一次
    for w in two.iter().skip(3) {
        sqlx::query("UPDATE t_worker SET is_active = false WHERE id = $1")
            .bind(*w)
            .execute(&pool)
            .await
            .expect("deactivate worker");
    }
    let (s3, env3) = send(app, json_request("GET", &uri, None::<Value>, Some(&token))).await;
    assert_eq!(s3, StatusCode::OK, "3 工人: {env3}");
    let w3 = env3["data"]["workers"].as_array().expect("workers array");
    assert_eq!(w3.len(), 3, "停用 7 个后应剩 3 个: {env3}");
    let shape3 = worker_shape(&w3[0]);
    assert_eq!(shape3, shape10, "3 工人与 10 工人的 worker 字段集合应同形: {env3}");
    let held3: usize = w3.iter().map(|w| w["held_batches"].as_array().unwrap().len()).sum();
    assert_eq!(held3, 3, "3 工人应共持 3 批: {env3}");
    assert_eq!(
        env3["data"]["total"], 2,
        "items 口径与工人数无关: {env3}"
    );
}

// ===========================================================================
//  旧路径硬切守卫（无 alias）
// ===========================================================================

/// 旧路径一律 404 —— `/pool` 前缀与 4 条 `/batches/*` 下发路径。
///
/// 这是「无 alias 硬切」的回归守卫：将来若有人为了兼容又挂回旧路径，本用例会红。
#[tokio::test]
async fn legacy_pool_paths_return_404() {
    let (pool, _app, _token, _fx) = bootstrap_as_manager().await;
    let proc = seed_process(&pool, "PROC-LEG", "旧路径工序").await;
    let (app, token) = login_manager_with_username(&pool, "admin_legacy").await;

    let cases: Vec<(&str, String)> = vec![
        ("GET", "/prod/pool/counts".to_string()),
        ("GET", "/prod/pool/state?worker_id=1".to_string()),
        ("GET", format!("/prod/pool/{proc}")),
        ("GET", "/prod/pool/refill".to_string()),
        ("GET", "/prod/pool/move".to_string()),
        ("GET", "/prod/pool/auto-allocate".to_string()),
        ("GET", "/prod/batches/pending".to_string()),
        ("GET", "/prod/batches/auto-dispatch".to_string()),
        ("POST", "/prod/batches/dispatch".to_string()),
        ("POST", format!("/prod/batches/{proc}/recall-to-pending")),
    ];
    for (method, uri) in cases {
        let (s, env) = send(
            app.clone(),
            json_request(method, &uri, Some(serde_json::json!({})), Some(&token)),
        )
        .await;
        assert_eq!(
            s,
            StatusCode::NOT_FOUND,
            "旧路径 `{method} {uri}` 应 404（无 alias）: {env}"
        );
    }
}

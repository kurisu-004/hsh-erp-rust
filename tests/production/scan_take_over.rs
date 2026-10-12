//! `prod::scan` 扫码接管端点（`POST /prod/scan/take-over`）集成测试
//!
//! 2026-10-15 新增（第 7 条端点，S2）。覆盖：
//! - happy path：holder worker → worker + 工序 / step 成对推进（**派生**与
//!   **显式指定**两条分支）、`version + 1`、`status` 仍 `IN_PROCESS`、数量不变；
//! - 判据顺序与错误码全矩阵（`20201` / `20202` / `20206` → `20109` / `20101` →
//!   `40901` → `20103` ×2 → `20104` ×2 → `40001` → `20904` / `20204` → `40300`）；
//! - 失败路径**零写入**（holder / version 不变）；
//! - `t_part_event` 的 `TRANSFERRED` 事件逐字段（含两种 note 形态）；
//! - **关键回归** `is_repairing` 保持 true（防误用无条件清标记的漏斗）；
//! - 2026-10-16 新语义（目标工序改由工种映射推导）：
//!   **单候选自动推导成功** / **多候选缺省 `40001`** / **零候选 `20104`** /
//!   **显式传链外工序成功且 step 落 NULL**（最后这条是
//!   `take_over_explicit_process_not_in_chain_returns_20702` 的反转）。
//!
//! ## fixture 复用
//! 全部从 `super::queue` 借（`bootstrap_as_manager` / `insert_work_type` /
//! `link_work_type_to_process` / `insert_worker` / `insert_worker_held_part` /
//! `insert_customer_l2` / `append_chain_step` / `seed_process` 等），不在本文件
//! 复制同款 fixture 避免漂移。本文件独享的 raw SQL 构造（改批次状态 / 读批次行）
//! 按仓库惯例保留为本地 `async fn`。

use axum::http::StatusCode;
use serde_json::{Value, json};
use sqlx::PgPool;

use hsh_erp_test_support::fixture::PartFixture;
use hsh_erp_test_support::{ProductionFixture, json_request, login_token, send};

use super::queue::{
    append_chain_step, bootstrap_as_manager, insert_customer_l2, insert_work_type, insert_worker,
    insert_worker_held_part, link_work_type_to_process, part_chain_id, seed_process,
};

/// take-over 路径（测试 app 不带 `/api/v2`）。
const TAKE_OVER_URI: &str = "/prod/scan/take-over";

/// 一个「不存在的批次」id（fixture 区段之外，与任何雪花都物理不相交）。
const MISSING_BATCH_ID: i64 = 9_000_000_000_000_099_999;

// ===========================================================================
//  本 sub-file 独享 helper
// ===========================================================================

/// 打一次 take-over。
async fn take_over(app: &axum::Router, token: &str, body: Value) -> (StatusCode, Value) {
    send(
        app.clone(),
        json_request("POST", TAKE_OVER_URI, Some(body), Some(token)),
    )
    .await
}

/// 读批次的 8 个关键列：`(status, location, holder, process, step, version, quantity, is_repairing)`。
#[allow(clippy::type_complexity)]
async fn read_batch(
    pool: &PgPool,
    batch_id: i64,
) -> (
    String,
    Option<String>,
    Option<i64>,
    Option<i64>,
    Option<i64>,
    i32,
    i32,
    bool,
) {
    sqlx::query_as(
        "SELECT status, location, current_holder_id, current_process_id, \
         current_process_step_id, version, quantity, is_repairing \
         FROM t_part_batch WHERE id = $1",
    )
    .bind(batch_id)
    .fetch_one(pool)
    .await
    .expect("read t_part_batch")
}

/// 直改批次 status / location（构造 20103 的两种形态）。
async fn force_batch_state(pool: &PgPool, batch_id: i64, status: &str, location: &str) {
    sqlx::query("UPDATE t_part_batch SET status = $2, location = $3 WHERE id = $1")
        .bind(batch_id)
        .bind(status)
        .bind(location)
        .execute(pool)
        .await
        .expect("force batch state");
}

/// 清掉 step 指针（构造「非顺应工序」形态：指针 NULL ⇒ `resolve_chain_position`
/// 落 `NONE`，显式 `next_process_id` 必填）。
async fn clear_step_pointer(pool: &PgPool, batch_id: i64) {
    sqlx::query("UPDATE t_part_batch SET current_process_step_id = NULL WHERE id = $1")
        .bind(batch_id)
        .execute(pool)
        .await
        .expect("clear step pointer");
}

/// 建一个「链 [proc1, proc2]、指针一致」的持有件 + 一个能接管的工人，返回
/// `(part_id, batch_id, holder_id, taker_id, proc2, step2)`。
///
/// `taker_maps_next`：接管人工种是否映射**下一道**（proc2）。`true` = 映射 proc2
/// （happy path / 容量用例）；`false` = 只映射 proc1（构造「推导出的下一道 ∉
/// 工种映射」的 `20104` 用例）。
async fn setup_derivable(
    pool: &PgPool,
    serial: &str,
    taker_maps_next: bool,
) -> (i64, i64, i64, i64, i64, i64) {
    let customer = insert_customer_l2(pool, &format!("{serial}-C")).await;
    let proc1 = seed_process(pool, &format!("{serial}-P1"), "接管工序1").await;
    let proc2 = seed_process(pool, &format!("{serial}-P2"), "接管工序2").await;
    let wt = insert_work_type(pool, &format!("{serial}-WT"), "接管工种", Some(5)).await;
    link_work_type_to_process(pool, wt, if taker_maps_next { proc2 } else { proc1 }).await;
    // 持有人不需要工种（接管侧只校验扫码人）；给 Some 避免误报 20206。
    let wt_holder = insert_work_type(pool, &format!("{serial}-WH"), "持有人工种", Some(5)).await;
    let holder = insert_worker(pool, &format!("{serial}-H"), "持有人", Some(wt_holder)).await;
    let taker = insert_worker(pool, &format!("{serial}-T"), "接管人", Some(wt)).await;
    let (part, batch, _step1) =
        insert_worker_held_part(pool, customer, serial, holder, proc1, 3, true).await;
    let chain = part_chain_id(pool, part).await.expect("part 应有链");
    let step2 = append_chain_step(pool, chain, proc2, 20).await;
    (part, batch, holder, taker, proc2, step2)
}

// ===========================================================================
//  happy path
// ===========================================================================

/// 显式指定 `next_process_id`（指针漂移形态）：holder 换成扫码人、工序 / step
/// 成对推进、`version + 1`、`status` 仍 `IN_PROCESS`、`quantity` 不变。
#[tokio::test]
async fn take_over_moves_batch_to_badge_worker_and_advances_process() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let (part, batch, holder, taker, proc2, step2) =
        setup_derivable(&pool, "TO-EXPLICIT", true).await;
    // 指针漂移 ⇒ 推导不出 NEXT，必须显式指定（本用例走显式分支）。
    clear_step_pointer(&pool, batch).await;

    let (s, env) = take_over(
        &app,
        &token,
        json!({
            "batch_id": batch.to_string(),
            "badge_code": "TO-EXPLICIT-T",
            "version": 0,
            "next_process_id": proc2.to_string(),
        }),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "显式接管应 200: {env}");
    assert_eq!(env["code"], 0);
    assert_eq!(env["data"]["batch_id"], batch.to_string());
    assert_eq!(env["data"]["part_id"], part.to_string());
    assert_eq!(env["data"]["from_worker_id"], holder.to_string());
    assert_eq!(env["data"]["to_worker_id"], taker.to_string());
    assert_eq!(env["data"]["next_process_id"], proc2.to_string());
    assert_eq!(env["data"]["next_process_step_id"], step2.to_string());
    assert_eq!(
        env["data"]["version"], 1,
        "接管是 OCC 写，version 必须 +1: {env}"
    );

    let (status, location, h, process, step, version, quantity, _repairing) =
        read_batch(&pool, batch).await;
    assert_eq!(status, "IN_PROCESS", "接管不改 status");
    assert_eq!(location.as_deref(), Some("WORKER"));
    assert_eq!(h, Some(taker), "holder 必须换成扫码人");
    assert_eq!(
        process,
        Some(proc2),
        "current_process_id 必须推进到目标工序"
    );
    assert_eq!(step, Some(step2), "step 指针必须与工序成对推进");
    assert_eq!(version, 1, "OCC 应 +1");
    assert_eq!(quantity, 3, "接管是整批动作，数量不变");
}

/// 指针一致 + 链上有下一道 ⇒ **不传** `next_process_id` 也能成功（后端按链推导）。
#[tokio::test]
async fn take_over_derives_next_process_when_pointer_consistent() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let (part, batch, holder, taker, proc2, step2) =
        setup_derivable(&pool, "TO-DERIVE", true).await;
    // fixture 的指针已指向 proc1 的 step、当前工序也是 proc1 ⇒ 指针一致 + NEXT。
    let _ = &part;

    let (s, env) = take_over(
        &app,
        &token,
        json!({
            "batch_id": batch.to_string(),
            "badge_code": "TO-DERIVE-T",
            "version": 0,
        }),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "顺应工序免填 next 应 200: {env}");
    assert_eq!(env["code"], 0);
    assert_eq!(
        env["data"]["from_worker_id"],
        holder.to_string(),
        "旧持有人从批次行读出: {env}"
    );
    assert_eq!(env["data"]["to_worker_id"], taker.to_string());
    assert_eq!(env["data"]["next_process_id"], proc2.to_string());
    assert_eq!(env["data"]["next_process_step_id"], step2.to_string());
    assert_eq!(env["data"]["version"], 1);

    let (_status, _loc, h, process, step, version, _q, _r) = read_batch(&pool, batch).await;
    assert_eq!(h, Some(taker));
    assert_eq!(process, Some(proc2), "推导出的下一道应落库");
    assert_eq!(step, Some(step2));
    assert_eq!(version, 1);
}

// ===========================================================================
//  判据顺序 / 错误码矩阵
// ===========================================================================

/// 链不可推导 + **工种有多道候选**且未传 `next_process_id` → `40001`（零写入）；
/// 补上显式 `next_process_id` 后成功，且**无链时 step 可为 NULL**。
///
/// ⚠️ 2026-10-16 调整 fixture：工种映射由 1 道改为 **2 道**。新语义下「链推不出且
/// 工种只有一道」会由服务端自动推导（成功），本用例要钉的是「**多候选必填**」那条
/// 分支 —— 改 fixture 而不是回退新语义。两条工序：`proc`（本用例显式传的那道）与
/// `proc_extra`（只用来把候选数抬到 2，它不在链上、也不参与本用例的断言）。
#[tokio::test]
async fn take_over_requires_next_process_id_when_not_consistent() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    // 手写工单形态：part 无链（`with_chain=false`）。
    let customer = insert_customer_l2(&pool, "TO-NOCHAIN").await;
    let proc = seed_process(&pool, "TO-NOCHAIN-P", "无链工序").await;
    let proc_extra = seed_process(&pool, "TO-NOCHAIN-PX", "候选工序2").await;
    let wt = insert_work_type(&pool, "TO-NOCHAIN-WT", "工种", Some(5)).await;
    link_work_type_to_process(&pool, wt, proc).await;
    link_work_type_to_process(&pool, wt, proc_extra).await;
    let wt_holder = insert_work_type(&pool, "TO-NOCHAIN-WH", "持有人工种", Some(5)).await;
    let holder = insert_worker(&pool, "TO-NOCHAIN-H", "持有人", Some(wt_holder)).await;
    let _taker = insert_worker(&pool, "TO-NOCHAIN-T", "接管人", Some(wt)).await;
    let (_part, batch, _step) =
        insert_worker_held_part(&pool, customer, "SN-TO-NOCHAIN", holder, proc, 2, false).await;
    // 指针漂移（NULL）⇒ 非顺应 ⇒ 工种候选决定目标工序。
    clear_step_pointer(&pool, batch).await;

    let (s, env) = take_over(
        &app,
        &token,
        json!({
            "batch_id": batch.to_string(),
            "badge_code": "TO-NOCHAIN-T",
            "version": 0,
        }),
    )
    .await;
    // 40001 VALIDATION_ERROR 的 HTTP 状态是 422（`status_from_code` 显式映射，不是 400）。
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "缺 next 应 422: {env}");
    assert_eq!(env["code"], 40001, "VALIDATION_ERROR: {env}");
    let (_st, _loc, h, process, step, version, _q, _r) = read_batch(&pool, batch).await;
    assert_eq!(h, Some(holder), "40001 必须零写入");
    assert_eq!(process, Some(proc));
    assert_eq!(step, None);
    assert_eq!(version, 0, "40001 不得改 version");

    // 带显式 next 后成功；无链 ⇒ step 列保持 NULL。
    let (s, env) = take_over(
        &app,
        &token,
        json!({
            "batch_id": batch.to_string(),
            "badge_code": "TO-NOCHAIN-T",
            "version": 0,
            "next_process_id": proc.to_string(),
        }),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "显式指定后应成功: {env}");
    assert_eq!(env["data"]["next_process_step_id"], Value::Null);
    let (_st, _loc, _h, process, step, version, _q, _r) = read_batch(&pool, batch).await;
    assert_eq!(process, Some(proc));
    assert_eq!(step, None, "无链时 step 可空（该列允许 NULL）");
    assert_eq!(version, 1);
}

/// 显式指定的工序**在该工单的链之外**（但在接管人工种的活跃映射里）→ **成功**，
/// 且 `current_process_step_id` 落 `NULL`（2026-10-16 新语义）。
///
/// ⚠️ 本用例在 2026-10-16 **反转**：此前它钉的是「显式传链外工序 ⇒ `20702`」，而
/// 产品决策是「跨工序接管是独立路径，不因越链而拒」—— 非同工序内接管时链可能整体
/// 不适用（无链 / 链已软删 / 链外工序），`current_process_step_id` 是可空列，链上
/// 没有落点就落 NULL。写侧 `service/transfer.rs` 第 6 步的分支 B / C 因此不再调
/// `optional_step_id`（那函数越链就抛 `20702`），改用
/// `resolve_step_id_by_process`（返 `Option` 不抛）。
///
/// 所选工序必须在 `t_work_type_process` 里（否则先吃 `20104`，钉不到本用例的语义）
/// 且不在该 part 的 `t_process_chain_step` 里（链只有 proc1 / proc2）。
#[tokio::test]
async fn take_over_explicit_process_outside_chain_lands_null_step() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let customer = insert_customer_l2(&pool, "TO-NOTIN").await;
    let proc1 = seed_process(&pool, "TO-NOTIN-P1", "链内工序1").await;
    let proc2 = seed_process(&pool, "TO-NOTIN-P2", "链内工序2").await;
    let proc3 = seed_process(&pool, "TO-NOTIN-P3", "链外工序").await;
    let wt = insert_work_type(&pool, "TO-NOTIN-WT", "接管工种", Some(5)).await;
    // 工种映射 proc2（链内）+ proc3（链外）—— 两个候选，确保走到的是「显式分支」
    // 而不是「单候选自动推导」。
    link_work_type_to_process(&pool, wt, proc2).await;
    link_work_type_to_process(&pool, wt, proc3).await;
    let wt_holder = insert_work_type(&pool, "TO-NOTIN-WH", "持有人工种", Some(5)).await;
    let holder = insert_worker(&pool, "TO-NOTIN-H", "持有人", Some(wt_holder)).await;
    let taker = insert_worker(&pool, "TO-NOTIN-T", "接管人", Some(wt)).await;
    let (part, batch, _step1) =
        insert_worker_held_part(&pool, customer, "SN-TO-NOTIN", holder, proc1, 3, true).await;
    let chain = part_chain_id(&pool, part).await.expect("part 应有链");
    append_chain_step(&pool, chain, proc2, 20).await;
    clear_step_pointer(&pool, batch).await;
    assert!(
        super::queue::part_chain_id(&pool, part).await.is_some(),
        "前置：part 必须有链"
    );

    let (s, env) = take_over(
        &app,
        &token,
        json!({
            "batch_id": batch.to_string(),
            "badge_code": "TO-NOTIN-T",
            "version": 0,
            "next_process_id": proc3.to_string(),
        }),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "链外但属工种映射的工序应放行: {env}");
    assert_eq!(env["data"]["next_process_id"], proc3.to_string());
    assert_eq!(
        env["data"]["next_process_step_id"],
        Value::Null,
        "工序不在链内 ⇒ step 落 NULL（不是 20702）: {env}"
    );

    let (_st, _loc, h, process, step, version, _q, _r) = read_batch(&pool, batch).await;
    assert_eq!(h, Some(taker), "holder 必须换成接管人");
    assert_eq!(process, Some(proc3), "目标工序必须落库");
    assert_eq!(step, None, "链外工序没有链上落点");
    assert_eq!(version, 1, "OCC 应 +1");
}

/// 2026-10-16 新语义：链不可推导（无链）+ 工种**只映射一道**工序 ⇒ **不传**
/// `next_process_id` 也成功（服务端自动推导），落库工序 = 那一道、step 为 NULL。
#[tokio::test]
async fn take_over_auto_derives_single_work_type_process() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let customer = insert_customer_l2(&pool, "TO-AUTO1").await;
    let proc = seed_process(&pool, "TO-AUTO1-P", "唯一工序").await;
    let wt = insert_work_type(&pool, "TO-AUTO1-WT", "单工序工种", Some(5)).await;
    link_work_type_to_process(&pool, wt, proc).await;
    let wt_holder = insert_work_type(&pool, "TO-AUTO1-WH", "持有人工种", Some(5)).await;
    let holder = insert_worker(&pool, "TO-AUTO1-H", "持有人", Some(wt_holder)).await;
    let taker = insert_worker(&pool, "TO-AUTO1-T", "接管人", Some(wt)).await;
    let (_part, batch, _step) =
        insert_worker_held_part(&pool, customer, "SN-TO-AUTO1", holder, proc, 2, false).await;
    clear_step_pointer(&pool, batch).await;

    let (s, env) = take_over(
        &app,
        &token,
        json!({
            "batch_id": batch.to_string(),
            "badge_code": "TO-AUTO1-T",
            "version": 0,
        }),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::OK,
        "工种只有一道候选时不必让前端挑工序: {env}"
    );
    assert_eq!(env["data"]["next_process_id"], proc.to_string());
    assert_eq!(env["data"]["next_process_step_id"], Value::Null);
    assert_eq!(env["data"]["to_worker_id"], taker.to_string());

    let (_st, _loc, h, process, step, version, _q, _r) = read_batch(&pool, batch).await;
    assert_eq!(h, Some(taker));
    assert_eq!(process, Some(proc), "自动推导的工序必须落库");
    assert_eq!(step, None, "无链 ⇒ step 可空");
    assert_eq!(version, 1);
}

/// 2026-10-16 新语义：链不可推导 + 工种映射 **2 道**工序 + 未传 `next_process_id`
/// ⇒ `40001`（HTTP 422，零写入），文案说明「有 N 道工序」。
#[tokio::test]
async fn take_over_requires_explicit_when_multiple_candidates() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let customer = insert_customer_l2(&pool, "TO-MULTI").await;
    let proc1 = seed_process(&pool, "TO-MULTI-P1", "候选1").await;
    let proc2 = seed_process(&pool, "TO-MULTI-P2", "候选2").await;
    let wt = insert_work_type(&pool, "TO-MULTI-WT", "多工序工种", Some(5)).await;
    link_work_type_to_process(&pool, wt, proc1).await;
    link_work_type_to_process(&pool, wt, proc2).await;
    let wt_holder = insert_work_type(&pool, "TO-MULTI-WH", "持有人工种", Some(5)).await;
    let holder = insert_worker(&pool, "TO-MULTI-H", "持有人", Some(wt_holder)).await;
    let taker = insert_worker(&pool, "TO-MULTI-T", "接管人", Some(wt)).await;
    let (_part, batch, _step) =
        insert_worker_held_part(&pool, customer, "SN-TO-MULTI", holder, proc1, 2, false).await;
    clear_step_pointer(&pool, batch).await;

    let (s, env) = take_over(
        &app,
        &token,
        json!({
            "batch_id": batch.to_string(),
            "badge_code": "TO-MULTI-T",
            "version": 0,
        }),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "多候选必填: {env}");
    assert_eq!(env["code"], 40001, "VALIDATION_ERROR: {env}");
    let msg = env["message"].as_str().unwrap_or_default().to_string();
    assert!(
        msg.contains('2') && msg.contains("next_process_id"),
        "文案应说明「有 2 道工序」并指明字段: {env}"
    );
    let (_st, _loc, h, process, step, version, _q, _r) = read_batch(&pool, batch).await;
    assert_eq!(h, Some(holder), "40001 必须零写入");
    assert_ne!(h, Some(taker));
    assert_eq!(process, Some(proc1));
    assert_eq!(step, None);
    assert_eq!(version, 0, "40001 不得改 version");
}

/// 2026-10-16 新语义：链不可推导 + 工种**一条工序都没映射** ⇒ `20104`
/// （HTTP 400，零写入），文案点名工号。
#[tokio::test]
async fn take_over_empty_work_type_mapping_returns_20104() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let customer = insert_customer_l2(&pool, "TO-EMPTY").await;
    let proc = seed_process(&pool, "TO-EMPTY-P", "工序").await;
    // 接管人的工种**不 link** 任何工序 ⇒ 候选集为空。
    let wt = insert_work_type(&pool, "TO-EMPTY-WT", "空工种", Some(5)).await;
    let wt_holder = insert_work_type(&pool, "TO-EMPTY-WH", "持有人工种", Some(5)).await;
    let holder = insert_worker(&pool, "TO-EMPTY-H", "持有人", Some(wt_holder)).await;
    let taker = insert_worker(&pool, "TO-EMPTY-T", "接管人", Some(wt)).await;
    let (_part, batch, _step) =
        insert_worker_held_part(&pool, customer, "SN-TO-EMPTY", holder, proc, 2, false).await;
    clear_step_pointer(&pool, batch).await;

    let (s, env) = take_over(
        &app,
        &token,
        json!({
            "batch_id": batch.to_string(),
            "badge_code": "TO-EMPTY-T",
            "version": 0,
        }),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "零候选应 400: {env}");
    assert_eq!(env["code"], 20104, "BIZ_INVALID_VALUE: {env}");
    let msg = env["message"].as_str().unwrap_or_default().to_string();
    assert!(
        msg.contains(&taker.to_string()) && msg.contains("未映射任何工序"),
        "文案应点名工号并说明「工种未映射任何工序」: {env}"
    );
    let (_st, _loc, h, _process, _step, version, _q, _r) = read_batch(&pool, batch).await;
    assert_eq!(h, Some(holder), "20104 必须零写入");
    assert_eq!(version, 0);
}

/// 推导出的下一道 ∉ 接管人工种映射 → `20104`（零写入）。
#[tokio::test]
async fn take_over_next_process_not_in_work_type_returns_20104() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    // 接管人工种只映射 proc1；链推导出的是 proc2（不在映射里）。
    let (_part, batch, holder, _taker, _proc2, _step2) =
        setup_derivable(&pool, "TO-WT", false).await;

    let (s, env) = take_over(
        &app,
        &token,
        json!({
            "batch_id": batch.to_string(),
            "badge_code": "TO-WT-T",
            "version": 0,
        }),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "工种不含目标工序应 400: {env}");
    assert_eq!(env["code"], 20104, "BIZ_INVALID_VALUE: {env}");
    let (_st, _loc, h, _p, _step, version, _q, _r) = read_batch(&pool, batch).await;
    assert_eq!(h, Some(holder), "20104 必须零写入");
    assert_eq!(version, 0);
}

/// holder 就是扫码人自己 → `20104`「批次已在你手上，无需接管」（零写入）。
#[tokio::test]
async fn take_over_when_holder_is_me_returns_20104() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let customer = insert_customer_l2(&pool, "TO-MINE").await;
    let proc = seed_process(&pool, "TO-MINE-P", "工序").await;
    let wt = insert_work_type(&pool, "TO-MINE-WT", "工种", Some(5)).await;
    link_work_type_to_process(&pool, wt, proc).await;
    let me = insert_worker(&pool, "TO-MINE-ME", "本人", Some(wt)).await;
    let (_part, batch, _step) =
        insert_worker_held_part(&pool, customer, "SN-TO-MINE", me, proc, 1, true).await;

    let (s, env) = take_over(
        &app,
        &token,
        json!({
            "batch_id": batch.to_string(),
            "badge_code": "TO-MINE-ME",
            "version": 0,
        }),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{env}");
    assert_eq!(env["code"], 20104, "{env}");
    assert!(
        env["message"]
            .as_str()
            .unwrap_or_default()
            .contains("在你手上"),
        "文案应为「批次已在你手上，无需接管」: {env}"
    );
    let (_st, _loc, h, _p, _step, version, _q, _r) = read_batch(&pool, batch).await;
    assert_eq!(h, Some(me));
    assert_eq!(version, 0);
}

/// 批次状态不是 `IN_PROCESS`（如 `INSPECTION`）→ `20103`（零写入）。
#[tokio::test]
async fn take_over_wrong_status_returns_20103() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let (_part, batch, holder, _taker, _p2, _s2) = setup_derivable(&pool, "TO-ST", true).await;
    force_batch_state(&pool, batch, "INSPECTION", "WORKER").await;

    let (s, env) = take_over(
        &app,
        &token,
        json!({
            "batch_id": batch.to_string(),
            "badge_code": "TO-ST-T",
            "version": 0,
        }),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{env}");
    assert_eq!(env["code"], 20103, "BIZ_INVALID_TRANSITION: {env}");
    let (st, _loc, h, _p, _step, version, _q, _r) = read_batch(&pool, batch).await;
    assert_eq!(st, "INSPECTION", "20103 必须零写入");
    assert_eq!(h, Some(holder));
    assert_eq!(version, 0);
}

/// 批次位置不是 `WORKER`（如 `PRODUCTION_SHELF`）→ `20103`（零写入）。
#[tokio::test]
async fn take_over_wrong_location_returns_20103() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let (_part, batch, holder, _taker, _p2, _s2) = setup_derivable(&pool, "TO-LOC", true).await;
    force_batch_state(&pool, batch, "IN_PROCESS", "PRODUCTION_SHELF").await;

    let (s, env) = take_over(
        &app,
        &token,
        json!({
            "batch_id": batch.to_string(),
            "badge_code": "TO-LOC-T",
            "version": 0,
        }),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{env}");
    assert_eq!(env["code"], 20103, "BIZ_INVALID_TRANSITION: {env}");
    let (_st, loc, h, _p, _step, version, _q, _r) = read_batch(&pool, batch).await;
    assert_eq!(loc.as_deref(), Some("PRODUCTION_SHELF"));
    assert_eq!(h, Some(holder));
    assert_eq!(version, 0);
}

/// `IN_PROCESS + WORKER` 但 **holder 为 NULL**（数据异常）→ `20103`（零写入）。
///
/// 接管的前提是「批次挂在别人手上」；无人持有则无对象可接管 —— 写侧因此拒 `20103`
/// （与 S1 `locate` 的 `classify_batch`「holder 为 NULL 不给 TAKE_OVER」同一条订正）。
#[tokio::test]
async fn take_over_worker_holder_null_returns_20103() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let (_part, batch, _holder, _taker, _proc2, _step2) =
        setup_derivable(&pool, "TO-NOH", true).await;
    // 制造数据异常形态：位置还在 WORKER，持有人已被清空。
    sqlx::query("UPDATE t_part_batch SET current_holder_id = NULL WHERE id = $1")
        .bind(batch)
        .execute(&pool)
        .await
        .expect("clear holder");

    let (s, env) = take_over(
        &app,
        &token,
        json!({
            "batch_id": batch.to_string(),
            "badge_code": "TO-NOH-T",
            "version": 0,
        }),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{env}");
    assert_eq!(env["code"], 20103, "BIZ_INVALID_TRANSITION: {env}");
    assert!(
        env["message"]
            .as_str()
            .unwrap_or_default()
            .contains("没有持有人"),
        "文案应指向「数据异常：没有持有人」: {env}"
    );
    let (_st, _loc, h, _p, _step, version, _q, _r) = read_batch(&pool, batch).await;
    assert_eq!(h, None, "20103 必须零写入");
    assert_eq!(version, 0);
}

/// 工牌未注册 → `20201`（HTTP 404）。
#[tokio::test]
async fn take_over_badge_not_found_20201() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let (_part, batch, _holder, _taker, _p2, _s2) = setup_derivable(&pool, "TO-BADGE", true).await;

    let (s, env) = take_over(
        &app,
        &token,
        json!({
            "batch_id": batch.to_string(),
            "badge_code": "NO-SUCH-BADGE",
            "version": 0,
        }),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{env}");
    assert_eq!(env["code"], 20201, "{env}");
}

/// 工人已停用 → `20202`（HTTP 400）。
#[tokio::test]
async fn take_over_badge_inactive_20202() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let (_part, batch, _holder, taker, _p2, _s2) = setup_derivable(&pool, "TO-INACT", true).await;
    sqlx::query("UPDATE t_worker SET is_active = false WHERE id = $1")
        .bind(taker)
        .execute(&pool)
        .await
        .expect("deactivate taker");

    let (s, env) = take_over(
        &app,
        &token,
        json!({
            "batch_id": batch.to_string(),
            "badge_code": "TO-INACT-T",
            "version": 0,
        }),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{env}");
    assert_eq!(env["code"], 20202, "{env}");
}

/// 工人未分配工种 → `20206`。
#[tokio::test]
async fn take_over_worker_without_work_type_20206() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let customer = insert_customer_l2(&pool, "TO-NOWT").await;
    let proc = seed_process(&pool, "TO-NOWT-P", "工序").await;
    let wt_holder = insert_work_type(&pool, "TO-NOWT-WH", "持有人工种", Some(5)).await;
    let holder = insert_worker(&pool, "TO-NOWT-H", "持有人", Some(wt_holder)).await;
    // 扫码人：active 但 work_type_id = NULL
    let _taker = insert_worker(&pool, "TO-NOWT-T", "无工种", None).await;
    let (_part, batch, _step) =
        insert_worker_held_part(&pool, customer, "SN-TO-NOWT", holder, proc, 1, true).await;

    let (s, env) = take_over(
        &app,
        &token,
        json!({
            "batch_id": batch.to_string(),
            "badge_code": "TO-NOWT-T",
            "version": 0,
        }),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{env}");
    assert_eq!(env["code"], 20206, "{env}");
}

/// 批次不存在（或已软删）→ `20109`（HTTP 404）。
#[tokio::test]
async fn take_over_batch_not_found_20109() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let _customer = insert_customer_l2(&pool, "TO-NOBATCH").await;
    let _wt = insert_work_type(&pool, "TO-NOBATCH-WT", "工种", Some(5)).await;
    let _taker = insert_worker(&pool, "TO-NOBATCH-T", "接管人", Some(_wt)).await;

    let (s, env) = take_over(
        &app,
        &token,
        json!({
            "batch_id": MISSING_BATCH_ID.to_string(),
            "badge_code": "TO-NOBATCH-T",
            "version": 0,
        }),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{env}");
    assert_eq!(env["code"], 20109, "{env}");
}

/// 客户端 `version` 与批次不一致 → `40901`（零写入）。
#[tokio::test]
async fn take_over_version_conflict_40901() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let (_part, batch, holder, _taker, _p2, _s2) = setup_derivable(&pool, "TO-OCC", true).await;

    let (s, env) = take_over(
        &app,
        &token,
        json!({
            "batch_id": batch.to_string(),
            "badge_code": "TO-OCC-T",
            "version": 7,
        }),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "OCC 冲突应 409: {env}");
    assert_eq!(env["code"], 40901, "VERSION_CONFLICT: {env}");
    let (_st, _loc, h, _p, _step, version, _q, _r) = read_batch(&pool, batch).await;
    assert_eq!(h, Some(holder), "40901 必须零写入");
    assert_eq!(version, 0);
}

/// 接管人已持有 `max_held_batches` 批 → `20204`（HTTP 409，零写入）。
#[tokio::test]
async fn take_over_when_worker_full_20204() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let customer = insert_customer_l2(&pool, "TO-FULL").await;
    let proc1 = seed_process(&pool, "TO-FULL-P1", "工序1").await;
    let proc2 = seed_process(&pool, "TO-FULL-P2", "工序2").await;
    let wt = insert_work_type(&pool, "TO-FULL-WT", "工种", Some(1)).await;
    link_work_type_to_process(&pool, wt, proc2).await;
    let wt_holder = insert_work_type(&pool, "TO-FULL-WH", "持有人工种", Some(5)).await;
    let holder = insert_worker(&pool, "TO-FULL-H", "持有人", Some(wt_holder)).await;
    let taker = insert_worker(&pool, "TO-FULL-T", "接管人", Some(wt)).await;
    // 接管人已经拿满 1/1
    insert_worker_held_part(&pool, customer, "SN-TO-FULL-HELD", taker, proc2, 1, true).await;
    // 目标：别人持有的可推导批次
    let (part, batch, _step1) =
        insert_worker_held_part(&pool, customer, "SN-TO-FULL", holder, proc1, 2, true).await;
    let chain = super::queue::part_chain_id(&pool, part).await.expect("链");
    append_chain_step(&pool, chain, proc2, 20).await;

    let (s, env) = take_over(
        &app,
        &token,
        json!({
            "batch_id": batch.to_string(),
            "badge_code": "TO-FULL-T",
            "version": 0,
        }),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "容量触顶应 409: {env}");
    assert_eq!(env["code"], 20204, "BIZ_WORKER_HOLD_LIMIT_EXCEEDED: {env}");
    let (_st, _loc, h, _p, _step, version, _q, _r) = read_batch(&pool, batch).await;
    assert_eq!(h, Some(holder), "20204 必须零写入");
    assert_eq!(version, 0);
}

/// 工种未配 `max_held_batches`（NULL = 未配置）→ `20904`（HTTP 400，零写入）。
#[tokio::test]
async fn take_over_max_held_not_set_20904() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let customer = insert_customer_l2(&pool, "TO-NOMAX").await;
    let proc1 = seed_process(&pool, "TO-NOMAX-P1", "工序1").await;
    let proc2 = seed_process(&pool, "TO-NOMAX-P2", "工序2").await;
    let wt = insert_work_type(&pool, "TO-NOMAX-WT", "工种", None).await;
    link_work_type_to_process(&pool, wt, proc2).await;
    let wt_holder = insert_work_type(&pool, "TO-NOMAX-WH", "持有人工种", Some(5)).await;
    let holder = insert_worker(&pool, "TO-NOMAX-H", "持有人", Some(wt_holder)).await;
    let _taker = insert_worker(&pool, "TO-NOMAX-T", "接管人", Some(wt)).await;
    let (part, batch, _step1) =
        insert_worker_held_part(&pool, customer, "SN-TO-NOMAX", holder, proc1, 1, true).await;
    let chain = super::queue::part_chain_id(&pool, part).await.expect("链");
    append_chain_step(&pool, chain, proc2, 20).await;

    let (s, env) = take_over(
        &app,
        &token,
        json!({
            "batch_id": batch.to_string(),
            "badge_code": "TO-NOMAX-T",
            "version": 0,
        }),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{env}");
    assert_eq!(env["code"], 20904, "BIZ_WORK_TYPE_MAX_HELD_NOT_SET: {env}");
    let (_st, _loc, h, _p, _step, version, _q, _r) = read_batch(&pool, batch).await;
    assert_eq!(h, Some(holder), "20904 必须零写入");
    assert_eq!(version, 0);
}

/// 角色门禁：CLERK → `40300`（与 worker-scan 同款，Manager + ShelfAccount 才可）。
#[tokio::test]
async fn take_over_clerk_forbidden_40300() {
    let (pool, app, _manager, _fx) = bootstrap_as_manager().await;
    let clerk = login_token(
        &app,
        PartFixture::CLERK_USERNAME,
        ProductionFixture::PASSWORD,
    )
    .await;
    let (_part, batch, holder, _taker, _p2, _s2) = setup_derivable(&pool, "TO-CLERK", true).await;

    let (s, env) = take_over(
        &app,
        &clerk,
        json!({
            "batch_id": batch.to_string(),
            "badge_code": "TO-CLERK-T",
            "version": 0,
        }),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "CLERK 应 403: {env}");
    assert_eq!(env["code"], 40300, "FORBIDDEN: {env}");
    let (_st, _loc, h, _p, _step, version, _q, _r) = read_batch(&pool, batch).await;
    assert_eq!(h, Some(holder), "40300 必须零写入");
    assert_eq!(version, 0);
}

// ===========================================================================
//  事件日志 / 关键回归
// ===========================================================================

/// `t_part_event` 的 `TRANSFERRED` 事件逐字段：`event_type` / `worker_id`（接管人）/
/// `process_id`（目标工序）/ `badge_code` / `batch_id` / `quantity` / 前后状态，以及
/// **两种 note 形态**（缺省自动文案 / 显式 note 拼接）。
#[tokio::test]
async fn take_over_writes_transferred_event() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;

    // 形态 ①：不传 note → "接管自 worker {旧持有人}"
    let (_part1, batch1, holder1, taker1, proc2_a, _s2a) =
        setup_derivable(&pool, "TO-EV1", true).await;
    let (s, env) = take_over(
        &app,
        &token,
        json!({
            "batch_id": batch1.to_string(),
            "badge_code": "TO-EV1-T",
            "version": 0,
        }),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{env}");

    // 形态 ②：传 note → "接管自 worker {旧持有人}：{note}"
    let (_part2, batch2, holder2, _taker2, _proc2b, _s2b) =
        setup_derivable(&pool, "TO-EV2", true).await;
    let (s, env) = take_over(
        &app,
        &token,
        json!({
            "batch_id": batch2.to_string(),
            "badge_code": "TO-EV2-T",
            "version": 0,
            "note": "图纸已在我台面",
        }),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{env}");

    // 逐字段断言（直接查事件表，锚 part_id + event_type）。
    let (
        event_type,
        from_status,
        to_status,
        batch_id,
        quantity,
        badge_code,
        worker_id,
        process_id,
        note,
    ): (String, String, String, i64, i32, String, i64, i64, String) = sqlx::query_as(
        "SELECT event_type, from_status, to_status, batch_id, quantity, badge_code, \
         worker_id, process_id, note \
         FROM t_part_event WHERE part_id = $1 AND event_type = 'TRANSFERRED'",
    )
    .bind(_part1)
    .fetch_one(&pool)
    .await
    .expect("应有 1 条 TRANSFERRED 事件");
    assert_eq!(event_type, "TRANSFERRED");
    assert_eq!(from_status, "IN_PROCESS");
    assert_eq!(to_status, "IN_PROCESS");
    assert_eq!(batch_id, batch1);
    assert_eq!(quantity, 3);
    assert_eq!(badge_code, "TO-EV1-T");
    assert_eq!(worker_id, taker1, "worker_id 必须是接管人（不是旧持有人）");
    assert_eq!(process_id, proc2_a, "process_id 必须快照目标工序");
    assert_eq!(
        note,
        format!("接管自 worker {holder1}"),
        "缺省 note 文案逐字固定"
    );

    let (_, _, _, batch_id2, quantity2, badge_code2, worker_id2, process_id2, note2): (
        String,
        String,
        String,
        i64,
        i32,
        String,
        i64,
        i64,
        String,
    ) = sqlx::query_as(
        "SELECT event_type, from_status, to_status, batch_id, quantity, badge_code, \
         worker_id, process_id, note \
         FROM t_part_event WHERE part_id = $1 AND event_type = 'TRANSFERRED'",
    )
    .bind(_part2)
    .fetch_one(&pool)
    .await
    .expect("应有 1 条 TRANSFERRED 事件");
    assert_eq!(batch_id2, batch2);
    assert_eq!(quantity2, 3);
    assert_eq!(badge_code2, "TO-EV2-T");
    assert_eq!(worker_id2, _taker2);
    assert_eq!(process_id2, _proc2b);
    assert_eq!(
        note2,
        format!("接管自 worker {holder2}：图纸已在我台面"),
        "显式 note 必须拼在自动文案之后（全角冒号）"
    );
}

/// **关键回归**：`is_repairing = true` 的批次接管后仍为 `true`。
///
/// 防的是「用 `mark_batch_with_status_and_meta` 实现接管」——那个漏斗会
/// **无条件清 `is_repairing`**，接一批返修件就会把它静默挪出返修流。
#[tokio::test]
async fn take_over_keeps_is_repairing_flag() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let (_part, batch, _holder, taker, proc2, step2) =
        setup_derivable(&pool, "TO-REPAIR", true).await;
    sqlx::query("UPDATE t_part_batch SET is_repairing = true WHERE id = $1")
        .bind(batch)
        .execute(&pool)
        .await
        .expect("mark repairing");

    let (s, env) = take_over(
        &app,
        &token,
        json!({
            "batch_id": batch.to_string(),
            "badge_code": "TO-REPAIR-T",
            "version": 0,
        }),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{env}");

    let (status, _loc, h, process, step, _version, _q, repairing) = read_batch(&pool, batch).await;
    assert_eq!(status, "IN_PROCESS");
    assert_eq!(h, Some(taker));
    assert_eq!(process, Some(proc2));
    assert_eq!(step, Some(step2));
    assert!(
        repairing,
        "接管后 is_repairing 必须保持 true —— 接管不改变返修标记，\
         严禁改用无条件清标记的 mark_batch_with_status_and_meta"
    );
}

// ===========================================================================
//  字段契约
// ===========================================================================

/// 逐键断言 `TakeOverOut` 的 7 个 JSON 键 + 雪花 string 形态。
#[tokio::test]
async fn take_over_response_pins_every_key() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let (_part, batch, _holder, _taker, _p2, _s2) = setup_derivable(&pool, "TO-KEYS", true).await;

    let (s, env) = take_over(
        &app,
        &token,
        json!({
            "batch_id": batch.to_string(),
            "badge_code": "TO-KEYS-T",
            "version": 0,
        }),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{env}");
    let data = env["data"].as_object().expect("data 是对象");
    let mut keys: Vec<&str> = data.keys().map(String::as_str).collect();
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
        "TakeOverOut 字段集变了（同步前端 schema 与 docs/api/scan.md）: {env}"
    );
    for key in [
        "batch_id",
        "part_id",
        "from_worker_id",
        "to_worker_id",
        "next_process_id",
    ] {
        assert!(
            env["data"][key].is_string(),
            "{key} 必须是 JSON string（雪花精度）: {env}"
        );
    }
    assert!(
        env["data"]["next_process_step_id"].is_string(),
        "有链时 next_process_step_id 也必须是 string: {env}"
    );
    assert!(
        env["data"]["version"].is_number(),
        "version 是裸 number: {env}"
    );
}

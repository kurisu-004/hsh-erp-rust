//! part 域 Phase 1（2026-09-13）返修闭环集成测试：1.4 端点。
//!
//! 覆盖：
//!   - complete_repair: 返修中 → IN_PROCESS（PRODUCTION 区）
//!   - complete_repair: 返修中 → INSPECTION（INSPECTION 区）
//!   - complete_repair: 非返修态 IN_PROCESS 批次被拒（20118，2026-10-01 新增）
//!   - repair_dispatch: 一步式返修下发
//!   - list_repair_batches / list_repairing_batches
//!
//! 2026-10-01 契约变更：REPAIRING 降级为 `t_part_batch.is_repairing` 标记列
//! （migration 005/006）—— 「返修中」批次的形态是
//! `status='IN_PROCESS' + is_repairing=true`，本文件所有「返修中」数据均由
//! `insert_repairing_part_batch` 造出，不再有任何一列取到 `'REPAIRING'`。

use axum::http::StatusCode;
use serde_json::json;
use sqlx::PgPool;

use hsh_erp_test_support::fixture::PartFixture;
use hsh_erp_test_support::{
    json_request, load_part_fixture, login_token, send, test_app, test_pool, test_state,
};

// ===========================================================================
//  动态 fixture helpers（PR-C.Final retry 第 3 轮，2026-09-24）
//  原从 `hsh_erp_test_support::fixtures::{create_chain_for_part / create_step}`
//  引入 2 helper，因 fixtures.rs 本轮被删，复制到本地（同形 sqlx::query 直插）。
// ===========================================================================

/// 为指定 part 建一个最小工艺链（t_part_process_chain），并把 part.process_chain_id 绑回。
async fn create_chain_for_part(pool: &PgPool, part_id: i64) -> i64 {
    use hsh_erp_test_support::pool_snowflake;
    let snowflake = pool_snowflake().lock().unwrap_or_else(|p| p.into_inner());
    let chain_id = snowflake.next_id();
    sqlx::query(
        "INSERT INTO t_part_process_chain (id, name, version, created_at, created_by, \
         updated_at, updated_by) \
         VALUES ($1, $2, 0, now(), 0, now(), 0)",
    )
    .bind(chain_id)
    .bind(format!("chain-{part_id}"))
    .execute(pool)
    .await
    .expect("insert chain");
    sqlx::query("UPDATE t_part SET process_chain_id = $1 WHERE id = $2")
        .bind(chain_id)
        .bind(part_id)
        .execute(pool)
        .await
        .expect("bind part to chain");
    chain_id
}

/// 在指定 chain 内创建 step（process_id + sort_order）。
async fn create_step(pool: &PgPool, chain_id: i64, process_id: i64, sort_order: i32) -> i64 {
    use hsh_erp_test_support::pool_snowflake;
    let snowflake = pool_snowflake().lock().unwrap_or_else(|p| p.into_inner());
    let step_id = snowflake.next_id();
    sqlx::query(
        "INSERT INTO t_process_chain_step (id, chain_id, sort_order, process_id, \
         estimated_minutes, version, created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, $3, $4, 30, 0, now(), 0, now(), 0)",
    )
    .bind(step_id)
    .bind(chain_id)
    .bind(sort_order)
    .bind(process_id)
    .execute(pool)
    .await
    .expect("insert chain step");
    step_id
}

// ===========================================================================
//  动态 part/batch 插入 helper（sub-file 私有，PR-C 末统一迁）
// ===========================================================================

/// 插入 part + 单批次（status 同步写到两列）。
///
/// 2026-10-01：新增 `is_repairing` 形参 —— REPAIRING 已从 `PartStatus` 降级为
/// `t_part_batch.is_repairing` 标记列（migration 005/006），「返修中」的批次
/// 形态是 `status='IN_PROCESS' + is_repairing=true`，**不再**有任何一列取到
/// `'REPAIRING'`。`insert_part_with_batch` 保留为「非返修」快捷入口。
#[allow(clippy::too_many_arguments)]
async fn insert_part_with_batch_flagged(
    pool: &PgPool,
    name: &str,
    customer_id: i64,
    status: &str,
    qty: i32,
    is_repairing: bool,
) -> (i64, i64) {
    use hsh_erp_rust::infra::clock::now_naive;
    // 2026-10-09：ID 统一从全进程共享 generator 取（`shared_test_snowflake`）——
    // 两个 fresh generator 同 instance 同毫秒各取 seq 0 会撞主键（23505），
    // 与「同一个 helper 调几次」无关，取号顺序保持不变。
    let part_id = hsh_erp_test_support::shared_test_snowflake().next_id();
    let batch_id = hsh_erp_test_support::shared_test_snowflake().next_id();
    let now = now_naive();
    let today = now.date();
    sqlx::query(
        "INSERT INTO t_part (id, serial_no, name, drawing_no, customer_id, status, \
         applicant_name, request_date, planned_delivery_date, quantity, version, \
         created_at, updated_at) \
         VALUES ($1, NULL, $2, 'D-001', $3, $6, $2, $4, $4, 1, 0, $5, $5)",
    )
    .bind(part_id)
    .bind(name)
    .bind(customer_id)
    .bind(today)
    .bind(now)
    .bind(status)
    .execute(pool)
    .await
    .expect("insert part");
    sqlx::query(
        "INSERT INTO t_part_batch (id, part_id, batch_no, quantity, status, is_repairing, \
         version, created_at, updated_at) \
         VALUES ($1, $2, 1, $3, $4, $5, 0, $6, $6)",
    )
    .bind(batch_id)
    .bind(part_id)
    .bind(qty)
    .bind(status)
    .bind(is_repairing)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert batch");
    (part_id, batch_id)
}

async fn insert_part_with_batch(
    pool: &PgPool,
    name: &str,
    customer_id: i64,
    status: &str,
    qty: i32,
) -> (i64, i64) {
    insert_part_with_batch_flagged(pool, name, customer_id, status, qty, false).await
}

/// 插入「返修中」批次：`status='IN_PROCESS'` + `is_repairing=true`
/// （2026-10-01 起的返修中唯一形态）。
async fn insert_repairing_part_batch(
    pool: &PgPool,
    name: &str,
    customer_id: i64,
    qty: i32,
) -> (i64, i64) {
    insert_part_with_batch_flagged(pool, name, customer_id, "IN_PROCESS", qty, true).await
}

async fn batch_version(pool: &PgPool, batch_id: i64) -> i32 {
    sqlx::query_scalar::<_, i32>("SELECT version FROM t_part_batch WHERE id = $1")
        .bind(batch_id)
        .fetch_one(pool)
        .await
        .expect("batch not found")
}

// ===========================================================================
//  bootstrap helpers
// ===========================================================================

async fn bootstrap_as_manager() -> (PgPool, axum::Router, String, PartFixture) {
    let pool = test_pool().await;
    let fx = load_part_fixture(&pool).await;
    let app = test_app(test_state(pool.clone()).await);
    let token = login_token(&app, &fx.manager_username, PartFixture::PASSWORD).await;
    (pool, app, token, fx)
}

#[tokio::test]
async fn complete_repair_to_process_happy_path() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let (pid, bid) = insert_repairing_part_batch(&pool, "P0", fx.customer_l2_id, 5).await;
    let version = batch_version(&pool, bid).await;

    // 2026-09-16 PR-3 批次 step 化：complete-repair / repair-dispatch
    // PRODUCTION 区要求 part 已绑定工艺链
    let chain_id = create_chain_for_part(&pool, pid).await;
    let _step_id = create_step(&pool, chain_id, fx.process_id, 1).await;
    // 注：fixture 已预置 fx.production_shelf_id ↔ fx.process_id 的映射
    // （t_shelf_process WORK_TYPE_PROCESS_ID），无需 link_shelf_to_process。
    let body = json!({
        "version": version,
        "shelf_id": fx.production_shelf_id.to_string(),
        "next_process_id": fx.process_id.to_string(),
    });
    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/prod/batches/{bid}/complete-repair"),
            Some(body),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "complete-repair: {env}");
    assert_eq!(env["data"]["status"], "IN_PROCESS");
    // 2026-10-01：返修完成必须清 `is_repairing` —— 否则「待返修列表」会把已经
    // 落回生产架的批次继续列成待返修。
    let is_repairing: bool =
        sqlx::query_scalar("SELECT is_repairing FROM t_part_batch WHERE id = $1")
            .bind(bid)
            .fetch_one(&pool)
            .await
            .expect("read is_repairing");
    assert!(
        !is_repairing,
        "complete-repair 完成后 is_repairing 应为 false"
    );
}

#[tokio::test]
async fn complete_repair_to_inspection_happy_path() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let (_pid, bid) = insert_repairing_part_batch(&pool, "P0", fx.customer_l2_id, 5).await;
    let version = batch_version(&pool, bid).await;
    let body = json!({
        "version": version,
        "shelf_id": fx.inspection_shelf_id.to_string(),
    });
    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/prod/batches/{bid}/complete-repair"),
            Some(body),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "complete-repair INSPECTION: {env}");
    assert_eq!(env["data"]["status"], "INSPECTION");
    // 2026-10-01：送检区去向同样清标记（否则「待返修列表」会列出已送检批次）
    let is_repairing: bool =
        sqlx::query_scalar("SELECT is_repairing FROM t_part_batch WHERE id = $1")
            .bind(bid)
            .fetch_one(&pool)
            .await
            .expect("read is_repairing");
    assert!(
        !is_repairing,
        "complete-repair（送检区）后 is_repairing 应为 false"
    );
}

#[tokio::test]
async fn complete_repair_invalid_source_rejects() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let (pid, bid) = insert_part_with_batch(&pool, "P0", fx.customer_l2_id, "PENDING", 5).await;
    let version = batch_version(&pool, bid).await;
    // PR-3：repair-dispatch PRODUCTION 区要求 part 已绑定工艺链
    let chain_id = create_chain_for_part(&pool, pid).await;
    let _step_id = create_step(&pool, chain_id, fx.process_id, 1).await;
    let body = json!({
        "version": version,
        "shelf_id": fx.production_shelf_id.to_string(),
        "next_process_id": fx.process_id.to_string(),
    });
    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/prod/batches/{bid}/complete-repair"),
            Some(body),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "PENDING 起点不允许: {env}");
    assert_eq!(env["code"], 20118);
}

/// 2026-10-01 新增：非返修态的 IN_PROCESS 批次必须被 `complete-repair` 拒。
///
/// 这是本轮最重要的一条收口：REPAIRING 降级为 `is_repairing` 标记列后，
/// 「返修中」与「正常生产中」的 `status` **完全相同**（都是 IN_PROCESS）。
/// 若守卫只判 status（上一轮任务 #1 的过渡形态），任意在产批次都能调
/// complete-repair 改位置 / 改工序 —— 等于绕开 start-repair / scan-inspect
/// 两个起修入口任意搬运在产批次。守卫必须落到 `is_repairing` 列上。
#[tokio::test]
async fn complete_repair_non_repairing_in_process_rejects() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    // 正常在产批次：IN_PROCESS + is_repairing = false
    let (pid, bid) = insert_part_with_batch(&pool, "P0", fx.customer_l2_id, "IN_PROCESS", 5).await;
    let version = batch_version(&pool, bid).await;
    let chain_id = create_chain_for_part(&pool, pid).await;
    let _step_id = create_step(&pool, chain_id, fx.process_id, 1).await;
    let body = json!({
        "version": version,
        "shelf_id": fx.production_shelf_id.to_string(),
        "next_process_id": fx.process_id.to_string(),
    });
    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/prod/batches/{bid}/complete-repair"),
            Some(body),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::BAD_REQUEST,
        "非返修态 IN_PROCESS 批次不允许 complete-repair: {env}"
    );
    assert_eq!(
        env["code"], 20118,
        "错误码应为 BIZ_PART_REPAIR_NOT_TRIGGERED"
    );
    assert!(
        env["message"].as_str().unwrap_or_default().contains("返修"),
        "错误信息应说明「未处于返修中」: {env}"
    );
    // 批次必须原封不动（守卫发生在任何写之前）
    let (status, is_repairing, location): (String, bool, Option<String>) =
        sqlx::query_as("SELECT status, is_repairing, location FROM t_part_batch WHERE id = $1")
            .bind(bid)
            .fetch_one(&pool)
            .await
            .expect("read batch");
    assert_eq!(status, "IN_PROCESS");
    assert!(!is_repairing);
    assert_eq!(location, None, "守卫失败时不得写 location");
}

#[tokio::test]
async fn repair_dispatch_happy_path() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    // 入口：INSPECTION / READY_TO_SHIP / IN_PROCESS / DELIVERED
    let (_pid, bid) = insert_part_with_batch(&pool, "P0", fx.customer_l2_id, "INSPECTION", 5).await;
    let version = batch_version(&pool, bid).await;
    let body = json!({
        "version": version,
        "shelf_id": fx.inspection_shelf_id.to_string(),
    });
    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/prod/batches/{bid}/repair-dispatch"),
            Some(body),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "repair-dispatch: {env}");
    assert_eq!(env["data"]["status"], "INSPECTION");
}

#[tokio::test]
async fn repair_dispatch_invalid_source_rejects() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let (_pid, bid) = insert_part_with_batch(&pool, "P0", fx.customer_l2_id, "CANCELLED", 5).await;
    let version = batch_version(&pool, bid).await;
    let body = json!({
        "version": version,
        "shelf_id": fx.inspection_shelf_id.to_string(),
    });
    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/prod/batches/{bid}/repair-dispatch"),
            Some(body),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "CANCELLED 起点不允许: {env}");
    assert_eq!(env["code"], 20103);
}

#[tokio::test]
async fn list_repair_batches_happy_path() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let (pid, _bid) = insert_part_with_batch(&pool, "P0", fx.customer_l2_id, "DELIVERED", 5).await;
    let (s, env) = send(
        app,
        json_request("GET", "/prod/batches/repair", None, Some(&token)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "list-repair-batches: {env}");
    assert_eq!(env["code"], 0);
    let _ = (pid, fx); // suppress unused
}

/// 2026-10-01 契约变更：判据由 `status='REPAIRING'` 改为 `is_repairing = true`。
/// 本测试因此**同时**断言正反两面 —— 返修中批次在列、非返修批次（DELIVERED）
/// 不在列：只断言「返修批次在列」的话，判据退化成「不过滤」时也会通过。
#[tokio::test]
async fn list_repairing_batches_happy_path() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let (_pid, bid) = insert_repairing_part_batch(&pool, "P0", fx.customer_l2_id, 5).await;
    let (_pid2, delivered_bid) =
        insert_part_with_batch(&pool, "P1", fx.customer_l2_id, "DELIVERED", 5).await;
    let (s, env) = send(
        app,
        json_request("GET", "/prod/batches/repairing", None, Some(&token)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "list-repairing-batches: {env}");
    assert_eq!(env["code"], 0);
    let items = env["data"]["items"].as_array().expect("data.items");
    let ids: Vec<String> = items
        .iter()
        .map(|i| i["batch_id"].as_str().unwrap_or_default().to_string())
        .collect();
    assert!(
        ids.contains(&bid.to_string()),
        "返修中批次（is_repairing=true）应出现在列表: {env}"
    );
    assert!(
        !ids.contains(&delivered_bid.to_string()),
        "非返修批次（DELIVERED）不应出现在返修中列表: {env}"
    );
}

// ===========================================================================
//  回归测试（2026-09-30，review 第 3 轮 M3 补漏项）
// ===========================================================================

/// 造出**真实送检后形态**的 DELIVERED 批次：`current_process_step_id` 非空
/// （送检期间被刻意保留）+ `current_process_id` 显式 NULL。
///
/// 为什么必须是这个形态：进 `READY_TO_SHIP` 的迁移边**只有**
/// `INSPECTION → READY_TO_SHIP`，进 `DELIVERED` 的边**只有**
/// `READY_TO_SHIP → DELIVERED`；而所有进 INSPECTION 的写点都把
/// `current_process_id` 置 NULL，其后 `mark_batch_passed_inspection` 与
/// `mark_batch_delivered` 都不写该列 ⇒ DELIVERED 批次该列**结构性恒 NULL**。
/// 换言之，真实生产库里没有「DELIVERED 批次带 cpid」这种数据形态。
///
/// 返回 `(part_id, batch_id, process_id, process_name)`。
#[allow(clippy::too_many_arguments)]
async fn insert_step_located_delivered_part_batch(
    pool: &PgPool,
    part_name: &str,
    customer_id: i64,
    serial_no: &str,
    process_code: &str,
    process_name: &str,
) -> (i64, i64, i64, String) {
    use hsh_erp_rust::infra::clock::now_naive;
    // 2026-10-09：ID 统一从全进程共享 generator 取（`shared_test_snowflake`）——
    // 两个 fresh generator 同 instance 同毫秒各取 seq 0 会撞主键（23505），
    // 与「同一个 helper 调几次」无关，取号顺序保持不变。
    let now = now_naive();
    let today = now.date();

    // 1. 工序（列表查询只 JOIN t_process 取名，不需 t_shelf_process 映射）
    let process_id = hsh_erp_test_support::shared_test_snowflake().next_id();
    sqlx::query(
        "INSERT INTO t_process (id, code, name, category, sort_order, requires_approval, \
         version, created_at, updated_at) \
         VALUES ($1, $2, $3, 'INHOUSE', 0, false, 0, $4, $4)",
    )
    .bind(process_id)
    .bind(process_code)
    .bind(process_name)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_process");

    // 2. 工艺链 + step（step 指向该工序）
    let chain_id = hsh_erp_test_support::shared_test_snowflake().next_id();
    sqlx::query(
        "INSERT INTO t_part_process_chain (id, name, version, created_at, created_by, \
         updated_at, updated_by) VALUES ($1, $2, 0, $3, 0, $3, 0)",
    )
    .bind(chain_id)
    .bind(format!("chain-{process_code}"))
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_part_process_chain");
    let step_id = hsh_erp_test_support::shared_test_snowflake().next_id();
    sqlx::query(
        "INSERT INTO t_process_chain_step (id, chain_id, sort_order, process_id, \
         estimated_minutes, version, created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, 1, $3, 30, 0, $4, 0, $4, 0)",
    )
    .bind(step_id)
    .bind(chain_id)
    .bind(process_id)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_process_chain_step");

    // 3. DELIVERED part（绑上 chain，与真实数据一致）
    let part_id = hsh_erp_test_support::shared_test_snowflake().next_id();
    sqlx::query(
        "INSERT INTO t_part (id, serial_no, name, drawing_no, customer_id, status, \
         applicant_name, request_date, planned_delivery_date, quantity, version, \
         created_at, updated_at, process_chain_id) \
         VALUES ($1, $2, $3, 'D-REPAIR-001', $4, 'DELIVERED', $3, $5, $5, 1, 0, $6, $6, $7)",
    )
    .bind(part_id)
    .bind(serial_no)
    .bind(part_name)
    .bind(customer_id)
    .bind(today)
    .bind(now)
    .bind(chain_id)
    .execute(pool)
    .await
    .expect("insert DELIVERED part with chain");

    // 4. DELIVERED 批次：step 有值、cpid **显式 NULL**（真实送检后形态）
    let batch_id = hsh_erp_test_support::shared_test_snowflake().next_id();
    sqlx::query(
        "INSERT INTO t_part_batch (id, part_id, batch_no, quantity, status, \
         current_process_id, current_process_step_id, version, created_at, updated_at) \
         VALUES ($1, $2, 1, 1, 'DELIVERED', NULL, $3, 0, $4, $4)",
    )
    .bind(batch_id)
    .bind(part_id)
    .bind(step_id)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert step-located DELIVERED batch");

    (part_id, batch_id, process_id, process_name.to_string())
}

/// `GET /prod/batches/repair`：`next_process_id` / `next_process_name` 必须由
/// `current_process_step_id` → step JOIN 派生，**不直读** `current_process_id`。
///
/// 若有人把 `list_batches_matching` 改回直读 cpid，本测试必红
/// （DELIVERED 必经 INSPECTION → cpid 恒 NULL → 两字段变 null）。
#[tokio::test]
async fn repair_batches_derives_next_process_from_step_not_cpid() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let (_part_id, batch_id, process_id, process_name) = insert_step_located_delivered_part_batch(
        &pool,
        "PART_REPAIR_STEP",
        fx.customer_l2_id,
        "P-RPR-001",
        "PROC-RPR-STEP",
        "返修显示工序",
    )
    .await;

    // 前置断言：DB 层确实是「step 有值 + cpid 为 NULL」的真实 DELIVERED 形态
    let (step_opt, cpid): (Option<i64>, Option<i64>) = sqlx::query_as(
        "SELECT current_process_step_id, current_process_id FROM t_part_batch WHERE id = $1",
    )
    .bind(batch_id)
    .fetch_one(&pool)
    .await
    .expect("read batch process columns");
    assert!(
        step_opt.is_some(),
        "前置条件：current_process_step_id 应非空（送检期间保留），实际 {step_opt:?}"
    );
    assert_eq!(
        cpid, None,
        "前置条件：current_process_id 应为 NULL（DELIVERED 必经 INSPECTION = 出池），\
         实际 {cpid:?}；若本断言失败说明出池不变式被回退，测试前提已变"
    );

    let (s, env) = send(
        app,
        json_request("GET", "/prod/batches/repair?limit=50", None, Some(&token)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "list-repair-batches: {env}");
    assert_eq!(env["code"], 0);

    let items = env["data"]["items"].as_array().expect("data.items");
    let batch_id_str = batch_id.to_string();
    let process_id_str = process_id.to_string();
    let hit = items
        .iter()
        .find(|i| i["batch_id"] == batch_id_str)
        .unwrap_or_else(|| panic!("items 应含 batch_id={batch_id}: {env}"));

    assert_eq!(
        hit["next_process_id"], process_id_str,
        "next_process_id 必须由 current_process_step_id → step JOIN 派生为该 step 的 \
         process_id（{process_id}）。实际 {:?} —— 若为 null 说明查询被改成直读 \
         current_process_id，而 DELIVERED 必经 INSPECTION（=出池）已把该列置 NULL \
         （review M3 同形回归）",
        hit["next_process_id"]
    );
    assert_eq!(
        hit["next_process_name"].as_str(),
        Some(process_name.as_str()),
        "next_process_name 应由同一条 step JOIN 链解析出 {process_name}: {env}"
    );
    assert_eq!(
        hit["current_process_step_id"],
        step_opt.unwrap().to_string(),
        "current_process_step_id 原样透出（它是 next_process_id 的派生源）: {env}"
    );
}

/// `GET /prod/batches/repairing` 走的是**同一条** SQL（2026-10-01 起判据是
/// `is_repairing = true`），故同样不直读 `current_process_id`。
///
/// ## 2026-10-01：测试前提随语义变更重写
///
/// 原前提是「`mark_batch_repairing` 把批次翻出 IN_PROCESS 却不清 cpid
/// ⇒ 返修批次带进返修前的陈旧 cpid」（迁移 004「已知局限 4d」）。REPAIRING
/// 降级为 `is_repairing` 标记列后该写点**不再翻 status**（保持 IN_PROCESS、
/// 不动 cpid），这个前提在生产数据里**已不可能出现**（返修批次要么停在送检架
/// → 出池、cpid 按不变式为 NULL；要么留在原池 → cpid 是它**当前**池归属、
/// 不是陈旧值）。
///
/// 因此本测试改为**故意造出不一致行**（cpid 指向另一道工序），断言端点**仍然**
/// 返回 step 派生的工序 —— 它现在守的是「展示类列表一律走 step 派生」这条
/// 统一分工（见 `part/batch/model.rs` 模块 doc 的读取方清单）：若有人把
/// `next_process_id` 改回直读 cpid，本测试必红。数据形态的来源在注释里说明
/// 为「迁移 006 之前的存量 / 人工订正」，而不是声称生产路径会产生它。
#[tokio::test]
async fn repairing_batches_derives_next_process_from_step_not_stale_cpid() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let (part_id, batch_id, process_id, process_name) = insert_step_located_delivered_part_batch(
        &pool,
        "PART_REPAIRING_STEP",
        fx.customer_l2_id,
        "P-RPR-002",
        "PROC-RPRING-STEP",
        "返修中显示工序",
    )
    .await;

    // 改造成「返修中」形态（status=IN_PROCESS + is_repairing=true），并写入一个
    // 与 step **不一致**的 cpid（模拟脏数据：迁移 006 之前的存量 / 人工订正）
    let stale_cpid = sqlx::query_scalar::<_, i64>(
        "INSERT INTO t_process (id, code, name, category, sort_order, requires_approval, \
         version, created_at, updated_at) \
         VALUES (9000000000000099901, 'PROC-STALE', '陈旧工序', 'INHOUSE', 0, false, 0, \
                 now(), now()) RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .expect("insert stale process");
    sqlx::query(
        "UPDATE t_part_batch SET status = 'IN_PROCESS', is_repairing = true, \
         current_process_id = $2 WHERE id = $1",
    )
    .bind(batch_id)
    .bind(stale_cpid)
    .execute(&pool)
    .await
    .expect("flip batch to repairing with inconsistent cpid");

    let (s, env) = send(
        app,
        json_request(
            "GET",
            "/prod/batches/repairing?limit=50",
            None,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "list-repairing-batches: {env}");
    assert_eq!(env["code"], 0);

    let items = env["data"]["items"].as_array().expect("data.items");
    let batch_id_str = batch_id.to_string();
    let hit = items
        .iter()
        .find(|i| i["batch_id"] == batch_id_str)
        .unwrap_or_else(|| panic!("items 应含 batch_id={batch_id}: {env}"));

    assert_eq!(
        hit["status"], "IN_PROCESS",
        "返修中批次的 status 应为 IN_PROCESS（返修事实在 is_repairing 标记上）: {env}"
    );
    assert_eq!(
        hit["next_process_id"],
        process_id.to_string(),
        "next_process_id 必须由 step JOIN 派生（{process_id}），而非与 step 不一致的 \
         cpid（{stale_cpid}）。实际 {:?}",
        hit["next_process_id"]
    );
    assert_eq!(
        hit["next_process_name"].as_str(),
        Some(process_name.as_str()),
        "next_process_name 应为 step 派生的 {process_name}，而非「陈旧工序」: {env}"
    );
    let _ = part_id;
}

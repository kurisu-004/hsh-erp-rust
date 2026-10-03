//! `GET /outsource-sendable` 集成测试（2026-10-03 新增）
//!
//! 覆盖：
//! - 判据是 `t_part_batch.current_process_id` 指向一道 OUTSOURCE 工序；
//!   **零件有没有工艺链都不影响**（生产库里绝大多数零件无链）
//! - `requires_approval = false`（免审批）→ `send_mode="DIRECT"`，即便历史上存在
//!   已批准报价也判 DIRECT 且报价三件套为 `null`
//! - `requires_approval = true` + 已批准报价 → `send_mode="APPROVAL"` / `price` 非
//!   null / `quote_id` 非 null / `company_options` 空数组
//! - `requires_approval = true` + 无已批准报价 → **不出行**（新增的排除语义）
//! - `requires_approval = true` + 只有 `is_direct=true` 的 0 元占位报价 → **不出行**
//!   （2026-10-03 review 第 1 轮：占位报价不是「被人审批过的报价」）；
//!   占位报价与真实审批报价并存时出行且回传后者
//! - DIRECT 但未映射任何活跃公司 → **该行仍返回**，`company_options` 空数组
//! - `current_process_id` 指向非 OUTSOURCE 工序 → 不出行
//! - `PENDING` 且未上架（无 holder）的批次 → 出行，`shelf_code` 为 `null`
//! - `version` == `t_part_batch.version`；`source_status` 区分 PENDING / IN_PROCESS
//! - `customer_id` query 过滤生效
//! - `total` 与 items 实际行数一致（含 DIRECT 空 options 行）
//! - `is_urgent` 排序在首

use axum::http::StatusCode;
use serde_json::json;
use sqlx::PgPool;

use hsh_erp_rust::infra::clock::now_naive;
use hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator;
use hsh_erp_test_support::{
    OutsourceFixture, json_request, load_outsource_fixture, login_token, send, test_app, test_pool,
    test_state,
};

async fn bootstrap_as_manager() -> (PgPool, axum::Router, String, OutsourceFixture) {
    let pool = test_pool().await;
    let fx = load_outsource_fixture(&pool).await;
    let app = test_app(test_state(pool.clone()).await);
    let token = login_token(&app, &fx.part_manager_username, OutsourceFixture::PASSWORD).await;
    (pool, app, token, fx)
}

// ===========================================================================
//  域独享 helpers
// ===========================================================================

async fn insert_customer(pool: &PgPool, name: &str, prefix: &str) -> i64 {
    let id = SnowflakeIdGenerator::new(1_577_836_800_000, 1).next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_customer (id, name, serial_prefix, version, created_at, updated_at) \
         VALUES ($1, $2, $3, 0, $4, $4)",
    )
    .bind(id)
    .bind(name)
    .bind(prefix)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_customer");
    id
}

async fn insert_part(
    pool: &PgPool,
    customer_id: i64,
    tag: &str,
    urgent: bool,
    planned: &str,
) -> i64 {
    let id = SnowflakeIdGenerator::new(1_577_836_800_000, 1).next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_part (id, name, drawing_no, applicant_name, quantity, unit_price, total_price, \
         request_date, planned_delivery_date, customer_id, is_urgent, status, version, created_at, updated_at) \
         VALUES ($1, $2, $3, 'Tester', 5, 1.00, 5.00, CURRENT_DATE, $4::date, $5, $6, 'PENDING', 0, $7, $7)",
    )
    .bind(id)
    .bind(format!("NAME-{tag}"))
    .bind(format!("DWG-{tag}-{id}"))
    .bind(planned)
    .bind(customer_id)
    .bind(urgent)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_part");
    id
}

/// `requires_approval` 是可发送判定的输入（false = 免审批直发），故由用例显式指定。
async fn seed_outsource_process(pool: &PgPool, code: &str, requires_approval: bool) -> i64 {
    let id = SnowflakeIdGenerator::new(1_577_836_800_000, 1).next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_process (id, code, name, category, sort_order, requires_approval, \
         version, created_at, updated_at) \
         VALUES ($1, $2, $3, 'OUTSOURCE', 0, $4, 0, $5, $5)",
    )
    .bind(id)
    .bind(code)
    .bind(format!("PROC-{code}"))
    .bind(requires_approval)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_process");
    id
}

async fn insert_shelf(pool: &PgPool, code: &str) -> i64 {
    let id = SnowflakeIdGenerator::new(1_577_836_800_000, 1).next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_shelf (id, code, name, zone, is_active, version, created_at, updated_at) \
         VALUES ($1, $2, $3, 'PRODUCTION', true, 0, $4, $4)",
    )
    .bind(id)
    .bind(code)
    .bind(format!("shelf-{code}"))
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_shelf");
    id
}

async fn link_shelf_process(pool: &PgPool, shelf_id: i64, process_id: i64) {
    let id = SnowflakeIdGenerator::new(1_577_836_800_000, 1).next_id();
    sqlx::query(
        "INSERT INTO t_shelf_process (id, shelf_id, process_id, sort_order, version, created_at, updated_at) \
         VALUES ($1, $2, $3, 0, 0, now(), now())",
    )
    .bind(id)
    .bind(shelf_id)
    .bind(process_id)
    .execute(pool)
    .await
    .expect("insert t_shelf_process");
}

async fn create_chain_with_step(pool: &PgPool, part_id: i64, process_id: i64) {
    let chain_id = SnowflakeIdGenerator::new(1_577_836_800_000, 1).next_id();
    sqlx::query(
        "INSERT INTO t_part_process_chain (id, name, version, created_at, created_by, updated_at, updated_by) \
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
    let step_id = SnowflakeIdGenerator::new(1_577_836_800_000, 1).next_id();
    sqlx::query(
        "INSERT INTO t_process_chain_step (id, chain_id, sort_order, process_id, estimated_minutes, \
         version, created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, 1, $3, 30, 0, now(), 0, now(), 0)",
    )
    .bind(step_id)
    .bind(chain_id)
    .bind(process_id)
    .execute(pool)
    .await
    .expect("insert step");
}

/// 直插一个候选批次。
///
/// `current_process_id` 是**可发送判定的权威输入**（必须指向一道 OUTSOURCE 工序），
/// 故作为形参显式给出；`shelf_id` 可为 `None`（`PENDING` 未上架的常态）。
#[allow(clippy::too_many_arguments)]
async fn insert_batch(
    pool: &PgPool,
    part_id: i64,
    batch_no: i32,
    shelf_id: Option<i64>,
    status: &str,
    location: Option<&str>,
    current_process_id: Option<i64>,
    version: i32,
) -> i64 {
    let id = SnowflakeIdGenerator::new(1_577_836_800_000, 1).next_id();
    sqlx::query(
        "INSERT INTO t_part_batch \
         (id, part_id, batch_no, quantity, status, location, current_holder_id, \
          current_process_id, version, created_at, updated_at) \
         VALUES ($1, $2, $3, 8, $4, $5, $6, $7, $8, now(), now())",
    )
    .bind(id)
    .bind(part_id)
    .bind(batch_no)
    .bind(status)
    .bind(location)
    .bind(shelf_id)
    .bind(current_process_id)
    .bind(version)
    .execute(pool)
    .await
    .expect("insert t_part_batch");
    id
}

/// 建一条 INHOUSE 工序（用作「`current_process_id` 不该被算成可发送」的反例）。
async fn seed_inhouse_process(pool: &PgPool, code: &str) -> i64 {
    let id = SnowflakeIdGenerator::new(1_577_836_800_000, 1).next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_process (id, code, name, category, sort_order, requires_approval, \
         version, created_at, updated_at) \
         VALUES ($1, $2, $3, 'INHOUSE', 0, false, 0, $4, $4)",
    )
    .bind(id)
    .bind(code)
    .bind(format!("PROC-{code}"))
    .bind(now)
    .execute(pool)
    .await
    .expect("insert INHOUSE t_process");
    id
}

async fn insert_company(pool: &PgPool, name: &str, is_active: bool) -> i64 {
    let id = SnowflakeIdGenerator::new(1_577_836_800_000, 1).next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_outsource_company (id, name, is_active, version, created_at, updated_at) \
         VALUES ($1, $2, $3, 0, $4, $4)",
    )
    .bind(id)
    .bind(name)
    .bind(is_active)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_outsource_company");
    id
}

async fn link_company_process(pool: &PgPool, company_id: i64, process_id: i64) {
    let id = SnowflakeIdGenerator::new(1_577_836_800_000, 1).next_id();
    sqlx::query(
        "INSERT INTO t_outsource_company_process \
         (id, outsource_company_id, process_id, sort_order, version, created_at, updated_at) \
         VALUES ($1, $2, $3, 0, 0, now(), now())",
    )
    .bind(id)
    .bind(company_id)
    .bind(process_id)
    .execute(pool)
    .await
    .expect("insert t_outsource_company_process");
}

async fn insert_approved_quote(
    pool: &PgPool,
    part_id: i64,
    company_id: i64,
    process_id: i64,
    price: &str,
) -> i64 {
    let id = SnowflakeIdGenerator::new(1_577_836_800_000, 1).next_id();
    sqlx::query(
        "INSERT INTO t_outsource_quote \
         (id, part_id, outsource_company_id, process_id, price, status, submitted_at, reviewed_at, \
          version, created_at, updated_at) \
         VALUES ($1, $2, $3, $4, $5::numeric, 'APPROVED', now(), now(), 0, now(), now())",
    )
    .bind(id)
    .bind(part_id)
    .bind(company_id)
    .bind(process_id)
    .bind(price)
    .execute(pool)
    .await
    .expect("insert t_outsource_quote");
    id
}

async fn get_sendable(
    app: &axum::Router,
    token: &str,
    qs: &str,
) -> (StatusCode, serde_json::Value) {
    send(
        app.clone(),
        json_request(
            "GET",
            &format!("/outsource-sendable{qs}"),
            None,
            Some(token),
        ),
    )
    .await
}

// ===========================================================================
//  Tests
// ===========================================================================

#[tokio::test]
async fn sendable_approval_mode_when_approved_quote_exists() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let cid = insert_customer(&pool, "SdCo", "J").await;
    let pid = insert_part(&pool, cid, "APPR", false, "2026-12-01").await;
    // requires_approval = true + 已有 APPROVED 报价 → APPROVAL
    let proc_id = seed_outsource_process(&pool, "SDAP", true).await;
    let shelf_id = insert_shelf(&pool, "SA1").await;
    // 同样映射一家公司 —— APPROVAL 模式下 company_options 必须仍为空数组
    let co = insert_company(&pool, "ApprovalCo", true).await;
    link_company_process(&pool, co, proc_id).await;
    let bid = insert_batch(
        &pool,
        pid,
        1,
        Some(shelf_id),
        "PENDING",
        None,
        Some(proc_id),
        4,
    )
    .await;
    let qid = insert_approved_quote(&pool, pid, co, proc_id, "12.50").await;

    let (s, env) = get_sendable(&app, &token, "").await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(env["data"]["total"], 1, "{env}");
    let row = &env["data"]["items"][0];
    assert_eq!(row["send_mode"], "APPROVAL", "{env}");
    assert_eq!(row["price"], "12.50", "{env}");
    assert_eq!(row["quote_id"], qid.to_string(), "{env}");
    assert_eq!(row["outsource_company_id"], co.to_string(), "{env}");
    assert_eq!(row["outsource_company_name"], "ApprovalCo", "{env}");
    assert_eq!(
        row["company_options"].as_array().unwrap().len(),
        0,
        "APPROVAL 模式 company_options 必须空数组: {env}"
    );
    // version 取 batch.version（4），batch_quantity / quantity 都来自批次
    assert_eq!(row["version"], 4, "{env}");
    assert_eq!(row["batch_id"], bid.to_string(), "{env}");
    assert_eq!(row["batch_no"], 1, "{env}");
    assert_eq!(row["batch_quantity"], 8, "{env}");
    assert_eq!(row["quantity"], 8, "{env}");
    assert_eq!(row["source_status"], "PENDING", "{env}");
    assert_eq!(row["current_process_id"], proc_id.to_string(), "{env}");
    assert_eq!(row["current_process_name"], "PROC-SDAP", "{env}");
    assert_eq!(row["shelf_code"], "SA1", "{env}");
    assert_eq!(row["status_label"], "sendable", "{env}");
    assert_eq!(row["customer_path"], "SdCo", "{env}");
    assert_eq!(row["planned_delivery_date"], "2026-12-01", "{env}");
}

#[tokio::test]
async fn sendable_direct_mode_lists_active_company_options() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let cid = insert_customer(&pool, "SdDir", "K").await;
    let pid = insert_part(&pool, cid, "DIR", false, "2026-12-01").await;
    // requires_approval = false → 免审批直发
    let proc_id = seed_outsource_process(&pool, "SDDR", false).await;
    let shelf_id = insert_shelf(&pool, "SB1").await;
    let co1 = insert_company(&pool, "DirectCo1", true).await;
    let co2 = insert_company(&pool, "DirectCo2", true).await;
    // 停用公司不得进 company_options
    let co_off = insert_company(&pool, "DisabledCo", false).await;
    link_company_process(&pool, co1, proc_id).await;
    link_company_process(&pool, co2, proc_id).await;
    link_company_process(&pool, co_off, proc_id).await;
    insert_batch(
        &pool,
        pid,
        1,
        Some(shelf_id),
        "PENDING",
        None,
        Some(proc_id),
        0,
    )
    .await;

    let (s, env) = get_sendable(&app, &token, "").await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(env["data"]["total"], 1, "{env}");
    let row = &env["data"]["items"][0];
    assert_eq!(row["send_mode"], "DIRECT", "{env}");
    // DIRECT 无报价
    assert!(row["quote_id"].is_null(), "{env}");
    assert!(row["price"].is_null(), "{env}");
    assert!(row["outsource_company_id"].is_null(), "{env}");
    let opts = row["company_options"].as_array().unwrap();
    assert_eq!(opts.len(), 2, "只列活跃公司: {env}");
    let ids: Vec<String> = opts
        .iter()
        .map(|o| o["id"].as_str().unwrap().to_string())
        .collect();
    assert!(ids.contains(&co1.to_string()), "{env}");
    assert!(ids.contains(&co2.to_string()), "{env}");
    assert!(
        !ids.contains(&co_off.to_string()),
        "停用公司不得出现: {env}"
    );
    let names: Vec<&str> = opts.iter().map(|o| o["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"DirectCo1"), "{env}");
    assert!(names.contains(&"DirectCo2"), "{env}");
}

/// 2026-10-03 新增：`requires_approval = false` 时**即便**（part, process）上存在
/// 一条已批准的报价也判 DIRECT，且报价三件套恒 `null`。
///
/// 这条锁住 SQL 层 `LEFT JOIN t_outsource_quote … AND pr.requires_approval` 那个
/// 谓词：少了它，免审批工序上的一条历史报价会把该行变成「APPROVAL + 空
/// company_options」—— 两种模式的字段契约同时被破坏（前端既拿不到候选公司，
/// 又被告知走 APPROVAL 报价价）。
#[tokio::test]
async fn sendable_direct_mode_even_with_approved_quote_when_approval_not_required() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    // ⚠️ `serial_prefix` 只能占 1 个字符且根客户全局唯一（uq_t_customer_root_prefix）：
    // `'P'` 已被 `load_outsource_fixture` 里的 PartFixture L1 客户占用，本文件其余用例
    // 各自占一位（J/K/L/M/N/O/Q/R/S/T/U/V）。新加用例请先照此清单挑一个没被占的。
    let cid = insert_customer(&pool, "SdNoAp", "W").await;
    let pid = insert_part(&pool, cid, "NOAP", false, "2026-12-01").await;
    let proc_id = seed_outsource_process(&pool, "SDNA", false).await;
    let shelf_id = insert_shelf(&pool, "SN1").await;
    let co1 = insert_company(&pool, "NoApCo1", true).await;
    let co2 = insert_company(&pool, "NoApCo2", true).await;
    link_company_process(&pool, co1, proc_id).await;
    link_company_process(&pool, co2, proc_id).await;
    // 历史遗留的已批准报价（免审批工序上不该用它发货）
    let qid = insert_approved_quote(&pool, pid, co1, proc_id, "99.00").await;
    let qid_s = qid.to_string();
    insert_batch(
        &pool,
        pid,
        1,
        Some(shelf_id),
        "PENDING",
        None,
        Some(proc_id),
        0,
    )
    .await;

    let (s, env) = get_sendable(&app, &token, "").await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(env["data"]["total"], 1, "{env}");
    let row = &env["data"]["items"][0];
    assert_eq!(row["send_mode"], "DIRECT", "免审批工序恒 DIRECT: {env}");
    assert!(
        row["quote_id"].is_null(),
        "DIRECT 行不得回传历史报价 id（{qid_s}）: {env}"
    );
    assert!(row["price"].is_null(), "{env}");
    assert!(row["outsource_company_id"].is_null(), "{env}");
    assert_eq!(
        row["company_options"].as_array().unwrap().len(),
        2,
        "DIRECT 行必须列候选公司供用户选: {env}"
    );
}

/// 2026-10-03 新增：`requires_approval = true` 但**没有**已批准报价 → 不出行。
///
/// 这是本次引入的新排除语义（原先 `requires_approval` 是只写不读的死字段），
/// 锁住「需审批的工序必须先有审批通过的报价才可发」。
#[tokio::test]
async fn sendable_requires_approval_without_quote_excluded() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let cid = insert_customer(&pool, "SdApNoQ", "Q").await;
    let pid = insert_part(&pool, cid, "APNOQ", false, "2026-12-01").await;
    let proc_id = seed_outsource_process(&pool, "SDAPNQ", true).await;
    let shelf_id = insert_shelf(&pool, "SQ1").await;
    // 映射了公司（DIRECT 的必要条件齐备）但缺审批报价 → 仍不得出行
    let co = insert_company(&pool, "ApNoQCo", true).await;
    link_company_process(&pool, co, proc_id).await;
    insert_batch(
        &pool,
        pid,
        1,
        Some(shelf_id),
        "PENDING",
        None,
        Some(proc_id),
        0,
    )
    .await;

    let (s, env) = get_sendable(&app, &token, "").await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(
        env["data"]["total"], 0,
        "需审批但无已批准报价必须不出现: {env}"
    );
    assert_eq!(env["data"]["items"].as_array().unwrap().len(), 0, "{env}");
}

/// 2026-10-03 新增：DRAFT 报价不算「已批准」⇒ 同样不得出行。
#[tokio::test]
async fn sendable_requires_approval_with_draft_quote_excluded() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let cid = insert_customer(&pool, "SdApDft", "R").await;
    let pid = insert_part(&pool, cid, "APDFT", false, "2026-12-01").await;
    let proc_id = seed_outsource_process(&pool, "SDAPDFT", true).await;
    let shelf_id = insert_shelf(&pool, "SR1").await;
    let co = insert_company(&pool, "ApDftCo", true).await;
    link_company_process(&pool, co, proc_id).await;
    let qid = SnowflakeIdGenerator::new(1_577_836_800_000, 1).next_id();
    sqlx::query(
        "INSERT INTO t_outsource_quote (id, part_id, outsource_company_id, process_id, price, \
         status, version, created_at, updated_at) \
         VALUES ($1, $2, $3, $4, 5.00::numeric, 'DRAFT', 0, now(), now())",
    )
    .bind(qid)
    .bind(pid)
    .bind(co)
    .bind(proc_id)
    .execute(&pool)
    .await
    .expect("insert DRAFT quote");
    insert_batch(
        &pool,
        pid,
        1,
        Some(shelf_id),
        "PENDING",
        None,
        Some(proc_id),
        0,
    )
    .await;

    let (s, env) = get_sendable(&app, &token, "").await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(env["data"]["total"], 0, "DRAFT 报价不是已批准报价: {env}");
}

/// 2026-10-03 review 第 1 轮：DIRECT 自动建的 0 元占位报价
/// （`status='APPROVED' AND is_direct=true`）**不满足**审批闸门 → 需审批工序
/// 仍不出行。
///
/// 这是本轮堵掉的真实业务漏洞：某 (part, process) 历史上被 `direct=true` 发过一次
/// （`resolve_direct_quote_id` 在库里留下 `is_direct=true, price=0` 的 APPROVED
/// 占位报价）→ 之后同一 (part, process) 的批次被 EXISTS 闸门命中 → 端点返回
/// `send_mode="APPROVAL"` / `price="0.00"` / `company_options=[]`，用户以为在按
/// 审批价发货，实际用的是一条**从未被人审批过**的 0 元占位报价，shipment 单价落 0。
///
/// 锁住 SQL 层两处谓词（LEFT JOIN 的 `q` 与 WHERE 里 EXISTS 的 `q2`）都带
/// `AND is_direct = false`；少任一处都会让该行以 APPROVAL 身份出现。
#[tokio::test]
async fn sendable_requires_approval_with_direct_placeholder_quote_excluded() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let cid = insert_customer(&pool, "SdApDir", "V").await;
    let pid = insert_part(&pool, cid, "APDIR", false, "2026-12-01").await;
    let proc_id = seed_outsource_process(&pool, "SDAPDIR", true).await;
    let shelf_id = insert_shelf(&pool, "SV1").await;
    let co = insert_company(&pool, "ApDirCo", true).await;
    link_company_process(&pool, co, proc_id).await;
    // DIRECT 占位报价的**逐字形状**（照 `resolve_direct_quote_id` 的 INSERT 抄）：
    // status=APPROVED + price=0 + is_direct=true + note 标明 DIRECT 来源
    let placeholder = SnowflakeIdGenerator::new(1_577_836_800_000, 1).next_id();
    sqlx::query(
        "INSERT INTO t_outsource_quote \
         (id, part_id, outsource_company_id, process_id, price, note, status, \
          submitted_at, reviewed_at, is_direct, version, created_at, updated_at) \
         VALUES ($1, $2, $3, $4, 0, 'DIRECT 直发自动创建（免审批，单价待对账补录）', \
                 'APPROVED', now(), now(), true, 0, now(), now())",
    )
    .bind(placeholder)
    .bind(pid)
    .bind(co)
    .bind(proc_id)
    .execute(&pool)
    .await
    .expect("insert DIRECT 占位报价");
    insert_batch(
        &pool,
        pid,
        1,
        Some(shelf_id),
        "PENDING",
        None,
        Some(proc_id),
        0,
    )
    .await;

    let (s, env) = get_sendable(&app, &token, "").await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(
        env["data"]["total"], 0,
        "is_direct=true 的占位报价不是真实审批报价，必须不出现: {env}"
    );
    assert_eq!(env["data"]["items"].as_array().unwrap().len(), 0, "{env}");
}

/// 2026-10-03 review 第 1 轮：同一 (part, process) 上**既有** `is_direct=true` 的
/// 占位报价、**又有**一条真实审批报价时，出行且回传的是后者（`is_direct=false`），
/// 价不是 0。
///
/// 上一条锁「只有占位报价 ⇒ 不出行」，本条锁「不能把判据写成 `NOT EXISTS(占位)`
/// 之类的反向排除」—— 加 `is_direct = false` 是**收窄命中集**（取真实审批报价），
/// 不是「有占位就整行剔除」。两条一起把谓词的语义钉死。
#[tokio::test]
async fn sendable_requires_approval_prefers_real_quote_over_direct_placeholder() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let cid = insert_customer(&pool, "SdApBoth", "Y").await;
    let pid = insert_part(&pool, cid, "APBOTH", false, "2026-12-01").await;
    let proc_id = seed_outsource_process(&pool, "SDAPBOTH", true).await;
    let shelf_id = insert_shelf(&pool, "SY1").await;
    let co = insert_company(&pool, "ApBothCo", true).await;
    link_company_process(&pool, co, proc_id).await;
    // 占位报价先建（id 更小），真实审批报价后建 —— 若谓词漏了 `is_direct = false`，
    // `DISTINCT ON … quote_id ASC` 会挑中这条 0 元占位报价，正好被断言抓住
    let placeholder = SnowflakeIdGenerator::new(1_577_836_800_000, 1).next_id();
    sqlx::query(
        "INSERT INTO t_outsource_quote \
         (id, part_id, outsource_company_id, process_id, price, note, status, \
          submitted_at, reviewed_at, is_direct, version, created_at, updated_at) \
         VALUES ($1, $2, $3, $4, 0, 'DIRECT 直发自动创建（免审批，单价待对账补录）', \
                 'APPROVED', now(), now(), true, 0, now(), now())",
    )
    .bind(placeholder)
    .bind(pid)
    .bind(co)
    .bind(proc_id)
    .execute(&pool)
    .await
    .expect("insert DIRECT 占位报价");
    let real = insert_approved_quote(&pool, pid, co, proc_id, "77.70").await;
    insert_batch(
        &pool,
        pid,
        1,
        Some(shelf_id),
        "PENDING",
        None,
        Some(proc_id),
        0,
    )
    .await;

    let (s, env) = get_sendable(&app, &token, "").await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(env["data"]["total"], 1, "{env}");
    let row = &env["data"]["items"][0];
    assert_eq!(row["send_mode"], "APPROVAL", "{env}");
    assert_eq!(
        row["quote_id"].as_str().unwrap(),
        real.to_string(),
        "必须回传真实验审批报价而不是 0 元占位报价: {env}"
    );
    assert_eq!(row["price"].as_str().unwrap(), "77.70", "{env}");
    assert_eq!(
        row["company_options"].as_array().unwrap().len(),
        0,
        "APPROVAL 行的 company_options 恒空: {env}"
    );
}

#[tokio::test]
async fn sendable_direct_row_kept_when_no_active_company() {
    // DIRECT 且该工序没映射任何活跃公司 → 行仍返回（前端 canSend() 置灰），
    // 但 total 也要把它算进去，否则分页对不上
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let cid = insert_customer(&pool, "SdBare", "L").await;
    let pid = insert_part(&pool, cid, "BARE", false, "2026-12-01").await;
    let proc_id = seed_outsource_process(&pool, "SDBR", false).await;
    let shelf_id = insert_shelf(&pool, "SC1").await;
    insert_batch(
        &pool,
        pid,
        1,
        Some(shelf_id),
        "PENDING",
        None,
        Some(proc_id),
        0,
    )
    .await;

    let (s, env) = get_sendable(&app, &token, "").await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(
        env["data"]["total"], 1,
        "空 options 行也必须计入 total: {env}"
    );
    let row = &env["data"]["items"][0];
    assert_eq!(row["send_mode"], "DIRECT", "{env}");
    assert_eq!(row["company_options"].as_array().unwrap().len(), 0, "{env}");
}

/// 2026-10-03 新增：`PENDING` 且没上架（`current_holder_id IS NULL`）的批次仍
/// 出行，`shelf_code` 为 `null`。
///
/// 锁住 `t_shelf` 由 INNER JOIN 降级为 LEFT JOIN —— 生产库里 PENDING 批次的
/// holder 全为 NULL，保持 INNER JOIN 会让「还没下发」的零件整批从列表消失。
#[tokio::test]
async fn sendable_pending_without_holder_has_null_shelf_code() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let cid = insert_customer(&pool, "SdNoHold", "S").await;
    let pid = insert_part(&pool, cid, "NOHOLD", false, "2026-12-01").await;
    let proc_id = seed_outsource_process(&pool, "SDNH", false).await;
    // 直接派工：PENDING + 无 holder + 已定位外协工序
    insert_batch(&pool, pid, 1, None, "PENDING", None, Some(proc_id), 0).await;

    let (s, env) = get_sendable(&app, &token, "").await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(
        env["data"]["total"], 1,
        "无 holder 的 PENDING 批次应出行: {env}"
    );
    let row = &env["data"]["items"][0];
    assert!(
        row["shelf_code"].is_null(),
        "无 holder 时 shelf_code 为 null: {env}"
    );
    assert_eq!(row["current_process_id"], proc_id.to_string(), "{env}");
}

/// 2026-10-03 新增：`current_process_id` 指向**非 OUTSOURCE** 工序 → 不出行。
#[tokio::test]
async fn sendable_excludes_non_outsource_current_process() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let cid = insert_customer(&pool, "SdInh", "T").await;
    let pid = insert_part(&pool, cid, "INHOUSE", false, "2026-12-01").await;
    let inhouse = seed_inhouse_process(&pool, "SDINH").await;
    let shelf_id = insert_shelf(&pool, "SI9").await;
    insert_batch(
        &pool,
        pid,
        1,
        Some(shelf_id),
        "IN_PROCESS",
        Some("PRODUCTION_SHELF"),
        Some(inhouse),
        0,
    )
    .await;

    let (s, env) = get_sendable(&app, &token, "").await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(
        env["data"]["total"], 0,
        "current_process_id 指向 INHOUSE 工序必须不出现: {env}"
    );
}

/// 2026-10-03 新增：`current_process_id` 为 NULL（PENDING 未派工）→ 不出行。
#[tokio::test]
async fn sendable_excludes_batch_without_current_process() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let cid = insert_customer(&pool, "SdNoProc", "U").await;
    let pid = insert_part(&pool, cid, "NOPROC", false, "2026-12-01").await;
    let shelf_id = insert_shelf(&pool, "SJ1").await;
    insert_batch(&pool, pid, 1, Some(shelf_id), "PENDING", None, None, 0).await;

    let (s, env) = get_sendable(&app, &token, "").await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(env["data"]["total"], 0, "未定位工序的批次必须不出现: {env}");
}

#[tokio::test]
async fn sendable_source_status_and_batch_version() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let cid = insert_customer(&pool, "SdSt", "M").await;
    let proc_id = seed_outsource_process(&pool, "SDST", false).await;
    let shelf_id = insert_shelf(&pool, "SD1").await;

    // PENDING
    let p_pending = insert_part(&pool, cid, "PEND", false, "2026-12-01").await;
    let b_pending = insert_batch(
        &pool,
        p_pending,
        1,
        Some(shelf_id),
        "PENDING",
        None,
        Some(proc_id),
        11,
    )
    .await;

    // IN_PROCESS + PRODUCTION_SHELF
    let p_inproc = insert_part(&pool, cid, "INPC", false, "2026-12-02").await;
    let b_inproc = insert_batch(
        &pool,
        p_inproc,
        1,
        Some(shelf_id),
        "IN_PROCESS",
        Some("PRODUCTION_SHELF"),
        Some(proc_id),
        22,
    )
    .await;

    // IN_PROCESS 但 location=WORKER → 不该出现
    let p_worker = insert_part(&pool, cid, "WRKR", false, "2026-12-03").await;
    insert_batch(
        &pool,
        p_worker,
        1,
        Some(shelf_id),
        "IN_PROCESS",
        Some("WORKER"),
        Some(proc_id),
        33,
    )
    .await;

    let (s, env) = get_sendable(&app, &token, "").await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(env["data"]["total"], 2, "WORKER 上的批次不应出现: {env}");
    let items = env["data"]["items"].as_array().unwrap();
    // 雪花主键在 JSON 里是字符串，比较前先转字符串键
    let by_batch = |b: i64| {
        let key = b.to_string();
        items
            .iter()
            .find(|i| i["batch_id"].as_str() == Some(key.as_str()))
            .unwrap_or_else(|| panic!("missing batch {b}: {env}"))
    };
    let row_p = by_batch(b_pending);
    assert_eq!(row_p["source_status"], "PENDING", "{env}");
    assert_eq!(row_p["version"], 11, "version 必须取 batch.version: {env}");
    let row_i = by_batch(b_inproc);
    assert_eq!(row_i["source_status"], "IN_PROCESS", "{env}");
    assert_eq!(row_i["version"], 22, "version 必须取 batch.version: {env}");
}

#[tokio::test]
async fn sendable_customer_id_filter_and_keyword() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let cid_a = insert_customer(&pool, "CustA", "N").await;
    let cid_b = insert_customer(&pool, "CustB", "O").await;
    let proc_id = seed_outsource_process(&pool, "SDCF", false).await;
    let shelf_id = insert_shelf(&pool, "SE1").await;
    for (cid, tag) in [(cid_a, "FJA"), (cid_b, "FJB")] {
        let pid = insert_part(&pool, cid, tag, false, "2026-12-01").await;
        insert_batch(
            &pool,
            pid,
            1,
            Some(shelf_id),
            "PENDING",
            None,
            Some(proc_id),
            0,
        )
        .await;
    }

    let (s, env) = get_sendable(&app, &token, &format!("?customer_id={cid_a}")).await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(env["data"]["total"], 1, "{env}");
    assert!(
        env["data"]["items"][0]["part_drawing_no"]
            .as_str()
            .unwrap()
            .contains("FJA"),
        "{env}"
    );

    let (s, env) = get_sendable(&app, &token, "?keyword=FJB").await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(env["data"]["total"], 1, "{env}");
}

#[tokio::test]
async fn sendable_total_matches_items_and_pagination() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let cid = insert_customer(&pool, "SdPg", "Q").await;
    // 两道工序各管一批：APPROVAL 走需审批 + 报价，DIRECT 走免审批
    let proc_appr = seed_outsource_process(&pool, "SDPG-A", true).await;
    let proc_dir = seed_outsource_process(&pool, "SDPG-D", false).await;
    let shelf_id = insert_shelf(&pool, "SF1").await;
    let co = insert_company(&pool, "PgCo", true).await;
    link_company_process(&pool, co, proc_appr).await;
    link_company_process(&pool, co, proc_dir).await;
    for i in 0..3 {
        let tag = format!("PG{i}");
        let pid = insert_part(&pool, cid, &tag, false, "2026-12-01").await;
        let proc_id = if i == 0 { proc_appr } else { proc_dir };
        insert_batch(
            &pool,
            pid,
            1,
            Some(shelf_id),
            "PENDING",
            None,
            Some(proc_id),
            0,
        )
        .await;
        if i == 0 {
            insert_approved_quote(&pool, pid, co, proc_appr, "9.99").await;
        }
    }

    let (s, env) = get_sendable(&app, &token, "").await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(env["data"]["total"], 3, "{env}");
    assert_eq!(env["data"]["items"].as_array().unwrap().len(), 3, "{env}");
    let modes: Vec<&str> = env["data"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["send_mode"].as_str().unwrap())
        .collect();
    assert_eq!(
        modes.iter().filter(|m| **m == "APPROVAL").count(),
        1,
        "{env}"
    );
    assert_eq!(modes.iter().filter(|m| **m == "DIRECT").count(), 2, "{env}");

    let (s, env) = get_sendable(&app, &token, "?limit=2&offset=2").await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(env["data"]["total"], 3, "{env}");
    assert_eq!(env["data"]["items"].as_array().unwrap().len(), 1, "{env}");
}

#[tokio::test]
async fn sendable_orders_urgent_first_then_planned_delivery() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let cid = insert_customer(&pool, "SdOr", "R").await;
    let proc_id = seed_outsource_process(&pool, "SDOR", false).await;
    let shelf_id = insert_shelf(&pool, "SG1").await;
    // 顺序插入：普通-晚、普通-早、加急-晚 → 期望输出：加急-晚、普通-早、普通-晚
    let a = insert_part(&pool, cid, "NLA", false, "2026-12-30").await;
    let b = insert_part(&pool, cid, "NEB", false, "2026-12-10").await;
    let c = insert_part(&pool, cid, "URG", true, "2026-12-31").await;
    for pid in [a, b, c] {
        insert_batch(
            &pool,
            pid,
            1,
            Some(shelf_id),
            "PENDING",
            None,
            Some(proc_id),
            0,
        )
        .await;
    }

    let (s, env) = get_sendable(&app, &token, "").await;
    assert_eq!(s, StatusCode::OK, "{env}");
    let names: Vec<&str> = env["data"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["part_name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["NAME-URG", "NAME-NEB", "NAME-NLA"], "{env}");
    assert_eq!(env["data"]["items"][0]["is_urgent"], true, "{env}");
}

/// 2026-10-03 新增（取代 `sendable_excludes_process_not_in_part_chain`）：
/// 零件**完全没有** `process_chain_id`（生产库里的常态），批次停在某道外协工序上
/// ⇒ 出行。
///
/// 旧判据要求「该 OUTSOURCE 工序在零件工艺链内」，而生产库 1874 个零件只有 2 个
/// 绑了链、`t_process_chain_step` 里 OUTSOURCE 类 step 有 0 条 ⇒ 交集恒空 ⇒ 端点恒空。
#[tokio::test]
async fn sendable_includes_outsource_process_when_part_has_no_chain() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let cid = insert_customer(&pool, "SdNoChain", "S").await;
    let pid = insert_part(&pool, cid, "NOCHAIN", false, "2026-12-01").await;
    let proc_id = seed_outsource_process(&pool, "SDNOC", false).await;
    let shelf_id = insert_shelf(&pool, "SH1").await;
    let co = insert_company(&pool, "NoChainCo", true).await;
    link_company_process(&pool, co, proc_id).await;
    // 前提断言：part 确实没有链
    let chain: Option<i64> =
        sqlx::query_scalar("SELECT process_chain_id FROM t_part WHERE id = $1")
            .bind(pid)
            .fetch_one(&pool)
            .await
            .expect("read process_chain_id");
    assert!(chain.is_none(), "本用例前提是 part 无工艺链");
    let bid = insert_batch(
        &pool,
        pid,
        1,
        Some(shelf_id),
        "PENDING",
        None,
        Some(proc_id),
        0,
    )
    .await;

    let (s, env) = get_sendable(&app, &token, "").await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(
        env["data"]["total"], 1,
        "无链零件的批次停在外协工序上必须出行: {env}"
    );
    let row = &env["data"]["items"][0];
    assert_eq!(row["batch_id"], bid.to_string(), "{env}");
    assert_eq!(row["current_process_id"], proc_id.to_string(), "{env}");
    assert_eq!(row["send_mode"], "DIRECT", "{env}");
}

/// 2026-10-03 新增：零件**有**链，但链内没有这道外协工序 ⇒ 仍然出行，且货架上
/// 绑的也不是这道工序。
///
/// 这条与上一条一起把「工艺链」和「货架工序映射」两层旧谓词彻底钉死为不参与判定。
#[tokio::test]
async fn sendable_includes_process_outside_part_chain() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let cid = insert_customer(&pool, "SdOutChain", "V").await;
    let pid = insert_part(&pool, cid, "OUTCHAIN", false, "2026-12-01").await;
    let chain_proc = seed_outsource_process(&pool, "SDOC-IN", false).await;
    let cur_proc = seed_outsource_process(&pool, "SDOC-OUT", false).await;
    let shelf_id = insert_shelf(&pool, "SH2").await;
    // 货架绑的是链内那道工序；批次停在另一道外协工序上
    link_shelf_process(&pool, shelf_id, chain_proc).await;
    create_chain_with_step(&pool, pid, chain_proc).await;
    let bid = insert_batch(
        &pool,
        pid,
        1,
        Some(shelf_id),
        "PENDING",
        None,
        Some(cur_proc),
        0,
    )
    .await;

    let (s, env) = get_sendable(&app, &token, "").await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(
        env["data"]["total"], 1,
        "工艺链内没有该工序也必须出行（判据只看 current_process_id）: {env}"
    );
    let row = &env["data"]["items"][0];
    assert_eq!(row["batch_id"], bid.to_string(), "{env}");
    assert_eq!(row["current_process_id"], cur_proc.to_string(), "{env}");
    assert_eq!(row["shelf_code"], "SH2", "{env}");
}

/// 行粒度 = 一批次一行（2026-10-03 起不再有「工序」这一维：同一批次恒对应一道
/// `current_process_id`）。同一零件的多个批次仍各出一行。
#[tokio::test]
async fn sendable_one_row_per_batch_even_with_many_batches() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let cid = insert_customer(&pool, "SdDup", "T").await;
    let pid = insert_part(&pool, cid, "DUPB", false, "2026-12-01").await;
    let proc_id = seed_outsource_process(&pool, "SDDUP", false).await;
    let shelf_id = insert_shelf(&pool, "SK1").await;
    insert_batch(
        &pool,
        pid,
        1,
        Some(shelf_id),
        "PENDING",
        None,
        Some(proc_id),
        0,
    )
    .await;
    insert_batch(
        &pool,
        pid,
        2,
        Some(shelf_id),
        "PENDING",
        None,
        Some(proc_id),
        0,
    )
    .await;

    let (s, env) = get_sendable(&app, &token, "").await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(
        env["data"]["total"], 2,
        "同一零件的两个批次是**两行**（行粒度 = 批次）: {env}"
    );
    let batch_ids: Vec<&str> = env["data"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["batch_id"].as_str().unwrap())
        .collect();
    assert_eq!(batch_ids.len(), 2, "{env}");
    assert_ne!(batch_ids[0], batch_ids[1], "两行必须是不同批次: {env}");
}

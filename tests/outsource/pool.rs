//! `GET /outsource-pool/*` 看板三件套集成测试（2026-10-03 新增）
//!
//! 形态模板：`tests/production/worker_pool.rs`（`/prod/pool/{counts,state,/{process_id}}`）。
//!
//! 覆盖（与本轮验收标准逐条对应）：
//! 1. `pool_counts_returns_200_sorted_and_totals_match`
//! 2. `pool_detail_lists_all_mapped_companies_including_empty_column`
//! 3. `pool_detail_items_match_sendable_endpoint_field_by_field`（**防 SQL 分叉的核心断言**）
//! 4. `pool_detail_keeps_direct_row_with_empty_company_options`
//! 5. `pool_state_returns_held_batches_with_shipment_fields`
//! 6. `pool_state_chain_resolvable_when_next_step_exists`
//! 7. `pool_state_chain_unresolvable_when_no_step_or_chain_tail`
//! 8. `pool_state_rejects_missing_query_params_with_400`
//! 9. `pool_serializes_snowflake_ids_and_price_as_strings`
//! 10. `pool_counts_and_state_are_static_routes_not_process_id`（注册顺序守卫）
//! 11. 权限：`pool_counts_allows_clerk_role`（正向）+ `pool_counts_forbidden_for_shelf_account`
//!     / `pool_by_process_forbidden_for_shelf_account` / `pool_state_forbidden_for_shelf_account`
//!     （三个端点各一条负向回归网）
//! 12. `pool_state_derives_next_step_from_parts_current_chain_after_rebind`（读侧锚链与写侧同源）
//!
//! ## 候选侧（counts.sendable_count / {process_id}.items）的新判据
//! 2026-10-03 起候选批次由 `t_part_batch.current_process_id` 判定（必须指向一道
//! OUTSOURCE 工序），**不再要求零件绑了工艺链**；且该工序 `requires_approval = true`
//! 时必须已有 APPROVED 报价。故本文件的两个 helper 都加了对应形参：
//! `seed_outsource_process(.., requires_approval)` 与
//! `insert_candidate_batch(.., process_id, ..)`。
//! 13. `pool_state_does_not_fan_out_on_duplicate_applicant_name`（`t_applicant` 重名不扇出）
//!
//! ## fixture 范本
//! 通用基建（`send` / `json_request` / `test_app` / `test_pool` / `test_state` /
//! `login_token` / `load_outsource_fixture`）全部走 `hsh_erp_test_support`，
//! **不重复声明**。本文件只声明 pool 域独享的数据构造 helper（客户 / 零件 /
//! 工序 / 货架 / 工艺链多 step / 批次 / 公司 / 报价 / shipment）—— 这些每个用例
//! 都要按需造不同组合，fixture 预置会污染 counts 的「只含非零工序」断言。

use axum::http::StatusCode;
use serde_json::{Value, json};
use sqlx::PgPool;

use hsh_erp_rust::infra::clock::now_naive;
use hsh_erp_test_support::{
    OutsourceFixture, json_request, load_outsource_fixture, login_token, pool_snowflake, send,
    test_app, test_pool, test_state,
};

// ===========================================================================
//  Bootstrap（PR13 Phase H 风格）
// ===========================================================================

async fn bootstrap_as_manager() -> (PgPool, axum::Router, String, OutsourceFixture) {
    let pool = test_pool().await;
    let fx = load_outsource_fixture(&pool).await;
    let app = test_app(test_state(pool.clone()).await);
    let token = login_token(&app, &fx.part_manager_username, OutsourceFixture::PASSWORD).await;
    (pool, app, token, fx)
}

async fn bootstrap_as_clerk() -> (PgPool, axum::Router, String, OutsourceFixture) {
    let pool = test_pool().await;
    let fx = load_outsource_fixture(&pool).await;
    let app = test_app(test_state(pool.clone()).await);
    let token = login_token(&app, &fx.clerk_username, OutsourceFixture::PASSWORD).await;
    (pool, app, token, fx)
}

/// 插一个 `is_active=true` 的 `t_user` 行（bcrypt 哈希现场生成）。
async fn insert_user_with_password(pool: &PgPool, username: &str, plain_password: &str) -> i64 {
    use hsh_erp_rust::auth::password;

    let hash = password::hash(plain_password).expect("bcrypt hash");
    let id = next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_user (id, username, password_hash, full_name, is_active, \
         refresh_token_version, version, created_at, updated_at) \
         VALUES ($1, $2, $3, $4, true, 0, 0, $5, $5)",
    )
    .bind(id)
    .bind(username.to_lowercase())
    .bind(hash)
    .bind(username)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_user");
    id
}

/// 插一个 `t_user_role` 行（user_id + role + scope）。
async fn add_role(
    pool: &PgPool,
    user_id: i64,
    role: &str,
    scope_type: Option<&str>,
    scope_id: Option<i64>,
) -> i64 {
    let id = next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_user_role (id, user_id, role, scope_type, scope_id, version, \
         created_at, updated_at) VALUES ($1, $2, $3, $4, $5, 0, $6, $6)",
    )
    .bind(id)
    .bind(user_id)
    .bind(role)
    .bind(scope_type)
    .bind(scope_id)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_user_role");
    id
}

/// SHELF scope 账号登录（scope 限定到给定 shelves；必须给 scope 才能通过登录校验）。
///
/// pool 域独享：`OutsourceFixture` 的 shelf scope 绑的是 fixture 预置货架，
/// 负向用例要的是「只认货架、看不到全厂」的账号，本地 helper 便于按用例限定。
async fn login_shelf_account(
    pool: PgPool,
    username: &str,
    shelves: &[i64],
) -> (axum::Router, String) {
    let uid = insert_user_with_password(&pool, username, "changeme").await;
    for sid in shelves {
        add_role(&pool, uid, "SHELF_ACCOUNT", Some("shelf"), Some(*sid)).await;
    }
    let state = test_state(pool.clone()).await;
    let req = json_request(
        "POST",
        "/iam/login",
        Some(json!({"username": username, "password": "changeme"})),
        None,
    );
    let (_, env) = send(test_app(state.clone()), req).await;
    let token = env["data"]["token"].as_str().unwrap().to_string();
    (test_app(state), token)
}

// ===========================================================================
//  pool 域独享 helpers（直插 `sqlx::query`；与 sendable.rs / send_receive.rs 同形）
// ===========================================================================

/// 取一个测试用雪花 ID。
///
/// 走 `test-support::pool_snowflake()`（**进程级** `OnceLock<Mutex<..>>`，instance
/// 由 pid ⊕ 启动时间派生）而不是每次 `SnowflakeIdGenerator::new(...)` 新建 ——
/// 新建的话同一毫秒内连续两次调用会生成**完全相同**的 id（`instance=1` + 同一
/// 时间戳 + seq 都从 0 开始），撞 `t_*_pkey`。本文件建公司的间隔只有一次
/// await，实测能在同一毫秒内跑到两次。
fn next_id() -> i64 {
    pool_snowflake().lock().expect("pool_snowflake").next_id()
}

/// 直插 L1 客户。
async fn insert_customer(pool: &PgPool, name: &str, prefix: &str) -> i64 {
    let id = next_id();
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

/// 直插 part（`status='PENDING'`，`planned_delivery_date` 可指定）。
async fn insert_part(pool: &PgPool, customer_id: i64, tag: &str, planned: &str) -> i64 {
    let id = next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_part (id, name, drawing_no, applicant_name, quantity, unit_price, \
         total_price, request_date, planned_delivery_date, customer_id, is_urgent, status, \
         version, created_at, updated_at) \
         VALUES ($1, $2, $3, 'Tester', 8, 1.00, 8.00, CURRENT_DATE, $4::date, $5, false, \
                 'PENDING', 0, $6, $6)",
    )
    .bind(id)
    .bind(format!("NAME-{tag}"))
    .bind(format!("DWG-{tag}-{id}"))
    .bind(planned)
    .bind(customer_id)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_part");
    id
}

/// 直插申请人（`t_applicant` 唯一索引是 `(name, customer_id)`，name 单独可重名）。
async fn insert_applicant(pool: &PgPool, name: &str, customer_id: i64) -> i64 {
    let id = next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_applicant (id, name, customer_id, version, created_at, updated_at) \
         VALUES ($1, $2, $3, 0, $4, $4)",
    )
    .bind(id)
    .bind(name)
    .bind(customer_id)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_applicant");
    id
}

/// 改 part 的申请人姓名（`insert_part` 固定写 `'Tester'`）。
async fn set_part_applicant(pool: &PgPool, part_id: i64, applicant_name: &str) {
    sqlx::query("UPDATE t_part SET applicant_name = $1 WHERE id = $2")
        .bind(applicant_name)
        .bind(part_id)
        .execute(pool)
        .await
        .expect("update t_part.applicant_name");
}

/// 直插 OUTSOURCE 类别工序，返回 `(id, code, name)`。
///
/// `requires_approval` 是候选侧谓词的输入（true = 必须有已批准报价才出现在
/// 候选列 / sendable 一览；false = 免审批直发），故由用例显式给出。
async fn seed_outsource_process(
    pool: &PgPool,
    code: &str,
    requires_approval: bool,
) -> (i64, String, String) {
    let id = next_id();
    let name = format!("PROC-{code}");
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_process (id, code, name, category, sort_order, requires_approval, \
         version, created_at, updated_at) \
         VALUES ($1, $2, $3, 'OUTSOURCE', 0, $4, 0, $5, $5)",
    )
    .bind(id)
    .bind(code)
    .bind(&name)
    .bind(requires_approval)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_process");
    (id, code.to_string(), name)
}

/// 直插 INHOUSE 类别工序（用作「外协之后回到厂内的下一道工序」）。
async fn seed_inhouse_process(pool: &PgPool, code: &str) -> (i64, String, String) {
    let id = next_id();
    let name = format!("PROC-{code}");
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_process (id, code, name, category, sort_order, requires_approval, \
         version, created_at, updated_at) \
         VALUES ($1, $2, $3, 'INHOUSE', 0, false, 0, $4, $4)",
    )
    .bind(id)
    .bind(code)
    .bind(&name)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_process");
    (id, code.to_string(), name)
}

async fn insert_shelf(pool: &PgPool, code: &str) -> i64 {
    let id = next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_shelf (id, code, name, zone, is_active, display_order, version, \
         created_at, updated_at) VALUES ($1, $2, $3, 'PRODUCTION', true, 0, 0, $4, $4)",
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
    sqlx::query(
        "INSERT INTO t_shelf_process (id, shelf_id, process_id, sort_order, version, \
         created_at, updated_at) VALUES ($1, $2, $3, 0, 0, now(), now())",
    )
    .bind(next_id())
    .bind(shelf_id)
    .bind(process_id)
    .execute(pool)
    .await
    .expect("insert t_shelf_process");
}

/// 建一条空的工艺链（**不**绑到任何 part）。
async fn create_chain(pool: &PgPool, name: &str) -> i64 {
    let chain_id = next_id();
    sqlx::query(
        "INSERT INTO t_part_process_chain (id, name, version, created_at, created_by, \
         updated_at, updated_by) VALUES ($1, $2, 0, now(), 0, now(), 0)",
    )
    .bind(chain_id)
    .bind(name)
    .execute(pool)
    .await
    .expect("insert t_part_process_chain");
    chain_id
}

/// 往 `chain_id` 追加一个 step，返回 step_id。
async fn add_chain_step(pool: &PgPool, chain_id: i64, process_id: i64, sort_order: i32) -> i64 {
    let step_id = next_id();
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
    .expect("insert t_process_chain_step");
    step_id
}

/// 把 part 绑到指定工艺链（改绑用）。
async fn bind_part_to_chain(pool: &PgPool, part_id: i64, chain_id: i64) {
    sqlx::query("UPDATE t_part SET process_chain_id = $1 WHERE id = $2")
        .bind(chain_id)
        .bind(part_id)
        .execute(pool)
        .await
        .expect("bind part to chain");
}

/// 建工艺链并按 `steps`（`[(process_id, sort_order)]`）建 step。
///
/// 返回 `(chain_id, Vec<step_id>)`（下标与 `steps` 下标一一对应）。
/// `sort_order` 显式传，是为了让「链尾 / 有下一 step」两条派生分支可控。
async fn create_chain_with_steps(
    pool: &PgPool,
    part_id: i64,
    steps: &[(i64, i32)],
) -> (i64, Vec<i64>) {
    let chain_id = create_chain(pool, &format!("chain-{part_id}")).await;
    bind_part_to_chain(pool, part_id, chain_id).await;

    let mut step_ids = Vec::with_capacity(steps.len());
    for (process_id, sort_order) in steps {
        step_ids.push(add_chain_step(pool, chain_id, *process_id, *sort_order).await);
    }
    (chain_id, step_ids)
}

/// 直插候选批次（在架上等发外协）：`status='PENDING'` + `location='PRODUCTION_SHELF'`
/// + `current_process_id = process_id`。
///
/// `current_process_id` 是候选侧判定的权威依据（2026-10-03 起不再靠「货架绑了哪些
/// 外协工序」枚举），故必须显式给出。
async fn insert_candidate_batch(
    pool: &PgPool,
    part_id: i64,
    shelf_id: i64,
    process_id: i64,
    version: i32,
) -> i64 {
    let id = next_id();
    sqlx::query(
        "INSERT INTO t_part_batch (id, part_id, batch_no, quantity, status, location, \
         current_holder_id, current_process_id, version, created_at, updated_at) \
         VALUES ($1, $2, 1, 8, 'PENDING', 'PRODUCTION_SHELF', $3, $4, $5, now(), now())",
    )
    .bind(id)
    .bind(part_id)
    .bind(shelf_id)
    .bind(process_id)
    .bind(version)
    .execute(pool)
    .await
    .expect("insert t_part_batch (candidate)");
    id
}

/// 直插在外协的批次：`status='OUTSOURCE'` + `location='OUTSOURCE_COMPANY'` +
/// `current_holder_id = company_id` + `current_process_id = process_id`。
///
/// 这 4 列正是 `prod::batch::service::outsource.rs::send_to_outsource` 落的形状
/// （见该文件 `mark_batch_with_status_and_meta(...)` 调用）。
async fn insert_held_batch(
    pool: &PgPool,
    part_id: i64,
    company_id: i64,
    process_id: i64,
    step_id: Option<i64>,
    quantity: i32,
    version: i32,
) -> i64 {
    let id = next_id();
    sqlx::query(
        "INSERT INTO t_part_batch (id, part_id, batch_no, quantity, status, location, \
         current_holder_id, current_process_id, current_process_step_id, version, \
         created_at, updated_at) \
         VALUES ($1, $2, 1, $3, 'OUTSOURCE', 'OUTSOURCE_COMPANY', $4, $5, $6, $7, now(), now())",
    )
    .bind(id)
    .bind(part_id)
    .bind(quantity)
    .bind(company_id)
    .bind(process_id)
    .bind(step_id)
    .bind(version)
    .execute(pool)
    .await
    .expect("insert t_part_batch (held)");
    id
}

async fn insert_company(pool: &PgPool, name: &str, is_active: bool) -> i64 {
    let id = next_id();
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
    sqlx::query(
        "INSERT INTO t_outsource_company_process (id, outsource_company_id, process_id, \
         sort_order, version, created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, $3, 0, 0, now(), 0, now(), 0)",
    )
    .bind(next_id())
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
    let id = next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_outsource_quote (id, part_id, outsource_company_id, process_id, price, \
         status, submitted_at, reviewed_at, version, created_at, updated_at) \
         VALUES ($1, $2, $3, $4, $5::numeric, 'APPROVED', $6, $6, 0, $6, $6)",
    )
    .bind(id)
    .bind(part_id)
    .bind(company_id)
    .bind(process_id)
    .bind(price)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_outsource_quote");
    id
}

#[allow(clippy::too_many_arguments)]
async fn insert_open_shipment(
    pool: &PgPool,
    quote_id: i64,
    part_id: i64,
    batch_id: i64,
    company_id: i64,
    process_id: i64,
    quantity: i32,
    price: &str,
) -> i64 {
    let id = next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_outsource_shipment (id, quote_id, part_id, batch_id, \
         outsource_company_id, process_id, quantity, unit_price, status, sent_at, version, \
         created_at, updated_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8::numeric, 'OUTSOURCING', $9, 0, $9, $9)",
    )
    .bind(id)
    .bind(quote_id)
    .bind(part_id)
    .bind(batch_id)
    .bind(company_id)
    .bind(process_id)
    .bind(quantity)
    .bind(price)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_outsource_shipment");
    id
}

// ===========================================================================
//  HTTP helpers
// ===========================================================================

async fn get(app: &axum::Router, token: &str, uri: &str) -> (StatusCode, Value) {
    send(
        app.clone(),
        json_request("GET", uri, None::<Value>, Some(token)),
    )
    .await
}

async fn get_counts(app: &axum::Router, token: &str) -> (StatusCode, Value) {
    get(app, token, "/outsource-pool/counts").await
}

async fn get_detail(app: &axum::Router, token: &str, process_id: i64) -> (StatusCode, Value) {
    get(app, token, &format!("/outsource-pool/{process_id}")).await
}

async fn get_state(
    app: &axum::Router,
    token: &str,
    company_id: i64,
    process_id: i64,
) -> (StatusCode, Value) {
    get(
        app,
        token,
        &format!("/outsource-pool/state?outsource_company_id={company_id}&process_id={process_id}"),
    )
    .await
}

async fn get_sendable(app: &axum::Router, token: &str) -> (StatusCode, Value) {
    get(app, token, "/outsource-sendable?limit=200").await
}

/// 只取状态码 + 原始 body（不解析 JSON）。
///
/// axum 的 `QueryRejection`（query 反序列化失败）返回的是**纯文本** 400，不走
/// `R<T>` 信封 —— 与 `docs/api/inconsistencies.md` § 9.2 记的「旧路径返回纯文本
/// 400」是同一个 axum 层行为。因此断言「缺参数必须 400」不能用共享的 `send`
/// （它强制解析 JSON，非 JSON body 会 panic）。
async fn get_raw(app: &axum::Router, token: &str, uri: &str) -> (StatusCode, String) {
    use tower::ServiceExt;
    let response = app
        .clone()
        .oneshot(json_request("GET", uri, None::<Value>, Some(token)))
        .await
        .expect("oneshot");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read body");
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

/// 从 `items[]` 里按 `batch_id` 找一行（雪花 ID 在 JSON 里是字符串）。
fn row_by_batch<'a>(items: &'a [Value], batch_id: i64, env: &Value) -> &'a Value {
    let key = batch_id.to_string();
    items
        .iter()
        .find(|i| i["batch_id"].as_str() == Some(key.as_str()))
        .unwrap_or_else(|| panic!("items 缺 batch {batch_id}: {env}"))
}

// ===========================================================================
//  1. counts
// ===========================================================================

#[tokio::test]
async fn pool_counts_returns_200_sorted_and_totals_match() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let cid = insert_customer(&pool, "PcCnt", "A").await;

    // 工序 A：既有候选、又有在途。
    let (proc_a, _, _) = seed_outsource_process(&pool, "PC-A", true).await;
    let shelf_a = insert_shelf(&pool, "PCS-A").await;
    link_shelf_process(&pool, shelf_a, proc_a).await;
    let co_a = insert_company(&pool, "PcCntCoA", true).await;
    link_company_process(&pool, co_a, proc_a).await;
    let p_a1 = insert_part(&pool, cid, "CA1", "2026-12-01").await;
    create_chain_with_steps(&pool, p_a1, &[(proc_a, 1)]).await;
    insert_candidate_batch(&pool, p_a1, shelf_a, proc_a, 0).await;
    let q_a = insert_approved_quote(&pool, p_a1, co_a, proc_a, "12.50").await;
    let p_a2 = insert_part(&pool, cid, "CA2", "2026-12-02").await;
    create_chain_with_steps(&pool, p_a2, &[(proc_a, 1)]).await;
    let held_a = insert_held_batch(&pool, p_a2, co_a, proc_a, None, 8, 3).await;
    insert_open_shipment(&pool, q_a, p_a2, held_a, co_a, proc_a, 8, "12.50").await;

    // 工序 B：只有候选（免审批直发，无报价）。
    // ⚠️ 必须比 proc_a 大，才能验证 `process_id ASC` 而不是插入序。
    let (proc_b, _, _) = seed_outsource_process(&pool, "PC-B", false).await;
    let shelf_b = insert_shelf(&pool, "PCS-B").await;
    link_shelf_process(&pool, shelf_b, proc_b).await;
    let p_b = insert_part(&pool, cid, "CB1", "2026-12-03").await;
    create_chain_with_steps(&pool, p_b, &[(proc_b, 1)]).await;
    insert_candidate_batch(&pool, p_b, shelf_b, proc_b, 0).await;

    // 工序 C：只有在途。
    let (proc_c, _, _) = seed_outsource_process(&pool, "PC-C", true).await;
    let co_c = insert_company(&pool, "PcCntCoC", true).await;
    link_company_process(&pool, co_c, proc_c).await;
    let p_c = insert_part(&pool, cid, "CC1", "2026-12-04").await;
    create_chain_with_steps(&pool, p_c, &[(proc_c, 1)]).await;
    let q_c = insert_approved_quote(&pool, p_c, co_c, proc_c, "3.00").await;
    let held_c = insert_held_batch(&pool, p_c, co_c, proc_c, None, 5, 1).await;
    insert_open_shipment(&pool, q_c, p_c, held_c, co_c, proc_c, 5, "3.00").await;

    // 工序 D：映射了公司但一个批次都没有 → **不得出现**（0+0）。
    let (proc_d, _, _) = seed_outsource_process(&pool, "PC-D", true).await;
    let co_d = insert_company(&pool, "PcCntCoD", true).await;
    link_company_process(&pool, co_d, proc_d).await;

    let (s, env) = get_counts(&app, &token).await;
    assert_eq!(s, StatusCode::OK, "{env}");
    let data = &env["data"];
    let counts = data["counts"].as_array().unwrap();
    let ids: Vec<String> = counts
        .iter()
        .map(|c| c["process_id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        ids,
        vec![proc_a.to_string(), proc_b.to_string(), proc_c.to_string()],
        "counts 只含非零工序且按 process_id ASC: {env}"
    );
    assert!(
        !ids.contains(&proc_d.to_string()),
        "0 可发 + 0 在途的工序不得出现: {env}"
    );

    let row_of = |pid: i64| {
        let key = pid.to_string();
        counts
            .iter()
            .find(|c| c["process_id"].as_str() == Some(key.as_str()))
            .unwrap_or_else(|| panic!("counts 缺工序 {pid}: {env}"))
    };
    let a = row_of(proc_a);
    assert_eq!(a["sendable_count"], 1, "{env}");
    assert_eq!(a["in_flight_count"], 1, "{env}");
    assert_eq!(a["process_code"], "PC-A", "{env}");
    assert_eq!(a["process_name"], "PROC-PC-A", "{env}");
    assert_eq!(row_of(proc_b)["sendable_count"], 1, "{env}");
    assert_eq!(row_of(proc_b)["in_flight_count"], 0, "{env}");
    assert_eq!(row_of(proc_c)["sendable_count"], 0, "{env}");
    assert_eq!(row_of(proc_c)["in_flight_count"], 1, "{env}");

    assert_eq!(data["sendable_total"], 2, "{env}");
    assert_eq!(data["in_flight_total"], 2, "{env}");
    assert_eq!(data["total"], 4, "{env}");
    assert_eq!(
        data["total"].as_i64().unwrap(),
        data["sendable_total"].as_i64().unwrap() + data["in_flight_total"].as_i64().unwrap(),
        "total 必须等于两项之和: {env}"
    );
}

/// 权限口径：`counts` / `state` 是 Manager + Clerk（`state` 与同域
/// `/outsource-shipments/in-flight` 对齐；`counts` 另含 Inspector），
/// CLERK 都在集合内。
#[tokio::test]
async fn pool_counts_allows_clerk_role() {
    let (_pool, app, token, _fx) = bootstrap_as_clerk().await;
    let (s, env) = get_counts(&app, &token).await;
    assert_eq!(s, StatusCode::OK, "CLERK 应可读 counts: {env}");
    let (s, env) = get_state(
        &app,
        &token,
        9_000_000_000_000_000_101,
        9_000_000_000_000_000_100,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "CLERK 应可读 state: {env}");
}

/// 权限负向回归：SHELF scope 账号被 `counts` 拒（403）。
///
/// 只测正向的话，将来有人删掉 `pool_counts` 里的 `require_any_role` 这条用例
/// 仍然全绿。口径对齐 `tests/production/worker_pool.rs` 同名用例。
#[tokio::test]
async fn pool_counts_forbidden_for_shelf_account() {
    let (pool, _app, _token, _fx) = bootstrap_as_manager().await;
    let (proc_id, _, _) = seed_outsource_process(&pool, "PC-FB", true).await;
    let shelf = insert_shelf(&pool, "PCS-FB").await;
    link_shelf_process(&pool, shelf, proc_id).await;
    let co = insert_company(&pool, "FbCo", true).await;
    link_company_process(&pool, co, proc_id).await;

    // scope 必须给才能登录；给了也仍然被 service 的 role 守卫拒。
    let (app, token) = login_shelf_account(pool.clone(), "shelf_user_pool_counts", &[shelf]).await;

    let (s, env) = get_counts(&app, &token).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "ShelfAccount 应 403: {env}");
    assert_eq!(env["code"], 40300, "FORBIDDEN: {env}");
}

/// 权限负向回归：SHELF scope 账号被 `{process_id}` 拒（403）。
#[tokio::test]
async fn pool_by_process_forbidden_for_shelf_account() {
    let (pool, _app, _token, _fx) = bootstrap_as_manager().await;
    let (proc_id, _, _) = seed_outsource_process(&pool, "PBP-FB", true).await;
    let shelf = insert_shelf(&pool, "PBPS-FB").await;
    link_shelf_process(&pool, shelf, proc_id).await;

    let (app, token) = login_shelf_account(pool.clone(), "shelf_user_pool_detail", &[shelf]).await;

    let (s, env) = get_detail(&app, &token, proc_id).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "ShelfAccount 应 403: {env}");
    assert_eq!(env["code"], 40300, "FORBIDDEN: {env}");
}

/// 权限负向回归：SHELF scope 账号被 `state` 拒（403）。
///
/// `state` 吐 `unit_price` + 客户名 / 申请人名，是外协域的敏感读面 ——
/// 若守卫被放宽回「已登录即可读」，SHELF 账号就能枚举任意外协公司的在外协批次
/// 与单价，而 `counts` / `{process_id}` 对它是 403。这条用例就是那道防线。
#[tokio::test]
async fn pool_state_forbidden_for_shelf_account() {
    let (pool, _app, _token, _fx) = bootstrap_as_manager().await;
    let (proc_id, _, _) = seed_outsource_process(&pool, "PST-FB", true).await;
    let shelf = insert_shelf(&pool, "PSTS-FB").await;
    let co = insert_company(&pool, "StFbCo", true).await;
    link_company_process(&pool, co, proc_id).await;

    let (app, token) = login_shelf_account(pool.clone(), "shelf_user_pool_state", &[shelf]).await;

    let (s, env) = get_state(&app, &token, co, proc_id).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "ShelfAccount 应 403: {env}");
    assert_eq!(env["code"], 40300, "FORBIDDEN: {env}");
}

// ===========================================================================
//  2 + 4 + 10. detail：companies / items / DIRECT 空 options / 路由顺序
// ===========================================================================

#[tokio::test]
async fn pool_detail_lists_all_mapped_companies_including_empty_column() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let cid = insert_customer(&pool, "PcDet", "B").await;
    let (proc_id, code, name) = seed_outsource_process(&pool, "PD-CO", true).await;
    let shelf_id = insert_shelf(&pool, "PDS").await;
    link_shelf_process(&pool, shelf_id, proc_id).await;

    // 3 家活跃公司 + 1 家停用（不得出现）。
    let co1 = insert_company(&pool, "PdCo1", true).await;
    let co2 = insert_company(&pool, "PdCo2", true).await;
    let co3 = insert_company(&pool, "PdCo3", true).await;
    let co_off = insert_company(&pool, "PdCoOff", false).await;
    for c in [co1, co2, co3, co_off] {
        link_company_process(&pool, c, proc_id).await;
    }

    // 另一道工序的候选行 —— detail 不得把它带进来。
    let (other_proc, _, _) = seed_outsource_process(&pool, "PD-OTHER", false).await;
    let other_shelf = insert_shelf(&pool, "PDS-OTHER").await;
    link_shelf_process(&pool, other_shelf, other_proc).await;
    let p_other = insert_part(&pool, cid, "OTH", "2026-12-01").await;
    create_chain_with_steps(&pool, p_other, &[(other_proc, 1)]).await;
    let other_batch = insert_candidate_batch(&pool, p_other, other_shelf, other_proc, 0).await;

    // 本工序：1 个候选（APPROVAL，co1）+ 1 个在外协（co2）。
    let p1 = insert_part(&pool, cid, "DT1", "2026-12-02").await;
    let (chain1, steps1) = create_chain_with_steps(&pool, p1, &[(proc_id, 1)]).await;
    let batch1 = insert_candidate_batch(&pool, p1, shelf_id, proc_id, 7).await;
    insert_approved_quote(&pool, p1, co1, proc_id, "42.00").await;

    let p2 = insert_part(&pool, cid, "DT2", "2026-12-03").await;
    let (_, steps2) = create_chain_with_steps(&pool, p2, &[(proc_id, 1)]).await;
    let held = insert_held_batch(&pool, p2, co2, proc_id, Some(steps2[0]), 6, 9).await;
    let q2 = insert_approved_quote(&pool, p2, co2, proc_id, "8.25").await;
    insert_open_shipment(&pool, q2, p2, held, co2, proc_id, 6, "8.25").await;
    let _ = (&chain1, &steps1);

    let (s, env) = get_detail(&app, &token, proc_id).await;
    assert_eq!(s, StatusCode::OK, "{env}");
    let data = &env["data"];
    assert_eq!(data["process_id"], proc_id.to_string(), "{env}");
    assert_eq!(data["process_code"], code, "{env}");
    assert_eq!(data["process_name"], name, "{env}");

    // —— companies：3 家活跃公司都在，含 held_count=0 的空列 ——
    let companies = data["companies"].as_array().unwrap();
    let mut got: Vec<(String, i64)> = companies
        .iter()
        .map(|c| {
            (
                c["company_id"].as_str().unwrap().to_string(),
                c["held_count"].as_i64().unwrap(),
            )
        })
        .collect();
    got.sort();
    let mut want = vec![
        (co1.to_string(), 0i64),
        (co2.to_string(), 1i64),
        (co3.to_string(), 0i64),
    ];
    want.sort();
    assert_eq!(
        got, want,
        "companies 必须是映射的 3 家活跃公司且计数正确: {env}"
    );
    let names: Vec<&str> = companies
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"PdCo3"), "无在途批次的公司也要在列: {env}");
    assert!(!names.contains(&"PdCoOff"), "停用公司不得出现: {env}");

    // —— items：只含本工序的候选行 ——
    let items = data["items"].as_array().unwrap();
    assert_eq!(items.len(), 1, "{env}");
    assert_eq!(
        data["total"].as_i64().unwrap(),
        items.len() as i64,
        "total 必须等于 items.len(): {env}"
    );
    let row = row_by_batch(items, batch1, &env);
    assert_eq!(row["batch_id"], batch1.to_string(), "{env}");
    assert!(
        items
            .iter()
            .all(|i| i["batch_id"].as_str() != Some(other_batch.to_string().as_str())),
        "不得含其它工序的行: {env}"
    );
    assert_eq!(row["send_mode"], "APPROVAL", "{env}");
    assert_eq!(row["can_send"], true, "{env}");
    assert_eq!(row["status_label"], "sendable", "{env}");
    assert_eq!(row["version"], 7, "{env}");
    assert_eq!(row["shelf_code"], "PDS", "{env}");
    assert_eq!(row["customer_path"], "PcDet", "{env}");
}

/// `counts` / `state` 是 1 段静态路径，必须注册在 `/{process_id}` 之前，
/// 否则会被 `Path<i64>` 兜住 → 400 而不是 200。
#[tokio::test]
async fn pool_counts_and_state_are_static_routes_not_process_id() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let co = insert_company(&pool, "RouteCo", true).await;
    let (proc_id, _, _) = seed_outsource_process(&pool, "PD-ROUTE", true).await;

    // `/counts` 若被 `/{process_id}` 吞掉，这里会是 400（"counts" 不是 i64）。
    let (s, env) = get_counts(&app, &token).await;
    assert_eq!(s, StatusCode::OK, "/counts 被当成 process_id 解析了: {env}");
    assert!(env["data"]["counts"].is_array(), "{env}");

    let (s, env) = get_state(&app, &token, co, proc_id).await;
    assert_eq!(s, StatusCode::OK, "/state 被当成 process_id 解析了: {env}");
    assert_eq!(env["data"]["current_held"], 0, "{env}");
}

#[tokio::test]
async fn pool_detail_keeps_direct_row_with_empty_company_options() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let cid = insert_customer(&pool, "PcBare", "C").await;
    let (proc_id, _, _) = seed_outsource_process(&pool, "PD-BARE", false).await;
    let shelf_id = insert_shelf(&pool, "PDB").await;
    link_shelf_process(&pool, shelf_id, proc_id).await;
    // 该工序**不映射任何活跃公司** → DIRECT 且 company_options 为空。
    let part_id = insert_part(&pool, cid, "BARE", "2026-12-01").await;
    create_chain_with_steps(&pool, part_id, &[(proc_id, 1)]).await;
    let batch_id = insert_candidate_batch(&pool, part_id, shelf_id, proc_id, 0).await;

    let (s, env) = get_detail(&app, &token, proc_id).await;
    assert_eq!(s, StatusCode::OK, "{env}");
    let items = env["data"]["items"].as_array().unwrap();
    assert_eq!(items.len(), 1, "空 options 行仍要返回: {env}");
    assert_eq!(env["data"]["total"], 1, "并计入 total: {env}");
    let row = row_by_batch(items, batch_id, &env);
    assert_eq!(row["send_mode"], "DIRECT", "{env}");
    assert_eq!(row["company_options"].as_array().unwrap().len(), 0, "{env}");
    assert_eq!(row["can_send"], false, "无候选公司不可发: {env}");
    assert!(row["quote_id"].is_null(), "{env}");
    assert!(row["price"].is_null(), "{env}");
    // companies 为空数组（该工序没映射公司）
    assert_eq!(
        env["data"]["companies"].as_array().unwrap().len(),
        0,
        "{env}"
    );

    // counts 侧口径也必须把它算进 sendable_count。
    let (s, counts_env) = get_counts(&app, &token).await;
    assert_eq!(s, StatusCode::OK, "{counts_env}");
    let c = counts_env["data"]["counts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["process_id"].as_str() == Some(proc_id.to_string().as_str()))
        .unwrap_or_else(|| panic!("counts 缺工序 {proc_id}: {counts_env}"));
    assert_eq!(c["sendable_count"], 1, "{counts_env}");
}

// ===========================================================================
//  3. items 与 /outsource-sendable 逐字段一致（防 SQL 分叉的核心断言）
// ===========================================================================

#[tokio::test]
async fn pool_detail_items_match_sendable_endpoint_field_by_field() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let cid = insert_customer(&pool, "PcCmp", "E").await;
    // 两道工序：APPROVAL（需审批 + 已批准报价）与 DIRECT（免审批）。
    // `requires_approval` 是**工序级**属性，同一道工序不可能同时产出两种模式，
    // 所以必须分两道工序来对照。
    let (proc_appr, _, _) = seed_outsource_process(&pool, "PD-CMP-A", true).await;
    let (proc_dir, _, _) = seed_outsource_process(&pool, "PD-CMP-D", false).await;
    let shelf_id = insert_shelf(&pool, "PDC").await;
    link_shelf_process(&pool, shelf_id, proc_appr).await;
    link_shelf_process(&pool, shelf_id, proc_dir).await;
    let co1 = insert_company(&pool, "CmpCo1", true).await;
    let co2 = insert_company(&pool, "CmpCo2", true).await;
    link_company_process(&pool, co1, proc_appr).await;
    link_company_process(&pool, co1, proc_dir).await;
    link_company_process(&pool, co2, proc_dir).await;
    // 停用但已映射的公司：两边都不得把它列进 company_options。
    let co_off = insert_company(&pool, "CmpCoOff", false).await;
    link_company_process(&pool, co_off, proc_dir).await;

    let batch_appr = {
        let p = insert_part(&pool, cid, "AP", "2026-12-01").await;
        create_chain_with_steps(&pool, p, &[(proc_appr, 1)]).await;
        let b = insert_candidate_batch(&pool, p, shelf_id, proc_appr, 11).await;
        insert_approved_quote(&pool, p, co1, proc_appr, "19.90").await;
        b
    };
    let batch_direct = {
        let p = insert_part(&pool, cid, "DI", "2026-12-02").await;
        create_chain_with_steps(&pool, p, &[(proc_dir, 1)]).await;
        insert_candidate_batch(&pool, p, shelf_id, proc_dir, 22).await
    };
    // 另一道工序的行 —— detail 端点不得带出它。
    let (other_proc, _, _) = seed_outsource_process(&pool, "PD-CMP-OTHER", false).await;
    let other_shelf = insert_shelf(&pool, "PDC-OTHER").await;
    link_shelf_process(&pool, other_shelf, other_proc).await;
    let p_other = insert_part(&pool, cid, "OT", "2026-12-03").await;
    create_chain_with_steps(&pool, p_other, &[(other_proc, 1)]).await;
    let other_batch = insert_candidate_batch(&pool, p_other, other_shelf, other_proc, 33).await;

    let (s, sendable_env) = get_sendable(&app, &token).await;
    assert_eq!(s, StatusCode::OK, "{sendable_env}");

    // 逐字段比对（detail 少了 current_process_*，工序已提到顶层）。
    let fields = [
        "batch_id",
        "batch_no",
        "batch_quantity",
        "version",
        "send_mode",
        "source_status",
        "part_id",
        "part_serial_no",
        "part_drawing_no",
        "part_name",
        "quantity",
        "planned_delivery_date",
        "is_urgent",
        "customer_path",
        "shelf_code",
        "outsource_company_id",
        "outsource_company_name",
        "quote_id",
        "price",
        "status_label",
        "company_options",
    ];
    for (proc_id, expect_modes) in [(proc_appr, vec!["APPROVAL"]), (proc_dir, vec!["DIRECT"])] {
        let (s, detail_env) = get_detail(&app, &token, proc_id).await;
        assert_eq!(s, StatusCode::OK, "{detail_env}");
        let detail_items = detail_env["data"]["items"].as_array().unwrap();
        // sendable 侧按 current_process_id 过滤出同一批行。
        let pid = proc_id.to_string();
        let sendable_items: Vec<&Value> = sendable_env["data"]["items"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|i| i["current_process_id"] == pid)
            .collect();
        assert_eq!(
            detail_items.len(),
            sendable_items.len(),
            "同一工序的行数必须一致: detail={detail_env} sendable={sendable_env}"
        );
        assert_eq!(detail_items.len(), expect_modes.len(), "{detail_env}");
        assert!(
            detail_items
                .iter()
                .all(|i| i["batch_id"].as_str() != Some(other_batch.to_string().as_str())),
            "不得含其它工序的行: {detail_env}"
        );
        let modes: Vec<&str> = detail_items
            .iter()
            .map(|i| i["send_mode"].as_str().unwrap())
            .collect();
        assert_eq!(modes, expect_modes, "{detail_env}");
        for d in detail_items {
            let key = d["batch_id"].as_str().unwrap().to_string();
            let s_row = sendable_items
                .iter()
                .find(|i| i["batch_id"].as_str() == Some(key.as_str()))
                .unwrap_or_else(|| panic!("sendable 缺 batch {key}: {sendable_env}"));
            for f in fields {
                assert_eq!(
                    d[f], s_row[f],
                    "字段 {f} 在 batch {key} 上不一致: detail={detail_env} sendable={sendable_env}"
                );
            }
        }
    }

    // 停用公司确实不在 DIRECT 的 options 里（顺带锁 DIRECT 口径一致性）。
    let (s, dir_env) = get_detail(&app, &token, proc_dir).await;
    assert_eq!(s, StatusCode::OK, "{dir_env}");
    let dir_items = dir_env["data"]["items"].as_array().unwrap();
    let direct = row_by_batch(dir_items, batch_direct, &dir_env);
    let opt_ids: Vec<String> = direct["company_options"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(opt_ids.len(), 2, "{dir_env}");
    assert!(!opt_ids.contains(&co_off.to_string()), "{dir_env}");
    let (s, appr_env) = get_detail(&app, &token, proc_appr).await;
    assert_eq!(s, StatusCode::OK, "{appr_env}");
    let appr = row_by_batch(
        appr_env["data"]["items"].as_array().unwrap(),
        batch_appr,
        &appr_env,
    );
    assert_eq!(appr["send_mode"], "APPROVAL", "{appr_env}");
    assert_eq!(
        appr["company_options"].as_array().unwrap().len(),
        0,
        "{appr_env}"
    );
}

// ===========================================================================
//  5 + 6 + 7 + 8 + 9. state
// ===========================================================================

#[tokio::test]
async fn pool_state_returns_held_batches_with_shipment_fields() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let cid = insert_customer(&pool, "PcSt", "F").await;
    let (proc_id, _, _) = seed_outsource_process(&pool, "PST-MAIN", true).await;
    let co1 = insert_company(&pool, "StCo1", true).await;
    let co2 = insert_company(&pool, "StCo2", true).await;
    link_company_process(&pool, co1, proc_id).await;
    link_company_process(&pool, co2, proc_id).await;

    // 链尾 + 带 step（无下一 step）。
    let p1 = insert_part(&pool, cid, "ST1", "2026-12-01").await;
    let (_, s1) = create_chain_with_steps(&pool, p1, &[(proc_id, 1)]).await;
    let b1 = insert_held_batch(&pool, p1, co1, proc_id, Some(s1[0]), 6, 4).await;
    let q1 = insert_approved_quote(&pool, p1, co1, proc_id, "5.55").await;
    insert_open_shipment(&pool, q1, p1, b1, co1, proc_id, 6, "5.55").await;

    let p2 = insert_part(&pool, cid, "ST2", "2026-12-02").await;
    let (_, s2) = create_chain_with_steps(&pool, p2, &[(proc_id, 1)]).await;
    let b2 = insert_held_batch(&pool, p2, co1, proc_id, Some(s2[0]), 3, 2).await;
    let q2 = insert_approved_quote(&pool, p2, co1, proc_id, "7.77").await;
    insert_open_shipment(&pool, q2, p2, b2, co1, proc_id, 3, "7.77").await;

    // co2 在同工序上也有一个批次，但 state 是按公司查的 → 不该出现。
    let p3 = insert_part(&pool, cid, "ST3", "2026-12-03").await;
    let (_, s3) = create_chain_with_steps(&pool, p3, &[(proc_id, 1)]).await;
    let b3 = insert_held_batch(&pool, p3, co2, proc_id, Some(s3[0]), 9, 6).await;
    let q3 = insert_approved_quote(&pool, p3, co2, proc_id, "1.11").await;
    insert_open_shipment(&pool, q3, p3, b3, co2, proc_id, 9, "1.11").await;

    let (s, env) = get_state(&app, &token, co1, proc_id).await;
    assert_eq!(s, StatusCode::OK, "{env}");
    let data = &env["data"];
    assert_eq!(data["outsource_company_id"], co1.to_string(), "{env}");
    assert_eq!(data["outsource_company_name"], "StCo1", "{env}");
    assert_eq!(data["process_id"], proc_id.to_string(), "{env}");
    let items = data["items"].as_array().unwrap();
    assert_eq!(items.len(), 2, "{env}");
    assert_eq!(
        data["current_held"].as_i64().unwrap(),
        items.len() as i64,
        "current_held 必须等于 items.len(): {env}"
    );
    assert!(
        items
            .iter()
            .all(|i| i["batch_id"].as_str() != Some(b3.to_string().as_str())),
        "别的公司的批次不得出现: {env}"
    );

    let r1 = row_by_batch(items, b1, &env);
    assert_eq!(r1["part_id"], p1.to_string(), "{env}");
    assert_eq!(r1["batch_no"], 1, "{env}");
    assert_eq!(r1["quantity"], 6, "quantity 取当前余量: {env}");
    assert_eq!(r1["location"], "OUTSOURCE_COMPANY", "{env}");
    assert_eq!(r1["version"], 4, "version 取 batch.version: {env}");
    assert_eq!(r1["price"], "5.55", "price 取 shipment.unit_price: {env}");
    assert!(
        r1["sent_at"].as_str().unwrap().starts_with("20"),
        "sent_at 必须有值: {r1}"
    );
    assert_eq!(r1["customer_name"], "PcSt", "{env}");
    assert!(
        r1["parent_customer_name"].is_null(),
        "无 L1 时为 null: {env}"
    );

    // criterion 9：雪花 ID 全字符串、price 是字符串。
    assert!(r1["batch_id"].is_string(), "{r1}");
    assert!(r1["part_id"].is_string(), "{r1}");
    assert!(r1["price"].is_string(), "price 必须是字符串: {r1}");

    // 另一工序上同公司的批次也不该出现。
    let (other_proc, _, _) = seed_outsource_process(&pool, "PST-OTHER", true).await;
    let p4 = insert_part(&pool, cid, "ST4", "2026-12-04").await;
    let (_, s4) = create_chain_with_steps(&pool, p4, &[(other_proc, 1)]).await;
    let b4 = insert_held_batch(&pool, p4, co1, other_proc, Some(s4[0]), 2, 1).await;
    let q4 = insert_approved_quote(&pool, p4, co1, other_proc, "2.00").await;
    insert_open_shipment(&pool, q4, p4, b4, co1, other_proc, 2, "2.00").await;

    let (s, env) = get_state(&app, &token, co1, proc_id).await;
    assert_eq!(s, StatusCode::OK, "{env}");
    let items = env["data"]["items"].as_array().unwrap();
    assert_eq!(items.len(), 2, "不得含其它工序的批次: {env}");
}

#[tokio::test]
async fn pool_state_chain_resolvable_when_next_step_exists() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let cid = insert_customer(&pool, "PcChain", "G").await;
    let (proc_id, _, _) = seed_outsource_process(&pool, "PCH-OS", true).await;
    let (next_id_, _, next_name) = seed_inhouse_process(&pool, "PCH-NEXT").await;
    let co = insert_company(&pool, "ChainCo", true).await;
    link_company_process(&pool, co, proc_id).await;

    // 链：sort 1 = 外协工序，sort 2 = 厂内下一道工序。
    let p = insert_part(&pool, cid, "CH1", "2026-12-01").await;
    let (_, steps) = create_chain_with_steps(&pool, p, &[(proc_id, 1), (next_id_, 2)]).await;
    let b = insert_held_batch(&pool, p, co, proc_id, Some(steps[0]), 7, 5).await;
    let q = insert_approved_quote(&pool, p, co, proc_id, "3.21").await;
    insert_open_shipment(&pool, q, p, b, co, proc_id, 7, "3.21").await;

    let (s, env) = get_state(&app, &token, co, proc_id).await;
    assert_eq!(s, StatusCode::OK, "{env}");
    let row = row_by_batch(env["data"]["items"].as_array().unwrap(), b, &env);
    assert_eq!(row["chain_resolvable"], true, "有下一 step ⇒ 可解析: {env}");
    assert_eq!(
        row["receive_next_process_id"],
        next_id_.to_string(),
        "必须是下一 step 的 process_id: {env}"
    );
    assert_eq!(row["receive_next_process_name"], next_name, "{env}");
}

#[tokio::test]
async fn pool_state_chain_unresolvable_when_no_step_or_chain_tail() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let cid = insert_customer(&pool, "PcNoChain", "H").await;
    let (proc_id, _, _) = seed_outsource_process(&pool, "PNC-OS", true).await;
    let co = insert_company(&pool, "NoChainCo", true).await;
    link_company_process(&pool, co, proc_id).await;

    // ① 链尾：step sort=1 是最后一道 → 无下一 step。
    let p_tail = insert_part(&pool, cid, "TAIL", "2026-12-01").await;
    let (_, steps_tail) = create_chain_with_steps(&pool, p_tail, &[(proc_id, 1)]).await;
    let b_tail = insert_held_batch(&pool, p_tail, co, proc_id, Some(steps_tail[0]), 4, 1).await;
    let q_tail = insert_approved_quote(&pool, p_tail, co, proc_id, "1.00").await;
    insert_open_shipment(&pool, q_tail, p_tail, b_tail, co, proc_id, 4, "1.00").await;

    // ② 批次没写 current_process_step_id（DB NULL ⇒ 后端 0 兜底）。
    let p_nostep = insert_part(&pool, cid, "NOS", "2026-12-02").await;
    create_chain_with_steps(&pool, p_nostep, &[(proc_id, 1)]).await;
    let b_nostep = insert_held_batch(&pool, p_nostep, co, proc_id, None, 4, 1).await;
    let q_nostep = insert_approved_quote(&pool, p_nostep, co, proc_id, "1.00").await;
    insert_open_shipment(&pool, q_nostep, p_nostep, b_nostep, co, proc_id, 4, "1.00").await;

    let (s, env) = get_state(&app, &token, co, proc_id).await;
    assert_eq!(s, StatusCode::OK, "{env}");
    let items = env["data"]["items"].as_array().unwrap();
    assert_eq!(items.len(), 2, "{env}");
    for (b, tag) in [(b_tail, "链尾"), (b_nostep, "无 step")] {
        let row = row_by_batch(items, b, &env);
        assert_eq!(row["chain_resolvable"], false, "{tag} 应不可解析: {env}");
        assert_eq!(
            row["receive_next_process_id"], "0",
            "{tag} 时 receive_next_process_id 必须是字符串 \"0\": {env}"
        );
        assert!(row["receive_next_process_name"].is_null(), "{tag}: {env}");
    }
}

/// 读侧派生下一 step 时，**当前 step 必须在锚链内按 `current_process_id` 重新
/// 定位**，不能拿 `pb.current_process_step_id` 的 `sort_order` 当位置。
///
/// 本用例把外协工序在锚链内从 `sort 1` 挪到 `sort 2`（**位置漂移**）后，断言
/// 返回的是**真正的下一道工序**，而不是外协工序自己：
/// - 位置式定位会返回 `sort_order = 旧 sort + 1` 那一步 = 外协工序自己，
///   且 `chain_resolvable` 仍为 `true` ⇒ 写侧照单全收，是**静默错值**；
/// - 按 `current_process_id` 定位才能取到外协工序在锚链内的真实位置 +1。
///
/// ⚠️ **本用例构造的状态产品不可达**：`bind_part_to_chain` 是裸
/// `UPDATE t_part SET process_chain_id`，绕过了 `link_chain_to_part` 的
/// `AND process_chain_id IS NULL` 守卫（已绑定的 part 走产品路径不可改绑）。
/// 保留它是**防御性回归网** —— 守住「锚链内位置漂移时仍取对值」这条性质，
/// 而不是把该混合语义固化成期望行为。断言未因此弱化。
#[tokio::test]
async fn pool_state_derives_next_step_from_parts_current_chain_after_rebind() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let cid = insert_customer(&pool, "PcRebind", "I").await;
    let (proc_os, _, os_name) = seed_outsource_process(&pool, "PRB-OS", true).await;
    let (proc_next_a, _, next_name_a) = seed_inhouse_process(&pool, "PRB-NEXTA").await;
    let (proc_next_b, _, next_name_b) = seed_inhouse_process(&pool, "PRB-NEXTB").await;
    let (proc_next_c, _, next_name_c) = seed_inhouse_process(&pool, "PRB-NEXTC").await;
    let co = insert_company(&pool, "RebindCo", true).await;
    link_company_process(&pool, co, proc_os).await;

    // 发出时：part 绑链 A（sort 1 = 外协，sort 2 = 下一道 NEXT_A）。
    let p = insert_part(&pool, cid, "RB", "2026-12-01").await;
    let (_, steps_a) = create_chain_with_steps(&pool, p, &[(proc_os, 1), (proc_next_a, 2)]).await;
    let b = insert_held_batch(&pool, p, co, proc_os, Some(steps_a[0]), 6, 2).await;
    let q = insert_approved_quote(&pool, p, co, proc_os, "9.00").await;
    insert_open_shipment(&pool, q, p, b, co, proc_os, 6, "9.00").await;

    // 改绑到链 B。**关键：外协工序在链 B 里被挪到 sort 2**（sort 1 = NEXT_B），
    // 批次的 `current_process_step_id` 仍指向**旧链 A** 的 step（sort 1）。
    // ⇒ 位置式定位会算成「链 B 的 sort 2」= 外协工序自己（静默错值）；
    //   按 current_process_id 定位才会拿到「链 B 的 sort 2 → sort 3」= NEXT_C。
    let chain_b = create_chain(&pool, "rebound-chain").await;
    add_chain_step(&pool, chain_b, proc_next_b, 1).await;
    add_chain_step(&pool, chain_b, proc_os, 2).await;
    add_chain_step(&pool, chain_b, proc_next_c, 3).await;
    bind_part_to_chain(&pool, p, chain_b).await;

    let (s, env) = get_state(&app, &token, co, proc_os).await;
    assert_eq!(s, StatusCode::OK, "{env}");
    let row = row_by_batch(env["data"]["items"].as_array().unwrap(), b, &env);
    assert_eq!(
        row["receive_next_process_id"],
        proc_next_c.to_string(),
        "外协工序在锚链内位于 sort 2，真正的下一道是 sort 3 的 NEXT_C \
         （按位置取会返回外协工序自己）: {env}"
    );
    assert_eq!(row["receive_next_process_name"], next_name_c, "{env}");
    // 位置式定位的取值（外协工序自己）必须被排除，且它带 `chain_resolvable=true`
    // ⇒ 一旦实现退回按位置取，这两条断言就是它唯一的护栏。
    assert_ne!(
        row["receive_next_process_id"],
        proc_os.to_string(),
        "绝不能把外协工序自己当成下一道工序返回（静默错值）: {env}"
    );
    assert_ne!(row["receive_next_process_name"], os_name, "{env}");
    // 位置式定位不会退回旧链 A ⇒ NEXT_A 同样必须被排除。
    assert_ne!(
        row["receive_next_process_id"],
        proc_next_a.to_string(),
        "不得返回旧链 A 里的下一道工序: {env}"
    );
    assert_ne!(row["receive_next_process_name"], next_name_a, "{env}");
    assert_ne!(
        row["receive_next_process_id"],
        proc_next_b.to_string(),
        "NEXT_B 排在当前工序之前，不是下一道: {env}"
    );
    assert_ne!(row["receive_next_process_name"], next_name_b, "{env}");
    assert_eq!(
        row["chain_resolvable"], true,
        "锚链内存在下一 step ⇒ 可解析: {env}"
    );

    // 写侧前提：`optional_process_chain(part)` 取到的链 + 在该链内按
    // `resolve_step_id_by_process` 解析，必须落在**同一个 step** 上 ——
    // 否则上一组断言等于给了前端一个写侧会拒收（404 `20702`）的默认值。
    let write_side: Vec<(i64, i64)> = sqlx::query_as(
        "SELECT (SELECT process_chain_id FROM t_part WHERE id = $1)::bigint, s.id \
         FROM t_process_chain_step s \
         WHERE s.chain_id = (SELECT process_chain_id FROM t_part WHERE id = $1) \
           AND s.process_id = $2 AND s.deleted_at IS NULL LIMIT 1",
    )
    .bind(p)
    .bind(proc_next_c)
    .fetch_all(&pool)
    .await
    .expect("resolve write-side step for proc_next_c");
    assert_eq!(
        write_side.len(),
        1,
        "写侧必须在 chain B 内解析出 NEXT_C 的 step，否则默认值会被 404 拒收"
    );
    assert_eq!(
        write_side[0].0, chain_b,
        "写侧 optional_process_chain 取到的链必须与读侧锚链一致"
    );
}

/// `t_applicant` 按 name 匹配（字符串非 FK，唯一索引是 `(name, customer_id)`），
/// 同名申请人跨客户并存时 `list_held` **不得扇出**：一个批次恒一行，且
/// `current_held == items.len()`。
#[tokio::test]
async fn pool_state_does_not_fan_out_on_duplicate_applicant_name() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    // `serial_prefix` 是 varchar(1) 且全局唯一（uq_t_customer_root_prefix），
    // 两个客户必须用不同前缀。
    let cid1 = insert_customer(&pool, "PcAp1", "Z").await;
    let cid2 = insert_customer(&pool, "PcAp2", "Y").await;
    let (proc_id, _, _) = seed_outsource_process(&pool, "PAP-OS", true).await;
    let co = insert_company(&pool, "ApCo", true).await;
    link_company_process(&pool, co, proc_id).await;

    // 同名申请人分属两个客户 —— DB 允许（唯一索引含 customer_id）。
    insert_applicant(&pool, "DupName", cid1).await;
    insert_applicant(&pool, "DupName", cid2).await;

    let p = insert_part(&pool, cid1, "AP", "2026-12-01").await;
    set_part_applicant(&pool, p, "DupName").await;
    let (_, steps) = create_chain_with_steps(&pool, p, &[(proc_id, 1)]).await;
    let b = insert_held_batch(&pool, p, co, proc_id, Some(steps[0]), 5, 2).await;
    let q = insert_approved_quote(&pool, p, co, proc_id, "4.00").await;
    insert_open_shipment(&pool, q, p, b, co, proc_id, 5, "4.00").await;

    let (s, env) = get_state(&app, &token, co, proc_id).await;
    assert_eq!(s, StatusCode::OK, "{env}");
    let items = env["data"]["items"].as_array().unwrap();
    assert_eq!(
        items.len(),
        1,
        "同名申请人跨客户不得把一行批次扇成多行: {env}"
    );
    assert_eq!(
        env["data"]["current_held"].as_i64().unwrap(),
        items.len() as i64,
        "current_held 必须等于 items.len(): {env}"
    );
    let batch_ids: Vec<&Value> = items.iter().map(|i| &i["batch_id"]).collect();
    assert_eq!(batch_ids.len(), 1, "batch_id 不得重复: {env}");
    let row = row_by_batch(items, b, &env);
    assert_eq!(row["applicant_name"], "DupName", "{env}");
}

#[tokio::test]
async fn pool_state_rejects_missing_query_params_with_400() {
    let (_pool, app, token, _fx) = bootstrap_as_manager().await;
    for uri in [
        "/outsource-pool/state",
        "/outsource-pool/state?outsource_company_id=1",
        "/outsource-pool/state?process_id=1",
    ] {
        let (s, body) = get_raw(&app, &token, uri).await;
        assert_eq!(
            s,
            StatusCode::BAD_REQUEST,
            "{uri} 缺参数必须 400（不得静默给默认值）: body={body}"
        );
        assert_ne!(s, StatusCode::INTERNAL_SERVER_ERROR, "{uri}: body={body}");
    }
}

/// 工序不存在 → 404（口径同 `/prod/pool/{process_id}`）。
#[tokio::test]
async fn pool_detail_unknown_process_returns_404() {
    let (_pool, app, token, _fx) = bootstrap_as_manager().await;
    let (s, env) = get_detail(&app, &token, 9_000_000_000_000_009_999).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{env}");
    assert_eq!(env["code"], 20801, "{env}");
}

/// counts 端点无候选也无在协时返回空数组 + 全零（不是 500）。
#[tokio::test]
async fn pool_counts_empty_returns_zeroed_totals() {
    let (_pool, app, token, _fx) = bootstrap_as_manager().await;
    let (s, env) = get_counts(&app, &token).await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(env["data"]["counts"].as_array().unwrap().len(), 0, "{env}");
    assert_eq!(env["data"]["sendable_total"], 0, "{env}");
    assert_eq!(env["data"]["in_flight_total"], 0, "{env}");
    assert_eq!(env["data"]["total"], 0, "{env}");
}

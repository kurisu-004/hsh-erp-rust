//! prod::programming 域端到端集成测试（2026-10-01 新增）
//!
//! 端点：`GET /api/v2/prod/programming/pending`（测试内路径 `/prod/programming/pending`）
//!
//! 覆盖 10 个场景：
//!   1. 规则1 单独命中：`p.status='PROGRAMMING'`（无链、无批次进 CNC）能查到
//!   2. 规则2 单独命中：工单工艺链上某 step 的工序 `is_cnc=true` 能查到
//!   3. 规则3 单独命中：工单无工艺链，但批次 `current_process_id` 指向 CNC 工序
//!   4. 三规则并集去重：同时满足多条规则的工单只出现一次
//!   5. 反例：不满足任何规则的工单（批次仅挂在非 CNC 工序）查不到
//!   6. `has_cnc_program` 三态：缺省=全部 / `false`=仅未上传 / `true`=仅已上传
//!   7. `keyword` 模糊（name / drawing_no / serial_no 任一命中）+ `serial_no` 精确
//!   8. 排序：默认 `PLANNED_DELIVERY_DATE ASC`；`DESC` 生效；非法 `sort_by` 退化
//!   9. 分页：limit=0 → clamp 1；offset=-1 → max 0
//!   10. 角色守卫：MANAGER / CNC_PROGRAMMER 可访问；SHELF_ACCOUNT → 40300
//!
//! ## 串行化
//! 进程级 test_pool 每次 fresh database（plan 2 2026-09-20），DB 间 schema
//! 完全独立，无需 Mutex / `--test-threads=1` 双保险。每个测试内部造自己的
//! part / batch，互不污染；断言一律具体到「查到哪几个 id」。
//!
//! ## 集成测试范本（PR13 Phase F / H）
//! HTTP helper（`send` / `json_request` / `login_token` / `test_app` / `test_state` /
//! `test_pool`）与客户 fixture 走 `hsh_erp_test_support`；本域独享的 raw SQL 构造
//! （CNC 工序 / 工艺链 / 工单 / 批次 / G_CODE 文件 / CNC 编程员账号）按 `batch.rs`
//! 惯例保留为本地 fn。

use axum::http::StatusCode;
use chrono::NaiveDate;
use serde_json::{Value, json};
use sqlx::PgPool;

use hsh_erp_test_support::{
    CncProgramFixture, ProductionFixture, json_request, load_cnc_program_fixture,
    load_production_fixture, login_token, pool_snowflake, send, test_app, test_pool, test_state,
};

/// 待编程列表端点路径（测试 app 不带 `/api/v2` 前缀）。
const PENDING_URI: &str = "/prod/programming/pending";

// ===========================================================================
//  Bootstrap helpers（PR13 Phase F 风格）
// ===========================================================================

/// fresh database + production fixture（提供 MANAGER 账号）+ cnc_program fixture
/// （提供 L1/L2 客户）→ 以 MANAGER 身份登录。
async fn bootstrap() -> (
    PgPool,
    axum::Router,
    String,
    ProductionFixture,
    CncProgramFixture,
) {
    let pool = test_pool().await;
    let fx = load_production_fixture(&pool).await;
    let cfx = load_cnc_program_fixture(&pool).await;
    let app = test_app(test_state(pool.clone()).await);
    let token = login_token(&app, &fx.part_manager_username, ProductionFixture::PASSWORD).await;
    (pool, app, token, fx, cfx)
}

// ===========================================================================
//  prod::programming 域独享 raw SQL helper
// ===========================================================================

/// 插一个 INHOUSE 工序，`is_cnc` 由入参控制。
///
/// 2026-10-01：predicate 真相源是 `t_process.is_cnc` 列（不是 code 字面量），
/// 故 helper 直接写该列。
async fn seed_process(pool: &PgPool, code: &str, name: &str, is_cnc: bool) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;

    let id = pool_snowflake()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .next_id();
    sqlx::query(
        "INSERT INTO t_process (id, code, name, category, sort_order, requires_approval, \
         is_cnc, version, created_at, updated_at) \
         VALUES ($1, $2, $3, 'INHOUSE', 0, false, $4, 0, $5, $5)",
    )
    .bind(id)
    .bind(code)
    .bind(name)
    .bind(is_cnc)
    .bind(now_naive())
    .execute(pool)
    .await
    .expect("insert t_process");
    id
}

/// 建一条工艺链 + 单 step（step 指向 `process_id`），返回 chain_id。
async fn insert_chain_with_step(pool: &PgPool, process_id: i64, name: &str) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;

    let snowflake = pool_snowflake().lock().unwrap_or_else(|p| p.into_inner());
    let chain_id = snowflake.next_id();
    let step_id = snowflake.next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_part_process_chain (id, name, version, created_at, created_by, \
         updated_at, updated_by) VALUES ($1, $2, 0, $3, 0, $3, 0)",
    )
    .bind(chain_id)
    .bind(name)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_part_process_chain");
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
    chain_id
}

/// 插一个 `t_part` 行（`applicant_name` NOT NULL → 空串占位）。
#[allow(clippy::too_many_arguments)] // 与 prod/batch/repo.rs 同惯例：测试 helper 允许平铺参数
async fn insert_part(
    pool: &PgPool,
    customer_id: i64,
    serial_no: &str,
    name: &str,
    drawing_no: &str,
    status: &str,
    planned_delivery_date: NaiveDate,
    process_chain_id: Option<i64>,
) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;

    let id = pool_snowflake()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_part (id, serial_no, name, drawing_no, applicant_name, quantity, \
         request_date, planned_delivery_date, status, is_urgent, customer_id, version, \
         created_at, updated_at, process_chain_id) \
         VALUES ($1, $2, $3, $4, '', 1, $5, $5, $6, false, $7, 0, $8, $8, $9)",
    )
    .bind(id)
    .bind(serial_no)
    .bind(name)
    .bind(drawing_no)
    .bind(planned_delivery_date)
    .bind(status)
    .bind(customer_id)
    .bind(now)
    .bind(process_chain_id)
    .execute(pool)
    .await
    .expect("insert t_part");
    id
}

/// 插一个 `t_part_batch` 行（`current_process_id` 是批次工序归属唯一权威依据）。
async fn insert_batch(pool: &PgPool, part_id: i64, status: &str, current_process_id: i64) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;

    let id = pool_snowflake()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_part_batch (id, part_id, batch_no, quantity, status, location, \
         current_process_id, version, created_at, updated_at) \
         VALUES ($1, $2, 1, 1, $3, 'PRODUCTION_SHELF', $4, 0, $5, $5)",
    )
    .bind(id)
    .bind(part_id)
    .bind(status)
    .bind(current_process_id)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_part_batch");
    id
}

/// 给工单挂一个 `kind='G_CODE'` 文件（`has_cnc_program` 真相源）。
async fn insert_gcode_file(pool: &PgPool, part_id: i64, object_key: &str) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;

    let id = pool_snowflake()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_part_file (id, part_id, kind, file_type, object_key, original_filename, \
         file_size, content_type, version, created_at, updated_at) \
         VALUES ($1, $2, 'G_CODE', 'NC', $3, 'prog.nc', 128, 'text/plain', 0, $4, $4)",
    )
    .bind(id)
    .bind(part_id)
    .bind(object_key)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_part_file G_CODE");
    id
}

/// 插一个 `t_user`（bcrypt 现场 hash）+ 指定 role，返回 username。
///
/// fixture 预置的 4 个账号里没有 CNC_PROGRAMMER，而「CNC 编程员能进本页」是本
/// 端点的硬需求（漏角色 = 403），故本地建号。
async fn login_user_with_role(pool: &PgPool, username: &str, role: &str) -> String {
    use hsh_erp_rust::auth::password;
    use hsh_erp_rust::infra::clock::now_naive;

    let hash = password::hash(ProductionFixture::PASSWORD).expect("bcrypt hash");
    let snowflake = pool_snowflake().lock().unwrap_or_else(|p| p.into_inner());
    let user_id = snowflake.next_id();
    let role_id = snowflake.next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_user (id, username, password_hash, full_name, is_active, \
         refresh_token_version, version, created_at, updated_at) \
         VALUES ($1, $2, $3, $2, true, 0, 0, $4, $4)",
    )
    .bind(user_id)
    .bind(username)
    .bind(hash)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_user");
    sqlx::query(
        "INSERT INTO t_user_role (id, user_id, role, scope_type, scope_id, version, \
         created_at, updated_at) VALUES ($1, $2, $3, NULL, NULL, 0, $4, $4)",
    )
    .bind(role_id)
    .bind(user_id)
    .bind(role)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_user_role");

    let app = test_app(test_state(pool.clone()).await);
    login_token(&app, username, ProductionFixture::PASSWORD).await
}

// ===========================================================================
//  断言 helper
// ===========================================================================

/// 打一次待编程列表端点，返回信封。
async fn get_pending(app: &axum::Router, token: &str, query: &str) -> Value {
    let uri = if query.is_empty() {
        PENDING_URI.to_string()
    } else {
        format!("{PENDING_URI}?{query}")
    };
    let (status, env) = send(app.clone(), json_request("GET", &uri, None, Some(token))).await;
    assert_eq!(status, StatusCode::OK, "GET {uri}: {env}");
    assert_eq!(env["code"], 0, "GET {uri}: {env}");
    env
}

/// 从信封里抽出按返回顺序排好的 item id 列表（雪花 i64 → string）。
fn item_ids(env: &Value) -> Vec<String> {
    env["data"]["items"]
        .as_array()
        .expect("data.items")
        .iter()
        .map(|it| {
            it["id"]
                .as_str()
                .expect("item.id 必须是 string(i64)")
                .to_string()
        })
        .collect()
}

/// 把 id 列表排序（消除 ORDER BY 无关的行间不确定性，便于精确集合断言）。
fn sorted(mut ids: Vec<String>) -> Vec<String> {
    ids.sort();
    ids
}

// ===========================================================================
//  Tests
// ===========================================================================

/// 场景 1: 规则1 单独命中 —— `p.status = 'PROGRAMMING'`（无工艺链、无批次）。
#[tokio::test]
async fn rule1_programming_status_matches() {
    let (pool, app, token, _fx, cfx) = bootstrap().await;
    let today = hsh_erp_rust::infra::clock::now_naive().date();

    let hit = insert_part(
        &pool,
        cfx.l2_customer_id,
        "PG-R1",
        "规则1 件",
        "D-R1",
        "PROGRAMMING",
        today,
        None,
    )
    .await;
    // 干扰项：同客户但 PENDING + 无链无批次 → 不该被捞出
    let miss = insert_part(
        &pool,
        cfx.l2_customer_id,
        "PG-R1-MISS",
        "规则1 反例",
        "D-R1-MISS",
        "PENDING",
        today,
        None,
    )
    .await;

    let env = get_pending(&app, &token, "").await;
    assert_eq!(
        sorted(item_ids(&env)),
        vec![hit.to_string()],
        "只应命中 status=PROGRAMMING 的工单（反例 {miss} 不该出现）: {env}"
    );
    assert_eq!(env["data"]["total"], 1, "{env}");
    // 客户两级 JOIN：L2 叶子 + L1 集团
    let item = &env["data"]["items"][0];
    assert_eq!(item["customer_name"], "FX CNC L2", "{env}");
    assert_eq!(item["parent_customer_name"], "FX CNC L1", "{env}");
    assert_eq!(item["has_cnc_program"], false, "{env}");
    assert_eq!(item["status"], "PROGRAMMING", "{env}");
}

/// 场景 2: 规则2 单独命中 —— 工艺链上某 step 的工序 `is_cnc = true`。
#[tokio::test]
async fn rule2_chain_contains_cnc_process_matches() {
    let (pool, app, token, _fx, cfx) = bootstrap().await;
    let today = hsh_erp_rust::infra::clock::now_naive().date();

    let cnc = seed_process(&pool, "PG-CNC-A", "CNC 工序 A", true).await;
    let plain = seed_process(&pool, "PG-PLAIN-A", "普通工序 A", false).await;
    let cnc_chain = insert_chain_with_step(&pool, cnc, "PG-CNC 链").await;
    let plain_chain = insert_chain_with_step(&pool, plain, "PG 普通链").await;

    let hit = insert_part(
        &pool,
        cfx.l2_customer_id,
        "PG-R2",
        "规则2 件",
        "D-R2",
        "PENDING",
        today,
        Some(cnc_chain),
    )
    .await;
    let miss = insert_part(
        &pool,
        cfx.l2_customer_id,
        "PG-R2-MISS",
        "规则2 反例",
        "D-R2-MISS",
        "PENDING",
        today,
        Some(plain_chain),
    )
    .await;

    let env = get_pending(&app, &token, "").await;
    assert_eq!(
        sorted(item_ids(&env)),
        vec![hit.to_string()],
        "只应命中链上含 CNC 工序的工单（反例 {miss} 不该出现）: {env}"
    );
}

/// 场景 3: 规则3 单独命中 —— 无工艺链，但批次 `current_process_id` 指向 CNC 工序。
///
/// 本场景是「工单还没建工艺链，但批次已在 CNC 工序流转」的形态，也是本端点相对
/// part 域旧端点（走 `current_holder_id → t_shelf_process` 间接链路）的核心增益。
#[tokio::test]
async fn rule3_batch_current_process_is_cnc_matches() {
    let (pool, app, token, _fx, cfx) = bootstrap().await;
    let today = hsh_erp_rust::infra::clock::now_naive().date();

    let cnc = seed_process(&pool, "PG-CNC-B", "CNC 工序 B", true).await;

    let hit = insert_part(
        &pool,
        cfx.l2_customer_id,
        "PG-R3",
        "规则3 件",
        "D-R3",
        "IN_PROCESS",
        today,
        None,
    )
    .await;
    let _batch = insert_batch(&pool, hit, "IN_PROCESS", cnc).await;

    let env = get_pending(&app, &token, "").await;
    assert_eq!(
        sorted(item_ids(&env)),
        vec![hit.to_string()],
        "批次 current_process_id 指向 CNC 工序应命中规则3: {env}"
    );
}

/// 场景 4: 三规则并集去重 —— 同时命中 3 条规则的工单只出现一次。
#[tokio::test]
async fn rules_union_dedupes_parts() {
    let (pool, app, token, _fx, cfx) = bootstrap().await;
    let today = hsh_erp_rust::infra::clock::now_naive().date();

    let cnc = seed_process(&pool, "PG-CNC-C", "CNC 工序 C", true).await;
    let chain = insert_chain_with_step(&pool, cnc, "PG 三规则链").await;

    // 工单 A：规则1（status）+ 规则2（链）+ 规则3（批次）全中
    let all_three = insert_part(
        &pool,
        cfx.l2_customer_id,
        "PG-UNION-A",
        "三规则全中件",
        "D-UA",
        "PROGRAMMING",
        today,
        Some(chain),
    )
    .await;
    insert_batch(&pool, all_three, "PENDING", cnc).await;

    // 工单 B：只中规则3
    let only_rule3 = insert_part(
        &pool,
        cfx.l2_customer_id,
        "PG-UNION-B",
        "只中规则3 件",
        "D-UB",
        "PENDING",
        today,
        None,
    )
    .await;
    insert_batch(&pool, only_rule3, "PENDING", cnc).await;

    let env = get_pending(&app, &token, "").await;
    assert_eq!(
        sorted(item_ids(&env)),
        sorted(vec![all_three.to_string(), only_rule3.to_string()]),
        "三规则并集：A 只应出现 1 次（part 级去重），B 正常出现: {env}"
    );
    assert_eq!(env["data"]["total"], 2, "total 同样按 part 去重: {env}");
}

/// 场景 5: 反例 —— 三条规则都不满足的工单查不到（批次仅挂在非 CNC 工序）。
#[tokio::test]
async fn non_matching_parts_excluded() {
    let (pool, app, token, _fx, cfx) = bootstrap().await;
    let today = hsh_erp_rust::infra::clock::now_naive().date();

    let plain = seed_process(&pool, "PG-PLAIN-C", "普通工序 C", false).await;
    let plain_chain = insert_chain_with_step(&pool, plain, "PG 普通链 C").await;

    // PENDING + 链上无 CNC + 批次在非 CNC 工序 + 无 PROGRAMMING 状态
    let miss_batch = insert_part(
        &pool,
        cfx.l2_customer_id,
        "PG-MISS-A",
        "非 CNC 批次件",
        "D-MA",
        "IN_PROCESS",
        today,
        Some(plain_chain),
    )
    .await;
    insert_batch(&pool, miss_batch, "IN_PROCESS", plain).await;

    // 已完成批次（status 不在 PENDING/IN_PROCESS/PROGRAMMING）+ CNC 工序
    let done = insert_part(
        &pool,
        cfx.l2_customer_id,
        "PG-MISS-B",
        "已完成批次件",
        "D-MB",
        "IN_PROCESS",
        today,
        None,
    )
    .await;
    let cnc = seed_process(&pool, "PG-CNC-D", "CNC 工序 D", true).await;
    insert_batch(&pool, done, "READY_TO_SHIP", cnc).await;

    // 唯一对照：status=PROGRAMMING 的工单
    let hit = insert_part(
        &pool,
        cfx.l2_customer_id,
        "PG-MISS-C",
        "对照命中件",
        "D-MC",
        "PROGRAMMING",
        today,
        None,
    )
    .await;

    let env = get_pending(&app, &token, "").await;
    assert_eq!(
        sorted(item_ids(&env)),
        vec![hit.to_string()],
        "仅非 CNC 批次 {miss_batch} / 已完成批次 {done} 都不该出现: {env}"
    );
}

/// 场景 6: `has_cnc_program` 三态（缺省 / `false` / `true`）。
#[tokio::test]
async fn has_cnc_program_tristate() {
    let (pool, app, token, _fx, cfx) = bootstrap().await;
    let today = hsh_erp_rust::infra::clock::now_naive().date();

    // 两件都走规则1（status=PROGRAMMING），仅 G_CODE 文件不同
    let uploaded = insert_part(
        &pool,
        cfx.l2_customer_id,
        "PG-TRI-U",
        "已编程件",
        "D-TU",
        "PROGRAMMING",
        today,
        None,
    )
    .await;
    insert_gcode_file(&pool, uploaded, "gcode/uploaded.nc").await;
    let not_uploaded = insert_part(
        &pool,
        cfx.l2_customer_id,
        "PG-TRI-N",
        "未编程件",
        "D-TN",
        "PROGRAMMING",
        today,
        None,
    )
    .await;

    let all = get_pending(&app, &token, "").await;
    assert_eq!(
        sorted(item_ids(&all)),
        sorted(vec![uploaded.to_string(), not_uploaded.to_string()]),
        "缺省 has_cnc_program → 全部: {all}"
    );

    let only_missing = get_pending(&app, &token, "has_cnc_program=false").await;
    assert_eq!(
        sorted(item_ids(&only_missing)),
        vec![not_uploaded.to_string()],
        "has_cnc_program=false → 仅未上传 G_CODE 的: {only_missing}"
    );
    assert_eq!(only_missing["data"]["items"][0]["has_cnc_program"], false);

    let only_uploaded = get_pending(&app, &token, "has_cnc_program=true").await;
    assert_eq!(
        sorted(item_ids(&only_uploaded)),
        vec![uploaded.to_string()],
        "has_cnc_program=true → 仅已上传 G_CODE 的: {only_uploaded}"
    );
    assert_eq!(only_uploaded["data"]["items"][0]["has_cnc_program"], true);
}

/// 场景 7: `keyword` 模糊（name / drawing_no / serial_no 任一命中）+ `serial_no` 精确。
#[tokio::test]
async fn keyword_and_serial_no_filters() {
    let (pool, app, token, _fx, cfx) = bootstrap().await;
    let today = hsh_erp_rust::infra::clock::now_naive().date();

    let by_name = insert_part(
        &pool,
        cfx.l2_customer_id,
        "PG-KW-1",
        "法兰盘 冲压件",
        "D-KW-1",
        "PROGRAMMING",
        today,
        None,
    )
    .await;
    let by_drawing = insert_part(
        &pool,
        cfx.l2_customer_id,
        "PG-KW-2",
        "普通件",
        "ZZ-法兰-DWG",
        "PROGRAMMING",
        today,
        None,
    )
    .await;
    let by_serial = insert_part(
        &pool,
        cfx.l2_customer_id,
        "SN-法兰-9",
        "普通件",
        "D-KW-3",
        "PROGRAMMING",
        today,
        None,
    )
    .await;
    let unrelated = insert_part(
        &pool,
        cfx.l2_customer_id,
        "PG-KW-4",
        "齿轮",
        "D-KW-4",
        "PROGRAMMING",
        today,
        None,
    )
    .await;

    let kw = get_pending(&app, &token, "keyword=%E6%B3%95%E5%85%B0").await; // URL 编码「法兰」
    assert_eq!(
        sorted(item_ids(&kw)),
        sorted(vec![
            by_name.to_string(),
            by_drawing.to_string(),
            by_serial.to_string()
        ]),
        "keyword 模糊应命中 name / drawing_no / serial_no 三处: {kw}"
    );

    let sn = get_pending(&app, &token, "serial_no=SN-%E6%B3%95%E5%85%B0-9").await;
    assert_eq!(
        item_ids(&sn),
        vec![by_serial.to_string()],
        "serial_no 精确匹配（应排除 {unrelated} 等）: {sn}"
    );
    assert_eq!(sn["data"]["total"], 1, "{sn}");
}

/// 场景 8: 排序 —— 默认 `PLANNED_DELIVERY_DATE ASC`；`DESC` 生效；非法 `sort_by` 退化。
#[tokio::test]
async fn sorting_default_desc_and_invalid_sort_by() {
    let (pool, app, token, _fx, cfx) = bootstrap().await;

    let early = insert_part(
        &pool,
        cfx.l2_customer_id,
        "PG-SORT-EARLY",
        "ZZZ-early-date",
        "D-S1",
        "PROGRAMMING",
        NaiveDate::from_ymd_opt(2026, 1, 1).expect("date"),
        None,
    )
    .await;
    let mid = insert_part(
        &pool,
        cfx.l2_customer_id,
        "PG-SORT-MID",
        "MMM-mid-date",
        "D-S2",
        "PROGRAMMING",
        NaiveDate::from_ymd_opt(2026, 2, 1).expect("date"),
        None,
    )
    .await;
    let late = insert_part(
        &pool,
        cfx.l2_customer_id,
        "PG-SORT-LATE",
        "AAA-late-date",
        "D-S3",
        "PROGRAMMING",
        NaiveDate::from_ymd_opt(2026, 3, 1).expect("date"),
        None,
    )
    .await;
    let ascending = vec![early.to_string(), mid.to_string(), late.to_string()];

    let env = get_pending(&app, &token, "").await;
    assert_eq!(
        item_ids(&env),
        ascending,
        "默认排序 = PLANNED_DELIVERY_DATE ASC: {env}"
    );

    let desc = get_pending(&app, &token, "sort_by=PLANNED_DELIVERY_DATE&sort_dir=DESC").await;
    let mut reversed = ascending.clone();
    reversed.reverse();
    assert_eq!(item_ids(&desc), reversed, "sort_dir=DESC 生效: {desc}");

    // 非法 sort_by 不报错，退化为默认列
    let invalid = get_pending(&app, &token, "sort_by=DROP_TABLE&sort_dir=ASC").await;
    assert_eq!(
        item_ids(&invalid),
        ascending,
        "非法 sort_by 退化为默认列: {invalid}"
    );

    // 白名单列 NAME 生效。name 用 ASCII（避免依赖容器 locale 下的中文 collation）：
    // 名字序（AAA-late < MMM-mid < ZZZ-early）与交期序（early < mid < late）刻意相反，
    // 这样「排序列真的换了」才可被断言捕捉。
    let by_name = get_pending(&app, &token, "sort_by=NAME&sort_dir=ASC").await;
    assert_eq!(
        item_ids(&by_name),
        vec![late.to_string(), mid.to_string(), early.to_string()],
        "sort_by=NAME 生效（按 name 字典序，与交期序相反）: {by_name}"
    );

    let by_name_desc = get_pending(&app, &token, "sort_by=NAME&sort_dir=DESC").await;
    assert_eq!(
        item_ids(&by_name_desc),
        vec![early.to_string(), mid.to_string(), late.to_string()],
        "sort_by=NAME + sort_dir=DESC 生效: {by_name_desc}"
    );
}

/// 场景 9: 分页边界 —— limit=0 → clamp 1；offset=-1 → max 0。
#[tokio::test]
async fn pagination_limit_offset_bounds() {
    let (pool, app, token, _fx, cfx) = bootstrap().await;
    let today = hsh_erp_rust::infra::clock::now_naive().date();

    for i in 0..3 {
        insert_part(
            &pool,
            cfx.l2_customer_id,
            &format!("PG-PAGE-{i}"),
            "分页件",
            "D-PG",
            "PROGRAMMING",
            today,
            None,
        )
        .await;
    }

    let env = get_pending(&app, &token, "limit=0").await;
    assert_eq!(env["data"]["limit"], 1, "limit=0 → clamp 1: {env}");
    assert_eq!(item_ids(&env).len(), 1, "limit=0 应只返回 1 条: {env}");
    assert_eq!(env["data"]["total"], 3, "total 不受 limit 影响: {env}");

    let env = get_pending(&app, &token, "limit=2&offset=1").await;
    assert_eq!(item_ids(&env).len(), 2, "limit=2 → 2 条: {env}");
    assert_eq!(env["data"]["offset"], 1, "{env}");

    let env = get_pending(&app, &token, "offset=-1").await;
    assert_eq!(
        env["data"]["offset"], 0,
        "offset=-1 → max 0 不 panic: {env}"
    );
    assert_eq!(
        item_ids(&env).len(),
        3,
        "offset 归 0 → 返回全部 3 条: {env}"
    );

    // limit / offset 允许字符串形态（前端可能发 "50"）
    let env = get_pending(&app, &token, "limit=1&offset=0").await;
    assert_eq!(env["data"]["limit"], 1, "{env}");
}

/// 场景 10: 角色守卫 —— MANAGER / CNC_PROGRAMMER 可访问；SHELF_ACCOUNT → 40300。
#[tokio::test]
async fn role_guard_manager_cnc_programmer_ok_shelf_account_403() {
    let (pool, app, token, fx, cfx) = bootstrap().await;
    let today = hsh_erp_rust::infra::clock::now_naive().date();
    insert_part(
        &pool,
        cfx.l2_customer_id,
        "PG-ROLE",
        "守卫件",
        "D-ROLE",
        "PROGRAMMING",
        today,
        None,
    )
    .await;

    // MANAGER（bootstrap 拿到的 token）
    let env = get_pending(&app, &token, "").await;
    assert_eq!(env["data"]["total"], 1, "MANAGER 应可访问: {env}");

    // CNC_PROGRAMMER —— 前端「待编程一览」页主力账号，漏角色即 403
    let cnc_token = login_user_with_role(&pool, "pg_cnc_programmer", "CNC_PROGRAMMER").await;
    let env = get_pending(&app, &cnc_token, "").await;
    assert_eq!(env["data"]["total"], 1, "CNC_PROGRAMMER 应可访问: {env}");

    // SHELF_ACCOUNT —— 无权角色
    let shelf_token = login_token(
        &app,
        &fx.part_shelf_account_username,
        ProductionFixture::PASSWORD,
    )
    .await;
    let (status, env) = send(
        app,
        json_request("GET", PENDING_URI, None, Some(&shelf_token)),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "SHELF_ACCOUNT 应 403: {env}");
    assert_eq!(env["code"], 40300, "FORBIDDEN: {env}");
}

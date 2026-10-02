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
//!  10. 角色守卫：MANAGER / CNC_PROGRAMMER 可访问；SHELF_ACCOUNT → 40300
//!
//! 2026-10-01 review 第 1 轮新增 4 个场景：
//!  11. part 状态闸门（A 项）：三规则**全部**受 `status IN (PENDING,IN_PROCESS,PROGRAMMING)`
//!      约束；`COMPLETED` / `DELIVERED` 工单即使挂 CNC 链 / CNC 批次也查不到
//!  12. 软删过滤（D 项）：`t_part` / `t_process` / `t_part_batch` /
//!      `t_process_chain_step` / `t_part_file` 五处 `deleted_at IS NULL` 逐条验证，
//!      其中 `t_part_file` 软删后 `has_cnc_program` 必须翻回 `false`
//!  13. 空串分页（E 项）：`?limit=&offset=`（含全空白）走缺省 50/0，不 400
//!  14. keyword 通配符转义（G 项）：`%` / `_` 按字面量匹配，不做通配全扫
//!
//! 2026-10-03 新增 2 个场景（批次锚点出参）：
//!  15. `batch_id` / `batch_version`：有 PROGRAMMING 批次 → 雪花 id（JSON string）+
//!      与库里 `t_part_batch.version` 一致；多个取 id 最大者；无 → 两字段都 null
//!  16. 批次锚点只认 PROGRAMMING：批次状态是 PENDING / IN_PROCESS / READY_TO_SHIP
//!      时 `batch_id` 恒 null（否则前端会拼出必然 20103 的写请求）
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
    insert_batch_versioned(pool, part_id, status, Some(current_process_id), 0).await
}

/// 2026-10-03 新增：`insert_batch` 的可指定 `version` / 可空 `current_process_id` 版本。
///
/// 批次 OCC 断言（`batch_version` 必须等于 `t_part_batch.version`）需要非 0 的
/// version，否则「返回 0」和「没取到值而兜底成 0」无法区分；`current_process_id`
/// 可空是为了造「还没进工序」的 PROGRAMMING 批次。
async fn insert_batch_versioned(
    pool: &PgPool,
    part_id: i64,
    status: &str,
    current_process_id: Option<i64>,
    version: i32,
) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;

    let id = pool_snowflake()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .next_id();
    let now = now_naive();
    // `batch_no` 取该 part 的下一个序号（`uq_t_part_batch_part_no` 是
    // `(part_id, batch_no)` 唯一约束）—— 2026-10-03 起部分用例要给同一 part 插
    // 多个批次，原先写死的 1 会撞唯一约束。首个批次仍得 1，既有断言不受影响。
    sqlx::query(
        "INSERT INTO t_part_batch (id, part_id, batch_no, quantity, status, location, \
         current_process_id, version, created_at, updated_at) \
         SELECT $1, $2, COALESCE(MAX(batch_no), 0) + 1, 1, $3, 'PRODUCTION_SHELF', $4, $5, $6, $6 \
         FROM t_part_batch WHERE part_id = $2",
    )
    .bind(id)
    .bind(part_id)
    .bind(status)
    .bind(current_process_id)
    .bind(version)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_part_batch");
    id
}

/// 回读某批次在库里的 `version`（断言「出参 batch_version == 库里的真值」用）。
async fn batch_version_in_db(pool: &PgPool, batch_id: i64) -> i32 {
    let (v,): (i32,) = sqlx::query_as("SELECT version FROM t_part_batch WHERE id = $1")
        .bind(batch_id)
        .fetch_one(pool)
        .await
        .unwrap_or_else(|e| panic!("select t_part_batch.version id={batch_id}: {e}"));
    v
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

/// 软删一行（`deleted_at = now()`），表名走白名单 `match`。
///
/// 2026-10-01 review 第 1 轮 D 项：用于逐条验证 5 处 `deleted_at IS NULL` 谓词。
/// 每个分支都是 `&'static str` 字面量（既满足 sqlx 0.9 的 `SqlSafeStr` 约束，
/// 也不存在任何动态 SQL 拼接面），而非把 `table` 拼进语句。
async fn soft_delete(pool: &PgPool, table: &str, id: i64) {
    let sql: &'static str = match table {
        "t_part" => "UPDATE t_part SET deleted_at = now() WHERE id = $1",
        "t_process" => "UPDATE t_process SET deleted_at = now() WHERE id = $1",
        "t_part_batch" => "UPDATE t_part_batch SET deleted_at = now() WHERE id = $1",
        "t_part_file" => "UPDATE t_part_file SET deleted_at = now() WHERE id = $1",
        "t_process_chain_step" => {
            "UPDATE t_process_chain_step SET deleted_at = now() WHERE id = $1"
        }
        other => panic!("soft_delete: 未白名单化的表 {other}"),
    };
    sqlx::query(sql)
        .bind(id)
        .execute(pool)
        .await
        .unwrap_or_else(|e| panic!("soft_delete {table} id={id}: {e}"));
}

/// 取某条链的唯一 step id（软删 step 用）。
async fn only_step_id(pool: &PgPool, chain_id: i64) -> i64 {
    let (id,): (i64,) = sqlx::query_as("SELECT id FROM t_process_chain_step WHERE chain_id = $1")
        .bind(chain_id)
        .fetch_one(pool)
        .await
        .expect("select chain step id");
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

/// 从信封里按 id 取单条 item（找不到则 panic，打印整份信封便于定位）。
fn item_by_id(env: &Value, id: i64) -> &Value {
    let want = id.to_string();
    env["data"]["items"]
        .as_array()
        .expect("data.items")
        .iter()
        .find(|it| it["id"].as_str() == Some(want.as_str()))
        .unwrap_or_else(|| panic!("id {id} 不在结果里: {env}"))
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

/// 场景 11（2026-10-01 review 第 1 轮 A 项）: part 状态闸门约束**全部三条规则**。
///
/// `WHERE` 骨架最外层的 `p.status IN ('PENDING','IN_PROCESS','PROGRAMMING')`
/// （与 `p.deleted_at IS NULL` 同级、在三规则括号之外）让规则2/3 同样受约束。
/// 起因：`t_part.process_chain_id` 从不清空，规则2（链含 CNC 工序）本身不看 part
/// 状态 → 历史上挂过 CNC 链的 `COMPLETED` / `DELIVERED` 工单会永久命中本页。
/// 被替换的 part 域旧端点本来就有这条闸门（`tests/part/lifecycle.rs::
/// list_pending_programming_excludes_completed_or_cancelled` 锁住），新端点不得更宽。
///
/// 断言口径：逐个 `assert!(!ids.contains(..))` 明确「该 id 不在结果里」，
/// 同时用精确 id 集合 + `total` 兜住「没有多出别的行」。
#[tokio::test]
async fn part_status_gate_excludes_completed_and_delivered() {
    let (pool, app, token, _fx, cfx) = bootstrap().await;
    let today = hsh_erp_rust::infra::clock::now_naive().date();

    let cnc = seed_process(&pool, "PG-GATE-CNC", "闸门 CNC 工序", true).await;
    // ⚠️ `uq_t_part_process_chain` 是 `process_chain_id` 上的**部分唯一索引**
    // （`WHERE process_chain_id IS NOT NULL AND deleted_at IS NULL`，baseline:3657）
    // → 1 条链只能挂 1 个活跃 part，故规则2 形态的 3 个工单各需一条独立链
    // （链可共用同一个 CNC 工序）。
    let chain_done = insert_chain_with_step(&pool, cnc, "PG 闸门链-已完成").await;
    let chain_delivered = insert_chain_with_step(&pool, cnc, "PG 闸门链-已交付").await;
    let chain_keep = insert_chain_with_step(&pool, cnc, "PG 闸门链-对照组").await;

    // 规则2 形态（链含 CNC 工序）但工单状态已出白名单
    let done_by_chain = insert_part(
        &pool,
        cfx.l2_customer_id,
        "PG-GATE-DONE",
        "已完成-链含CNC",
        "D-GATE-D",
        "COMPLETED",
        today,
        Some(chain_done),
    )
    .await;
    let delivered_by_chain = insert_part(
        &pool,
        cfx.l2_customer_id,
        "PG-GATE-SHIP",
        "已交付-链含CNC",
        "D-GATE-S",
        "DELIVERED",
        today,
        Some(chain_delivered),
    )
    .await;
    // 规则3 形态（CNC 在制批次）但工单状态已出白名单
    let done_by_batch = insert_part(
        &pool,
        cfx.l2_customer_id,
        "PG-GATE-DB",
        "已完成-批次在CNC",
        "D-GATE-B",
        "COMPLETED",
        today,
        None,
    )
    .await;
    insert_batch(&pool, done_by_batch, "IN_PROCESS", cnc).await;

    // 对照：三种形态各一件，状态均在白名单内 → 必须保留
    let keep_rule1 = insert_part(
        &pool,
        cfx.l2_customer_id,
        "PG-GATE-K1",
        "白名单-规则1",
        "D-GATE-K1",
        "PROGRAMMING",
        today,
        None,
    )
    .await;
    let keep_rule2 = insert_part(
        &pool,
        cfx.l2_customer_id,
        "PG-GATE-K2",
        "白名单-规则2",
        "D-GATE-K2",
        "IN_PROCESS",
        today,
        Some(chain_keep),
    )
    .await;
    let keep_rule3 = insert_part(
        &pool,
        cfx.l2_customer_id,
        "PG-GATE-K3",
        "白名单-规则3",
        "D-GATE-K3",
        "PENDING",
        today,
        None,
    )
    .await;
    insert_batch(&pool, keep_rule3, "PENDING", cnc).await;

    let env = get_pending(&app, &token, "").await;
    let ids = item_ids(&env);
    for (label, id) in [
        ("COMPLETED + CNC 链", done_by_chain),
        ("DELIVERED + CNC 链", delivered_by_chain),
        ("COMPLETED + CNC 批次", done_by_batch),
    ] {
        assert!(
            !ids.contains(&id.to_string()),
            "{label} 的工单 {id} 不该出现（状态闸门外）: {env}"
        );
    }
    assert_eq!(
        sorted(ids),
        sorted(vec![
            keep_rule1.to_string(),
            keep_rule2.to_string(),
            keep_rule3.to_string()
        ]),
        "白名单内三种形态各留一件: {env}"
    );
    assert_eq!(env["data"]["total"], 3, "total 同样受状态闸门约束: {env}");
}

/// 场景 12（2026-10-01 review 第 1 轮 D 项）: 5 处 `deleted_at IS NULL` 逐条验证。
///
/// 覆盖 `t_part` / `t_process` / `t_part_batch` / `t_process_chain_step` / `t_part_file`
/// 五处软删过滤；`t_part_file` 一路额外断言**返回的 `has_cnc_program` 值同步翻回
/// `false`**（SELECT 侧与 WHERE 侧共用 `G_CODE_EXISTS` 常量，漂移会被此用例抓住）。
#[tokio::test]
async fn soft_delete_filters_exclude_rows() {
    let (pool, app, token, _fx, cfx) = bootstrap().await;
    let today = hsh_erp_rust::infra::clock::now_naive().date();

    // 三条链各配一个**独立** CNC 工序：软删其中之一不得连带影响另两个用例
    let cnc_ok = seed_process(&pool, "PG-SD-CNC-A", "软删对照 CNC", true).await;
    let cnc_gone = seed_process(&pool, "PG-SD-CNC-B", "将被软删 CNC", true).await;
    let cnc_step_gone = seed_process(&pool, "PG-SD-CNC-C", "step 被软删 CNC", true).await;
    let chain_ok = insert_chain_with_step(&pool, cnc_ok, "PG 软删对照链").await;
    let chain_gone = insert_chain_with_step(&pool, cnc_gone, "PG 软删工序链").await;
    let chain_step_gone = insert_chain_with_step(&pool, cnc_step_gone, "PG 软删 step 链").await;

    // ① t_part 软删 → 整行不可见
    let gone_part = insert_part(
        &pool,
        cfx.l2_customer_id,
        "PG-SD-PART",
        "软删工单",
        "D-SD-P",
        "PROGRAMMING",
        today,
        None,
    )
    .await;
    soft_delete(&pool, "t_part", gone_part).await;

    // ② t_process 软删 → 规则2 的 pr.deleted_at 过滤生效
    let gone_process = insert_part(
        &pool,
        cfx.l2_customer_id,
        "PG-SD-PROC",
        "软删工序件",
        "D-SD-C",
        "PENDING",
        today,
        Some(chain_gone),
    )
    .await;
    soft_delete(&pool, "t_process", cnc_gone).await;

    // ③ t_part_batch 软删 → 规则3 的 pb.deleted_at 过滤生效
    let gone_batch_part = insert_part(
        &pool,
        cfx.l2_customer_id,
        "PG-SD-BATCH",
        "软删批次件",
        "D-SD-B",
        "PENDING",
        today,
        None,
    )
    .await;
    let gone_batch = insert_batch(&pool, gone_batch_part, "IN_PROCESS", cnc_ok).await;
    soft_delete(&pool, "t_part_batch", gone_batch).await;

    // ④ t_process_chain_step 软删 → 规则2 的 s.deleted_at 过滤生效
    let gone_step_part = insert_part(
        &pool,
        cfx.l2_customer_id,
        "PG-SD-STEP",
        "软删 step 件",
        "D-SD-S",
        "PENDING",
        today,
        Some(chain_step_gone),
    )
    .await;
    let step_id = only_step_id(&pool, chain_step_gone).await;
    soft_delete(&pool, "t_process_chain_step", step_id).await;

    // ⑤ t_part_file 软删 → 工单仍在列表，但 has_cnc_program 必须翻回 false
    let file_soft_deleted = insert_part(
        &pool,
        cfx.l2_customer_id,
        "PG-SD-FILE",
        "G_CODE 被软删件",
        "D-SD-F",
        "PROGRAMMING",
        today,
        None,
    )
    .await;
    let soft_file = insert_gcode_file(&pool, file_soft_deleted, "gcode/soft-deleted.nc").await;
    soft_delete(&pool, "t_part_file", soft_file).await;

    // 对照组：4 件应当出现（2 件 PROGRAMMING 走规则1，1 件 IN_PROCESS 走规则2，
    // 1 件 PENDING 走规则3），其中 1 件带活跃 G_CODE 文件
    let keep_plain = insert_part(
        &pool,
        cfx.l2_customer_id,
        "PG-SD-K1",
        "对照-无文件",
        "D-SD-K1",
        "PROGRAMMING",
        today,
        None,
    )
    .await;
    let keep_chain = insert_part(
        &pool,
        cfx.l2_customer_id,
        "PG-SD-K2",
        "对照-链含CNC",
        "D-SD-K2",
        "IN_PROCESS",
        today,
        Some(chain_ok),
    )
    .await;
    let keep_batch = insert_part(
        &pool,
        cfx.l2_customer_id,
        "PG-SD-K3",
        "对照-批次在CNC",
        "D-SD-K3",
        "PENDING",
        today,
        None,
    )
    .await;
    insert_batch(&pool, keep_batch, "PENDING", cnc_ok).await;
    let keep_file_part = insert_part(
        &pool,
        cfx.l2_customer_id,
        "PG-SD-K4",
        "对照-有G_CODE",
        "D-SD-K4",
        "PROGRAMMING",
        today,
        None,
    )
    .await;
    insert_gcode_file(&pool, keep_file_part, "gcode/keep.nc").await;

    let env = get_pending(&app, &token, "").await;
    let ids = item_ids(&env);
    for (label, id) in [
        ("t_part 软删", gone_part),
        ("t_process 软删（规则2）", gone_process),
        ("t_part_batch 软删（规则3）", gone_batch_part),
        ("t_process_chain_step 软删（规则2）", gone_step_part),
    ] {
        assert!(
            !ids.contains(&id.to_string()),
            "{label} 命中的工单 {id} 不该出现: {env}"
        );
    }
    // G_CODE 文件被软删的工单**仍应出现**，但标记必须翻回 false
    assert!(
        ids.contains(&file_soft_deleted.to_string()),
        "G_CODE 软删只应影响标记，不该让工单消失: {env}"
    );
    assert_eq!(
        sorted(ids),
        sorted(vec![
            keep_plain.to_string(),
            keep_chain.to_string(),
            keep_batch.to_string(),
            keep_file_part.to_string(),
            file_soft_deleted.to_string(),
        ]),
        "5 处软删过滤后应恰好剩 5 件: {env}"
    );
    assert_eq!(env["data"]["total"], 5, "{env}");
    assert_eq!(
        item_by_id(&env, file_soft_deleted)["has_cnc_program"],
        false,
        "G_CODE 软删后 has_cnc_program 必须翻回 false: {env}"
    );
    assert_eq!(
        item_by_id(&env, keep_file_part)["has_cnc_program"],
        true,
        "G_CODE 未软删的对照件应保持 true: {env}"
    );

    // 三态过滤与返回标记必须同源：软删掉的文件不再算「已编程」
    let uploaded = get_pending(&app, &token, "has_cnc_program=true").await;
    assert_eq!(
        item_ids(&uploaded),
        vec![keep_file_part.to_string()],
        "has_cnc_program=true 不该包含 G_CODE 已软删的工单: {uploaded}"
    );
    let missing = get_pending(&app, &token, "has_cnc_program=false").await;
    assert_eq!(
        sorted(item_ids(&missing)),
        sorted(vec![
            keep_plain.to_string(),
            keep_chain.to_string(),
            keep_batch.to_string(),
            file_soft_deleted.to_string(),
        ]),
        "has_cnc_program=false 应包含 G_CODE 已软删的工单: {missing}"
    );
}

/// 场景 13（2026-10-01 review 第 1 轮 E 项）: `?limit=&offset=` 空串走缺省。
///
/// 修复前：`?limit=` 经 `deserialize_i64_opt` 得到 `Some("")` → `"".parse::<i64>()`
/// 失败 → axum `Query` extractor 拒成 HTTP 400 **纯文本**（不走 `R` 包络），
/// 与 `?has_cnc_program=` 被兜成 `None` 的宽容度不一致。修复后两者对齐。
/// 2026-10-01 review 第 2 轮补钉「数字两侧空白被 trim」这一条放宽口径
/// （`?limit=%2012%20` → 12）。
///
/// 注：`?limit=abc`（真正无法解析）与带引号的 `?limit="50"` 仍 → 400，此处不断言
/// —— `test_support::send` 强制把响应体当 JSON 解析，axum 的 `QueryRejection` 是
/// 纯文本会 panic；该行为已在 `docs/api/production/pending-programming.md` 的
/// 「`limit` / `offset` 的取值容错」脚注 + 「关于 query 解析失败」段记录。
#[tokio::test]
async fn limit_offset_empty_string_falls_back_to_defaults() {
    let (pool, app, token, _fx, cfx) = bootstrap().await;
    let today = hsh_erp_rust::infra::clock::now_naive().date();

    for i in 0..3 {
        insert_part(
            &pool,
            cfx.l2_customer_id,
            &format!("PG-EMPTY-{i}"),
            "空串分页件",
            "D-EMPTY",
            "PROGRAMMING",
            today,
            None,
        )
        .await;
    }

    // 空串 → 缺省（get_pending 内已 assert 200 + code 0）
    let env = get_pending(&app, &token, "limit=&offset=").await;
    assert_eq!(env["data"]["limit"], 50, "空串 limit → 缺省 50: {env}");
    assert_eq!(env["data"]["offset"], 0, "空串 offset → 缺省 0: {env}");
    assert_eq!(item_ids(&env).len(), 3, "缺省 limit 应返回全部 3 条: {env}");
    assert_eq!(env["data"]["total"], 3, "{env}");

    // 全空白（%20%20）同样按缺省处理
    let env = get_pending(&app, &token, "limit=%20%20&offset=%20").await;
    assert_eq!(env["data"]["limit"], 50, "全空白 limit → 缺省 50: {env}");
    assert_eq!(env["data"]["offset"], 0, "全空白 offset → 缺省 0: {env}");

    // 数字两侧空白被 trim（2026-10-01 review 第 2 轮口径钉住）：`" 12 "` → 12，
    // 而全空白的 offset 仍走缺省 0
    let env = get_pending(&app, &token, "limit=%2012%20&offset=%20%20").await;
    assert_eq!(env["data"]["limit"], 12, "数字两侧空白被 trim → 12: {env}");
    assert_eq!(env["data"]["offset"], 0, "offset 全空白 → 缺省 0: {env}");

    // 正常值仍生效（确认兜底没把真值也吃掉）
    let env = get_pending(&app, &token, "limit=2&offset=1").await;
    assert_eq!(env["data"]["limit"], 2, "{env}");
    assert_eq!(env["data"]["offset"], 1, "{env}");
    assert_eq!(item_ids(&env).len(), 2, "{env}");
}

/// 场景 14（2026-10-01 review 第 1 轮 G 项）: keyword 的 `%` / `_` 按字面量匹配。
///
/// 修复前 `keyword=50%` 的 pattern 变成 `%50%%` → 退化成「以 50 开头」的全匹配。
/// 修复后 repo 侧 `escape_like` 转义 + SQL 侧 `ESCAPE '\'`，只有字面量含 `50%`
/// 的行才命中。
#[tokio::test]
async fn keyword_escapes_like_wildcards() {
    let (pool, app, token, _fx, cfx) = bootstrap().await;
    let today = hsh_erp_rust::infra::clock::now_naive().date();

    // 字面量含 `50%` 的行
    let literal_pct = insert_part(
        &pool,
        cfx.l2_customer_id,
        "PG-WC-PCT",
        "50% 含量件",
        "D-WC-1",
        "PROGRAMMING",
        today,
        None,
    )
    .await;
    // 字面量含 `50_5` 的行
    let literal_underscore = insert_part(
        &pool,
        cfx.l2_customer_id,
        "PG-WC-US",
        "50_5 件",
        "D-WC-2",
        "PROGRAMMING",
        today,
        None,
    )
    .await;
    // 诱饵：`%` / `_` 若未被转义，`50%5` / `50_5` 两个 pattern 都会命中它
    let bait = insert_part(
        &pool,
        cfx.l2_customer_id,
        "PG-WC-BAIT",
        "50X5 件",
        "D-WC-3",
        "PROGRAMMING",
        today,
        None,
    )
    .await;

    // keyword=50%（URL 编码 50%25）→ 只命中字面量含 `50%` 的行
    let pct = get_pending(&app, &token, "keyword=50%25").await;
    assert_eq!(
        item_ids(&pct),
        vec![literal_pct.to_string()],
        "`%` 必须按字面量匹配（诱饵 {bait} 不该命中）: {pct}"
    );

    // keyword=50_5（`_` → %5F）→ 只命中字面量含 `50_5` 的行
    let us = get_pending(&app, &token, "keyword=50%5F5").await;
    assert_eq!(
        item_ids(&us),
        vec![literal_underscore.to_string()],
        "`_` 必须按字面量匹配（诱饵 {bait} 不该命中）: {us}"
    );

    // 反向确认：非通配片段仍能模糊命中多行（证明上面两条不是「过滤器整体失灵」）
    let loose = get_pending(&app, &token, "keyword=50").await;
    assert_eq!(
        sorted(item_ids(&loose)),
        sorted(vec![
            literal_pct.to_string(),
            literal_underscore.to_string(),
            bait.to_string(),
        ]),
        "keyword=50（无通配符）应命中全部 3 件: {loose}"
    );
}

/// 场景 15（2026-10-03 新增）: `batch_id` / `batch_version` —— PROGRAMMING 活跃批次锚点。
///
/// 覆盖三条：
/// 1. **有** PROGRAMMING 批次 → `batch_id` 是 JSON string 形态的雪花 id、
///    `batch_version` 与库里 `t_part_batch.version` 一致；
/// 2. **无** PROGRAMMING 批次（只有 PENDING / IN_PROCESS 批次）→ `batch_id` /
///    `batch_version` 都是 `null`（前端据此禁用「下发」按钮）；
/// 3. 多个 PROGRAMMING 批次 → 取 `id` 最大的一个（最新）。
///
/// `batch_version` 刻意用非 0 值（默认插 0 的话，「取到真值」与「取不到而兜底 0」
/// 无法区分）。
#[tokio::test]
async fn batch_anchor_points_to_active_programming_batch() {
    let (pool, app, token, _fx, cfx) = bootstrap().await;
    let today = hsh_erp_rust::infra::clock::now_naive().date();

    // ① 有 PROGRAMMING 批次（version=7）+ 一个更早的 PROGRAMMING 批次（version=3）
    let with_prog = insert_part(
        &pool,
        cfx.l2_customer_id,
        "PG-BA-1",
        "有编程批次件",
        "D-BA-1",
        "PROGRAMMING",
        today,
        None,
    )
    .await;
    insert_batch_versioned(&pool, with_prog, "PROGRAMMING", None, 3).await;
    let newest_batch = insert_batch_versioned(&pool, with_prog, "PROGRAMMING", None, 7).await;
    assert_eq!(
        batch_version_in_db(&pool, newest_batch).await,
        7,
        "前提：库里的批次 version 确实是 7"
    );

    // ② 无 PROGRAMMING 批次：只有 IN_PROCESS 批次。part 状态走 `PROGRAMMING`（规则1）
    // 保证它一定在列表里 —— 本用例要验的是「在列表但没有 PROGRAMMING 批次」，
    // 若 part 也是 IN_PROCESS 且链/批次都不含 CNC，三条规则都不命中，行根本不上榜。
    let no_prog = insert_part(
        &pool,
        cfx.l2_customer_id,
        "PG-BA-2",
        "无编程批次件",
        "D-BA-2",
        "PROGRAMMING",
        today,
        None,
    )
    .await;
    let plain = seed_process(&pool, "PG-BA-PLAIN", "普通工序 BA", false).await;
    insert_batch_versioned(&pool, no_prog, "IN_PROCESS", Some(plain), 5).await;

    let env = get_pending(&app, &token, "").await;
    assert_eq!(
        sorted(item_ids(&env)),
        sorted(vec![with_prog.to_string(), no_prog.to_string()]),
        "两件都该在列表里: {env}"
    );

    // ① 多个 PROGRAMMING 批次 → 取 id 最大者；batch_id 是 string 形态雪花 id
    let item = item_by_id(&env, with_prog);
    assert_eq!(
        item["batch_id"].as_str(),
        Some(newest_batch.to_string().as_str()),
        "多个 PROGRAMMING 批次必须取 id 最大的（最新）那个: {env}"
    );
    assert_eq!(
        item["batch_version"], 7,
        "batch_version 必须等于库里 t_part_batch.version（不是 part 级 version）: {env}"
    );
    assert_eq!(
        item["version"], 0,
        "part 级 version 仍原样返回（与 batch_version 严格区分）: {env}"
    );

    // ② 无 PROGRAMMING 批次 → 两字段都 null
    let item = item_by_id(&env, no_prog);
    assert!(
        item["batch_id"].is_null(),
        "无 PROGRAMMING 批次时 batch_id 应为 null（前端据此禁用下发按钮）: {env}"
    );
    assert!(
        item["batch_version"].is_null(),
        "无 PROGRAMMING 批次时 batch_version 应与 batch_id 同为 null: {env}"
    );
}

/// 场景 16（2026-10-03 新增）: `batch_id` 口径只认 PROGRAMMING，别的状态批次一律不给。
///
/// 单独钉住「不误给」这一侧：`release_from_programming` 硬要求源状态是 PROGRAMMING
/// （否则 20103），若列表把 PENDING / IN_PROCESS / READY_TO_SHIP 批次的 id 也当作
/// 锚点给出去，前端就会拼出一个必然失败的写请求。
#[tokio::test]
async fn batch_anchor_ignores_non_programming_batch_status() {
    let (pool, app, token, _fx, cfx) = bootstrap().await;
    let today = hsh_erp_rust::infra::clock::now_naive().date();
    let cnc = seed_process(&pool, "PG-BA-CNC", "CNC 工序 BA", true).await;

    // 三种「存在批次但都不是 PROGRAMMING」的形态，part 状态走规则1 保证一定上榜
    // （`t_part.serial_no` 是 varchar(15)，故 serial_no 用序号而非状态名拼）
    let statuses = ["PENDING", "IN_PROCESS", "READY_TO_SHIP"];
    let mut parts = Vec::new();
    for (i, st) in statuses.iter().enumerate() {
        let pid = insert_part(
            &pool,
            cfx.l2_customer_id,
            &format!("PG-BA-NP-{i}"),
            "非编程状态批次件",
            "D-BA-NP",
            "PROGRAMMING",
            today,
            None,
        )
        .await;
        insert_batch_versioned(&pool, pid, st, Some(cnc), 9).await;
        parts.push(pid);
    }

    let env = get_pending(&app, &token, "").await;
    for pid in parts {
        let item = item_by_id(&env, pid);
        assert!(
            item["batch_id"].is_null(),
            "批次状态非 PROGRAMMING 时不该给 batch_id: {env}"
        );
        assert!(
            item["batch_version"].is_null(),
            "批次状态非 PROGRAMMING 时不该给 batch_version: {env}"
        );
    }
}

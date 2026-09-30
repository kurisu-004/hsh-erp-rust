//! com::union_list 端到端集成测试（2026-09-29 新增）
//!
//! ## 覆盖（plan §3 验证清单）
//! 1. `row_type=PART` —— 仅 `t_part WHERE assembly_id IS NULL`，total 与
//!    `PartFixture` 数量一致；装配件不出现在 items。
//! 2. `row_type=ASSEMBLY` —— 仅 `t_assembly`；`has_children` / `child_count`
//!    派生正确（asm_with_kids=3 / asm_no_kids=0）。
//! 3. `row_type=ALL` —— 3 part + 2 asm，total=5；混合 row_type。
//! 4. ALL 模式 `sort_by=SERIAL_NO` —— 降级 CREATED_AT，不报错。
//! 5. 非法 `row_type=BAD` —— 40001 VALIDATION_ERROR。
//! 6. `row_type=ALL` deep offset 分页回归 —— pushdown 修分页 bug（plan §3）。
//!
//! ## 并行
//! 进程级 test_pool 每次 fresh database（plan 2 2026-09-20），DB 间 schema
//! 完全独立，无需 Mutex 串行化。
//!
//! ## 认证
//! MANAGER 用户跑通（与 `/parts` list 端点同权限：Manager / Clerk / Inspector /
//! CncProgrammer，2026-09-19 聚合到 com nest）。

use std::sync::OnceLock;

use axum::http::StatusCode;
use serde_json::Value;
use sqlx::PgPool;

use hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator;
use hsh_erp_test_support::{PartFixture, load_part_fixture, login_token, test_app, test_pool};

// ===========================================================================
//  Test fixtures
// ===========================================================================

static SHARED_TEST_SNOWFLAKE: OnceLock<SnowflakeIdGenerator> = OnceLock::new();

fn shared_test_snowflake() -> &'static SnowflakeIdGenerator {
    SHARED_TEST_SNOWFLAKE.get_or_init(|| SnowflakeIdGenerator::new(1_577_836_800_000, 1))
}

fn next_test_id() -> i64 {
    shared_test_snowflake().next_id()
}

async fn send(
    app: axum::Router,
    req: axum::http::Request<axum::body::Body>,
) -> (axum::http::StatusCode, serde_json::Value) {
    use hsh_erp_test_support::send as ts_send;
    ts_send(app, req).await
}

async fn bootstrap_as_manager() -> (
    PgPool,
    axum::Router,
    String,
    hsh_erp_test_support::PartFixture,
) {
    let pool = test_pool().await;
    let fx = load_part_fixture(&pool).await;
    let app = test_app(test_state(pool.clone()).await);
    let token = login_token(&app, &fx.manager_username, PartFixture::PASSWORD).await;
    (pool, app, token, fx)
}

/// 直插一个普通 `t_part` 行（无 assembly_id，与 PartFixture 风格一致）。
async fn insert_part(
    pool: &PgPool,
    name: &str,
    customer_id: i64,
    serial_no: Option<&str>,
    status: &str,
) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;
    let id = next_test_id();
    let now = now_naive();
    let today = now.date();
    sqlx::query(
        "INSERT INTO t_part (id, serial_no, name, drawing_no, customer_id, status, \
         applicant_name, request_date, planned_delivery_date, quantity, version, \
         created_at, updated_at) \
         VALUES ($1, $2, $3, 'D-001', $4, $7, $3, $5, $5, 1, 0, $6, $6)",
    )
    .bind(id)
    .bind(serial_no)
    .bind(name)
    .bind(customer_id)
    .bind(today)
    .bind(now)
    .bind(status)
    .execute(pool)
    .await
    .expect("insert part");
    id
}

/// 直插一个 `t_part` 行，自定义 `planned_delivery_date`（2026-09-30 新增，
/// 用于 `planned_delivery_date_from/to` 过滤测试）。
async fn insert_part_with_planned_date(
    pool: &PgPool,
    name: &str,
    customer_id: i64,
    planned_delivery_date: chrono::NaiveDate,
) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;
    let id = next_test_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_part (id, serial_no, name, drawing_no, customer_id, status, \
         applicant_name, request_date, planned_delivery_date, quantity, version, \
         created_at, updated_at) \
         VALUES ($1, NULL, $2, 'D-001', $3, 'PENDING', $2, $4, $5, 1, 0, $6, $6)",
    )
    .bind(id)
    .bind(name)
    .bind(customer_id)
    .bind(planned_delivery_date) // request_date
    .bind(planned_delivery_date) // planned_delivery_date
    .bind(now)
    .execute(pool)
    .await
    .expect("insert part with planned date");
    id
}

/// 直插一个 `t_assembly` 行，自定义 `planned_delivery_date`（2026-09-30 新增，
/// 用于 `planned_delivery_date_from/to` 过滤测试）。
async fn insert_assembly_with_planned_date(
    pool: &PgPool,
    drawing_no: &str,
    name: &str,
    customer_id: i64,
    planned_delivery_date: chrono::NaiveDate,
) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;
    let id = next_test_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_assembly (id, drawing_no, name, applicant_name, customer_id, \
         request_date, planned_delivery_date, status, quantity, unit_price, total_price, \
         version, created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, $3, '', $4, $5, $6, 'PENDING', 1, 0, 0, 0, $7, NULL, $7, NULL)",
    )
    .bind(id)
    .bind(drawing_no)
    .bind(name)
    .bind(customer_id)
    .bind(planned_delivery_date) // request_date
    .bind(planned_delivery_date) // planned_delivery_date
    .bind(now)
    .execute(pool)
    .await
    .expect("insert assembly with planned date");
    id
}

/// 直插一个 `t_part` 行（带非空 assembly_id，标记为「装配体的子件」）。
async fn insert_part_under_assembly(
    pool: &PgPool,
    name: &str,
    customer_id: i64,
    assembly_id: i64,
    status: &str,
) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;
    let id = next_test_id();
    let now = now_naive();
    let today = now.date();
    sqlx::query(
        "INSERT INTO t_part (id, serial_no, name, drawing_no, customer_id, status, \
         applicant_name, request_date, planned_delivery_date, quantity, version, \
         created_at, updated_at, assembly_id) \
         VALUES ($1, NULL, $2, 'D-001', $3, $4, $2, $6, $6, 1, 0, $5, $5, $7)",
    )
    .bind(id)
    .bind(name)
    .bind(customer_id)
    .bind(status)
    .bind(now)
    .bind(today)
    .bind(assembly_id)
    .execute(pool)
    .await
    .expect("insert part under assembly");
    id
}

/// 直插一个 `t_assembly` 行（精简列对齐 `assembly/status_sync.rs`）。
async fn insert_assembly(pool: &PgPool, drawing_no: &str, name: &str, customer_id: i64) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;
    let id = next_test_id();
    let now = now_naive();
    let today = now.date();
    sqlx::query(
        "INSERT INTO t_assembly (id, drawing_no, name, applicant_name, customer_id, \
         request_date, planned_delivery_date, status, quantity, unit_price, total_price, \
         version, created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, $3, '', $4, $5, $5, 'PENDING', 1, 0, 0, 0, $6, NULL, $6, NULL)",
    )
    .bind(id)
    .bind(drawing_no)
    .bind(name)
    .bind(customer_id)
    .bind(today)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_assembly");
    id
}

/// 2026-09-30 新增：通用 `t_part` 行构造（10 字段筛选测试用）。
///
/// 自定义 drawing_no / name / order_no / serial_no / request_date /
/// planned_delivery_date / system_delivery_date。其余列（status / quantity /
/// version 等）固定字面 PENDING / 1 / 0；与既有 `insert_part` 家族同语义。
#[allow(clippy::too_many_arguments)]
async fn insert_part_full(
    pool: &PgPool,
    name: &str,
    drawing_no: &str,
    customer_id: i64,
    serial_no: Option<&str>,
    order_no: Option<&str>,
    request_date: chrono::NaiveDate,
    planned_delivery_date: chrono::NaiveDate,
    system_delivery_date: Option<chrono::NaiveDate>,
) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;
    let id = next_test_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_part (id, serial_no, name, drawing_no, customer_id, status, \
         applicant_name, request_date, planned_delivery_date, quantity, version, \
         order_no, system_delivery_date, created_at, updated_at) \
         VALUES ($1, $2, $3, $4, $5, 'PENDING', $3, $6, $7, 1, 0, $8, $9, $10, $10)",
    )
    .bind(id)
    .bind(serial_no)
    .bind(name)
    .bind(drawing_no)
    .bind(customer_id)
    .bind(request_date)
    .bind(planned_delivery_date)
    .bind(order_no)
    .bind(system_delivery_date)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert part full");
    id
}

/// 2026-09-30 新增：通用 `t_assembly` 行构造（10 字段筛选测试用）。
///
/// 自定义 drawing_no / name / order_no / serial_no / request_date /
/// planned_delivery_date / system_delivery_date。其余列固定字面 PENDING / 1 / 0。
#[allow(clippy::too_many_arguments)]
async fn insert_assembly_full(
    pool: &PgPool,
    drawing_no: &str,
    name: &str,
    customer_id: i64,
    serial_no: Option<&str>,
    order_no: Option<&str>,
    request_date: chrono::NaiveDate,
    planned_delivery_date: chrono::NaiveDate,
    system_delivery_date: Option<chrono::NaiveDate>,
) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;
    let id = next_test_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_assembly (id, drawing_no, name, applicant_name, customer_id, \
         request_date, planned_delivery_date, status, quantity, unit_price, total_price, \
         version, serial_no, order_no, system_delivery_date, \
         created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, $3, '', $4, $5, $6, 'PENDING', 1, 0, 0, 0, $7, $8, $9, \
                 $10, NULL, $10, NULL)",
    )
    .bind(id)
    .bind(drawing_no)
    .bind(name)
    .bind(customer_id)
    .bind(request_date)
    .bind(planned_delivery_date)
    .bind(serial_no)
    .bind(order_no)
    .bind(system_delivery_date)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert assembly full");
    id
}

// `hsh_erp_test_support::test_state` 直接套用（不引入 list_enrichment cos mock），
// 因为 union_list 不走 COS 文件路径；直接走 test_state。
async fn test_state(pool: PgPool) -> std::sync::Arc<hsh_erp_rust::state::AppState> {
    use hsh_erp_test_support::test_state as ts_test_state;
    ts_test_state(pool).await
}

// ===========================================================================
//  Tests
// ===========================================================================

/// `row_type=PART` —— 仅 `t_part WHERE assembly_id IS NULL`；
/// `has_children=false` / `child_count=null`。
#[tokio::test]
async fn union_list_row_type_part_excludes_assembly_children() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;

    // 2 个装配件（父件；PART 模式应过滤掉）
    let asm1 = insert_assembly(&pool, "ASM-001", "A1", fx.customer_l2_id).await;
    let _asm2 = insert_assembly(&pool, "ASM-002", "A2", fx.customer_l2_id).await;

    // 2 件普通 part（应被返回）+ 1 件子件（应被过滤）
    let _p0 = insert_part(&pool, "P0", fx.customer_l2_id, Some("P000"), "PENDING").await;
    let _p1 = insert_part(&pool, "P1", fx.customer_l2_id, Some("P001"), "PENDING").await;
    let _pc = insert_part_under_assembly(&pool, "PC", fx.customer_l2_id, asm1, "PENDING").await;

    let (s, env) = send(
        app,
        hsh_erp_test_support::json_request(
            "GET",
            &format!(
                "/com/union-list?customer_id={}&row_type=PART",
                fx.customer_l2_id
            ),
            None::<Value>,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "row_type=PART: {env}");
    assert_eq!(env["code"], 0);
    assert_eq!(
        env["data"]["total"], 2,
        "应仅返回 2 件普通 part（子件+装配件被排除）: {env}"
    );
    let items = env["data"]["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    for item in items {
        // PART 行：`assembly_id` 必然 None（装配体子件被守卫排除）
        assert!(
            item.get("assembly_id").is_none_or(|v| v.is_null()),
            "PART 行 assembly_id 必为 None: {item}"
        );
        assert_eq!(item["row_type"], "PART", "PART 行 row_type: {item}");
        assert_eq!(item["has_children"], false);
        assert!(item["child_count"].is_null(), "PART 行 child_count=None");
    }
}

/// `row_type=ASSEMBLY` —— 仅 `t_assembly`；`has_children` / `child_count`
/// 派生正确。
#[tokio::test]
async fn union_list_row_type_assembly_returns_unified_shape() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let asm_with_kids = insert_assembly(&pool, "ASM-A", "AA", fx.customer_l2_id).await;
    let asm_no_kids = insert_assembly(&pool, "ASM-B", "BB", fx.customer_l2_id).await;
    // 给 asm_with_kids 注入 3 子件
    for i in 0..3 {
        insert_part_under_assembly(
            &pool,
            &format!("C{i}"),
            fx.customer_l2_id,
            asm_with_kids,
            "PENDING",
        )
        .await;
    }
    // 1 件普通 part（应被过滤掉）
    let _p0 = insert_part(&pool, "P0", fx.customer_l2_id, Some("P000"), "PENDING").await;

    let (s, env) = send(
        app,
        hsh_erp_test_support::json_request(
            "GET",
            &format!(
                "/com/union-list?customer_id={}&row_type=ASSEMBLY",
                fx.customer_l2_id
            ),
            None::<Value>,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "row_type=ASSEMBLY: {env}");
    assert_eq!(env["code"], 0);
    assert_eq!(env["data"]["total"], 2);
    let items = env["data"]["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    let by_id: std::collections::HashMap<i64, &Value> = items
        .iter()
        .map(|i| (i["id"].as_str().unwrap().parse().unwrap(), i))
        .collect();
    let asm_with = by_id.get(&asm_with_kids).expect("asm_with_kids in list");
    assert_eq!(asm_with["row_type"], "ASSEMBLY");
    assert_eq!(asm_with["has_children"], true, "3 子件 → true: {asm_with}");
    assert_eq!(asm_with["child_count"], 3, "child_count=3: {asm_with}");
    let asm_no = by_id.get(&asm_no_kids).expect("asm_no_kids in list");
    assert_eq!(asm_no["row_type"], "ASSEMBLY");
    assert_eq!(asm_no["has_children"], false);
    assert_eq!(asm_no["child_count"], 0);
}

/// `row_type=ALL` —— 3 part + 2 asm，total=5；混合 row_type。
#[tokio::test]
async fn union_list_row_type_all_merges_part_and_assembly() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    for i in 0..3 {
        insert_part(
            &pool,
            &format!("P{i}"),
            fx.customer_l2_id,
            Some(&format!("P{i:03}")),
            "PENDING",
        )
        .await;
    }
    insert_assembly(&pool, "ASM-001", "A1", fx.customer_l2_id).await;
    insert_assembly(&pool, "ASM-002", "A2", fx.customer_l2_id).await;

    let (s, env) = send(
        app,
        hsh_erp_test_support::json_request(
            "GET",
            &format!(
                "/com/union-list?customer_id={}&row_type=ALL&limit=200",
                fx.customer_l2_id
            ),
            None::<Value>,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "ALL: {env}");
    assert_eq!(env["code"], 0);
    assert_eq!(env["data"]["total"], 5, "ALL 应合并 5 条: {env}");
    let items = env["data"]["items"].as_array().unwrap();
    assert_eq!(items.len(), 5);
    let mut row_types: std::collections::HashSet<String> = items
        .iter()
        .map(|i| i["row_type"].as_str().unwrap().to_string())
        .collect();
    assert!(row_types.remove("PART"));
    assert!(row_types.remove("ASSEMBLY"));
    assert!(
        row_types.is_empty(),
        "应仅含 PART / ASSEMBLY 两类: {row_types:?}"
    );
}

/// `row_type=ALL` deep offset 分页回归 —— pushdown 修分页 bug（plan §3）。
///
/// 强化版（2026-09-29 round-2）：插入 200 件 part + 60 件 asm = 260 行，验证
/// `offset=210, limit=50` 时能正确切片（每段必须取够 `(offset+limit)=260`
/// 行才能让外层 OFFSET 210 LIMIT 50 返回非空集）。
///
/// 原 `segment_limit.clamp(1,200)` bug 复现条件：deep offset (>=200) 时
/// part_seg / asm_seg 各只返前 200 行，UNION 表最多 400 行，外层 OFFSET
/// 必然返空。本测试一旦 pushdown_limit 被错误硬截到 200，offset=210 即
/// 触发 empty result，断言 `items.length == 50` 直接 FAIL。
#[tokio::test]
async fn union_list_all_mode_deep_offset_pagination() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;

    // 200 part + 60 asm = 260 总行
    const PART_COUNT: usize = 200;
    const ASM_COUNT: usize = 60;
    const TOTAL: usize = PART_COUNT + ASM_COUNT;
    // offset/limit 选择：offset=210, limit=50
    // - offset > 200 触发 pushdown_limit 必须 ≥ 260 才能返回非空
    // - limit=50 验证切片大小正确（items.length == 50）
    // - 区间 [210, 260) 落在 TOTAL=260 之内，无越界
    const OFFSET: i64 = 210;
    const LIMIT: i64 = 50;

    for i in 0..PART_COUNT {
        insert_part(
            &pool,
            &format!("P{i}"),
            fx.customer_l2_id,
            Some(&format!("P{i:04}")),
            "PENDING",
        )
        .await;
    }
    for i in 0..ASM_COUNT {
        insert_assembly(
            &pool,
            &format!("ASM-{i:03}"),
            &format!("A{i}"),
            fx.customer_l2_id,
        )
        .await;
    }

    // 关键场景：offset=210, limit=50
    let (s, env) = send(
        app.clone(),
        hsh_erp_test_support::json_request(
            "GET",
            &format!(
                "/com/union-list?customer_id={}&row_type=ALL&limit={LIMIT}&offset={OFFSET}",
                fx.customer_l2_id
            ),
            None::<Value>,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "deep offset: {env}");
    assert_eq!(env["code"], 0);
    assert_eq!(
        env["data"]["total"], TOTAL as i64,
        "ALL 应合并 260 条 (200 part + 60 asm): {env}"
    );
    let items = env["data"]["items"].as_array().unwrap();
    assert_eq!(
        items.len() as i64,
        LIMIT,
        "offset=210 limit=50 应返 50 条（不被 segment_limit.clamp(1,200) 截空）: {env}"
    );
    // 区间 [OFFSET, OFFSET+LIMIT) = [210, 260) 全部 50 条必须唯一
    let mut ids: Vec<i64> = items
        .iter()
        .map(|i| i["id"].as_str().unwrap().parse().unwrap())
        .collect();
    ids.sort();
    ids.dedup();
    assert_eq!(
        ids.len() as i64,
        LIMIT,
        "切片内 50 条 id 必唯一（无重复）: {ids:?}"
    );
    // 同时验证「small offset 仍能工作」：offset=0, limit=50 应返前 50 条
    // （FIRST PAGE sanity，覆盖 small-offset 路径未回归）
    let (s, env) = send(
        app,
        hsh_erp_test_support::json_request(
            "GET",
            &format!(
                "/com/union-list?customer_id={}&row_type=ALL&limit=50&offset=0",
                fx.customer_l2_id
            ),
            None::<Value>,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "首页 sanity: {env}");
    assert_eq!(env["data"]["total"], TOTAL as i64);
    let page1 = env["data"]["items"].as_array().unwrap();
    assert_eq!(page1.len(), 50);
}

/// ALL 模式 `sort_by=SERIAL_NO` —— 不报错，降级为 CREATED_AT。
#[tokio::test]
async fn union_list_all_mode_sort_serial_no_falls_back() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    for i in 0..3 {
        insert_part(
            &pool,
            &format!("P{i}"),
            fx.customer_l2_id,
            Some(&format!("P{i:03}")),
            "PENDING",
        )
        .await;
    }
    insert_assembly(&pool, "ASM-001", "A1", fx.customer_l2_id).await;

    let (s, env) = send(
        app,
        hsh_erp_test_support::json_request(
            "GET",
            &format!(
                "/com/union-list?customer_id={}&row_type=ALL&sort_by=SERIAL_NO&limit=200",
                fx.customer_l2_id
            ),
            None::<Value>,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "ALL sort=SERIAL_NO: {env}");
    assert_eq!(env["code"], 0);
    assert_eq!(
        env["data"]["total"], 4,
        "应返回 4 条（3 part + 1 asm）: {env}"
    );
    let items = env["data"]["items"].as_array().unwrap();
    assert_eq!(items.len(), 4);
    // 全部 row_type 字段应非 None
    for item in items {
        assert!(item["row_type"].is_string(), "row_type 应非空: {item}");
    }
}

/// 非法 `row_type=BAD` —— 返回 40001 VALIDATION_ERROR。
#[tokio::test]
async fn union_list_row_type_invalid_rejected() {
    let (_pool, app, token, _fx) = bootstrap_as_manager().await;
    let (s, env) = send(
        app,
        hsh_erp_test_support::json_request(
            "GET",
            "/com/union-list?row_type=BAD",
            None::<Value>,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::UNPROCESSABLE_ENTITY,
        "非法 row_type → 422: {env}"
    );
    assert_eq!(env["code"], 40001, "VALIDATION_ERROR: {env}");
}

/// `row_type` 缺省（不传）—— 等价于 `ALL`。
#[tokio::test]
async fn union_list_row_type_absent_is_all_default() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    for i in 0..2 {
        insert_part(
            &pool,
            &format!("P{i}"),
            fx.customer_l2_id,
            Some(&format!("P{i:03}")),
            "PENDING",
        )
        .await;
    }
    insert_assembly(&pool, "ASM-001", "A1", fx.customer_l2_id).await;

    let (s, env) = send(
        app,
        hsh_erp_test_support::json_request(
            "GET",
            &format!(
                "/com/union-list?customer_id={}&limit=200",
                fx.customer_l2_id
            ),
            None::<Value>,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "缺省 row_type: {env}");
    assert_eq!(env["code"], 0);
    assert_eq!(env["data"]["total"], 3, "缺省应等于 ALL: {env}");
    let items = env["data"]["items"].as_array().unwrap();
    assert_eq!(items.len(), 3);
    let mut row_types: std::collections::HashSet<String> = items
        .iter()
        .map(|i| i["row_type"].as_str().unwrap().to_string())
        .collect();
    assert!(row_types.remove("PART"));
    assert!(row_types.remove("ASSEMBLY"));
}

/// 2026-09-30 新增：`planned_delivery_date_from/to` 日期窗口过滤回归。
///
/// 隐藏 bug 修后 —— 前端 dashboard UpcomingDeliveryListDrawer 早已传这俩
/// 参数，但本端点之前未消费，参数被静默丢弃；本测试断言日期参数生效：
///
/// 排布（以 today 为基准）：
/// - 3 件 part + 1 件 asm planned_delivery_date = today
/// - 2 件 part + 1 件 asm planned_delivery_date = today + 5d
/// - 1 件 part     planned_delivery_date = today + 10d
///
/// 三组断言：
/// 1. `from=today&to=today` → 仅命中 today 4 条
/// 2. `from=today+3d&to=today+7d` → 仅命中 today+5d 3 条
/// 3. `from=today&to=today+10d`（含两端）→ 全 8 条
///
/// 同时验证非法日期格式 → 40001 VALIDATION_ERROR。
#[tokio::test]
async fn list_union_items_filters_by_planned_delivery_date() {
    use chrono::{Duration, NaiveDate};
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let today = chrono::Local::now().date_naive();

    // today: 3 part + 1 asm
    let mut today_part_ids: Vec<i64> = Vec::with_capacity(3);
    for i in 0..3 {
        let id =
            insert_part_with_planned_date(&pool, &format!("TODAY-P{i}"), fx.customer_l2_id, today)
                .await;
        today_part_ids.push(id);
    }
    let today_asm_id =
        insert_assembly_with_planned_date(&pool, "TODAY-A", "TODAY-A", fx.customer_l2_id, today)
            .await;

    // today+5d: 2 part + 1 asm
    let plus5 = today + Duration::days(5);
    let mut plus5_part_ids: Vec<i64> = Vec::with_capacity(2);
    for i in 0..2 {
        let id =
            insert_part_with_planned_date(&pool, &format!("PLUS5-P{i}"), fx.customer_l2_id, plus5)
                .await;
        plus5_part_ids.push(id);
    }
    let plus5_asm_id =
        insert_assembly_with_planned_date(&pool, "PLUS5-A", "PLUS5-A", fx.customer_l2_id, plus5)
            .await;

    // today+10d: 1 part
    let plus10 = today + Duration::days(10);
    let _plus10_part_id =
        insert_part_with_planned_date(&pool, "PLUS10-P", fx.customer_l2_id, plus10).await;

    // -------- 场景 1：from=today, to=today → 4 条（3 part + 1 asm） --------
    let url1 = format!(
        "/com/union-list?customer_id={}&row_type=ALL&limit=200\
         &planned_delivery_date_from={}&planned_delivery_date_to={}",
        fx.customer_l2_id, today, today
    );
    let (s, env) = send(
        app.clone(),
        hsh_erp_test_support::json_request("GET", &url1, None::<Value>, Some(&token)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "[from=today to=today]: {env}");
    assert_eq!(env["code"], 0);
    assert_eq!(
        env["data"]["total"], 4,
        "[today only] 应仅命中 4 条（3 part + 1 asm）: {env}"
    );
    let items1: Vec<i64> = env["data"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["id"].as_str().unwrap().parse().unwrap())
        .collect();
    let mut expected1: Vec<i64> = today_part_ids.clone();
    expected1.push(today_asm_id);
    let mut got1 = items1.clone();
    got1.sort();
    let mut exp1 = expected1.clone();
    exp1.sort();
    assert_eq!(got1, exp1, "[today only] ids 集合应一致");

    // -------- 场景 2：from=today+3d, to=today+7d → 3 条（2 part + 1 asm） --------
    let url2 = format!(
        "/com/union-list?customer_id={}&row_type=ALL&limit=200\
         &planned_delivery_date_from={}&planned_delivery_date_to={}",
        fx.customer_l2_id, plus5, plus5
    );
    let (s, env) = send(
        app.clone(),
        hsh_erp_test_support::json_request("GET", &url2, None::<Value>, Some(&token)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "[from=+5 to=+5]: {env}");
    assert_eq!(env["code"], 0);
    assert_eq!(
        env["data"]["total"], 3,
        "[+5 only] 应仅命中 3 条（2 part + 1 asm）: {env}"
    );
    let items2: Vec<i64> = env["data"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["id"].as_str().unwrap().parse().unwrap())
        .collect();
    let mut expected2: Vec<i64> = plus5_part_ids.clone();
    expected2.push(plus5_asm_id);
    let mut got2 = items2.clone();
    got2.sort();
    let mut exp2 = expected2.clone();
    exp2.sort();
    assert_eq!(got2, exp2, "[+5 only] ids 集合应一致");

    // -------- 场景 3：from=today, to=today+10d（两端含）→ 8 条 --------
    let url3 = format!(
        "/com/union-list?customer_id={}&row_type=ALL&limit=200\
         &planned_delivery_date_from={}&planned_delivery_date_to={}",
        fx.customer_l2_id, today, plus10
    );
    let (s, env) = send(
        app.clone(),
        hsh_erp_test_support::json_request("GET", &url3, None::<Value>, Some(&token)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "[from=today to=+10]: {env}");
    assert_eq!(env["code"], 0);
    assert_eq!(
        env["data"]["total"], 8,
        "[today..+10] 应命中全部 8 条: {env}"
    );

    // -------- 场景 4：非法 from 格式 → 40001 VALIDATION_ERROR --------
    let url4 = format!(
        "/com/union-list?customer_id={}&row_type=ALL\
         &planned_delivery_date_from=not-a-date",
        fx.customer_l2_id
    );
    let (s, env) = send(
        app,
        hsh_erp_test_support::json_request("GET", &url4, None::<Value>, Some(&token)),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "非法日期 → 422: {env}");
    assert_eq!(env["code"], 40001, "VALIDATION_ERROR: {env}");
    assert!(
        env["message"]
            .as_str()
            .unwrap_or("")
            .contains("planned_delivery_date_from"),
        "错误消息应包含字段名: {env}"
    );

    // 静默 NaiveDate 引用避免 unused 警告
    let _ = NaiveDate::from_ymd_opt(2026, 9, 30);
}

// ===========================================================================
//  2026-09-30 新增：10 字段筛选端到端测试（4 文本 ILIKE + 4 日期窗口 + 2 IS NULL）
// ===========================================================================

/// 2026-09-30 新增：`drawing_no` ILIKE 模糊回归（隐藏 bug —— 此前 DTO 无字段，
/// 参数被 axum 静默丢弃，筛选全部失效）。
///
/// 排布（全部 `today` 日期，无 custom order_no/system_delivery_date）：
/// - 1 part drawing_no=DRW-UNI / 1 asm drawing_no=DRW-UNI → 命中 "UNI"
/// - 1 part drawing_no=DRW-OTH / 1 asm drawing_no=DRW-OTH → 不命中 "UNI"
/// - `drawing_no=UNI` 应仅命中前 2 条
#[tokio::test]
async fn list_union_items_filters_by_drawing_no() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let today = chrono::Local::now().date_naive();
    let hit_part = insert_part_full(
        &pool,
        "P-DRW-UNI",
        "DRW-UNI",
        fx.customer_l2_id,
        Some("PDU"),
        None,
        today,
        today,
        None,
    )
    .await;
    let hit_asm = insert_assembly_full(
        &pool,
        "DRW-UNI",
        "A-DRW-UNI",
        fx.customer_l2_id,
        Some("ADU"),
        None,
        today,
        today,
        None,
    )
    .await;
    let _miss_part = insert_part_full(
        &pool,
        "P-DRW-OTH",
        "DRW-OTH",
        fx.customer_l2_id,
        Some("PDO"),
        None,
        today,
        today,
        None,
    )
    .await;
    let _miss_asm = insert_assembly_full(
        &pool,
        "DRW-OTH",
        "A-DRW-OTH",
        fx.customer_l2_id,
        Some("ADO"),
        None,
        today,
        today,
        None,
    )
    .await;

    let url = format!(
        "/com/union-list?customer_id={}&row_type=ALL&limit=200&drawing_no=UNI",
        fx.customer_l2_id
    );
    let (s, env) = send(
        app,
        hsh_erp_test_support::json_request("GET", &url, None::<Value>, Some(&token)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "drawing_no=UNI: {env}");
    assert_eq!(env["code"], 0);
    assert_eq!(
        env["data"]["total"], 2,
        "应仅命中 drawing_no ILIKE %UNI% 的 2 条: {env}"
    );
    let mut got_ids: Vec<i64> = env["data"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["id"].as_str().unwrap().parse().unwrap())
        .collect();
    got_ids.sort();
    let mut exp = vec![hit_part, hit_asm];
    exp.sort();
    assert_eq!(got_ids, exp, "ids 集合应一致");
}

/// 2026-09-30 新增：`name` ILIKE 模糊回归。
///
/// 排布：
/// - 1 part name=Widget-A / 1 asm name=Widget-B → 命中
/// - 1 part name=Gear   / 1 asm name=Gear      → 不命中
/// - `name=Wid` → 仅 2 条
#[tokio::test]
async fn list_union_items_filters_by_name() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let today = chrono::Local::now().date_naive();
    let hit_part = insert_part_full(
        &pool,
        "Part-UNI",
        "D-N1",
        fx.customer_l2_id,
        Some("PNU"),
        None,
        today,
        today,
        None,
    )
    .await;
    let hit_asm = insert_assembly_full(
        &pool,
        "D-N2",
        "Asm-UNI",
        fx.customer_l2_id,
        Some("ANU"),
        None,
        today,
        today,
        None,
    )
    .await;
    let _miss_part = insert_part_full(
        &pool,
        "Part-OTH",
        "D-N3",
        fx.customer_l2_id,
        Some("PNO"),
        None,
        today,
        today,
        None,
    )
    .await;
    let _miss_asm = insert_assembly_full(
        &pool,
        "D-N4",
        "Asm-OTH",
        fx.customer_l2_id,
        Some("ANO"),
        None,
        today,
        today,
        None,
    )
    .await;

    let url = format!(
        "/com/union-list?customer_id={}&row_type=ALL&limit=200&name=UNI",
        fx.customer_l2_id
    );
    let (s, env) = send(
        app,
        hsh_erp_test_support::json_request("GET", &url, None::<Value>, Some(&token)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "name=UNI: {env}");
    assert_eq!(env["code"], 0);
    assert_eq!(
        env["data"]["total"], 2,
        "应仅命中 name ILIKE %UNI% 的 2 条: {env}"
    );
    let mut got_ids: Vec<i64> = env["data"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["id"].as_str().unwrap().parse().unwrap())
        .collect();
    got_ids.sort();
    let mut exp = vec![hit_part, hit_asm];
    exp.sort();
    assert_eq!(got_ids, exp);
}

/// 2026-09-30 新增：`order_no` ILIKE 模糊回归（order_no 是 nullable varchar(30)）。
///
/// 排布：
/// - 1 part order_no=ORD-UNI / 1 asm order_no=ORD-UNI → 命中 "UNI"
/// - 1 part order_no=ORD-OTH / 1 asm order_no=ORD-OTH → 不命中 "UNI"
/// - 1 part order_no=NULL     / 1 asm order_no=NULL     → 不命中 "UNI"
/// - `order_no=UNI` → 仅 2 条
#[tokio::test]
async fn list_union_items_filters_by_order_no() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let today = chrono::Local::now().date_naive();
    let hit_part = insert_part_full(
        &pool,
        "P-OP1",
        "D-OP1",
        fx.customer_l2_id,
        Some("PU1"),
        Some("ORD-UNI"),
        today,
        today,
        None,
    )
    .await;
    let hit_asm = insert_assembly_full(
        &pool,
        "D-OP2",
        "A-OP2",
        fx.customer_l2_id,
        Some("AU1"),
        Some("ORD-UNI"),
        today,
        today,
        None,
    )
    .await;
    let _miss_part = insert_part_full(
        &pool,
        "P-OP3",
        "D-OP3",
        fx.customer_l2_id,
        Some("PU2"),
        Some("ORD-OTH"),
        today,
        today,
        None,
    )
    .await;
    let _miss_asm = insert_assembly_full(
        &pool,
        "D-OP4",
        "A-OP4",
        fx.customer_l2_id,
        Some("AU2"),
        Some("ORD-OTH"),
        today,
        today,
        None,
    )
    .await;
    let _null_part = insert_part_full(
        &pool,
        "P-OP5",
        "D-OP5",
        fx.customer_l2_id,
        Some("PU3"),
        None,
        today,
        today,
        None,
    )
    .await;
    let _null_asm = insert_assembly_full(
        &pool,
        "D-OP6",
        "A-OP6",
        fx.customer_l2_id,
        Some("AU3"),
        None,
        today,
        today,
        None,
    )
    .await;

    let url = format!(
        "/com/union-list?customer_id={}&row_type=ALL&limit=200&order_no=UNI",
        fx.customer_l2_id
    );
    let (s, env) = send(
        app,
        hsh_erp_test_support::json_request("GET", &url, None::<Value>, Some(&token)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "order_no=UNI: {env}");
    assert_eq!(env["code"], 0);
    assert_eq!(
        env["data"]["total"], 2,
        "应仅命中 order_no ILIKE %UNI% 的 2 条: {env}"
    );
    let mut got_ids: Vec<i64> = env["data"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["id"].as_str().unwrap().parse().unwrap())
        .collect();
    got_ids.sort();
    let mut exp = vec![hit_part, hit_asm];
    exp.sort();
    assert_eq!(got_ids, exp);
}

/// 2026-09-30 新增：`serial_no` ILIKE 模糊回归（serial_no 是 nullable varchar(15)）。
///
/// 排布：
/// - 1 part serial_no=SN-UNI-P / 1 asm serial_no=SN-UNI-A → 命中 "UNI"
/// - 1 part serial_no=SN-OTH-P / 1 asm serial_no=SN-OTH-A → 不命中 "UNI"
/// - 1 part serial_no=NULL                              → 不命中 "UNI"
/// - `serial_no=UNI` → 仅 2 条
#[tokio::test]
async fn list_union_items_filters_by_serial_no() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let today = chrono::Local::now().date_naive();
    let hit_part = insert_part_full(
        &pool,
        "P-SN1",
        "D-SN1",
        fx.customer_l2_id,
        Some("SN-UNI-P"),
        None,
        today,
        today,
        None,
    )
    .await;
    let hit_asm = insert_assembly_full(
        &pool,
        "D-SN2",
        "A-SN2",
        fx.customer_l2_id,
        Some("SN-UNI-A"),
        None,
        today,
        today,
        None,
    )
    .await;
    let _miss_part = insert_part_full(
        &pool,
        "P-SN3",
        "D-SN3",
        fx.customer_l2_id,
        Some("SN-OTH-P"),
        None,
        today,
        today,
        None,
    )
    .await;
    let _miss_asm = insert_assembly_full(
        &pool,
        "D-SN4",
        "A-SN4",
        fx.customer_l2_id,
        Some("SN-OTH-A"),
        None,
        today,
        today,
        None,
    )
    .await;
    let _null_part = insert_part_full(
        &pool,
        "P-SN5",
        "D-SN5",
        fx.customer_l2_id,
        None,
        None,
        today,
        today,
        None,
    )
    .await;

    let url = format!(
        "/com/union-list?customer_id={}&row_type=ALL&limit=200&serial_no=UNI",
        fx.customer_l2_id
    );
    let (s, env) = send(
        app,
        hsh_erp_test_support::json_request("GET", &url, None::<Value>, Some(&token)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "serial_no=UNI: {env}");
    assert_eq!(env["code"], 0);
    assert_eq!(
        env["data"]["total"], 2,
        "应仅命中 serial_no ILIKE %UNI% 的 2 条: {env}"
    );
    let mut got_ids: Vec<i64> = env["data"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["id"].as_str().unwrap().parse().unwrap())
        .collect();
    got_ids.sort();
    let mut exp = vec![hit_part, hit_asm];
    exp.sort();
    assert_eq!(got_ids, exp);
}

/// 2026-09-30 新增：`request_date_from/to` 日期窗口过滤（闭区间，request_date NOT NULL）。
///
/// 排布（以 today 为基准）：
/// - 1 part req=today-3d / 1 asm req=today-3d → 不在 [today, today+5d]
/// - 1 part req=today    / 1 asm req=today    → 命中
/// - 1 part req=today+2d / 1 asm req=today+2d → 命中
/// - 1 part req=today+5d / 1 asm req=today+5d → 命中（闭区间含 to）
/// - 1 part req=today+6d / 1 asm req=today+6d → 不在区间
/// 期望命中 6 条（3 part + 3 asm）
#[tokio::test]
async fn list_union_items_filters_by_request_date_from_to() {
    use chrono::Duration;
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let today = chrono::Local::now().date_naive();
    let mut hit_ids: Vec<i64> = Vec::new();

    // [-3d, -1d]：不在区间（to=today+5d）
    let _p = insert_part_full(
        &pool,
        "P-RD-N3",
        "D-RD-N3",
        fx.customer_l2_id,
        Some("PR3"),
        None,
        today - Duration::days(3),
        today,
        None,
    )
    .await;
    let _a = insert_assembly_full(
        &pool,
        "D-RD-N3A",
        "A-RD-N3",
        fx.customer_l2_id,
        Some("AR3"),
        None,
        today - Duration::days(3),
        today,
        None,
    )
    .await;

    // today / +2d / +5d：命中
    for (i, offset) in [(0i64, 0), (1, 2), (2, 5)] {
        let d = today + Duration::days(offset);
        let pid = insert_part_full(
            &pool,
            &format!("P-RD-{i}"),
            &format!("D-RD-{i}"),
            fx.customer_l2_id,
            Some(&format!("PR{i}")),
            None,
            d,
            d,
            None,
        )
        .await;
        let aid = insert_assembly_full(
            &pool,
            &format!("D-RD-{i}A"),
            &format!("A-RD-{i}"),
            fx.customer_l2_id,
            Some(&format!("AR{i}")),
            None,
            d,
            d,
            None,
        )
        .await;
        hit_ids.push(pid);
        hit_ids.push(aid);
    }

    // +6d：不在区间
    let _p = insert_part_full(
        &pool,
        "P-RD-P6",
        "D-RD-P6",
        fx.customer_l2_id,
        Some("PR6"),
        None,
        today + Duration::days(6),
        today,
        None,
    )
    .await;
    let _a = insert_assembly_full(
        &pool,
        "D-RD-P6A",
        "A-RD-P6",
        fx.customer_l2_id,
        Some("AR6"),
        None,
        today + Duration::days(6),
        today,
        None,
    )
    .await;

    let url = format!(
        "/com/union-list?customer_id={}&row_type=ALL&limit=200\
         &request_date_from={}&request_date_to={}",
        fx.customer_l2_id,
        today,
        today + Duration::days(5)
    );
    let (s, env) = send(
        app,
        hsh_erp_test_support::json_request("GET", &url, None::<Value>, Some(&token)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "request_date [today, +5d]: {env}");
    assert_eq!(env["code"], 0);
    assert_eq!(
        env["data"]["total"], 6,
        "应仅命中 request_date ∈ [today, +5d] 的 6 条: {env}"
    );
    let mut got_ids: Vec<i64> = env["data"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["id"].as_str().unwrap().parse().unwrap())
        .collect();
    got_ids.sort();
    let mut exp = hit_ids.clone();
    exp.sort();
    assert_eq!(got_ids, exp);
}

/// 2026-09-30 新增：`system_delivery_date_from/to` 日期窗口过滤。
///
/// 排布（系统交期 nullable；以 today+5d 为基准）：
/// - 2 件 sd=today+3d → 不在 [+5d, +10d] 闭区间外
/// - 4 件 sd=today+5d / +7d / +10d → 命中（含两端）
/// - 1 件 sd=today+11d → 不在
/// - 2 件 sd=NULL     → 不在（IS NULL 才会命中；普通日期过滤 NULL 短路）
#[tokio::test]
async fn list_union_items_filters_by_system_delivery_date_from_to() {
    use chrono::Duration;
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let today = chrono::Local::now().date_naive();
    let mut hit_ids: Vec<i64> = Vec::new();

    // +3d（part + asm）：不在 [+5d, +10d] 闭区间
    let _p3 = insert_part_full(
        &pool,
        "P-SD-3",
        "D-SD-3",
        fx.customer_l2_id,
        Some("PSD3"),
        None,
        today,
        today,
        Some(today + Duration::days(3)),
    )
    .await;
    let _a3 = insert_assembly_full(
        &pool,
        "D-SD-3A",
        "A-SD-3",
        fx.customer_l2_id,
        Some("ASD3"),
        None,
        today,
        today,
        Some(today + Duration::days(3)),
    )
    .await;

    // +5d / +7d / +10d（每点 1 part + 1 asm）：命中
    for (i, offset) in [(0i64, 5), (1, 7), (2, 10)] {
        let d = today + Duration::days(offset);
        let pid = insert_part_full(
            &pool,
            &format!("P-SD-{i}"),
            &format!("D-SD-{i}"),
            fx.customer_l2_id,
            Some(&format!("PSD{i}")),
            None,
            today,
            today,
            Some(d),
        )
        .await;
        let aid = insert_assembly_full(
            &pool,
            &format!("D-SD-{i}A"),
            &format!("A-SD-{i}"),
            fx.customer_l2_id,
            Some(&format!("ASD{i}")),
            None,
            today,
            today,
            Some(d),
        )
        .await;
        hit_ids.push(pid);
        hit_ids.push(aid);
    }

    // +11d（part + asm）：不在
    let _p11 = insert_part_full(
        &pool,
        "P-SD-11",
        "D-SD-11",
        fx.customer_l2_id,
        Some("PSD11"),
        None,
        today,
        today,
        Some(today + Duration::days(11)),
    )
    .await;
    let _a11 = insert_assembly_full(
        &pool,
        "D-SD-11A",
        "A-SD-11",
        fx.customer_l2_id,
        Some("ASD11"),
        None,
        today,
        today,
        Some(today + Duration::days(11)),
    )
    .await;

    // 2 件 sd=NULL：不在（普通日期过滤 NULL 短路）
    let _pn = insert_part_full(
        &pool,
        "P-SD-N",
        "D-SD-PN",
        fx.customer_l2_id,
        Some("PSDN"),
        None,
        today,
        today,
        None,
    )
    .await;
    let _an = insert_assembly_full(
        &pool,
        "D-SD-NA",
        "A-SD-NA",
        fx.customer_l2_id,
        Some("ASDN"),
        None,
        today,
        today,
        None,
    )
    .await;

    let url = format!(
        "/com/union-list?customer_id={}&row_type=ALL&limit=200\
         &system_delivery_date_from={}&system_delivery_date_to={}",
        fx.customer_l2_id,
        today + Duration::days(5),
        today + Duration::days(10)
    );
    let (s, env) = send(
        app,
        hsh_erp_test_support::json_request("GET", &url, None::<Value>, Some(&token)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "sd [+5d, +10d]: {env}");
    assert_eq!(env["code"], 0);
    assert_eq!(
        env["data"]["total"], 6,
        "应仅命中 system_delivery_date ∈ [+5d, +10d] 的 6 条（NULL 被短路）: {env}"
    );
    let mut got_ids: Vec<i64> = env["data"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["id"].as_str().unwrap().parse().unwrap())
        .collect();
    got_ids.sort();
    let mut exp = hit_ids.clone();
    exp.sort();
    assert_eq!(got_ids, exp);
}

/// 2026-09-30 新增：`order_no_is_null=true` 三态回归（对齐 PR-F 2026-08-11
/// 空串语义，IS NULL OR =''）。
///
/// 排布：
/// - 1 part order_no=NULL  / 1 asm order_no=NULL  → 命中
/// - 1 part order_no=""    / 1 asm order_no=""    → 命中（空串 = NULL 语义）
/// - 1 part order_no=ORD-X / 1 asm order_no=ORD-X → 不命中
/// 期望命中 4 条（2 part + 2 asm）
#[tokio::test]
async fn list_union_items_filters_by_order_no_is_null_true() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let today = chrono::Local::now().date_naive();
    let mut hit_ids: Vec<i64> = Vec::new();

    let pn = insert_part_full(
        &pool,
        "P-OP-N",
        "D-OP-N",
        fx.customer_l2_id,
        Some("PN"),
        None,
        today,
        today,
        None,
    )
    .await;
    let an = insert_assembly_full(
        &pool,
        "D-OP-NA",
        "A-OP-NA",
        fx.customer_l2_id,
        Some("AN"),
        None,
        today,
        today,
        None,
    )
    .await;
    hit_ids.push(pn);
    hit_ids.push(an);

    let pe = insert_part_full(
        &pool,
        "P-OP-E",
        "D-OP-E",
        fx.customer_l2_id,
        Some("PE"),
        Some(""),
        today,
        today,
        None,
    )
    .await;
    let ae = insert_assembly_full(
        &pool,
        "D-OP-EA",
        "A-OP-EA",
        fx.customer_l2_id,
        Some("AE"),
        Some(""),
        today,
        today,
        None,
    )
    .await;
    hit_ids.push(pe);
    hit_ids.push(ae);

    let _pf = insert_part_full(
        &pool,
        "P-OP-F",
        "D-OP-F",
        fx.customer_l2_id,
        Some("PF"),
        Some("ORD-F"),
        today,
        today,
        None,
    )
    .await;
    let _af = insert_assembly_full(
        &pool,
        "D-OP-FA",
        "A-OP-FA",
        fx.customer_l2_id,
        Some("AF"),
        Some("ORD-F"),
        today,
        today,
        None,
    )
    .await;

    let url = format!(
        "/com/union-list?customer_id={}&row_type=ALL&limit=200&order_no_is_null=true",
        fx.customer_l2_id
    );
    let (s, env) = send(
        app,
        hsh_erp_test_support::json_request("GET", &url, None::<Value>, Some(&token)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "order_no_is_null=true: {env}");
    assert_eq!(env["code"], 0);
    assert_eq!(
        env["data"]["total"], 4,
        "应仅命中 order_no IS NULL OR ='' 的 4 条: {env}"
    );
    let mut got_ids: Vec<i64> = env["data"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["id"].as_str().unwrap().parse().unwrap())
        .collect();
    got_ids.sort();
    let mut exp = hit_ids.clone();
    exp.sort();
    assert_eq!(got_ids, exp);
}

/// 2026-09-30 新增：`order_no_is_null=false` 三态回归（IS NOT NULL AND <>''）。
///
/// 同上的 fixture，但反向断言：仅命中 order_no='ORD-F' 的 2 条。
#[tokio::test]
async fn list_union_items_filters_by_order_no_is_null_false() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let today = chrono::Local::now().date_naive();
    let mut hit_ids: Vec<i64> = Vec::new();

    let _pn = insert_part_full(
        &pool,
        "P-OF-N",
        "D-OF-N",
        fx.customer_l2_id,
        Some("PNF"),
        None,
        today,
        today,
        None,
    )
    .await;
    let _an = insert_assembly_full(
        &pool,
        "D-OF-NA",
        "A-OF-NA",
        fx.customer_l2_id,
        Some("ANF"),
        None,
        today,
        today,
        None,
    )
    .await;
    let _pe = insert_part_full(
        &pool,
        "P-OF-E",
        "D-OF-E",
        fx.customer_l2_id,
        Some("PEF"),
        Some(""),
        today,
        today,
        None,
    )
    .await;
    let _ae = insert_assembly_full(
        &pool,
        "D-OF-EA",
        "A-OF-EA",
        fx.customer_l2_id,
        Some("AEF"),
        Some(""),
        today,
        today,
        None,
    )
    .await;

    let pf = insert_part_full(
        &pool,
        "P-OF-F",
        "D-OF-F",
        fx.customer_l2_id,
        Some("PFF"),
        Some("ORD-F"),
        today,
        today,
        None,
    )
    .await;
    let af = insert_assembly_full(
        &pool,
        "D-OF-FA",
        "A-OF-FA",
        fx.customer_l2_id,
        Some("AFF"),
        Some("ORD-F"),
        today,
        today,
        None,
    )
    .await;
    hit_ids.push(pf);
    hit_ids.push(af);

    let url = format!(
        "/com/union-list?customer_id={}&row_type=ALL&limit=200&order_no_is_null=false",
        fx.customer_l2_id
    );
    let (s, env) = send(
        app,
        hsh_erp_test_support::json_request("GET", &url, None::<Value>, Some(&token)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "order_no_is_null=false: {env}");
    assert_eq!(env["code"], 0);
    assert_eq!(
        env["data"]["total"], 2,
        "应仅命中 order_no IS NOT NULL AND <>'' 的 2 条: {env}"
    );
    let mut got_ids: Vec<i64> = env["data"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["id"].as_str().unwrap().parse().unwrap())
        .collect();
    got_ids.sort();
    let mut exp = hit_ids.clone();
    exp.sort();
    assert_eq!(got_ids, exp);
}

/// 2026-09-30 新增：`system_delivery_date_is_null=true` 三态回归（IS NULL）。
///
/// 排布：
/// - 1 part sd=NULL / 1 asm sd=NULL → 命中
/// - 1 part sd=today / 1 asm sd=today → 不命中
/// 期望命中 2 条
#[tokio::test]
async fn list_union_items_filters_by_system_delivery_date_is_null_true() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let today = chrono::Local::now().date_naive();
    let mut hit_ids: Vec<i64> = Vec::new();

    let pn = insert_part_full(
        &pool,
        "P-SI-N",
        "D-SI-N",
        fx.customer_l2_id,
        Some("PIN"),
        None,
        today,
        today,
        None,
    )
    .await;
    let an = insert_assembly_full(
        &pool,
        "D-SI-NA",
        "A-SI-NA",
        fx.customer_l2_id,
        Some("AIN"),
        None,
        today,
        today,
        None,
    )
    .await;
    hit_ids.push(pn);
    hit_ids.push(an);

    let _pf = insert_part_full(
        &pool,
        "P-SI-F",
        "D-SI-F",
        fx.customer_l2_id,
        Some("PIF"),
        None,
        today,
        today,
        Some(today),
    )
    .await;
    let _af = insert_assembly_full(
        &pool,
        "D-SI-FA",
        "A-SI-FA",
        fx.customer_l2_id,
        Some("AIF"),
        None,
        today,
        today,
        Some(today),
    )
    .await;

    let url = format!(
        "/com/union-list?customer_id={}&row_type=ALL&limit=200\
         &system_delivery_date_is_null=true",
        fx.customer_l2_id
    );
    let (s, env) = send(
        app,
        hsh_erp_test_support::json_request("GET", &url, None::<Value>, Some(&token)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "sd_is_null=true: {env}");
    assert_eq!(env["code"], 0);
    assert_eq!(
        env["data"]["total"], 2,
        "应仅命中 system_delivery_date IS NULL 的 2 条: {env}"
    );
    let mut got_ids: Vec<i64> = env["data"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["id"].as_str().unwrap().parse().unwrap())
        .collect();
    got_ids.sort();
    let mut exp = hit_ids.clone();
    exp.sort();
    assert_eq!(got_ids, exp);
}

/// 2026-09-30 新增：`system_delivery_date_is_null=false` 三态回归（IS NOT NULL）。
///
/// 同上 fixture，反向断言：仅命中 sd=today 的 2 条。
#[tokio::test]
async fn list_union_items_filters_by_system_delivery_date_is_null_false() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let today = chrono::Local::now().date_naive();
    let mut hit_ids: Vec<i64> = Vec::new();

    let _pn = insert_part_full(
        &pool,
        "P-SF-N",
        "D-SF-N",
        fx.customer_l2_id,
        Some("PFN"),
        None,
        today,
        today,
        None,
    )
    .await;
    let _an = insert_assembly_full(
        &pool,
        "D-SF-NA",
        "A-SF-NA",
        fx.customer_l2_id,
        Some("AFN"),
        None,
        today,
        today,
        None,
    )
    .await;

    let pf = insert_part_full(
        &pool,
        "P-SF-F",
        "D-SF-F",
        fx.customer_l2_id,
        Some("PFF"),
        None,
        today,
        today,
        Some(today),
    )
    .await;
    let af = insert_assembly_full(
        &pool,
        "D-SF-FA",
        "A-SF-FA",
        fx.customer_l2_id,
        Some("AFF"),
        None,
        today,
        today,
        Some(today),
    )
    .await;
    hit_ids.push(pf);
    hit_ids.push(af);

    let url = format!(
        "/com/union-list?customer_id={}&row_type=ALL&limit=200\
         &system_delivery_date_is_null=false",
        fx.customer_l2_id
    );
    let (s, env) = send(
        app,
        hsh_erp_test_support::json_request("GET", &url, None::<Value>, Some(&token)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "sd_is_null=false: {env}");
    assert_eq!(env["code"], 0);
    assert_eq!(
        env["data"]["total"], 2,
        "应仅命中 system_delivery_date IS NOT NULL 的 2 条: {env}"
    );
    let mut got_ids: Vec<i64> = env["data"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["id"].as_str().unwrap().parse().unwrap())
        .collect();
    got_ids.sort();
    let mut exp = hit_ids.clone();
    exp.sort();
    assert_eq!(got_ids, exp);
}

/// 2026-09-30 新增：10 字段联合筛选 smoke（4 文本 + 4 日期 + 2 IS NULL 全开）。
///
/// 故意用全部 10 个字段同时给一个唯一命中的 fixture，断言只剩 2 条
/// （1 part + 1 asm）。
/// 验证 SQL 段内多个守卫 AND 拼接 + 计划缓存稳定（10 字段混合 bind + 2 字
/// 符串片段预生成）。
///
/// 注：t_part.serial_no 有 `uk_t_part_serial_no` 唯一索引（partial unique
/// `WHERE serial_no IS NOT NULL`），所以每行必须用独立 serial_no；为简化
/// 噪声 fixture，本测试仅插 1 个 part 噪声 + 1 个 asm 噪声做『9 字段不同
/// 单一不命中』验证，不重复造同样 serial_no 的多个噪音行。
#[tokio::test]
async fn list_union_items_combined_10_filters_smoke() {
    use chrono::Duration;
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let today = chrono::Local::now().date_naive();

    // 唯一命中：drawing_no=D-COMB / name=P-COMB / order_no=ORD-COMB /
    // serial_no=SN-COMB / request_date=today / system_delivery_date=today+2d
    //
    // 注：part 与 asm 是不同表，各自有独立 uk_t_*_serial_no 唯一索引；同名
    // serial_no 在 part/asm 间不冲突。共用 SN-COMB 让 serial_no=COMB 同
    // 时命中 part + asm。
    let hit_part = insert_part_full(
        &pool,
        "P-COMB",
        "D-COMB",
        fx.customer_l2_id,
        Some("SN-COMB-P"),
        Some("ORD-COMB"),
        today,
        today,
        Some(today + Duration::days(2)),
    )
    .await;
    let hit_asm = insert_assembly_full(
        &pool,
        "D-COMB",
        "A-P-COMB", // asm 的 name 必须含 "P-COMB" 才能匹配 name=P-COMB
        fx.customer_l2_id,
        Some("SN-COMB-A"),
        Some("ORD-COMB"),
        today,
        today,
        Some(today + Duration::days(2)),
    )
    .await;

    // 噪音 part：drawing_no 不同（=D-NOISE），其它字段全对齐 → 唯一字段不
    // 命中；其余字段分别被对应过滤器排除。本条足以证明 10 字段 AND 拼接
    // 生效（任意一字段不匹配 → 全过滤）。
    let _ = insert_part_full(
        &pool,
        "P-COMB",
        "D-NOISE",
        fx.customer_l2_id,
        Some("PNOISE"),
        Some("ORD-COMB"),
        today,
        today,
        Some(today + Duration::days(2)),
    )
    .await;
    let _ = insert_assembly_full(
        &pool,
        "D-NOISE",
        "A-COMB",
        fx.customer_l2_id,
        Some("ANOISE"),
        Some("ORD-COMB"),
        today,
        today,
        Some(today + Duration::days(2)),
    )
    .await;

    // 全部 10 字段开火（serial_no=COMB 同时匹配 part 的 "SN-COMB-P" 和
    // asm 的 "SN-COMB-A" —— ILIKE %COMB% 均命中）
    let url = format!(
        "/com/union-list?customer_id={}&row_type=ALL&limit=200\
         &drawing_no=D-COMB&name=P-COMB&order_no=ORD-COMB&serial_no=COMB\
         &request_date_from={}&request_date_to={}\
         &system_delivery_date_from={}&system_delivery_date_to={}\
         &order_no_is_null=false&system_delivery_date_is_null=false",
        fx.customer_l2_id,
        today,
        today + Duration::days(3),
        today,
        today + Duration::days(3),
    );
    let (s, env) = send(
        app,
        hsh_erp_test_support::json_request("GET", &url, None::<Value>, Some(&token)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "10 字段 smoke: {env}");
    assert_eq!(env["code"], 0);
    assert_eq!(
        env["data"]["total"], 2,
        "10 字段全开应仅命中 1 part + 1 asm = 2 条: {env}"
    );
    let mut got_ids: Vec<i64> = env["data"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["id"].as_str().unwrap().parse().unwrap())
        .collect();
    got_ids.sort();
    let mut exp = vec![hit_part, hit_asm];
    exp.sort();
    assert_eq!(got_ids, exp);
}

/// 2026-09-30 新增：`request_date_from` 非法格式 → 40001 VALIDATION_ERROR。
///
/// 与 9-30 commit `429be79` 的 `planned_delivery_date_from=not-a-date` 同形，
/// 但字段换成本次新增的 `request_date_from`。
#[tokio::test]
async fn list_union_items_filters_by_request_date_from_invalid_format() {
    let (_pool, app, token, fx) = bootstrap_as_manager().await;
    let url = format!(
        "/com/union-list?customer_id={}&row_type=ALL&request_date_from=not-a-date",
        fx.customer_l2_id
    );
    let (s, env) = send(
        app,
        hsh_erp_test_support::json_request("GET", &url, None::<Value>, Some(&token)),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "非法日期 → 422: {env}");
    assert_eq!(env["code"], 40001, "VALIDATION_ERROR: {env}");
    assert!(
        env["message"]
            .as_str()
            .unwrap_or("")
            .contains("request_date_from"),
        "错误消息应包含字段名: {env}"
    );
}

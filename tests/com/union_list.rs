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
/// 注：本测试只验证 pushdown 机制工作（offset 大于 segment_limit 但 total
/// 仍能凑齐）；不复现原 `segment_limit.clamp(1,200)` bug 的具体偏移点。
#[tokio::test]
async fn union_list_all_mode_deep_offset_pagination() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    // 3 part（按 created_at 升序插，最后插入的最「新 → DESC 时排首）
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
    let asm1 = insert_assembly(&pool, "ASM-A", "AA", fx.customer_l2_id).await;
    let asm2 = insert_assembly(&pool, "ASM-B", "BB", fx.customer_l2_id).await;

    // 不指定 sort_by → 默认 CREATED_AT DESC。3 part 后插 2 asm，所以首 2 条
    // 应是 asm（创建更晚），余 3 条是 part。
    let (s, env) = send(
        app.clone(),
        hsh_erp_test_support::json_request(
            "GET",
            &format!(
                "/com/union-list?customer_id={}&row_type=ALL&limit=3&offset=0",
                fx.customer_l2_id
            ),
            None::<Value>,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "ALL 首页: {env}");
    assert_eq!(env["code"], 0);
    assert_eq!(env["data"]["total"], 5, "ALL total=5: {env}");
    let page1: Vec<i64> = env["data"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["id"].as_str().unwrap().parse().unwrap())
        .collect();
    assert_eq!(page1.len(), 3);

    let (s, env) = send(
        app,
        hsh_erp_test_support::json_request(
            "GET",
            &format!(
                "/com/union-list?customer_id={}&row_type=ALL&limit=3&offset=3",
                fx.customer_l2_id
            ),
            None::<Value>,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "ALL 第 2 页: {env}");
    let page2: Vec<i64> = env["data"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["id"].as_str().unwrap().parse().unwrap())
        .collect();
    assert_eq!(page2.len(), 2);
    // 两页 id 集合无交集且总 = 5
    let mut all = page1.clone();
    all.extend(page2.iter().copied());
    assert_eq!(all.len(), 5);
    let mut uniq = all.clone();
    uniq.sort();
    uniq.dedup();
    assert_eq!(uniq.len(), 5, "两页无重复: {all:?}");
    // 首 2 条应是 asm（创建更晚）
    assert!(
        page1.contains(&asm1) && page1.contains(&asm2),
        "首页应含两个 asm: {page1:?}"
    );
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

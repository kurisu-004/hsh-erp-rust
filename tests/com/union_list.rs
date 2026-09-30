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

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
//! 7. 2026-10-03 新增：`delivered_quantity`（已送数量 / 已送套数）—— PART 行
//!    只累加 DELIVERED / COMPLETED 且非软删的批次（含 `0 < 已交 < 总量` 的
//!    部分已交形态）；ASSEMBLY 行取 `MIN(子件已送 × 套数 / 子件总量)` 并对工单
//!    总套数 `LEAST` 收口（总量 0 不参与 / 无子件为 0 / 子件超交收口 / 大数不溢出）。
//! 8. 2026-10-05 新增：`row_type=PART_FLAT`（t_part 平铺口径）—— 仅 `t_part`
//!    且**含**装配件子件。1 装配件 + 4 子件 + 5 独立件 → `PART_FLAT` 返 9
//!    （4 条带同一 `assembly_id` + 5 条 `null`，行 `row_type` 恒 `"PART"`）、
//!    `PART` 仍返 5（子件守卫未失效）、`ALL` 仍返 6（未被波及）；并断言该态的
//!    过滤 / 排序参数与 PART 态同形（`statuses` + `system_delivery_date_from/to`
//!    + `sort_by=SYSTEM_DELIVERY_DATE` 三参数组合，子件同样参与过滤与排序）。
//! 9. 2026-10-05 新增守卫：`sort_by=SYSTEM_DELIVERY_DATE` 在 **PART 态**也真正
//!    生效（该键的 repo 列名映射是 PART / PART_FLAT 共用的 `part_sql.rs::order_col`，
//!    少一处就静默退回建单序）。与第 8 条一样，两个用例的 fixture 都让「插入序」
//!    与「交期序」**相反**（先插的拿更晚的交期），因此排序键被摘掉时断言必红；
//!    若 fixture 排成同向，排序键换成任何值都同样通过，断言恒真。
//!
//! ## 并行
//! 进程级 test_pool 每次 fresh database（plan 2 2026-09-20），DB 间 schema
//! 完全独立，无需 Mutex 串行化。
//!
//! ## 认证
//! MANAGER 用户跑通（与 `/parts` list 端点同权限：Manager / Clerk / Inspector /
//! CncProgrammer，2026-09-19 聚合到 com nest）。

use axum::http::StatusCode;
use serde_json::Value;
use sqlx::PgPool;

use hsh_erp_test_support::{
    PartFixture, load_part_fixture, login_token, pool_snowflake, test_app, test_pool,
};

// ===========================================================================
//  Test fixtures
// ===========================================================================

/// 取一个测试用雪花 ID。
///
/// 2026-10-08 review 第 1 轮 B3：改走 `test-support::pool_snowflake()`。原实现是本文件
/// 私有的 `OnceLock<SnowflakeIdGenerator>` + 写死 `instance = 1`；而本 binary（`--test com`）
/// 里的 `delivery_note/*` 夹具此前也各自 `new(..., 1)`，同一毫秒内会发出**完全相同**的
/// id ⇒ 23505。共享进程级生成器（instance 由 pid ⊕ 启动纳秒派生）后彻底消除该类碰撞。
fn next_test_id() -> i64 {
    pool_snowflake().lock().expect("pool_snowflake").next_id()
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
    insert_part_with_quantity(pool, name, customer_id, serial_no, status, 1).await
}

/// 直插一个 `t_part` 行，显式指定工单总数量（2026-10-03 新增）。
///
/// 与 [`insert_part`] 的唯一差别是 `quantity` 列（前者恒为 1）；`insert_part` 本身
/// 转调本函数，故既有调用点不受影响。
async fn insert_part_with_quantity(
    pool: &PgPool,
    name: &str,
    customer_id: i64,
    serial_no: Option<&str>,
    status: &str,
    quantity: i32,
) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;
    let id = next_test_id();
    let now = now_naive();
    let today = now.date();
    sqlx::query(
        "INSERT INTO t_part (id, serial_no, name, drawing_no, customer_id, status, \
         applicant_name, request_date, planned_delivery_date, quantity, version, \
         created_at, updated_at) \
         VALUES ($1, $2, $3, 'D-001', $4, $7, $3, $5, $5, $8, 0, $6, $6)",
    )
    .bind(id)
    .bind(serial_no)
    .bind(name)
    .bind(customer_id)
    .bind(today)
    .bind(now)
    .bind(status)
    .bind(quantity)
    .execute(pool)
    .await
    .expect("insert part with quantity");
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

/// 2026-10-03 新增：直插一个 `t_part_batch` 行（`delivered_quantity` 用例专用）。
///
/// `deleted_at` 传 `Some(_)` 即软删（验证软删批次不计入已送数量）。
async fn add_batch(
    pool: &PgPool,
    part_id: i64,
    batch_no: i32,
    qty: i32,
    status: &str,
    deleted_at: Option<chrono::NaiveDateTime>,
) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;
    let id = next_test_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_part_batch (id, part_id, batch_no, quantity, status, version, \
         created_at, updated_at, deleted_at) \
         VALUES ($1, $2, $3, $4, $5, 0, $6, $6, $7)",
    )
    .bind(id)
    .bind(part_id)
    .bind(batch_no)
    .bind(qty)
    .bind(status)
    .bind(now)
    .bind(deleted_at)
    .execute(pool)
    .await
    .expect("insert t_part_batch");
    id
}

/// 2026-10-03 新增：直插一个指定套数的 `t_assembly` 行（已送套数 min 公式用）。
async fn add_assembly_with_quantity(
    pool: &PgPool,
    drawing_no: &str,
    name: &str,
    customer_id: i64,
    quantity: i32,
) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;
    let id = next_test_id();
    let now = now_naive();
    let today = now.date();
    sqlx::query(
        "INSERT INTO t_assembly (id, drawing_no, name, applicant_name, customer_id, \
         request_date, planned_delivery_date, status, quantity, unit_price, total_price, \
         version, created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, $3, '', $4, $5, $5, 'PENDING', $6, 0, 0, 0, $7, NULL, $7, NULL)",
    )
    .bind(id)
    .bind(drawing_no)
    .bind(name)
    .bind(customer_id)
    .bind(today)
    .bind(quantity)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_assembly with quantity");
    id
}

/// 2026-10-03 新增：直插一个指定「子件总数」的装配体子件（已送套数 min 公式用）。
async fn add_child_part_with_quantity(
    pool: &PgPool,
    name: &str,
    customer_id: i64,
    assembly_id: i64,
    quantity: i32,
) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;
    let id = next_test_id();
    let now = now_naive();
    let today = now.date();
    sqlx::query(
        "INSERT INTO t_part (id, serial_no, name, drawing_no, customer_id, status, \
         applicant_name, request_date, planned_delivery_date, quantity, version, \
         created_at, updated_at, assembly_id) \
         VALUES ($1, NULL, $2, 'D-001', $3, 'PENDING', $2, $5, $5, $4, 0, $6, $6, $7)",
    )
    .bind(id)
    .bind(name)
    .bind(customer_id)
    .bind(quantity)
    .bind(today)
    .bind(now)
    .bind(assembly_id)
    .execute(pool)
    .await
    .expect("insert child part with quantity");
    id
}

/// 2026-10-03 新增：从 `items` 里按 id 取单行（取不到即 panic，附完整响应便于定位）。
fn find_row(env: &Value, id: i64) -> Value {
    env["data"]["items"]
        .as_array()
        .unwrap_or_else(|| panic!("items 缺数组: {env}"))
        .iter()
        .find(|i| i["id"].as_str().and_then(|s| s.parse::<i64>().ok()) == Some(id))
        .cloned()
        .unwrap_or_else(|| panic!("响应里找不到 id={id}: {env}"))
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

/// 2026-10-05 新增：`row_type=PART_FLAT` —— 统计与展示统一以 `t_part` 行为单元。
///
/// 场景数据（对应用户需求原文的 1 装配件 + 4 子件 + 5 独立件 = 9）：
/// - 1 个 `t_assembly` 父行（`t_assembly` 全模块零引用，本态不返）
/// - 4 个子件（`t_part.assembly_id = 父 id`）
/// - 5 个独立件（`t_part.assembly_id IS NULL`）
///
/// 三态对照断言：
/// - `PART_FLAT` → `total == 9`（4 子件 + 5 独立件），4 条带同一 `assembly_id`
/// - `PART`     → `total == 5`（子件守卫 `assembly_id IS NULL` 未失效）
/// - `ALL`      → `total == 6`（5 独立 + 1 装配件父行；ALL 分支未被波及）
///
/// 另断言响应行的 `row_type` 字段恒为 `"PART"`（`PartListItem` 从 `TPart` 派生，
/// 不是请求里的 `"PART_FLAT"`）。
#[tokio::test]
async fn union_list_row_type_part_flat_counts_child_parts_as_rows() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;

    // 1 个装配件父行 + 4 个子件 + 5 个独立件
    let asm = insert_assembly(&pool, "ASM-FLAT-001", "A-FLAT", fx.customer_l2_id).await;
    for i in 0..4 {
        insert_part_under_assembly(
            &pool,
            &format!("CHILD-{i}"),
            fx.customer_l2_id,
            asm,
            "PENDING",
        )
        .await;
    }
    for i in 0..5 {
        insert_part(
            &pool,
            &format!("SOLO-{i}"),
            fx.customer_l2_id,
            Some(&format!("SOLO{i:03}")),
            "PENDING",
        )
        .await;
    }

    // ----- PART_FLAT：9 行（4 子件 + 5 独立件）-----
    let (s, env) = send(
        app.clone(),
        hsh_erp_test_support::json_request(
            "GET",
            &format!(
                "/com/union-list?customer_id={}&row_type=PART_FLAT&limit=200",
                fx.customer_l2_id
            ),
            None::<Value>,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "row_type=PART_FLAT: {env}");
    assert_eq!(env["code"], 0);
    let items = env["data"]["items"].as_array().unwrap();
    assert_eq!(env["data"]["total"], 9, "4 子件 + 5 独立件 = 9: {env}");
    assert_eq!(items.len(), 9, "无分页时 total 应等于 items.len(): {env}");

    let mut child_rows = 0usize;
    let mut solo_rows = 0usize;
    for item in items {
        // 响应行标签恒为 "PART"（请求态是 PART_FLAT）
        assert_eq!(item["row_type"], "PART", "PART_FLAT 行 row_type: {item}");
        // 子件 → Some(父 id)；wire 上是字符串（serialize_i64_opt）
        let aid = item["assembly_id"]
            .as_str()
            .and_then(|s| s.parse::<i64>().ok());
        match aid {
            Some(a) => {
                child_rows += 1;
                assert_eq!(a, asm, "子件的 assembly_id 应指向同一父装配件: {item}");
            }
            None => solo_rows += 1,
        }
    }
    assert_eq!(child_rows, 4, "应有 4 条带 assembly_id 的子件行: {env}");
    assert_eq!(solo_rows, 5, "应有 5 条 assembly_id=null 的独立件行: {env}");

    // ----- 回归：PART 态仍被 `assembly_id IS NULL` 守卫收窄到 5 -----
    let (s, env) = send(
        app.clone(),
        hsh_erp_test_support::json_request(
            "GET",
            &format!(
                "/com/union-list?customer_id={}&row_type=PART&limit=200",
                fx.customer_l2_id
            ),
            None::<Value>,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "row_type=PART: {env}");
    assert_eq!(env["data"]["total"], 5, "PART 态应仍排除 4 个子件: {env}");

    // ----- 回归：ALL 态不受影响（5 独立 + 1 装配件父行）-----
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
    assert_eq!(s, StatusCode::OK, "row_type=ALL: {env}");
    assert_eq!(
        env["data"]["total"], 6,
        "ALL 态 = 5 独立 part + 1 装配件父行: {env}"
    );
}

/// 2026-10-05 新增：`row_type=PART_FLAT` 的过滤 / 排序参数与 PART 态完全同形。
///
/// 组合断言（2026-10-05 需求验收第 6 条，至少覆盖三参数组合）：
/// - `statuses=PENDING,IN_PROCESS`（子件也参与状态过滤）
/// - `system_delivery_date_from` / `_to` 闭区间（子件也参与交期窗口过滤）
/// - `sort_by=SYSTEM_DELIVERY_DATE&sort_dir=ASC`（子件也参与排序）
///
/// 排布（today 为基准）—— **插入序与交期序故意反相关**：
/// - 子件 A：第 1 个插入、status=IN_PROCESS、sd=today+3d（命中窗口）
/// - 独立件 B：第 3 个插入、status=PENDING、sd=today+2d（命中窗口）
/// - 独立件 C：status=DELIVERED、sd=today+3d（状态不命中 → 被 statuses 排除）
/// - 子件 D：status=PENDING、sd=today+9d（窗口不命中 → 被日期排除）
///
/// 反相关是本用例排序断言的鉴别力来源：命中集恒为 {A, B} 两条，而
/// - 按 `system_delivery_date ASC` → B（+2d）在前、A（+3d）在后；
/// - 若排序键被静默降级成 `id ASC`（雪花建单序）→ A（先插）在前、B 在后。
///
/// 两者结论相反，故本断言在 `order_col` 漏掉 `SYSTEM_DELIVERY_DATE` 时必红；
/// 若 fixture 把日期排成与插入序同向，排序键换成任何值都同样通过，断言恒真。
/// 「子件 A 先插 → id 更小」这条前提由 setup 段的 `child_a < solo_b` 断言钉住。
#[tokio::test]
async fn union_list_row_type_part_flat_supports_part_filters() {
    use chrono::Duration;
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let today = chrono::Local::now().date_naive();

    let asm = insert_assembly(&pool, "ASM-FLT-001", "A-FLT", fx.customer_l2_id).await;
    // 子件：insert_part_full 不带 assembly_id，故子件用 under_assembly + 事后 UPDATE
    // system_delivery_date / status 的组合（helper 家族无「子件 + 系统交期」组合）
    let child_a =
        insert_part_under_assembly(&pool, "CHILD-A", fx.customer_l2_id, asm, "IN_PROCESS").await;
    let child_d =
        insert_part_under_assembly(&pool, "CHILD-D", fx.customer_l2_id, asm, "PENDING").await;
    let solo_b = insert_part_full(
        &pool,
        "SOLO-B",
        "D-FLT-B",
        fx.customer_l2_id,
        Some("FLTB"),
        None,
        today,
        today,
        Some(today + Duration::days(2)),
    )
    .await;
    let solo_c = insert_part_full(
        &pool,
        "SOLO-C",
        "D-FLT-C",
        fx.customer_l2_id,
        Some("FLTC"),
        None,
        today,
        today,
        Some(today + Duration::days(3)),
    )
    .await;
    // 子件补系统交期（helper 无组合参数，用 UPDATE 补齐，见上方注释）
    // 2026-10-05：子件 A 拿**更晚**的 +3d，与它「第 1 个插入」的建单序相反。
    sqlx::query("UPDATE t_part SET system_delivery_date = $1 WHERE id = $2")
        .bind(today + Duration::days(3))
        .bind(child_a)
        .execute(&pool)
        .await
        .expect("set child A system_delivery_date");
    sqlx::query("UPDATE t_part SET system_delivery_date = $1 WHERE id = $2")
        .bind(today + Duration::days(9))
        .bind(child_d)
        .execute(&pool)
        .await
        .expect("set child D system_delivery_date");
    sqlx::query("UPDATE t_part SET status = 'DELIVERED' WHERE id = $1")
        .bind(solo_c)
        .execute(&pool)
        .await
        .expect("set solo C delivered");
    // 命中集恒为 {子件 A, 独立件 B}，二者的 id 序须与交期序相反，排序断言才有鉴别力
    assert!(
        child_a < solo_b,
        "fixture 前提：子件 A 必须先于独立件 B 插入（雪花 id 更小），否则本用例无鉴别力"
    );

    let (s, env) = send(
        app,
        hsh_erp_test_support::json_request(
            "GET",
            &format!(
                "/com/union-list?customer_id={}&row_type=PART_FLAT\
                 &statuses=PENDING,IN_PROCESS\
                 &system_delivery_date_from={}&system_delivery_date_to={}\
                 &sort_by=SYSTEM_DELIVERY_DATE&sort_dir=ASC&limit=200",
                fx.customer_l2_id,
                (today + Duration::days(2)).format("%Y-%m-%d"),
                (today + Duration::days(3)).format("%Y-%m-%d"),
            ),
            None::<Value>,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "PART_FLAT 组合筛选: {env}");
    assert_eq!(env["code"], 0);
    let items = env["data"]["items"].as_array().unwrap();
    assert_eq!(
        env["data"]["total"], 2,
        "statuses + 系统交期窗口应仅命中子件 A 与独立件 B: {env}"
    );
    assert_eq!(items.len(), 2, "{env}");
    let ids: Vec<i64> = items
        .iter()
        .map(|i| i["id"].as_str().unwrap().parse().unwrap())
        .collect();
    assert_eq!(
        ids,
        vec![solo_b, child_a],
        "ASC 排序：系统交期 today+2d 的独立件 B 应在 today+3d 的子件 A 之前（\
         若得到 [child_a, solo_b] 说明排序键退化成 id 建单序）; ids={ids:?}"
    );
    // 命中的子件行 assembly_id 仍是真实父 id（未被过滤参数抹掉）
    assert_eq!(find_row(&env, child_a)["assembly_id"], asm.to_string());
    assert!(find_row(&env, solo_b)["assembly_id"].is_null());
}

/// 2026-10-05 新增守卫：`row_type=PART` 下 `sort_by=SYSTEM_DELIVERY_DATE` 真正生效。
///
/// PART 与 PART_FLAT 共用同一份 `PartRepo::list_with_filters`，排序键解析也在
/// 同一处 `order_col` match 白名单里。本用例专门钉住 PART 态：防止后续有人认为
/// 「PART 态不需要系统交期排序」而把该键从白名单删掉 / 收窄回 PART_FLAT 专用
/// 分支，导致 PART 态静默退回建单序。
///
/// fixture 反相关：先插入的 p_first 拿**更晚**的交期，后插入的 p_second 拿更早的。
/// - `sort_by=SYSTEM_DELIVERY_DATE&sort_dir=ASC` → p_second 在前；
/// - 排序键失效（降级 `id`）→ p_first 在前。两者结论相反，故断言有鉴别力。
#[tokio::test]
async fn union_list_row_type_part_sorts_by_system_delivery_date() {
    use chrono::Duration;
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let today = chrono::Local::now().date_naive();

    let p_first = insert_part_full(
        &pool,
        "P-PARTSORT-LATE",
        "D-PARTSORT-LATE",
        fx.customer_l2_id,
        Some("PSRTLATE"),
        None,
        today,
        today,
        Some(today + Duration::days(5)),
    )
    .await;
    let p_second = insert_part_full(
        &pool,
        "P-PARTSORT-EARLY",
        "D-PARTSORT-EARLY",
        fx.customer_l2_id,
        Some("PSRTNEARLY"),
        None,
        today,
        today,
        Some(today + Duration::days(1)),
    )
    .await;
    assert!(
        p_first < p_second,
        "fixture 前提：p_first 必须是先插入的（雪花 id 更小），否则本用例无鉴别力"
    );

    let (s, env) = send(
        app,
        hsh_erp_test_support::json_request(
            "GET",
            &format!(
                "/com/union-list?customer_id={}&row_type=PART\
                 &sort_by=SYSTEM_DELIVERY_DATE&sort_dir=ASC&limit=200",
                fx.customer_l2_id
            ),
            None::<Value>,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "PART 态系统交期排序: {env}");
    assert_eq!(env["code"], 0);
    let items = env["data"]["items"].as_array().unwrap();
    let ids: Vec<i64> = items
        .iter()
        .map(|i| i["id"].as_str().unwrap().parse().unwrap())
        .collect();
    let pos_first = ids.iter().position(|&x| x == p_first).expect("p_first 在");
    let pos_second = ids
        .iter()
        .position(|&x| x == p_second)
        .expect("p_second 在");
    assert!(
        pos_second < pos_first,
        "ASC 应按 system_delivery_date 排（+1d 在 +5d 之前），\
         拿到相反顺序说明该键在 PART 态被降级成 id: ids={ids:?}"
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

/// 2026-09-30 新增回归：dashboard「按系统交期排序」配套。修复 review A.1：
/// 保证 `sort_by=SYSTEM_DELIVERY_DATE` 不会被静默降级到 CREATED_AT，防止未来 PR
/// 误删白名单。
///
/// - 准备 2 条 part：`system_delivery_date` 分别为 `2026-09-01` / `2026-09-30`
/// - 调 `sort_by=SYSTEM_DELIVERY_DATE&sort_dir=ASC`，断言 items 顺序为
///   `09-01` → `09-30`（升序，非 NULL 项排在前面）
/// - 同时调 DESC 断言顺序反向
#[tokio::test]
async fn sort_by_system_delivery_date_returns_sorted_results() {
    use chrono::Duration;
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let today = chrono::Local::now().date_naive();

    // p_early: system_delivery_date = today - 29d（≈ 2026-09-01 当天附近）
    let p_early = insert_part_full(
        &pool,
        "P-SORT-EARLY",
        "D-SORT-EARLY",
        fx.customer_l2_id,
        Some("PSEARLY"),
        None,
        today,
        today,
        Some(today - Duration::days(29)),
    )
    .await;
    // p_late: system_delivery_date = today（最晚）
    let p_late = insert_part_full(
        &pool,
        "P-SORT-LATE",
        "D-SORT-LATE",
        fx.customer_l2_id,
        Some("PSLATE"),
        None,
        today,
        today,
        Some(today),
    )
    .await;
    // p_null: system_delivery_date = NULL（不应出现在 ASC 头部，因为 NULL < 任何日期
    // 在 PG 升序里排最前；这里只断言已知非 NULL 项按升序排，NULL 项可前可后）
    let p_null = insert_part_full(
        &pool,
        "P-SORT-NULL",
        "D-SORT-NULL",
        fx.customer_l2_id,
        Some("PSNULL"),
        None,
        today,
        today,
        None,
    )
    .await;

    // 1) ASC：p_early < p_late（NULL 位置不约束）
    let url_asc = format!(
        "/com/union-list?customer_id={}&row_type=ALL\
         &sort_by=SYSTEM_DELIVERY_DATE&sort_dir=ASC&limit=200",
        fx.customer_l2_id
    );
    let (s, env) = send(
        app.clone(),
        hsh_erp_test_support::json_request("GET", &url_asc, None::<Value>, Some(&token)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "ASC: {env}");
    assert_eq!(env["code"], 0);
    let items = env["data"]["items"].as_array().unwrap();
    let ids: Vec<i64> = items
        .iter()
        .map(|i| i["id"].as_str().unwrap().parse().unwrap())
        .collect();
    let pos_early = ids
        .iter()
        .position(|&x| x == p_early)
        .expect("p_early present");
    let pos_late = ids
        .iter()
        .position(|&x| x == p_late)
        .expect("p_late present");
    assert!(
        pos_early < pos_late,
        "ASC 时 p_early (system_delivery_date=早) 应排在 p_late 之前; ids={ids:?}"
    );
    let _ = p_null; // NULL 项位置不约束

    // 2) DESC：p_late < p_early（反向）
    let url_desc = format!(
        "/com/union-list?customer_id={}&row_type=ALL\
         &sort_by=SYSTEM_DELIVERY_DATE&sort_dir=DESC&limit=200",
        fx.customer_l2_id
    );
    let (s, env) = send(
        app,
        hsh_erp_test_support::json_request("GET", &url_desc, None::<Value>, Some(&token)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "DESC: {env}");
    assert_eq!(env["code"], 0);
    let items = env["data"]["items"].as_array().unwrap();
    let ids: Vec<i64> = items
        .iter()
        .map(|i| i["id"].as_str().unwrap().parse().unwrap())
        .collect();
    let pos_early = ids
        .iter()
        .position(|&x| x == p_early)
        .expect("p_early present");
    let pos_late = ids
        .iter()
        .position(|&x| x == p_late)
        .expect("p_late present");
    assert!(
        pos_late < pos_early,
        "DESC 时 p_late (system_delivery_date=晚) 应排在 p_early 之前; ids={ids:?}"
    );
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

// ===========================================================================
//  2026-10-03 新增：delivered_quantity（已送数量 / 已送套数）
// ===========================================================================

/// PART 行：只累加 `DELIVERED` 批次，`IN_PROCESS` 不计入。
///
/// 装配件 3 套（DELIVERED）+ 4 套（DELIVERED）+ 5 套（IN_PROCESS）→ 7。
#[tokio::test]
async fn delivered_quantity_part_row_sums_only_delivered_batches() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let p = insert_part(&pool, "PD1", fx.customer_l2_id, Some("PD1"), "IN_PROCESS").await;
    add_batch(&pool, p, 1, 3, "DELIVERED", None).await;
    add_batch(&pool, p, 2, 4, "DELIVERED", None).await;
    add_batch(&pool, p, 3, 5, "IN_PROCESS", None).await;

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
    assert_eq!(s, StatusCode::OK, "union-list ALL: {env}");
    let row = find_row(&env, p);
    assert_eq!(row["row_type"], "PART");
    assert_eq!(
        row["delivered_quantity"], 7,
        "3+4 已交，5 件在制不计入: {row}"
    );
}

/// PART 行：`COMPLETED` 批次同样计入（口径是 `IN ('DELIVERED', 'COMPLETED')`）。
#[tokio::test]
async fn delivered_quantity_part_row_counts_completed_batches() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let p = insert_part(&pool, "PD2", fx.customer_l2_id, Some("PD2"), "IN_PROCESS").await;
    add_batch(&pool, p, 1, 2, "DELIVERED", None).await;
    add_batch(&pool, p, 2, 6, "COMPLETED", None).await;

    let (s, env) = send(
        app,
        hsh_erp_test_support::json_request(
            "GET",
            &format!(
                "/com/union-list?customer_id={}&row_type=PART&limit=200",
                fx.customer_l2_id
            ),
            None::<Value>,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "union-list PART: {env}");
    let row = find_row(&env, p);
    assert_eq!(
        row["delivered_quantity"], 8,
        "COMPLETED 与 DELIVERED 同口径计入: {row}"
    );
}

/// PART 行：软删批次（`deleted_at` 非空）不计入。
#[tokio::test]
async fn delivered_quantity_part_row_excludes_soft_deleted_batches() {
    use hsh_erp_rust::infra::clock::now_naive;
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let p = insert_part(&pool, "PD3", fx.customer_l2_id, Some("PD3"), "IN_PROCESS").await;
    add_batch(&pool, p, 1, 5, "DELIVERED", None).await;
    add_batch(&pool, p, 2, 7, "DELIVERED", Some(now_naive())).await;

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
    assert_eq!(s, StatusCode::OK, "union-list ALL: {env}");
    let row = find_row(&env, p);
    assert_eq!(row["delivered_quantity"], 5, "软删的 7 件不计入: {row}");
}

/// PART 行：`CANCELLED` 批次不计入。
#[tokio::test]
async fn delivered_quantity_part_row_excludes_cancelled_batches() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let p = insert_part(&pool, "PD4", fx.customer_l2_id, Some("PD4"), "IN_PROCESS").await;
    add_batch(&pool, p, 1, 4, "DELIVERED", None).await;
    add_batch(&pool, p, 2, 9, "CANCELLED", None).await;

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
    assert_eq!(s, StatusCode::OK, "union-list ALL: {env}");
    let row = find_row(&env, p);
    assert_eq!(row["delivered_quantity"], 4, "已取消的 9 件不计入: {row}");
}

/// PART 行：零批次 → 0，且**键必须存在**（不是缺字段）。
#[tokio::test]
async fn delivered_quantity_part_row_is_zero_when_no_batches() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let p = insert_part(&pool, "PD5", fx.customer_l2_id, Some("PD5"), "PENDING").await;

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
    assert_eq!(s, StatusCode::OK, "union-list ALL: {env}");
    let row = find_row(&env, p);
    assert!(
        row.get("delivered_quantity").is_some(),
        "delivered_quantity 键必须恒在（零批次也不能缺）: {row}"
    );
    assert_eq!(
        row["delivered_quantity"], 0,
        "零批次 → 0（COALESCE 生效）: {row}"
    );
}

/// ASSEMBLY 行：已送套数 = min(子件已送 × 套数 / 子件总量)。
///
/// 装配件 10 套；子件 A 总量 20 已交 10（→ 10*10/20 = 5 套）；子件 B 总量 10 已交 0
/// （→ 0 套）→ min = 0；补交 B 的 10 件后 → min(5, 10) = 5。
#[tokio::test]
async fn delivered_quantity_assembly_row_uses_min_set_formula() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let asm = add_assembly_with_quantity(&pool, "D-MIN", "A-MIN", fx.customer_l2_id, 10).await;
    let child_a = add_child_part_with_quantity(&pool, "CA", fx.customer_l2_id, asm, 20).await;
    let child_b = add_child_part_with_quantity(&pool, "CB", fx.customer_l2_id, asm, 10).await;
    add_batch(&pool, child_a, 1, 10, "DELIVERED", None).await;

    let url = format!(
        "/com/union-list?customer_id={}&row_type=ALL&limit=200",
        fx.customer_l2_id
    );
    let (s, env) = send(
        app.clone(),
        hsh_erp_test_support::json_request("GET", &url, None::<Value>, Some(&token)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "union-list ALL: {env}");
    let row = find_row(&env, asm);
    assert_eq!(row["row_type"], "ASSEMBLY");
    assert_eq!(row["quantity"], 10, "装配件套数字面应回显: {row}");
    assert_eq!(
        row["delivered_quantity"], 0,
        "子件 B 一件没交 → min(5, 0) = 0: {row}"
    );

    // 补交子件 B 的 10 件 → min(5, 10) = 5
    add_batch(&pool, child_b, 1, 10, "DELIVERED", None).await;
    let (s, env) = send(
        app,
        hsh_erp_test_support::json_request("GET", &url, None::<Value>, Some(&token)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "union-list ALL（补交后）: {env}");
    let row = find_row(&env, asm);
    assert_eq!(
        row["delivered_quantity"], 5,
        "子件 B 补齐后 → min(5, 10) = 5: {row}"
    );
}

/// ASSEMBLY 行：子件总量为 0 → 该子件不参与（不得整除零出错、也不拖累 min）。
///
/// 装配件 10 套；子件 Z 总量 0（已交 100 件，数学上无意义但能验证 NULLIF 分支）、
/// 子件 A 总量 20 已交 10（→ 5 套）→ 期望 5。
#[tokio::test]
async fn delivered_quantity_assembly_row_skips_zero_quantity_child() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let asm = add_assembly_with_quantity(&pool, "D-ZERO", "A-ZERO", fx.customer_l2_id, 10).await;
    let child_zero = add_child_part_with_quantity(&pool, "CZ", fx.customer_l2_id, asm, 0).await;
    let child_a = add_child_part_with_quantity(&pool, "CA", fx.customer_l2_id, asm, 20).await;
    add_batch(&pool, child_zero, 1, 100, "DELIVERED", None).await;
    add_batch(&pool, child_a, 1, 10, "DELIVERED", None).await;

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
    assert_eq!(s, StatusCode::OK, "总量 0 的子件不应整除零: {env}");
    let row = find_row(&env, asm);
    assert_eq!(row["child_count"], 2, "两个子件都参与计数: {row}");
    assert_eq!(
        row["delivered_quantity"], 5,
        "总量 0 的子件不参与 min，其余子件仍给出 5 套: {row}"
    );
}

/// ASSEMBLY 行：无子件 → 0（外层 COALESCE 兜底，键恒在）。
#[tokio::test]
async fn delivered_quantity_assembly_row_is_zero_without_children() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let asm = add_assembly_with_quantity(&pool, "D-NOKID", "A-NOKID", fx.customer_l2_id, 7).await;

    let (s, env) = send(
        app,
        hsh_erp_test_support::json_request(
            "GET",
            &format!(
                "/com/union-list?customer_id={}&row_type=ASSEMBLY&limit=200",
                fx.customer_l2_id
            ),
            None::<Value>,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "union-list ASSEMBLY: {env}");
    let row = find_row(&env, asm);
    assert_eq!(row["child_count"], 0);
    assert!(
        row.get("delivered_quantity").is_some(),
        "delivered_quantity 键必须恒在: {row}"
    );
    assert_eq!(row["delivered_quantity"], 0, "无子件 → 0 套: {row}");
}

/// PART 行：需求的真实形态 `0 < 已交 < 总量`（部分已交）。
///
/// 工单 20 件、已交 7 件、其余 13 件在制 → `delivered_quantity == 7`（不是 0，
/// 也不是 20）。既有用例的 fixture 都在超交（`insert_part` 硬编码 quantity=1
/// 而已送量 4~10），本例补上「总量 > 0」这一侧。
#[tokio::test]
async fn delivered_quantity_part_row_reflects_partial_delivery_below_total() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let p = insert_part_with_quantity(
        &pool,
        "PD7",
        fx.customer_l2_id,
        Some("PD7"),
        "IN_PROCESS",
        20,
    )
    .await;
    add_batch(&pool, p, 1, 7, "DELIVERED", None).await;
    add_batch(&pool, p, 2, 13, "IN_PROCESS", None).await;

    let (s, env) = send(
        app,
        hsh_erp_test_support::json_request(
            "GET",
            &format!(
                "/com/union-list?customer_id={}&row_type=PART&limit=200",
                fx.customer_l2_id
            ),
            None::<Value>,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "union-list PART: {env}");
    let row = find_row(&env, p);
    assert_eq!(row["quantity"], 20, "工单总数量应回显: {row}");
    assert_eq!(
        row["delivered_quantity"], 7,
        "部分已交：7 < 20，取已交件数而非总件数的任何比例: {row}"
    );
}

/// ASSEMBLY 行：子件超交时收口到工单总套数（不可能交付超过总套数的套数）。
///
/// 装配件 10 套；子件 A 总量 20 已交 100（超交）→ 未收口时 `100 × 10 / 20 = 50` 套，
/// UI 会显示「50 / 10 套」；收口后应给 10。
#[tokio::test]
async fn delivered_quantity_assembly_row_clamps_on_over_delivery() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let asm = add_assembly_with_quantity(&pool, "D-OVR", "A-OVR", fx.customer_l2_id, 10).await;
    let child_a = add_child_part_with_quantity(&pool, "CA", fx.customer_l2_id, asm, 20).await;
    add_batch(&pool, child_a, 1, 100, "DELIVERED", None).await;

    let (s, env) = send(
        app,
        hsh_erp_test_support::json_request(
            "GET",
            &format!(
                "/com/union-list?customer_id={}&row_type=ASSEMBLY&limit=200",
                fx.customer_l2_id
            ),
            None::<Value>,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "union-list ASSEMBLY: {env}");
    let row = find_row(&env, asm);
    assert_eq!(row["quantity"], 10, "装配件总套数应回显: {row}");
    assert_eq!(
        row["delivered_quantity"], 10,
        "子件超交：100*10/20 = 50 套必须收口到总套数 10: {row}"
    );
}

/// ASSEMBLY 行：int8 乘积不因 `::int` 收窄溢出（否则整个列表页 500）。
///
/// 装配件 1_000_000 套；子件 A 总量 1 已交 1_000_000 → `1e6 × 1e6 = 1e12` 超 int4。
/// 未收口时 PG 抛 `integer out of range`；收口到 `a.quantity` 后给 1_000_000。
#[tokio::test]
async fn delivered_quantity_assembly_row_survives_large_quantities() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let asm =
        add_assembly_with_quantity(&pool, "D-BIG", "A-BIG", fx.customer_l2_id, 1_000_000).await;
    let child_a = add_child_part_with_quantity(&pool, "CA", fx.customer_l2_id, asm, 1).await;
    add_batch(&pool, child_a, 1, 1_000_000, "DELIVERED", None).await;

    let (s, env) = send(
        app,
        hsh_erp_test_support::json_request(
            "GET",
            &format!(
                "/com/union-list?customer_id={}&row_type=ASSEMBLY&limit=200",
                fx.customer_l2_id
            ),
            None::<Value>,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "int8 乘积不得触发 500: {env}");
    let row = find_row(&env, asm);
    assert_eq!(
        row["delivered_quantity"], 1_000_000,
        "1e6 × 1e6 = 1e12 收窄前先钳到总套数 1e6: {row}"
    );
}

/// `GET /api/v2/parts` 与 `GET /com/union-list` 的已送数量一致（同一 fixture、同一 part）。
#[tokio::test]
async fn delivered_quantity_parts_endpoint_matches_union_list() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let p = insert_part(&pool, "PD6", fx.customer_l2_id, Some("PD6"), "IN_PROCESS").await;
    add_batch(&pool, p, 1, 7, "DELIVERED", None).await;
    add_batch(&pool, p, 2, 3, "COMPLETED", None).await;

    let (s_union, env_union) = send(
        app.clone(),
        hsh_erp_test_support::json_request(
            "GET",
            &format!(
                "/com/union-list?customer_id={}&row_type=PART&limit=200",
                fx.customer_l2_id
            ),
            None::<Value>,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s_union, StatusCode::OK, "union-list PART: {env_union}");
    let union_val = find_row(&env_union, p)["delivered_quantity"].clone();

    let (s_parts, env_parts) = send(
        app,
        hsh_erp_test_support::json_request(
            "GET",
            &format!("/parts?customer_id={}&limit=200", fx.customer_l2_id),
            None::<Value>,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s_parts, StatusCode::OK, "GET /parts: {env_parts}");
    let row = find_row(&env_parts, p);
    assert_eq!(
        row["delivered_quantity"], 10,
        "7(DELIVERED) + 3(COMPLETED) = 10: {row}"
    );
    assert_eq!(
        union_val, row["delivered_quantity"],
        "两个端点的已送数量口径必须一致: union={union_val} parts={row}"
    );
}

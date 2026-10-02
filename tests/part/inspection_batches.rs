//! part 域集成测试 —— `GET /prod/batches/inspection` 端点
//!
//! 覆盖：
//!   1. happy path：list 仅返回 INSPECTION 批次，返回的 `batch_id + version`
//!      可直接拼 `POST /prod/batches/{batch_id}/to-ship` 请求体（核心验收）。
//!   2. VO 收口守门（2026-10-03）：`items[*]` 的 key 集合**恰好** 13 个，多一个
//!      少一个都失败（前端 Zod schema 依赖这份契约）。
//!   3. 表头筛选：`drawing_no` / `name` / `system_delivery_date_from|to` /
//!      `customer_id`（L1 展开）各自命中预期行。
//!   4. 服务端排序：`sort_by` 白名单 7 值 + `sort_dir` + 非法值退化。
//!   5. 角色守卫：白名单外的角色 → 403 / 40300 FORBIDDEN。brief 原话
//!      「Worker role」并不存在，本仓库 5 角色中 ShelfAccount 是唯一合法登录、
//!      但不在 `INSPECTION_LIST_ROLES = [Manager, Inspector]` 内的角色；
//!      `PartFixture::SHELF_ACCOUNT_USERNAME` + SHELF_ACCOUNT role 提供该登录态。
//!   6. 分页：`limit + offset` 正确切分 total / items。
//!
//! ## 并行 / 认证
//! 进程级 test_pool 每次 fresh database（plan 2 2026-09-20），DB 间 schema
//! 完全独立，无需 Mutex 串行化。
//!
//! 2026-10-03 VO 收口：`holder_name` / `next_process_*` / `current_process_step_id`
//! / `status` 等 15 个字段随本 VO 一并下线（待品检页只渲染 7 个数据列），相关
//! 断言已随之删除。返修两条端点（`/repair` / `/repairing`）仍用宽 VO，其字段
//! 守门测试在 `tests/part/repair.rs`。

use axum::http::StatusCode;
use serde_json::{Value, json};
use sqlx::PgPool;

use hsh_erp_test_support::fixture::PartFixture;
use hsh_erp_test_support::*;

// ===========================================================================
//  动态 part/batch 插入 helper（tests/part/ 各 sub-file 私有，Phase G 收敛后
//  暂保留为 sub-file 内联，PR-C 末统一迁 test-support）
// ===========================================================================

/// INSPECTION 批次插入（带 holder 指向 insp_shelf_id，便于 to-ship 测试链）。
async fn insert_part_with_insp_batch(
    pool: &PgPool,
    name: &str,
    customer_id: i64,
    serial_no: Option<&str>,
    insp_shelf_id: i64,
) -> (i64, i64) {
    use hsh_erp_rust::infra::clock::now_naive;
    use hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator;
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let part_id = snowflake.next_id();
    let now = now_naive();
    let today = now.date();
    sqlx::query(
        "INSERT INTO t_part (id, serial_no, name, drawing_no, customer_id, status, \
         applicant_name, request_date, planned_delivery_date, quantity, version, \
         created_at, updated_at) \
         VALUES ($1, $2, $3, 'D-001', $4, 'INSPECTION', $3, $5, $5, 1, 0, $6, $6)",
    )
    .bind(part_id)
    .bind(serial_no)
    .bind(name)
    .bind(customer_id)
    .bind(today)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert INSPECTION part");
    let batch_id = snowflake.next_id();
    sqlx::query(
        "INSERT INTO t_part_batch (id, part_id, batch_no, quantity, status, version, \
         created_at, updated_at) \
         VALUES ($1, $2, 1, 1, 'INSPECTION', 0, $3, $3)",
    )
    .bind(batch_id)
    .bind(part_id)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert INSPECTION batch");
    sqlx::query(
        "UPDATE t_part_batch SET location = 'INSPECTION_SHELF', current_holder_id = $1 \
         WHERE id = $2",
    )
    .bind(insp_shelf_id)
    .bind(batch_id)
    .execute(pool)
    .await
    .expect("set batch holder to inspection shelf");
    (part_id, batch_id)
}

/// 造一条「真实送检后状态」的 INSPECTION 批次：**`current_process_step_id` 非空
/// （首次定位的 step）、`current_process_id` 为 NULL**。
///
/// 后者正是 review 第 2 轮 H2 修复（送检 = 出池 → 置 NULL）之后所有 INSPECTION
/// 批次的真实形态：5 个进 INSPECTION 的写点无一例外清该列。
/// 返回 (part_id, batch_id, step 所属 process_id, process_name)。
async fn insert_part_with_step_located_insp_batch(
    pool: &PgPool,
    name: &str,
    customer_id: i64,
    serial_no: &str,
    insp_shelf_id: i64,
    process_code: &str,
    process_name: &str,
) -> (i64, i64, i64, String) {
    use hsh_erp_rust::infra::clock::now_naive;
    use hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator;
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let now = now_naive();
    let today = now.date();

    // 1. 工序（逻辑 FK 目标；本测试不建 t_shelf_process 映射 —— 列表查询不需要）
    let process_id = snowflake.next_id();
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
    let chain_id = snowflake.next_id();
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
    let step_id = snowflake.next_id();
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

    // 3. INSPECTION part（绑上 chain，与真实数据一致）
    let part_id = snowflake.next_id();
    sqlx::query(
        "INSERT INTO t_part (id, serial_no, name, drawing_no, customer_id, status, \
         applicant_name, request_date, planned_delivery_date, quantity, version, \
         created_at, updated_at, process_chain_id) \
         VALUES ($1, $2, $3, 'D-001', $4, 'INSPECTION', $3, $5, $5, 1, 0, $6, $6, $7)",
    )
    .bind(part_id)
    .bind(serial_no)
    .bind(name)
    .bind(customer_id)
    .bind(today)
    .bind(now)
    .bind(chain_id)
    .execute(pool)
    .await
    .expect("insert INSPECTION part with chain");

    // 4. INSPECTION 批次：step 有值、cpid **显式置 NULL**（模拟 H2 修复后的真实形态）
    let batch_id = snowflake.next_id();
    sqlx::query(
        "INSERT INTO t_part_batch (id, part_id, batch_no, quantity, status, location, \
         current_holder_id, current_process_id, current_process_step_id, version, \
         created_at, updated_at) \
         VALUES ($1, $2, 1, 1, 'INSPECTION', 'INSPECTION_SHELF', $3, NULL, $4, 0, $5, $5)",
    )
    .bind(batch_id)
    .bind(part_id)
    .bind(insp_shelf_id)
    .bind(step_id)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert step-located INSPECTION batch");

    (part_id, batch_id, process_id, process_name.to_string())
}

// ===========================================================================
//  Tests
// ===========================================================================

/// INSPECTION part + 批次的插入规格（表头筛选 / 排序测试用）。
///
/// 收 struct 而非 9 个位置参数：`clippy::too_many_arguments` + 免得调用处数错位。
#[derive(Debug, Clone)]
struct InspBatchSpec<'a> {
    name: &'a str,
    drawing_no: &'a str,
    serial_no: Option<&'a str>,
    customer_id: i64,
    quantity: i32,
    batch_no: i32,
    /// `None` → 该工单不填系统交期（用于验证 `null` 投影与 `NULLS LAST`）。
    system_delivery_date: Option<chrono::NaiveDate>,
}

/// 按规格插入一条 INSPECTION part + 批次（holder = 品检架），返回 (part_id, batch_id)。
async fn insert_insp_batch_with_spec(
    pool: &PgPool,
    insp_shelf_id: i64,
    spec: &InspBatchSpec<'_>,
) -> (i64, i64) {
    use hsh_erp_rust::infra::clock::now_naive;
    use hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator;
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let part_id = snowflake.next_id();
    let now = now_naive();
    let today = now.date();
    sqlx::query(
        "INSERT INTO t_part (id, serial_no, name, drawing_no, customer_id, status, \
         applicant_name, request_date, planned_delivery_date, system_delivery_date, \
         quantity, version, created_at, updated_at) \
         VALUES ($1, $2, $3, $4, $5, 'INSPECTION', $3, $6, $6, $7, $8, 0, $9, $9)",
    )
    .bind(part_id)
    .bind(spec.serial_no)
    .bind(spec.name)
    .bind(spec.drawing_no)
    .bind(spec.customer_id)
    .bind(today)
    .bind(spec.system_delivery_date)
    .bind(spec.quantity)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert INSPECTION part");
    let batch_id = snowflake.next_id();
    sqlx::query(
        "INSERT INTO t_part_batch (id, part_id, batch_no, quantity, status, location, \
         current_holder_id, version, created_at, updated_at) \
         VALUES ($1, $2, $3, $4, 'INSPECTION', 'INSPECTION_SHELF', $5, 0, $6, $6)",
    )
    .bind(batch_id)
    .bind(part_id)
    .bind(spec.batch_no)
    .bind(spec.quantity)
    .bind(insp_shelf_id)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert INSPECTION batch");
    (part_id, batch_id)
}

/// 造一个 L1 客户（`serial_prefix` 单个大写字母，全库唯一）。
async fn insert_l1_customer(pool: &PgPool, name: &str, prefix: &str) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;
    use hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator;
    let id = SnowflakeIdGenerator::new(1_577_836_800_000, 1).next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_customer (id, name, parent_id, serial_prefix, version, created_at, updated_at) \
         VALUES ($1, $2, NULL, $3, 0, $4, $4)",
    )
    .bind(id)
    .bind(name)
    .bind(prefix)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert L1 customer");
    id
}

/// 取响应 `data.items[*].batch_id` 的字符串列表（雪花 id 走 `serialize_i64`）。
fn item_batch_ids(body: &Value) -> Vec<String> {
    body["data"]["items"]
        .as_array()
        .expect("data.items")
        .iter()
        .map(|i| {
            i["batch_id"]
                .as_str()
                .expect("batch_id 应为 string")
                .to_string()
        })
        .collect()
}

/// happy path：list 仅返回 INSPECTION 状态的批次；返回的 `batch_id + version`
/// 可直接喂给 `POST /prod/batches/{batch_id}/to-ship`（核心验收）。
///
/// 步骤：
///   1. 插 part A + INSPECTION 批次（qty=5，holder=INSPECTION 货架）
///   2. 插 part B + IN_PROCESS 批次（qty=3）—— 必须不出现在 list 中
///   3. GET /prod/batches/inspection?limit=10（INSPECTOR token）
///   4. 断言：
///      - status 200
///      - data.total >= 1
///      - items 包含 A 的 batch_id 且 status=="INSPECTION"
///      - items 不包含 B 的 batch_id
///      - 命中项：batch_id / part_id / version / customer_name 字段语义正确
///   5. 用 items[0].batch_id + version 调 POST /prod/batches/{batch_id}/to-ship → 200
///      （to-ship 前先 UPDATE part.current_holder_id 指向 INSPECTION 货架，
///       让 holder_name 解析为 Some；to-ship 路径本身不强制 holder，但
///       前端会基于 holder_name 渲染提示）。
#[tokio::test]
async fn inspection_batches_list_returns_only_inpection_status_with_batch_id_and_version() {
    let (pool, app, token, fx) = bootstrap_as_inspector().await;
    // part A：INSPECTION 状态 + INSPECTION 批次 + 货架 holder 指向品检架
    let (part_a, batch_a) = insert_part_with_insp_batch(
        &pool,
        "PART_A",
        fx.customer_l2_id,
        Some("P-A-001"),
        fx.inspection_shelf_id,
    )
    .await;
    // part B：IN_PROCESS 状态 + INPROCESS 批次（必须不出现在 list 中）
    use hsh_erp_rust::infra::clock::now_naive;
    use hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator;
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let part_b_id = snowflake.next_id();
    let now = now_naive();
    let today = now.date();
    sqlx::query(
        "INSERT INTO t_part (id, serial_no, name, drawing_no, customer_id, status, \
         applicant_name, request_date, planned_delivery_date, quantity, version, \
         created_at, updated_at) \
         VALUES ($1, 'P-B-001', 'PART_B', 'D-001', $2, 'IN_PROCESS', 'PART_B', $3, $3, 1, 0, $4, $4)",
    )
    .bind(part_b_id)
    .bind(fx.customer_l2_id)
    .bind(today)
    .bind(now)
    .execute(&pool)
    .await
    .expect("insert IN_PROCESS part");
    let batch_b = snowflake.next_id();
    sqlx::query(
        "INSERT INTO t_part_batch (id, part_id, batch_no, quantity, status, version, \
         created_at, updated_at) \
         VALUES ($1, $2, 1, 3, 'IN_PROCESS', 0, $3, $3)",
    )
    .bind(batch_b)
    .bind(part_b_id)
    .bind(now)
    .execute(&pool)
    .await
    .expect("insert IN_PROCESS batch");

    // Step 3：调 list 端点
    let (status, body) = send(
        app.clone(),
        json_request(
            "GET",
            "/prod/batches/inspection?limit=10",
            None::<Value>,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "list: body={body}");
    assert_eq!(body["code"], 0);
    // total 是 i64 经 `serialize_i64` → JSON string（防 JS 精度截断），
    // 约定用 `parse::<i64>()` 比较（与 part_crud.rs 一致）。
    let total_val: i64 = body["data"]["total"]
        .as_str()
        .expect("total 应为 string (i64 serialize)")
        .parse()
        .expect("total 应可解析为 i64");
    assert!(total_val >= 1, "total 应 ≥ 1（至少 part A）: body={body}");
    let items = body["data"]["items"].as_array().expect("data.items");
    assert!(
        !items.is_empty(),
        "items 应非空（至少含 part A 的 INSPECTION 行）: body={body}"
    );
    // 预拼 id 字符串（避免 `.to_string()` 在比较时被 `or_fun_call` lint 警告）
    let batch_a_str = batch_a.to_string();
    let part_a_str = part_a.to_string();

    // 命中 part A 的 batch_id 行
    let hit = items
        .iter()
        .find(|i| i["batch_id"] == batch_a_str)
        .unwrap_or_else(|| panic!("items 应含 part A 的 batch_id={batch_a}: body={body}"));
    assert_eq!(
        hit["batch_id"], batch_a_str,
        "hit.batch_id 应等于 part A 的 batch_id"
    );
    assert!(
        hit["version"].as_i64().unwrap() >= 0,
        "hit.version 应 ≥ 0 (乐观锁基线): body={body}"
    );
    assert_eq!(
        hit["part_id"], part_a_str,
        "hit.part_id 应等于 part A 的 part_id"
    );
    assert!(
        hit["customer_name"].is_string(),
        "hit.customer_name 应为 Some: body={body}"
    );
    assert_eq!(
        hit["customer_name"].as_str().unwrap(),
        "FX 客户 L2",
        "customer_name 应解析为 L2 客户名"
    );
    assert_eq!(
        hit["l1_customer_name"].as_str(),
        Some("FX 客户 L1"),
        "l1_customer_name 应解析到 L1 客户名（L2.parent_id → pc.name）: body={body}"
    );

    // 不应包含 part B 的 IN_PROCESS 批次
    let batch_b_str = batch_b.to_string();
    let contains_b = items.iter().any(|i| i["batch_id"] == batch_b_str);
    assert!(
        !contains_b,
        "items 不应含 part B 的 IN_PROCESS 批次（batch_id={batch_b}）: body={body}"
    );

    // Step 5（核心验收）：用 hit.batch_id + hit.version 调 POST /prod/batches/{batch_id}/to-ship
    let to_ship_batch_id = hit["batch_id"].as_str().unwrap().to_string();
    let to_ship_version = hit["version"].as_i64().unwrap() as i32;
    let (ship_status, ship_body) = send(
        app,
        json_request(
            "POST",
            &format!("/prod/batches/{to_ship_batch_id}/to-ship"),
            Some(json!({
                "version": to_ship_version,
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(
        ship_status,
        StatusCode::OK,
        "用 list 返回的 batch_id+version 调 to-ship 应 200，证明响应 shape 给前端够用: body={ship_body}"
    );
    assert_eq!(ship_body["code"], 0);
    assert_eq!(ship_body["data"]["part"]["status"], "READY_TO_SHIP");
}

/// 表头筛选（2026-10-03）：`drawing_no` / `name` / `serial_no` 各一个独立 ILIKE
/// 参数，且**与 `customer_id` 正交**（不再共用跨字段 `keyword`）。
#[tokio::test]
async fn inspection_batches_filters_by_header_columns() {
    let (pool, app, token, _fx) = bootstrap_as_inspector().await;
    let l1_a = insert_l1_customer(&pool, "ACMEA", "A").await;
    let l1_b = insert_l1_customer(&pool, "ACMEB", "B").await;

    let (part_a, batch_a) = insert_insp_batch_with_spec(
        &pool,
        PartFixture::INSPECTION_SHELF_ID,
        &InspBatchSpec {
            name: "PARTA",
            drawing_no: "DWG-A-001",
            serial_no: Some("PA001"),
            customer_id: l1_a,
            quantity: 5,
            batch_no: 1,
            system_delivery_date: None,
        },
    )
    .await;
    let (_part_b, batch_b) = insert_insp_batch_with_spec(
        &pool,
        PartFixture::INSPECTION_SHELF_ID,
        &InspBatchSpec {
            name: "PARTB",
            drawing_no: "DWG-B-001",
            serial_no: Some("PB001"),
            customer_id: l1_b,
            quantity: 3,
            batch_no: 1,
            system_delivery_date: None,
        },
    )
    .await;

    // 预拼 id 字符串（避免 `.to_string()` 在比较时被 `or_fun_call` lint 警告）
    let batch_a_str = batch_a.to_string();
    let batch_b_str = batch_b.to_string();
    let part_a_str = part_a.to_string();
    let l1_a_str = l1_a.to_string();

    // 1) drawing_no 独立筛选
    let (status, body) = send(
        app.clone(),
        json_request(
            "GET",
            "/prod/batches/inspection?drawing_no=DWG-A-001",
            None::<Value>,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "drawing_no filter: body={body}");
    assert_eq!(
        item_batch_ids(&body),
        vec![batch_a_str.clone()],
        "drawing_no=DWG-A-001 应只命中 A: body={body}"
    );

    // 2) name 独立筛选（值取自 B）
    let (status, body) = send(
        app.clone(),
        json_request(
            "GET",
            "/prod/batches/inspection?name=PARTB",
            None::<Value>,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "name filter: body={body}");
    assert_eq!(
        item_batch_ids(&body),
        vec![batch_b_str.clone()],
        "name=PARTB 应只命中 B: body={body}"
    );

    // 3) serial_no 独立筛选
    let (status, body) = send(
        app.clone(),
        json_request(
            "GET",
            "/prod/batches/inspection?serial_no=PB001",
            None::<Value>,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "serial_no filter: body={body}");
    assert_eq!(
        item_batch_ids(&body),
        vec![batch_b_str.clone()],
        "serial_no=PB001 应只命中 B: body={body}"
    );

    // 4) customer_id + name 组合
    let (status, body) = send(
        app.clone(),
        json_request(
            "GET",
            &format!("/prod/batches/inspection?customer_id={l1_a}&name=PARTA"),
            None::<Value>,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "combined filter: body={body}");
    let items = body["data"]["items"].as_array().expect("data.items");
    assert_eq!(items.len(), 1, "组合筛选应只命中 1 行: body={body}");
    assert_eq!(items[0]["batch_id"], batch_a_str);
    assert_eq!(
        items[0]["customer_id"], l1_a_str,
        "所有命中项 customer_id 都应 = L1_a: body={body}"
    );
    assert_eq!(
        items[0]["part_id"], part_a_str,
        "所有命中项 part_id 都应 = part_a: body={body}"
    );

    // 5) 空串筛选值 = 不筛选（表头筛选框清空态）
    let (status, body) = send(
        app.clone(),
        json_request(
            "GET",
            "/prod/batches/inspection?name=",
            None::<Value>,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "blank filter: body={body}");
    assert_eq!(
        item_batch_ids(&body).len(),
        2,
        "空串筛选不应过滤掉任何行: body={body}"
    );

    // 6) 通配符 → 40001 VALIDATION_ERROR（HTTP 422），不是 500
    let (status, body) = send(
        app,
        json_request(
            "GET",
            "/prod/batches/inspection?name=PART%25",
            None::<Value>,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "含 % 的筛选值应 422: body={body}"
    );
    assert_eq!(body["code"], 40001, "应报 VALIDATION_ERROR: body={body}");
}

/// **VO 收口守门（2026-10-03）**：`items[*]` 的 key 集合必须**恰好** 13 个。
///
/// 前端待品检页按这 13 个字段建 Zod schema，VO 多投一个字段就是无用负载，
/// 少投一个则前端渲染缺列 —— 故做「集合相等」断言（而非逐字段存在性）。
#[tokio::test]
async fn inspection_batches_item_keys_exactly_thirteen() {
    let (pool, app, token, fx) = bootstrap_as_inspector().await;
    // 一条有系统交期 + 一条无（NULL 投影 case）
    let (_p1, b1) = insert_insp_batch_with_spec(
        &pool,
        fx.inspection_shelf_id,
        &InspBatchSpec {
            name: "PART_DATE",
            drawing_no: "DWG-DATE-1",
            serial_no: Some("P-DATE-1"),
            customer_id: fx.customer_l2_id,
            quantity: 7,
            batch_no: 1,
            system_delivery_date: chrono::NaiveDate::from_ymd_opt(2026, 3, 1),
        },
    )
    .await;
    let (_p2, b2) = insert_insp_batch_with_spec(
        &pool,
        fx.inspection_shelf_id,
        &InspBatchSpec {
            name: "PART_NODATE",
            drawing_no: "DWG-DATE-2",
            serial_no: None,
            customer_id: fx.customer_l2_id,
            quantity: 2,
            batch_no: 1,
            system_delivery_date: None,
        },
    )
    .await;

    let (status, body) = send(
        app,
        json_request(
            "GET",
            "/prod/batches/inspection?limit=50",
            None::<Value>,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "list: body={body}");
    let items = body["data"]["items"].as_array().expect("data.items");

    let mut expected: Vec<&str> = vec![
        "batch_id",
        "batch_no",
        "quantity",
        "version",
        "part_id",
        "serial_no",
        "drawing_no",
        "name",
        "system_delivery_date",
        "is_urgent",
        "customer_id",
        "customer_name",
        "l1_customer_name",
    ];
    expected.sort_unstable();
    assert_eq!(expected.len(), 13, "白名单本身应是 13 个");
    for item in items {
        let obj = item.as_object().expect("item 应为 object");
        let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys, expected,
            "item key 集合应恰好 13 个（多一个 / 少一个都失败）: body={body}"
        );
    }

    // system_delivery_date / serial_no 的「有值」与「null」两种 case
    let b1_str = b1.to_string();
    let b2_str = b2.to_string();
    let hit1 = items
        .iter()
        .find(|i| i["batch_id"] == b1_str)
        .unwrap_or_else(|| panic!("应含 b1: body={body}"));
    assert_eq!(
        hit1["system_delivery_date"].as_str(),
        Some("2026-03-01"),
        "有系统交期时应投出该日期: body={body}"
    );
    assert_eq!(hit1["quantity"], 7, "数量列应透传: body={body}");
    assert_eq!(hit1["batch_no"], 1);
    assert_eq!(hit1["serial_no"].as_str(), Some("P-DATE-1"));
    assert_eq!(hit1["drawing_no"].as_str(), Some("DWG-DATE-1"));
    assert_eq!(hit1["name"].as_str(), Some("PART_DATE"));
    assert_eq!(hit1["is_urgent"], false);
    assert_eq!(hit1["customer_name"].as_str(), Some("FX 客户 L2"));
    assert_eq!(hit1["l1_customer_name"].as_str(), Some("FX 客户 L1"));
    let hit2 = items
        .iter()
        .find(|i| i["batch_id"] == b2_str)
        .unwrap_or_else(|| panic!("应含 b2: body={body}"));
    assert!(
        hit2["system_delivery_date"].is_null(),
        "无系统交期时应投影 JSON null: body={body}"
    );
    assert!(
        hit2["serial_no"].is_null(),
        "无序列号时应投影 JSON null（手工工单形态）: body={body}"
    );
}

/// 服务端排序（2026-10-03）：`sort_by` 白名单 7 值 + `sort_dir` + 非法值退化。
///
/// 4 行数据刻意让**所有排序列的升序结果一致**（`A < B < C < Z`），故一张表即可
/// 覆盖 7 个 `sort_by`；`Z` 行系统交期为 NULL，用来验证 `NULLS LAST`
/// （PG 默认 DESC → NULLS FIRST，不显式指定的话未填交期的行会顶到最前）。
#[tokio::test]
async fn inspection_batches_sorts_by_whitelisted_columns() {
    let (pool, app, token, fx) = bootstrap_as_inspector().await;
    // 4 个 L1 客户（客户名与 A/B/C/Z 同序）
    let cust_a = insert_l1_customer(&pool, "AAA Corp", "K").await;
    let cust_b = insert_l1_customer(&pool, "BBB Corp", "L").await;
    let cust_c = insert_l1_customer(&pool, "CCC Corp", "M").await;
    let cust_z = insert_l1_customer(&pool, "ZZZ Corp", "N").await;

    let rows: Vec<(i64, InspBatchSpec<'_>)> = vec![
        (
            1,
            InspBatchSpec {
                name: "AAA",
                drawing_no: "A-001",
                serial_no: Some("S-001"),
                customer_id: cust_a,
                quantity: 1,
                batch_no: 1,
                system_delivery_date: chrono::NaiveDate::from_ymd_opt(2026, 1, 1),
            },
        ),
        (
            2,
            InspBatchSpec {
                name: "BBB",
                drawing_no: "B-001",
                serial_no: Some("S-002"),
                customer_id: cust_b,
                quantity: 2,
                batch_no: 2,
                system_delivery_date: chrono::NaiveDate::from_ymd_opt(2026, 6, 1),
            },
        ),
        (
            3,
            InspBatchSpec {
                name: "CCC",
                drawing_no: "C-001",
                serial_no: Some("S-003"),
                customer_id: cust_c,
                quantity: 3,
                batch_no: 3,
                system_delivery_date: chrono::NaiveDate::from_ymd_opt(2026, 12, 1),
            },
        ),
        (
            4,
            InspBatchSpec {
                name: "ZZZ",
                drawing_no: "Z-001",
                serial_no: Some("S-004"),
                customer_id: cust_z,
                quantity: 4,
                batch_no: 4,
                system_delivery_date: None,
            },
        ),
    ];
    let mut ids: Vec<String> = Vec::new();
    for (_n, spec) in &rows {
        let (_part_id, batch_id) =
            insert_insp_batch_with_spec(&pool, fx.inspection_shelf_id, spec).await;
        ids.push(batch_id.to_string());
    }
    let (a, b, c, z) = (&ids[0], &ids[1], &ids[2], &ids[3]);
    let asc = vec![a.clone(), b.clone(), c.clone(), z.clone()];
    let desc = vec![z.clone(), c.clone(), b.clone(), a.clone()];

    // 7 个 sort_by：升序结果一致；降序仅系统交期列因 NULLS LAST 而不同
    for sort_by in [
        "SERIAL_NO",
        "DRAWING_NO",
        "NAME",
        "BATCH_NO",
        "QUANTITY",
        "CUSTOMER_NAME",
    ] {
        let (status, body) = send(
            app.clone(),
            json_request(
                "GET",
                &format!("/prod/batches/inspection?sort_by={sort_by}&sort_dir=ASC&limit=50"),
                None::<Value>,
                Some(&token),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "sort_by={sort_by}: body={body}");
        assert_eq!(
            item_batch_ids(&body),
            asc,
            "sort_by={sort_by} 的升序结果应与统一期望一致: body={body}"
        );

        let (status, body) = send(
            app.clone(),
            json_request(
                "GET",
                &format!("/prod/batches/inspection?sort_by={sort_by}&sort_dir=DESC&limit=50"),
                None::<Value>,
                Some(&token),
            ),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::OK,
            "sort_by={sort_by} DESC: body={body}"
        );
        assert_eq!(
            item_batch_ids(&body),
            desc,
            "sort_by={sort_by} 的降序结果应与统一期望一致: body={body}"
        );
    }

    // 系统交期列：ASC → NULL 兜底；DESC → NULLS LAST（不是 PG 默认的 NULLS FIRST）
    let (status, body) = send(
        app.clone(),
        json_request(
            "GET",
            "/prod/batches/inspection?sort_by=SYSTEM_DELIVERY_DATE&sort_dir=ASC&limit=50",
            None::<Value>,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "date ASC: body={body}");
    assert_eq!(
        item_batch_ids(&body),
        asc,
        "系统交期升序应 NULL 兜底在末尾: body={body}"
    );
    let (status, body) = send(
        app.clone(),
        json_request(
            "GET",
            "/prod/batches/inspection?sort_by=SYSTEM_DELIVERY_DATE&sort_dir=DESC&limit=50",
            None::<Value>,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "date DESC: body={body}");
    let date_desc = vec![c.clone(), b.clone(), a.clone(), z.clone()];
    assert_eq!(
        item_batch_ids(&body),
        date_desc,
        "系统交期降序必须 NULLS LAST（未填交期不顶到最前）: body={body}"
    );

    // 非法 sort_by / sort_dir → 退化为「系统交期 ASC」，绝不 500
    let (status, body) = send(
        app,
        json_request(
            "GET",
            "/prod/batches/inspection\
             ?sort_by=pb.id%3B%20DROP%20TABLE%20t_part_batch&sort_dir=sideways&limit=50",
            None::<Value>,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "非法 sort_by / sort_dir 应退化而非报错: body={body}"
    );
    assert_eq!(
        item_batch_ids(&body),
        asc,
        "退化排序应等同系统交期 ASC: body={body}"
    );
}

/// 系统交期区间筛选（2026-10-03：日期筛选从计划交期改筛系统交期）。
#[tokio::test]
async fn inspection_batches_filters_by_system_delivery_date_range() {
    let (pool, app, token, fx) = bootstrap_as_inspector().await;
    let d = |m: u32, day: u32| chrono::NaiveDate::from_ymd_opt(2026, m, day);
    let mk =
        |name: &'static str, sn: &'static str, date: Option<chrono::NaiveDate>| InspBatchSpec {
            name,
            drawing_no: sn,
            serial_no: Some(sn),
            customer_id: fx.customer_l2_id,
            quantity: 1,
            batch_no: 1,
            system_delivery_date: date,
        };
    let (_p1, b_early) = insert_insp_batch_with_spec(
        &pool,
        fx.inspection_shelf_id,
        &mk("RANGE-EARLY", "RG-EARLY", d(2, 1)),
    )
    .await;
    let (_p2, b_mid) = insert_insp_batch_with_spec(
        &pool,
        fx.inspection_shelf_id,
        &mk("RANGE-MID", "RG-MID", d(6, 15)),
    )
    .await;
    let (_p3, b_late) = insert_insp_batch_with_spec(
        &pool,
        fx.inspection_shelf_id,
        &mk("RANGE-LATE", "RG-LATE", d(11, 20)),
    )
    .await;
    // 无系统交期（NULL）行：任一区间筛选都应排除（NULL 比较恒不成立）
    let (_p4, b_null) = insert_insp_batch_with_spec(
        &pool,
        fx.inspection_shelf_id,
        &mk("RANGE-NULL", "RG-NULL", None),
    )
    .await;
    let (early, mid, late, null) = (
        b_early.to_string(),
        b_mid.to_string(),
        b_late.to_string(),
        b_null.to_string(),
    );

    // 闭区间 [2026-06-15, 2026-11-20] → 只命中中 + 晚
    let (status, body) = send(
        app.clone(),
        json_request(
            "GET",
            "/prod/batches/inspection?system_delivery_date_from=2026-06-15\
             &system_delivery_date_to=2026-11-20&limit=50",
            None::<Value>,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "range filter: body={body}");
    let got = item_batch_ids(&body);
    assert!(
        got.contains(&mid) && got.contains(&late),
        "区间内应命中: body={body}"
    );
    assert!(!got.contains(&early), "区间下界之前应被排除: body={body}");
    assert!(
        !got.contains(&null),
        "NULL 系统交期不被区间命中: body={body}"
    );

    // 单边：只给 from → 命中晚（6-15 与 11-20）
    let (status, body) = send(
        app.clone(),
        json_request(
            "GET",
            "/prod/batches/inspection?system_delivery_date_from=2026-06-15&limit=50",
            None::<Value>,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "from only: body={body}");
    let got = item_batch_ids(&body);
    assert!(
        got.contains(&late),
        "from=2026-06-15 应含 11-20: body={body}"
    );
    assert!(
        !got.contains(&early),
        "from=2026-06-15 不应含 02-01: body={body}"
    );

    // 单边：只给 to → 命中早 + 中
    let (status, body) = send(
        app.clone(),
        json_request(
            "GET",
            "/prod/batches/inspection?system_delivery_date_to=2026-06-15&limit=50",
            None::<Value>,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "to only: body={body}");
    let got = item_batch_ids(&body);
    assert!(
        got.contains(&early) && got.contains(&mid),
        "to=2026-06-15 应含 02-01 与 06-15: body={body}"
    );
    assert!(
        !got.contains(&late),
        "to=2026-06-15 不应含 11-20: body={body}"
    );
}

/// `customer_id` 走 L1 展开：传 L1 应同时命中其下 L2 的批次。
#[tokio::test]
async fn inspection_batches_customer_filter_expands_l1_to_l2() {
    let (pool, app, token, fx) = bootstrap_as_inspector().await;
    // fixture 的 L2 下的批次（customer_id = L2）
    let (_p_l2, b_l2) = insert_insp_batch_with_spec(
        &pool,
        fx.inspection_shelf_id,
        &InspBatchSpec {
            name: "CUSTL2",
            drawing_no: "DWG-C-1",
            serial_no: Some("CUSTL2"),
            customer_id: fx.customer_l2_id,
            quantity: 1,
            batch_no: 1,
            system_delivery_date: None,
        },
    )
    .await;
    // 另一个独立 L1 客户下的批次（该 L1 无子客户）
    let other_l1 = insert_l1_customer(&pool, "CUSTOTHER", "Z").await;
    let (_p_other, b_other) = insert_insp_batch_with_spec(
        &pool,
        fx.inspection_shelf_id,
        &InspBatchSpec {
            name: "CUSTOTHER",
            drawing_no: "DWG-C-2",
            serial_no: Some("CUSTOTH"),
            customer_id: other_l1,
            quantity: 1,
            batch_no: 1,
            system_delivery_date: None,
        },
    )
    .await;

    // 传 L1 → 展开出 [L1, L2...] → 命中 fixture L2 的批次
    let (status, body) = send(
        app.clone(),
        json_request(
            "GET",
            &format!(
                "/prod/batches/inspection?customer_id={}&limit=50",
                fx.customer_l1_id
            ),
            None::<Value>,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "customer L1 filter: body={body}");
    let got = item_batch_ids(&body);
    let l2_str = b_l2.to_string();
    let other_str = b_other.to_string();
    assert!(
        got.contains(&l2_str),
        "传 L1 应展开命中其下 L2 的批次 {l2_str}: body={body}"
    );
    assert!(
        !got.contains(&other_str),
        "别家客户的批次不应命中: body={body}"
    );

    // 传 L2 本身 → 只命中该 L2 的批次
    let (status, body) = send(
        app,
        json_request(
            "GET",
            &format!(
                "/prod/batches/inspection?customer_id={}&limit=50",
                fx.customer_l2_id
            ),
            None::<Value>,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "customer L2 filter: body={body}");
    let got = item_batch_ids(&body);
    assert_eq!(
        got,
        vec![l2_str],
        "传 L2 应只命中 L2 自己的批次: body={body}"
    );
}

// ===========================================================================
//  bootstrap helpers（PR13 Phase G 风格 B：抽出公共样板）
// ===========================================================================

async fn bootstrap_as_inspector() -> (PgPool, axum::Router, String, PartFixture) {
    let pool = test_pool().await;
    let fx = load_part_fixture(&pool).await;
    let app = test_app(test_state(pool.clone()).await);
    let token = login_token(&app, &fx.inspector_username, PartFixture::PASSWORD).await;
    (pool, app, token, fx)
}

async fn bootstrap_as_shelf_account() -> (PgPool, axum::Router, String, PartFixture) {
    let pool = test_pool().await;
    let fx = load_part_fixture(&pool).await;
    let app = test_app(test_state(pool.clone()).await);
    let token = login_token(&app, &fx.shelf_account_username, PartFixture::PASSWORD).await;
    (pool, app, token, fx)
}

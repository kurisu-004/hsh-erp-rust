//! part 域集成测试 —— `GET /parts/inspection-batches` 端点
//!
//! 覆盖：
//!   1. happy path：list 仅返回 INSPECTION 批次，返回的 `batch_id + version`
//!      可直接拼 `POST /parts/{part_id}/to-ship` 请求体（核心验收）。
//!   2. keyword + customer_id 过滤：组合筛选命中预期行（其余行被过滤）。
//!   3. 角色守卫：白名单外的角色 → 403 / 40300 FORBIDDEN。brief 原话
//!      「Worker role」并不存在，本仓库 5 角色中 ShelfAccount 是唯一合法登录、
//!      但不在 `INSPECTION_LIST_ROLES = [Manager, Inspector]` 内的角色；
//!      `PartFixture::SHELF_ACCOUNT_USERNAME` + SHELF_ACCOUNT role 提供该登录态。
//!   4. 分页：`limit + offset` 正确切分 total / items。
//!
//! ## 并行 / 认证
//! 进程级 test_pool 每次 fresh database（plan 2 2026-09-20），DB 间 schema
//! 完全独立，无需 Mutex 串行化。

use axum::http::StatusCode;
use serde_json::{Value, json};
use sqlx::PgPool;

use hsh_erp_test_support::*;
use hsh_erp_test_support::fixture::PartFixture;

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

// ===========================================================================
//  Tests
// ===========================================================================

/// happy path：list 仅返回 INSPECTION 状态的批次；返回的 `batch_id + version`
/// 可直接喂给 `POST /parts/{part_id}/to-ship`（核心验收）。
///
/// 步骤：
///   1. 插 part A + INSPECTION 批次（qty=5，holder=INSPECTION 货架）
///   2. 插 part B + IN_PROCESS 批次（qty=3）—— 必须不出现在 list 中
///   3. GET /parts/inspection-batches?limit=10（INSPECTOR token）
///   4. 断言：
///      - status 200
///      - data.total >= 1
///      - items 包含 A 的 batch_id 且 status=="INSPECTION"
///      - items 不包含 B 的 batch_id
///      - 命中项：batch_id / part_id / version / customer_name 字段语义正确
///   5. 用 items[0].batch_id + version 调 POST /parts/{A.id}/to-ship → 200
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
            "/parts/inspection-batches?limit=10",
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
    assert_eq!(hit["status"], "INSPECTION", "hit.status 应为 INSPECTION");
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
    // holder_name 由 COALESCE 三表解析，holder = INSPECTION 货架 → 应 = "FX 检验架"
    assert!(
        hit["holder_name"].is_string(),
        "hit.holder_name 应为 Some（holder 指向 INSPECTION 货架）: body={body}"
    );
    assert_eq!(
        hit["holder_name"].as_str().unwrap(),
        "FX 检验架",
        "holder_name 应解析为品检架名称"
    );

    // 不应包含 part B 的 IN_PROCESS 批次
    let batch_b_str = batch_b.to_string();
    let contains_b = items.iter().any(|i| i["batch_id"] == batch_b_str);
    assert!(
        !contains_b,
        "items 不应含 part B 的 IN_PROCESS 批次（batch_id={batch_b}）: body={body}"
    );

    // Step 5（核心验收）：用 hit.batch_id + hit.version 调 POST /parts/{part_a}/to-ship
    let to_ship_batch_id = hit["batch_id"].as_str().unwrap().to_string();
    let to_ship_version = hit["version"].as_i64().unwrap() as i32;
    let (ship_status, ship_body) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{part_a}/to-ship"),
            Some(json!({
                "batch_id": to_ship_batch_id,
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

/// keyword + customer_id 组合过滤：仅 L1_a 的 part 命中，其余被过滤。
///
/// 步骤：
///   1. 2 个 L1 客户 L1_a / L1_b（互不关联）
///   2. 每个 L1 下挂 1 个 part，名字不同（带唯一关键字）
///   3. 每个 part 都有 INSPECTION 批次
///   4. GET /parts/inspection-batches?customer_id=L1_a&keyword=<L1_a part name>
///      → items 仅含 L1_a 的 batch（L1_b 的被过滤）
///
/// **keyword 字符约束**：service 层拒绝 `%` / `_` / `\\` 通配符特殊字符
/// （VALIDATION_ERROR 40001）。关键字用大写字母串（避开 `_` / `%` / `\\`）。
#[tokio::test]
async fn inspection_batches_filters_by_keyword_and_customer() {
    let (pool, app, token, _fx) = bootstrap_as_inspector().await;
    // 建 2 个独立的 L1 客户 + L2（避免污染 fixture 内的 customer_id）
    use hsh_erp_rust::infra::clock::now_naive;
    use hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator;
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let l1_a = snowflake.next_id();
    let l1_b = snowflake.next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_customer (id, name, parent_id, serial_prefix, version, created_at, updated_at) \
         VALUES ($1, 'ACMEA', NULL, 'A', 0, $2, $2), ($3, 'ACMEB', NULL, 'B', 0, $2, $2)",
    )
    .bind(l1_a)
    .bind(now)
    .bind(l1_b)
    .execute(&pool)
    .await
    .expect("insert L1 customers");

    // L1_a 下 1 个 part
    let (part_a, batch_a) = insert_part_with_insp_batch(
        &pool,
        "PARTA",
        l1_a,
        Some("PA001"),
        PartFixture::INSPECTION_SHELF_ID,
    )
    .await;
    // L1_b 下 1 个 part
    let (_part_b, batch_b) = insert_part_with_insp_batch(
        &pool,
        "PARTB",
        l1_b,
        Some("PB001"),
        PartFixture::INSPECTION_SHELF_ID,
    )
    .await;

    // 组合过滤：customer_id=L1_a + keyword="PARTA"
    let (status, body) = send(
        app,
        json_request(
            "GET",
            &format!("/parts/inspection-batches?customer_id={l1_a}&keyword=PARTA"),
            None::<Value>,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "filtered list: body={body}");
    assert_eq!(body["code"], 0);
    let items = body["data"]["items"].as_array().expect("data.items");

    // 应仅含 L1_a 的 batch
    // 预拼 id 字符串（避免 `or_fun_call` lint 警告）
    let batch_a_str = batch_a.to_string();
    let batch_b_str = batch_b.to_string();
    let part_a_str = part_a.to_string();
    let l1_a_str = l1_a.to_string();

    // 应仅含 L1_a 的 batch
    let contains_a = items.iter().any(|i| i["batch_id"] == batch_a_str);
    assert!(
        contains_a,
        "items 应含 L1_a 的 batch_id={batch_a}: body={body}"
    );
    let contains_b = items.iter().any(|i| i["batch_id"] == batch_b_str);
    assert!(
        !contains_b,
        "items 不应含 L1_b 的 batch_id={batch_b}（被 customer_id 过滤）: body={body}"
    );
    // 进一步断言：所有命中项的 part_id 都是 part_a
    for item in items {
        assert_eq!(
            item["part_id"], part_a_str,
            "所有命中项 part_id 都应 = part_a: body={body}"
        );
        assert_eq!(
            item["customer_id"], l1_a_str,
            "所有命中项 customer_id 都应 = L1_a: body={body}"
        );
    }
}

/// 角色守卫：白名单外的角色 → 403 / 40300 FORBIDDEN。
///
/// brief 原话「Worker role」并不存在（5 角色：Manager / Clerk / Inspector /
/// CncProgrammer / ShelfAccount）。`INSPECTION_LIST_ROLES = [Manager, Inspector]`，
/// `PartFixture::SHELF_ACCOUNT_USERNAME` 用 ShelfAccount（合法登录但不在白名单内）
/// 模拟 SHELF_ACCOUNT 越权。
#[tokio::test]
async fn inspection_batches_role_guard_rejects_worker() {
    let (pool, app, token, _fx) = bootstrap_as_shelf_account().await;

    // 准备 1 个 INSPECTION 批次（让 list 在权限通过时返回非空，确保拒绝原因是角色）
    let (_part_id, _batch_id) = insert_part_with_insp_batch(
        &pool,
        "PART_RG",
        PartFixture::CUSTOMER_L2_ID,
        Some("P-RG-001"),
        PartFixture::INSPECTION_SHELF_ID,
    )
    .await;

    let (status, body) = send(
        app,
        json_request(
            "GET",
            "/parts/inspection-batches",
            None::<Value>,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "ShelfAccount 越权应 403: body={body}"
    );
    assert_eq!(
        body["code"], 40300,
        "白名单外角色应得 40300 FORBIDDEN: body={body}"
    );
    // message 形态由 require_any_role 决定，含「无权限」
    let msg = body["message"].as_str().expect("message 应为 string");
    assert!(
        msg.contains("无权限") || msg.contains("40300") || msg.contains("forbidden"),
        "message 应含权限拒绝语义（无权限 / 40300 / forbidden）: msg={msg}"
    );
}

/// 分页：`limit + offset` 正确切分 total / items / 透传 limit / offset。
///
/// 步骤：
///   1. 3 个 L1 客户各下 1 个 part，每个 part 有 INSPECTION 批次（≥3 条活跃批次）
///   2. GET /parts/inspection-batches?limit=2&offset=1
///   3. 断言：items.len() == 2；total >= 3；limit == 2；offset == 1
///
/// **L1 prefix 约束**：`t_customer.serial_prefix` 是 `varchar(1)` +
/// CHECK `^[A-Z]$`（大写字母单字符）。每个 L1 用不同大写字母当 prefix。
#[tokio::test]
async fn inspection_batches_pagination_limit_offset() {
    let (pool, app, token, _fx) = bootstrap_as_inspector().await;
    // 3 个 L1 客户（互不关联）；serial_prefix 单字符大写字母（C / D / E，
    // 跳过 A/B 避免与已有 prefix 碰撞 —— 数据库有 UNIQUE 索引约束）。
    use hsh_erp_rust::infra::clock::now_naive;
    use hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator;
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let now = now_naive();
    let l1_a = snowflake.next_id();
    let l1_c = snowflake.next_id();
    let l1_e = snowflake.next_id();
    sqlx::query(
        "INSERT INTO t_customer (id, name, parent_id, serial_prefix, version, created_at, updated_at) \
         VALUES ($1, 'PAGA', NULL, 'A', 0, $4, $4), ($2, 'PAGC', NULL, 'C', 0, $4, $4), ($3, 'PAGE', NULL, 'E', 0, $4, $4)",
    )
    .bind(l1_a)
    .bind(l1_c)
    .bind(l1_e)
    .bind(now)
    .execute(&pool)
    .await
    .expect("insert L1 customers for pagination");
    let customers = [l1_a, l1_c, l1_e];

    // 每个 L1 下 1 个 part + 1 个 INSPECTION 批次（≥3 条活跃批次）
    for (i, &cust) in customers.iter().enumerate() {
        insert_part_with_insp_batch(
            &pool,
            &format!("PAGPART{i}"),
            cust,
            Some(&format!("PAG{i:03}")),
            PartFixture::INSPECTION_SHELF_ID,
        )
        .await;
    }

    // limit=2, offset=1
    let (status, body) = send(
        app.clone(),
        json_request(
            "GET",
            "/parts/inspection-batches?limit=2&offset=1",
            None::<Value>,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "paginated list: body={body}");
    assert_eq!(body["code"], 0);

    let items = body["data"]["items"].as_array().expect("data.items");
    assert_eq!(items.len(), 2, "limit=2 时 items.len() 应 = 2: body={body}");
    let total = body["data"]["total"]
        .as_str()
        .expect("data.total 应为 string (i64 serialize)")
        .parse::<i64>()
        .expect("data.total 应可解析为 i64");
    assert!(
        total >= 3,
        "total 应 ≥ 3（插了 3 条 INSPECTION 批次）: body={body}"
    );
    // limit / offset 在响应里也是 i64 经 serialize_i64 → JSON string
    assert_eq!(
        body["data"]["limit"].as_str().unwrap(),
        "2",
        "响应 limit 应透传 = 2: body={body}"
    );
    assert_eq!(
        body["data"]["offset"].as_str().unwrap(),
        "1",
        "响应 offset 应透传 = 1: body={body}"
    );

    // 二次校验：第二页（offset=2, limit=2）应只剩 ≤ 1 条（3 - 2 = 1）
    let (status2, body2) = send(
        app.clone(),
        json_request(
            "GET",
            "/parts/inspection-batches?limit=2&offset=2",
            None::<Value>,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status2, StatusCode::OK, "second page: body={body2}");
    let items2 = body2["data"]["items"].as_array().expect("data.items2");
    assert_eq!(items2.len(), 1, "offset=2, limit=2 应剩 1 条: body={body2}");
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
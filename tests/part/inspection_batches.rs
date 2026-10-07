//! 待品检队列端点集成测试 —— `GET /api/v2/prod/inspection/queue`
//!
//! 2026-10-07：该端点自 `prod::batch` 迁入 `prod::inspection`（旧路径
//! `GET /prod/batches/inspection` 已下线、无 alias），测试内的请求路径随之改为
//! `/prod/inspection/queue`；**断言逐字未改**。文件仍留在 `tests/part/` 下（与同为
//! `prod::batch` 集合读的 `tests/part/repair.rs` 同处）—— 测试目录按历史来源归档，
//! 与端点当前的域归属无关。
//!
//! 覆盖：
//!   1. happy path：list 仅返回 INSPECTION 批次，返回的 `batch_id + version`
//!      可直接拼 `POST /prod/batches/{batch_id}/to-ship` 请求体（核心验收）。
//!   2. VO 收口守门（2026-10-03）：`items[*]` 的 key 集合**恰好** 13 个，多一个
//!      少一个都失败（前端 Zod schema 依赖这份契约）。
//!   3. 表头筛选：`drawing_no` / `name` / `system_delivery_date_from|to` /
//!      `customer_id`（L1 展开）各自命中预期行。
//!   4. 服务端排序：`sort_by` 白名单 7 值 + `sort_dir` + 非法值退化。
//!   5. 角色守卫：白名单外的角色 → 403 / 40300 FORBIDDEN（**本文件是该守卫的唯一
//!      覆盖**，守卫在 `prod::inspection::service::InspectionQueueService::list_queue`
//!      第一行）。
//!      brief 原话「Worker role」并不存在，本仓库 5 角色中 ShelfAccount 是唯一合法
//!      登录、但不在队列读白名单（`prod/inspection/service.rs` 的模块私有常量
//!      `READ_ROLES` = Manager + Inspector）内的角色；
//!      `PartFixture::SHELF_ACCOUNT_USERNAME` + SHELF_ACCOUNT role 提供该登录态。
//!   6. 分页：`limit + offset` 切分 items，且 `total` 恒等于**过滤后**的实际条数
//!      （锁住「list / count 共用同一 WHERE 拼装器」这个核心主张）。
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
//
//  2026-10-09：ID 一律从 `shared_test_snowflake()`（全进程唯一 generator 对象）取号，
//  不再就地 `SnowflakeIdGenerator::new(...)` —— 本文件 6 处局部 generator、instance 全是
//  1，同毫秒各取 seq 0 即撞 pkey（23505）；同一 `part` binary 内本文件与 `crud.rs` /
//  `serial.rs` 等文件各自 fresh 也照样撞。共享一个对象后 `next_id()` 进程内串行发号，
//  「雪花 id 恒随插入顺序升序」这条既有性质（见 `insert_insp_batch_with_spec` 的 doc）
//  不受影响。
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
    let part_id = shared_test_snowflake().next_id();
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
    let batch_id = shared_test_snowflake().next_id();
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
    let now = now_naive();
    let today = now.date();

    // 1. 工序（逻辑 FK 目标；本测试不建 t_shelf_process 映射 —— 列表查询不需要）
    let process_id = shared_test_snowflake().next_id();
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
    let chain_id = shared_test_snowflake().next_id();
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
    let step_id = shared_test_snowflake().next_id();
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
    let part_id = shared_test_snowflake().next_id();
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
    let batch_id = shared_test_snowflake().next_id();
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

/// 按规格插入一条 INSPECTION part + 批次（holder = 品检架），**两个主键都由调用方
/// 指定**，返回无（part_id 由调用方自己持有）。
///
/// 2026-10-03 新增：拆出这一层是为了让
/// `inspection_batches_pagination_tiebreak_by_batch_id_is_stable` 能**按指定顺序**
/// 写入 batch id（降序插入），从而让「`ORDER BY` 有没有 `pb.id ASC` 兜底键」变成
/// 可观测差异而不是靠 PG 恰好稳定的返回顺序。雪花 id 恒随插入顺序升序，用
/// `insert_insp_batch_with_spec` 造不出这种数据。
async fn insert_insp_batch_with_ids(
    pool: &PgPool,
    insp_shelf_id: i64,
    part_id: i64,
    batch_id: i64,
    spec: &InspBatchSpec<'_>,
) {
    use hsh_erp_rust::infra::clock::now_naive;
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
}

/// 按规格插入一条 INSPECTION part + 批次（holder = 品检架），返回 (part_id, batch_id)。
/// 主键走雪花生成器 ⇒ **恒随插入顺序升序**。
async fn insert_insp_batch_with_spec(
    pool: &PgPool,
    insp_shelf_id: i64,
    spec: &InspBatchSpec<'_>,
) -> (i64, i64) {
    let part_id = shared_test_snowflake().next_id();
    let batch_id = shared_test_snowflake().next_id();
    insert_insp_batch_with_ids(pool, insp_shelf_id, part_id, batch_id, spec).await;
    (part_id, batch_id)
}

/// 造一个 L1 客户（`serial_prefix` 单个大写字母，全库唯一）。
async fn insert_l1_customer(pool: &PgPool, name: &str, prefix: &str) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;
    let id = shared_test_snowflake().next_id();
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
///   3. GET /prod/inspection/queue?limit=10（INSPECTOR token）
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
    let part_b_id = shared_test_snowflake().next_id();
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
    let batch_b = shared_test_snowflake().next_id();
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
            "/prod/inspection/queue?limit=10",
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
            "/prod/inspection/queue?drawing_no=DWG-A-001",
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
            "/prod/inspection/queue?name=PARTB",
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
            "/prod/inspection/queue?serial_no=PB001",
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
            &format!("/prod/inspection/queue?customer_id={l1_a}&name=PARTA"),
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
            "/prod/inspection/queue?name=",
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
            "/prod/inspection/queue?name=PART%25",
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
            "/prod/inspection/queue?limit=50",
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
                &format!("/prod/inspection/queue?sort_by={sort_by}&sort_dir=ASC&limit=50"),
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
                &format!("/prod/inspection/queue?sort_by={sort_by}&sort_dir=DESC&limit=50"),
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
            "/prod/inspection/queue?sort_by=SYSTEM_DELIVERY_DATE&sort_dir=ASC&limit=50",
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
            "/prod/inspection/queue?sort_by=SYSTEM_DELIVERY_DATE&sort_dir=DESC&limit=50",
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
            "/prod/inspection/queue\
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
            "/prod/inspection/queue?system_delivery_date_from=2026-06-15\
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
            "/prod/inspection/queue?system_delivery_date_from=2026-06-15&limit=50",
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
            "/prod/inspection/queue?system_delivery_date_to=2026-06-15&limit=50",
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

/// 角色守卫：白名单外的角色 → 403 / 40300 FORBIDDEN。
///
/// brief 原话「Worker role」并不存在（本仓库 5 角色：Manager / Clerk / Inspector /
/// CncProgrammer / ShelfAccount）。队列读白名单 = Manager + Inspector，即
/// `prod/inspection/service.rs` 的模块私有常量 `READ_ROLES`（取值 Manager +
/// Inspector）；集成测试拿不到该私有项，故此处只描述取值、不做代码级引用。
/// ShelfAccount 是「能登录但在白名单外」的唯一角色，故用它模拟越权。
/// 守卫在 service 层第一行（`require_any_role`），**本文件是该守卫的唯一覆盖**。
#[tokio::test]
async fn inspection_batches_role_guard_rejects_shelf_account() {
    let (pool, app, token, _fx) = bootstrap_as_shelf_account().await;

    // 造 1 条 INSPECTION 批次：让 list 在权限通过时返回非空，确保拒绝原因确实是角色
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
        json_request("GET", "/prod/inspection/queue", None::<Value>, Some(&token)),
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

/// 分页（2026-10-03 补回）：`limit + offset` 切分 items，且 **`total` 恒等于过滤后
/// 的实际条数**。
///
/// 这条用例锁的是本次改造的核心正确性主张 —— `list_inspection_queue` 与
/// `count_inspection_queue` 共用同一个 WHERE 拼装器（`push_inspection_queue_where`），
/// 判据只此一份。本文件在改写前删掉了分页用例、也没有任何一处断言 `total` 等于实际
/// 条数，于是「count 与 items 各说各话」这类 bug 无处可卡（master 上被删的
/// `count_batches_with_part` 漏了 `JOIN t_customer c`，正是这样一只真实存在过的
/// count/list 不一致实现）。
///
/// 数据：4 行 INSPECTION 批次，全部客户 id 相同（`customer_id` 筛选留空 ⇒ 不过滤），
/// 系统交期刻意不同（1/2/3 月 + 一行 NULL），故 `sort_by=SYSTEM_DELIVERY_DATE` 升序
/// 下的 4 行顺序确定。
#[tokio::test]
async fn inspection_batches_pagination_splits_items_and_total_matches() {
    let (pool, app, token, fx) = bootstrap_as_inspector().await;
    let d = |m: u32| chrono::NaiveDate::from_ymd_opt(2026, m, 1);
    // 4 行，全部挂 fixture 的 L2 客户；交期 1/2/3 月 + NULL（NULL 在 ASC 下兜底末尾）
    let specs = [
        ("PAGE-1", "PG-001", 1, d(1)),
        ("PAGE-2", "PG-002", 2, d(2)),
        ("PAGE-3", "PG-003", 3, d(3)),
        ("PAGE-4", "PG-004", 4, None),
    ];
    let mut ids: Vec<String> = Vec::new();
    for (name, sn, qty, date) in specs {
        let (_p, batch_id) = insert_insp_batch_with_spec(
            &pool,
            fx.inspection_shelf_id,
            &InspBatchSpec {
                name,
                drawing_no: sn,
                serial_no: Some(sn),
                customer_id: fx.customer_l2_id,
                quantity: qty,
                batch_no: qty,
                system_delivery_date: date,
            },
        )
        .await;
        ids.push(batch_id.to_string());
    }
    let (p1, p2, p3, p4) = (&ids[0], &ids[1], &ids[2], &ids[3]);

    // 整页（limit=50）：items 与 total 必须都是 4，且顺序 = 交期 ASC、NULL 兜底末位
    let (status, body) = send(
        app.clone(),
        json_request(
            "GET",
            "/prod/inspection/queue?limit=50&sort_by=SYSTEM_DELIVERY_DATE&sort_dir=ASC",
            None::<Value>,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "full page: body={body}");
    let full = item_batch_ids(&body);
    assert_eq!(
        full,
        vec![p1.clone(), p2.clone(), p3.clone(), p4.clone()],
        "整页应按交期升序、NULL 兜底末位: body={body}"
    );
    let total = body["data"]["total"]
        .as_str()
        .expect("data.total 应为 string (i64 serialize)")
        .parse::<i64>()
        .expect("data.total 应可解析为 i64");
    assert_eq!(
        total,
        full.len() as i64,
        "total 应等于 items 实际条数（list / count 共用同一 WHERE）: body={body}"
    );

    // 第 1 页：limit=2&offset=0 → 前 2 行；total 仍是 4（total 不随分页变）
    let (status, body) = send(
        app.clone(),
        json_request(
            "GET",
            "/prod/inspection/queue?limit=2&offset=0&sort_by=SYSTEM_DELIVERY_DATE&sort_dir=ASC",
            None::<Value>,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "page 1: body={body}");
    assert_eq!(
        item_batch_ids(&body),
        vec![p1.clone(), p2.clone()],
        "limit=2&offset=0 应切出前 2 行: body={body}"
    );
    assert_eq!(
        body["data"]["limit"].as_str(),
        Some("2"),
        "响应 limit 应透传生效值: body={body}"
    );
    assert_eq!(
        body["data"]["offset"].as_str(),
        Some("0"),
        "响应 offset 应透传生效值: body={body}"
    );
    let total_p1 = body["data"]["total"]
        .as_str()
        .expect("data.total 应为 string")
        .parse::<i64>()
        .expect("data.total 应可解析为 i64");
    assert_eq!(total_p1, 4, "total 不随 offset 变化: body={body}");

    // 第 2 页：limit=2&offset=2 → 后 2 行，**无重复无漏行**（本用例 4 行的排序键互异，
    // 未触发 `pb.id ASC` 兜底路径；该路径由下面的并列行用例专门覆盖）
    let (status, body) = send(
        app.clone(),
        json_request(
            "GET",
            "/prod/inspection/queue?limit=2&offset=2&sort_by=SYSTEM_DELIVERY_DATE&sort_dir=ASC",
            None::<Value>,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "page 2: body={body}");
    assert_eq!(
        item_batch_ids(&body),
        vec![p3.clone(), p4.clone()],
        "limit=2&offset=2 应切出后 2 行: body={body}"
    );
    let total_p2 = body["data"]["total"]
        .as_str()
        .expect("data.total 应为 string")
        .parse::<i64>()
        .expect("data.total 应可解析为 i64");
    assert_eq!(total_p2, 4, "total 不随 offset 变化: body={body}");

    // 切到超尾页：items 空、total 仍 4
    let (status, body) = send(
        app,
        json_request(
            "GET",
            "/prod/inspection/queue?limit=2&offset=4&sort_by=SYSTEM_DELIVERY_DATE&sort_dir=ASC",
            None::<Value>,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "page 3: body={body}");
    assert!(
        item_batch_ids(&body).is_empty(),
        "offset=4（恰好超尾）应返回空 items: body={body}"
    );
    let total_p3 = body["data"]["total"]
        .as_str()
        .expect("data.total 应为 string")
        .parse::<i64>()
        .expect("data.total 应可解析为 i64");
    assert_eq!(total_p3, 4, "越界翻页 total 仍是全量条数: body={body}");
}

/// 排序键**全并列**时的 `pb.id ASC` 兜底：返回顺序必须由 batch id 升序决定，
/// 且分页两页拼回与整页同序、无重复无漏行。
///
/// 上一条 `inspection_batches_pagination_splits_items_and_total_matches` 的 4 行
/// 排序键互异（交期 1/2/3 月 + NULL），**没走过兜底键**。本条补缺口：3 行的
/// `system_delivery_date` / `batch_no` / `quantity` **全部相同**（只有 `name` /
/// `serial_no` / `drawing_no` 不同，够不上任何排序列），排序键零区分度。
///
/// **关键是按降序写 batch id**（`+2 / +1 / +0`，物理堆顺序 = 降序）。若按升序插入，
/// 兜底键在与不在都会返回升序（PG 恰好按堆顺序吐行）⇒ 断言假绿；降序插入把「PG 的
/// 自然返回顺序」与「`pb.id ASC` 要求的顺序」掰成相反方向，兜底键存在与否才成为
/// 可观测差异。本用例已用删掉 `pb.id ASC` 的变异体验证过会红。
#[tokio::test]
async fn inspection_batches_pagination_tiebreak_by_batch_id_is_stable() {
    let (pool, app, token, fx) = bootstrap_as_inspector().await;
    let tied_date = chrono::NaiveDate::from_ymd_opt(2026, 5, 1).expect("固定交期必可构造");
    // 固定 id 基座（远高于任何雪花 id，测试库内不会撞）；按 **降序** 插入 3 行全并列数据
    let id_base = 8_000_000_000_000_000_000i64;
    let mut expected_asc: Vec<String> = Vec::new();
    for (i, name) in ["TIE-A", "TIE-B", "TIE-C"].iter().enumerate() {
        let batch_id = id_base + (2 - i as i64);
        insert_insp_batch_with_ids(
            &pool,
            fx.inspection_shelf_id,
            shared_test_snowflake().next_id(),
            batch_id,
            &InspBatchSpec {
                name,
                drawing_no: "TIE-DWG",
                serial_no: Some(name),
                customer_id: fx.customer_l2_id,
                quantity: 7,
                batch_no: 7,
                system_delivery_date: Some(tied_date),
            },
        )
        .await;
        expected_asc.push(batch_id.to_string());
    }
    expected_asc.sort();
    assert_eq!(
        expected_asc,
        vec![
            (id_base).to_string(),
            (id_base + 1).to_string(),
            (id_base + 2).to_string(),
        ],
        "基座自检：3 个 batch id 应为 base+0/1/2 且升序"
    );

    // 整页（limit=50）：并列行必须按 batch id **升序**返回 —— 这条就是兜底键本身
    let (status, body) = send(
        app.clone(),
        json_request(
            "GET",
            "/prod/inspection/queue?limit=50&sort_by=SYSTEM_DELIVERY_DATE&sort_dir=ASC",
            None::<Value>,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "full page: body={body}");
    let full = item_batch_ids(&body);
    assert_eq!(
        full, expected_asc,
        "排序键全并列时，整页应严格按 batch id 升序（pb.id ASC 兜底）: body={body}"
    );
    assert_eq!(
        body["data"]["total"].as_str(),
        Some("3"),
        "total 应为 3（三行全并列不影响计数）: body={body}"
    );

    // 并列组跨 limit=2 的页边界（并列行占位置 0/1/2，边界落在 1 与 2 之间）
    let mut paged: Vec<String> = Vec::new();
    for (label, offset, expected_len) in [("page 1", 0usize, 2usize), ("page 2", 2, 1)] {
        let (status, body) = send(
            app.clone(),
            json_request(
                "GET",
                &format!(
                    "/prod/inspection/queue?limit=2&offset={offset}&sort_by=SYSTEM_DELIVERY_DATE&sort_dir=ASC"
                ),
                None::<Value>,
                Some(&token),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{label}: body={body}");
        let page = item_batch_ids(&body);
        assert_eq!(
            page.len(),
            expected_len,
            "{label} 应返回 {expected_len} 行: body={body}"
        );
        paged.extend(page);
    }

    // 核心断言：两页拼回与整页**逐位同序** —— 无重复、无漏行
    assert_eq!(
        paged, full,
        "两页拼回应与整页同序且无重复无漏行（pb.id ASC 兜底提供全序）: paged={paged:?} full={full:?}"
    );
    let mut paged_sorted = paged.clone();
    paged_sorted.sort();
    assert_eq!(
        paged_sorted, expected_asc,
        "两页拼回的 id 集合应恰好等于 3 个期望 id（无重复无漏行）: paged={paged:?}"
    );
}

/// 筛选 + 分页 + 排序三者叠加：`total` 必须等于**过滤后**的条数，不是全表条数。
///
/// 与上一条的「无过滤翻页」互补 —— 上一条证明翻页不重复不漏，本条证明
/// `count` 用的是同一份 WHERE（若 count 漏了某个过滤条件，`total` 会虚高）。
/// 另带一个 `limit × sort_by` 组合：按 `QUANTITY DESC` 排 + `limit=1`，
/// 断言「第 1 行是最大数量」+ `total` 不受排序/limit 影响。
#[tokio::test]
async fn inspection_batches_pagination_total_respects_filter_and_sort() {
    let (pool, app, token, fx) = bootstrap_as_inspector().await;
    let cust_hit = fx.customer_l2_id;
    // 3 行命中 `name=PAGECLIENT`（数量 1 / 5 / 9），2 行不命中（name 其它值）
    // serial_no 有唯一约束 ⇒ 逐行唯一（filter 走 name，不依赖 serial_no）
    for (name, qty) in [
        ("PAGECLIENT-1", 1),
        ("PAGECLIENT-5", 5),
        ("PAGECLIENT-9", 9),
        ("PAGEXOTHER-3", 3),
        ("PAGEXOTHER-7", 7),
    ] {
        insert_insp_batch_with_spec(
            &pool,
            fx.inspection_shelf_id,
            &InspBatchSpec {
                name,
                drawing_no: "PGCL-001",
                serial_no: Some(name),
                customer_id: cust_hit,
                quantity: qty,
                batch_no: qty,
                system_delivery_date: None,
            },
        )
        .await;
    }

    // 1) name 过滤 + limit=1 + QUANTITY DESC：命中 3 行中的最大数量那条
    let (status, body) = send(
        app.clone(),
        json_request(
            "GET",
            "/prod/inspection/queue?name=PAGECLIENT&sort_by=QUANTITY&sort_dir=DESC&limit=1",
            None::<Value>,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "filtered+sorted+limit: body={body}");
    let items = body["data"]["items"].as_array().expect("data.items");
    assert_eq!(items.len(), 1, "limit=1 应只返回 1 行: body={body}");
    assert_eq!(
        items[0]["quantity"], 9,
        "QUANTITY DESC 的第 1 行应是最大数量: body={body}"
    );
    let total = body["data"]["total"]
        .as_str()
        .expect("data.total 应为 string (i64 serialize)")
        .parse::<i64>()
        .expect("data.total 应可解析为 i64");
    assert_eq!(
        total, 3,
        "total 应是过滤后的条数（3），既不是全表 5 也不受 limit/排序影响: body={body}"
    );

    // 2) 不加 name 过滤 → total 回到 5（证明 1) 的 3 是过滤生效，不是巧合）
    let (status, body) = send(
        app,
        json_request(
            "GET",
            "/prod/inspection/queue?sort_by=QUANTITY&sort_dir=DESC&limit=1",
            None::<Value>,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "unfiltered: body={body}");
    let total_all = body["data"]["total"]
        .as_str()
        .expect("data.total 应为 string (i64 serialize)")
        .parse::<i64>()
        .expect("data.total 应可解析为 i64");
    assert_eq!(
        total_all, 5,
        "无过滤时 total 应是全表 INSPECTION 条数: body={body}"
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
                "/prod/inspection/queue?customer_id={}&limit=50",
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
                "/prod/inspection/queue?customer_id={}&limit=50",
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

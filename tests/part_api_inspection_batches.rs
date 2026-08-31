//! part 域集成测试 —— `GET /parts/inspection-batches` 端点
//!
//! 覆盖：
//!   1. happy path：list 仅返回 INSPECTION 批次，返回的 `batch_id + version`
//!      可直接拼 `POST /parts/{part_id}/to-ship` 请求体（核心验收）。
//!   2. keyword + customer_id 过滤：组合筛选命中预期行（其余行被过滤）。
//!   3. 角色守卫：白名单外的角色 → 403 / 40300 FORBIDDEN。brief 原话
//!      「Worker role」并不存在，本仓库 5 角色中 ShelfAccount 是唯一合法登录、
//!      但不在 `INSPECTION_LIST_ROLES = [Manager, Inspector]` 内的角色；
//!      helpers::login_worker 用 ShelfAccount，与既有 part_api_to_inspection 测 8
//!      同形。
//!   4. 分页：`limit + offset` 正确切分 total / items。
//!
//! ## 并行 / 认证
//! 共享 `postgres_rust_test`；进程级 `tokio::sync::Mutex` 串行化。

#[path = "part_api_helpers.rs"]
mod helpers;

use axum::http::StatusCode;
use serde_json::{json, Value};

use helpers::*;

// ===========================================================================
//  Tests
// ===========================================================================

/// happy path：list 仅返回 INSPECTION 状态的批次；返回的 `batch_id + version`
/// 可直接喂给 `POST /parts/{part_id}/to-ship`（核心验收）。
///
/// 步骤：
///   1. 插 part A + INSPECTION 批次（qty=5）
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
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "F", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let (insp_shelf, _prod_shelf, _proc) =
        setup_inspection_and_production_shelves(&pool).await;

    // part A：INSPECTION 状态 + INSPECTION 批次 + 货架 holder 指向品检架
    let part_a = insert_part_with_status(
        &pool,
        "PART_A",
        l2,
        Some("P-A-001"),
        None,
        "INSPECTION",
    )
    .await;
    let batch_a = insert_batch(&pool, part_a, 1, 5, "INSPECTION").await;
    sqlx::query!(
        "UPDATE t_part SET current_holder_id = $1 WHERE id = $2",
        insp_shelf, part_a
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query!(
        "UPDATE t_part_batch SET current_holder_id = $1 WHERE id = $2",
        insp_shelf, batch_a
    )
    .execute(&pool)
    .await
    .unwrap();

    // part B：IN_PROCESS 状态 + IN_PROCESS 批次（必须不出现在 list 中）
    let _part_b = insert_part_with_status(
        &pool,
        "PART_B",
        l2,
        Some("P-B-001"),
        None,
        "IN_PROCESS",
    )
    .await;
    let batch_b = insert_batch(&pool, _part_b, 1, 3, "IN_PROCESS").await;

    let (app, token, _pool) = login_inspector(pool, "inspector_list").await;

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
    assert!(
        total_val >= 1,
        "total 应 ≥ 1（至少 part A）: body={body}"
    );
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
        "二厂",
        "customer_name 应解析为 L2 客户名"
    );
    // holder_name 由 COALESCE 三表解析，holder = INSPECTION 货架 → 应 = "品检架A"
    assert!(
        hit["holder_name"].is_string(),
        "hit.holder_name 应为 Some（holder 指向 INSPECTION 货架）: body={body}"
    );
    assert_eq!(
        hit["holder_name"].as_str().unwrap(),
        "品检架A",
        "holder_name 应解析为品检架名称"
    );

    // 不应包含 part B 的 IN_PROCESS 批次
    let batch_b_str = batch_b.to_string();
    let contains_b = items
        .iter()
        .any(|i| i["batch_id"] == batch_b_str);
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
    let (_guard, pool) = setup().await;
    let l1_a = insert_l1(&pool, "ACMEA", "A").await;
    let l1_b = insert_l1(&pool, "ACMEB", "B").await;
    let (insp_shelf, _prod_shelf, _proc) =
        setup_inspection_and_production_shelves(&pool).await;

    // L1_a 下 1 个 part（用 L1_a 自己作为 customer_id：expand_customer_id
    // 对 L1 返回 [L1_a]，L1_b 不会命中）
    let part_a = insert_part_with_status(
        &pool,
        "PARTA",
        l1_a,
        Some("PA001"),
        None,
        "INSPECTION",
    )
    .await;
    let batch_a = insert_batch(&pool, part_a, 1, 2, "INSPECTION").await;
    sqlx::query!(
        "UPDATE t_part_batch SET current_holder_id = $1 WHERE id = $2",
        insp_shelf, batch_a
    )
    .execute(&pool)
    .await
    .unwrap();

    // L1_b 下 1 个 part
    let part_b = insert_part_with_status(
        &pool,
        "PARTB",
        l1_b,
        Some("PB001"),
        None,
        "INSPECTION",
    )
    .await;
    let batch_b = insert_batch(&pool, part_b, 1, 2, "INSPECTION").await;
    sqlx::query!(
        "UPDATE t_part_batch SET current_holder_id = $1 WHERE id = $2",
        insp_shelf, batch_b
    )
    .execute(&pool)
    .await
    .unwrap();

    let (app, token, _pool) = login_inspector(pool, "inspector_filter").await;

    // 组合过滤：customer_id=L1_a + keyword="PARTA"
    let (status, body) = send(
        app,
        json_request(
            "GET",
            &format!(
                "/parts/inspection-batches?customer_id={l1_a}&keyword=PARTA"
            ),
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
    let contains_a = items
        .iter()
        .any(|i| i["batch_id"] == batch_a_str);
    assert!(
        contains_a,
        "items 应含 L1_a 的 batch_id={batch_a}: body={body}"
    );
    let contains_b = items
        .iter()
        .any(|i| i["batch_id"] == batch_b_str);
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
/// `helpers::login_worker` 用 ShelfAccount（合法登录但不在白名单内）模拟。
#[tokio::test]
async fn inspection_batches_role_guard_rejects_worker() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "F", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let (insp_shelf, _prod_shelf, _proc) =
        setup_inspection_and_production_shelves(&pool).await;

    // 准备 1 个 INSPECTION 批次（让 list 在权限通过时返回非空，确保拒绝原因是角色）
    let part_id = insert_part_with_status(
        &pool,
        "PART_RG",
        l2,
        Some("P-RG-001"),
        None,
        "INSPECTION",
    )
    .await;
    let batch_id = insert_batch(&pool, part_id, 1, 1, "INSPECTION").await;
    sqlx::query!(
        "UPDATE t_part_batch SET current_holder_id = $1 WHERE id = $2",
        insp_shelf, batch_id
    )
    .execute(&pool)
    .await
    .unwrap();

    // 用 login_worker 拿一个 ShelfAccount 的合法 token
    let (app, token, _pool) = login_worker(pool, "worker_rg").await;

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
    let msg = body["message"]
        .as_str()
        .expect("message 应为 string");
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
    let (_guard, pool) = setup().await;
    let (insp_shelf, _prod_shelf, _proc) =
        setup_inspection_and_production_shelves(&pool).await;
    // 3 个 L1 客户（互不关联）；serial_prefix 单字符大写字母（A / C / E，
    // 故意跳过 B/D 避免与已有 prefix 碰撞 —— 数据库有 UNIQUE 索引约束）。
    let l1_a = insert_l1(&pool, "PAGA", "A").await;
    let l1_c = insert_l1(&pool, "PAGC", "C").await;
    let l1_e = insert_l1(&pool, "PAGE", "E").await;
    let customers = [l1_a, l1_c, l1_e];

    // 每个 L1 下 1 个 part + 1 个 INSPECTION 批次（≥3 条活跃批次）
    let mut part_ids = Vec::new();
    for (i, &cust) in customers.iter().enumerate() {
        let pid = insert_part_with_status(
            &pool,
            &format!("PAGPART{i}"),
            cust,
            Some(&format!("PAG{i:03}")),
            None,
            "INSPECTION",
        )
        .await;
        let bid = insert_batch(&pool, pid, 1, 1, "INSPECTION").await;
        sqlx::query!(
            "UPDATE t_part_batch SET current_holder_id = $1 WHERE id = $2",
            insp_shelf, bid
        )
        .execute(&pool)
        .await
        .unwrap();
        part_ids.push(pid);
    }

    let (app, token, _pool) = login_inspector(pool, "inspector_pag").await;

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
    assert_eq!(
        items.len(),
        2,
        "limit=2 时 items.len() 应 = 2: body={body}"
    );
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
    assert_eq!(
        items2.len(),
        1,
        "offset=2, limit=2 应剩 1 条: body={body2}"
    );
}
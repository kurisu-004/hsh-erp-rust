//! part 域 Phase 1（2026-09-13）批次集成测试：1.5 拆分/取消 + 1.6 事件/位置树。
//!
//! 覆盖：
//!   - split_batch: happy path + 数量校验 + 批次守恒不变量
//!   - cancel_batch: happy path + 终态保护
//!   - list_batches: 工单全部活跃批次
//!   - list_events: 工单事件历史
//!   - location_tree: 位置树聚合
//!
//! ## 批次守恒不变量测试
//! `Σ(未删批次.quantity) = t_part.quantity` 必须保持 —— 用 `invariant` 命名空间测试。

use axum::http::StatusCode;
use serde_json::json;
use sqlx::PgPool;

use hsh_erp_test_support::fixture::PartFixture;
use hsh_erp_test_support::*;

// ===========================================================================
//  动态 part/batch 插入 helper（sub-file 私有，PR-C 末统一迁）
// ===========================================================================

async fn insert_part_with_batch(
    pool: &PgPool,
    name: &str,
    customer_id: i64,
    status: &str,
    qty: i32,
) -> (i64, i64) {
    use hsh_erp_rust::infra::clock::now_naive;
    use hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator;
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let part_id = snowflake.next_id();
    let batch_id = snowflake.next_id();
    let now = now_naive();
    let today = now.date();
    sqlx::query(
        "INSERT INTO t_part (id, serial_no, name, drawing_no, customer_id, status, \
         applicant_name, request_date, planned_delivery_date, quantity, version, \
         created_at, updated_at) \
         VALUES ($1, NULL, $2, 'D-001', $3, $6, $2, $4, $4, 1, 0, $5, $5)",
    )
    .bind(part_id)
    .bind(name)
    .bind(customer_id)
    .bind(today)
    .bind(now)
    .bind(status)
    .execute(pool)
    .await
    .expect("insert part");
    sqlx::query(
        "INSERT INTO t_part_batch (id, part_id, batch_no, quantity, status, version, \
         created_at, updated_at) \
         VALUES ($1, $2, 1, $3, $4, 0, $5, $5)",
    )
    .bind(batch_id)
    .bind(part_id)
    .bind(qty)
    .bind(status)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert batch");
    (part_id, batch_id)
}

async fn insert_extra_batch(
    pool: &PgPool,
    part_id: i64,
    batch_no: i32,
    qty: i32,
    status: &str,
) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;
    use hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator;
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let batch_id = snowflake.next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_part_batch (id, part_id, batch_no, quantity, status, version, \
         created_at, updated_at) \
         VALUES ($1, $2, $3, $4, $5, 0, $6, $6)",
    )
    .bind(batch_id)
    .bind(part_id)
    .bind(batch_no)
    .bind(qty)
    .bind(status)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert extra batch");
    batch_id
}

async fn batch_version(pool: &PgPool, batch_id: i64) -> i32 {
    sqlx::query_scalar::<_, i32>("SELECT version FROM t_part_batch WHERE id = $1")
        .bind(batch_id)
        .fetch_one(pool)
        .await
        .expect("batch not found")
}

// ===========================================================================
//  bootstrap helpers
// ===========================================================================

async fn bootstrap_as_manager() -> (PgPool, axum::Router, String, PartFixture) {
    let pool = test_pool().await;
    let fx = load_part_fixture(&pool).await;
    let app = test_app(test_state(pool.clone()).await);
    let token = login_token(&app, &fx.manager_username, PartFixture::PASSWORD).await;
    (pool, app, token, fx)
}

// ===========================================================================
//  拆分 / 取消 / 列表
// ===========================================================================

#[tokio::test]
async fn split_batch_happy_path() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let (_pid, bid) = insert_part_with_batch(&pool, "P0", fx.customer_l2_id, "PENDING", 10).await;
    let version = batch_version(&pool, bid).await;
    let body = json!({
        "version": version,
        "quantity": "3",
    });
    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/prod/batches/{bid}/split"),
            Some(body),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "split: {env}");
    assert_eq!(env["code"], 0);
    let new_batch_id = env["data"].as_i64().expect("data is i64");
    assert!(new_batch_id > 0);
}

#[tokio::test]
async fn split_batch_invalid_quantity_rejects() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let (_pid, bid) = insert_part_with_batch(&pool, "P0", fx.customer_l2_id, "PENDING", 10).await;
    let version = batch_version(&pool, bid).await;
    // quantity == batch.quantity (不允许，等于整批)
    let body = json!({
        "batch_id": bid.to_string(),
        "version": version,
        "quantity": "10",
    });
    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/prod/batches/{bid}/split"),
            Some(body),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "quantity=全量应拒绝: {env}");
    assert_eq!(env["code"], 20111);
}

#[tokio::test]
async fn split_batch_quantity_negative_rejects() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let (_pid, bid) = insert_part_with_batch(&pool, "P0", fx.customer_l2_id, "PENDING", 10).await;
    let version = batch_version(&pool, bid).await;
    let body = json!({
        "batch_id": bid.to_string(),
        "version": version,
        "quantity": "-1",
    });
    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/prod/batches/{bid}/split"),
            Some(body),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "quantity<0 应拒绝: {env}");
    assert_eq!(env["code"], 20111);
}

// ===== 批次守恒不变量测试 =====

#[tokio::test]
async fn invariant_split_preserves_total_quantity() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let (pid, bid) = insert_part_with_batch(&pool, "P0", fx.customer_l2_id, "PENDING", 10).await;
    let version = batch_version(&pool, bid).await;
    let body = json!({
        "version": version,
        "quantity": "3",
    });
    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/prod/batches/{bid}/split"),
            Some(body),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "split: {env}");
    // 不变量：Σ quantity == 10
    let total: i64 = sqlx::query_scalar(
        "SELECT COALESCE(SUM(quantity), 0)::bigint FROM t_part_batch WHERE part_id = $1 AND deleted_at IS NULL",
    )
    .bind(pid)
    .fetch_one(&pool)
    .await
    .expect("sum quantity");
    assert_eq!(total, 10, "拆批前后总件数必须守恒 (10=3+7): {env}");
}

#[tokio::test]
async fn cancel_batch_happy_path() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let (_pid, bid) = insert_part_with_batch(&pool, "P0", fx.customer_l2_id, "PENDING", 5).await;
    let version = batch_version(&pool, bid).await;
    let body = json!({
        "version": version,
    });
    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/prod/batches/{bid}/cancel"),
            Some(body),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "cancel-batch: {env}");
    assert_eq!(env["code"], 0);
}

#[tokio::test]
async fn cancel_batch_terminal_protection() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let (_pid, bid) = insert_part_with_batch(&pool, "P0", fx.customer_l2_id, "COMPLETED", 5).await;
    let version = batch_version(&pool, bid).await;
    let body = json!({
        "version": version,
    });
    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/prod/batches/{bid}/cancel"),
            Some(body),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "COMPLETED 批次禁止取消: {env}");
    assert_eq!(env["code"], 20103);
}

#[tokio::test]
async fn list_batches_happy_path() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let (pid, _bid) = insert_part_with_batch(&pool, "P0", fx.customer_l2_id, "PENDING", 5).await;
    insert_extra_batch(&pool, pid, 2, 3, "PENDING").await;
    let (s, env) = send(
        app,
        json_request("GET", &format!("/parts/{pid}/batches"), None, Some(&token)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "list_batches: {env}");
    assert_eq!(env["code"], 0);
    let items = env["data"].as_array().expect("data is array");
    assert_eq!(items.len(), 2);
    // 2026-09-30 Phase 2 dashboard 二次调整：验证 7 新字段（含 holder_name → current_holder_display 重命名）
    for (i, item) in items.iter().enumerate() {
        assert!(item["id"].is_string(), "items[{i}].id is string");
        assert!(item["part_id"].is_string(), "items[{i}].part_id is string");
        assert_eq!(
            item["part_id"].as_str().unwrap(),
            pid.to_string(),
            "items[{i}].part_id matches URL"
        );
        assert!(
            item["batch_label"].is_string(),
            "items[{i}].batch_label is string"
        );
        assert_eq!(
            item["batch_label"].as_str().unwrap(),
            format!("L{}", item["id"].as_str().unwrap()),
            "items[{i}].batch_label is L{{id}}"
        );
        assert!(
            item["current_holder_display"].is_null(),
            "items[{i}].current_holder_display present (null ok)"
        );
        assert!(
            item["current_process_step_id"].is_null(),
            "items[{i}].current_process_step_id present (null ok)"
        );
        assert!(
            item["next_process_name"].is_null(),
            "items[{i}].next_process_name present (null ok)"
        );
        assert!(
            item["delivery_note_no"].is_null(),
            "items[{i}].delivery_note_no present (null ok)"
        );
        assert!(
            item["created_at"].is_string(),
            "items[{i}].created_at is ISO string"
        );
        assert!(
            item["updated_at"].is_string(),
            "items[{i}].updated_at is ISO string"
        );
        assert!(item["version"].is_number(), "items[{i}].version is number");
        // 2026-09-30 Phase 2：验证旧字段 holder_name 不应再出现在响应里
        assert!(
            item.get("holder_name").is_none(),
            "items[{i}].holder_name should be renamed to current_holder_display"
        );
    }
}

#[tokio::test]
async fn list_events_happy_path() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let (pid, _bid) = insert_part_with_batch(&pool, "P0", fx.customer_l2_id, "PENDING", 1).await;
    let (s, env) = send(
        app,
        json_request("GET", &format!("/parts/{pid}/events"), None, Some(&token)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "list_events: {env}");
    assert_eq!(env["code"], 0);
    let items = env["data"].as_array().expect("data is array");
    assert!(items.is_empty(), "新工单无事件");
}

#[tokio::test]
async fn location_tree_happy_path() {
    let (_pool, app, token, _fx) = bootstrap_as_manager().await;
    let (s, env) = send(
        app,
        json_request("GET", "/parts/location-tree", None, Some(&token)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "location-tree: {env}");
    assert_eq!(env["code"], 0);
    let items = env["data"]["items"].as_array().expect("data.items");
    assert!(
        !items.is_empty(),
        "至少返回 OFFICE / PRODUCTION_SHELF 等父节点"
    );
}

// ===== 2026-09-29 扁平化新测试：batch_create_with_bindings_object_key_test =====
//
// 走完整 service 链路（service 调用 copy_object），断言 part_file 行的 object_key
// 已切换到新两段模板 `{prefix}{sha16}_{safe_filename}`，不再含 owner_kind/owner_id/
// KIND 段。

#[tokio::test]
async fn batch_create_with_bindings_object_key_test() {
    use hsh_erp_rust::auth::rbac::Role;
    use hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator;
    use hsh_erp_rust::modules::part::dto_crud::{
        FileBindingIn, PartBatchCreateItem, PartBatchCreateRequest,
    };
    use hsh_erp_rust::modules::part::service::PartService;
    use hsh_erp_test_support::fixture::PartFixture;

    let pool = test_pool().await;
    let fx: PartFixture = load_part_fixture(&pool).await;
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    // 2026-09-29 扁平化新增测试：本文件无 manager_current 私有 helper（仅 crud.rs 有），
    // 直接构造 Manager CurrentUser（与 file.rs::test_current_user_with_roles 同形）。
    let current = hsh_erp_rust::auth::rbac::CurrentUser {
        id: 1,
        username: "test-manager".into(),
        roles: vec![Role::Manager],
        shelf_ids: vec![],
        shelf_wildcard: false,
    };
    let cos = std::sync::Arc::new(MockCos::new());

    let tmp_key = "tmp/test/binding-flat.pdf";
    let sha = "1".repeat(64);
    cos.set_head(tmp_key, 1024);

    let today = chrono::Utc::now()
        .with_timezone(&chrono::FixedOffset::east_opt(8 * 3600).unwrap())
        .date_naive();
    let req = PartBatchCreateRequest {
        customer_id: fx.customer_l2_id,
        items: vec![PartBatchCreateItem {
            name: "ok-item".into(),
            drawing_no: "D-FLAT".into(),
            applicant_name: "X".into(),
            quantity: 1,
            request_date: today,
            planned_delivery_date: today,
            is_urgent: false,
            order_no: None,
            system_delivery_date: None,
            note: None,
            assembly_id: None,
            drawing_file: Some(FileBindingIn {
                tmp_key: tmp_key.into(),
                content_sha256: sha.clone(),
                original_filename: "flat.pdf".into(),
                file_size: 1024,
                content_type: "application/pdf".into(),
                ext: None, // 2026-09-29 新增字段
            }),
            model3d_file: None,
        }],
    };

    let mut tx = pool.begin().await.unwrap();
    let (out, _keys) = PartService::batch_create_parts_with_bindings(
        &mut *tx,
        &snowflake,
        cos.clone(),
        "uploads",
        "tmp/",
        &req,
        &current,
    )
    .await
    .map_err(|(e, _)| e)
    .expect("batch_create_parts_with_bindings 应 Ok");
    tx.commit().await.unwrap();

    // 直接查 DB 找 part_file 行：按 owner_id + kind 找出刚生成的 part_file
    let new_part_id = out.created[0].part.id;
    let mut tx = pool.begin().await.unwrap();
    let rows = hsh_erp_rust::modules::part_file::repo::PartFileRepo::list_by_owner(
        &mut *tx,
        "PART",
        new_part_id,
    )
    .await
    .unwrap();
    drop(tx);

    assert_eq!(rows.len(), 1, "新 part 应该有 1 个 part_file 行");
    let pf = &rows[0];

    // 新模板两段：`{prefix}{sha16}_{safe_filename}`，不含 owner_kind/owner_id/KIND
    let expected_cas_key = "uploads/1111111111111111_flat.pdf";
    assert_eq!(
        pf.object_key, expected_cas_key,
        "2026-09-29 扁平化：DB 行 object_key 必须为新两段模板（不含 owner_kind/owner_id/KIND）"
    );
    // 反向断言：旧五段前缀不能出现
    assert!(
        !pf.object_key.contains("/part/") && !pf.object_key.contains("/DRAWING/"),
        "object_key 不应再含 owner_kind 'part/' 或 KIND 'DRAWING/'"
    );
}

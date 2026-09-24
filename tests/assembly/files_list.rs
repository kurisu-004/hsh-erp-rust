//! assembly 域 集成测试 — 2026-09-25 新增 D-09 `GET /assemblies/{id}/files`
//!
//! 覆盖场景：
//!   1. files_list_empty: 新建装配体无文件 → 返回空列表
//!   2. files_list_with_one: 直接 SQL 写一条 ASSEMBLY_MASTER → list 见 1
//!   3. files_list_part_kind_filtered: 直接写一条 PART 类型的 part_file → list 不应返回（只过滤 ASSEMBLY_MASTER）
//!   4. files_list_assembly_not_found: 不存在的 assembly_id → 20301

#![allow(dead_code, clippy::await_holding_lock, unused_imports)]

use sqlx::PgPool;

use hsh_erp_rust::auth::rbac::{CurrentUser, Role};
use hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator;
use hsh_erp_rust::modules::assembly::dto::AssemblyCreateRequest;
use hsh_erp_rust::modules::assembly::service::AssemblyService;
use hsh_erp_rust::shared::error::AppError;

use hsh_erp_test_support::{AssemblyFixture, load_assembly_fixture, test_pool};

async fn setup() -> (PgPool, AssemblyFixture) {
    let pool = test_pool().await;
    let fx = load_assembly_fixture(&pool).await;
    (pool, fx)
}

async fn insert_l1_customer(pool: &PgPool, name: &str, serial_prefix: &str) -> i64 {
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let id = snowflake.next_id();
    let now = chrono::Local::now().naive_utc();
    sqlx::query!(
        "INSERT INTO t_customer (id, name, parent_id, serial_prefix, version, \
         created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, NULL, $3, 0, $4, NULL, $4, NULL)",
        id,
        name,
        serial_prefix,
        now,
    )
    .execute(pool)
    .await
    .expect("insert L1 customer");
    id
}

async fn insert_l2_customer(pool: &PgPool, name: &str, parent_id: i64) -> i64 {
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let id = snowflake.next_id();
    let now = chrono::Local::now().naive_utc();
    sqlx::query!(
        "INSERT INTO t_customer (id, name, parent_id, serial_prefix, version, \
         created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, $3, NULL, 0, $4, NULL, $4, NULL)",
        id,
        name,
        parent_id,
        now,
    )
    .execute(pool)
    .await
    .expect("insert L2 customer");
    id
}

fn test_current_user() -> CurrentUser {
    CurrentUser {
        id: 1,
        username: "files_list_tester".into(),
        roles: vec![Role::Manager],
        shelf_ids: vec![],
        shelf_wildcard: false,
    }
}

/// 直接 INSERT 一条 t_part_file（绕过 COS 上传；只为 list 路径测试）
async fn insert_part_file_row(
    pool: &PgPool,
    owner_id: i64,
    _owner_kind: &str,
    kind: &str,
    filename: &str,
) -> i64 {
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let id = snowflake.next_id();
    sqlx::query!(
        "INSERT INTO t_part_file (id, part_id, kind, file_type, object_key, \
         original_filename, file_size, content_type, upload_status, \
         created_at, created_by, updated_at, updated_by, version) \
         VALUES ($1, $2, $3, 'PDF', $4, $5, 1024, 'application/pdf', 'READY', \
                 now(), 1, now(), 1, 0)",
        id,
        owner_id,
        kind,
        format!("assembly/{owner_id}/{kind}/test-key"),
        filename,
    )
    .execute(pool)
    .await
    .expect("insert t_part_file fixture");
    id
}

// ===========================================================================
//  Tests
// ===========================================================================

/// 1. 新建装配体（无 PDF / 无任何 file 上传）→ list 返回空列表。
#[tokio::test]
async fn files_list_empty() {
    let (pool, _fx) = setup().await;
    let l1 = insert_l1_customer(&pool, "客户FL-1", "F").await;
    let l2 = insert_l2_customer(&pool, "子客FL-1", l1).await;
    let current = test_current_user();
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);

    // 建空装配体
    let req = AssemblyCreateRequest {
        drawing_no: "FL-D001".into(),
        name: "ASM-FL-1".into(),
        applicant_name: None,
        customer_id: l2.to_string(),
        request_date: None,
        planned_delivery_date: None,
        is_urgent: Some(false),
        quantity: Some(1),
        unit_price: None,
        total_price: None,
        order_no: None,
        system_delivery_date: None,
        note: None,
        children: vec![],
    };
    let mut tx = pool.begin().await.unwrap();
    let asm = AssemblyService::create_assembly(&mut tx, &snowflake, &req, vec![], &current)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let asm_id = asm.assembly.id;

    // list
    let mut conn = pool.acquire().await.unwrap();
    let out = AssemblyService::list_assembly_files(&mut conn, asm_id, &current)
        .await
        .expect("list files empty");
    drop(conn);

    assert!(out.items.is_empty(), "无文件时 items 应为空");
    assert_eq!(out.total, 0);
}

/// 2. 直接写一条 ASSEMBLY_MASTER 文件 → list 应返回 1 条。
#[tokio::test]
async fn files_list_with_one() {
    let (pool, _fx) = setup().await;
    let l1 = insert_l1_customer(&pool, "客户FL-2", "F").await;
    let l2 = insert_l2_customer(&pool, "子客FL-2", l1).await;
    let current = test_current_user();
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);

    // 建空装配体
    let req = AssemblyCreateRequest {
        drawing_no: "FL-D002".into(),
        name: "ASM-FL-2".into(),
        applicant_name: None,
        customer_id: l2.to_string(),
        request_date: None,
        planned_delivery_date: None,
        is_urgent: Some(false),
        quantity: Some(1),
        unit_price: None,
        total_price: None,
        order_no: None,
        system_delivery_date: None,
        note: None,
        children: vec![],
    };
    let mut tx = pool.begin().await.unwrap();
    let asm = AssemblyService::create_assembly(&mut tx, &snowflake, &req, vec![], &current)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let asm_id = asm.assembly.id;

    // 写 1 条 ASSEMBLY_MASTER 文件
    let file_id =
        insert_part_file_row(&pool, asm_id, "ASSEMBLY", "ASSEMBLY_MASTER", "test.pdf").await;

    // list
    let mut conn = pool.acquire().await.unwrap();
    let out = AssemblyService::list_assembly_files(&mut conn, asm_id, &current)
        .await
        .expect("list files with one");
    drop(conn);

    assert_eq!(out.items.len(), 1);
    assert_eq!(out.total, 1);
    assert_eq!(out.items[0].id, file_id);
    assert_eq!(out.items[0].owner_kind, "ASSEMBLY");
    assert_eq!(out.items[0].kind, "ASSEMBLY_MASTER");
    assert_eq!(out.items[0].original_filename, "test.pdf");
    assert_eq!(out.items[0].owner_id, asm_id);
}

/// 3. 同时存在 ASSEMBLY_MASTER 与非 ASSEMBLY_MASTER 文件 → list 仅返回 ASSEMBLY_MASTER。
#[tokio::test]
async fn files_list_filters_by_kind() {
    let (pool, _fx) = setup().await;
    let l1 = insert_l1_customer(&pool, "客户FL-3", "F").await;
    let l2 = insert_l2_customer(&pool, "子客FL-3", l1).await;
    let current = test_current_user();
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);

    let req = AssemblyCreateRequest {
        drawing_no: "FL-D003".into(),
        name: "ASM-FL-3".into(),
        applicant_name: None,
        customer_id: l2.to_string(),
        request_date: None,
        planned_delivery_date: None,
        is_urgent: Some(false),
        quantity: Some(1),
        unit_price: None,
        total_price: None,
        order_no: None,
        system_delivery_date: None,
        note: None,
        children: vec![],
    };
    let mut tx = pool.begin().await.unwrap();
    let asm = AssemblyService::create_assembly(&mut tx, &snowflake, &req, vec![], &current)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let asm_id = asm.assembly.id;

    // 写 1 条 ASSEMBLY_MASTER（注意 `uk_t_part_file_single` 唯一约束：同 (part_id, kind)
    // 只能 1 条活跃行）+ 1 条 DRAWING（同 owner_id 但不同 kind）。
    // 用 ASSEMBLY 域唯一允许的多 kind 场景：ASSEMBLY_MASTER + DRAWING（同 owner 不同 kind）。
    insert_part_file_row(
        &pool,
        asm_id,
        "ASSEMBLY",
        "ASSEMBLY_MASTER",
        "asm-master.pdf",
    )
    .await;
    insert_part_file_row(
        &pool,
        asm_id,
        "ASSEMBLY",
        "DRAWING",
        "should-be-filtered.pdf",
    )
    .await;

    let mut conn = pool.acquire().await.unwrap();
    let out = AssemblyService::list_assembly_files(&mut conn, asm_id, &current)
        .await
        .expect("list files with kinds");
    drop(conn);

    assert_eq!(
        out.items.len(),
        1,
        "list 应仅返回 ASSEMBLY_MASTER kind；DRAWING 应被过滤"
    );
    for item in &out.items {
        assert_eq!(item.kind, "ASSEMBLY_MASTER");
    }
}

/// 4. 不存在的 assembly_id → 20301 BIZ_ASSEMBLY_NOT_FOUND。
#[tokio::test]
async fn files_list_assembly_not_found() {
    let (pool, _fx) = setup().await;
    let current = test_current_user();

    let mut conn = pool.acquire().await.unwrap();
    let err = AssemblyService::list_assembly_files(&mut conn, 9_999_999_999, &current)
        .await
        .expect_err("不存在的 assembly_id 应抛错");
    drop(conn);

    match err {
        AppError::Biz { code, .. } => {
            assert_eq!(
                code, 20301,
                "BIZ_ASSEMBLY_NOT_FOUND（assembly 不存在）；got {code}"
            );
        }
        other => panic!("期望 AppError::Biz(20301)，got {other:?}"),
    }
}

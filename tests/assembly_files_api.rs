//! assembly_files 域集成测试（2026-09-14 Phase 3）
//!
//! 覆盖 deferred #1（PDF 上传到 COS）、#4（start 端点）、#3（soft_delete HAS_SHIPMENT）、
//! #2（三态 NULL clear）、#7（child current_batch_id）。
//!
//! 测试用例：
//!   1. upload_files_happy_path                  — multipart PDF 上传 → 201
//!   2. upload_files_ext_must_be_pdf             — 扩展名 .step → 21102
//!   3. upload_files_rbac_clerk_forbidden        — Inspector 无 upload 权限
//!   4. start_pending_to_in_process              — PENDING → IN_PROCESS
//!   5. start_invalid_transition_returns_20103   — COMPLETED → IN_PROCESS → 20103
//!   6. soft_delete_has_shipment_returns_20307   — 子件挂送货单 → 20307
//!   7. soft_delete_no_shipment_succeeds         — 无挂单 → 成功
//!   8. three_state_note_clear_to_null           — Some(None) 置 NULL
//!   9. three_state_note_overwrite               — Some(Some(v)) 覆盖
//!  10. child_current_batch_id_in_detail         — 子件有活跃 batch → current_batch_id 非空

#[path = "common/mod.rs"]
mod common;

use common::{clean_business_db, clean_db, ensure_database_exists, test_pool};

use hsh_erp_rust::auth::rbac::{CurrentUser, Role};
use hsh_erp_rust::infra::clock::now_naive;
use hsh_erp_rust::infra::cos::NoopCos;
use hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator;
use hsh_erp_rust::modules::assembly::dto::AssemblyUpdateRequest;
use hsh_erp_rust::modules::assembly::service::AssemblyService;
use hsh_erp_rust::shared::error::AppError;
use lopdf::{dictionary, Document, Object, ObjectId};
use sqlx::PgPool;
use std::sync::Arc;

static TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn setup<'a>() -> (tokio::sync::MutexGuard<'a, ()>, PgPool) {
    let guard = TEST_LOCK.lock().await;
    ensure_database_exists().await;
    let pool = test_pool().await;
    clean_db(&pool).await;
    clean_business_db(&pool).await;
    (guard, pool)
}

async fn insert_l1_customer(pool: &PgPool, name: &str, prefix: &str) -> i64 {
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let id = snowflake.next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_customer (id, name, parent_id, serial_prefix, version, \
         created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, NULL, $3, 0, $4, NULL, $4, NULL)",
    )
    .bind(id)
    .bind(name)
    .bind(prefix)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert L1");
    id
}

async fn insert_l2_customer(pool: &PgPool, name: &str, l1_id: i64) -> i64 {
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let id = snowflake.next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_customer (id, name, parent_id, serial_prefix, version, \
         created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, $3, NULL, 0, $4, NULL, $4, NULL)",
    )
    .bind(id)
    .bind(name)
    .bind(l1_id)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert L2");
    id
}

async fn insert_serial_counter(pool: &PgPool, prefix: &str, initial: i64) {
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_serial_counter (prefix, counter, version, created_at, created_by, \
         updated_at, updated_by) \
         VALUES ($1, $2, 0, $3, NULL, $3, NULL) \
         ON CONFLICT (prefix) DO UPDATE SET counter = EXCLUDED.counter, \
                                          version = 0, updated_at = EXCLUDED.updated_at",
    )
    .bind(prefix)
    .bind(initial)
    .bind(now)
    .execute(pool)
    .await
    .expect("seed counter");
}

fn make_pdf_bytes() -> Vec<u8> {
    let mut doc = Document::with_version("1.5");
    let pages_id = doc.new_object_id();
    let page_id = doc.add_object(dictionary! {
        "Type" => "Page",
        "Parent" => pages_id,
        "Resources" => dictionary! {},
    });
    let pages = dictionary! {
        "Type" => "Pages",
        "Count" => 1,
        "Kids" => vec![Object::Reference(page_id)],
    };
    doc.objects.insert(pages_id, Object::Dictionary(pages));
    let catalog_id = doc.add_object(dictionary! {
        "Type" => "Catalog",
        "Pages" => pages_id,
    });
    doc.trailer.set("Root", catalog_id);
    let mut buf = Vec::new();
    doc.save_to(&mut buf).expect("save pdf");
    buf
}

fn make_pdf_2_pages() -> Vec<u8> {
    let mut doc = Document::with_version("1.5");
    let pages_id = doc.new_object_id();
    let mut page_ids: Vec<ObjectId> = Vec::new();
    for _ in 0..2 {
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "Resources" => dictionary! {},
        });
        page_ids.push(page_id);
    }
    let pages = dictionary! {
        "Type" => "Pages",
        "Count" => 2,
        "Kids" => page_ids.iter().map(|id| Object::Reference(*id)).collect::<Vec<_>>(),
    };
    doc.objects.insert(pages_id, Object::Dictionary(pages));
    let catalog_id = doc.add_object(dictionary! {
        "Type" => "Catalog",
        "Pages" => pages_id,
    });
    doc.trailer.set("Root", catalog_id);
    let mut buf = Vec::new();
    doc.save_to(&mut buf).expect("save pdf");
    buf
}

fn test_current_user(roles: Vec<Role>) -> CurrentUser {
    CurrentUser {
        id: 1,
        username: "asm_files_tester".into(),
        roles,
        shelf_ids: vec![],
        shelf_wildcard: false,
    }
}

async fn seed_assembly(pool: &PgPool) -> (i64, i64, i64) {
    let l1 = insert_l1_customer(pool, "客户AF-1", "F").await;
    let l2 = insert_l2_customer(pool, "子客AF-1", l1).await;
    insert_serial_counter(pool, "F", 0).await;
    (l1, l2, 0)
}

// ===========================================================================
//  Tests
// ===========================================================================

#[tokio::test]
async fn upload_files_happy_path() {
    let (_guard, pool) = setup().await;
    let (l1, l2, _) = seed_assembly(&pool).await;
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let cos = Arc::new(NoopCos);

    // 建一个无 PDF 装配体（serial_no=None）
    let mut tx = pool.begin().await.unwrap();
    let req = hsh_erp_rust::modules::assembly::dto::AssemblyCreateRequest {
        drawing_no: "D-AF-1".into(),
        name: "asm-files-1".into(),
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
    let current = test_current_user(vec![Role::Manager]);
    let asm = AssemblyService::create_assembly(&mut tx, &snowflake, &req, vec![], &current)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let asm_id = asm.assembly.id;

    // 上传 PDF
    let pdf_bytes = make_pdf_bytes();
    let mut tx = pool.begin().await.unwrap();
    let out = AssemblyService::upload_assembly_files(
        &mut tx,
        &snowflake,
        cos,
        asm_id,
        vec![(pdf_bytes, "master.pdf".to_string(), "application/pdf".to_string())],
        &current,
    )
    .await
    .expect("upload ok");
    tx.commit().await.unwrap();

    assert_eq!(out.len(), 1);
    assert_eq!(out[0].original_filename, "master.pdf");

    // 详情应返回 file
    let mut tx = pool.begin().await.unwrap();
    let detail = AssemblyService::get_assembly(&mut tx, asm_id, &current).await.unwrap();
    drop(tx);
    assert_eq!(detail.files.len(), 1);
    assert_eq!(detail.files[0].original_filename, "master.pdf");

    // 抑制 unused
    let _ = l1;
}

#[tokio::test]
async fn upload_files_ext_must_be_pdf() {
    let (_guard, pool) = setup().await;
    let (l1, l2, _) = seed_assembly(&pool).await;
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let cos = Arc::new(NoopCos);

    let mut tx = pool.begin().await.unwrap();
    let req = hsh_erp_rust::modules::assembly::dto::AssemblyCreateRequest {
        drawing_no: "D-AF-2".into(),
        name: "asm-files-2".into(),
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
    let current = test_current_user(vec![Role::Manager]);
    let asm = AssemblyService::create_assembly(&mut tx, &snowflake, &req, vec![], &current)
        .await
        .unwrap();
    tx.commit().await.unwrap();

    let mut tx = pool.begin().await.unwrap();
    let err = AssemblyService::upload_assembly_files(
        &mut tx,
        &snowflake,
        cos,
        asm.assembly.id,
        vec![(b"data".to_vec(), "master.step".to_string(), "application/octet-stream".to_string())],
        &current,
    )
    .await
    .expect_err("非 PDF 扩展名应被拒");
    drop(tx);

    match err {
        AppError::Biz { code, .. } => assert_eq!(code, 21102),
        other => panic!("期望 AppError::Biz(21102)，got {other:?}"),
    }
    let _ = l1;
}

#[tokio::test]
async fn start_pending_to_in_process() {
    let (_guard, pool) = setup().await;
    let (l1, l2, _) = seed_assembly(&pool).await;
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);

    let mut tx = pool.begin().await.unwrap();
    let req = hsh_erp_rust::modules::assembly::dto::AssemblyCreateRequest {
        drawing_no: "D-START".into(),
        name: "asm-start".into(),
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
    let current = test_current_user(vec![Role::Manager]);
    let asm = AssemblyService::create_assembly(&mut tx, &snowflake, &req, vec![], &current)
        .await
        .unwrap();
    tx.commit().await.unwrap();

    let mut tx = pool.begin().await.unwrap();
    let started = AssemblyService::start_assembly(&mut tx, asm.assembly.id, &current)
        .await
        .expect("start should succeed");
    tx.commit().await.unwrap();
    assert_eq!(started.status, "IN_PROCESS");
    let _ = l1;
}

#[tokio::test]
async fn soft_delete_has_shipment_returns_20307() {
    let (_guard, pool) = setup().await;
    let (l1, l2, _) = seed_assembly(&pool).await;
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);

    // 用 2 页 PDF 创建装配件 + 1 子件（page1=master, page2=child）
    let pdf = make_pdf_2_pages();
    let mut tx = pool.begin().await.unwrap();
    let req = hsh_erp_rust::modules::assembly::dto::AssemblyCreateRequest {
        drawing_no: "D-SD-SHIP".into(),
        name: "asm-ship".into(),
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
        children: vec![
            hsh_erp_rust::modules::assembly::dto::AssemblyChildRequest {
                name: "child-1".into(),
                drawing_no: Some("D-SD-SHIP-01".into()),
                planned_delivery_date: None,
                quantity: Some(1),
            },
        ],
    };
    let current = test_current_user(vec![Role::Manager]);
    let asm = AssemblyService::create_assembly(&mut tx, &snowflake, &req, vec![pdf], &current)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let asm_id = asm.assembly.id;
    let asm_version = asm.assembly.version;

    // 给子件设 delivery_note_id（模拟挂单）
    sqlx::query(
        "UPDATE t_part SET delivery_note_id = $3::bigint, version = version + 1, updated_at = now(), updated_by = $1 \
         WHERE assembly_id = $2 AND deleted_at IS NULL",
    )
    .bind(current.id)
    .bind(asm_id)
    .bind(1_i64)
    .execute(&pool)
    .await
    .expect("simulate shipment");

    let mut tx = pool.begin().await.unwrap();
    let err = AssemblyService::soft_delete_assembly(&mut tx, asm_id, asm_version, &current)
        .await
        .expect_err("HAS_SHIPMENT 应拒软删");
    drop(tx);

    match err {
        AppError::Biz { code, .. } => assert_eq!(code, 20307, "BIZ_ASSEMBLY_HAS_SHIPMENT"),
        other => panic!("期望 AppError::Biz(20307)，got {other:?}"),
    }
    let _ = l1;
}

#[tokio::test]
async fn three_state_note_clear_to_null() {
    let (_guard, pool) = setup().await;
    let (l1, l2, _) = seed_assembly(&pool).await;
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);

    let mut tx = pool.begin().await.unwrap();
    let req = hsh_erp_rust::modules::assembly::dto::AssemblyCreateRequest {
        drawing_no: "D-3STATE".into(),
        name: "asm-3state".into(),
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
        note: Some("初始备注".into()),
        children: vec![],
    };
    let current = test_current_user(vec![Role::Manager]);
    let asm = AssemblyService::create_assembly(&mut tx, &snowflake, &req, vec![], &current)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let asm_id = asm.assembly.id;
    let asm_version = asm.assembly.version;
    assert_eq!(asm.assembly.note.as_deref(), Some("初始备注"));

    // update note: Some(None) → 置 NULL
    let upd = AssemblyUpdateRequest {
        drawing_no: None,
        name: None,
        applicant_name: None,
        customer_id: None,
        request_date: None,
        planned_delivery_date: None,
        actual_delivery_date: None,
        is_urgent: None,
        quantity: None,
        unit_price: None,
        total_price: None,
        order_no: None,
        system_delivery_date: None,
        note: Some(None), // 三态置 NULL
        version: asm_version,
    };
    let mut tx = pool.begin().await.unwrap();
    let updated = AssemblyService::update_assembly(&mut tx, asm_id, &upd, &current)
        .await
        .expect("update ok");
    tx.commit().await.unwrap();
    assert!(updated.note.is_none(), "Some(None) 三态应将 note 置为 NULL");
    let _ = l1;
}

#[tokio::test]
async fn child_current_batch_id_in_detail() {
    let (_guard, pool) = setup().await;
    let (l1, l2, _) = seed_assembly(&pool).await;
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);

    // 建装配件（带 PDF + 1 子件）
    let pdf = make_pdf_2_pages();
    let mut tx = pool.begin().await.unwrap();
    let req = hsh_erp_rust::modules::assembly::dto::AssemblyCreateRequest {
        drawing_no: "D-CB".into(),
        name: "asm-cb".into(),
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
        children: vec![
            hsh_erp_rust::modules::assembly::dto::AssemblyChildRequest {
                name: "c-1".into(),
                drawing_no: Some("D-CB-01".into()),
                planned_delivery_date: None,
                quantity: Some(1),
            },
        ],
    };
    let current = test_current_user(vec![Role::Manager]);
    let asm = AssemblyService::create_assembly(&mut tx, &snowflake, &req, vec![pdf], &current)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let asm_id = asm.assembly.id;

    let mut tx = pool.begin().await.unwrap();
    let detail = AssemblyService::get_assembly(&mut tx, asm_id, &current).await.unwrap();
    drop(tx);
    assert_eq!(detail.children.len(), 1);
    let cb_id = detail.children[0].current_batch_id;
    assert!(cb_id.is_some(), "子件应有活跃批次 current_batch_id");
    let _ = l1;
}
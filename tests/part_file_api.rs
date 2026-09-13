//! part_file 域集成测试（2026-09-14 Phase 3）
//!
//! 覆盖：
//!   1. upload_pdf_happy_path                  — Manager 上传 PDF → 201 / 含 sha
//!   2. upload_invalid_kind_returns_21102      — kind=G_CODE 但 content=pdf → 21102
//!   3. upload_cas_dedup_skips_cos             — 同 sha 二次上传 → CAS 命中
//!   4. upload_owner_not_found_returns_21105   — part_id 不存在 → 21105
//!   5. rbac_inspector_can_upload_returns_403  — Inspector 无 upload 权限
//!   6. list_filter_by_kind                    — 仅返回 DRAWING
//!   7. get_url_returns_presigned_url          — 详情 + 预签 URL
//!
//! ## 集成策略
//! 不启 axum（避免 JWT/Redis 开销），直接 service 直调；`pool.begin()` 开 tx →
//! 传 `&mut *tx` 给 service → 显式 `tx.commit()`。

#[path = "common/mod.rs"]
mod common;

use common::{clean_business_db, clean_db, ensure_database_exists, test_pool};

use hsh_erp_rust::auth::rbac::{CurrentUser, Role};
use hsh_erp_rust::infra::clock::now_naive;
use hsh_erp_rust::infra::cos::NoopCos;
use hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator;
use hsh_erp_rust::modules::part_file::service::PartFileService;
use hsh_erp_rust::shared::error::AppError;
use sqlx::PgPool;
use std::sync::Arc;

// ===========================================================================
//  全局串行化 + setup
// ===========================================================================

static TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn setup<'a>() -> (tokio::sync::MutexGuard<'a, ()>, PgPool) {
    let guard = TEST_LOCK.lock().await;
    ensure_database_exists().await;
    let pool = test_pool().await;
    clean_db(&pool).await;
    clean_business_db(&pool).await;
    (guard, pool)
}

// ===========================================================================
//  私有 fixture helpers
// ===========================================================================

fn test_current_user_with_roles(roles: Vec<Role>) -> CurrentUser {
    CurrentUser {
        id: 1,
        username: "tester".into(),
        roles,
        shelf_ids: vec![],
        shelf_wildcard: false,
    }
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
    .expect("insert L2 customer");
    id
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
    .expect("insert L1 customer");
    id
}

async fn insert_part_for_owner(pool: &PgPool, customer_id: i64) -> i64 {
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let id = snowflake.next_id();
    let now = now_naive();
    let today = now.date();
    sqlx::query(
        "INSERT INTO t_part (id, name, drawing_no, applicant_name, customer_id, \
         request_date, planned_delivery_date, status, version, \
         created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, 'DWG-PF', $3, $4, $5, $5, 'PENDING', 0, $6, NULL, $6, NULL)",
    )
    .bind(id)
    .bind(format!("part-{id}"))
    .bind("tester")
    .bind(customer_id)
    .bind(today)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_part");
    id
}

// ===========================================================================
//  Tests
// ===========================================================================

#[tokio::test]
async fn upload_pdf_happy_path() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1_customer(&pool, "客户PF-1", "F").await;
    let l2 = insert_l2_customer(&pool, "子客PF-1", l1).await;
    let part_id = insert_part_for_owner(&pool, l2).await;

    let current = test_current_user_with_roles(vec![Role::Manager]);
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let cos = Arc::new(NoopCos);

    let pdf_bytes = b"%PDF-1.5\nhello world\n%%EOF".to_vec();
    let mut tx = pool.begin().await.unwrap();
    let out = PartFileService::upload_file_for_owner(
        &mut tx,
        &snowflake,
        cos,
        "PART",
        part_id,
        "DRAWING",
        "drawing.pdf",
        "application/pdf",
        pdf_bytes,
        &current,
    )
    .await
    .expect("upload should succeed");
    tx.commit().await.unwrap();

    assert_eq!(out.kind, "DRAWING");
    assert_eq!(out.file_type, "PDF");
    assert!(out.content_sha256.is_some(), "sha256 必须已计算");
    assert_eq!(out.content_type, "application/pdf");
    assert_eq!(out.upload_status, "READY");
    assert!(out.object_key.contains("drawing.pdf"));
}

#[tokio::test]
async fn upload_invalid_kind_returns_21102() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1_customer(&pool, "客户PF-2", "F").await;
    let l2 = insert_l2_customer(&pool, "子客PF-2", l1).await;
    let part_id = insert_part_for_owner(&pool, l2).await;

    let current = test_current_user_with_roles(vec![Role::Manager]);
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let cos = Arc::new(NoopCos);

    let bytes = b"some step data".to_vec();
    let mut tx = pool.begin().await.unwrap();
    let err = PartFileService::upload_file_for_owner(
        &mut tx,
        &snowflake,
        cos,
        "PART",
        part_id,
        "DRAWING", // DRAWING 不接受 step
        "drawing.step",
        "application/octet-stream",
        bytes,
        &current,
    )
    .await
    .expect_err("扩展名 step 不在 DRAWING 白名单");
    drop(tx);

    match err {
        AppError::Biz { code, .. } => assert_eq!(code, 21102, "BIZ_PART_FILE_BAD_TYPE"),
        other => panic!("期望 AppError::Biz(21102)，got {other:?}"),
    }
}

#[tokio::test]
async fn upload_cas_dedup_skips_cos() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1_customer(&pool, "客户PF-3", "F").await;
    let l2 = insert_l2_customer(&pool, "子客PF-3", l1).await;
    let part_id = insert_part_for_owner(&pool, l2).await;

    let current = test_current_user_with_roles(vec![Role::Manager]);
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let cos = Arc::new(NoopCos);

    let pdf_bytes = b"%PDF-1.5\nidentical content\n%%EOF".to_vec();

    // 第一次上传
    let mut tx = pool.begin().await.unwrap();
    let out1 = PartFileService::upload_file_for_owner(
        &mut tx,
        &snowflake,
        cos.clone(),
        "PART",
        part_id,
        "DRAWING",
        "first.pdf",
        "application/pdf",
        pdf_bytes.clone(),
        &current,
    )
    .await
    .expect("first upload ok");
    tx.commit().await.unwrap();

    // 第二次上传同 sha → CAS 命中，复用 object_key，id 不同
    let mut tx = pool.begin().await.unwrap();
    let out2 = PartFileService::upload_file_for_owner(
        &mut tx,
        &snowflake,
        cos.clone(),
        "PART",
        part_id,
        "DRAWING",
        "second.pdf",
        "application/pdf",
        pdf_bytes.clone(),
        &current,
    )
    .await
    .expect("CAS upload ok");
    tx.commit().await.unwrap();

    // CAS 命中：返回已有 id，object_key 相同
    assert_eq!(out1.id, out2.id, "CAS 命中应返回已有 id");
    assert_eq!(out1.object_key, out2.object_key);
    assert_eq!(out1.content_sha256, out2.content_sha256);
}

#[tokio::test]
async fn upload_owner_not_found_returns_21105() {
    let (_guard, pool) = setup().await;
    let current = test_current_user_with_roles(vec![Role::Manager]);
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let cos = Arc::new(NoopCos);

    let nonexistent = 99_999_999_999_i64;
    let mut tx = pool.begin().await.unwrap();
    let err = PartFileService::upload_file_for_owner(
        &mut tx,
        &snowflake,
        cos,
        "PART",
        nonexistent,
        "DRAWING",
        "drawing.pdf",
        "application/pdf",
        b"%PDF-1.5\n".to_vec(),
        &current,
    )
    .await
    .expect_err("不存在的 owner");
    drop(tx);

    match err {
        AppError::Biz { code, .. } => assert_eq!(code, 21105, "BIZ_PART_FILE_OWNER_NOT_FOUND"),
        other => panic!("期望 AppError::Biz(21105)，got {other:?}"),
    }
}

#[tokio::test]
async fn rbac_inspector_can_upload_returns_403() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1_customer(&pool, "客户PF-RBAC", "F").await;
    let l2 = insert_l2_customer(&pool, "子客PF-RBAC", l1).await;
    let part_id = insert_part_for_owner(&pool, l2).await;

    let inspector = test_current_user_with_roles(vec![Role::Inspector]);
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let cos = Arc::new(NoopCos);

    let mut tx = pool.begin().await.unwrap();
    let err = PartFileService::upload_file_for_owner(
        &mut tx,
        &snowflake,
        cos,
        "PART",
        part_id,
        "DRAWING",
        "drawing.pdf",
        "application/pdf",
        b"%PDF-1.5\n".to_vec(),
        &inspector,
    )
    .await
    .expect_err("Inspector 无 upload 权限");
    drop(tx);

    match err {
        AppError::Biz { code, .. } => assert_eq!(code, 40300, "FORBIDDEN"),
        other => panic!("期望 AppError::Biz(40300)，got {other:?}"),
    }
}

#[tokio::test]
async fn list_filter_by_kind() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1_customer(&pool, "客户PF-List", "F").await;
    let l2 = insert_l2_customer(&pool, "子客PF-List", l1).await;
    let part_id = insert_part_for_owner(&pool, l2).await;

    let current = test_current_user_with_roles(vec![Role::Manager]);
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let cos = Arc::new(NoopCos);

    // 上传 1 个 DRAWING + 1 个 3D_MODEL（每 kind 在 uk_t_part_file_single 下只能 1 个/owner）
    for (i, (filename, kind, content_type)) in [
        ("a.pdf", "DRAWING", "application/pdf"),
        ("b.step", "3D_MODEL", "application/step"),
    ]
    .into_iter()
    .enumerate()
    {
        let unique_prefix = format!("UNIQUE-PREFIX-{}-{}", i, chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0));
        let body = format!("{}{}", unique_prefix, "x".repeat(1024)).into_bytes();
        let mut tx = pool.begin().await.unwrap();
        PartFileService::upload_file_for_owner(
            &mut tx,
            &snowflake,
            cos.clone(),
            "PART",
            part_id,
            kind,
            filename,
            content_type,
            body,
            &current,
        )
        .await
        .expect("seed upload");
        tx.commit().await.unwrap();
    }

    // list 仅 DRAWING（只上传了 1 个 DRAWING）
    let query = hsh_erp_rust::modules::part_file::dto::PartFileListQuery {
        owner_kind: Some("PART".into()),
        owner_id: Some(part_id.to_string()),
        kind: Some("DRAWING".into()),
        limit: Some(50),
        offset: Some(0),
    };
    let mut tx = pool.begin().await.unwrap();
    let out = PartFileService::list_files(&mut tx, &query, &current).await.unwrap();
    drop(tx);
    assert_eq!(out.items.len(), 1, "list_filter_by_kind 应仅返 DRAWING");
    assert!(out.items.iter().all(|i| i.kind == "DRAWING"));
}

#[tokio::test]
async fn get_url_returns_presigned_url() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1_customer(&pool, "客户PF-URL", "F").await;
    let l2 = insert_l2_customer(&pool, "子客PF-URL", l1).await;
    let part_id = insert_part_for_owner(&pool, l2).await;

    let current = test_current_user_with_roles(vec![Role::Manager]);
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let cos = Arc::new(NoopCos);

    let mut tx = pool.begin().await.unwrap();
    let out = PartFileService::upload_file_for_owner(
        &mut tx,
        &snowflake,
        cos.clone(),
        "PART",
        part_id,
        "DRAWING",
        "test.pdf",
        "application/pdf",
        b"%PDF-1.5\n".to_vec(),
        &current,
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let mut tx = pool.begin().await.unwrap();
    let detail = PartFileService::get_file_with_url(&mut tx, cos, out.id, &current)
        .await
        .unwrap();
    drop(tx);
    assert_eq!(detail.id, out.id.to_string());
    assert_eq!(detail.kind, "DRAWING");
    assert!(detail.download_url.contains(&out.object_key));
    assert_eq!(detail.url_expires_in_seconds, 3600);
}
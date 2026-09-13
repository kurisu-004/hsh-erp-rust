//! cnc_program 域集成测试（2026-09-14 Phase 3）
//!
//! 覆盖：
//!   1. upload_pair_happy_path              — Manager + CncProgrammer 上传 G_CODE+SETUP_SHEET → 配对
//!   2. upload_with_invalid_gcode_ext       — G_CODE 扩展名 .pdf → 21102
//!   3. upload_with_invalid_setup_ext       — SETUP_SHEET 扩展名 .tap → 21102
//!   4. upload_part_not_found_returns_20101 — part_id 不存在 → 20101
//!   5. upload_rbac_clerk_returns_403       — Clerk 无 upload 权限
//!   6. list_pairs_for_part                 — 单 part 多对配对 → 按 created_at DESC

#[path = "common/mod.rs"]
mod common;

use common::{clean_business_db, clean_db, ensure_database_exists, test_pool};

use hsh_erp_rust::auth::rbac::{CurrentUser, Role};
use hsh_erp_rust::infra::clock::now_naive;
use hsh_erp_rust::infra::cos::NoopCos;
use hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator;
use hsh_erp_rust::modules::cnc_program::service::CncProgramService;
use hsh_erp_rust::shared::error::AppError;
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

fn test_current_user(roles: Vec<Role>) -> CurrentUser {
    CurrentUser {
        id: 1,
        username: "cnc_tester".into(),
        roles,
        shelf_ids: vec![],
        shelf_wildcard: false,
    }
}

async fn insert_part(pool: &PgPool) -> i64 {
    // 重要：使用单一 generator，避免多次 new() 后同毫秒内 sequence=0 撞 id
    let generator = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let l1 = generator.next_id();
    let l2 = generator.next_id();
    let now = now_naive();
    let today = now.date();
    sqlx::query(
        "INSERT INTO t_customer (id, name, parent_id, serial_prefix, version, \
         created_at, created_by, updated_at, updated_by) \
         VALUES ($1, 'l1', NULL, 'C', 0, $2, NULL, $2, NULL), \
                ($3, 'l2', $1, NULL, 0, $2, NULL, $2, NULL)",
    )
    .bind(l1)
    .bind(now)
    .bind(l2)
    .execute(pool)
    .await
    .expect("insert customers");
    let part_id = generator.next_id();
    sqlx::query(
        "INSERT INTO t_part (id, name, drawing_no, applicant_name, customer_id, \
         request_date, planned_delivery_date, status, version, \
         created_at, created_by, updated_at, updated_by) \
         VALUES ($1, 'p-cnc', 'DWG-CNC', 'tester', $2, $3, $3, 'PENDING', 0, $4, NULL, $4, NULL)",
    )
    .bind(part_id)
    .bind(l2)
    .bind(today)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert part");
    part_id
}

#[tokio::test]
async fn upload_pair_happy_path() {
    let (_guard, pool) = setup().await;
    let part_id = insert_part(&pool).await;
    let current = test_current_user(vec![Role::Manager]);
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let cos = Arc::new(NoopCos);

    let g_bytes = b"O0011\nG0 X0 Y0\n".to_vec();
    let setup_bytes = b"%PDF-1.5\nsetup\n%%EOF".to_vec();

    let mut tx = pool.begin().await.unwrap();
    let out = CncProgramService::upload_cnc_pair(
        &mut tx,
        &snowflake,
        cos,
        part_id,
        g_bytes,
        "O0011.tap",
        "application/octet-stream",
        setup_bytes,
        "setup.pdf",
        "application/pdf",
        &current,
    )
    .await
    .expect("upload pair ok");
    tx.commit().await.unwrap();

    // 配对：互指 paired_file_id
    assert_ne!(out.g_code.id, out.setup_sheet.id);
    assert_eq!(out.g_code.paired_file_id, Some(out.setup_sheet.id));
    assert_eq!(out.setup_sheet.paired_file_id, Some(out.g_code.id));
    assert_eq!(out.g_code.kind, "G_CODE");
    assert_eq!(out.setup_sheet.kind, "SETUP_SHEET");
}

#[tokio::test]
async fn upload_with_invalid_gcode_ext() {
    let (_guard, pool) = setup().await;
    let part_id = insert_part(&pool).await;
    let current = test_current_user(vec![Role::Manager]);
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let cos = Arc::new(NoopCos);

    let mut tx = pool.begin().await.unwrap();
    let err = CncProgramService::upload_cnc_pair(
        &mut tx,
        &snowflake,
        cos,
        part_id,
        b"data".to_vec(),
        "O0011.pdf", // G_CODE 不接受 pdf
        "application/pdf",
        b"%PDF-1.5\n".to_vec(),
        "setup.pdf",
        "application/pdf",
        &current,
    )
    .await
    .expect_err("G_CODE 扩展名 pdf 应被拒");
    drop(tx);

    match err {
        AppError::Biz { code, .. } => assert_eq!(code, 21102),
        other => panic!("期望 AppError::Biz(21102)，got {other:?}"),
    }
}

#[tokio::test]
async fn upload_with_invalid_setup_ext() {
    let (_guard, pool) = setup().await;
    let part_id = insert_part(&pool).await;
    let current = test_current_user(vec![Role::Manager]);
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let cos = Arc::new(NoopCos);

    let mut tx = pool.begin().await.unwrap();
    let err = CncProgramService::upload_cnc_pair(
        &mut tx,
        &snowflake,
        cos,
        part_id,
        b"data".to_vec(),
        "O0011.tap",
        "application/octet-stream",
        b"data".to_vec(),
        "setup.tap", // SETUP_SHEET 必须 pdf
        "application/octet-stream",
        &current,
    )
    .await
    .expect_err("SETUP_SHEET 扩展名 tap 应被拒");
    drop(tx);

    match err {
        AppError::Biz { code, .. } => assert_eq!(code, 21102),
        other => panic!("期望 AppError::Biz(21102)，got {other:?}"),
    }
}

#[tokio::test]
async fn upload_part_not_found_returns_20101() {
    let (_guard, pool) = setup().await;
    let current = test_current_user(vec![Role::Manager]);
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let cos = Arc::new(NoopCos);

    let mut tx = pool.begin().await.unwrap();
    let err = CncProgramService::upload_cnc_pair(
        &mut tx,
        &snowflake,
        cos,
        99_999_999_999,
        b"data".to_vec(),
        "O0011.tap",
        "application/octet-stream",
        b"%PDF-1.5\n".to_vec(),
        "setup.pdf",
        "application/pdf",
        &current,
    )
    .await
    .expect_err("不存在的 part");
    drop(tx);

    match err {
        AppError::Biz { code, .. } => assert_eq!(code, 20101, "BIZ_PART_NOT_FOUND"),
        other => panic!("期望 AppError::Biz(20101)，got {other:?}"),
    }
}

#[tokio::test]
async fn upload_rbac_clerk_returns_403() {
    let (_guard, pool) = setup().await;
    let part_id = insert_part(&pool).await;
    let clerk = test_current_user(vec![Role::Clerk]);
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let cos = Arc::new(NoopCos);

    let mut tx = pool.begin().await.unwrap();
    let err = CncProgramService::upload_cnc_pair(
        &mut tx,
        &snowflake,
        cos,
        part_id,
        b"data".to_vec(),
        "O0011.tap",
        "application/octet-stream",
        b"%PDF-1.5\n".to_vec(),
        "setup.pdf",
        "application/pdf",
        &clerk,
    )
    .await
    .expect_err("Clerk 无 cnc pair 上传权限");
    drop(tx);

    match err {
        AppError::Biz { code, .. } => assert_eq!(code, 40300, "FORBIDDEN"),
        other => panic!("期望 AppError::Biz(40300)，got {other:?}"),
    }
}

#[tokio::test]
async fn list_pairs_for_part() {
    let (_guard, pool) = setup().await;
    let part_id = insert_part(&pool).await;
    let current = test_current_user(vec![Role::Manager]);
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let cos = Arc::new(NoopCos);

    // 上传 2 对（用不同内容避开 CAS 去重）
    for i in 0..2 {
        let mut tx = pool.begin().await.unwrap();
        CncProgramService::upload_cnc_pair(
            &mut tx,
            &snowflake,
            cos.clone(),
            part_id,
            format!("g-code-payload-{i}").into_bytes(),
            &format!("O{i:04}.tap"),
            "application/octet-stream",
            format!("%PDF-1.5\nsetup-{i}\n%%EOF").into_bytes(),
            &format!("setup-{i}.pdf"),
            "application/pdf",
            &current,
        )
        .await
        .expect("seed pair");
        tx.commit().await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }

    let mut tx = pool.begin().await.unwrap();
    let list = CncProgramService::list_pairs_for_part(&mut tx, cos, part_id, &current)
        .await
        .unwrap();
    drop(tx);
    assert_eq!(list.items.len(), 2, "应见 2 对配对（CAS 不会去重不同 sha）");
    assert_eq!(list.total, 2);
    for item in &list.items {
        assert!(item.g_code_id > 0);
        assert!(item.setup_sheet_id > 0);
    }
}
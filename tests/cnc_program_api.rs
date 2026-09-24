//! cnc_program 域集成测试（2026-09-14 Phase 3）
//!
//! 覆盖：
//!   1. upload_pair_happy_path              — Manager + CncProgrammer 上传 G_CODE+SETUP_SHEET → 配对
//!   2. upload_with_invalid_gcode_ext       — G_CODE 扩展名 .pdf → 21102
//!   3. upload_with_invalid_setup_ext       — SETUP_SHEET 扩展名 .tap → 21102
//!   4. upload_part_not_found_returns_20101 — part_id 不存在 → 20101
//!   5. upload_rbac_clerk_returns_403       — Clerk 无 upload 权限
//!   6. list_pairs_for_part                 — 单 part 多对配对 → 按 created_at DESC
//!
//! ## Fixture 范本化（2026-09-24 PR13 Phase I）
//! 本文件原 `#[path = "common/mod.rs"] mod common;` + `use common::{...};` 改走
//! `use hsh_erp_test_support::*` + `load_cnc_program_fixture(&pool)` +
//! `CncProgramFixture`。fixture 提供 baseline L1+L2 customer + 1 PENDING part；
//! cnc_program 是 service 层单测（不走 HTTP），用 `test_current_user` 构造
//! `CurrentUser`，不依赖 DB user 行。字面断言逐字保留。

use hsh_erp_rust::auth::rbac::{CurrentUser, Role};
use hsh_erp_rust::infra::cos::NoopCos;
use hsh_erp_rust::modules::cnc_program::service::CncProgramService;
use hsh_erp_rust::modules::part_file::service::PartFileService;
use hsh_erp_rust::shared::error::AppError;
use hsh_erp_test_support::{
    CncProgramFixture, load_cnc_program_fixture, test_pool,
};
use sqlx::PgPool;
use std::sync::Arc;


async fn setup() -> PgPool {
    let pool = test_pool().await;
    let _fx = load_cnc_program_fixture(&pool).await;
    pool
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

#[tokio::test]
async fn upload_pair_happy_path() {
    let pool = setup().await;
    let part_id = CncProgramFixture::PART_ID;
    let current = test_current_user(vec![Role::Manager]);
    let snowflake = Arc::new(hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator::new(1_577_836_800_000, 1));
    let cos = Arc::new(NoopCos);

    let g_bytes = b"O0011\nG0 X0 Y0\n".to_vec();
    let setup_bytes = b"%PDF-1.5\nsetup\n%%EOF".to_vec();

    let mut tx = pool.begin().await.unwrap();
    let out = CncProgramService::new(snowflake.clone(), cos.clone()).upload_cnc_pair(
        &mut *tx,
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
    let pool = setup().await;
    let part_id = CncProgramFixture::PART_ID;
    let current = test_current_user(vec![Role::Manager]);
    let snowflake = Arc::new(hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator::new(1_577_836_800_000, 1));
    let cos = Arc::new(NoopCos);

    let mut tx = pool.begin().await.unwrap();
    let err = CncProgramService::new(snowflake.clone(), cos.clone()).upload_cnc_pair(
        &mut *tx,
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
    let pool = setup().await;
    let part_id = CncProgramFixture::PART_ID;
    let current = test_current_user(vec![Role::Manager]);
    let snowflake = Arc::new(hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator::new(1_577_836_800_000, 1));
    let cos = Arc::new(NoopCos);

    let mut tx = pool.begin().await.unwrap();
    let err = CncProgramService::new(snowflake.clone(), cos.clone()).upload_cnc_pair(
        &mut *tx,
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
    let pool = setup().await;
    let current = test_current_user(vec![Role::Manager]);
    let snowflake = Arc::new(hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator::new(1_577_836_800_000, 1));
    let cos = Arc::new(NoopCos);

    let mut tx = pool.begin().await.unwrap();
    let err = CncProgramService::new(snowflake.clone(), cos.clone()).upload_cnc_pair(
        &mut *tx,
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
    let pool = setup().await;
    let part_id = CncProgramFixture::PART_ID;
    let clerk = test_current_user(vec![Role::Clerk]);
    let snowflake = Arc::new(hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator::new(1_577_836_800_000, 1));
    let cos = Arc::new(NoopCos);

    let mut tx = pool.begin().await.unwrap();
    let err = CncProgramService::new(snowflake.clone(), cos.clone()).upload_cnc_pair(
        &mut *tx,
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
    let pool = setup().await;
    let part_id = CncProgramFixture::PART_ID;
    let current = test_current_user(vec![Role::Manager]);
    let snowflake = Arc::new(hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator::new(1_577_836_800_000, 1));
    let cos = Arc::new(NoopCos);

    // 上传 2 对（用不同内容避开 CAS 去重）
    for i in 0..2 {
        let mut tx = pool.begin().await.unwrap();
        CncProgramService::new(snowflake.clone(), cos.clone()).upload_cnc_pair(
            &mut *tx,
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
    let list = CncProgramService::new(snowflake.clone(), cos.clone()).list_pairs_for_part(&mut *tx, part_id, &current)
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

// ===== 2026-09-15 takeover-fill：alias 端点测试 =====

#[tokio::test]
async fn alias_download_url_returns_part_file_with_url() {
    let pool = setup().await;
    let part_id = CncProgramFixture::PART_ID;
    let current = test_current_user(vec![Role::Manager]);
    let snowflake = Arc::new(hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator::new(1_577_836_800_000, 1));
    let cos = Arc::new(NoopCos);

    let mut tx = pool.begin().await.unwrap();
    let out = CncProgramService::new(snowflake.clone(), cos.clone()).upload_cnc_pair(
        &mut *tx,
        part_id,
        b"O0011\n".to_vec(),
        "O0011.tap",
        "application/octet-stream",
        b"%PDF-1.5\nsetup\n%%EOF".to_vec(),
        "setup.pdf",
        "application/pdf",
        &current,
    )
    .await
    .expect("upload pair ok");
    tx.commit().await.unwrap();

    let g_id = out.g_code.id;
    let s_id = out.setup_sheet.id;

    // /download-url alias 应等于 part-files/{id}/url
    let mut tx = pool.begin().await.unwrap();
    let dl_url = PartFileService::new(snowflake.clone(), cos.clone()).get_file_with_url(&mut *tx, cos.clone(), g_id, &current)
        .await
        .expect("alias download-url ok");
    tx.commit().await.unwrap();
    assert_eq!(dl_url.id, g_id.to_string());
    assert_eq!(dl_url.kind, "G_CODE");

    let mut tx = pool.begin().await.unwrap();
    let dl_url_s = PartFileService::new(snowflake.clone(), cos.clone()).get_file_with_url(&mut *tx, cos.clone(), s_id, &current)
        .await
        .expect("alias download-url setup ok");
    tx.commit().await.unwrap();
    assert_eq!(dl_url_s.id, s_id.to_string());
    assert_eq!(dl_url_s.kind, "SETUP_SHEET");
}

#[tokio::test]
async fn alias_content_returns_bytes() {
    let pool = setup().await;
    let part_id = CncProgramFixture::PART_ID;
    let current = test_current_user(vec![Role::Manager]);
    let snowflake = Arc::new(hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator::new(1_577_836_800_000, 1));
    let cos = Arc::new(NoopCos);

    let mut tx = pool.begin().await.unwrap();
    let out = CncProgramService::new(snowflake.clone(), cos.clone()).upload_cnc_pair(
        &mut *tx,
        part_id,
        b"G0 X0\n".to_vec(),
        "O0001.tap",
        "application/octet-stream",
        b"%PDF-1.5\nsetup\n%%EOF".to_vec(),
        "setup.pdf",
        "application/pdf",
        &current,
    )
    .await
    .expect("upload pair ok");
    tx.commit().await.unwrap();

    // alias content
    let mut tx = pool.begin().await.unwrap();
    let content = PartFileService::new(snowflake.clone(), cos.clone()).get_file_content(&mut *tx, cos.clone(), out.g_code.id, &current)
        .await
        .expect("alias content ok");
    drop(tx);
    // NoopCos.get_object 返空
    assert!(content.bytes.is_empty());
    assert!(content.content_type.is_some());
}

#[tokio::test]
async fn alias_delete_soft_deletes_file() {
    let pool = setup().await;
    let part_id = CncProgramFixture::PART_ID;
    let current = test_current_user(vec![Role::Manager]);
    let snowflake = Arc::new(hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator::new(1_577_836_800_000, 1));
    let cos = Arc::new(NoopCos);

    let mut tx = pool.begin().await.unwrap();
    let out = CncProgramService::new(snowflake.clone(), cos.clone()).upload_cnc_pair(
        &mut *tx,
        part_id,
        b"G0 X0\n".to_vec(),
        "O0001.tap",
        "application/octet-stream",
        b"%PDF-1.5\nsetup\n%%EOF".to_vec(),
        "setup.pdf",
        "application/pdf",
        &current,
    )
    .await
    .expect("upload pair ok");
    tx.commit().await.unwrap();
    let version = sqlx::query_scalar::<_, i32>("SELECT version FROM t_part_file WHERE id = $1")
        .bind(out.g_code.id)
        .fetch_one(&pool)
        .await
        .expect("get version");

    let mut tx = pool.begin().await.unwrap();
    PartFileService::new(snowflake.clone(), cos.clone()).soft_delete_file(&mut *tx, out.g_code.id, version, &current)
        .await
        .expect("alias delete ok");
    tx.commit().await.unwrap();

    // 查应 not found
    let mut tx = pool.begin().await.unwrap();
    let row = hsh_erp_rust::modules::part_file::repo::PartFileRepo::get_by_id(
        &mut *tx,
        out.g_code.id,
        false,
    )
    .await
    .unwrap();
    drop(tx);
    assert!(row.is_none(), "G_CODE 软删后应查不到");
}
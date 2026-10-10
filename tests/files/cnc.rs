//! `files::part_file::cnc` 子模块集成测试
//! （2026-09-14 Phase 3 建为 `tests/cnc_program_api.rs`；2026-10-13 随域收编进
//! files 域迁入本 binary）
//!
//! 覆盖（5 条，`upload_cnc_pair` 的全部分支）：
//!   1. upload_pair_happy_path              — Manager + CncProgrammer 上传 G_CODE+SETUP_SHEET → 配对
//!   2. upload_with_invalid_gcode_ext       — G_CODE 扩展名 .pdf → 21102
//!   3. upload_with_invalid_setup_ext       — SETUP_SHEET 扩展名 .tap → 21102
//!   4. upload_part_not_found_returns_20101 — part_id 不存在 → 20101
//!   5. upload_rbac_clerk_returns_403       — Clerk 无 upload 权限
//!
//! ## 2026-10-13 随端点下线删除的用例（4 条）
//! - `list_pairs_for_part` —— 对应 `GET /api/v2/cnc-programs/parts/{part_id}`，
//!   零前端调用方，service 方法 `CncProgramService::list_pairs_for_part` 一并删除。
//! - `alias_download_url_returns_part_file_with_url` / `alias_content_returns_bytes` /
//!   `alias_delete_soft_deletes_file` —— 对应 3 条 alias 端点
//!   （`GET /{id}/download-url` / `GET /{id}/content` / `POST /{id}/delete`），
//!   它们本来只是转调 `PartFileService` 同名方法的薄壳；对应的
//!   `GET /files/objects/{id}/url`、`GET /files/objects/{id}/content`、
//!   `POST /files/objects/{id}/delete` 三条端点仍存在，其行为由
//!   `tests/files/part_file.rs` 的 service 直调用例覆盖。
//!
//! ## Fixture 范本化（2026-09-24 PR13 Phase I）
//! 本文件原 `#[path = "common/mod.rs"] mod common;` + `use common::{...};` 改走
//! `use hsh_erp_test_support::*` + `load_cnc_program_fixture(&pool)` +
//! `CncProgramFixture`。fixture 提供 baseline L1+L2 customer + 1 PENDING part；
//! cnc 是 service 层单测（不走 HTTP），用 `test_current_user` 构造
//! `CurrentUser`，不依赖 DB user 行。字面断言逐字保留。
//!
//! ⚠️ `test-support/src/fixture/cnc_program.rs` 与 `fixtures/cnc_program.sql`
//! **保留不动** —— 本文件 5 条存活用例仍用 `CncProgramFixture::PART_ID`。

use hsh_erp_rust::auth::rbac::{CurrentUser, Role};
use hsh_erp_rust::infra::cos::NoopCos;
use hsh_erp_rust::modules::files::part_file::cnc::service::CncProgramService;
use hsh_erp_rust::shared::error::AppError;
use hsh_erp_test_support::{
    CncProgramFixture, load_cnc_program_fixture, shared_test_snowflake, test_pool,
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
    // 2026-10-09：ID 统一从全进程共享 generator 取 —— 原为就地
    // `Arc::new(SnowflakeIdGenerator::new(1_577_836_800_000, 1))`，两个 fresh generator
    // 同 instance 同毫秒各取 seq 0 会撞主键（23505）。实参直接给 `shared_test_snowflake()`
    // 克隆出的同一个 `Arc`，`src/` 侧 `Arc<SnowflakeIdGenerator>` 签名一行未改。
    let cos = Arc::new(NoopCos);

    let g_bytes = b"O0011\nG0 X0 Y0\n".to_vec();
    let setup_bytes = b"%PDF-1.5\nsetup\n%%EOF".to_vec();

    let mut tx = pool.begin().await.unwrap();
    let out = CncProgramService::new(shared_test_snowflake().clone(), cos.clone())
        .upload_cnc_pair(
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
    let cos = Arc::new(NoopCos);

    let mut tx = pool.begin().await.unwrap();
    let err = CncProgramService::new(shared_test_snowflake().clone(), cos.clone())
        .upload_cnc_pair(
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
    let cos = Arc::new(NoopCos);

    let mut tx = pool.begin().await.unwrap();
    let err = CncProgramService::new(shared_test_snowflake().clone(), cos.clone())
        .upload_cnc_pair(
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
    let cos = Arc::new(NoopCos);

    let mut tx = pool.begin().await.unwrap();
    let err = CncProgramService::new(shared_test_snowflake().clone(), cos.clone())
        .upload_cnc_pair(
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
    let cos = Arc::new(NoopCos);

    let mut tx = pool.begin().await.unwrap();
    let err = CncProgramService::new(shared_test_snowflake().clone(), cos.clone())
        .upload_cnc_pair(
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

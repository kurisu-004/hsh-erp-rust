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
//!   8. list_includes_paired_file_id           — 列表项投影 paired_file_id（2026-09-16 补）
//!
//! ## 集成策略
//! 不启 axum（避免 JWT/Redis 开销），直接 service 直调；`pool.begin()` 开 tx →
//! 传 `&mut *tx` 给 service → 显式 `tx.commit()`。

// 2026-09-23 PR13 Phase C：edition 2024 下 `use common::*;` 不自动 fallback 到 crate root，
// 故本文件自带 `mod common;`（与 main.rs 的同名 pub mod 不冲突）。
#[path = "../common/mod.rs"]
mod common;

use common::{MockCos, clean_business_db, clean_db, ensure_database_exists, test_pool};

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


async fn setup() -> PgPool {
    ensure_database_exists().await;
    let pool = test_pool().await;
    clean_db(&pool).await;
    clean_business_db(&pool).await;
    pool
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
    let snowflake = Arc::new(SnowflakeIdGenerator::new(1_577_836_800_000, 1));
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
    let snowflake = Arc::new(SnowflakeIdGenerator::new(1_577_836_800_000, 1));
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
    let snowflake = Arc::new(SnowflakeIdGenerator::new(1_577_836_800_000, 1));
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
    let pool = setup().await;
    let l1 = insert_l1_customer(&pool, "客户PF-1", "F").await;
    let l2 = insert_l2_customer(&pool, "子客PF-1", l1).await;
    let part_id = insert_part_for_owner(&pool, l2).await;

    let current = test_current_user_with_roles(vec![Role::Manager]);
    let snowflake = Arc::new(SnowflakeIdGenerator::new(1_577_836_800_000, 1));
    let cos = Arc::new(NoopCos);

    let pdf_bytes = b"%PDF-1.5\nhello world\n%%EOF".to_vec();
    let mut tx = pool.begin().await.unwrap();
    let out = PartFileService::new(snowflake.clone(), cos.clone()).upload_file_for_owner(
        &mut *tx,
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
    // 2026-09-16 补：新上传的 DRAWING 无配对，字段须存在且为 None
    assert!(out.paired_file_id.is_none(), "新上传的 DRAWING 无配对文件");
}

#[tokio::test]
async fn upload_invalid_kind_returns_21102() {
    let pool = setup().await;
    let l1 = insert_l1_customer(&pool, "客户PF-2", "F").await;
    let l2 = insert_l2_customer(&pool, "子客PF-2", l1).await;
    let part_id = insert_part_for_owner(&pool, l2).await;

    let current = test_current_user_with_roles(vec![Role::Manager]);
    let snowflake = Arc::new(SnowflakeIdGenerator::new(1_577_836_800_000, 1));
    let cos = Arc::new(NoopCos);

    let bytes = b"some step data".to_vec();
    let mut tx = pool.begin().await.unwrap();
    let err = PartFileService::new(snowflake.clone(), cos.clone()).upload_file_for_owner(
        &mut *tx,
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
    let pool = setup().await;
    let l1 = insert_l1_customer(&pool, "客户PF-3", "F").await;
    let l2 = insert_l2_customer(&pool, "子客PF-3", l1).await;
    let part_id = insert_part_for_owner(&pool, l2).await;

    let current = test_current_user_with_roles(vec![Role::Manager]);
    let snowflake = Arc::new(SnowflakeIdGenerator::new(1_577_836_800_000, 1));
    let cos = Arc::new(NoopCos);

    let pdf_bytes = b"%PDF-1.5\nidentical content\n%%EOF".to_vec();

    // 第一次上传
    let mut tx = pool.begin().await.unwrap();
    let out1 = PartFileService::new(snowflake.clone(), cos.clone()).upload_file_for_owner(
        &mut *tx,
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
    let out2 = PartFileService::new(snowflake.clone(), cos.clone()).upload_file_for_owner(
        &mut *tx,
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
    let pool = setup().await;
    let current = test_current_user_with_roles(vec![Role::Manager]);
    let snowflake = Arc::new(SnowflakeIdGenerator::new(1_577_836_800_000, 1));
    let cos = Arc::new(NoopCos);

    let nonexistent = 99_999_999_999_i64;
    let mut tx = pool.begin().await.unwrap();
    let err = PartFileService::new(snowflake.clone(), cos.clone()).upload_file_for_owner(
        &mut *tx,
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
    let pool = setup().await;
    let l1 = insert_l1_customer(&pool, "客户PF-RBAC", "F").await;
    let l2 = insert_l2_customer(&pool, "子客PF-RBAC", l1).await;
    let part_id = insert_part_for_owner(&pool, l2).await;

    let inspector = test_current_user_with_roles(vec![Role::Inspector]);
    let snowflake = Arc::new(SnowflakeIdGenerator::new(1_577_836_800_000, 1));
    let cos = Arc::new(NoopCos);

    let mut tx = pool.begin().await.unwrap();
    let err = PartFileService::new(snowflake.clone(), cos.clone()).upload_file_for_owner(
        &mut *tx,
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
    let pool = setup().await;
    let l1 = insert_l1_customer(&pool, "客户PF-List", "F").await;
    let l2 = insert_l2_customer(&pool, "子客PF-List", l1).await;
    let part_id = insert_part_for_owner(&pool, l2).await;

    let current = test_current_user_with_roles(vec![Role::Manager]);
    let snowflake = Arc::new(SnowflakeIdGenerator::new(1_577_836_800_000, 1));
    let cos = Arc::new(NoopCos);

    // 上传 1 个 DRAWING + 1 个 3D_MODEL（每 kind 在 uk_t_part_file_single 下只能 1 个/owner）
    for (i, (filename, kind, content_type)) in [
        ("a.pdf", "DRAWING", "application/pdf"),
        ("b.step", "3D_MODEL", "application/step"),
    ]
    .into_iter()
    .enumerate()
    {
        let unique_prefix = format!(
            "UNIQUE-PREFIX-{}-{}",
            i,
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        );
        let body = format!("{}{}", unique_prefix, "x".repeat(1024)).into_bytes();
        let mut tx = pool.begin().await.unwrap();
        PartFileService::new(snowflake.clone(), cos.clone()).upload_file_for_owner(
            &mut *tx,
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
    let out = PartFileService::new(snowflake.clone(), cos.clone()).list_files(&mut *tx, &query, &current)
        .await
        .unwrap();
    drop(tx);
    assert_eq!(out.items.len(), 1, "list_filter_by_kind 应仅返 DRAWING");
    assert!(out.items.iter().all(|i| i.kind == "DRAWING"));
}

#[tokio::test]
async fn get_url_returns_presigned_url() {
    let pool = setup().await;
    let l1 = insert_l1_customer(&pool, "客户PF-URL", "F").await;
    let l2 = insert_l2_customer(&pool, "子客PF-URL", l1).await;
    let part_id = insert_part_for_owner(&pool, l2).await;

    let current = test_current_user_with_roles(vec![Role::Manager]);
    let snowflake = Arc::new(SnowflakeIdGenerator::new(1_577_836_800_000, 1));
    let cos = Arc::new(NoopCos);

    let mut tx = pool.begin().await.unwrap();
    let out = PartFileService::new(snowflake.clone(), cos.clone()).upload_file_for_owner(
        &mut *tx,
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
    let detail = PartFileService::new(snowflake.clone(), cos.clone()).get_file_with_url(&mut *tx, cos.clone(), out.id, &current)
        .await
        .unwrap();
    drop(tx);
    assert_eq!(detail.id, out.id.to_string());
    assert_eq!(detail.kind, "DRAWING");
    assert!(detail.download_url.contains(&out.object_key));
    assert_eq!(detail.url_expires_in_seconds, 3600);
}

// ===== 2026-09-16 补：paired_file_id 投影测试（前端 CNC 配对分组契约字段） =====

/// 列表项必须投影 `paired_file_id`：直接 SQL 造一对 CNC 配对行
/// （G_CODE <-> SETUP_SHEET 互指），断言 list_files 出参带回配对 id，
/// 且 JSON 序列化为 string（雪花 id 防 JS 精度截断）。
#[tokio::test]
async fn list_includes_paired_file_id() {
    let pool = setup().await;
    let l1 = insert_l1_customer(&pool, "客户PF-Pair", "F").await;
    let l2 = insert_l2_customer(&pool, "子客PF-Pair", l1).await;
    let part_id = insert_part_for_owner(&pool, l2).await;

    // 直接 SQL 造配对行（绕开 cnc_program 上传通道，聚焦 part_file 列表投影）
    let snowflake = Arc::new(SnowflakeIdGenerator::new(1_577_836_800_000, 1));
    let cos = Arc::new(NoopCos);
    let g_id = snowflake.next_id();
    let s_id = snowflake.next_id();
    for (id, kind, file_type, filename, sha, paired_id) in [
        (g_id, "G_CODE", "NC", "prog.nc", "g".repeat(64), s_id),
        (
            s_id,
            "SETUP_SHEET",
            "PDF",
            "setup.pdf",
            "s".repeat(64),
            g_id,
        ),
    ] {
        sqlx::query(
            "INSERT INTO t_part_file \
               (id, part_id, kind, file_type, object_key, original_filename, \
                file_size, content_type, upload_status, content_sha256, paired_file_id, \
                created_at, created_by, updated_at, updated_by) \
             VALUES ($1, $2, $3, $4, $5, $6, 100, 'application/octet-stream', 'READY', $7, \
                     $8, now(), 1, now(), 1)",
        )
        .bind(id)
        .bind(part_id)
        .bind(kind)
        .bind(file_type)
        .bind(format!("part/{part_id}/{kind}/{filename}"))
        .bind(filename)
        .bind(sha)
        .bind(paired_id)
        .execute(&pool)
        .await
        .expect("insert paired row");
    }

    let current = test_current_user_with_roles(vec![Role::Manager]);
    let query = hsh_erp_rust::modules::part_file::dto::PartFileListQuery {
        owner_kind: Some("PART".into()),
        owner_id: Some(part_id.to_string()),
        kind: None,
        limit: Some(50),
        offset: Some(0),
    };
    let mut tx = pool.begin().await.unwrap();
    let out = PartFileService::new(snowflake.clone(), cos.clone()).list_files(&mut *tx, &query, &current)
        .await
        .unwrap();
    drop(tx);

    assert_eq!(out.items.len(), 2);
    let g = out
        .items
        .iter()
        .find(|i| i.kind == "G_CODE")
        .expect("G_CODE 行");
    let s = out
        .items
        .iter()
        .find(|i| i.kind == "SETUP_SHEET")
        .expect("SETUP_SHEET 行");
    assert_eq!(g.paired_file_id, Some(s_id), "G_CODE 应指向 SETUP_SHEET");
    assert_eq!(s.paired_file_id, Some(g_id), "SETUP_SHEET 应指向 G_CODE");

    // 契约守护：paired_file_id 字段必须存在于 JSON 且序列化为 string
    let v = serde_json::to_value(g).unwrap();
    assert_eq!(
        v["paired_file_id"],
        serde_json::Value::String(s_id.to_string()),
        "paired_file_id 必须以 string 形式出现在 JSON"
    );
}

// ===== 2026-09-15 takeover-fill：content / delete 端点测试 =====
#[tokio::test]
async fn content_happy_path() {
    let pool = setup().await;
    let l1 = insert_l1_customer(&pool, "客户PF-Content", "F").await;
    let l2 = insert_l2_customer(&pool, "子客PF-Content", l1).await;
    let part_id = insert_part_for_owner(&pool, l2).await;

    let current = test_current_user_with_roles(vec![Role::Manager]);
    let snowflake = Arc::new(SnowflakeIdGenerator::new(1_577_836_800_000, 1));
    let cos = Arc::new(NoopCos);

    let mut tx = pool.begin().await.unwrap();
    let out = PartFileService::new(snowflake.clone(), cos.clone()).upload_file_for_owner(
        &mut *tx,
        "PART",
        part_id,
        "DRAWING",
        "content.pdf",
        "application/pdf",
        b"%PDF-1.5\nhello\n%%EOF".to_vec(),
        &current,
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    // content 端点 — NoopCos.get_object 返空字节，但应正常返回
    let mut tx = pool.begin().await.unwrap();
    let content = PartFileService::new(snowflake.clone(), cos.clone()).get_file_content(&mut *tx, cos.clone(), out.id, &current)
        .await
        .expect("content ok");
    drop(tx);
    assert!(content.content_type.is_some());
    // NoopCos.get_object 返回空
    assert!(content.bytes.is_empty());
}

#[tokio::test]
async fn soft_delete_happy_path() {
    let pool = setup().await;
    let l1 = insert_l1_customer(&pool, "客户PF-Del", "F").await;
    let l2 = insert_l2_customer(&pool, "子客PF-Del", l1).await;
    let part_id = insert_part_for_owner(&pool, l2).await;

    let current = test_current_user_with_roles(vec![Role::Manager]);
    let snowflake = Arc::new(SnowflakeIdGenerator::new(1_577_836_800_000, 1));
    let cos = Arc::new(NoopCos);

    let mut tx = pool.begin().await.unwrap();
    let out = PartFileService::new(snowflake.clone(), cos.clone()).upload_file_for_owner(
        &mut *tx,
        "PART",
        part_id,
        "DRAWING",
        "del.pdf",
        "application/pdf",
        b"%PDF-1.5\n".to_vec(),
        &current,
    )
    .await
    .unwrap();
    let version = out.version;
    tx.commit().await.unwrap();

    // delete by Manager（kind=DRAWING → M+C 通行）
    let mut tx = pool.begin().await.unwrap();
    let object_key =
        PartFileService::new(snowflake.clone(), cos.clone()).soft_delete_file(&mut *tx, out.id, version, &current)
            .await
            .expect("delete ok");
    tx.commit().await.unwrap();
    // 2026-09-15 review A2：service 应返回 cos object_key 供 handler commit 后清理
    assert!(
        !object_key.is_empty(),
        "soft_delete_file 应返回 cos object_key"
    );

    // 再次查应 not found（include_deleted=false）
    let mut tx = pool.begin().await.unwrap();
    let row =
        hsh_erp_rust::modules::part_file::repo::PartFileRepo::get_by_id(&mut *tx, out.id, false)
            .await
            .unwrap();
    drop(tx);
    assert!(row.is_none(), "软删后应查不到");
}

#[tokio::test]
async fn soft_delete_version_conflict() {
    let pool = setup().await;
    let l1 = insert_l1_customer(&pool, "客户PF-Conf", "F").await;
    let l2 = insert_l2_customer(&pool, "子客PF-Conf", l1).await;
    let part_id = insert_part_for_owner(&pool, l2).await;

    let current = test_current_user_with_roles(vec![Role::Manager]);
    let snowflake = Arc::new(SnowflakeIdGenerator::new(1_577_836_800_000, 1));
    let cos = Arc::new(NoopCos);

    let mut tx = pool.begin().await.unwrap();
    let out = PartFileService::new(snowflake.clone(), cos.clone()).upload_file_for_owner(
        &mut *tx,
        "PART",
        part_id,
        "DRAWING",
        "conf.pdf",
        "application/pdf",
        b"%PDF-1.5\n".to_vec(),
        &current,
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    // 故意 version+999 → 冲突
    let mut tx = pool.begin().await.unwrap();
    let err = PartFileService::new(snowflake.clone(), cos.clone()).soft_delete_file(&mut *tx, out.id, out.version + 999, &current)
        .await
        .expect_err("version 冲突应抛错");
    drop(tx);
    match err {
        hsh_erp_rust::shared::error::AppError::Biz { code, .. } => {
            assert_eq!(code, 40901, "VERSION_CONFLICT");
        }
        other => panic!("期望 AppError::Biz(40901)，got {other:?}"),
    }
}

// ===========================================================================
// 2026-09-18 注释：原 M2-B review 第 1 轮的 upload-intents 集成测试
// （upload_intents_owner_not_found / upload_intents_dedup_hit）已删除——
// upload-intents 端点 2026-09-18 删除，迁移至 upload_session 域（Redis 共享
// STS 凭证机制）。本文件保留 confirm 端点的集成测试（tmp_missing /
// size_mismatch / replace_old_single 三条），通过 PartFileService::bind_uploaded_file
// 端到端验证。
//
// 2026-09-18 注：原 upload-intents 测试用到的 `infra::sts::NoopSts` 也已删除，
// 现统一用 `infra::python_sts::NoopPythonSts`（占位 STS 转发到 python 后端）。
// ===========================================================================

use hsh_erp_rust::modules::part_file::dto::ConfirmFileIn;
use hsh_erp_rust::modules::part_file::repo::PartFileRepo;

const UPLOAD_TMP_PREFIX: &str = "tmp/";
const UPLOAD_UPLOAD_PREFIX: &str = "uploads";

#[tokio::test]
async fn confirm_tmp_missing_returns_21114() {
    // 验证 plan T2.6：head_object 返回 NoSuchKey → 21114 TMP_OBJECT_MISSING
    let pool = setup().await;
    let l1 = insert_l1_customer(&pool, "客户PF-TmpMiss", "F").await;
    let l2 = insert_l2_customer(&pool, "子客PF-TmpMiss", l1).await;
    let part_id = insert_part_for_owner(&pool, l2).await;

    let current = test_current_user_with_roles(vec![Role::Manager]);
    let snowflake = Arc::new(SnowflakeIdGenerator::new(1_577_836_800_000, 1));
    let cos = Arc::new(MockCos::new()); // 未注册 tmp_key → head_object NoSuch

    let tmp_key = "tmp/test/missing.pdf";
    let sha = "c".repeat(64);
    let req = ConfirmFileIn {
        kind: "DRAWING".into(),
        tmp_key: tmp_key.into(),
        content_sha256: sha,
        original_filename: "drawing.pdf".into(),
        file_size: 1024,
        content_type: "application/pdf".into(),
    };
    let err = PartFileService::bind_uploaded_file(
        &pool,
        &snowflake,
        cos.clone(),
        UPLOAD_UPLOAD_PREFIX,
        UPLOAD_TMP_PREFIX,
        part_id,
        &req.kind,
        &req.tmp_key,
        &req.content_sha256,
        &req.original_filename,
        req.file_size,
        &req.content_type,
        &current,
    )
    .await
    .expect_err("head_object 找不到 tmp_key 应报错");

    match err {
        AppError::Biz { code, .. } => {
            assert_eq!(code, 21114, "BIZ_PART_FILE_TMP_OBJECT_MISSING");
        }
        other => panic!("期望 AppError::Biz(21114)，got {other:?}"),
    }
    // 验证 head 被调用了一次（mock 计数）
    assert_eq!(
        cos.head_call_count(tmp_key),
        1,
        "MockCos.head_object 应被调用 1 次"
    );
}

#[tokio::test]
async fn confirm_size_mismatch_returns_21115() {
    // 验证 plan T2.6：head size 与声明 size 不一致 → 21115 SIZE_MISMATCH
    let pool = setup().await;
    let l1 = insert_l1_customer(&pool, "客户PF-SizeMM", "F").await;
    let l2 = insert_l2_customer(&pool, "子客PF-SizeMM", l1).await;
    let part_id = insert_part_for_owner(&pool, l2).await;

    let current = test_current_user_with_roles(vec![Role::Manager]);
    let snowflake = Arc::new(SnowflakeIdGenerator::new(1_577_836_800_000, 1));
    let cos = Arc::new(MockCos::new());

    let tmp_key = "tmp/test/sizemm.pdf";
    // mock head 返回 size=2048，但客户端声明 size=1024 → 触发 size mismatch
    cos.set_head(tmp_key, 2048);
    let sha = "d".repeat(64);
    let req = ConfirmFileIn {
        kind: "DRAWING".into(),
        tmp_key: tmp_key.into(),
        content_sha256: sha,
        original_filename: "drawing.pdf".into(),
        file_size: 1024, // 客户端声明
        content_type: "application/pdf".into(),
    };
    let err = PartFileService::bind_uploaded_file(
        &pool,
        &snowflake,
        cos.clone(),
        UPLOAD_UPLOAD_PREFIX,
        UPLOAD_TMP_PREFIX,
        part_id,
        &req.kind,
        &req.tmp_key,
        &req.content_sha256,
        &req.original_filename,
        req.file_size,
        &req.content_type,
        &current,
    )
    .await
    .expect_err("size 不一致应报错");

    match err {
        AppError::Biz { code, .. } => {
            assert_eq!(code, 21115, "BIZ_PART_FILE_SIZE_MISMATCH");
        }
        other => panic!("期望 AppError::Biz(21115)，got {other:?}"),
    }
}

#[tokio::test]
async fn confirm_replace_old_single_returns_ready() {
    // 验证 plan T2.6：同 part+kind 二次 confirm → 旧行 deleted_at 已设，新行 READY
    let pool = setup().await;
    let l1 = insert_l1_customer(&pool, "客户PF-Replace", "F").await;
    let l2 = insert_l2_customer(&pool, "子客PF-Replace", l1).await;
    let part_id = insert_part_for_owner(&pool, l2).await;

    let current = test_current_user_with_roles(vec![Role::Manager]);
    let snowflake = Arc::new(SnowflakeIdGenerator::new(1_577_836_800_000, 1));
    let cos = Arc::new(MockCos::new());

    // 第一次 confirm（kind=DRAWING / sha=aaa...）
    let tmp_key_1 = "tmp/test/replace-1.pdf";
    cos.set_head(tmp_key_1, 1024);
    let sha_1 = "a".repeat(64);
    let (out1, _tk1) = PartFileService::bind_uploaded_file(
        &pool,
        &snowflake,
        cos.clone(),
        UPLOAD_UPLOAD_PREFIX,
        UPLOAD_TMP_PREFIX,
        part_id,
        "DRAWING",
        tmp_key_1,
        &sha_1,
        "first.pdf",
        1024,
        "application/pdf",
        &current,
    )
    .await
    .expect("first confirm ok");
    let first_id: i64 = out1.id;
    assert_eq!(out1.upload_status, "READY");

    // 第二次 confirm（kind=DRAWING / sha=bbb... 不同 sha → 触发旧行软删）
    let tmp_key_2 = "tmp/test/replace-2.pdf";
    cos.set_head(tmp_key_2, 2048);
    let sha_2 = "b".repeat(64);
    let (out2, _tk2) = PartFileService::bind_uploaded_file(
        &pool,
        &snowflake,
        cos.clone(),
        UPLOAD_UPLOAD_PREFIX,
        UPLOAD_TMP_PREFIX,
        part_id,
        "DRAWING",
        tmp_key_2,
        &sha_2,
        "second.pdf",
        2048,
        "application/pdf",
        &current,
    )
    .await
    .expect("second confirm ok");
    let second_id: i64 = out2.id;
    assert_eq!(out2.upload_status, "READY");
    assert_ne!(first_id, second_id, "新行 id 应不同");

    // 旧行：查 include_deleted=true → 行存在 + deleted_at 非空
    let mut tx = pool.begin().await.unwrap();
    let old_row = PartFileRepo::get_by_id(&mut *tx, first_id, true)
        .await
        .unwrap();
    drop(tx);
    assert!(old_row.is_some(), "include_deleted=true 旧行仍可查到");
    let old = old_row.unwrap();
    assert!(
        old.deleted_at.is_some(),
        "旧行 deleted_at 必须被设置（uk_t_part_file_single 单文件替换）"
    );
    assert_eq!(old.content_sha256.as_deref(), Some(sha_1.as_str()));

    // 新行：READ 且 deleted_at=NULL
    let mut tx = pool.begin().await.unwrap();
    let new_row = PartFileRepo::get_by_id(&mut *tx, second_id, false)
        .await
        .unwrap();
    drop(tx);
    assert!(new_row.is_some(), "新行应可查到（deleted_at IS NULL）");
    let new = new_row.unwrap();
    assert!(new.deleted_at.is_none(), "新行 deleted_at 必须为 NULL");
    assert_eq!(new.content_sha256.as_deref(), Some(sha_2.as_str()));
    assert_eq!(new.upload_status, "READY");
    assert_eq!(new.file_size, 2048);
}

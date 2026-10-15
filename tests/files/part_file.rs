//! `files::part_file` 子模块集成测试（2026-09-14 Phase 3 建，2026-10-13 自
//! `tests/part/file.rs` 迁入本 binary）
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
//!   9. rbac_shelf_account_can_list_files — 工控机账号能列文件（2026-10-10 放开）
//!  10. rbac_shelf_account_can_get_file_content — 工控机账号能读内容（2026-10-10 放开）
//!  11. rbac_shelf_account_get_file_url_still_forbidden — 预签直链**不**放开给工控机
//!  12. rbac_shelf_account_cannot_upload_or_delete — 只放开只读，写路径仍拒
//!  13. old_part_files_prefix_returns_404 — 硬切守卫（2026-10-13 新增）
//!  14. old_part_files_numeric_path_returns_404 — 硬切守卫（2026-10-13 新增）
//!  15. old_cnc_programs_prefix_returns_404 — 硬切守卫（2026-10-13 新增）
//!  16. old_part_upload_drawing_returns_404 — PR1b 硬切守卫（part 侧上传）
//!  17. old_part_upload_3d_model_returns_404 — PR1b 硬切守卫（part 侧 3D 上传）
//!  18. old_part_files_confirm_returns_404 — PR1b 硬切守卫（confirm 搬家）
//!  19. old_assembly_files_returns_404 — PR1b 硬切守卫（assembly 侧 POST + GET）
//!  20. list_assembly_owner_filters_by_kind_and_owner — ASSEMBLY owner 列表**内容**
//!      断言（review 第 1 轮 MINOR-7：被删的 `tests/assembly/files_list.rs` 留下的空洞）
//!  21. soft_delete_assembly_master_happy_path — `ASSEMBLY_MASTER` 可软删
//!      （review 第 1 轮 MINOR-6 回归：闭合前是「能造不能删」）
//!  22. get_url_requires_permission_point          — `/url` 守卫是权限点，Manager 未持点也
//!      403（2026-10-14 迁到 `part.drawing.print`，随域收编迁到 `/files/objects/{id}/url`）
//!
//! ## 集成策略
//! 主体是**直接 service 直调**（避免 JWT/Redis 开销）：`pool.begin()` 开 tx →
//! 传 `&mut *tx` 给 service → 显式 `tx.commit()`。
//! 例外是末尾 7 条**硬切守卫**（2026-10-13 PR1a 3 条 + PR1b 4 条）：它们打的是真实
//! HTTP 路由，断言旧前缀 404 —— 这是 URL 硬切的回归护栏，service 直调结构上测不到。
//!
//! ⚠️ 2026-10-13 PR1b：`upload_file_for_owner` / `bind_uploaded_file` 各多收一个
//! 形参（前者的 cfg 前缀、后者的 `owner_kind`），本文件全部调用点已同步。
//! 2026-10-13 review 第 1 轮 MAJOR-1：两者又各多收 `cfg_max_file_size`（原先
//! `bind_uploaded_file` 写死 300MB），调用点同样已同步。
//! 装配件 owner（`owner_kind=ASSEMBLY`）的上传用例在 `upload.rs`。
//!
//! ## 2026-10-13 随端点下线删除的用例
//! `rbac_shelf_account_rejects_part_nested_lists` —— 它断言的是零件维度三条嵌套列表
//! 端点（`GET /part-files/parts/{id}/{files,cnc-programs,setup-sheets}`）对
//! SHELF_ACCOUNT 返回 403。这三条端点本身已随零调用方清理下线，端点不存在了、
//! 「handler 级闸门比 service 严」这个分叉也不再存在，故整条用例删除
//! （角色矩阵的「`/url` 不放开给 SHELF」部分由本文件第 11 条独立守住）。

use std::sync::Arc;

use axum::http::StatusCode;
use hsh_erp_rust::auth::rbac::{CurrentUser, Role};
use hsh_erp_rust::infra::cos::NoopCos;
use hsh_erp_rust::modules::files::part_file::service::PartFileService;
use hsh_erp_rust::shared::error::AppError;
use serde_json::json;
use sqlx::PgPool;

use hsh_erp_test_support::*;

// ===========================================================================
//  动态 customer / part 插入 helper（sub-file 私有，PR-C 末统一迁）
// ===========================================================================

/// 在 fixture 之外另建 1 个 L1 客户（避免污染 fixture）。
async fn insert_l1_customer(pool: &PgPool, name: &str, prefix: &str) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;
    let id = shared_test_snowflake().next_id();
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

async fn insert_l2_customer(pool: &PgPool, name: &str, l1_id: i64) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;
    let id = shared_test_snowflake().next_id();
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

async fn insert_part_for_owner(pool: &PgPool, customer_id: i64) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;
    let id = shared_test_snowflake().next_id();
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
//  setup helper
// ===========================================================================

async fn setup() -> PgPool {
    let pool = test_pool().await;
    let _fx = load_part_fixture(&pool).await;
    pool
}

// ===========================================================================
//  私有 fixture helpers
// ===========================================================================

fn test_current_user_with_roles(roles: Vec<Role>) -> CurrentUser {
    test_current_user_with_roles_and_perms(roles, &[])
}

/// 同 [`test_current_user_with_roles`]，另装权限点（2026-10-14：`/url` 的守卫
/// 从 4 角色白名单迁到权限点 `part.drawing.print` 后按需构造持有者）。
fn test_current_user_with_roles_and_perms(roles: Vec<Role>, perms: &[&str]) -> CurrentUser {
    CurrentUser {
        id: 1,
        username: "tester".into(),
        roles,
        shelf_ids: vec![],
        shelf_wildcard: false,
        permissions: perms.iter().map(|s| s.to_string()).collect(),
    }
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
    let cos = Arc::new(NoopCos);

    let pdf_bytes = b"%PDF-1.5\nhello world\n%%EOF".to_vec();
    let mut tx = pool.begin().await.unwrap();
    let out = PartFileService::new(shared_test_snowflake().clone(), cos.clone())
        .upload_file_for_owner(
            &mut *tx,
            UPLOAD_UPLOAD_PREFIX,
            UPLOAD_MAX_FILE_SIZE,
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
    let cos = Arc::new(NoopCos);

    let bytes = b"some step data".to_vec();
    let mut tx = pool.begin().await.unwrap();
    let err = PartFileService::new(shared_test_snowflake().clone(), cos.clone())
        .upload_file_for_owner(
            &mut *tx,
            UPLOAD_UPLOAD_PREFIX,
            UPLOAD_MAX_FILE_SIZE,
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
    let cos = Arc::new(NoopCos);

    let pdf_bytes = b"%PDF-1.5\nidentical content\n%%EOF".to_vec();

    // 第一次上传
    let mut tx = pool.begin().await.unwrap();
    let out1 = PartFileService::new(shared_test_snowflake().clone(), cos.clone())
        .upload_file_for_owner(
            &mut *tx,
            UPLOAD_UPLOAD_PREFIX,
            UPLOAD_MAX_FILE_SIZE,
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
    let out2 = PartFileService::new(shared_test_snowflake().clone(), cos.clone())
        .upload_file_for_owner(
            &mut *tx,
            UPLOAD_UPLOAD_PREFIX,
            UPLOAD_MAX_FILE_SIZE,
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
    let cos = Arc::new(NoopCos);

    let nonexistent = 99_999_999_999_i64;
    let mut tx = pool.begin().await.unwrap();
    let err = PartFileService::new(shared_test_snowflake().clone(), cos.clone())
        .upload_file_for_owner(
            &mut *tx,
            UPLOAD_UPLOAD_PREFIX,
            UPLOAD_MAX_FILE_SIZE,
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
    let cos = Arc::new(NoopCos);

    let mut tx = pool.begin().await.unwrap();
    let err = PartFileService::new(shared_test_snowflake().clone(), cos.clone())
        .upload_file_for_owner(
            &mut *tx,
            UPLOAD_UPLOAD_PREFIX,
            UPLOAD_MAX_FILE_SIZE,
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
        PartFileService::new(shared_test_snowflake().clone(), cos.clone())
            .upload_file_for_owner(
                &mut *tx,
                UPLOAD_UPLOAD_PREFIX,
                UPLOAD_MAX_FILE_SIZE,
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
    let query = hsh_erp_rust::modules::files::part_file::dto::PartFileListQuery {
        owner_kind: Some("PART".into()),
        owner_id: Some(part_id.to_string()),
        kind: Some("DRAWING".into()),
        limit: Some(50),
        offset: Some(0),
    };
    let mut tx = pool.begin().await.unwrap();
    let out = PartFileService::new(shared_test_snowflake().clone(), cos.clone())
        .list_files(&mut *tx, &query, &current)
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

    // 2026-10-14：`/url` 的守卫是权限点 `part.drawing.print`（默认矩阵授 MANAGER），
    // fixture 同时装角色与点。
    let current =
        test_current_user_with_roles_and_perms(vec![Role::Manager], &["part.drawing.print"]);
    let cos = Arc::new(NoopCos);

    let mut tx = pool.begin().await.unwrap();
    let out = PartFileService::new(shared_test_snowflake().clone(), cos.clone())
        .upload_file_for_owner(
            &mut *tx,
            UPLOAD_UPLOAD_PREFIX,
            UPLOAD_MAX_FILE_SIZE,
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
    let detail = PartFileService::new(shared_test_snowflake().clone(), cos.clone())
        .get_file_with_url(
            &mut *tx,
            cos.clone(),
            UPLOAD_UPLOAD_PREFIX,
            UPLOAD_PRESIGN_EXPIRE,
            out.id,
            &current,
        )
        .await
        .unwrap();
    drop(tx);
    assert_eq!(detail.id, out.id.to_string());
    assert_eq!(detail.kind, "DRAWING");
    assert!(detail.download_url.contains(&out.object_key));
    assert_eq!(detail.url_expires_in_seconds, UPLOAD_PRESIGN_EXPIRE);
}

/// 守卫是**权限点**而不是角色：同为 MANAGER（原白名单内角色），未持
/// `part.drawing.print` 即 40300（2026-10-14 迁移的回归锚）。
///
/// 这正是跨仓权限点迁移要消灭的分叉 —— 前端按 `hasPermission` 显隐、后端按同一
/// 权限点放行；角色本身不再单独决定访问权（自定义角色持点同样放行）。
#[tokio::test]
async fn get_url_requires_permission_point() {
    let pool = setup().await;
    let l1 = insert_l1_customer(&pool, "客户PF-URL-Perm", "F").await;
    let l2 = insert_l2_customer(&pool, "子客PF-URL-Perm", l1).await;
    let part_id = insert_part_for_owner(&pool, l2).await;

    let cos = Arc::new(NoopCos);
    let mut tx = pool.begin().await.unwrap();
    let uploaded = PartFileService::new(shared_test_snowflake().clone(), cos.clone())
        .upload_file_for_owner(
            &mut *tx,
            UPLOAD_UPLOAD_PREFIX,
            UPLOAD_MAX_FILE_SIZE,
            "PART",
            part_id,
            "DRAWING",
            "perm.pdf",
            "application/pdf",
            b"%PDF-1.5\n".to_vec(),
            &test_current_user_with_roles_and_perms(vec![Role::Manager], &["part.drawing.print"]),
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();

    // Manager 角色在手、权限点未授 ⇒ 40300
    let manager_no_perm = test_current_user_with_roles(vec![Role::Manager]);
    let mut tx = pool.begin().await.unwrap();
    let err = PartFileService::new(shared_test_snowflake().clone(), cos.clone())
        .get_file_with_url(
            &mut *tx,
            cos.clone(),
            UPLOAD_UPLOAD_PREFIX,
            UPLOAD_PRESIGN_EXPIRE,
            uploaded.id,
            &manager_no_perm,
        )
        .await
        .expect_err("未持 part.drawing.print 即应拒绝，哪怕角色是 MANAGER");
    drop(tx);

    match err {
        AppError::Biz { code, .. } => assert_eq!(
            code, 40300,
            "FORBIDDEN：守卫是权限点 `part.drawing.print`，角色不再单独决定访问权"
        ),
        other => panic!("期望 AppError::Biz(40300)，got {other:?}"),
    }
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
    let cos = Arc::new(NoopCos);
    let g_id = shared_test_snowflake().next_id();
    let s_id = shared_test_snowflake().next_id();
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
    let query = hsh_erp_rust::modules::files::part_file::dto::PartFileListQuery {
        owner_kind: Some("PART".into()),
        owner_id: Some(part_id.to_string()),
        kind: None,
        limit: Some(50),
        offset: Some(0),
    };
    let mut tx = pool.begin().await.unwrap();
    let out = PartFileService::new(shared_test_snowflake().clone(), cos.clone())
        .list_files(&mut *tx, &query, &current)
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
    let cos = Arc::new(NoopCos);

    let mut tx = pool.begin().await.unwrap();
    let out = PartFileService::new(shared_test_snowflake().clone(), cos.clone())
        .upload_file_for_owner(
            &mut *tx,
            UPLOAD_UPLOAD_PREFIX,
            UPLOAD_MAX_FILE_SIZE,
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
    // ⚠️ 2026-10-16 review 第 2 轮 MINOR-A：原先这里调 `get_file_content`（那个
    // 「有角色闸门、无 kind 闸门」的 pub 方法已随本轮整条删除），现走生产同款形态
    // —— handler 取一次行判完 kind，再交给 `get_file_content_of_checked_row`。
    let svc = PartFileService::new(shared_test_snowflake().clone(), cos.clone());
    let mut tx = pool.begin().await.unwrap();
    let row = svc.get_file_row(&mut *tx, out.id).await.unwrap();
    let content = svc
        .get_file_content_of_checked_row(cos.as_ref(), UPLOAD_UPLOAD_PREFIX, &row, &current)
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
    let cos = Arc::new(NoopCos);

    let mut tx = pool.begin().await.unwrap();
    let out = PartFileService::new(shared_test_snowflake().clone(), cos.clone())
        .upload_file_for_owner(
            &mut *tx,
            UPLOAD_UPLOAD_PREFIX,
            UPLOAD_MAX_FILE_SIZE,
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
    let object_key = PartFileService::new(shared_test_snowflake().clone(), cos.clone())
        .soft_delete_file(&mut *tx, out.id, version, &current)
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
    let row = hsh_erp_rust::modules::files::part_file::repo::PartFileRepo::get_by_id(
        &mut *tx, out.id, false,
    )
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
    let cos = Arc::new(NoopCos);

    let mut tx = pool.begin().await.unwrap();
    let out = PartFileService::new(shared_test_snowflake().clone(), cos.clone())
        .upload_file_for_owner(
            &mut *tx,
            UPLOAD_UPLOAD_PREFIX,
            UPLOAD_MAX_FILE_SIZE,
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
    let err = PartFileService::new(shared_test_snowflake().clone(), cos.clone())
        .soft_delete_file(&mut *tx, out.id, out.version + 999, &current)
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

// ===== 2026-09-18 takeover：confirm 端点测试 =====

use hsh_erp_rust::modules::files::part_file::dto::ConfirmFileIn;
use hsh_erp_rust::modules::files::part_file::repo::PartFileRepo;

const UPLOAD_TMP_PREFIX: &str = "tmp/";
const UPLOAD_UPLOAD_PREFIX: &str = "uploads";

/// 上传 / confirm 两条写路径的 size 上限（2026-10-13 review 第 1 轮 MAJOR-1：
/// 两条路径都改成收 handler 下发的 `config.cos.max_file_size`）。
///
/// 取值与 `test_state` 的 `CosConfig.max_file_size` **逐字一致**（300MB），使本文件
/// 既有用例的行为与走 HTTP handler 的路径保持同一条上限；想测「超限」分支请显式传
/// 一个更小的值（见 `upload_over_max_file_size_returns_21103`），别依赖改这里。
const UPLOAD_MAX_FILE_SIZE: usize = 300 * 1024 * 1024;
/// 2026-10-15：`/url` 的预签直链有效期改读 `COS_PRESIGN_EXPIRE`（不再写死 3600），
/// service 层单测与 handler 集成测试共用同一个常量。
const UPLOAD_PRESIGN_EXPIRE: u32 = 3600;

#[tokio::test]
async fn confirm_tmp_missing_returns_21114() {
    let pool = setup().await;
    let l1 = insert_l1_customer(&pool, "客户PF-TmpMiss", "F").await;
    let l2 = insert_l2_customer(&pool, "子客PF-TmpMiss", l1).await;
    let part_id = insert_part_for_owner(&pool, l2).await;

    let current = test_current_user_with_roles(vec![Role::Manager]);
    let cos = Arc::new(MockCos::new()); // 未注册 tmp_key → head_object NoSuch

    let tmp_key = "tmp/test/missing.pdf";
    let sha = "c".repeat(64);
    let req = ConfirmFileIn {
        // 2026-10-13 PR1b：owner 改由 body 携带（旧端点的 part_id 在 path 段）
        owner_kind: "PART".into(),
        owner_id: part_id.to_string(),
        kind: "DRAWING".into(),
        tmp_key: tmp_key.into(),
        content_sha256: sha,
        original_filename: "drawing.pdf".into(),
        file_size: 1024,
        content_type: "application/pdf".into(),
        ext: None, // 2026-09-29 新增字段：client 未传走兼容回退
    };
    let err = PartFileService::bind_uploaded_file(
        &pool,
        shared_test_snowflake().as_ref(),
        cos.clone(),
        UPLOAD_UPLOAD_PREFIX,
        UPLOAD_TMP_PREFIX,
        UPLOAD_MAX_FILE_SIZE,
        "PART",
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
    let pool = setup().await;
    let l1 = insert_l1_customer(&pool, "客户PF-SizeMM", "F").await;
    let l2 = insert_l2_customer(&pool, "子客PF-SizeMM", l1).await;
    let part_id = insert_part_for_owner(&pool, l2).await;

    let current = test_current_user_with_roles(vec![Role::Manager]);
    let cos = Arc::new(MockCos::new());

    let tmp_key = "tmp/test/sizemm.pdf";
    // mock head 返回 size=2048，但客户端声明 size=1024 → 触发 size mismatch
    cos.set_head(tmp_key, 2048);
    let sha = "d".repeat(64);
    let req = ConfirmFileIn {
        // 2026-10-13 PR1b：owner 改由 body 携带（旧端点的 part_id 在 path 段）
        owner_kind: "PART".into(),
        owner_id: part_id.to_string(),
        kind: "DRAWING".into(),
        tmp_key: tmp_key.into(),
        content_sha256: sha,
        original_filename: "drawing.pdf".into(),
        file_size: 1024, // 客户端声明
        content_type: "application/pdf".into(),
        ext: None, // 2026-09-29 新增字段
    };
    let err = PartFileService::bind_uploaded_file(
        &pool,
        shared_test_snowflake().as_ref(),
        cos.clone(),
        UPLOAD_UPLOAD_PREFIX,
        UPLOAD_TMP_PREFIX,
        UPLOAD_MAX_FILE_SIZE,
        "PART",
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
    let pool = setup().await;
    let l1 = insert_l1_customer(&pool, "客户PF-Replace", "F").await;
    let l2 = insert_l2_customer(&pool, "子客PF-Replace", l1).await;
    let part_id = insert_part_for_owner(&pool, l2).await;

    let current = test_current_user_with_roles(vec![Role::Manager]);
    let cos = Arc::new(MockCos::new());

    // 第一次 confirm（kind=DRAWING / sha=aaa...）
    let tmp_key_1 = "tmp/test/replace-1.pdf";
    cos.set_head(tmp_key_1, 1024);
    let sha_1 = "a".repeat(64);
    let (out1, _tk1) = PartFileService::bind_uploaded_file(
        &pool,
        shared_test_snowflake().as_ref(),
        cos.clone(),
        UPLOAD_UPLOAD_PREFIX,
        UPLOAD_TMP_PREFIX,
        UPLOAD_MAX_FILE_SIZE,
        "PART",
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
        shared_test_snowflake().as_ref(),
        cos.clone(),
        UPLOAD_UPLOAD_PREFIX,
        UPLOAD_TMP_PREFIX,
        UPLOAD_MAX_FILE_SIZE,
        "PART",
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

// ===== 2026-09-29 扁平化新测试：tmp→parts copy_object + 读端点 legacy fallback =====
//
// 不依赖 axum，直接 service 直调：
// - tmp_to_parts_copy_test：确认 CAS key 模板五段→两段后，bind_uploaded_file
//   仍走 copy_object(tmp → cas_key)，cas_key 形如 `{prefix}{sha16}_{safe_filename}`。
// - read_fallback_to_legacy_key_test：模拟 DB 历史行 object_key 是五段格式
//   但 COS 桶里真实对象按新模板存的场景，验证 `resolve_effective_key` 重写后
//   能命中。

#[tokio::test]
async fn tmp_to_parts_copy_test() {
    // 2026-09-29 新增：bind_uploaded_file 派生 CAS key 走新模板（两段），copy 仍触发
    let pool = setup().await;
    let l1 = insert_l1_customer(&pool, "客户PF-Copy2Seg", "F").await;
    let l2 = insert_l2_customer(&pool, "子客PF-Copy2Seg", l1).await;
    let part_id = insert_part_for_owner(&pool, l2).await;

    let current = test_current_user_with_roles(vec![Role::Manager]);
    let cos = Arc::new(MockCos::new());

    let tmp_key = "tmp/test/copy-2seg.pdf";
    cos.set_head(tmp_key, 1024);
    let sha = "f".repeat(64);
    let (out, _tk) = PartFileService::bind_uploaded_file(
        &pool,
        shared_test_snowflake().as_ref(),
        cos.clone(),
        UPLOAD_UPLOAD_PREFIX,
        UPLOAD_TMP_PREFIX,
        UPLOAD_MAX_FILE_SIZE,
        "PART",
        part_id,
        "DRAWING",
        tmp_key,
        &sha,
        "test.pdf",
        1024,
        "application/pdf",
        &current,
    )
    .await
    .expect("bind_uploaded_file 应成功");

    // 2026-09-29 扁平化：新模板两段 `{prefix}{sha16}_{safe_filename}`
    // 即 `uploads/fffffff...{16chars}_test.pdf`（prefix 兼容 .env `uploads` /
    // `uploads/` 两种风格 —— 此处传 "uploads"，build_cas_key 自动补尾斜杠）。
    let expected_cas_key = "uploads/ffffffffffffffff_test.pdf";
    assert_eq!(
        out.object_key, expected_cas_key,
        "新模板 CAS key 必须两段：prefix + sha16_<safe_filename>"
    );

    // copy_object 应被调用：src=tmp_key, dst=新 cas_key
    let copy_calls = cos
        .copy_calls
        .lock()
        .unwrap()
        .iter()
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(copy_calls.len(), 1, "copy_object 应被调用 1 次");
    assert_eq!(copy_calls[0].0, tmp_key);
    assert_eq!(copy_calls[0].1, expected_cas_key);
}

#[tokio::test]
async fn read_fallback_to_legacy_key_test() {
    // 2026-09-29 新增：DB 历史行 object_key 是五段（legacy），但 COS 桶里真实
    // 对象按新模板存；`resolve_effective_key` 必须能解析 legacy 并按新模板重写
    // 后 head 命中。
    let cos = Arc::new(MockCos::new());

    // DB 行 object_key（历史五段）：prefix= uploads/, owner_kind=part, owner_id=123,
    // kind=DRAWING, sha16=abcdef0123456789, safe_filename=old_drawing.pdf
    let db_key = "uploads/part/123/DRAWING/abcdef0123456789_old_drawing.pdf";
    // COS 桶里真实对象（新两段）：prefix + sha16_<safe_filename>
    let new_key = "uploads/abcdef0123456789_old_drawing.pdf";
    // 只注册新模板 key 的 head；DB 五段 key 不注册 → resolve_effective_key 走 fallback
    cos.set_head(new_key, 2048);

    let resolved = PartFileService::resolve_effective_key(cos.as_ref(), "uploads/", db_key)
        .await
        .expect("resolve_effective_key 应返回 Ok");
    assert_eq!(
        resolved, new_key,
        "DB 五段 key 找不到 + 新模板 key 存在 → 应回退到新模板 key"
    );

    // 反向断言：新模板 key 直接传入 resolve，应直接返回（不尝试 fallback）
    let resolved_direct = PartFileService::resolve_effective_key(cos.as_ref(), "uploads/", new_key)
        .await
        .expect("resolve_effective_key 应返回 Ok");
    assert_eq!(
        resolved_direct, new_key,
        "新模板 key 直接命中，无需 fallback"
    );
}

// ===== 2026-10-10 新增：工控机账号（SHELF_ACCOUNT）的图纸只读权限 =====
//
// 背景：报工台（前端路由 `/scan/*`）的路由守卫就是 `SHELF_ACCOUNT` —— 车间工控机
// 用的就是这个角色账号，它的图纸预览要打本域的**列表** `GET /api/v2/part-files` 与
// **内容** `GET /api/v2/part-files/{id}/content`。这两条原本只放 4 角色
// （Manager / Clerk / Inspector / CncProgrammer）⇒ 工控机点预览必然 40300。
//
// 守卫矩阵的决策与理由见 `src/modules/files/part_file/mod.rs` 模块 doc。关键一条：
// **`GET /files/objects/{id}/url` 默认矩阵不含 SHELF_ACCOUNT**（2026-10-14 起守卫是
// 权限点 `part.drawing.print`）—— 它回 COS 预签直链（1 小时有效、拿到即可脱离本
// 后端直接 GET），而 `content` 是后端代理、每次过 RBAC。

//
// ⚠️ 上面两条 URL 写的是 2026-10-10 加角色时的历史路径；2026-10-13 起已硬切为
// `GET /api/v2/files/objects/{id}/url` 与 `GET /api/v2/files/objects/{id}/content`
// （**无 alias**），角色矩阵逐字未变。
//
// ⚠️ 已知安全面（产品已拍板的取舍）：owner 是多态的（PART / ASSEMBLY）+ 客户端可
// 任意指定 `owner_id`，放开后 SHELF_ACCOUNT 能拉任意 `owner_id` 的文件，没有货架 /
// 工单级收窄。本域的权限模型止步于角色，不含 `CurrentUser.shelf_ids` 那一层。
//
// 下面的 `CurrentUser` 用 `test_current_user_with_roles` 造（`shelf_ids: vec![]`、
// `shelf_wildcard: false`）：本域的白名单是**纯角色判定**（`require_any_role`），
// 不读货架范围字段，故无需造带 scope 的真实工控机账号。

/// SHELF_ACCOUNT 能列文件（报工台预览的前置拉取）。
#[tokio::test]
async fn rbac_shelf_account_can_list_files() {
    let pool = setup().await;
    let l1 = insert_l1_customer(&pool, "客户PF-ShelfList", "F").await;
    let l2 = insert_l2_customer(&pool, "子客PF-ShelfList", l1).await;
    let part_id = insert_part_for_owner(&pool, l2).await;

    let cos = Arc::new(NoopCos);
    // 先由 MANAGER 造一个 DRAWING（写路径对 SHELF_ACCOUNT 仍是拒的，见下方用例）
    let mut tx = pool.begin().await.unwrap();
    PartFileService::new(shared_test_snowflake().clone(), cos.clone())
        .upload_file_for_owner(
            &mut *tx,
            UPLOAD_UPLOAD_PREFIX,
            UPLOAD_MAX_FILE_SIZE,
            "PART",
            part_id,
            "DRAWING",
            "shelf.pdf",
            "application/pdf",
            b"%PDF-1.5\nshelf\n%%EOF".to_vec(),
            &test_current_user_with_roles(vec![Role::Manager]),
        )
        .await
        .expect("MANAGER 上传应成功");
    tx.commit().await.unwrap();

    let shelf_account = test_current_user_with_roles(vec![Role::ShelfAccount]);
    let query = hsh_erp_rust::modules::files::part_file::dto::PartFileListQuery {
        owner_kind: Some("PART".into()),
        owner_id: Some(part_id.to_string()),
        kind: Some("DRAWING".into()),
        limit: Some(50),
        offset: Some(0),
    };
    let mut tx = pool.begin().await.unwrap();
    let out = PartFileService::new(shared_test_snowflake().clone(), cos.clone())
        .list_files(&mut *tx, &query, &shelf_account)
        .await
        .expect("SHELF_ACCOUNT 应能列文件（2026-10-10 放开）");
    drop(tx);

    assert_eq!(out.items.len(), 1, "应看到那条 DRAWING");
    assert_eq!(out.items[0].kind, "DRAWING");
    assert_eq!(out.total, 1);
}

/// SHELF_ACCOUNT 能读文件内容（报工台预览的实际取字节动作）。
#[tokio::test]
async fn rbac_shelf_account_can_get_file_content() {
    let pool = setup().await;
    let l1 = insert_l1_customer(&pool, "客户PF-ShelfContent", "F").await;
    let l2 = insert_l2_customer(&pool, "子客PF-ShelfContent", l1).await;
    let part_id = insert_part_for_owner(&pool, l2).await;

    let cos = Arc::new(NoopCos);
    let mut tx = pool.begin().await.unwrap();
    let uploaded = PartFileService::new(shared_test_snowflake().clone(), cos.clone())
        .upload_file_for_owner(
            &mut *tx,
            UPLOAD_UPLOAD_PREFIX,
            UPLOAD_MAX_FILE_SIZE,
            "PART",
            part_id,
            "DRAWING",
            "shelf-content.pdf",
            "application/pdf",
            b"%PDF-1.5\nshelf\n%%EOF".to_vec(),
            &test_current_user_with_roles(vec![Role::Manager]),
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();

    let shelf_account = test_current_user_with_roles(vec![Role::ShelfAccount]);
    // ⚠️ 2026-10-16 review 第 2 轮 MINOR-A：同上，取行与取字节走生产同款的两步。
    let svc = PartFileService::new(shared_test_snowflake().clone(), cos.clone());
    let mut tx = pool.begin().await.unwrap();
    let row = svc.get_file_row(&mut *tx, uploaded.id).await.unwrap();
    let content = svc
        .get_file_content_of_checked_row(cos.as_ref(), UPLOAD_UPLOAD_PREFIX, &row, &shelf_account)
        .await
        .expect("SHELF_ACCOUNT 应能读内容（2026-10-10 放开）");
    drop(tx);

    assert_eq!(
        content.content_type.as_deref(),
        Some("application/pdf"),
        "content_type 透传"
    );
    // NoopCos.get_object 返回空字节（本用例只守 RBAC 放行 + content_type 透传）
    assert!(content.bytes.is_empty(), "NoopCos 不返真实字节");
}

/// SHELF_ACCOUNT 调 `/part-files/{id}/url` **仍被拒**：`part.drawing.print` 的默认
/// 矩阵不含它（COS 预签直链不在放开范围）。
#[tokio::test]
async fn rbac_shelf_account_get_file_url_still_forbidden() {
    let pool = setup().await;
    let l1 = insert_l1_customer(&pool, "客户PF-ShelfURL", "F").await;
    let l2 = insert_l2_customer(&pool, "子客PF-ShelfURL", l1).await;
    let part_id = insert_part_for_owner(&pool, l2).await;

    let cos = Arc::new(NoopCos);
    let mut tx = pool.begin().await.unwrap();
    let uploaded = PartFileService::new(shared_test_snowflake().clone(), cos.clone())
        .upload_file_for_owner(
            &mut *tx,
            UPLOAD_UPLOAD_PREFIX,
            UPLOAD_MAX_FILE_SIZE,
            "PART",
            part_id,
            "DRAWING",
            "shelf-url.pdf",
            "application/pdf",
            b"%PDF-1.5\n".to_vec(),
            &test_current_user_with_roles(vec![Role::Manager]),
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();

    let shelf_account = test_current_user_with_roles(vec![Role::ShelfAccount]);
    let mut tx = pool.begin().await.unwrap();
    let err = PartFileService::new(shared_test_snowflake().clone(), cos.clone())
        .get_file_with_url(
            &mut *tx,
            cos.clone(),
            UPLOAD_UPLOAD_PREFIX,
            UPLOAD_PRESIGN_EXPIRE,
            uploaded.id,
            &shelf_account,
        )
        .await
        .expect_err("预签直链不放开给 SHELF_ACCOUNT");
    drop(tx);

    match err {
        AppError::Biz { code, .. } => assert_eq!(
            code, 40300,
            "FORBIDDEN：`/url` 回的是 1 小时有效的 COS 直链，默认矩阵不含 SHELF_ACCOUNT"
        ),
        other => panic!("期望 AppError::Biz(40300)，got {other:?}"),
    }
}

/// SHELF_ACCOUNT 对**写路径**仍被拒：上传 + 软删。
///
/// 这是「只放开只读」这条边界的守门用例 —— 有人日后往白名单里加 ShelfAccount 时，
/// 写端点若被顺手一起放开，本用例当场红。
#[tokio::test]
async fn rbac_shelf_account_cannot_upload_or_delete() {
    let pool = setup().await;
    let l1 = insert_l1_customer(&pool, "客户PF-ShelfWrite", "F").await;
    let l2 = insert_l2_customer(&pool, "子客PF-ShelfWrite", l1).await;
    let part_id = insert_part_for_owner(&pool, l2).await;

    let cos = Arc::new(NoopCos);
    let shelf_account = test_current_user_with_roles(vec![Role::ShelfAccount]);

    // 上传：40300
    let mut tx = pool.begin().await.unwrap();
    let up_err = PartFileService::new(shared_test_snowflake().clone(), cos.clone())
        .upload_file_for_owner(
            &mut *tx,
            UPLOAD_UPLOAD_PREFIX,
            UPLOAD_MAX_FILE_SIZE,
            "PART",
            part_id,
            "DRAWING",
            "shelf-write.pdf",
            "application/pdf",
            b"%PDF-1.5\n".to_vec(),
            &shelf_account,
        )
        .await
        .expect_err("SHELF_ACCOUNT 无上传权限");
    drop(tx);
    match up_err {
        AppError::Biz { code, .. } => assert_eq!(code, 40300, "上传：FORBIDDEN"),
        other => panic!("上传：期望 AppError::Biz(40300)，got {other:?}"),
    }

    // 软删：先由 MANAGER 造一个 DRAWING，SHELF_ACCOUNT 去删应 40300
    let mut tx = pool.begin().await.unwrap();
    let uploaded = PartFileService::new(shared_test_snowflake().clone(), cos.clone())
        .upload_file_for_owner(
            &mut *tx,
            UPLOAD_UPLOAD_PREFIX,
            UPLOAD_MAX_FILE_SIZE,
            "PART",
            part_id,
            "DRAWING",
            "shelf-write.pdf",
            "application/pdf",
            b"%PDF-1.5\n".to_vec(),
            &test_current_user_with_roles(vec![Role::Manager]),
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();

    let mut tx = pool.begin().await.unwrap();
    let del_err = PartFileService::new(shared_test_snowflake().clone(), cos.clone())
        .soft_delete_file(&mut *tx, uploaded.id, 0, &shelf_account)
        .await
        .expect_err("SHELF_ACCOUNT 无软删权限");
    drop(tx);
    match del_err {
        AppError::Biz { code, .. } => assert_eq!(code, 40300, "软删：FORBIDDEN"),
        other => panic!("软删：期望 AppError::Biz(40300)，got {other:?}"),
    }
}

// ===========================================================================
//  2026-10-13 files 域重组硬切守卫：旧前缀 404、无 alias
//
//  三条用例的共同判据是 **HTTP 404**，且不能被 40100 / 40300 / 400 解释掉：
//
//  - 不是 40100：`v2_router` 的 `authenticate_middleware` 是 `route_layer`（只作用于
//    已匹配路由），路由匹配阶段就落空的请求不会被鉴权吃掉（见
//    `tests/iam/middleware.rs` 的同款不变量用例）。三条都带合法 MANAGER token。
//  - 不是 40300：同样带 MANAGER token，且守卫在 service 层、真落进端点必 200/4xx 业务码。
//  - 不是 400：`iam::shelf` 迁移时踩过「旧路径带前缀仍落在 `/iam/shelves/{id}` 的
//    `Path<i64>` 提取器 ⇒ 400 纯文本」的坑 —— 那里是**部分前缀**残留导致的 catch-all
//    命中。本域是整段前缀（`/part-files` + `/cnc-programs`）从 `v2_router` 上删除，
//    连 catch-all 都没有，故必须是干净的 404。若哪天有人「贴心地」加一个
//    `/part-files/*path` 之类的兜底，这三条会当场红。
//
//  ⚠️ 用 `send_raw`（不解析 JSON）而不是 `send`：404 的 body 是**空**，
//  `send` 的 JSON 解析会 panic（`EOF while parsing a value`）。与 `tests/iam/shelf.rs`
//  的 404 守卫同款做法。
//
//  ⚠️ 每条都先造一条**真实** `t_part_file` 行，让「404」不能被「资源本来就不存在」
//  解释掉（`tests/iam/shelf.rs::old_shelf_paths_are_gone` 的同款取证手法）。
// ===========================================================================

/// 为三条 404 守卫共用的 bootstrap：造真实 part + 真实 t_part_file 行 + MANAGER token。
///
/// 返回 `(pool, app, token, file_id, part_id)`：
/// - `file_id` 是真实存在的那条文件的雪花 id（404 守卫用它拼旧 URL）；
/// - `part_id` 是那条文件的 **owner id**（`owner_kind='PART'`）—— 列表端点的
///   `?owner_kind=PART&owner_id=…` 必须传它，**不是** `file_id`（两者都是雪花，
///   传错不会报错、只会让「正向对照」这条断言悄悄失去意义）。
async fn bootstrap_hardcut_guard() -> (PgPool, axum::Router, String, i64, i64) {
    let pool = test_pool().await;
    let fx = load_part_fixture(&pool).await;
    let app = test_app(test_state(pool.clone()).await);
    let token = login_token(&app, &fx.manager_username, PartFixture::PASSWORD).await;

    let l1 = insert_l1_customer(&pool, "客户PF-HardCut", "F").await;
    let l2 = insert_l2_customer(&pool, "子客PF-HardCut", l1).await;
    let part_id = insert_part_for_owner(&pool, l2).await;

    let cos = Arc::new(NoopCos);
    let mut tx = pool.begin().await.unwrap();
    let out = PartFileService::new(shared_test_snowflake().clone(), cos.clone())
        .upload_file_for_owner(
            &mut *tx,
            UPLOAD_UPLOAD_PREFIX,
            UPLOAD_MAX_FILE_SIZE,
            "PART",
            part_id,
            "DRAWING",
            "hardcut.pdf",
            "application/pdf",
            b"%PDF-1.5\nhardcut\n%%EOF".to_vec(),
            &test_current_user_with_roles(vec![Role::Manager]),
        )
        .await
        .expect("MANAGER 上传应成功（保证 404 不是「资源不存在」造成的）");
    tx.commit().await.unwrap();

    (pool, app, token, out.id, part_id)
}

/// `GET /api/v2/part-files`（旧前缀整段）→ 404。
#[tokio::test]
async fn old_part_files_prefix_returns_404() {
    let (_pool, app, token, file_id, part_id) = bootstrap_hardcut_guard().await;

    // 先钉正向对照：新路径对同一个 file_id 必须 200 —— 证明守卫打的确实是旧前缀，
    // 而不是「这个域整体坏了」。
    // ⚠️ 用 `send_raw`：`/content` 回的是二进制流（测试里是 NoopCos 的空字节），
    // 不是 JSON 信封，`send` 会 panic。
    let (new_status, new_raw) = send_raw(
        app.clone(),
        json_request(
            "GET",
            &format!("/files/objects/{file_id}/content"),
            None,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(
        new_status,
        StatusCode::OK,
        "新路径 GET /files/objects/{{id}}/content 应 200（正向对照）: {new_status} {new_raw:?}"
    );

    // 列表端点的正向对照：`GET /api/v2/files/objects`（**无尾斜杠**）必须命中。
    //
    // ⚠️ 尾斜杠形态（`/files/objects/`）是 404 —— 这**不是**本次引入的行为：
    // axum 0.8 的 `nest` 把内层 `.route("/", …)` 注册成 `<prefix>` 本身（不带尾斜杠），
    // 迁移前的 `GET /api/v2/part-files/` 同样是 404（2026-10-13 用等价路由形状的
    // 对照探针实测过 old/new 两侧逐条一致）。任务书端点映射表里写的
    // 「`GET /api/v2/part-files/` → `GET /api/v2/files/objects/`」中的尾斜杠来自
    // 路由注册串 `.route("/", …)`，不是 wire 上的 URL；前端照旧用**无尾斜杠**即可。
    // 这里把两种写法都钉死，避免后人「顺手补个尾斜杠 alias」。
    //
    // ⚠️ `owner_id` 传的是 **part_id**（`owner_kind=PART` 下的 owner），不是上面那条
    // 文件自己的 `file_id` —— 两者都是雪花 id，传错**不会报错**（列表只按 owner 过滤，
    // 命不中就是空数组），只会让「正向对照」悄悄变成「拿空结果证明路由活着」。
    let (list_status, list_raw) = send_raw(
        app.clone(),
        json_request(
            "GET",
            &format!("/files/objects?owner_kind=PART&owner_id={part_id}"),
            None,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(
        list_status,
        StatusCode::OK,
        "新路径 GET /files/objects 应 200（正向对照）: {list_status} {list_raw}"
    );

    let (slash_status, slash_raw) = send_raw(
        app.clone(),
        json_request(
            "GET",
            &format!("/files/objects/?owner_kind=PART&owner_id={part_id}"),
            None,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(
        slash_status,
        StatusCode::NOT_FOUND,
        "尾斜杠形态 /files/objects/ 迁移前后都是 404（axum 0.8 nest 语义，未变）: {slash_status} {slash_raw}"
    );

    for method_path in [
        ("GET", "/part-files".to_string()),
        ("POST", "/part-files".to_string()),
        ("GET", format!("/part-files/{file_id}/url")),
        ("GET", format!("/part-files/{file_id}/content")),
        ("POST", format!("/part-files/{file_id}/delete")),
    ] {
        let (method, uri) = method_path;
        let body = if method == "GET" {
            None
        } else {
            Some(json!({}))
        };
        let (status, raw) =
            send_raw(app.clone(), json_request(method, &uri, body, Some(&token))).await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "旧前缀 {method} /api/v2{uri} 必须 404（硬切无 alias）: {status} {raw}"
        );
    }
}

/// `GET /api/v2/part-files/{数字}/content`（旧前缀 + 数字段）→ 404，**不是** 400。
///
/// 这一条专门钉「旧前缀不落进任何 `Path<i64>` catch-all」：如果将来有人把
/// `/part-files` nest 加回来却只注册了 `/{file_id}/content` 之外的形状，这一条会
/// 先于其它用例报警。
#[tokio::test]
async fn old_part_files_numeric_path_returns_404() {
    let (_pool, app, token, _file_id, _part_id) = bootstrap_hardcut_guard().await;

    for path in [
        "/part-files/999999999999/content",
        "/part-files/abc/content",
        "/part-files/1/update",
        "/part-files/parts/1/files",
        // 2026-10-13：子模块挂载段曾一度写成 `/files/part-files/*`（把旧 URL 的
        // 路径段原样带进了新 nest）。此处钉死它**同样**不可达 —— 新 URL 只有
        // `/files/objects/*` 一个形状。
        "/files/part-files/999999999999/content",
        "/files/part-files",
    ] {
        let (status, raw) =
            send_raw(app.clone(), json_request("GET", path, None, Some(&token))).await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "GET {path} 应是干净的 404（不得落进 Path<i64> 变 400）: {status} {raw}"
        );
    }
}

/// `GET /api/v2/cnc-programs`（CNC 域旧前缀整段）→ 404。
#[tokio::test]
async fn old_cnc_programs_prefix_returns_404() {
    let (_pool, app, token, file_id, _part_id) = bootstrap_hardcut_guard().await;

    for method_path in [
        ("GET", "/cnc-programs".to_string()),
        ("POST", "/cnc-programs/pairs".to_string()),
        ("GET", format!("/cnc-programs/{file_id}/download-url")),
        ("GET", format!("/cnc-programs/{file_id}/content")),
        ("POST", format!("/cnc-programs/{file_id}/delete")),
    ] {
        let (method, uri) = method_path;
        let body = if method == "GET" {
            None
        } else {
            Some(json!({}))
        };
        let (status, raw) =
            send_raw(app.clone(), json_request(method, &uri, body, Some(&token))).await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "旧前缀 {method} /api/v2{uri} 必须 404（硬切无 alias）: {status} {raw}"
        );
    }
}

// ===========================================================================
//  2026-10-13 PR1b 硬切守卫：part / assembly 侧的文件端点整体下线且无 alias
//
//  与上方 PR1a 的三条守卫同源：整段路径消失后**不得落进 catch-all 变 400**。
//  判据仍是 HTTP 404，且不能被 40100 / 40300 / 400 解释掉 —— 四条都带合法
//  MANAGER token，且先造出真实的 part / assembly / t_part_file 行，保证「404」
//  不是「资源本来就不存在」造成的。
//
//  覆盖的 5 条旧 URL（`POST|GET /assemblies/{id}/files` 一条端点两种方法）：
//  - POST /api/v2/parts/{数字}/upload-drawing
//  - POST /api/v2/parts/{数字}/upload-3d-model
//  - POST /api/v2/parts/{数字}/files/confirm
//  - POST /api/v2/assemblies/{数字}/files
//  - GET  /api/v2/assemblies/{数字}/files
//
//  ⚠️ 其中 `POST /parts/{id}/files/confirm` 是 4 段、`upload-*` 是 3 段，而 part 域
//  的 `/{part_id}` catch-all 只有 **2 段**（`/{part_id}/xxx`），段数够不着 ⇒ matchit
//  无命中 ⇒ 干净 404。assembly 域的 `/{assembly_id}/files` 是 3 段，**曾经**注册过，
//  本轮整条从 router 删除后同样落空。若哪天有人加回 catch-all，这四条会当场红。
// ===========================================================================

/// 为 PR1b 守卫共用的 bootstrap：真实 part + **真实 assembly** + 真实 t_part_file
/// 行 + MANAGER token。
///
/// 比 `bootstrap_hardcut_guard` 多造一个 `t_assembly`：assembly 的两条旧端点挂在
/// 装配件 id 上，没有真实装配件行时「404」可能被误读成「路由对了、实体没了」。
async fn bootstrap_pr1b_hardcut_guard() -> (PgPool, axum::Router, String, i64, i64) {
    use hsh_erp_rust::infra::clock::now_naive;
    let pool = test_pool().await;
    let fx = load_part_fixture(&pool).await;
    let app = test_app(test_state(pool.clone()).await);
    let token = login_token(&app, &fx.manager_username, PartFixture::PASSWORD).await;

    let l1 = insert_l1_customer(&pool, "客户PF-HardCutB", "F").await;
    let l2 = insert_l2_customer(&pool, "子客PF-HardCutB", l1).await;
    let part_id = insert_part_for_owner(&pool, l2).await;

    // 真实装配件行（直接 SQL；assembly 的建单链要 serial_counter，不属本用例关注点）
    let asm_id = shared_test_snowflake().next_id();
    let now = now_naive();
    let today = now.date();
    sqlx::query(
        "INSERT INTO t_assembly (id, drawing_no, name, customer_id, request_date, \
         planned_delivery_date, is_urgent, status, version, \
         created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, $3, $4, $5, $5, false, 'PENDING', 0, $6, NULL, $6, NULL)",
    )
    .bind(asm_id)
    .bind("DWG-HARDCUT-B")
    .bind("asm-hardcut-b")
    .bind(l2)
    .bind(today)
    .bind(now)
    .execute(&pool)
    .await
    .expect("insert t_assembly");

    // 再给装配件造一条真实的 ASSEMBLY_MASTER 文件行（owner_kind=ASSEMBLY）
    let cos = Arc::new(NoopCos);
    let mut tx = pool.begin().await.unwrap();
    PartFileService::new(shared_test_snowflake().clone(), cos)
        .upload_file_for_owner(
            &mut *tx,
            UPLOAD_UPLOAD_PREFIX,
            UPLOAD_MAX_FILE_SIZE,
            "ASSEMBLY",
            asm_id,
            "ASSEMBLY_MASTER",
            "hardcut-master.pdf",
            "application/pdf",
            b"%PDF-1.5\nhardcut master\n%%EOF".to_vec(),
            &test_current_user_with_roles(vec![Role::Manager]),
        )
        .await
        .expect("MANAGER 上传装配件 PDF 应成功");
    tx.commit().await.unwrap();

    (pool, app, token, part_id, asm_id)
}

/// `POST /api/v2/parts/{数字}/upload-drawing` → 404（并入 `POST /files/objects`）。
#[tokio::test]
async fn old_part_upload_drawing_returns_404() {
    let (_pool, app, token, part_id, _asm_id) = bootstrap_pr1b_hardcut_guard().await;

    // 正向对照：新端点必须存在（multipart 缺 body → 400/422 业务错误，而不是 404），
    // 证明守卫打的确实是「旧路径消失」而不是「本域整体坏了」。
    let (new_status, new_raw) = send_raw(
        app.clone(),
        json_request("POST", "/files/objects", Some(json!({})), Some(&token)),
    )
    .await;
    assert_ne!(
        new_status,
        StatusCode::NOT_FOUND,
        "新路径 POST /files/objects 必须命中路由: {new_status} {new_raw:?}"
    );

    let (status, raw) = send_raw(
        app.clone(),
        json_request(
            "POST",
            &format!("/parts/{part_id}/upload-drawing"),
            Some(json!({})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "旧端点 POST /api/v2/parts/{{part_id}}/upload-drawing 必须 404（硬切无 alias）: {status} {raw}"
    );
}

/// `POST /api/v2/parts/{数字}/upload-3d-model` → 404（并入 `POST /files/objects`）。
#[tokio::test]
async fn old_part_upload_3d_model_returns_404() {
    let (_pool, app, token, part_id, _asm_id) = bootstrap_pr1b_hardcut_guard().await;

    let (status, raw) = send_raw(
        app.clone(),
        json_request(
            "POST",
            &format!("/parts/{part_id}/upload-3d-model"),
            Some(json!({})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "旧端点 POST /api/v2/parts/{{part_id}}/upload-3d-model 必须 404（硬切无 alias）: {status} {raw}"
    );
}

/// `POST /api/v2/parts/{数字}/files/confirm` → 404（硬切到 `POST /files/objects/confirm`）。
#[tokio::test]
async fn old_part_files_confirm_returns_404() {
    let (_pool, app, token, part_id, _asm_id) = bootstrap_pr1b_hardcut_guard().await;

    // 正向对照：新 confirm 端点必须命中路由（缺字段 → 422 纯文本 / 业务错误）。
    let (new_status, new_raw) = send_raw(
        app.clone(),
        json_request(
            "POST",
            "/files/objects/confirm",
            Some(json!({})),
            Some(&token),
        ),
    )
    .await;
    assert_ne!(
        new_status,
        StatusCode::NOT_FOUND,
        "新端点 POST /files/objects/confirm 必须命中路由: {new_status} {new_raw:?}"
    );

    let (status, raw) = send_raw(
        app.clone(),
        json_request(
            "POST",
            &format!("/parts/{part_id}/files/confirm"),
            Some(json!({})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "旧端点 POST /api/v2/parts/{{part_id}}/files/confirm 必须 404（硬切无 alias）: {status} {raw}"
    );
}

/// `POST|GET /api/v2/assemblies/{数字}/files` → **均** 404（上传并入
/// `POST /files/objects`；列表等价于 `GET /files/objects?owner_kind=ASSEMBLY&…`）。
#[tokio::test]
async fn old_assembly_files_returns_404() {
    let (_pool, app, token, _part_id, asm_id) = bootstrap_pr1b_hardcut_guard().await;

    // 正向对照：等价的读端点（query 形态）必须 200，证明「装配件文件仍可读」这件事
    // 没被端点下线误伤 —— 404 守的是**旧路径**，不是这个能力。
    let (list_status, list_raw) = send_raw(
        app.clone(),
        json_request(
            "GET",
            &format!("/files/objects?owner_kind=ASSEMBLY&owner_id={asm_id}"),
            None,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(
        list_status,
        StatusCode::OK,
        "新路径 GET /files/objects?owner_kind=ASSEMBLY 应 200（正向对照）: {list_status} {list_raw}"
    );

    for method in ["POST", "GET"] {
        let body = if method == "GET" {
            None
        } else {
            Some(json!({}))
        };
        let (status, raw) = send_raw(
            app.clone(),
            json_request(
                method,
                &format!("/assemblies/{asm_id}/files"),
                body,
                Some(&token),
            ),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "旧端点 {method} /api/v2/assemblies/{{assembly_id}}/files 必须 404（硬切无 alias）: {status} {raw}"
        );
    }
}

// ===========================================================================
//  2026-10-13 review 第 1 轮 MINOR-7 / MINOR-6：ASSEMBLY owner 的内容级覆盖
//
//  MINOR-7：原 `tests/assembly/files_list.rs` 整文件删除后，「按 owner_kind=ASSEMBLY
//  列出 + 按 kind 过滤」的**内容**断言归零 —— 替代覆盖只有硬切守卫里一条
//  `assert_eq!(list_status, OK)` 的**状态码**正向对照（`send_raw` 不解 JSON，压根没看
//  items），而 `list_filter_by_kind` 走的是 PART owner。下面补回内容断言。
//
//  MINOR-6：`ASSEMBLY_MASTER` 行原先能被上传、永远删不掉（soft_delete 的 kind 分派
//  缺口，本轮已闭合）。补一条软删回归，把「能造出来的 kind 就得能删」钉死。
// ===========================================================================

/// 造一条最小 `t_assembly` 行（直写 SQL：assembly 建单链要 serial_counter，
/// 不属这两条用例的关注点）。
async fn insert_assembly_row(pool: &PgPool, label: &str, customer_id: i64) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;
    let id = shared_test_snowflake().next_id();
    let now = now_naive();
    let today = now.date();
    sqlx::query(
        "INSERT INTO t_assembly (id, drawing_no, name, customer_id, request_date, \
         planned_delivery_date, is_urgent, status, version, \
         created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, $3, $4, $5, $5, false, 'PENDING', 0, $6, NULL, $6, NULL)",
    )
    .bind(id)
    .bind(format!("DWG-{label}"))
    .bind(format!("asm-{label}"))
    .bind(customer_id)
    .bind(today)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_assembly");
    id
}

fn asm_list_query(
    owner_kind: &str,
    owner_id: i64,
    kind: Option<&str>,
) -> hsh_erp_rust::modules::files::part_file::dto::PartFileListQuery {
    hsh_erp_rust::modules::files::part_file::dto::PartFileListQuery {
        owner_kind: Some(owner_kind.into()),
        owner_id: Some(owner_id.to_string()),
        kind: kind.map(str::to_string),
        limit: Some(50),
        offset: Some(0),
    }
}

/// `list_files` 的薄壳（service 直调 + 自开自提交 tx），供上面两条用例复用。
async fn asm_list(
    pool: &PgPool,
    cos: &Arc<NoopCos>,
    current: &CurrentUser,
    q: hsh_erp_rust::modules::files::part_file::dto::PartFileListQuery,
) -> hsh_erp_rust::modules::files::part_file::vo::PartFileListOut {
    let mut tx = pool.begin().await.unwrap();
    let out = PartFileService::new(shared_test_snowflake().clone(), cos.clone())
        .list_files(&mut *tx, &q, current)
        .await
        .unwrap();
    drop(tx);
    out
}

/// `owner_kind=ASSEMBLY` 的列表**内容**断言 + 按 kind 过滤。
///
/// MINOR-7 的替代覆盖。用两个装配件：A 传过 `ASSEMBLY_MASTER`，B 什么都没有。
///
/// ⚠️ **本用例刻意不覆盖「跨 owner_kind 隔离」**：`t_part_file` 没有 owner 类型列，
/// `owner_id` 装在 `part_id` 列里，`list_with_filters` 的 `owner_kind` 形参被显式
/// 忽略（见 `src/modules/files/part_file/repo/sql.rs` 该函数上方的注释：
/// 「owner_kind 仅作语义标签，不参与 SQL」）⇒ 拿 ASSEMBLY 的 id 当 `owner_kind=PART`
/// 查，当前**仍会返回该行**。这是 schema 层的既有形状、不是本 PR 引入的，本 PR 也
/// 没改删除/查询路径；要真隔离得给 `t_part_file` 加 owner 类型列（独立立项）。
#[tokio::test]
async fn list_assembly_owner_filters_by_kind_and_owner() {
    let pool = setup().await;
    let l1 = insert_l1_customer(&pool, "客户PF-AsmList", "F").await;
    let l2 = insert_l2_customer(&pool, "子客PF-AsmList", l1).await;
    let asm_a = insert_assembly_row(&pool, "ASM-LIST-A", l2).await;
    let asm_b = insert_assembly_row(&pool, "ASM-LIST-B", l2).await;

    let current = test_current_user_with_roles(vec![Role::Manager]);
    let cos = Arc::new(NoopCos);

    let mut tx = pool.begin().await.unwrap();
    let out = PartFileService::new(shared_test_snowflake().clone(), cos.clone())
        .upload_file_for_owner(
            &mut *tx,
            UPLOAD_UPLOAD_PREFIX,
            UPLOAD_MAX_FILE_SIZE,
            "ASSEMBLY",
            asm_a,
            "ASSEMBLY_MASTER",
            "master-a.pdf",
            "application/pdf",
            b"%PDF-1.5\nasm list a\n%%EOF".to_vec(),
            &current,
        )
        .await
        .expect("装配件上传应成功");
    tx.commit().await.unwrap();

    // ① 按 owner_kind=ASSEMBLY + owner_id 命中，且 items 内容正确
    let r = asm_list(
        &pool,
        &cos,
        &current,
        asm_list_query("ASSEMBLY", asm_a, None),
    )
    .await;
    assert_eq!(r.items.len(), 1, "A 应恰好列出 1 条");
    assert_eq!(r.total, 1);
    let item = &r.items[0];
    assert_eq!(item.id, out.id);
    assert_eq!(
        item.owner_kind, "ASSEMBLY",
        "出参 owner_kind 应回显 ASSEMBLY"
    );
    assert_eq!(item.owner_id, asm_a, "出参 owner_id 应是装配件 id");
    assert_eq!(item.kind, "ASSEMBLY_MASTER");
    assert_eq!(item.original_filename, "master-a.pdf");

    // ② 没有文件的装配件 → 空列表（不能被别的 owner 的行污染）
    assert_eq!(
        asm_list(
            &pool,
            &cos,
            &current,
            asm_list_query("ASSEMBLY", asm_b, None)
        )
        .await
        .items
        .len(),
        0
    );

    // ③ 按 kind 过滤：命中 / 不命中
    assert_eq!(
        asm_list(
            &pool,
            &cos,
            &current,
            asm_list_query("ASSEMBLY", asm_a, Some("ASSEMBLY_MASTER"))
        )
        .await
        .items
        .len(),
        1
    );
    assert_eq!(
        asm_list(
            &pool,
            &cos,
            &current,
            asm_list_query("ASSEMBLY", asm_a, Some("DRAWING"))
        )
        .await
        .items
        .len(),
        0,
        "装配件上不该有 DRAWING 行"
    );

    // ④ ⚠️ **不做**跨 owner_kind 隔离断言，理由见函数 doc —— `owner_kind` 不参与
    //    SQL（`repo/sql.rs::list_with_filters` 的 `_owner_kind` 形参被显式忽略），
    //    所以「拿 ASSEMBLY 的 id 当 PART 查」当前仍会返回该行。断言它返回 0 是
    //    断言一个**不存在**的行为；断言它返回 1 等于给一条已知偏差盖合法章。
    //    正确做法是等 schema 有 owner 侧类型列之后再补隔离断言（独立立项）。
}

/// `ASSEMBLY_MASTER` 行可被软删（MINOR-6 回归）。
///
/// 闭合前这条会拿到 21102 `BIZ_PART_FILE_BAD_TYPE`（kind 不可软删）—— 也就是
/// 「能造出来的 kind 却删不掉」。同时钉住角色面：Manager 可删（与 DRAWING 同款 M+C）。
#[tokio::test]
async fn soft_delete_assembly_master_happy_path() {
    let pool = setup().await;
    let l1 = insert_l1_customer(&pool, "客户PF-AsmDel", "F").await;
    let l2 = insert_l2_customer(&pool, "子客PF-AsmDel", l1).await;
    let asm_id = insert_assembly_row(&pool, "ASM-DEL", l2).await;

    let current = test_current_user_with_roles(vec![Role::Manager]);
    let cos = Arc::new(NoopCos);

    let mut tx = pool.begin().await.unwrap();
    let out = PartFileService::new(shared_test_snowflake().clone(), cos.clone())
        .upload_file_for_owner(
            &mut *tx,
            UPLOAD_UPLOAD_PREFIX,
            UPLOAD_MAX_FILE_SIZE,
            "ASSEMBLY",
            asm_id,
            "ASSEMBLY_MASTER",
            "master-del.pdf",
            "application/pdf",
            b"%PDF-1.5\nasm del\n%%EOF".to_vec(),
            &current,
        )
        .await
        .expect("装配件上传应成功");
    tx.commit().await.unwrap();

    let mut tx = pool.begin().await.unwrap();
    let object_key = PartFileService::new(shared_test_snowflake().clone(), cos.clone())
        .soft_delete_file(&mut *tx, out.id, out.version, &current)
        .await
        .expect("ASSEMBLY_MASTER 应可软删（与 DRAWING 同款 M+C 角色面）");
    tx.commit().await.unwrap();
    assert_eq!(object_key, out.object_key, "返回的应是软删行的 CAS key");

    // 取证：行真的软删了（get_by_id(deleted_at IS NULL) 查不到）
    let mut tx = pool.begin().await.unwrap();
    let after = PartFileService::new(shared_test_snowflake().clone(), cos.clone())
        .list_files(
            &mut *tx,
            &asm_list_query("ASSEMBLY", asm_id, None),
            &current,
        )
        .await
        .unwrap();
    drop(tx);
    assert_eq!(after.items.len(), 0, "软删后列表不应再有该行");
}

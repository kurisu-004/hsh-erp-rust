//! `files::part_file` 通用上传原语的集成测试（2026-10-13 PR1b 自
//! `tests/assembly/files.rs` 迁入）
//!
//! ## 为什么从 assembly binary 迁过来
//!
//! 端点 `POST /api/v2/assemblies/{assembly_id}/files` 已下线，上传并入
//! `POST /api/v2/files/objects`（四合一）。被测对象从
//! `AssemblyService::upload_assembly_files` 变成
//! `PartFileService::upload_file_for_owner`（`owner_kind=ASSEMBLY` /
//! `kind=ASSEMBLY_MASTER`），归属随端点一起搬进 files 域，故按「一域一份测试
//! binary」的约定迁到本 binary。`tests/assembly/lifecycle.rs` 保留其余 4 条
//! 生命周期用例（那是文件更名，不是搬家）。
//!
//! ## 集成策略
//! service 直调（不打 HTTP）：`pool.begin()` → `&mut *tx` 喂 service → 显式
//! `tx.commit()`。HTTP 层的 multipart 契约由 `part_file.rs` 的硬切守卫覆盖。
//!
//! ## 覆盖场景
//!   1. upload_assembly_master_happy_path — ASSEMBLY owner 上传 PDF → 落库 + 详情可见
//!   2. upload_assembly_master_ext_must_be_pdf — 扩展名 .step → 21102
//!   3. upload_assembly_master_cas_dedup — 同内容二次上传 → CAS 命中（同一 id）
//!   4. upload_unknown_owner_kind_rejected — owner_kind 不在 {PART, ASSEMBLY} → 20104

use std::sync::Arc;

use hsh_erp_rust::auth::rbac::{CurrentUser, Role};
use hsh_erp_rust::infra::clock::now_naive;
use hsh_erp_rust::infra::cos::NoopCos;
use hsh_erp_rust::modules::assembly::dto::AssemblyCreateRequest;
use hsh_erp_rust::modules::assembly::service::AssemblyService;
use hsh_erp_rust::modules::files::part_file::service::PartFileService;
use hsh_erp_rust::shared::error::AppError;
use sqlx::PgPool;

use hsh_erp_test_support::{
    AssemblyFixture, load_assembly_fixture, shared_test_snowflake, test_pool,
};

/// 2026-10-13：与 `part_file.rs` 的 `UPLOAD_UPLOAD_PREFIX` 同款常量 —— 那是
/// **测试自造**的 cfg 值（test-support 的 `test_state` 配 `uploads`），与
/// `config.rs` 的缺省 `obj/` 无关，两者互不干扰。
const UPLOAD_UPLOAD_PREFIX: &str = "uploads";

async fn setup() -> (PgPool, AssemblyFixture) {
    let pool = test_pool().await;
    let fx = load_assembly_fixture(&pool).await;
    (pool, fx)
}

async fn insert_l1_customer(pool: &PgPool, name: &str, prefix: &str) -> i64 {
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
    .expect("insert L1");
    id
}

async fn insert_l2_customer(pool: &PgPool, name: &str, l1_id: i64) -> i64 {
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

fn test_current_user(roles: Vec<Role>) -> CurrentUser {
    CurrentUser {
        id: 1,
        username: "asm_files_tester".into(),
        roles,
        shelf_ids: vec![],
        shelf_wildcard: false,
    }
}

/// 建一个不传 PDF 的装配件（建单端点仍会派发 serial_no，PDF 只用于页数校验）。
async fn seed_assembly(pool: &PgPool, suffix: &str) -> i64 {
    let l1 = insert_l1_customer(pool, &format!("客户AFU-{suffix}"), "F").await;
    let l2 = insert_l2_customer(pool, &format!("子客AFU-{suffix}"), l1).await;
    insert_serial_counter(pool, "F", 0).await;
    let mut tx = pool.begin().await.unwrap();
    let req = AssemblyCreateRequest {
        drawing_no: format!("D-AFU-{suffix}"),
        name: format!("asm-upload-{suffix}"),
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
    let asm = AssemblyService::create_assembly(
        &mut tx,
        shared_test_snowflake().as_ref(),
        &req,
        vec![],
        &current,
    )
    .await
    .expect("create assembly");
    tx.commit().await.unwrap();
    asm.assembly.id
}

fn pdf_bytes(tag: &str) -> Vec<u8> {
    format!("%PDF-1.5\n{tag}\n%%EOF").into_bytes()
}

/// ASSEMBLY owner 上传 `ASSEMBLY_MASTER` 成功，且 `GET /assemblies/{id}` 详情可见。
///
/// ⚠️ 这条同时守住了「端点下线不等于能力下线」：`GET /assemblies/{id}/files` 端点
/// 被删了，但**详情里的 `files[]` 仍在**（走 `service/crud.rs` 的
/// `list_part_files_by_owner`），两条是不同的路径。
#[tokio::test]
async fn upload_assembly_master_happy_path() {
    let (pool, _fx) = setup().await;
    let asm_id = seed_assembly(&pool, "OK").await;
    let cos = Arc::new(NoopCos);
    let current = test_current_user(vec![Role::Manager]);

    let mut tx = pool.begin().await.unwrap();
    let out = PartFileService::new(shared_test_snowflake().clone(), cos.clone())
        .upload_file_for_owner(
            &mut *tx,
            UPLOAD_UPLOAD_PREFIX,
            "ASSEMBLY",
            asm_id,
            "ASSEMBLY_MASTER",
            "master.pdf",
            "application/pdf",
            pdf_bytes("upload happy"),
            &current,
        )
        .await
        .expect("upload ok");
    tx.commit().await.unwrap();

    assert_eq!(out.kind, "ASSEMBLY_MASTER");
    assert_eq!(out.file_type, "PDF");
    assert_eq!(out.owner_kind, "ASSEMBLY");
    assert_eq!(out.owner_id, asm_id);
    assert_eq!(out.original_filename, "master.pdf");
    assert!(out.content_sha256.is_some());

    let mut tx = pool.begin().await.unwrap();
    let detail = AssemblyService::get_assembly(&mut tx, asm_id, &current)
        .await
        .unwrap();
    drop(tx);
    assert_eq!(detail.files.len(), 1, "详情 files[] 应看到刚上传的 PDF");
    assert_eq!(detail.files[0].id, out.id);
    assert_eq!(detail.files[0].original_filename, "master.pdf");
}

/// 非 PDF 扩展名 → 21102 `BIZ_PART_FILE_BAD_TYPE`。
#[tokio::test]
async fn upload_assembly_master_ext_must_be_pdf() {
    let (pool, _fx) = setup().await;
    let asm_id = seed_assembly(&pool, "EXT").await;
    let cos = Arc::new(NoopCos);
    let current = test_current_user(vec![Role::Manager]);

    let mut tx = pool.begin().await.unwrap();
    let err = PartFileService::new(shared_test_snowflake().clone(), cos.clone())
        .upload_file_for_owner(
            &mut *tx,
            UPLOAD_UPLOAD_PREFIX,
            "ASSEMBLY",
            asm_id,
            "ASSEMBLY_MASTER",
            "master.step",
            "application/octet-stream",
            b"some step data".to_vec(),
            &current,
        )
        .await
        .expect_err("非 PDF 扩展名应被拒");
    drop(tx);

    match err {
        AppError::Biz { code, .. } => assert_eq!(code, 21102, "BIZ_PART_FILE_BAD_TYPE"),
        other => panic!("期望 AppError::Biz(21102)，got {other:?}"),
    }
}

/// CAS 去重：同 owner + 同 kind + 同内容二次上传 → 返回**同一条**记录。
///
/// ⚠️ 这是 `upload_file_for_owner` 与被删的 `PartService::upload_part_file`
/// 判据不同带来的行为：`upload_file_for_owner` 按 **sha** 去重（省一次 COS PUT），
/// `upload_part_file` 按 `(owner, kind)` 去重（不看内容）。
#[tokio::test]
async fn upload_assembly_master_cas_dedup() {
    let (pool, _fx) = setup().await;
    let asm_id = seed_assembly(&pool, "CAS").await;
    let cos = Arc::new(NoopCos);
    let current = test_current_user(vec![Role::Manager]);
    let svc = PartFileService::new(shared_test_snowflake().clone(), cos.clone());

    let mut tx = pool.begin().await.unwrap();
    let first = svc
        .upload_file_for_owner(
            &mut *tx,
            UPLOAD_UPLOAD_PREFIX,
            "ASSEMBLY",
            asm_id,
            "ASSEMBLY_MASTER",
            "first.pdf",
            "application/pdf",
            pdf_bytes("identical"),
            &current,
        )
        .await
        .expect("first upload");
    tx.commit().await.unwrap();

    let mut tx = pool.begin().await.unwrap();
    let second = svc
        .upload_file_for_owner(
            &mut *tx,
            UPLOAD_UPLOAD_PREFIX,
            "ASSEMBLY",
            asm_id,
            "ASSEMBLY_MASTER",
            "second.pdf",
            "application/pdf",
            pdf_bytes("identical"),
            &current,
        )
        .await
        .expect("CAS 命中不应报错");
    tx.commit().await.unwrap();

    assert_eq!(first.id, second.id, "CAS 命中应返回已有记录");
    assert_eq!(first.object_key, second.object_key);
}

/// `owner_kind` 不在 `{PART, ASSEMBLY}` 白名单 → 20104 `BIZ_INVALID_VALUE`。
#[tokio::test]
async fn upload_unknown_owner_kind_rejected() {
    let (pool, _fx) = setup().await;
    let asm_id = seed_assembly(&pool, "KIND").await;
    let cos = Arc::new(NoopCos);
    let current = test_current_user(vec![Role::Manager]);

    let mut tx = pool.begin().await.unwrap();
    let err = PartFileService::new(shared_test_snowflake().clone(), cos.clone())
        .upload_file_for_owner(
            &mut *tx,
            UPLOAD_UPLOAD_PREFIX,
            "PART_FILE", // 既不是 PART 也不是 ASSEMBLY
            asm_id,
            "ASSEMBLY_MASTER",
            "master.pdf",
            "application/pdf",
            pdf_bytes("bad owner kind"),
            &current,
        )
        .await
        .expect_err("未知 owner_kind 应被拒");
    drop(tx);

    match err {
        AppError::Biz { code, .. } => assert_eq!(code, 20104, "BIZ_INVALID_VALUE"),
        other => panic!("期望 AppError::Biz(20104)，got {other:?}"),
    }
}

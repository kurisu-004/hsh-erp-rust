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
//! `tx.commit()`。
//!
//! ⚠️ 2026-10-13 review 第 1 轮 MINOR-4：service 直调**测不到** handler 层的
//! multipart 契约（字段分发 / `serde_json` 解析 / 雪花 id parse / 三条 20104 分支 /
//! 201 Created + 信封）。故本文件末尾另加了 3 条**打真实 HTTP 路由**的用例，
//! multipart body 由本地 `upload_request` 手工拼（test-support 无同名 helper，
//! 它不是 fixture，是请求构造）。
//!
//! ## 覆盖场景
//!   1. upload_assembly_master_happy_path — ASSEMBLY owner 上传 PDF → 落库 + 详情可见
//!   2. upload_assembly_master_ext_must_be_pdf — 扩展名 .step → 21102
//!   3. upload_assembly_master_cas_dedup — 同内容二次上传 → CAS 命中（同一 id）
//!   4. upload_unknown_owner_kind_rejected — owner_kind 不在 {PART, ASSEMBLY} → 20104
//!   5. upload_empty_bytes_rejected — 0 字节 → 40001（review 第 1 轮 MAJOR-1 补回）
//!   6. upload_over_max_file_size_returns_21103 — 超 `cfg_max_file_size` → 21103（同上）
//!   7. upload_cross_owner_kind_pair_rejected — PART + ASSEMBLY_MASTER 等错配 → 20104（MINOR-3）
//!   8. http_upload_happy_path_returns_201 — HTTP 层 happy path（MINOR-4）
//!   9. http_upload_non_numeric_owner_id_returns_20104 — `owner_id` 非数字（MINOR-4）
//!  10. http_upload_missing_file_returns_20104 — 缺 `file` 字段（MINOR-4）

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use hsh_erp_rust::auth::rbac::{CurrentUser, Role};
use hsh_erp_rust::infra::clock::now_naive;
use hsh_erp_rust::infra::cos::NoopCos;
use hsh_erp_rust::modules::assembly::dto::AssemblyCreateRequest;
use hsh_erp_rust::modules::assembly::service::AssemblyService;
use hsh_erp_rust::modules::files::part_file::service::PartFileService;
use hsh_erp_rust::shared::error::AppError;
use serde_json::json;
use sqlx::PgPool;

use hsh_erp_test_support::{
    AssemblyFixture, load_assembly_fixture, login_token, send, shared_test_snowflake, test_app,
    test_pool, test_state,
};

/// 2026-10-13：与 `part_file.rs` 的 `UPLOAD_UPLOAD_PREFIX` 同款常量 —— 那是
/// **测试自造**的 cfg 值（test-support 的 `test_state` 配 `uploads`），与
/// `config.rs` 的缺省 `obj/` 无关，两者互不干扰。
const UPLOAD_UPLOAD_PREFIX: &str = "uploads";

/// 上传 size 上限（2026-10-13 review 第 1 轮 MAJOR-1：`upload_file_for_owner` 新增收
/// `cfg_max_file_size` 形参，由 handler 传 `state.config.cos.max_file_size`）。
/// 与 `test_state` 的 `CosConfig.max_file_size`（300MB）逐字一致。
const UPLOAD_MAX_FILE_SIZE: usize = 300 * 1024 * 1024;

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
        permissions: Vec::new(),
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
            UPLOAD_MAX_FILE_SIZE,
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
            UPLOAD_MAX_FILE_SIZE,
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
            UPLOAD_MAX_FILE_SIZE,
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
            UPLOAD_MAX_FILE_SIZE,
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
            UPLOAD_MAX_FILE_SIZE,
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

/// **0 字节文件** → 40001 `VALIDATION_ERROR`（2026-10-13 review 第 1 轮 MAJOR-1）。
///
/// 这道守卫是被删的 `PartService::upload_part_file` 带过的，PR1b 四合一时丢了：
/// 丢了之后 `.pdf` 空文件能过 ext / content_type 两道校验 → `hash_bytes(&[])` →
/// `put_object` 写 0 字节对象 → INSERT `file_size = 0` 的行。
///
/// ⚠️ 断言的是 **40001 而不是 21103**：空文件是「无效输入」，不是「太大」，与被删
/// 实现的取舍逐字一致。
#[tokio::test]
async fn upload_empty_bytes_rejected() {
    let (pool, _fx) = setup().await;
    let asm_id = seed_assembly(&pool, "EMPTY").await;
    let cos = Arc::new(NoopCos);
    let current = test_current_user(vec![Role::Manager]);

    let mut tx = pool.begin().await.unwrap();
    let err = PartFileService::new(shared_test_snowflake().clone(), cos.clone())
        .upload_file_for_owner(
            &mut *tx,
            UPLOAD_UPLOAD_PREFIX,
            UPLOAD_MAX_FILE_SIZE,
            "ASSEMBLY",
            asm_id,
            "ASSEMBLY_MASTER",
            "master.pdf",
            "application/pdf",
            Vec::new(), // ← 0 字节
            &current,
        )
        .await
        .expect_err("0 字节应被拒");
    drop(tx);

    match err {
        AppError::Validation(msg) => assert!(
            msg.contains("字节为空"),
            "错误文案应指向空字节，实际：{msg}"
        ),
        other => panic!("期望 AppError::Validation(40001)，got {other:?}"),
    }

    // 取证：DB 里不能留下 file_size = 0 的行（守卫必须发生在 INSERT 之前）
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM t_part_file WHERE part_id = $1")
        .bind(asm_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(rows, 0, "被拒的上传不应留下任何 t_part_file 行");
}

/// **超过 `cfg_max_file_size`** → 21103 `BIZ_PART_FILE_TOO_LARGE`（同上，MAJOR-1）。
///
/// ⚠️ 上限**由形参传入**，所以这里传一个 16 字节的小值就能触发，不必真造 100MB 的
/// payload —— 这也顺带证明了「上限来自配置、不是写死的常量」。
///
/// 丢掉这道守卫的另一后果：21103 在 multipart 上传路径上**彻底不可达**
/// （`dto::validate::check_file_size` 的唯一调用方只剩 `bind_uploaded_file`）。
#[tokio::test]
async fn upload_over_max_file_size_returns_21103() {
    let (pool, _fx) = setup().await;
    let asm_id = seed_assembly(&pool, "BIG").await;
    let cos = Arc::new(NoopCos);
    let current = test_current_user(vec![Role::Manager]);
    const TINY_MAX: usize = 16;

    let mut tx = pool.begin().await.unwrap();
    let err = PartFileService::new(shared_test_snowflake().clone(), cos.clone())
        .upload_file_for_owner(
            &mut *tx,
            UPLOAD_UPLOAD_PREFIX,
            TINY_MAX,
            "ASSEMBLY",
            asm_id,
            "ASSEMBLY_MASTER",
            "master.pdf",
            "application/pdf",
            pdf_bytes("这段内容明确长于 16 字节上限"),
            &current,
        )
        .await
        .expect_err("超限应被拒");
    drop(tx);

    match err {
        AppError::Biz { code, .. } => assert_eq!(code, 21103, "BIZ_PART_FILE_TOO_LARGE"),
        other => panic!("期望 AppError::Biz(21103)，got {other:?}"),
    }
}

/// `owner_kind` × `kind` 错配 → 20104（2026-10-13 review 第 1 轮 MINOR-3）。
///
/// 四合一之前这层配对由各端点的 path 段结构性保证；合并后两个字段都来自 client，
/// 不加守卫就会造出 `part + ASSEMBLY_MASTER` 这种**永不可删**的行
/// （`soft_delete_file` 的 kind 分派原先不含 ASSEMBLY_MASTER，本轮已补上）。
///
/// 纯内存判断 ⇒ 不该碰 DB、不该有写入；用不存在的 owner id 也照样返回 20104，
/// 这正是「配对校验早于 owner 存在性校验」的可观测证据。
#[tokio::test]
async fn upload_cross_owner_kind_pair_rejected() {
    let (pool, _fx) = setup().await;
    let cos = Arc::new(NoopCos);
    let current = test_current_user(vec![Role::Manager]);

    for (owner_kind, kind, why) in [
        ("PART", "ASSEMBLY_MASTER", "零件挂装配件整图"),
        ("ASSEMBLY", "DRAWING", "装配件挂零件图纸"),
        ("ASSEMBLY", "3D_MODEL", "装配件挂 3D 模型"),
    ] {
        let mut tx = pool.begin().await.unwrap();
        // owner id 用一个必然不存在的值：若配对守卫没生效，报的就会是 21105
        // OWNER_NOT_FOUND 而不是 20104。
        let err = PartFileService::new(shared_test_snowflake().clone(), cos.clone())
            .upload_file_for_owner(
                &mut *tx,
                UPLOAD_UPLOAD_PREFIX,
                UPLOAD_MAX_FILE_SIZE,
                owner_kind,
                -1,
                kind,
                "x.pdf",
                "application/pdf",
                pdf_bytes("mismatched pair"),
                &current,
            )
            .await
            .expect_err("错配的 owner_kind × kind 应被拒");
        drop(tx);

        match err {
            AppError::Biz { code, .. } => assert_eq!(
                code, 20104,
                "{owner_kind}+{kind}（{why}）应得 20104 BIZ_INVALID_VALUE"
            ),
            other => panic!("{owner_kind}+{kind} 期望 AppError::Biz(20104)，got {other:?}"),
        }
    }
}

// ===========================================================================
//  2026-10-13 review 第 1 轮 MINOR-4：`POST /files/objects` 的 HTTP 层契约
//
//  service 直调测不到的部分全在这里：multipart 循环与 `data` / `file` 分发、
//  `serde_json::from_str`、`owner_id: String::parse::<i64>()`、缺 `data` / 缺
//  `file` / 缺文件名三条 20104 分支、以及 **HTTP 201 Created + `R<PartFileOut>`
//  信封**。角色守卫先于 multipart 解析这条纪律由既有 `part_file.rs` 的 RBAC 用例
//  （`rbac_inspector_can_upload_returns_403`）守着，不重复。
//
//  ⚠️ multipart body 由本文件本地拼：test-support 只提供 JSON 侧的
//  `json_request`，没有 multipart 版本的 helper。这是**请求构造**不是 fixture，
//  故不违反「新测试一律直接用 test-support 入口」那条例外清单的适用范围。
// ===========================================================================

/// multipart 边界（wire 上不可见，仅用于分隔 part）。
const MP_BOUNDARY: &str = "hshFilesBoundary7MA4YWxkTrun";

/// 手工拼一个 `multipart/form-data` body。
///
/// `file` 的四元组 = `(字段名, filename, content_type, 字节)`；`None` 即**不发**
/// 该字段（用来测「缺 file」分支）。⚠️ `filename` 也不能省 —— handler 用
/// `field.file_name()` 的 `None` 判「缺文件名」，那是另一条 20104 分支。
fn multipart_body(data: Option<&str>, file: Option<(&str, &str, &str, &[u8])>) -> Vec<u8> {
    let mut body: Vec<u8> = Vec::new();
    if let Some(d) = data {
        body.extend_from_slice(format!("--{MP_BOUNDARY}\r\n").as_bytes());
        body.extend_from_slice(b"Content-Disposition: form-data; name=\"data\"\r\n\r\n");
        body.extend_from_slice(d.as_bytes());
        body.extend_from_slice(b"\r\n");
    }
    if let Some((field, filename, content_type, bytes)) = file {
        body.extend_from_slice(format!("--{MP_BOUNDARY}\r\n").as_bytes());
        body.extend_from_slice(
            format!(
                "Content-Disposition: form-data; name=\"{field}\"; filename=\"{filename}\"\r\n"
            )
            .as_bytes(),
        );
        body.extend_from_slice(format!("Content-Type: {content_type}\r\n\r\n").as_bytes());
        body.extend_from_slice(bytes);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{MP_BOUNDARY}--\r\n").as_bytes());
    body
}

/// 构造一条打 `POST /api/v2/files/objects` 的 multipart 请求（带 MANAGER token）。
fn upload_request(
    data: Option<&str>,
    file: Option<(&str, &str, &str, &[u8])>,
    token: &str,
) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/files/objects")
        .header(
            "content-type",
            format!("multipart/form-data; boundary={MP_BOUNDARY}"),
        )
        .header("authorization", format!("Bearer {token}"))
        .body(Body::from(multipart_body(data, file)))
        .expect("build request")
}

/// HTTP 层 happy path：201 Created + `R<PartFileOut>` 信封 + `data` 键齐备。
///
/// 这是 MINOR-4 的核心断言 —— service 直调**看不到**状态码与信封，只能在这里钉。
#[tokio::test]
async fn http_upload_happy_path_returns_201() {
    let (pool, fx) = setup().await;
    let asm_id = seed_assembly(&pool, "HTTP").await;
    let app = test_app(test_state(pool.clone()).await);
    let token = login_token(&app, &fx.part_manager_username, AssemblyFixture::PASSWORD).await;

    let data = json!({
        "owner_kind": "ASSEMBLY",
        "owner_id": asm_id.to_string(), // 雪花 id 一律字符串（JS 精度约定）
        "kind": "ASSEMBLY_MASTER",
    })
    .to_string();

    let (status, envelope) = send(
        app.clone(),
        upload_request(
            Some(&data),
            Some((
                "file",
                "master.pdf",
                "application/pdf",
                &pdf_bytes("http happy"),
            )),
            &token,
        ),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::CREATED,
        "上传成功必须是 201 Created（旧 part 端点是 200），实际 {status}：{envelope}"
    );
    assert_eq!(envelope["code"], 0, "成功信封 code 应为 0：{envelope}");
    let out = &envelope["data"];
    assert_eq!(out["owner_kind"], "ASSEMBLY");
    assert_eq!(out["owner_id"], asm_id.to_string(), "owner_id 出参是字符串");
    assert_eq!(out["kind"], "ASSEMBLY_MASTER");
    assert_eq!(out["file_type"], "PDF");
    assert!(
        out["id"].is_string(),
        "雪花 id 出参必须是字符串（防 JS 精度截断）：{out}"
    );

    // 取证：HTTP 路径真的落了库（不是只回了个信封）
    let mut tx = pool.begin().await.unwrap();
    let detail =
        AssemblyService::get_assembly(&mut tx, asm_id, &test_current_user(vec![Role::Manager]))
            .await
            .unwrap();
    drop(tx);
    assert_eq!(detail.files.len(), 1);
    assert_eq!(detail.files[0].id.to_string(), out["id"]);
}

/// HTTP 层：`data.owner_id` 非数字 → 20104（雪花 id 解析分支）。
#[tokio::test]
async fn http_upload_non_numeric_owner_id_returns_20104() {
    let (pool, fx) = setup().await;
    let app = test_app(test_state(pool.clone()).await);
    let token = login_token(&app, &fx.part_manager_username, AssemblyFixture::PASSWORD).await;

    let data = json!({
        "owner_kind": "ASSEMBLY",
        "owner_id": "not-a-number",
        "kind": "ASSEMBLY_MASTER",
    })
    .to_string();

    let (status, envelope) = send(
        app.clone(),
        upload_request(
            Some(&data),
            Some((
                "file",
                "master.pdf",
                "application/pdf",
                &pdf_bytes("bad id"),
            )),
            &token,
        ),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST, "{envelope}");
    assert_eq!(envelope["code"], 20104, "owner_id 非法 → BIZ_INVALID_VALUE");
    assert!(
        envelope["message"].as_str().unwrap().contains("owner_id"),
        "错误文案应指向 owner_id：{envelope}"
    );
}

/// HTTP 层：缺 `file` 字段 → 20104（multipart 循环的 `file` 分发分支）。
#[tokio::test]
async fn http_upload_missing_file_returns_20104() {
    let (pool, fx) = setup().await;
    let asm_id = seed_assembly(&pool, "NOFILE").await;
    let app = test_app(test_state(pool.clone()).await);
    let token = login_token(&app, &fx.part_manager_username, AssemblyFixture::PASSWORD).await;

    let data = json!({
        "owner_kind": "ASSEMBLY",
        "owner_id": asm_id.to_string(),
        "kind": "ASSEMBLY_MASTER",
    })
    .to_string();

    let (status, envelope) = send(app.clone(), upload_request(Some(&data), None, &token)).await;

    assert_eq!(status, StatusCode::BAD_REQUEST, "{envelope}");
    assert_eq!(envelope["code"], 20104, "缺 file → BIZ_INVALID_VALUE");
    assert!(
        envelope["message"].as_str().unwrap().contains("file"),
        "错误文案应指向 file 字段：{envelope}"
    );

    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM t_part_file WHERE part_id = $1")
        .bind(asm_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(rows, 0, "被拒的请求不应留下 t_part_file 行");
}

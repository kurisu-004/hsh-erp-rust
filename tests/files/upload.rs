//! `POST /api/v2/files/objects/upload-intents` 集成测试（2026-10-15 新建）
//!
//! 被测端点是统一收文件链路的第 ② 步：业务提交成功后签发**预签 PUT URL**，
//! 前端拿它直传 COS（字节不过 rust 进程），再走第 ④ 步
//! `POST /files/objects/confirm` 登记。
//!
//! ## 为什么这三条打 HTTP、而不是 service 直调
//!
//! 本端点的契约面**大半在 HTTP 层**：批量 `items[]` 的反序列化、雪花 id 的
//! string 形态、`R<T>` 信封、`already_exists` / `upload_url` 的两态出参。service
//! 直调结构上测不到这些。
//!
//! ## 覆盖
//! 1. `intents_happy_path`                     — 正常签发：URL 含 object_key、content_type 与请求一致、expires_in > 0
//! 2. `intents_cas_hit_is_instant_upload`      — CAS 命中秒传：already_exists + existing_file + **不返回 upload_url**
//! 3. `intents_batch_echoes_client_ref_in_order` — 批量：`client_ref` 原样回显、顺序对应
//! 4. `intents_clerk_can_sign`                 — 角色：CLERK 可用（PART）
//! 5. `intents_inspector_and_shelf_rejected`   — 角色：INSPECTOR / SHELF_ACCOUNT 被拒（403）
//! 6. `intents_validation_matrix`              — sha 非 64hex / size=0 / size 超限 /
//!    扩展名与 kind 不匹配 / content_type 与 ext 不匹配 / owner 不存在 /
//!    owner_kind × kind 配对
//! 7. `intents_customer_delivery_template_*`   — CUSTOMER × DELIVERY_TEMPLATE：
//!    MANAGER 可、CLERK 403、绑**二级**客户被拒
//! 8. `confirm_content_type_mismatch_is_21102_like_upload_intents`
//!    —— 两条写入口对同一个 MIME 不匹配必须给**同一个**码（21102 / 400）
//! 9. `cad_2d_dwg_goes_through_the_whole_chain` —— CAD 源文件（DWG）整条链路可走通
//!    （`CAD_2D` 曾只缺写侧两张 policy 表，前端 CAD tab 传不上来）
//!
//! ## ⚠️ CAS 秒传那两条的零副作用断言
//!
//! 「命中就不签 URL」是秒传的**观测点**：`MockCos::put_url_calls` 必须**不增长**
//! （替身的 `presigned_put_url` 是纯内存签名，不发网络请求）。若哪天改成「命中也签」，
//! 用例当场红。

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;

use hsh_erp_rust::infra::cos::CosClient;
use hsh_erp_rust::modules::files::part_file::repo::PartFileRepoTrait;
use hsh_erp_test_support::{
    MockCos, PartFixture, load_part_fixture, login_token, send, test_app, test_pool, test_state,
    test_state_with_cos,
};

async fn setup() -> (PgPool, PartFixture) {
    let pool = test_pool().await;
    let fx = load_part_fixture(&pool).await;
    (pool, fx)
}

/// `MockCos` 注入的 app（签发 URL 的可观测点靠它；生产走 OpenDalCos）。
async fn app_with_mock_cos(pool: &PgPool) -> (axum::Router, Arc<MockCos>) {
    let cos = Arc::new(MockCos::new());
    let state = test_state_with_cos(pool.clone(), cos.clone()).await;
    (test_app(state), cos)
}

fn sha_of(bytes: &[u8]) -> String {
    hsh_erp_rust::modules::files::part_file::repo::hash_bytes(bytes)
}

/// 造一个 item 请求体（字段名与 `UploadIntentsIn` 逐字对齐）。
fn item(
    client_ref: &str,
    kind: &str,
    filename: &str,
    content_type: &str,
    size: i64,
    sha: &str,
) -> Value {
    json!({
        "client_ref": client_ref,
        "kind": kind,
        "original_filename": filename,
        "content_type": content_type,
        // ⚠️ 仓内约定：入参数字一律 JSON **string**（serde `deserialize_i64`）
        "file_size": size.to_string(),
        "content_sha256": sha,
    })
}

fn intents_request(
    token: &str,
    owner_kind: &str,
    owner_id: &str,
    items: Vec<Value>,
) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/files/objects/upload-intents")
        .header("Content-Type", "application/json")
        .header("Authorization", format!("Bearer {token}"))
        .body(Body::from(
            json!({ "owner_kind": owner_kind, "owner_id": owner_id, "items": items }).to_string(),
        ))
        .unwrap()
}

/// `POST /files/objects/confirm` 的请求体（字段名与 `ConfirmFileIn` 逐字对齐）。
#[allow(clippy::too_many_arguments)]
fn confirm_request(
    token: &str,
    owner_kind: &str,
    owner_id: &str,
    kind: &str,
    object_key: &str,
    sha: &str,
    filename: &str,
    size: i64,
    content_type: &str,
) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/files/objects/confirm")
        .header("Content-Type", "application/json")
        .header("Authorization", format!("Bearer {token}"))
        .body(Body::from(
            json!({
                "owner_kind": owner_kind,
                "owner_id": owner_id,
                "kind": kind,
                "object_key": object_key,
                "content_sha256": sha,
                "original_filename": filename,
                "file_size": size.to_string(),
                "content_type": content_type,
            })
            .to_string(),
        ))
        .unwrap()
}

/// 把「上传意图 → 模拟 PUT → confirm」跑完，返回 confirm 的行。
///
/// 与真实链路的唯一差别：第 ③ 步用 `cos.put_object`（替身）代替浏览器发 HTTP PUT。
#[allow(clippy::too_many_arguments)]
async fn upload_via_chain(
    pool: &PgPool,
    cos: &Arc<MockCos>,
    owner_kind: &str,
    owner_id: i64,
    kind: &str,
    filename: &str,
    content_type: &str,
    bytes: Vec<u8>,
    current: &hsh_erp_rust::auth::rbac::CurrentUser,
) -> hsh_erp_rust::modules::files::part_file::vo::PartFileOut {
    let svc = hsh_erp_rust::modules::files::part_file::service::PartFileService::new(
        hsh_erp_test_support::shared_test_snowflake().clone(),
        cos.clone(),
    );
    let sha = sha_of(&bytes);
    let object_key = hsh_erp_rust::util::cos_key::build_cas_key("uploads", &sha, filename);
    cos.put_object(&object_key, bytes.clone(), content_type)
        .await
        .expect("模拟前端 PUT 直传");
    svc.bind_uploaded_file(
        pool,
        cos.clone(),
        "uploads",
        300 * 1024 * 1024,
        owner_kind,
        owner_id,
        kind,
        &object_key,
        &sha,
        filename,
        bytes.len() as i64,
        content_type,
        None,
        current,
    )
    .await
    .expect("confirm 应成功")
}

fn manager() -> hsh_erp_rust::auth::rbac::CurrentUser {
    hsh_erp_rust::auth::rbac::CurrentUser {
        id: 1,
        username: "manager".into(),
        roles: vec![hsh_erp_rust::auth::rbac::Role::Manager],
        shelf_ids: vec![],
        shelf_wildcard: false,
        permissions: Vec::new(),
    }
}

/// 直接落一行（给「秒传」用例造前置数据用）。
async fn seed_part_file(
    pool: &PgPool,
    owner_id: i64,
    kind: &str,
    filename: &str,
    content_type: &str,
    bytes: Vec<u8>,
) -> String {
    let sha = sha_of(&bytes);
    let object_key = hsh_erp_rust::util::cos_key::build_cas_key("uploads", &sha, filename);
    {
        let mut conn = pool.acquire().await.unwrap();
        let mut repo: &mut sqlx::PgConnection = &mut conn;
        PartFileRepoTrait::create_part_file(
            &mut repo,
            hsh_erp_rust::modules::files::part_file::repo::NewPartFile {
                id: hsh_erp_test_support::shared_test_snowflake().next_id(),
                part_id: owner_id,
                owner_kind: "PART",
                kind,
                file_type: "PDF",
                object_key: &object_key,
                original_filename: filename,
                file_size: bytes.len() as i64,
                content_type,
                upload_status: "READY",
                content_sha256: Some(&sha),
                created_by: 1,
            },
        )
        .await
        .expect("seed t_part_file");
    }
    object_key
}

async fn insert_part(pool: &PgPool, label: &str) -> i64 {
    let id = hsh_erp_test_support::shared_test_snowflake().next_id();
    sqlx::query(
        "INSERT INTO t_part (id, name, drawing_no, applicant_name, customer_id, \
         request_date, planned_delivery_date, status, version, \
         created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, 'DWG-INTENTS', 'tester', $3, current_date, current_date, \
                 'PENDING', 0, now(), NULL, now(), NULL)",
    )
    .bind(id)
    .bind(format!("part-{label}"))
    .bind(PartFixture::CUSTOMER_L2_ID)
    .execute(pool)
    .await
    .expect("insert t_part");
    id
}

async fn insert_l1(pool: &PgPool, name: &str) -> i64 {
    let id = hsh_erp_test_support::shared_test_snowflake().next_id();
    sqlx::query(
        "INSERT INTO t_customer (id, name, parent_id, serial_prefix, version, \
         created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, NULL, 'F', 0, now(), NULL, now(), NULL)",
    )
    .bind(id)
    .bind(name)
    .execute(pool)
    .await
    .expect("insert L1");
    id
}

async fn insert_l2(pool: &PgPool, name: &str, l1: i64) -> i64 {
    let id = hsh_erp_test_support::shared_test_snowflake().next_id();
    sqlx::query(
        "INSERT INTO t_customer (id, name, parent_id, serial_prefix, version, \
         created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, $3, NULL, 0, now(), NULL, now(), NULL)",
    )
    .bind(id)
    .bind(name)
    .bind(l1)
    .execute(pool)
    .await
    .expect("insert L2");
    id
}

// ===========================================================================
//  1. 正常签发
// ===========================================================================

#[tokio::test]
async fn intents_happy_path() {
    let (pool, fx) = setup().await;
    let part_id = insert_part(&pool, "intents-happy").await;
    let (app, cos) = app_with_mock_cos(&pool).await;
    let token = login_token(&app, &fx.manager_username, PartFixture::PASSWORD).await;

    let bytes = b"%PDF-1.5\nintents happy\n%%EOF".to_vec();
    let sha = sha_of(&bytes);
    let (status, envelope) = send(
        app.clone(),
        intents_request(
            &token,
            "PART",
            &part_id.to_string(),
            vec![item(
                "ref-1",
                "DRAWING",
                "intents.pdf",
                "application/pdf",
                bytes.len() as i64,
                &sha,
            )],
        ),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "签发成功是 200：{envelope}");
    assert_eq!(envelope["code"], 0, "成功信封 code=0：{envelope}");
    let it = &envelope["data"]["items"][0];
    assert_eq!(it["client_ref"], "ref-1", "client_ref 原样回显");
    assert_eq!(it["already_exists"], false);
    let object_key = it["object_key"].as_str().expect("object_key 必为字符串");
    let expected_key = hsh_erp_rust::util::cos_key::build_cas_key("uploads", &sha, "intents.pdf");
    assert_eq!(
        object_key, expected_key,
        "object_key 必须是按 (sha, filename) 派生的 CAS key"
    );
    assert_eq!(
        it["content_type"], "application/pdf",
        "content_type 原样回传（前端 PUT 时必须原样带上）"
    );
    let expires = it["expires_in"].as_u64().expect("expires_in 必为数字");
    assert!(expires > 0, "expires_in 必须 > 0，实际 {expires}");
    let url = it["upload_url"].as_str().expect("upload_url 必为字符串");
    assert!(
        url.contains(object_key),
        "upload_url 必须含 object_key（前端拿它 PUT）：{url}"
    );
    // 替身观测点：确实签发了一次，且签的是这个 key
    let calls = cos.put_url_calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 1, "应恰好签发 1 张 URL");
    assert_eq!(calls[0].0, object_key);
    assert_eq!(calls[0].1, "application/pdf");
}

// ===========================================================================
//  2. CAS 命中秒传
// ===========================================================================

#[tokio::test]
async fn intents_cas_hit_is_instant_upload() {
    let (pool, fx) = setup().await;
    let part_id = insert_part(&pool, "intents-cas").await;
    let (app, cos) = app_with_mock_cos(&pool).await;
    let token = login_token(&app, &fx.manager_username, PartFixture::PASSWORD).await;

    // 前置：先跑完一轮完整链路（同 owner + kind + 同 sha 落一行）
    let bytes = b"%PDF-1.5\nidentical content\n%%EOF".to_vec();
    let seeded = upload_via_chain(
        &pool,
        &cos,
        "PART",
        part_id,
        "DRAWING",
        "first.pdf",
        "application/pdf",
        bytes.clone(),
        &manager(),
    )
    .await;
    let before = cos.put_url_call_count();
    assert!(seeded.id > 0, "前置行应已落库（helper 自检）");

    let sha = sha_of(&bytes);
    let (status, envelope) = send(
        app.clone(),
        intents_request(
            &token,
            "PART",
            &part_id.to_string(),
            vec![item(
                "ref-cas",
                "DRAWING",
                "second.pdf",
                "application/pdf",
                bytes.len() as i64,
                &sha,
            )],
        ),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "秒传也是 200：{envelope}");
    let it = &envelope["data"]["items"][0];
    assert_eq!(it["client_ref"], "ref-cas");
    assert_eq!(
        it["already_exists"], true,
        "同 owner+kind+sha 已有活跃行 ⇒ 秒传：{it}"
    );
    assert!(
        it.get("upload_url").is_none(),
        "秒传项**不得**返回 upload_url（服务端没签）：{it}"
    );
    assert!(
        it.get("object_key").is_none(),
        "秒传项不带 object_key：{it}"
    );
    let existing = &it["existing_file"];
    assert!(
        existing["id"].is_string(),
        "existing_file 必须是完整的 PartFileOut（id 为字符串）：{existing}"
    );
    assert_eq!(existing["kind"], "DRAWING");
    assert_eq!(existing["original_filename"], "first.pdf");
    assert_eq!(
        cos.put_url_call_count(),
        before,
        "命中 CAS 时**一张 URL 都不该签**（秒传的观测点）"
    );
}

/// 边界：被软删的旧行**不算**秒传命中（只有活跃行短路）。
#[tokio::test]
async fn intents_cas_ignores_soft_deleted_row() {
    let (pool, fx) = setup().await;
    let part_id = insert_part(&pool, "intents-cas-soft").await;
    let (app, cos) = app_with_mock_cos(&pool).await;
    let token = login_token(&app, &fx.manager_username, PartFixture::PASSWORD).await;

    let bytes = b"%PDF-1.5\nsoft deleted content\n%%EOF".to_vec();
    let out = upload_via_chain(
        &pool,
        &cos,
        "PART",
        part_id,
        "DRAWING",
        "old.pdf",
        "application/pdf",
        bytes.clone(),
        &manager(),
    )
    .await;
    {
        let mut conn = pool.acquire().await.unwrap();
        let mut repo: &mut sqlx::PgConnection = &mut conn;
        PartFileRepoTrait::soft_delete_file(&mut repo, out.id, out.version, 1)
            .await
            .expect("soft delete");
    }

    let sha = sha_of(&bytes);
    let (status, envelope) = send(
        app.clone(),
        intents_request(
            &token,
            "PART",
            &part_id.to_string(),
            vec![item(
                "ref-soft",
                "DRAWING",
                "new.pdf",
                "application/pdf",
                bytes.len() as i64,
                &sha,
            )],
        ),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{envelope}");
    let it = &envelope["data"]["items"][0];
    assert_eq!(
        it["already_exists"], false,
        "旧行已软删 ⇒ 不算秒传命中，应重新签 URL：{it}"
    );
    assert!(it["upload_url"].is_string());
}

// ===========================================================================
//  3. 批量：client_ref 原样回显、顺序对应
// ===========================================================================

#[tokio::test]
async fn intents_batch_echoes_client_ref_in_order() {
    let (pool, fx) = setup().await;
    let part_id = insert_part(&pool, "intents-batch").await;
    let (app, _cos) = app_with_mock_cos(&pool).await;
    let token = login_token(&app, &fx.manager_username, PartFixture::PASSWORD).await;

    let b1 = b"%PDF-1.5\nbatch one\n%%EOF".to_vec();
    let b2 = b"%PDF-1.5\nbatch two\n%%EOF".to_vec();
    let (status, envelope) = send(
        app.clone(),
        intents_request(
            &token,
            "PART",
            &part_id.to_string(),
            vec![
                item(
                    "a",
                    "DRAWING",
                    "a.pdf",
                    "application/pdf",
                    b1.len() as i64,
                    &sha_of(&b1),
                ),
                item(
                    "b",
                    "DRAWING",
                    "b.pdf",
                    "application/pdf",
                    b2.len() as i64,
                    &sha_of(&b2),
                ),
                item(
                    "c",
                    "3D_MODEL",
                    "c.step",
                    "application/step",
                    128,
                    &"c".repeat(64),
                ),
            ],
        ),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{envelope}");
    let items = envelope["data"]["items"].as_array().unwrap();
    assert_eq!(items.len(), 3, "批量项数应与请求一致：{envelope}");
    for (i, want) in ["a", "b", "c"].iter().enumerate() {
        assert_eq!(
            items[i]["client_ref"], *want,
            "第 {i} 项的 client_ref 必须原样回显且**顺序对应**"
        );
        assert_eq!(items[i]["already_exists"], false);
        assert!(items[i]["upload_url"].is_string());
    }
    assert_eq!(
        items[2]["content_type"], "application/step",
        "逐项 content_type 各自回传"
    );
}

// ===========================================================================
//  4./5. 角色矩阵
// ===========================================================================

#[tokio::test]
async fn intents_clerk_can_sign() {
    let (pool, fx) = setup().await;
    let part_id = insert_part(&pool, "intents-clerk").await;
    let (app, _cos) = app_with_mock_cos(&pool).await;
    let token = login_token(&app, &fx.clerk_username, PartFixture::PASSWORD).await;

    let bytes = b"%PDF-1.5\nclerk\n%%EOF".to_vec();
    let (status, envelope) = send(
        app.clone(),
        intents_request(
            &token,
            "PART",
            &part_id.to_string(),
            vec![item(
                "clerk",
                "DRAWING",
                "clerk.pdf",
                "application/pdf",
                bytes.len() as i64,
                &sha_of(&bytes),
            )],
        ),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "CLERK 在 UPLOAD_ROLES（M+C+CNC）内，应可签发：{envelope}"
    );
}

#[tokio::test]
async fn intents_inspector_and_shelf_rejected() {
    let (pool, fx) = setup().await;
    let part_id = insert_part(&pool, "intents-rbac").await;
    let (app, _cos) = app_with_mock_cos(&pool).await;

    for username in [&fx.inspector_username, &fx.shelf_account_username] {
        let token = login_token(&app, username, PartFixture::PASSWORD).await;
        let bytes = b"%PDF-1.5\nrbac\n%%EOF".to_vec();
        let (status, envelope) = send(
            app.clone(),
            intents_request(
                &token,
                "PART",
                &part_id.to_string(),
                vec![item(
                    "rbac",
                    "DRAWING",
                    "rbac.pdf",
                    "application/pdf",
                    bytes.len() as i64,
                    &sha_of(&bytes),
                )],
            ),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "{username} 不在写路径角色表内，应 403：{envelope}"
        );
        assert_eq!(envelope["code"], 40300, "{username} 应得 40300");
    }
}

// ===========================================================================
//  6. 校验矩阵
// ===========================================================================

#[tokio::test]
async fn intents_validation_matrix() {
    let (pool, fx) = setup().await;
    let part_id = insert_part(&pool, "intents-invalid").await;
    let (app, _cos) = app_with_mock_cos(&pool).await;
    let token = login_token(&app, &fx.manager_username, PartFixture::PASSWORD).await;
    let ok_sha = "a".repeat(64);

    // 逐条独立打一次：每条断言「状态码 + 业务码」两项。
    let cases: Vec<(&str, Vec<Value>, u16, i64)> = vec![
        (
            "sha 非 64 位 hex",
            vec![item(
                "v1",
                "DRAWING",
                "a.pdf",
                "application/pdf",
                10,
                "not-a-sha",
            )],
            422,
            40001,
        ),
        (
            "file_size = 0",
            vec![item(
                "v2",
                "DRAWING",
                "a.pdf",
                "application/pdf",
                0,
                &ok_sha,
            )],
            422,
            40001,
        ),
        (
            "file_size 超 COS_MAX_FILE_SIZE（300MB）",
            vec![item(
                "v3",
                "DRAWING",
                "a.pdf",
                "application/pdf",
                300 * 1024 * 1024 + 1,
                &ok_sha,
            )],
            400,
            21103,
        ),
        (
            "扩展名不在 kind 白名单（DRAWING + .step）",
            vec![item(
                "v4",
                "DRAWING",
                "a.step",
                "application/step",
                10,
                &ok_sha,
            )],
            400,
            21102,
        ),
        (
            "content_type 与扩展名不匹配（.pdf + octet-stream）",
            vec![item(
                "v5",
                "DRAWING",
                "a.pdf",
                "application/octet-stream",
                10,
                &ok_sha,
            )],
            400,
            21102,
        ),
        (
            "owner_kind × kind 配对非法（PART + ASSEMBLY_MASTER）",
            vec![item(
                "v6",
                "ASSEMBLY_MASTER",
                "a.pdf",
                "application/pdf",
                10,
                &ok_sha,
            )],
            400,
            20104,
        ),
        (
            "owner 不存在",
            vec![item(
                "v7",
                "DRAWING",
                "a.pdf",
                "application/pdf",
                10,
                &ok_sha,
            )],
            404,
            21105,
        ),
    ];

    for (label, items, want_status, want_code) in cases {
        let owner_id = if label == "owner 不存在" {
            "999999999999999999".to_string()
        } else {
            part_id.to_string()
        };
        let (status, envelope) = send(
            app.clone(),
            intents_request(&token, "PART", &owner_id, items),
        )
        .await;
        assert_eq!(
            status.as_u16(),
            want_status,
            "「{label}」的 HTTP 状态码：{envelope}"
        );
        assert_eq!(
            envelope["code"], want_code,
            "「{label}」的业务码：{envelope}"
        );
    }
}

/// `items` 为空 / 超 200 上限（批量形状校验，40001）。
#[tokio::test]
async fn intents_rejects_empty_and_oversized_items() {
    let (pool, fx) = setup().await;
    let part_id = insert_part(&pool, "intents-shape").await;
    let (app, _cos) = app_with_mock_cos(&pool).await;
    let token = login_token(&app, &fx.manager_username, PartFixture::PASSWORD).await;

    let (status, envelope) = send(
        app.clone(),
        intents_request(&token, "PART", &part_id.to_string(), vec![]),
    )
    .await;
    assert_eq!(status.as_u16(), 422, "空 items：{envelope}");
    assert_eq!(envelope["code"], 40001);

    let many: Vec<Value> = (0..201)
        .map(|i| {
            item(
                &format!("o{i}"),
                "DRAWING",
                &format!("o{i}.pdf"),
                "application/pdf",
                10,
                &format!("{:0>64}", i),
            )
        })
        .collect();
    let (status, envelope) = send(
        app.clone(),
        intents_request(&token, "PART", &part_id.to_string(), many),
    )
    .await;
    assert_eq!(status.as_u16(), 422, "201 项超上限：{envelope}");
    assert_eq!(envelope["code"], 40001);
}

// ===========================================================================
//  7. CUSTOMER × DELIVERY_TEMPLATE（送货单打印模板）
// ===========================================================================

#[tokio::test]
async fn intents_customer_delivery_template_manager_ok() {
    let (pool, fx) = setup().await;
    let l1 = insert_l1(&pool, "客户-模板-签发").await;
    let (app, _cos) = app_with_mock_cos(&pool).await;
    let token = login_token(&app, &fx.manager_username, PartFixture::PASSWORD).await;

    let mime = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet";
    let (status, envelope) = send(
        app.clone(),
        intents_request(
            &token,
            "CUSTOMER",
            &l1.to_string(),
            vec![item(
                "tpl",
                "DELIVERY_TEMPLATE",
                "模板.xlsx",
                mime,
                4096,
                &"d".repeat(64),
            )],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "MANAGER 应可签发模板：{envelope}");
    assert_eq!(envelope["data"]["items"][0]["content_type"], mime);
}

#[tokio::test]
async fn intents_customer_delivery_template_clerk_forbidden() {
    let (pool, fx) = setup().await;
    let l1 = insert_l1(&pool, "客户-模板-CLERK").await;
    let (app, _cos) = app_with_mock_cos(&pool).await;
    let token = login_token(&app, &fx.clerk_username, PartFixture::PASSWORD).await;

    let (status, envelope) = send(
        app.clone(),
        intents_request(
            &token,
            "CUSTOMER",
            &l1.to_string(),
            vec![item(
                "tpl",
                "DELIVERY_TEMPLATE",
                "模板.xlsx",
                "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
                4096,
                &"d".repeat(64),
            )],
        ),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "CUSTOMER 写路径仅 Manager，CLERK 必须 403：{envelope}"
    );
    assert_eq!(envelope["code"], 40300);
}

/// 绑**二级**客户被拒（`repo::customer_owner_exists` 的 `parent_id IS NULL` 判据）。
///
/// ⚠️ 错误码是 **21105**（OWNER_NOT_FOUND）而不是 20104 —— 上传端点下线后
/// 模板写入只剩这两条通用写入口，L1-only 判据在 files 域的共用闸门里，代码统一。
#[tokio::test]
async fn intents_customer_l2_rejected_with_21105() {
    let (pool, fx) = setup().await;
    let l1 = insert_l1(&pool, "客户-模板-L1").await;
    let l2 = insert_l2(&pool, "子客-模板-L2", l1).await;
    let (app, _cos) = app_with_mock_cos(&pool).await;
    let token = login_token(&app, &fx.manager_username, PartFixture::PASSWORD).await;

    let (status, envelope) = send(
        app.clone(),
        intents_request(
            &token,
            "CUSTOMER",
            &l2.to_string(),
            vec![item(
                "tpl",
                "DELIVERY_TEMPLATE",
                "模板.xlsx",
                "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
                4096,
                &"d".repeat(64),
            )],
        ),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "模板只绑一级客户，绑二级应被拒：{envelope}"
    );
    assert_eq!(envelope["code"], 21105);
    // 零副作用：L2 名下没有任何模板行
    let rows: Vec<(i64,)> = sqlx::query_as("SELECT id FROM t_part_file WHERE part_id = $1")
        .bind(l2)
        .fetch_all(&pool)
        .await
        .unwrap();
    assert!(rows.is_empty(), "被拒的签发不该留下任何行");
}

/// seed 专用：确认「helper 造的前置行确实落在库里」——避免上面两条用例的
/// 前置数据是空跑（helper 自己 panic 就会红，这条只是把因果写进测试输出）。
#[tokio::test]
async fn seed_helper_is_wired() {
    let (pool, _fx) = setup().await;
    let part_id = insert_part(&pool, "intents-seedcheck").await;
    let key = seed_part_file(
        &pool,
        part_id,
        "DRAWING",
        "seed.pdf",
        "application/pdf",
        b"%PDF-1.5\nseed\n%%EOF".to_vec(),
    )
    .await;
    assert!(key.starts_with("uploads/"), "seed key 应是 CAS 模板：{key}");
    assert_eq!(
        test_state(pool.clone())
            .await
            .cos
            .presigned_get_url(&key, 60)
            .await
            .unwrap(),
        format!("local://{key}"),
        "默认 state 用 NoopCos（占位 URL 含 key）；app_with_mock_cos 才是 MockCos"
    );
}

// ===========================================================================
//  8. content_type 与扩展名不匹配：两条写入口必须给**同一个**码
// ===========================================================================

/// ⚠️ **「声明的 MIME 与扩展名对不上」在两条写入口都是 21102 / HTTP 400**。
///
/// upload-intents 自判返 21102；confirm 原先走 `validate::check_content_type` 返
/// `AppError::validation` = 40001 / HTTP 422 —— 同一份文件、同一个判据、两个码两个
/// HTTP 状态，前端要写两套特判。现在统一为 21102。
#[tokio::test]
async fn confirm_content_type_mismatch_is_21102_like_upload_intents() {
    let (pool, fx) = setup().await;
    let part_id = insert_part(&pool, "intents-ct").await;
    let (app, cos) = app_with_mock_cos(&pool).await;
    let token = login_token(&app, &fx.manager_username, PartFixture::PASSWORD).await;

    // 前置：把字节真 PUT 到正式 CAS key 上 ⇒ confirm 的失败点只可能是 MIME 校验
    let bytes = b"%PDF-1.5\nct mismatch\n%%EOF".to_vec();
    let sha = sha_of(&bytes);
    let object_key = hsh_erp_rust::util::cos_key::build_cas_key("uploads", &sha, "ct.pdf");
    cos.put_object(&object_key, bytes.clone(), "application/pdf")
        .await
        .expect("模拟前端 PUT");

    // upload-intents 侧：同一个不匹配的 MIME ⇒ 21102 / 400
    let (status, envelope) = send(
        app.clone(),
        intents_request(
            &token,
            "PART",
            &part_id.to_string(),
            vec![item(
                "ct",
                "DRAWING",
                "ct.pdf",
                "application/octet-stream",
                bytes.len() as i64,
                &sha,
            )],
        ),
    )
    .await;
    assert_eq!(status.as_u16(), 400, "upload-intents 侧：{envelope}");
    assert_eq!(envelope["code"], 21102, "upload-intents 侧：{envelope}");

    // confirm 侧：同一个不匹配的 MIME ⇒ 必须**同样**是 21102 / 400
    let (status, envelope) = send(
        app.clone(),
        confirm_request(
            &token,
            "PART",
            &part_id.to_string(),
            "DRAWING",
            &object_key,
            &sha,
            "ct.pdf",
            bytes.len() as i64,
            "application/octet-stream",
        ),
    )
    .await;
    assert_eq!(
        status.as_u16(),
        400,
        "confirm 侧必须与 upload-intents 同码同状态（历史差异：422 / 40001）：{envelope}"
    );
    assert_eq!(
        envelope["code"], 21102,
        "confirm 侧必须与 upload-intents 同码：{envelope}"
    );

    // 零副作用：被拒的 confirm 不该留下任何行
    let rows: Vec<(i64,)> = sqlx::query_as("SELECT id FROM t_part_file WHERE part_id = $1")
        .bind(part_id)
        .fetch_all(&pool)
        .await
        .unwrap();
    assert!(rows.is_empty(), "被拒的 confirm 不该落行：{rows:?}");
}

// ===========================================================================
//  9. CAD_2D（DWG / DXF）整条链路
// ===========================================================================

/// ★ **CAD 源文件（DWG / DXF）从「无处可上传」到能走完整条链路**。
///
/// `CAD_2D` 早已在 DB CHECK / `uk_t_part_file_single` / 读侧 kind 白名单 / 软删分派
/// 里，前端零件详情也有专门的 CAD tab（`:accept='.dwg,.dxf'`）；只有**写侧两张
/// policy 表**漏了它 ⇒ 那两条入口都传不上去。本用例把三步（签发 → PUT → 登记）
/// 全走一遍，钉住 `file_type='CAD_2D'` 与 MIME 兜底（dwg/dxf 无官方 MIME ⇒ 浏览器
/// 发 `application/octet-stream` 必须被收）。
#[tokio::test]
async fn cad_2d_dwg_goes_through_the_whole_chain() {
    let (pool, fx) = setup().await;
    let part_id = insert_part(&pool, "intents-cad").await;
    let (app, cos) = app_with_mock_cos(&pool).await;
    let token = login_token(&app, &fx.manager_username, PartFixture::PASSWORD).await;

    let bytes = b"AutoCAD DWG placeholder".to_vec();
    let sha = sha_of(&bytes);

    // ① 签发：CAD_2D + .dwg + octet-stream 必须放行
    let (status, envelope) = send(
        app.clone(),
        intents_request(
            &token,
            "PART",
            &part_id.to_string(),
            vec![item(
                "cad",
                "CAD_2D",
                "bracket.dwg",
                "application/octet-stream",
                bytes.len() as i64,
                &sha,
            )],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "CAD_2D 签发应 200：{envelope}");
    let object_key = envelope["data"]["items"][0]["object_key"]
        .as_str()
        .expect("CAD_2D 应签出 object_key")
        .to_string();

    // ② 直传（替身）
    cos.put_object(&object_key, bytes.clone(), "application/octet-stream")
        .await
        .expect("模拟前端 PUT");

    // ③ 登记：行落库、file_type 由扩展名推导
    let (status, envelope) = send(
        app.clone(),
        confirm_request(
            &token,
            "PART",
            &part_id.to_string(),
            "CAD_2D",
            &object_key,
            &sha,
            "bracket.dwg",
            bytes.len() as i64,
            "application/octet-stream",
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "CAD_2D 登记应 200：{envelope}");
    assert_eq!(envelope["data"]["kind"], "CAD_2D");
    assert_eq!(
        envelope["data"]["file_type"], "CAD_2D",
        "dwg ⇒ file_type='CAD_2D'（与 kind 同名）"
    );

    let rows: Vec<(String,)> =
        sqlx::query_as("SELECT kind FROM t_part_file WHERE part_id = $1 AND deleted_at IS NULL")
            .bind(part_id)
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(rows, vec![("CAD_2D".to_string(),)], "CAD_2D 行应落库");

    // 反向：CAD_2D 只收 dwg / dxf，pdf 一律 21102
    let (status, envelope) = send(
        app.clone(),
        intents_request(
            &token,
            "PART",
            &part_id.to_string(),
            vec![item(
                "cad-pdf",
                "CAD_2D",
                "bracket.pdf",
                "application/pdf",
                10,
                &"c".repeat(64),
            )],
        ),
    )
    .await;
    assert_eq!(status.as_u16(), 400, "CAD_2D 不收 pdf：{envelope}");
    assert_eq!(envelope["code"], 21102);
}

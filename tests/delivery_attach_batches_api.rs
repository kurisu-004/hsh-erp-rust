//! delivery-notes/{note_id}/attach-batches 弹窗批量 attach 端到端集成测试
//!
//! 端点契约（POST /api/v2/delivery-notes/{note_id}/attach-batches）：
//! - 入参 `{ batches: [{ batch_id: string, version: i32 }, ...] }`
//! - 出参 `{ attached: usize, conflicts: [{ batch_id, reason }] }`
//! - 部分失败 → 200 + `conflicts[]`（不中断其它 item）
//! - note 非 DRAFT → 409 + code=BIZ_DELIVERY_NOTE_NOT_DRAFT(21403)
//! - req.batches 长度 > 200 → 400 + code=BIZ_INVALID_VALUE(20104)
//!
//! Reason 字符串：
//!   BATCH_NOT_FOUND     — 批次不存在 / 已软删
//!   ALREADY_ATTACHED    — delivery_note_id IS NOT NULL
//!   INVALID_STATE:XXX   — status 不在 A 组（INSPECTION / READY_TO_SHIP）
//!   VERSION_CONFLICT    — item.version 与 DB 不一致（OCC 失败）
//!
//! 并行 / 认证：共享 `postgres_rust_test`；进程级 `tokio::sync::Mutex` 串行化。
//! 每个用例 MANAGER token。

#[path = "common/mod.rs"]
mod common;

use axum::body::{to_bytes, Body};
use axum::http::{header::AUTHORIZATION, Request, StatusCode};
use serde_json::{json, Value};
use sqlx::PgPool;
use tower::ServiceExt;

use common::{
    add_role, clean_business_db, clean_db, ensure_database_exists, insert_user_with_password,
    test_app, test_pool,
};
use hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator;

// ===========================================================================
//  全局串行化 + helpers
// ===========================================================================

static TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn send(app: axum::Router, req: Request<Body>) -> (StatusCode, Value) {
    let response = app.oneshot(req).await.expect("oneshot");
    let status = response.status();
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read body");
    let envelope: Value = serde_json::from_slice(&body)
        .unwrap_or_else(|e| panic!("parse JSON: {e}; raw = {}", String::from_utf8_lossy(&body)));
    (status, envelope)
}

fn json_request(method: &str, uri: &str, body: Option<Value>, bearer: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(t) = bearer {
        builder = builder.header(AUTHORIZATION, format!("Bearer {t}"));
    }
    if body.is_some() {
        builder = builder.header("content-type", "application/json");
    }
    let body = match body {
        Some(v) => Body::from(v.to_string()),
        None => Body::empty(),
    };
    builder.body(body).expect("build request")
}

async fn setup<'a>() -> (tokio::sync::MutexGuard<'a, ()>, PgPool) {
    let guard = TEST_LOCK.lock().await;
    ensure_database_exists().await;
    let pool = test_pool().await;
    clean_db(&pool).await;
    clean_business_db(&pool).await;
    (guard, pool)
}

async fn login_manager(pool: PgPool, username: &str) -> (axum::Router, String, PgPool) {
    let uid = insert_user_with_password(&pool, username, "changeme").await;
    add_role(&pool, uid, "MANAGER", None, None).await;
    let state = common::test_state(pool.clone()).await;
    let app = test_app(state.clone());
    let (_, env) = send(
        app,
        json_request(
            "POST",
            "/auth/login",
            Some(json!({"username": username, "password": "changeme"})),
            None,
        ),
    )
    .await;
    let token = env["data"]["token"].as_str().unwrap().to_string();
    let app2 = test_app(state);
    (app2, token, pool)
}

// ---------- fixture helpers（与 delivery_scan_api.rs 同形；独立副本避免测试间耦合） ----------

async fn insert_l1(pool: &PgPool, name: &str, prefix: &str) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let id = snowflake.next_id();
    let now = now_naive();
    sqlx::query!(
        "INSERT INTO t_customer (id, name, parent_id, serial_prefix, version, \
         created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, NULL, $3, 0, $4, NULL, $4, NULL)",
        id, name, prefix, now,
    )
    .execute(pool)
    .await
    .expect("insert L1");
    id
}

async fn insert_l2(pool: &PgPool, name: &str, l1_id: i64) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let id = snowflake.next_id();
    let now = now_naive();
    sqlx::query!(
        "INSERT INTO t_customer (id, name, parent_id, serial_prefix, version, \
         created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, $3, NULL, 0, $4, NULL, $4, NULL)",
        id, name, l1_id, now,
    )
    .execute(pool)
    .await
    .expect("insert L2");
    id
}

async fn insert_part(
    pool: &PgPool,
    name: &str,
    customer_id: i64,
    serial_no: Option<&str>,
) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let id = snowflake.next_id();
    let now = now_naive();
    let today = now.date();
    sqlx::query!(
        "INSERT INTO t_part (id, serial_no, name, drawing_no, customer_id, status, \
         applicant_name, request_date, planned_delivery_date, \
         quantity, has_been_repaired, version, created_at, created_by, updated_at, updated_by, \
         assembly_id) \
         VALUES ($1, $2, $3, 'D-001', $4, 'INSPECTION', $3, $6, $6, 1, false, 0, $5, NULL, $5, NULL, NULL)",
        id, serial_no, name, customer_id, now, today,
    )
    .execute(pool)
    .await
    .expect("insert part");
    id
}

/// 走 `t_part_batch_id_seq` + version=1（与 scan 测试 `create_test_batch` 同形）。
async fn create_batch(
    pool: &PgPool,
    part_id: i64,
    status: &str,
    holder_id: Option<i64>,
    location: Option<&str>,
) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO t_part_batch (part_id, batch_no, quantity, status, current_holder_id, location, version) \
         VALUES ($1, 1, 10, $2, $3, $4, 1) RETURNING id",
    )
    .bind(part_id)
    .bind(status)
    .bind(holder_id)
    .bind(location)
    .fetch_one(pool)
    .await
    .unwrap()
}

/// 建草稿送货单（手动写 SQL；测试不依赖创建草稿的接口，避免与被测端点耦合）。
///
/// 返回 note.id。
async fn create_draft_note(pool: &PgPool, customer_id: i64) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let id = snowflake.next_id();
    let now = now_naive();
    // t_delivery_note.delivery_note_no 是 varchar(16)，截断雪花 id 到末 12 位
    let no_str = format!("DN{}", id % 1_000_000_000_000);
    sqlx::query!(
        "INSERT INTO t_delivery_note (id, delivery_note_no, customer_id, status, version, \
         created_at, created_by, updated_at, updated_by, leaf_customer_id) \
         VALUES ($1, $2, $3, 'DRAFT', 0, $4, NULL, $4, NULL, $3)",
        id,
        no_str,
        customer_id,
        now,
    )
    .execute(pool)
    .await
    .expect("insert draft note");
    id
}

/// 直接 POST /delivery-notes（建草稿）走业务路径，避免与 test 自身构造的字段耦合。
/// 创建 DRAFT 草稿并预挂 1 个 A 组批次（attach-batches 测试需要 note 已存在）。
/// `first_batch_id` 必须传一个 INSPECTION / READY_TO_SHIP 批次，否则服务端会拒。
async fn create_draft_note_via_api(
    app: axum::Router,
    token: &str,
    customer_id: i64,
    first_batch_id: i64,
) -> (axum::Router, Value) {
    let (s, env) = send(
        app.clone(),
        json_request(
            "POST",
            "/delivery-notes",
            Some(json!({
                "customer_id": customer_id.to_string(),
                "items": [{"batch_id": first_batch_id.to_string()}],
            })),
            Some(token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "create draft via API: {env}");
    // 再 build 一份新的 app（oneshot 消耗了原 router）
    (app, env)
}

/// 把 note 推到 SUBMITTED：先把所有挂单批次置为 READY_TO_SHIP，再调 submit。
async fn submit_note(app: axum::Router, token: &str, note_id: i64, pool: &PgPool) -> axum::Router {
    sqlx::query!(
        "UPDATE t_part_batch SET status = 'READY_TO_SHIP' \
         WHERE delivery_note_id = $1 AND status <> 'READY_TO_SHIP'",
        note_id
    )
    .execute(pool)
    .await
    .expect("bump batches to READY_TO_SHIP");
    let (s, env) = send(
        app.clone(),
        json_request(
            "POST",
            &format!("/delivery-notes/{note_id}/submit"),
            Some(json!({"version": 0})),
            Some(token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "submit: {env}");
    app
}

// ===========================================================================
//  Tests
// ===========================================================================

/// 1. 正常路径：DRAFT note + 1 个 INSPECTION batch（version=1）→ 200，
/// `{attached:1, conflicts:[]}`。
///
/// 这是最基础的 happy path：弹窗勾选后批量 attach 全成功。
#[tokio::test]
async fn attach_batches_normal_path() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "法拉电子", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let pid = insert_part(&pool, "P", l2, Some("ABN00001")).await;
    let bid = create_batch(&pool, pid, "INSPECTION", None, None).await;

    let (app, token, pool) = login_manager(pool, "admin").await;
    let (app, env) = create_draft_note_via_api(app, &token, l1, bid).await;
    let note_id = env["data"]["id"].as_str().unwrap().to_string();

    // detach 后再 attach（create_draft_note_via_api 已自动挂上 bid）
    sqlx::query!(
        "UPDATE t_part_batch SET delivery_note_id = NULL, version = version + 1 \
         WHERE id = $1",
        bid
    )
    .execute(&pool)
    .await
    .unwrap();
    // 拿当前 version
    let ver: i32 = sqlx::query_scalar("SELECT version FROM t_part_batch WHERE id = $1")
        .bind(bid)
        .fetch_one(&pool)
        .await
        .unwrap();

    let (s, resp) = send(
        app,
        json_request(
            "POST",
            &format!("/delivery-notes/{note_id}/attach-batches"),
            Some(json!({
                "batches": [{"batch_id": bid.to_string(), "version": ver}],
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "正常路径应 200: {resp}");
    assert_eq!(resp["data"]["attached"], 1);
    assert_eq!(resp["data"]["conflicts"].as_array().unwrap().len(), 0);

    // 落库断言：batch 已挂到本单
    let dn_id: Option<i64> =
        sqlx::query_scalar("SELECT delivery_note_id FROM t_part_batch WHERE id = $1")
            .bind(bid)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(dn_id.map(|v| v.to_string()), Some(note_id.clone()));
}

/// 2. OCC：item.version 与 DB 不一致（实际 version=1，发 version=999）→
/// 200 + `{attached:0, conflicts:[{batch_id, reason:"VERSION_CONFLICT"}]}`。
///
/// 验证乐观锁路径：不影响其它 item（这里只 1 个 item）。
#[tokio::test]
async fn attach_batches_occ_conflict_via_wrong_version() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "法拉电子", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let pid = insert_part(&pool, "P", l2, Some("ABOCC0001")).await;
    let bid = create_batch(&pool, pid, "INSPECTION", None, None).await;

    let (app, token, pool) = login_manager(pool, "admin").await;
    let (app, env) = create_draft_note_via_api(app, &token, l1, bid).await;
    let note_id = env["data"]["id"].as_str().unwrap().to_string();

    // detach 该批次，让 attach-batches 重新挂（带错 version）
    sqlx::query!(
        "UPDATE t_part_batch SET delivery_note_id = NULL WHERE id = $1",
        bid
    )
    .execute(&pool)
    .await
    .unwrap();

    let (s, resp) = send(
        app,
        json_request(
            "POST",
            &format!("/delivery-notes/{note_id}/attach-batches"),
            Some(json!({
                "batches": [{"batch_id": bid.to_string(), "version": 999}],
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "OCC 不影响 HTTP 状态: {resp}");
    assert_eq!(resp["data"]["attached"], 0);
    let conflicts = resp["data"]["conflicts"].as_array().unwrap();
    assert_eq!(conflicts.len(), 1);
    assert_eq!(
        conflicts[0]["batch_id"].as_str().unwrap(),
        bid.to_string()
    );
    assert_eq!(conflicts[0]["reason"], "VERSION_CONFLICT");

    // 落库断言：batch 未挂到本单
    let dn_id: Option<i64> =
        sqlx::query_scalar("SELECT delivery_note_id FROM t_part_batch WHERE id = $1")
            .bind(bid)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(dn_id.is_none(), "OCC 失败 batch 不应挂单");
}

/// 3. ALREADY_ATTACHED：batch 已挂在别的 note 上 → 200 + conflicts 含 ALREADY_ATTACHED。
#[tokio::test]
async fn attach_batches_already_attached_conflict() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "法拉电子", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let pid = insert_part(&pool, "P", l2, Some("ABA00001")).await;
    let bid = create_batch(&pool, pid, "INSPECTION", None, None).await;

    let (app, token, pool) = login_manager(pool, "admin").await;
    // 第一个 note：自动挂上 bid
    let (app, env1) = create_draft_note_via_api(app, &token, l1, bid).await;
    let _note1_id = env1["data"]["id"].as_str().unwrap().to_string();
    let ver1: i32 = sqlx::query_scalar("SELECT version FROM t_part_batch WHERE id = $1")
        .bind(bid)
        .fetch_one(&pool)
        .await
        .unwrap();

    // 第二个 note（手动构造，避免再次挂同一 batch）
    let note2_id = create_draft_note(&pool, l1).await;

    // 尝试把已挂的 batch 转到 note2 → 期望 ALREADY_ATTACHED 冲突
    let (s, resp) = send(
        app,
        json_request(
            "POST",
            &format!("/delivery-notes/{note2_id}/attach-batches"),
            Some(json!({
                "batches": [{"batch_id": bid.to_string(), "version": ver1}],
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "ALREADY_ATTACHED 不影响 HTTP 状态: {resp}");
    assert_eq!(resp["data"]["attached"], 0);
    let conflicts = resp["data"]["conflicts"].as_array().unwrap();
    assert_eq!(conflicts.len(), 1);
    assert_eq!(conflicts[0]["batch_id"].as_str().unwrap(), bid.to_string());
    assert_eq!(conflicts[0]["reason"], "ALREADY_ATTACHED");
}

/// 4. INVALID_STATE：batch.status=DELIVERED（C 组）→ 200 + conflicts 含 INVALID_STATE:DELIVERED。
///
/// 验证：A 组过滤在 attach 路径同样生效，非 INSPECTION/READY_TO_SHIP 一律拒绝。
#[tokio::test]
async fn attach_batches_invalid_state_conflict() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "法拉电子", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let pid = insert_part(&pool, "P", l2, Some("ABI00001")).await;
    let bid = create_batch(&pool, pid, "DELIVERED", None, None).await;
    let note_id = create_draft_note(&pool, l1).await;

    let (app, token, _pool) = login_manager(pool, "admin").await;
    let ver: i32 = sqlx::query_scalar("SELECT version FROM t_part_batch WHERE id = $1")
        .bind(bid)
        .fetch_one(&_pool)
        .await
        .unwrap();

    let (s, resp) = send(
        app,
        json_request(
            "POST",
            &format!("/delivery-notes/{note_id}/attach-batches"),
            Some(json!({
                "batches": [{"batch_id": bid.to_string(), "version": ver}],
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "INVALID_STATE 不影响 HTTP 状态: {resp}");
    assert_eq!(resp["data"]["attached"], 0);
    let conflicts = resp["data"]["conflicts"].as_array().unwrap();
    assert_eq!(conflicts.len(), 1);
    assert_eq!(conflicts[0]["batch_id"].as_str().unwrap(), bid.to_string());
    assert_eq!(conflicts[0]["reason"], "INVALID_STATE:DELIVERED");

    // 落库断言：batch 未挂单（DELIVERED 不允许 attach）
    let dn_id: Option<i64> =
        sqlx::query_scalar("SELECT delivery_note_id FROM t_part_batch WHERE id = $1")
            .bind(bid)
            .fetch_one(&_pool)
            .await
            .unwrap();
    assert!(dn_id.is_none());
}

/// 5. 非 DRAFT 硬错误：note.status=SUBMITTED → 409 + code=21403 (BIZ_DELIVERY_NOTE_NOT_DRAFT)。
///
/// attach-batches 是显式 batch 操作，要求 note 必须处于 DRAFT。
/// 状态机进入 SUBMITTED 后整单已对外承诺，不能再 attach。
#[tokio::test]
async fn attach_batches_non_draft_note_returns_409() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "法拉电子", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let pid = insert_part(&pool, "P", l2, Some("ABND00001")).await;
    let bid = create_batch(&pool, pid, "INSPECTION", None, None).await;

    let (app, token, pool) = login_manager(pool, "admin").await;
    let (app, env) = create_draft_note_via_api(app, &token, l1, bid).await;
    let note_id = env["data"]["id"].as_str().unwrap().to_string();
    let note_id_i64: i64 = note_id.parse().unwrap();

    // 推到 SUBMITTED
    let app = submit_note(app, &token, note_id_i64, &pool).await;

    // 新建一个 free batch 尝试 attach → 期望 409
    let pid2 = insert_part(&pool, "P2", l2, Some("ABND00002")).await;
    let bid2 = create_batch(&pool, pid2, "INSPECTION", None, None).await;
    let ver2: i32 = sqlx::query_scalar("SELECT version FROM t_part_batch WHERE id = $1")
        .bind(bid2)
        .fetch_one(&pool)
        .await
        .unwrap();

    let (s, resp) = send(
        app,
        json_request(
            "POST",
            &format!("/delivery-notes/{note_id}/attach-batches"),
            Some(json!({
                "batches": [{"batch_id": bid2.to_string(), "version": ver2}],
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "非 DRAFT 应 409: {resp}");
    assert_eq!(
        resp["code"], 21403,
        "code 应为 BIZ_DELIVERY_NOTE_NOT_DRAFT(21403)；got: {resp}"
    );
}

/// 6. 批量上限：req.batches 长度 201 → 400 + code=BIZ_INVALID_VALUE(20104)。
///
/// 单事务内对每个 item 至少 2 次 DB 调用；上限 200 防恶意请求长期持有连接。
#[tokio::test]
async fn attach_batches_too_many_items_returns_400() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "法拉电子", "F").await;
    let note_id = create_draft_note(&pool, l1).await;

    let (app, token, pool) = login_manager(pool, "admin").await;
    let _ = pool; // 抑制 unused warning（前置长度校验在 handler，不需访问 DB）

    // 构造 201 个虚拟 batch_id（不需真实存在——前置长度校验在 handler 层）
    let items: Vec<Value> = (0..201)
        .map(|i| {
            json!({
                "batch_id": (1_000_000_000i64 + i).to_string(),
                "version": 0,
            })
        })
        .collect();

    let (s, resp) = send(
        app,
        json_request(
            "POST",
            &format!("/delivery-notes/{note_id}/attach-batches"),
            Some(json!({"batches": items})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "超长应 400: {resp}");
    assert_eq!(
        resp["code"], 20104,
        "code 应为 BIZ_INVALID_VALUE(20104)；got: {resp}"
    );
}

/// 7. 部分成功：2 个 batches（1 个正常 + 1 个 OCC）→ 200 + `{attached:1, conflicts:[1]}`。
///
/// 验证：单 item 失败不中断其它 item；`attached` 与 `conflicts.len()` 之和等于总 items。
#[tokio::test]
async fn attach_batches_partial_success() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1(&pool, "法拉电子", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;

    // ok_batch: A 组可挂，version=1（DB 默认也是 1，对齐即可）
    let ok_pid = insert_part(&pool, "P_ok", l2, Some("ABPS00001")).await;
    let ok_bid = create_batch(&pool, ok_pid, "INSPECTION", None, None).await;

    // occ_batch: A 组可挂，但 version 错
    let occ_pid = insert_part(&pool, "P_occ", l2, Some("ABPS00002")).await;
    let occ_bid = create_batch(&pool, occ_pid, "INSPECTION", None, None).await;

    let note_id = create_draft_note(&pool, l1).await;
    let (app, token, db_pool) = login_manager(pool, "admin").await;

    let (s, resp) = send(
        app,
        json_request(
            "POST",
            &format!("/delivery-notes/{note_id}/attach-batches"),
            Some(json!({
                "batches": [
                    {"batch_id": ok_bid.to_string(), "version": 1},
                    {"batch_id": occ_bid.to_string(), "version": 999},
                ],
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "部分成功应 200: {resp}");
    assert_eq!(
        resp["data"]["attached"], 1,
        "1 个 OK 应挂单；got {resp}"
    );
    let conflicts = resp["data"]["conflicts"].as_array().unwrap();
    assert_eq!(conflicts.len(), 1, "1 个 OCC 应进 conflicts");
    assert_eq!(
        conflicts[0]["batch_id"].as_str().unwrap(),
        occ_bid.to_string()
    );
    assert_eq!(conflicts[0]["reason"], "VERSION_CONFLICT");

    // 落库断言：ok_batch 已挂，occ_batch 未挂
    let ok_dn: Option<i64> =
        sqlx::query_scalar("SELECT delivery_note_id FROM t_part_batch WHERE id = $1")
            .bind(ok_bid)
            .fetch_one(&db_pool)
            .await
            .unwrap();
    let occ_dn: Option<i64> =
        sqlx::query_scalar("SELECT delivery_note_id FROM t_part_batch WHERE id = $1")
            .bind(occ_bid)
            .fetch_one(&db_pool)
            .await
            .unwrap();
    assert_eq!(ok_dn, Some(note_id), "ok_batch 应挂到 note");
    assert!(occ_dn.is_none(), "occ_batch 不应挂单");
}
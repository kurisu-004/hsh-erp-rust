//! delivery_note 端到端集成测试
//!
//! Phase P2 覆盖（Python `tests/test_delivery_note.py` 移植 + 设计 §3.4 范围校验）：
//!  1. counter_acquires_sequential_numbers（spec 1）
//!  2. create_draft_for_l1_succeeds + create_for_l2_returns_400_21407
//!  3. list_with_filters (status + pagination)
//!  4. get_with_parts with line_items (assembly fields populated)
//!  5. add_parts: same L1 ok; different L1 → 21407; on other active note
//!     → 21406; not INSPECTION/READY_TO_SHIP → 21405; partial quantity
//!     splits batch; scope mismatch → 21416 (group-scoped note + L2
//!     outside the group)
//!  6. remove_parts: DRAFT ok; SUBMITTED → 21412
//!  7. submit: DRAFT ok; recall → DRAFT ok; recall into existing draft
//!     scope → 21419
//!  8. pickup: non-driver worker → 21409; happy path → PICKED_UP;
//!     ws_hub.broadcast observed
//!  9. soft_delete: DRAFT ok; non-DRAFT → 21403
//! 10. version conflict on any write → 40901
//! 11. list_candidate_parts for L1 (200, contains fixtures); non-L1 → 400
//!
//! 进程级 test_pool 每次 fresh database（plan 2 2026-09-20），DB 间 schema
//! 完全独立，无需 Mutex 串行化。
//!
//! 2026-09-23 PR13 Phase G 改造：本地 `fn send` / `fn json_request` / `fn setup` /
//! `fn login_manager` 全部删除，统一用 `hsh_erp_test_support::{send, json_request,
//! login_token, test_pool, test_state, test_app, load_delivery_fixture}`。
//! 新增 `bootstrap_as_manager` 样板；本地 `insert_l1` / `insert_l2` / `insert_part` /
//! `insert_batch` / `insert_group` / `insert_group_member` / `insert_worker`
//! 全部保留（测试需要特定 name / status / 业务数据，fixture 不预置此类业务数据；
//! pickup 测试的 t_work_type / t_worker 走「按 code 查找」路径，硬编码业务
//! code='送货司机'，不能由 fixture 预制不同 code 的 work_type 替代）。

use axum::http::StatusCode;
use serde_json::{Value, json};
use sqlx::PgPool;
use sqlx::Row;

use hsh_erp_test_support::{
    DeliveryFixture, json_request, load_delivery_fixture, login_token, send, test_app, test_pool,
    test_state,
};

use hsh_erp_rust::infra::clock::now_naive;
use hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator;

// ===========================================================================
//  Bootstrap helpers
// ===========================================================================

/// 找一个**尚未被占用**的 L1 序列号前缀。
///
/// `t_customer.serial_prefix` 是 `varchar(1)`，且 `uq_t_customer_root_prefix` 对
/// 未软删的 L1 全局唯一。2026-10-08 起「同 L1 只有一张 DRAFT」的数据库约束让「造
/// 多张草稿」必须配多个 L1，而多个 L1 又要求多个不同前缀 —— 这里从字母数字里线性
/// 探测第一个未占用的，免得测试之间与 fixture 之间互相踩前缀。
async fn fresh_prefix(pool: &PgPool) -> String {
    for ch in ('A'..='Z').chain('a'..='z').chain('0'..='9') {
        let used: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM t_customer \
             WHERE serial_prefix = $1 AND deleted_at IS NULL)",
        )
        .bind(ch.to_string())
        .fetch_one(pool)
        .await
        .expect("probe serial_prefix");
        if !used {
            return ch.to_string();
        }
    }
    panic!("没有可用的一字符序列号前缀");
}

/// 起一份 fresh database + 加载 delivery fixture + 以 MANAGER 身份登录。
async fn bootstrap_as_manager() -> (PgPool, axum::Router, String, DeliveryFixture) {
    let pool = test_pool().await;
    let fx = load_delivery_fixture(&pool).await;
    let app = test_app(test_state(pool.clone()).await);
    let token = login_token(&app, &fx.part_manager_username, DeliveryFixture::PASSWORD).await;
    (pool, app, token, fx)
}

// ===========================================================================
//  Domain fixtures：L1 / L2 客户 + part + batch + 分组 + 成员 + 工人
//  （保留本地 helper：测试需要特定 name / status / 业务数据）
// ===========================================================================

/// 直插 L1 客户
async fn insert_l1(pool: &PgPool, name: &str, prefix: &str) -> i64 {
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
    .expect("insert L1");
    id
}

/// 直插 L2 客户（parent_id = l1_id）
async fn insert_l2(pool: &PgPool, name: &str, l1_id: i64) -> i64 {
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
    .expect("insert L2");
    id
}

/// 直插工单
async fn insert_part(pool: &PgPool, name: &str, customer_id: i64, serial_no: Option<&str>) -> i64 {
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let id = snowflake.next_id();
    let now = now_naive();
    let today = now.date();
    // 2026-09-16 PR-2（migration 027）：t_part 删 `has_been_repaired`；INSERT 列名与
    // VALUES 占位符同步移除。
    sqlx::query(
        "INSERT INTO t_part (id, serial_no, name, drawing_no, customer_id, status, \
         applicant_name, request_date, planned_delivery_date, \
         quantity, version, created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, $3, 'D-001', $4, 'INSPECTION', $3, $6, $6, 1, 0, $5, NULL, $5, NULL)",
    )
    .bind(id)
    .bind(serial_no)
    .bind(name)
    .bind(customer_id)
    .bind(now)
    .bind(today)
    .execute(pool)
    .await
    .expect("insert part");
    id
}

/// 直插批次
async fn insert_batch(
    pool: &PgPool,
    part_id: i64,
    batch_no: i32,
    quantity: i32,
    status: &str,
) -> i64 {
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let id = snowflake.next_id();
    let now = now_naive();
    // 2026-09-16 PR-2（migration 027）：t_part_batch 删 `has_been_repaired`；INSERT
    // 列名与 VALUES 占位符同步移除 `false` 字面量。
    sqlx::query(
        "INSERT INTO t_part_batch (id, part_id, batch_no, quantity, status, \
         version, created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, $3, $4, $5, 0, $6, NULL, $6, NULL)",
    )
    .bind(id)
    .bind(part_id)
    .bind(batch_no)
    .bind(quantity)
    .bind(status)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert batch");
    id
}

/// 直插送货分组
async fn insert_group(pool: &PgPool, l1_id: i64, name: &str) -> i64 {
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let id = snowflake.next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_delivery_group (id, customer_id, name, version, created_at, \
         created_by, updated_at, updated_by) \
         VALUES ($1, $2, $3, 0, $4, NULL, $4, NULL)",
    )
    .bind(id)
    .bind(l1_id)
    .bind(name)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert group");
    id
}

/// 直插分组成员
async fn insert_group_member(pool: &PgPool, group_id: i64, l2_id: i64) -> i64 {
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let id = snowflake.next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_delivery_group_member (id, group_id, customer_id, created_at, created_by) \
         VALUES ($1, $2, $3, $4, NULL)",
    )
    .bind(id)
    .bind(group_id)
    .bind(l2_id)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert group member");
    id
}

/// 直插工种 + 工人
async fn insert_worker(
    pool: &PgPool,
    badge: &str,
    name: &str,
    is_active: bool,
    work_type_code: &str,
) -> i64 {
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    // 找或插工种
    let wt_id: i64 = sqlx::query_scalar("SELECT id FROM t_work_type WHERE code = $1 LIMIT 1")
        .bind(work_type_code)
        .fetch_optional(pool)
        .await
        .expect("query work_type")
        .unwrap_or_else(|| panic!("work_type {} not seeded", work_type_code));

    let id = snowflake.next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_worker (id, badge_code, name, is_active, work_type_id, version, \
         created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, $3, $4, $5, 0, $6, NULL, $6, NULL)",
    )
    .bind(id)
    .bind(badge)
    .bind(name)
    .bind(is_active)
    .bind(wt_id)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert worker");
    id
}

/// 直插一张 `DRAFT` 送货单（2026-10-08 起 `POST /` 手动建单端点已删，测试要造
/// 「一张已存在的草稿」只能走 SQL；生产路径只有 `POST /scan` 的 find-or-create）。
async fn insert_draft_note(pool: &PgPool, l1_id: i64) -> i64 {
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let id = snowflake.next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_delivery_note (id, delivery_note_no, customer_id, delivery_date, \
         status, version, created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, $3, $4, 'DRAFT', 0, $5, NULL, $5, NULL)",
    )
    .bind(id)
    .bind(format!("DN-T{:07}", id % 100_000_000))
    .bind(l1_id)
    .bind(now.date())
    .bind(now)
    .execute(pool)
    .await
    .expect("insert draft note");
    id
}

/// 把一个批次直接挂到某张单上（`delivery_note_id` UPDATE），绕过入单闸门 —— 用于
/// 造「已挂单的 READY_TO_SHIP 批次」这一前置状态（submit / soft-delete / pickup
/// 的测试需要它，而入单路径本身在 `entry_gate.rs` 里单独覆盖）。
async fn attach_batch(pool: &PgPool, batch_id: i64, note_id: i64) {
    let n = sqlx::query(
        "UPDATE t_part_batch SET delivery_note_id = $2, version = version + 1 \
         WHERE id = $1 AND deleted_at IS NULL",
    )
    .bind(batch_id)
    .bind(note_id)
    .execute(pool)
    .await
    .expect("attach batch")
    .rows_affected();
    assert_eq!(n, 1, "batch {batch_id} 应挂到 note {note_id}");
}

/// 调 `POST /api/v2/com/delivery/note/scan` 入单（2026-10-08 起的唯一入单入口）。
///
/// `entries` 传 `json!([{"node_kind":"PART","node_id":"<id>","quantity":n}])`
/// 这类数组（雪花 id 必须是 JSON **string**）。
async fn scan_entry(
    app: &axum::Router,
    token: &str,
    serial_no: &str,
    note_version: Option<i32>,
    entries: Value,
) -> (StatusCode, Value) {
    send(
        app.clone(),
        json_request(
            "POST",
            "/com/delivery/note/scan",
            Some(json!({
                "serial_no": serial_no,
                "note_version": note_version,
                "entries": entries,
            })),
            Some(token),
        ),
    )
    .await
}

// ===========================================================================
//  Tests
// ===========================================================================

#[tokio::test]
async fn counter_acquires_sequential_numbers() {
    use hsh_erp_rust::infra::serial::next_delivery_note_no;
    let (pool, _app, _token, _fx) = bootstrap_as_manager().await;

    let no1 = next_delivery_note_no(&pool, 0).await.expect("acquire 1");
    let no2 = next_delivery_note_no(&pool, 0).await.expect("acquire 2");

    assert!(no1.starts_with("DN-"), "no1 must start with DN-: {no1}");
    assert!(no2.starts_with("DN-"), "no2 must start with DN-: {no2}");

    // 取 NN 部分
    let nn1: i32 = no1.split('-').nth(2).unwrap().parse().unwrap();
    let nn2: i32 = no2.split('-').nth(2).unwrap().parse().unwrap();
    assert_eq!(nn1, 1, "first NN should be 1, got {nn1} from {no1}");
    assert_eq!(nn2, 2, "second NN should be 2, got {nn2} from {no2}");

    // 前缀（含日期）相同
    let prefix1: Vec<&str> = no1.split('-').collect();
    let prefix2: Vec<&str> = no2.split('-').collect();
    assert_eq!(
        prefix1[0..2],
        prefix2[0..2],
        "DN-{{ymd}} prefix should match"
    );
}

#[tokio::test]
async fn list_with_filters_status_and_pagination() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    // 建 3 张草稿（2026-10-08：`POST /` 手动建单端点已删，改直插）。
    // ⚠️ `uk_t_delivery_note_l1_open_draft` 保证同 L1 只有一张 DRAFT ⇒ 3 个不同 L1。
    for i in 0..3 {
        let prefix = fresh_prefix(&pool).await;
        let l1 = insert_l1(&pool, &format!("分页客户{i}"), &prefix).await;
        let id = insert_draft_note(&pool, l1).await;
        assert!(id > 0, "第 {i} 张草稿");
    }

    // status=DRAFT, limit=2 → 2 条 + total=3
    let (s, env) = send(
        app.clone(),
        json_request(
            "GET",
            "/com/delivery/note?statuses=DRAFT&limit=2&offset=0",
            None,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(env["data"]["total"], 3);
    assert_eq!(env["data"]["items"].as_array().unwrap().len(), 2);

    // customer_id 不存在 → 404 / 20102
    let (s2, env2) = send(
        app,
        json_request(
            "GET",
            "/com/delivery/note?customer_id=999999",
            None,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s2, StatusCode::OK);
    assert_eq!(env2["data"]["total"], 0);
}

#[tokio::test]
async fn get_with_parts_with_assembly_fields() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let l1 = insert_l1(&pool, "法拉电子", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let part_id = insert_part(&pool, "零件 A", l2, Some("A001")).await;
    let batch_id = insert_batch(&pool, part_id, 1, 5, "READY_TO_SHIP").await;
    let _ = l1;

    // 2026-10-08：入单只有 `POST /scan` 一个入口（`POST /` 手动建单已删）
    let (cs, env) = scan_entry(
        &app,
        &token,
        "A001",
        None,
        json!([{"node_kind": "PART", "node_id": part_id.to_string(), "quantity": 5}]),
    )
    .await;
    assert_eq!(cs, StatusCode::OK, "scan entry: {env}");
    let note_id = env["data"]["id"].as_str().unwrap().to_string();
    let head_no = env["data"]["delivery_note_no"]
        .as_str()
        .unwrap()
        .to_string();

    // GET /com/delivery/note/{id}
    let (gs, genv) = send(
        app.clone(),
        json_request(
            "GET",
            &format!("/com/delivery/note/{note_id}"),
            None,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(gs, StatusCode::OK, "get: {genv}");
    assert_eq!(genv["data"]["delivery_note_no"], head_no);
    let items = genv["data"]["line_items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    let item = &items[0];
    assert_eq!(item["id"].as_str().unwrap(), batch_id.to_string());
    assert_eq!(item["serial_no"], "A001");
    assert_eq!(item["quantity"], 5);
    assert_eq!(item["status"], "READY_TO_SHIP");
    // 无装配件时 assembly_* 为 null
    assert!(item["assembly_id"].is_null());
    assert!(item["assembly_drawing_no"].is_null());

    // 2026-10-08：`scanned_serials` 已从 VO 删除（恒空数组 + 送货台端点下线）
    assert!(genv["data"].get("scanned_serials").is_none());
    // 2026-10-08：3 个恒定值字段也已删除
    for gone in ["is_urgent", "is_scanned", "scanned"] {
        assert!(item.get(gone).is_none(), "{gone} 应已从 LineItem 删除");
    }
}

#[tokio::test]
async fn soft_delete_draft_ok_non_draft_returns_400_21403() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let l1 = insert_l1(&pool, "法拉电子", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let part_id = insert_part(&pool, "X", l2, Some("X009")).await;
    let batch_id = insert_batch(&pool, part_id, 1, 1, "READY_TO_SHIP").await;

    let note_id = insert_draft_note(&pool, l1).await;
    attach_batch(&pool, batch_id, note_id).await;

    // soft-delete DRAFT → ok
    let (s, _) = send(
        app.clone(),
        json_request(
            "POST",
            &format!("/com/delivery/note/{note_id}/soft-delete"),
            Some(json!({"version": 0})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK);

    // 新建一个 + 推到 READY + submit → soft-delete SUBMITTED → 21403
    let batch_id2 = insert_batch(&pool, part_id, 2, 1, "READY_TO_SHIP").await;
    let note_id2 = insert_draft_note(&pool, l1).await;
    attach_batch(&pool, batch_id2, note_id2).await;

    // 2026-10-08：submit 出参塌缩为 `R<String>`（送货单 id）
    let (sub_s, sub_env) = send(
        app.clone(),
        json_request(
            "POST",
            &format!("/com/delivery/note/{note_id2}/submit"),
            Some(json!({"version": 0})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(sub_s, StatusCode::OK, "submit: {sub_env}");
    assert_eq!(sub_env["data"], note_id2.to_string());

    let (s2, env2) = send(
        app,
        json_request(
            "POST",
            &format!("/com/delivery/note/{note_id2}/soft-delete"),
            Some(json!({"version": 1})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s2, StatusCode::BAD_REQUEST, "non-draft: {env2}");
    assert_eq!(env2["code"], 21403);
}

#[tokio::test]
async fn version_conflict_on_write_returns_409_40901() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let l1 = insert_l1(&pool, "法拉电子", "F").await;

    let note_id = insert_draft_note(&pool, l1).await;

    // 用错的 version submit → 40901
    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/com/delivery/note/{note_id}/submit"),
            Some(json!({"version": 999})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT);
    assert_eq!(env["code"], 40901);
}

/// 挂单批次被旁路改成 READY_TO_SHIP / INSPECTION 之外的状态 → 21421。
#[tokio::test]
async fn submit_with_illegal_batch_state_returns_21421() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let l1 = insert_l1(&pool, "法拉电子", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let part_id = insert_part(&pool, "X", l2, Some("X011")).await;
    let batch_id = insert_batch(&pool, part_id, 1, 1, "READY_TO_SHIP").await;

    let note_id = insert_draft_note(&pool, l1).await;
    attach_batch(&pool, batch_id, note_id).await;
    let note_version = 0;

    // 旁路改成非法状态
    sqlx::query("UPDATE t_part_batch SET status = 'PENDING' WHERE id = $1")
        .bind(batch_id)
        .execute(&pool)
        .await
        .expect("force illegal batch status");

    let (s, env) = send(
        app.clone(),
        json_request(
            "POST",
            &format!("/com/delivery/note/{note_id}/submit"),
            Some(json!({"version": note_version})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(env["code"], 21421, "illegal batch state: {s} {env}");

    // 单据未被提交
    let (_, denv) = send(
        app,
        json_request(
            "GET",
            &format!("/com/delivery/note/{note_id}"),
            None,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(denv["data"]["status"], "DRAFT");
}

#[tokio::test]
async fn batch_get_notes_returns_all_in_order_and_skips_missing() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;

    // 3 张 DRAFT 送货单。
    //
    // ⚠️ 2026-10-08 起 `uk_t_delivery_note_l1_open_draft` 保证「同 L1 只有一张
    // DRAFT」⇒ 这里给 3 个不同 L1 各建一张，而不是同 L1 建 3 张。
    let note_ids: Vec<i64> = {
        let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
        let mut ids = Vec::new();
        for i in 0..3 {
            let prefix = fresh_prefix(&pool).await;
            let l1 = insert_l1(&pool, &format!("批量客户{i}"), &prefix).await;
            let id = snowflake.next_id();
            // delivery_note_no 是 varchar(16)；雪花 id 17+ 位拼前缀会超 16 字符，
            // 这里手写 14-char 测试单号（DN-TEST-NNNN + i 适配）。
            let no = format!("DN-TEST-{i:04}");
            sqlx::query(
                "INSERT INTO t_delivery_note \
                 (id, delivery_note_no, customer_id, status, version, created_at, updated_at) \
                 VALUES ($1, $2, $3, 'DRAFT', 0, now(), now())",
            )
            .bind(id)
            .bind(no)
            .bind(l1)
            .execute(&pool)
            .await
            .expect("insert delivery note");
            ids.push(id);
        }
        ids
    };

    // 1) 全部存在 → 200, items.len() == 3, 顺序同入参
    let uri = format!(
        "/com/delivery/note/batch-detail?ids={},{},{}",
        note_ids[0], note_ids[1], note_ids[2]
    );
    let (status, body) = send(app.clone(), json_request("GET", &uri, None, Some(&token))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["code"], 0);
    assert_eq!(body["data"]["items"].as_array().unwrap().len(), 3);
    let r0 = body["data"]["items"][0]["id"].as_str().unwrap().to_string();
    let r1 = body["data"]["items"][1]["id"].as_str().unwrap().to_string();
    let r2 = body["data"]["items"][2]["id"].as_str().unwrap().to_string();
    assert_eq!(r0, note_ids[0].to_string());
    assert_eq!(r1, note_ids[1].to_string());
    assert_eq!(r2, note_ids[2].to_string());

    // 2) 中间缺失 → 200, items.len() == 2, 顺序 [a, c]
    let uri = format!(
        "/com/delivery/note/batch-detail?ids={},99999999,{}",
        note_ids[0], note_ids[2]
    );
    let (status, body) = send(app.clone(), json_request("GET", &uri, None, Some(&token))).await;
    assert_eq!(status, StatusCode::OK);
    let items = body["data"]["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["id"].as_str().unwrap(), note_ids[0].to_string());
    assert_eq!(items[1]["id"].as_str().unwrap(), note_ids[2].to_string());

    // 3) 缺 ids → 400 BIZ_INVALID_VALUE (20104)
    let (status, body) = send(
        app.clone(),
        json_request("GET", "/com/delivery/note/batch-detail", None, Some(&token)),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["code"], 20104);

    // 4) 201 项 → 400 BIZ_INVALID_VALUE
    let too_many: String = (1..=201)
        .map(|i| i.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let uri = format!("/com/delivery/note/batch-detail?ids={too_many}");
    let (status, body) = send(app, json_request("GET", &uri, None, Some(&token))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["code"], 20104);
}

/// 2026-09-02 回归测试 —— 详情接口 line_items 必须把 part 的 6 字段
/// （applicant_name / order_no / request_date / planned_delivery_date /
/// system_delivery_date / note）从 `TPart` 透传到 `DeliveryNoteLineItem`。
///
/// 历史 bug：`service/inner.rs::get_with_parts` 与 `service/crud.rs`
/// batch-detail 在组装 `DeliveryNoteLineItem` 时把这 6 个字段 hard-code
/// 为 `None`，导致前端 `DeliveryNoteLineItemsTable` 的订单号 / 申请人 /
/// 请购日期 / 计划交期 / 系统交期 / 备注 6 列永远显示 `—`。
/// 本测试锁定修复行为：建一个 6 字段都填的 part → 扫码入单 → GET 详情，
/// 断言 line_items[0].{6 字段} 等于 part 原值。
///
/// 2026-10-08：建单入口由 `POST /` 换成 `POST /scan`（入单唯一入口）。
#[tokio::test]
async fn test_get_delivery_note_line_items_fields_are_populated() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;

    // 1. 建 L1 客户（detail 端点的 customer_id 必须是 L1，service 校验）
    let l1 = insert_l1(&pool, "详情字段客户", "D").await;
    let _l2 = insert_l2(&pool, "二厂", l1).await;

    // 2. 直插 part（**6 字段全填**）：用 `sqlx::query()` 而非 `query!` 宏，
    //    避免为这条纯测试 INSERT 往 `.sqlx/` 离线缓存里塞新条目。
    //
    // 2026-09-16 PR-2（migration 027）：t_part 删 `has_been_repaired`；INSERT 列名
    // 与 VALUES 占位符同步移除 `false` 字面量。
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let part_id = snowflake.next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_part (id, serial_no, name, drawing_no, customer_id, status, \
         applicant_name, request_date, planned_delivery_date, system_delivery_date, \
         order_no, note, quantity, version, \
         created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, 'Detail-Fields-Part', 'D-002', $3, 'READY_TO_SHIP', \
         '张三', $4, $5, $6, \
         $7, $8, 1, 0, \
         $9, NULL, $9, NULL)",
    )
    .bind(part_id)
    .bind(Some("D-002-SN"))
    .bind(l1)
    .bind(chrono::NaiveDate::from_ymd_opt(2026, 9, 1).unwrap())
    .bind(chrono::NaiveDate::from_ymd_opt(2026, 9, 15).unwrap())
    .bind(chrono::NaiveDate::from_ymd_opt(2026, 9, 20).unwrap())
    .bind("ORD-2026-09-02-0001")
    .bind("测试备注 2026-09-02")
    .bind(now)
    .execute(&pool)
    .await
    .expect("insert part with full fields");

    // 3. 直插批次（READY_TO_SHIP 才能挂到 DRAFT 单；service add_parts 校验）
    let batch_id = insert_batch(&pool, part_id, 1, 5, "READY_TO_SHIP").await;

    // 4. 登录 → POST /com/delivery/note/scan 入单（2026-10-08 起的唯一入口）→ GET 详情
    let (cs, env) = scan_entry(
        &app,
        &token,
        "D-002-SN",
        None,
        json!([{"node_kind": "PART", "node_id": part_id.to_string(), "quantity": 5}]),
    )
    .await;
    assert_eq!(cs, StatusCode::OK, "scan entry: {env}");
    let note_id = env["data"]["id"].as_str().unwrap().to_string();
    assert_eq!(
        env["data"]["line_items"][0]["id"].as_str().unwrap(),
        batch_id.to_string(),
        "整批 5 件应整批入单、不拆批"
    );

    let (gs, genv) = send(
        app.clone(),
        json_request(
            "GET",
            &format!("/com/delivery/note/{note_id}"),
            None,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(gs, StatusCode::OK, "get detail: {genv}");

    // 5. 断言 line_items[0] 的 6 字段 == part 原值
    let items = genv["data"]["line_items"].as_array().unwrap();
    assert_eq!(items.len(), 1, "line_items 应包含挂上的 1 个批次");
    let item = &items[0];
    assert_eq!(
        item["applicant_name"].as_str(),
        Some("张三"),
        "applicant_name 透传"
    );
    assert_eq!(
        item["order_no"].as_str(),
        Some("ORD-2026-09-02-0001"),
        "order_no 透传"
    );
    assert_eq!(
        item["request_date"].as_str(),
        Some("2026-09-01"),
        "request_date 透传"
    );
    assert_eq!(
        item["planned_delivery_date"].as_str(),
        Some("2026-09-15"),
        "planned_delivery_date 透传"
    );
    assert_eq!(
        item["system_delivery_date"].as_str(),
        Some("2026-09-20"),
        "system_delivery_date 透传"
    );
    assert_eq!(
        item["note"].as_str(),
        Some("测试备注 2026-09-02"),
        "note 透传"
    );
}

// ===========================================================================
//  2026-10-04 新增：line_items 的装配件套数字段（只读展示用）
// ===========================================================================

/// 本节新增 helper 共用的雪花生成器。
///
/// ⚠️ 必须共享：每次 `SnowflakeIdGenerator::new()` 的首个 id 相同（sequence=0），
/// 各自 `new()` 的 helper 在**同一张表**插两行会直接撞主键。范式
/// `tests/com/union_list.rs::next_test_id`。
static SHARED_SNOWFLAKE: std::sync::OnceLock<SnowflakeIdGenerator> = std::sync::OnceLock::new();

fn next_shared_id() -> i64 {
    SHARED_SNOWFLAKE
        .get_or_init(|| SnowflakeIdGenerator::new(1_577_836_800_000, 1))
        .next_id()
}

/// 直插装配件（`quantity` = 工单总套数）。测试侧用 `sqlx::query()` 而非 `query!`
/// 宏，避免污染 `.sqlx/` 离线缓存（同本文件 `test_get_delivery_note_line_items_
/// fields_are_populated` 的约定）。
async fn insert_assembly(pool: &PgPool, customer_id: i64, name: &str, quantity: i32) -> i64 {
    let id = next_shared_id();
    let now = now_naive();
    let today = now.date();
    sqlx::query(
        "INSERT INTO t_assembly (id, drawing_no, name, applicant_name, customer_id, \
         request_date, planned_delivery_date, status, quantity, unit_price, total_price, \
         version, created_at, created_by, updated_at, updated_by) \
         VALUES ($1, 'ASM-001', $2, '', $3, $4, $4, 'ACTIVE', $5, 0, 0, 0, $6, NULL, $6, NULL)",
    )
    .bind(id)
    .bind(name)
    .bind(customer_id)
    .bind(today)
    .bind(quantity)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert assembly");
    id
}

/// 直插工单：`assembly_id = Some(..)` 是装配件子件，`None` 是散件；
/// `quantity` = 整单数量（每套需要「整单数量 / 装配件套数」件子）。
async fn insert_part_local(
    pool: &PgPool,
    customer_id: i64,
    name: &str,
    assembly_id: Option<i64>,
    quantity: i32,
) -> i64 {
    let id = next_shared_id();
    let now = now_naive();
    let today = now.date();
    sqlx::query(
        "INSERT INTO t_part (id, serial_no, name, drawing_no, customer_id, status, \
         applicant_name, request_date, planned_delivery_date, quantity, version, \
         created_at, created_by, updated_at, updated_by, assembly_id) \
         VALUES ($1, $2, $3, 'D-001', $4, 'READY_TO_SHIP', $3, $5, $5, $6, 0, \
         $7, NULL, $7, NULL, $8)",
    )
    .bind(id)
    .bind(Option::<String>::None) // serial_no 可空（varchar(15)，别塞雪花 id 进去）
    .bind(name)
    .bind(customer_id)
    .bind(today)
    .bind(quantity)
    .bind(now)
    .bind(assembly_id)
    .execute(pool)
    .await
    .expect("insert part");
    id
}

/// 直插一张送货单（默认 `DRAFT`）。
///
/// ⚠️ 2026-10-08 起数据库有部分唯一索引 `uk_t_delivery_note_l1_open_draft`
/// `(customer_id) WHERE status='DRAFT' AND deleted_at IS NULL` ⇒ **同一 L1 名下
/// 只能有一张 DRAFT**。同一 L1 要造第二张单时传 `status = "SUBMITTED"`。
async fn insert_note_row(pool: &PgPool, l1_id: i64, no: &str) -> i64 {
    insert_note_row_with_status(pool, l1_id, no, "DRAFT").await
}

/// [`insert_note_row`] 的显式 status 版本。
async fn insert_note_row_with_status(pool: &PgPool, l1_id: i64, no: &str, status: &str) -> i64 {
    let id = next_shared_id();
    sqlx::query(
        "INSERT INTO t_delivery_note \
         (id, delivery_note_no, customer_id, status, version, created_at, updated_at) \
         VALUES ($1, $2, $3, $4, 0, now(), now())",
    )
    .bind(id)
    .bind(no)
    .bind(l1_id)
    .bind(status)
    .execute(pool)
    .await
    .expect("insert delivery note");
    id
}

/// 直插一个**已挂在单上**的批次（`delivery_note_id` 直写，跳过 add_parts 的状态机
/// 校验 —— 详情与打印都只读「本单挂了哪些批次」）。
///
/// ⚠️ 不能复用本文件既有的 `insert_batch`：它每次 `SnowflakeIdGenerator::new()` 拿
/// 同一个 sequence=0 的 id，同一测试里连插 2 个批次就会撞主键。
async fn insert_note_batch(pool: &PgPool, part_id: i64, note_id: i64, quantity: i32) -> i64 {
    insert_note_batch_no(pool, part_id, note_id, quantity, 1).await
}

/// [`insert_note_batch`] 的显式 `batch_no` 版本。
///
/// 同一个 part 挂到**两张**单上时需要 `batch_no` 递进 —— `uq_t_part_batch_part_no`
/// 是 `(part_id, batch_no)` 唯一，两张单都写 `batch_no = 1` 会撞约束。
async fn insert_note_batch_no(
    pool: &PgPool,
    part_id: i64,
    note_id: i64,
    quantity: i32,
    batch_no: i32,
) -> i64 {
    let id = next_shared_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_part_batch (id, part_id, batch_no, quantity, status, \
         delivery_note_id, version, created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, $6, $3, 'READY_TO_SHIP', $4, 0, $5, NULL, $5, NULL)",
    )
    .bind(id)
    .bind(part_id)
    .bind(quantity)
    .bind(note_id)
    .bind(now)
    .bind(batch_no)
    .execute(pool)
    .await
    .expect("insert t_part_batch on note");
    id
}

/// 装配件子件行的 `assembly_quantity` / `shippable_sets` 必须填对；散件行为 null。
///
/// 数据：装配件 10 套；子件 A 整单 10 件 / 本单 8 件（8 套）；子件 B 整单 10 件 /
/// 本单 5 件（5 套）⇒ 两行的 `shippable_sets` 都是 5（同装配件口径一致），
/// `assembly_quantity` 都是 10；同单里的散件行两个字段都是 `null`。
#[tokio::test]
async fn get_with_parts_exposes_assembly_quantity_and_shippable_sets() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let l1 = insert_l1(&pool, "套数客户", "F").await;
    let asm_id = insert_assembly(&pool, l1, "套数装配体", 10).await;
    let note_id = insert_note_row(&pool, l1, "DN-TEST-9101").await;

    // 2 个装配件子件 + 1 个散件，各挂一个批次
    let mut child_ids = Vec::new();
    for (name, part_qty, note_qty) in [("子件A", 10, 8), ("子件B", 10, 5)] {
        let pid = insert_part_local(&pool, l1, name, Some(asm_id), part_qty).await;
        insert_note_batch(&pool, pid, note_id, note_qty).await;
        child_ids.push(pid);
    }
    let loose_id = insert_part_local(&pool, l1, "散件C", None, 10).await;
    insert_note_batch(&pool, loose_id, note_id, 3).await;

    let (gs, genv) = send(
        app,
        json_request(
            "GET",
            &format!("/com/delivery/note/{note_id}"),
            None,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(gs, StatusCode::OK, "get detail: {genv}");

    let items = genv["data"]["line_items"].as_array().unwrap();
    assert_eq!(items.len(), 3, "2 子件 + 1 散件: {genv}");
    for pid in &child_ids {
        let item = items
            .iter()
            .find(|i| i["part_id"].as_str() == Some(&pid.to_string()))
            .unwrap_or_else(|| panic!("找不到 part {pid} 的行: {genv}"));
        assert_eq!(
            item["assembly_id"].as_str(),
            Some(asm_id.to_string().as_str()),
            "子件行必须带装配件 id"
        );
        assert_eq!(item["assembly_quantity"], 10, "装配件工单总套数");
        assert_eq!(
            item["shippable_sets"], 5,
            "本单可出货套数 = min(子件 A 8 套, 子件 B 5 套)；同一装配件的所有行同值"
        );
    }
    let loose = items
        .iter()
        .find(|i| i["part_id"].as_str() == Some(&loose_id.to_string()))
        .expect("找不到散件行");
    assert!(
        loose["assembly_quantity"].is_null(),
        "散件行不应带装配件套数: {loose}"
    );
    assert!(
        loose["shippable_sets"].is_null(),
        "散件行本单可出货套数应为 null: {loose}"
    );
}

/// ★ 2026-10-04 review 第 1 轮（BLOCKER-1）：`min` 的定义域必须是「该装配件的
/// **全部**子件」，本单没交批次的子件按 0 参与。本例同时覆盖批量详情
/// （`get_many_with_parts`，1 条 SQL 批量取子件）这条链路。
///
/// 同 1 个装配件（10 套）、同 2 个子件 A / C（各整单 10 件），2 张单：
/// - 单 1：A 交 8 件、C 交 5 件 ⇒ min(8, 5) = **5** 套；
/// - 单 2：只交 A 8 件、C 一件没交 ⇒ min(8, 0) = **0** 套（凑不齐整套不能发）。
///
/// 若实现退回「只看本单批次行」的口径，单 2 会误判成 8 套。
#[tokio::test]
async fn batch_detail_shippable_sets_use_all_children_not_only_note_rows() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let l1 = insert_l1(&pool, "批量套数客户", "F").await;
    let asm_id = insert_assembly(&pool, l1, "批量套数装配体", 10).await;
    // ⚠️ 同 L1 只能有一张 DRAFT（uk_t_delivery_note_l1_open_draft）⇒ 单 2 用 SUBMITTED。
    // SUBMITTED 单上的批次照样计入该单的 `shippable_sets`（分子按批次状态筛，不按
    // 单据状态筛），所以本用例的口径不受影响。
    let note1 = insert_note_row(&pool, l1, "DN-TEST-9201").await;
    let note2 = insert_note_row_with_status(&pool, l1, "DN-TEST-9202", "SUBMITTED").await;

    let child_a = insert_part_local(&pool, l1, "批量子件A", Some(asm_id), 10).await;
    let child_c = insert_part_local(&pool, l1, "批量子件C", Some(asm_id), 10).await;
    insert_note_batch(&pool, child_a, note1, 8).await;
    insert_note_batch(&pool, child_c, note1, 5).await;
    // 单 2 只挂子件 A，子件 C 完全不在这张单上（同一 part 挂两张单 ⇒ batch_no 递进）
    insert_note_batch_no(&pool, child_a, note2, 8, 2).await;

    let uri = format!("/com/delivery/note/batch-detail?ids={note1},{note2}");
    let (status, env) = send(app, json_request("GET", &uri, None, Some(&token))).await;
    assert_eq!(status, StatusCode::OK, "batch detail: {env}");

    let items = env["data"]["items"].as_array().expect("items 必须是数组");
    assert_eq!(items.len(), 2, "两张单都要返回: {env}");

    let sets_of = |note_id: i64| -> Vec<i64> {
        let it = items
            .iter()
            .find(|i| i["id"].as_str() == Some(note_id.to_string().as_str()))
            .unwrap_or_else(|| panic!("找不到单 {note_id}: {env}"));
        it["line_items"]
            .as_array()
            .expect("line_items 必须是数组")
            .iter()
            .map(|li| {
                li["shippable_sets"]
                    .as_i64()
                    .unwrap_or_else(|| panic!("子件行必须有 shippable_sets: {li}"))
            })
            .collect()
    };

    assert_eq!(
        sets_of(note1),
        vec![5, 5],
        "单 1：子件 A 8 套 / 子件 C 5 套 ⇒ 5 套（同一装配件的所有行同值）"
    );
    assert_eq!(
        sets_of(note2),
        vec![0],
        "单 2：子件 C 本单没交批次（按 0 参与 min）⇒ 0 套，不能误判成 8 套"
    );
}

//! `line_items[].applicant_name` 的**装配件回落口径**（2026-10-10 新增）
//!
//! 口径本体在 `src/modules/com/delivery_note/service/line_item.rs::resolve_applicant_name`，
//! 本文件是它的集成测试，覆盖三条分支 + 两条出参路径：
//!
//! | # | 用例 | 断言 |
//! |---|---|---|
//! | 1 | `assembly_child_with_empty_applicant_falls_back_to_assembly` | 子件自身空串 ⇒ 行项给装配件的申请人（单张详情 + 批量详情） |
//! | 2 | `loose_part_with_empty_applicant_stays_null` | 散件自身空串 ⇒ 仍 `null`，**不回落**（单张详情 + 批量详情） |
//! | 3 | `assembly_child_with_own_applicant_keeps_its_own_value` | 子件自身有值且与装配件不同 ⇒ 给子件自己的（单张详情 + 批量详情） |
//!
//! ## 为什么必须有这几条
//!
//! 装配件**设计上**由父件向下继承申请人（`assembly` 域建单 / update 两条级联写路径），
//! 但存量数据里这条继承大量未落地 —— 开发库实测 490 条装配件子件中 360 条
//! `t_part.applicant_name` 是空串，而 90 个 `t_assembly.applicant_name` 无一为空。
//! 没有回落时这些行的申请人在详情列表、标签工作簿、打印模板三处全是「—」。
//!
//! ## 两条出参路径同源
//!
//! 单张详情（`GET /{id}` → `inner.rs::get_with_parts`）与批量详情
//! （`GET /batch-detail` → `crud.rs::get_many_with_parts`）**逐字共用**
//! `line_item::build_line_item`，故每个用例都断言两条路径给同一组值 —— 这正是
//! 「口径只维护一份」的可执行证明。

use axum::http::StatusCode;
use serde_json::{Value, json};
use sqlx::PgPool;

use hsh_erp_test_support::{
    DeliveryFixture, json_request, load_delivery_fixture, login_token, pool_snowflake, send,
    test_app, test_pool, test_state,
};

use hsh_erp_rust::infra::clock::now_naive;

/// 取一个测试用雪花 ID（进程内唯一 generator，见 test-support::snowflake）。
fn next_id() -> i64 {
    pool_snowflake().lock().expect("pool_snowflake").next_id()
}

// ===========================================================================
//  建数 helper
//
//  ⚠️ 本文件**不复用** `note.rs` 的 `insert_part` / `insert_assembly`：那几个 helper
//  把 `applicant_name` 写死成零件名 / 空串，本文件必须按用例显式控制申请人。
//  其余（L1 / L2 / 送货单 / 批次）与 `note.rs` 同形态（各测试进程内独立 database，
//  'F' 前缀可用）。
// ===========================================================================

async fn insert_l1(pool: &PgPool, name: &str, prefix: &str) -> i64 {
    let id = next_id();
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

async fn insert_l2(pool: &PgPool, name: &str, l1_id: i64) -> i64 {
    let id = next_id();
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

/// 直插装配件；`applicant_name` 由用例显式给（传 `""` 即「装配件自己也没申请人」）。
async fn insert_assembly(
    pool: &PgPool,
    customer_id: i64,
    name: &str,
    applicant_name: &str,
    quantity: i32,
) -> i64 {
    let id = next_id();
    let now = now_naive();
    let today = now.date();
    sqlx::query(
        "INSERT INTO t_assembly (id, drawing_no, name, applicant_name, customer_id, \
         request_date, planned_delivery_date, status, quantity, version, \
         created_at, created_by, updated_at, updated_by) \
         VALUES ($1, 'ASM-APPLICANT', $2, $3, $4, $5, $5, 'ACTIVE', $6, 0, $7, NULL, $7, NULL)",
    )
    .bind(id)
    .bind(name)
    .bind(applicant_name)
    .bind(customer_id)
    .bind(today)
    .bind(quantity)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert assembly");
    id
}

/// 直插工单；`assembly_id = Some(..)` 即装配件子件，`None` 即散件。
/// `applicant_name` 由用例显式给（`""` = 历史继承未落地的那种空串）。
async fn insert_part(
    pool: &PgPool,
    customer_id: i64,
    name: &str,
    assembly_id: Option<i64>,
    applicant_name: &str,
) -> i64 {
    let id = next_id();
    let now = now_naive();
    let today = now.date();
    sqlx::query(
        "INSERT INTO t_part (id, name, drawing_no, customer_id, status, applicant_name, \
         request_date, planned_delivery_date, quantity, version, \
         created_at, created_by, updated_at, updated_by, assembly_id) \
         VALUES ($1, $2, 'D-APPLICANT', $3, 'READY_TO_SHIP', $4, $5, $5, 10, 0, \
         $6, NULL, $6, NULL, $7)",
    )
    .bind(id)
    .bind(name)
    .bind(customer_id)
    .bind(applicant_name)
    .bind(today)
    .bind(now)
    .bind(assembly_id)
    .execute(pool)
    .await
    .expect("insert part");
    id
}

async fn insert_draft_note(pool: &PgPool, l1_id: i64, no: &str) -> i64 {
    let id = next_id();
    sqlx::query(
        "INSERT INTO t_delivery_note \
         (id, delivery_note_no, customer_id, status, version, created_at, updated_at) \
         VALUES ($1, $2, $3, 'DRAFT', 0, now(), now())",
    )
    .bind(id)
    .bind(no)
    .bind(l1_id)
    .execute(pool)
    .await
    .expect("insert delivery note");
    id
}

async fn insert_note_batch(pool: &PgPool, part_id: i64, note_id: i64, quantity: i32) -> i64 {
    let id = next_id();
    sqlx::query(
        "INSERT INTO t_part_batch (id, part_id, batch_no, quantity, status, \
         delivery_note_id, version, created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, 1, $3, 'READY_TO_SHIP', $4, 0, now(), NULL, now(), NULL)",
    )
    .bind(id)
    .bind(part_id)
    .bind(quantity)
    .bind(note_id)
    .execute(pool)
    .await
    .expect("insert t_part_batch on note");
    id
}

// ===========================================================================
//  查询 helper
// ===========================================================================

async fn bootstrap() -> (PgPool, axum::Router, String) {
    let pool = test_pool().await;
    let fx = load_delivery_fixture(&pool).await;
    let app = test_app(test_state(pool.clone()).await);
    let token = login_token(&app, &fx.part_manager_username, DeliveryFixture::PASSWORD).await;
    (pool, app, token)
}

/// 取一张单的行项（`Vec<Value>`），两条出参路径共用同一套断言。
async fn detail_items(app: &axum::Router, token: &str, note_id: i64) -> Vec<Value> {
    let (s, env) = send(
        app.clone(),
        json_request(
            "GET",
            &format!("/com/delivery/note/{note_id}"),
            None,
            Some(token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "单张详情: {env}");
    env["data"]["line_items"]
        .as_array()
        .unwrap_or_else(|| panic!("line_items 必须是数组: {env}"))
        .clone()
}

async fn batch_detail_items(app: &axum::Router, token: &str, note_id: i64) -> Vec<Value> {
    let (s, env) = send(
        app.clone(),
        json_request(
            "GET",
            &format!("/com/delivery/note/batch-detail?ids={note_id}"),
            None,
            Some(token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "批量详情: {env}");
    env["data"]["items"][0]["line_items"]
        .as_array()
        .unwrap_or_else(|| panic!("line_items 必须是数组: {env}"))
        .clone()
}

/// 按行项的 `name`（工单名）定位一行，并断言它在两条路径上给**同一个**申请人。
async fn assert_applicant_both_paths(
    app: &axum::Router,
    token: &str,
    note_id: i64,
    row_name: &str,
    expected: Option<&str>,
    why: &str,
) {
    let pick = |items: &Vec<Value>, ctx: &str| -> Value {
        items
            .iter()
            .find(|li| li["name"].as_str() == Some(row_name))
            .unwrap_or_else(|| panic!("找不到行 {row_name}: {ctx}"))
            .clone()
    };
    let expect_json = match expected {
        Some(s) => json!(s),
        None => Value::Null,
    };

    let detail = detail_items(app, token, note_id).await;
    let detail_row = pick(&detail, "单张详情");
    assert_eq!(
        detail_row["applicant_name"], expect_json,
        "单张详情 / {why}"
    );

    let batch = batch_detail_items(app, token, note_id).await;
    let batch_row = pick(&batch, "批量详情");
    assert_eq!(
        batch_row["applicant_name"], expect_json,
        "批量详情 / {why}（两条路径逐字共用 line_item::build_line_item）"
    );
}

// ===========================================================================
//  Tests
// ===========================================================================

/// 装配件子件 `t_part.applicant_name` 是空串 ⇒ 行项回落到装配件的申请人。
#[tokio::test]
async fn assembly_child_with_empty_applicant_falls_back_to_assembly() {
    let (pool, app, token) = bootstrap().await;
    let l1 = insert_l1(&pool, "回落客户", "F").await;
    let l2 = insert_l2(&pool, "回落二厂", l1).await;

    let asm_id = insert_assembly(&pool, l1, "回落装配件", "郭东海", 10).await;
    let note_id = insert_draft_note(&pool, l1, "DN-TEST-FB01").await;

    // 子件申请人 = 空串（存量继承未落地的形态）
    let child = insert_part(&pool, l2, "空申请人子件", Some(asm_id), "").await;
    insert_note_batch(&pool, child, note_id, 4).await;

    // 前提校验：库里确实是空串
    let raw: String = sqlx::query_scalar("SELECT applicant_name FROM t_part WHERE id = $1")
        .bind(child)
        .fetch_one(&pool)
        .await
        .expect("查子件申请人");
    assert_eq!(raw, "", "前提：子件自身的 applicant_name 是空串");

    assert_applicant_both_paths(
        &app,
        &token,
        note_id,
        "空申请人子件",
        Some("郭东海"),
        "子件为空 ⇒ 回落装配件",
    )
    .await;
}

/// 散件 `t_part.applicant_name` 是空串 ⇒ 行项仍给 `null`（**不回落**，无父装配件）。
#[tokio::test]
async fn loose_part_with_empty_applicant_stays_null() {
    let (pool, app, token) = bootstrap().await;
    let l1 = insert_l1(&pool, "散件客户", "F").await;
    let l2 = insert_l2(&pool, "散件二厂", l1).await;

    // ⚠️ 同一张单里放一个装配件，证明「不是同单有装配件就会全单回落」
    let asm_id = insert_assembly(&pool, l1, "同单装配件", "郭东海", 10).await;
    let note_id = insert_draft_note(&pool, l1, "DN-TEST-FB02").await;

    let loose = insert_part(&pool, l2, "空申请人散件", None, "").await;
    insert_note_batch(&pool, loose, note_id, 4).await;
    let child = insert_part(&pool, l2, "有申请人子件", Some(asm_id), "李四").await;
    insert_note_batch(&pool, child, note_id, 4).await;

    assert_applicant_both_paths(
        &app,
        &token,
        note_id,
        "空申请人散件",
        None,
        "散件行没有父装配件可回落 ⇒ 仍 null",
    )
    .await;
}

/// 子件自身有申请人且**与装配件不同** ⇒ 给子件自己的值（回落只在为空时发生）。
#[tokio::test]
async fn assembly_child_with_own_applicant_keeps_its_own_value() {
    let (pool, app, token) = bootstrap().await;
    let l1 = insert_l1(&pool, "自带客户", "F").await;
    let l2 = insert_l2(&pool, "自带二厂", l1).await;

    let asm_id = insert_assembly(&pool, l1, "自带装配件", "郭东海", 10).await;
    let note_id = insert_draft_note(&pool, l1, "DN-TEST-FB03").await;

    let child = insert_part(&pool, l2, "自带申请人子件", Some(asm_id), "李四").await;
    insert_note_batch(&pool, child, note_id, 4).await;

    assert_applicant_both_paths(
        &app,
        &token,
        note_id,
        "自带申请人子件",
        Some("李四"),
        "子件自身有值 ⇒ 不被装配件覆盖",
    )
    .await;

    // 前提校验：子件与装配件的申请人确实不同（本用例才有意义）
    let asm_applicant: String =
        sqlx::query_scalar("SELECT applicant_name FROM t_assembly WHERE id = $1")
            .bind(asm_id)
            .fetch_one(&pool)
            .await
            .expect("查装配件申请人");
    assert_eq!(asm_applicant, "郭东海", "前提：装配件申请人不是李四");
}

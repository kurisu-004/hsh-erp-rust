//! 建单期序列号派发 + 金额入参集成测试（2026-10-05 新增）
//!
//! 覆盖用户报障「pdf 上传批量创建工单后新工单没有分配序列号，单价也没有自动计入」：
//! 派发逻辑此前只存在于 `POST /parts/batch-with-pdfs`（且是 INSERT 后补 UPDATE 一列），
//! `POST /parts` 与 `POST /parts/batch` 从不派发；金额则在建单 DTO → `NewPartCreate`
//! → INSERT 三层全丢。本文件钉死收口后的行为：
//!
//! 1. `batch_create_dispatches_serial_and_keeps_prices` —— `POST /parts/batch`
//!    两件都拿到 `P` + 4 位序列号，item 传入的单价 / 总价原样落库；
//! 2. `batch_create_price_defaults_to_zero` —— 金额缺省落 `0`（不是 NULL）；
//! 3. `create_single_part_dispatches_serial` —— `POST /parts` 单件同样派发；
//! 4. `batch_create_rejects_when_l1_serial_prefix_missing` —— L1 未配 prefix
//!    → 20308 整单拒，且 t_part 行数不变；
//! 5. `batch_create_serials_are_unique_and_increasing` —— 同批 / 跨批都不重号且递增；
//! 6. `batch_with_pdfs_keeps_master_and_child_serial_pattern` —— 遗留端点回归：
//!    有 PDF 仍派 master 号 + `{master}-{NN}` 子件号；
//! 7. `batch_with_pdfs_without_pdf_leaves_serial_null` —— 无 PDF 时仍不派号；
//! 8. `batch_create_with_bindings_dispatches_serial_and_keeps_prices` —— 带
//!    `drawing_file` 的另一个 service 入口（`has_bindings` 分流）同样派号 + 透传金额；
//! 9. `batch_create_rejects_when_l1_parent_soft_deleted` —— L1 父行已软删 → 20102；
//! 10. `batch_create_rejects_when_serial_prefix_not_uppercase` —— prefix 非 A-Z → 20104。
//!
//! 基建沿用 `crud.rs` / `batch.rs` 的写法（`test_pool` + `load_part_fixture` +
//! `send` / `json_request` / `login_token`），不新建 helper 体系。

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use serde_json::{Value, json};
use sqlx::PgPool;

use hsh_erp_rust::shared::error::code;
use hsh_erp_test_support::fixture::PartFixture;
use hsh_erp_test_support::*;

// ===========================================================================
//  本文件私有 helper（与 crud.rs / batch.rs 同风格：sub-file 私有，不进 test-support）
// ===========================================================================

async fn bootstrap_as_manager() -> (PgPool, axum::Router, String, PartFixture) {
    let pool = test_pool().await;
    let fx = load_part_fixture(&pool).await;
    let app = test_app(test_state(pool.clone()).await);
    let token = login_token(&app, &fx.manager_username, PartFixture::PASSWORD).await;
    (pool, app, token, fx)
}

fn today() -> chrono::NaiveDate {
    chrono::Utc::now()
        .with_timezone(&chrono::FixedOffset::east_opt(8 * 3600).unwrap())
        .date_naive()
}

/// 断言序列号等于**预期字面值**（fixture L1 客户 prefix `'P'` + 4 位数字）。
///
/// `test_pool()` 给**每个测试** `CREATE DATABASE` 一个全新库
/// （`test-support/src/pool.rs` 的 `test_pool()`），`load_part_fixture` 预置的
/// `t_serial_counter('P', counter=0)` 因此恒为起点 ⇒ 首个派发号恒是 `P1000`、
/// 第 n 个恒是 `P{999+n}`（4 位号池 `1000 + counter % 9000`，counter 是下一个
/// 要发的池内下标）。所以断言直接写字面值，既验了「派发了」也钉死了 counter
/// 起点与逐件递增（哪天 fixture 那行不再是 `counter=0` 这些用例会红）。
fn assert_dispatched_serial(serial: Option<&str>, expected: &str, ctx: &str) -> String {
    let s = serial.unwrap_or_else(|| panic!("{ctx}: serial_no 应已派发，实际为 null"));
    assert_eq!(s, expected, "{ctx}: 序列号应为 {expected}（P + 4 位数字）");
    assert_eq!(s.len(), 5, "{ctx}: 序列号应为 5 字符，实际 {s:?}");
    assert!(
        s.starts_with('P'),
        "{ctx}: 序列号应以 L1 prefix 'P' 开头，实际 {s:?}"
    );
    assert!(
        s[1..].chars().all(|c| c.is_ascii_digit()),
        "{ctx}: 序列号后 4 位必须是数字，实际 {s:?}"
    );
    s.to_string()
}

/// 取序列号里的数字部分（用于递增断言）。
fn serial_number(s: &str) -> i64 {
    s[1..]
        .parse()
        .unwrap_or_else(|e| panic!("序列号 {s:?} 的数字部分解析失败: {e}"))
}

async fn count_parts(pool: &PgPool) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT count(*) FROM t_part")
        .fetch_one(pool)
        .await
        .expect("count t_part")
}

/// 造客户行用的雪花 ID：每次调用换一个 `instance_id`。
///
/// 同一 epoch + 同一 `instance_id` 的两个 `SnowflakeIdGenerator` 在同一毫秒会吐出
/// **相同**的 ID（同毫秒内 sequence 从 0 起），而本文件多个测试要在同一个测试里连插
/// L1 + L2 两行 ⇒ 固定 instance_id 会撞主键。用自增 instance_id 避开。
fn raw_customer_id() -> i64 {
    use hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator;
    use std::sync::atomic::{AtomicU16, Ordering};
    static INSTANCE: AtomicU16 = AtomicU16::new(900);
    let instance = INSTANCE.fetch_add(1, Ordering::SeqCst);
    SnowflakeIdGenerator::new(1_577_836_800_000, instance).next_id()
}

/// 直插一个 L1 客户行（绕过 customer 域 service 的前缀双校验，只为造 DB 形态）。
/// `prefix = None` 即 `serial_prefix IS NULL`。
async fn insert_l1_customer(pool: &PgPool, name: &str, prefix: Option<&str>) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;
    let id = raw_customer_id();
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

/// 直插一个 L2 客户行（`parent_id` 指 L1）。L2 的 `serial_prefix` 恒为 NULL。
async fn insert_l2_customer(pool: &PgPool, name: &str, parent_id: i64) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;
    let id = raw_customer_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_customer (id, name, parent_id, serial_prefix, version, \
         created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, $3, NULL, 0, $4, NULL, $4, NULL)",
    )
    .bind(id)
    .bind(name)
    .bind(parent_id)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert L2 customer");
    id
}

/// 软删一行客户。
async fn soft_delete_customer(pool: &PgPool, id: i64) {
    sqlx::query("UPDATE t_customer SET deleted_at = now() WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await
        .expect("soft delete customer");
}

/// 造 N 页 PDF（`batch-with-pdfs` 按 `lopdf` 解析出的页数决定派几个子件号）。
///
/// 关键：每个 page 用 `add_object` 注册，`Pages.Kids` 必须是
/// `Vec<Object::Reference(page_id)>`，否则 `get_pages().len()` 拿不到正确页数。
fn make_fixture_pdf(page_count: usize) -> Vec<u8> {
    use lopdf::{Document, Object, ObjectId, dictionary};
    let mut doc = Document::with_version("1.5");
    let pages_id = doc.new_object_id();
    let mut page_ids: Vec<ObjectId> = Vec::with_capacity(page_count);
    for _ in 1..=page_count {
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "Resources" => dictionary! {},
        });
        page_ids.push(page_id);
    }
    let pages = dictionary! {
        "Type" => "Pages",
        "Count" => page_count as i32,
        "Kids" => page_ids.iter().map(|id| Object::Reference(*id)).collect::<Vec<_>>(),
    };
    doc.objects.insert(pages_id, Object::Dictionary(pages));
    let catalog_id = doc.add_object(dictionary! {
        "Type" => "Catalog",
        "Pages" => pages_id,
    });
    doc.trailer.set("Root", catalog_id);
    let mut buf = Vec::new();
    doc.save_to(&mut buf).expect("save fixture pdf");
    buf
}

/// 手工拼 `multipart/form-data`（handler 只认 `json` + `pdf`/`file` 两个字段名）。
fn batch_with_pdfs_request(json_field: &str, pdfs: &[Vec<u8>], token: &str) -> Request<Body> {
    const BOUNDARY: &str = "----hshpartserialprice7f3a";
    let mut body: Vec<u8> = Vec::new();
    body.extend_from_slice(
        format!(
            "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"json\"\r\n\r\n{json_field}\r\n"
        )
        .as_bytes(),
    );
    for (i, pdf) in pdfs.iter().enumerate() {
        body.extend_from_slice(
            format!(
                "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"pdf\"; \
                 filename=\"p{i}.pdf\"\r\nContent-Type: application/pdf\r\n\r\n"
            )
            .as_bytes(),
        );
        body.extend_from_slice(pdf);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());
    Request::builder()
        .method("POST")
        .uri("/parts/batch-with-pdfs")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(
            header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .body(Body::from(body))
        .expect("build multipart request")
}

/// 构造 `items` 数组：单价 / 总价是 JSON **字符串**（rust_decimal 只开了
/// `serde-with-str`，写裸数字会在反序列化阶段 400）。
fn batch_items(n: usize, with_price: bool) -> Value {
    let today = today().to_string();
    let mut items = Vec::new();
    for i in 0..n {
        let mut item = json!({
            "name": format!("串号件-{i}"),
            "drawing_no": format!("D-SN-{i}"),
            "applicant_name": "甲",
            "quantity": 1,
            "request_date": today,
            "planned_delivery_date": today,
            "is_urgent": false,
        });
        if with_price {
            item["unit_price"] = json!("95.00");
            item["total_price"] = json!("95.00");
        }
        items.push(item);
    }
    Value::Array(items)
}

// ===========================================================================
//  1. POST /parts/batch：派发序列号 + 保留金额
// ===========================================================================

#[tokio::test]
async fn batch_create_dispatches_serial_and_keeps_prices() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let body = json!({
        "customer_id": fx.customer_l2_id.to_string(),
        "items": batch_items(2, true),
    });
    let (s, env) = send(
        app,
        json_request("POST", "/parts/batch", Some(body), Some(&token)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "batch create: {env}");
    assert_eq!(env["code"], 0, "batch create: {env}");
    let created = env["data"]["created"].as_array().expect("created 数组");
    assert_eq!(created.len(), 2, "两件都应成功: {env}");
    assert!(
        env["data"]["failed"]
            .as_array()
            .expect("failed 数组")
            .is_empty(),
        "不应有失败件: {env}"
    );

    // 全新测试库 ⇒ counter 从 0 起 ⇒ 两件分别是第 1、第 2 个号
    const EXPECTED: [&str; 2] = ["P1000", "P1001"];
    for (i, item) in created.iter().enumerate() {
        let serial = assert_dispatched_serial(
            item["serial_no"].as_str(),
            EXPECTED[i],
            &format!("created[{i}]"),
        );
        // 单价 / 总价：TPart 用 rust_decimal serde-with-str 序列化 → JSON 字符串
        assert_eq!(
            item["unit_price"].as_str(),
            Some("95.00"),
            "created[{i}].unit_price 应回显入参: {item}"
        );
        assert_eq!(
            item["total_price"].as_str(),
            Some("95.00"),
            "created[{i}].total_price 应回显入参: {item}"
        );
        // 响应来自 INSERT 后的 detail 回读，所以这里同时验证了「INSERT 期就写进去了」
        let db_serial: Option<String> =
            sqlx::query_scalar("SELECT serial_no FROM t_part WHERE id = $1::bigint")
                .bind(item["id"].as_str().unwrap())
                .fetch_one(&pool)
                .await
                .expect("查 t_part.serial_no");
        assert_eq!(
            db_serial.as_deref(),
            Some(serial.as_str()),
            "created[{i}]：DB 行 serial_no 必须与响应一致（INSERT 期写入，非事后 UPDATE）"
        );
    }
    let s1 = assert_dispatched_serial(created[0]["serial_no"].as_str(), "P1000", "created[0]");
    let s2 = assert_dispatched_serial(created[1]["serial_no"].as_str(), "P1001", "created[1]");
    assert!(
        serial_number(&s1) < serial_number(&s2),
        "同批内序列号应递增: {s1} vs {s2}"
    );
}

// ===========================================================================
//  2. POST /parts/batch：金额缺省落 0（不是 NULL）
// ===========================================================================

#[tokio::test]
async fn batch_create_price_defaults_to_zero() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let body = json!({
        "customer_id": fx.customer_l2_id.to_string(),
        "items": batch_items(1, false),
    });
    let (s, env) = send(
        app,
        json_request("POST", "/parts/batch", Some(body), Some(&token)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "batch create: {env}");
    let created = env["data"]["created"].as_array().expect("created 数组");
    assert_eq!(created.len(), 1, "一件应成功: {env}");
    let item = &created[0];
    assert_eq!(
        item["unit_price"].as_str(),
        Some("0"),
        "缺省单价应落 0（JSON 字符串）而非 null: {item}"
    );
    assert_eq!(
        item["total_price"].as_str(),
        Some("0"),
        "缺省总价应落 0（JSON 字符串）而非 null: {item}"
    );
    // 直接查 DB：两列是 NOT NULL，绑 NULL 会在 INSERT 就违反约束
    let (unit_price, total_price): (rust_decimal::Decimal, rust_decimal::Decimal) =
        sqlx::query_as("SELECT unit_price, total_price FROM t_part WHERE id = $1::bigint")
            .bind(item["id"].as_str().unwrap())
            .fetch_one(&pool)
            .await
            .expect("查 t_part 金额列");
    assert_eq!(
        unit_price,
        rust_decimal::Decimal::ZERO,
        "DB unit_price 应为 0"
    );
    assert_eq!(
        total_price,
        rust_decimal::Decimal::ZERO,
        "DB total_price 应为 0"
    );
}

// ===========================================================================
//  3. POST /parts 单件建单也派发
// ===========================================================================

#[tokio::test]
async fn create_single_part_dispatches_serial() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let today = today();
    let body = json!({
        "name": "单件建单",
        "drawing_no": "D-SINGLE",
        "applicant_name": "乙",
        "quantity": 3,
        "request_date": today,
        "planned_delivery_date": today,
        "is_urgent": false,
        "customer_id": fx.customer_l2_id.to_string(),
        "unit_price": "12.34",
        "total_price": "37.02",
    });
    let (s, env) = send(
        app,
        json_request("POST", "/parts", Some(body), Some(&token)),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "create part: {env}");
    let serial =
        assert_dispatched_serial(env["data"]["serial_no"].as_str(), "P1000", "POST /parts");
    assert_eq!(
        env["data"]["unit_price"].as_str(),
        Some("12.34"),
        "单件建单应保留单价: {env}"
    );
    assert_eq!(
        env["data"]["total_price"].as_str(),
        Some("37.02"),
        "单件建单应保留总价: {env}"
    );
    let db_serial: Option<String> =
        sqlx::query_scalar("SELECT serial_no FROM t_part WHERE id = $1::bigint")
            .bind(env["data"]["id"].as_str().unwrap())
            .fetch_one(&pool)
            .await
            .expect("查 t_part.serial_no");
    assert_eq!(db_serial.as_deref(), Some(serial.as_str()));
}

// ===========================================================================
//  4. L1 客户没配 serial_prefix → 20308 整单拒，且不落任何行
// ===========================================================================

#[tokio::test]
async fn batch_create_rejects_when_l1_serial_prefix_missing() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    // 先成功建 2 件，把「行数不变」变成有基线的断言
    let ok_body = json!({
        "customer_id": fx.customer_l2_id.to_string(),
        "items": batch_items(2, false),
    });
    let (s, env) = send(
        app.clone(),
        json_request("POST", "/parts/batch", Some(ok_body), Some(&token)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "前置建单应成功: {env}");
    let before = count_parts(&pool).await;
    assert_eq!(before, 2, "前置应已建 2 件");

    let l1_no_prefix = insert_l1_customer(&pool, "无 prefix 的 L1", None).await;
    let bad_body = json!({
        "customer_id": l1_no_prefix.to_string(),
        "items": batch_items(2, false),
    });
    let (s, env) = send(
        app,
        json_request("POST", "/parts/batch", Some(bad_body), Some(&token)),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::BAD_REQUEST,
        "L1 无 serial_prefix 应整单拒（2xxxx 兜底 400）: {env}"
    );
    assert_eq!(
        env["code"],
        code::BIZ_CUSTOMER_NO_SERIAL_PREFIX,
        "错误码应为 20308 BIZ_CUSTOMER_NO_SERIAL_PREFIX: {env}"
    );
    let after = count_parts(&pool).await;
    assert_eq!(
        after, before,
        "20308 fail-fast：t_part 行数必须不变（{before} → {after}）"
    );
}

// ===========================================================================
//  5. 序列号不重复 + 递增（同批 + 跨批）
// ===========================================================================

#[tokio::test]
async fn batch_create_serials_are_unique_and_increasing() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let mut all: Vec<String> = Vec::new();
    // 全新测试库 ⇒ counter 从 0 起；两轮各 3 件，号连续 P1000..=P1005
    const EXPECTED: [[&str; 3]; 2] = [["P1000", "P1001", "P1002"], ["P1003", "P1004", "P1005"]];
    for (round, expected) in EXPECTED.iter().enumerate() {
        let body = json!({
            "customer_id": fx.customer_l2_id.to_string(),
            "items": batch_items(3, false),
        });
        let (s, env) = send(
            app.clone(),
            json_request("POST", "/parts/batch", Some(body), Some(&token)),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "第 {round} 批建单: {env}");
        let created = env["data"]["created"].as_array().expect("created 数组");
        assert_eq!(created.len(), 3, "第 {round} 批应建 3 件: {env}");
        let round_serials: Vec<String> = created
            .iter()
            .enumerate()
            .map(|(i, item)| {
                assert_dispatched_serial(
                    item["serial_no"].as_str(),
                    expected[i],
                    &format!("round{round}[{i}]"),
                )
            })
            .collect();
        for w in round_serials.windows(2) {
            assert!(
                serial_number(&w[0]) < serial_number(&w[1]),
                "同批内应递增: {w:?}"
            );
        }
        all.extend(round_serials);
    }
    let mut sorted = all.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(sorted.len(), all.len(), "跨批序列号不得重复: {all:?}");
    assert!(
        serial_number(&all[0]) < serial_number(&all[3]),
        "第 2 批应整体大于第 1 批（counter 单调）: {all:?}"
    );
    // DB 侧同样唯一：uk_t_part_serial_no 在 INSERT 期就参与判定
    let db_serials: Vec<Option<String>> =
        sqlx::query_scalar("SELECT serial_no FROM t_part ORDER BY id")
            .fetch_all(&pool)
            .await
            .expect("查全部 serial_no");
    assert_eq!(db_serials.len(), 6, "共 6 件");
    for s in db_serials.iter().flatten() {
        assert!(all.contains(s), "DB serial {s} 应在响应集合内: {all:?}");
    }
}

// ===========================================================================
//  6-7. POST /parts/batch-with-pdfs 回归（遗留端点，行为不得变）
// ===========================================================================

#[tokio::test]
async fn batch_with_pdfs_keeps_master_and_child_serial_pattern() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let today = today().to_string();
    let json_field = json!({
        "customer_id": fx.customer_l2_id.to_string(),
        "applicant_name": "丙",
        "request_date": today,
        "planned_delivery_date": today,
        "is_urgent": false,
    })
    .to_string();
    // 3 页 PDF → master + 2 子件
    let (s, env) = send(
        app,
        batch_with_pdfs_request(&json_field, &[make_fixture_pdf(3)], &token),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "batch-with-pdfs: {env}");
    let master = assert_dispatched_serial(env["data"]["serial_no"].as_str(), "P1000", "master");
    let master_id = env["data"]["id"].as_str().expect("master id").to_string();

    let child_serials: Vec<Option<String>> = sqlx::query_scalar(
        "SELECT serial_no FROM t_part \
         WHERE drawing_no LIKE 'ASM-%-01' OR drawing_no LIKE 'ASM-%-02' \
         ORDER BY drawing_no",
    )
    .fetch_all(&pool)
    .await
    .expect("查子件 serial_no");
    assert_eq!(
        child_serials,
        vec![Some(format!("{master}-01")), Some(format!("{master}-02"))],
        "子件序列号应派生自 master 号（{master}），且由 INSERT 期写入"
    );
    let master_db: Option<String> =
        sqlx::query_scalar("SELECT serial_no FROM t_part WHERE id = $1::bigint")
            .bind(&master_id)
            .fetch_one(&pool)
            .await
            .expect("查 master serial_no");
    assert_eq!(master_db.as_deref(), Some(master.as_str()));
}

#[tokio::test]
async fn batch_with_pdfs_without_pdf_leaves_serial_null() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let today = today().to_string();
    let json_field = json!({
        "customer_id": fx.customer_l2_id.to_string(),
        "applicant_name": "丁",
        "request_date": today,
        "planned_delivery_date": today,
        "is_urgent": false,
    })
    .to_string();
    let (s, env) = send(app, batch_with_pdfs_request(&json_field, &[], &token)).await;
    assert_eq!(s, StatusCode::OK, "batch-with-pdfs（无 PDF）: {env}");
    assert!(
        env["data"]["serial_no"].is_null(),
        "PDF 页数为 0 时不应派发序列号: {env}"
    );
    let master_id = env["data"]["id"].as_str().expect("master id").to_string();
    let master_db: Option<String> =
        sqlx::query_scalar("SELECT serial_no FROM t_part WHERE id = $1::bigint")
            .bind(&master_id)
            .fetch_one(&pool)
            .await
            .expect("查 master serial_no");
    assert!(
        master_db.is_none(),
        "DB 里 master.serial_no 应为 NULL: {master_db:?}"
    );
    assert_eq!(count_parts(&pool).await, 1, "无 PDF 时只建 master 一件");
}

// ===========================================================================
//  8. 带文件绑定的 with_bindings 分支：同样派号 + 透传金额
// ===========================================================================

/// 2026-10-05 补：handler 按 `has_bindings` 分流，带 `drawing_file` 时走
/// `batch_create_parts_with_bindings`（另一个 service 入口）。该入口的 `acquire`
/// 刻意放在 per-item SAVEPOINT **之前**（失败时 counter 不回退，代价是号留空洞），
/// 此前只有「补两个字段」的覆盖、零 serial_no / 金额断言，这里钉死它与 legacy
/// 分支同口径。
#[tokio::test]
async fn batch_create_with_bindings_dispatches_serial_and_keeps_prices() {
    let pool = test_pool().await;
    let fx = load_part_fixture(&pool).await;
    let cos = std::sync::Arc::new(MockCos::new());
    let tmp_key = "tmp/test/with-bindings.pdf";
    cos.set_head(tmp_key, 1024);
    let app = test_app(test_state_with_cos(pool.clone(), cos.clone()).await);
    let token = login_token(&app, &fx.manager_username, PartFixture::PASSWORD).await;

    let today = today().to_string();
    let body = json!({
        "customer_id": fx.customer_l2_id.to_string(),
        "items": [{
            "name": "带图纸件",
            "drawing_no": "D-BIND",
            "applicant_name": "甲",
            "quantity": 1,
            "request_date": today,
            "planned_delivery_date": today,
            "is_urgent": false,
            "unit_price": "88.80",
            "total_price": "177.60",
            "drawing_file": {
                "tmp_key": tmp_key,
                "content_sha256": "c".repeat(64),
                "original_filename": "with-bindings.pdf",
                "file_size": "1024",
                "content_type": "application/pdf",
            },
            "model3d_file": null,
        }],
    });
    let (s, env) = send(
        app,
        json_request("POST", "/parts/batch", Some(body), Some(&token)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "batch create（带绑定）: {env}");
    assert_eq!(env["code"], 0, "batch create（带绑定）: {env}");
    let created = env["data"]["created"].as_array().expect("created 数组");
    assert_eq!(created.len(), 1, "一件应成功: {env}");
    assert!(
        env["data"]["failed"]
            .as_array()
            .expect("failed 数组")
            .is_empty(),
        "不应有失败件: {env}"
    );
    let item = &created[0];
    // 全新库 ⇒ counter 从 0 起 ⇒ 本件拿到第 1 个号
    let serial = assert_dispatched_serial(item["serial_no"].as_str(), "P1000", "created[0]");
    assert_eq!(
        item["unit_price"].as_str(),
        Some("88.80"),
        "with_bindings 分支也要透传单价: {item}"
    );
    assert_eq!(
        item["total_price"].as_str(),
        Some("177.60"),
        "with_bindings 分支也要透传总价: {item}"
    );
    let (db_serial, unit_price, total_price): (
        Option<String>,
        rust_decimal::Decimal,
        rust_decimal::Decimal,
    ) = sqlx::query_as(
        "SELECT serial_no, unit_price, total_price FROM t_part WHERE id = $1::bigint",
    )
    .bind(item["id"].as_str().unwrap())
    .fetch_one(&pool)
    .await
    .expect("查 t_part");
    assert_eq!(
        db_serial.as_deref(),
        Some(serial.as_str()),
        "DB 行 serial_no 必须与响应一致（INSERT 期写入）"
    );
    assert_eq!(
        unit_price,
        rust_decimal::Decimal::new(8880, 2),
        "DB 单价 88.80"
    );
    assert_eq!(
        total_price,
        rust_decimal::Decimal::new(17760, 2),
        "DB 总价 177.60"
    );
    // 确实走了 with_bindings 分支（不是 legacy）：part_file 行存在
    let file_rows: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM t_part_file WHERE part_id = $1::bigint AND kind = 'DRAWING'",
    )
    .bind(item["id"].as_str().unwrap())
    .fetch_one(&pool)
    .await
    .expect("count t_part_file");
    assert_eq!(
        file_rows, 1,
        "drawing_file 绑定应落 1 行 t_part_file: {env}"
    );
    // counter 恰好被推进 1 次（1 件 1 号，无重复派发）
    let counter: i64 =
        sqlx::query_scalar("SELECT counter FROM t_serial_counter WHERE prefix = 'P'")
            .fetch_one(&pool)
            .await
            .expect("查 t_serial_counter");
    assert_eq!(counter, 1, "1 件只该派 1 个号，counter 应为 1");
}

// ===========================================================================
//  9-10. serial_prefix_for_customer 的负向分支（20102 / 20104）
// ===========================================================================

/// 2026-10-05 补：L2 自身未软删、但 L1 父行已软删。`CustomerRepo::get_by_id` 只看
/// 自己那一行（不递归父行）所以客户存在性检查过得去，挂在
/// `serial_prefix_for_customer` 的内层 `COALESCE` 折回后外层查不到未删的 L1 ⇒
/// 20102 整批拒，且不落任何行。
#[tokio::test]
async fn batch_create_rejects_when_l1_parent_soft_deleted() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    // 先建 2 件，让「行数不变」有基线
    let ok_body = json!({
        "customer_id": fx.customer_l2_id.to_string(),
        "items": batch_items(2, false),
    });
    let (s, env) = send(
        app.clone(),
        json_request("POST", "/parts/batch", Some(ok_body), Some(&token)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "前置建单应成功: {env}");
    let before = count_parts(&pool).await;
    assert_eq!(before, 2, "前置应已建 2 件");

    let l1 = insert_l1_customer(&pool, "将软删的 L1", Some("Q")).await;
    let l2 = insert_l2_customer(&pool, "L1 已软删的 L2", l1).await;
    soft_delete_customer(&pool, l1).await;

    let bad_body = json!({
        "customer_id": l2.to_string(),
        "items": batch_items(2, false),
    });
    let (s, env) = send(
        app,
        json_request("POST", "/parts/batch", Some(bad_body), Some(&token)),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::NOT_FOUND,
        "L1 父行已软删应整批拒（20102 映射 404）: {env}"
    );
    assert_eq!(
        env["code"],
        code::BIZ_CUSTOMER_NOT_FOUND,
        "错误码应为 20102 BIZ_CUSTOMER_NOT_FOUND: {env}"
    );
    assert_eq!(
        count_parts(&pool).await,
        before,
        "20102 fail-fast：t_part 行数必须不变"
    );
}

/// 2026-10-05 补：`serial_prefix` 非 A-Z 的兜底分支（20104）。
///
/// 该形态被 DB CHECK `ck_t_customer_serial_prefix_uppercase`（`^[A-Z]$`）挡在正常
/// 写入路径外，所以本测试先摘掉该 CHECK 造脏数据 —— 目的是钉死
/// `serial_prefix_for_customer` 遇到它返回 20104（而不是 500 或静默用一个非法 prefix
/// 去 `acquire`）。摘 CHECK 只影响本测试的 fresh DB。
/// 取舍理由与参照范式写在 fixture 头部注释
/// （`test-support/fixtures/part.sql` 的「造『客户 serial_prefix 非 A-Z』脏数据时
/// 要改 schema」一节），改 fixture / 造客户数据前先看那段。
#[tokio::test]
async fn batch_create_rejects_when_serial_prefix_not_uppercase() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    sqlx::query("ALTER TABLE t_customer DROP CONSTRAINT ck_t_customer_serial_prefix_uppercase")
        .execute(&pool)
        .await
        .expect("drop ck_t_customer_serial_prefix_uppercase");
    let bad_l1 = insert_l1_customer(&pool, "prefix 非大写的 L1", Some("1")).await;
    let before = count_parts(&pool).await;

    let body = json!({
        "customer_id": bad_l1.to_string(),
        "items": batch_items(1, false),
    });
    let (s, env) = send(
        app,
        json_request("POST", "/parts/batch", Some(body), Some(&token)),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::BAD_REQUEST,
        "serial_prefix 非 A-Z 应整批拒（2xxxx 兜底 400）: {env}"
    );
    assert_eq!(
        env["code"],
        code::BIZ_INVALID_VALUE,
        "错误码应为 20104 BIZ_INVALID_VALUE: {env}"
    );
    assert_eq!(
        count_parts(&pool).await,
        before,
        "20104 fail-fast：t_part 行数必须不变"
    );
}

//! 建单期序列号派发 + 金额入参集成测试（2026-10-05 新增）
//!
//! 覆盖用户报障「pdf 上传批量创建工单后新工单没有分配序列号，单价也没有自动计入」：
//! 派发逻辑此前只存在于 `POST /parts/batch-with-pdfs`（且是 INSERT 后补 UPDATE 一列），
//! `POST /parts` 与 `POST /parts/batch` 从不派发；金额则在建单 DTO → `NewPartCreate`
//! → INSERT 三层全丢。本文件钉死收口后的行为：
//!
//! 1. `batch_create_dispatches_serial_and_keeps_prices` —— `POST /parts/batch`
//!    两件都拿到 `P` + 7 位序列号，item 传入的单价 / 总价原样落库；
//! 2. `batch_create_price_defaults_to_zero` —— 金额缺省落 `0`（不是 NULL）；
//! 3. `create_single_part_dispatches_serial` —— `POST /parts` 单件同样派发；
//! 4. `batch_create_rejects_when_l1_serial_prefix_missing` —— L1 未配 prefix
//!    → 20308 整单拒，且 t_part 行数不变；
//! 5. `batch_create_serials_are_unique_and_increasing` —— 同批 / 跨批都不重号且递增；
//! 6. `batch_with_pdfs_keeps_master_and_child_serial_pattern` —— 遗留端点回归：
//!    有 PDF 仍派 master 号 + `{master}-{NN}` 子件号；
//! 7. `batch_with_pdfs_without_pdf_leaves_serial_null` —— 无 PDF 时仍不派号。
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

/// 断言序列号是 fixture L1 客户 prefix（`'P'`）+ 7 位数字。
///
/// 只校验**格式**不校验字面值：`t_serial_counter` 的起始值取决于该测试库此前被
/// 派过多少次（`cargo test` 单进程多线程下同 binary 共用一个库），写死
/// `P0000001` 会让断言依赖执行顺序。
fn assert_dispatched_serial(serial: Option<&str>, ctx: &str) -> String {
    let s = serial.unwrap_or_else(|| panic!("{ctx}: serial_no 应已派发，实际为 null"));
    assert_eq!(
        s.len(),
        8,
        "{ctx}: 序列号应为 prefix + 7 位数字（8 字符），实际 {s:?}"
    );
    assert!(
        s.starts_with('P'),
        "{ctx}: 序列号应以 L1 prefix 'P' 开头，实际 {s:?}"
    );
    assert!(
        s[1..].chars().all(|c| c.is_ascii_digit()),
        "{ctx}: 序列号后 7 位必须是数字，实际 {s:?}"
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

/// 造一个 L1 客户但**不配** `serial_prefix`（customer 域 service 会拒这种组合，
/// 只能直插；用来验 20308 分支）。
async fn insert_l1_customer_without_prefix(pool: &PgPool, name: &str) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;
    use hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator;
    let id = SnowflakeIdGenerator::new(1_577_836_800_000, 7).next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_customer (id, name, parent_id, serial_prefix, version, \
         created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, NULL, NULL, 0, $3, NULL, $3, NULL)",
    )
    .bind(id)
    .bind(name)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert L1 customer without serial_prefix");
    id
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

    for (i, item) in created.iter().enumerate() {
        let serial = assert_dispatched_serial(item["serial_no"].as_str(), &format!("created[{i}]"));
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
    let s1 = assert_dispatched_serial(created[0]["serial_no"].as_str(), "created[0]");
    let s2 = assert_dispatched_serial(created[1]["serial_no"].as_str(), "created[1]");
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
    let serial = assert_dispatched_serial(env["data"]["serial_no"].as_str(), "POST /parts");
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

    let l1_no_prefix = insert_l1_customer_without_prefix(&pool, "无 prefix 的 L1").await;
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
    for round in 0..2 {
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
                assert_dispatched_serial(item["serial_no"].as_str(), &format!("round{round}[{i}]"))
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
    let master = assert_dispatched_serial(env["data"]["serial_no"].as_str(), "master");
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

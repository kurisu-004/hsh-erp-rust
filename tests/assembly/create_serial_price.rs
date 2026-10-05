//! `POST /api/v2/assemblies` 建单期序列号派发 + 子件金额集成测试（2026-10-05 新增）
//!
//! 覆盖用户报障「合成装配件后实际也没有创建为装配件」的后端侧根因：该端点此前把
//! 序列号派发与子件创建挂在「有没有传 PDF」上，不传 PDF 就只建一个空父件
//! （`t_assembly.serial_no = NULL`、0 子件）。本文件钉死放开后的行为：
//!
//! 1. `create_without_pdf_dispatches_serial_and_children` —— 不传 PDF：父件拿
//!    `P` + 4 位序列号，N 个子件全建、`serial_no` 为 `{asm}-{i:02d}`（01 起）、
//!    `assembly_id` 指向父件、初始批次同步建；
//! 2. `create_with_pdf_dispatches_serial_and_children` —— 传 PDF（页数相符）：结果
//!    与不传 PDF 完全一致（PDF 只用于页数校验，不入库）；
//! 3. `create_accepts_singular_file_field` —— multipart 字段名 `file` / `files`
//!    等价：单数名也必须真的参与页数校验（页数不符要报 20305，而不是被静默丢弃）；
//! 4. `create_persists_child_prices` —— 子件 `unit_price` / `total_price` 入参
//!    原样落库并回显；缺省落 `0`（不是 NULL）；
//! 5. `create_pdf_page_mismatch_rejects_and_keeps_counter` —— 放开门槛没有顺带
//!    去掉页数校验；且页数校验排在派发前，20305 时计数器不动、不落任何行；
//! 6. `create_rejects_when_l1_serial_prefix_missing` —— L1 未配 prefix → 20308，
//!    行数不变；
//! 7. `create_rejects_when_serial_prefix_not_registered` —— prefix 未在
//!    `t_serial_counter` 注册 → 20108，行数不变；
//! 8. `create_rejects_when_customer_not_l2_leaf` —— customer 传 L1 → 20302，
//!    行数不变。
//!
//! 全程走 HTTP（`POST /assemblies` 是 multipart 端点，service 直调验证不了字段名
//! 解析），基建沿用 `tests/part/create_serial_price.rs` 的写法：`test_pool` +
//! `load_part_fixture`（其 L1 客户 `serial_prefix='P'` 且 fixture 自带
//! `t_serial_counter('P', 0)`）+ `test_app` / `login_token` / `send`。

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use serde_json::{Value, json};
use sqlx::PgPool;

use hsh_erp_rust::shared::error::code;
use hsh_erp_test_support::fixture::PartFixture;
use hsh_erp_test_support::*;

// ===========================================================================
//  本文件私有 helper
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

/// 造 N 页 PDF（`lopdf` 解析出的页数决定端点是否接受）。
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

/// 拼 `POST /assemblies` 的 multipart 请求。
///
/// `pdf_field_name` 显式参数化：`files` / `file` 两个名字必须等价，测试要能分别
/// 发。`None` = 只发 `data`（不传 PDF）。
fn create_assembly_request(
    data_json: &Value,
    pdf_field_name: Option<&str>,
    pdf: Option<&[u8]>,
    token: &str,
) -> Request<Body> {
    const BOUNDARY: &str = "----hshassemblycreate5a1c";
    let mut body: Vec<u8> = Vec::new();
    body.extend_from_slice(
        format!(
            "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"data\"\r\n\
             Content-Type: application/json\r\n\r\n{data_json}\r\n"
        )
        .as_bytes(),
    );
    if let (Some(field_name), Some(pdf)) = (pdf_field_name, pdf) {
        body.extend_from_slice(
            format!(
                "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"{field_name}\"; \
                 filename=\"master.pdf\"\r\nContent-Type: application/pdf\r\n\r\n"
            )
            .as_bytes(),
        );
        body.extend_from_slice(pdf);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());
    Request::builder()
        .method("POST")
        .uri("/assemblies")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(
            header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .body(Body::from(body))
        .expect("build multipart request")
}

/// 建单入参：`children` 逐个给 `name` / `drawing_no` / 可选价格。
fn create_body(customer_id: i64, children: &[(&str, Option<&str>, Option<&str>)]) -> Value {
    let t = today().to_string();
    json!({
        "drawing_no": "D-ASM-CREATE",
        "name": "装配件-建单",
        "applicant_name": "甲",
        "customer_id": customer_id.to_string(),
        "request_date": t,
        "planned_delivery_date": t,
        "is_urgent": false,
        "quantity": 1,
        "children": children.iter().map(|(name, drawing_no, price)| {
            let mut c = json!({
                "name": name,
                "drawing_no": drawing_no,
                "quantity": 1,
            });
            if let Some(p) = price {
                c["unit_price"] = json!(p);
                c["total_price"] = json!(p);
            }
            c
        }).collect::<Vec<_>>(),
    })
}

/// 断言序列号是「P + 4 位数字」形态；`test_pool()` 每测试一个 fresh database，
/// fixture 预置的 `t_serial_counter('P', 0)` 因而是恒定起点 ⇒ 首个派发号恒为
/// `P1000`，直接写字面值既验了「派发了」也钉死了 counter 起点。
fn assert_p_serial(serial: Option<&str>, expected: &str, ctx: &str) -> String {
    let s = serial.unwrap_or_else(|| panic!("{ctx}: serial_no 应已派发，实际为 null"));
    assert_eq!(s, expected, "{ctx}: 序列号应为 {expected}（P + 4 位数字）");
    assert_eq!(s.len(), 5, "{ctx}: 序列号应为 5 字符，实际 {s:?}");
    assert!(
        s.starts_with('P'),
        "{ctx}: 应以 L1 prefix 'P' 开头，实际 {s:?}"
    );
    assert!(
        s[1..].chars().all(|c| c.is_ascii_digit()),
        "{ctx}: 后 4 位必须是数字，实际 {s:?}"
    );
    s.to_string()
}

async fn count_assemblies(pool: &PgPool) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT count(*) FROM t_assembly")
        .fetch_one(pool)
        .await
        .expect("count t_assembly")
}

async fn count_parts(pool: &PgPool) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT count(*) FROM t_part")
        .fetch_one(pool)
        .await
        .expect("count t_part")
}

async fn counter_of(pool: &PgPool, prefix: &str) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT counter FROM t_serial_counter WHERE prefix = $1")
        .bind(prefix)
        .fetch_one(pool)
        .await
        .expect("查 t_serial_counter")
}

/// 造客户行用的雪花 ID：每次调用换一个 `instance_id`（同毫秒固定 instance 会撞主键）。
fn raw_customer_id() -> i64 {
    use hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator;
    use std::sync::atomic::{AtomicU16, Ordering};
    static INSTANCE: AtomicU16 = AtomicU16::new(910);
    let instance = INSTANCE.fetch_add(1, Ordering::SeqCst);
    SnowflakeIdGenerator::new(1_577_836_800_000, instance).next_id()
}

/// 直插 L1 客户（绕开 customer 域 service 的前缀校验，只为造 DB 形态）。
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

// ===========================================================================
//  1-2. 无条件派发序列号 + 无条件建子件
// ===========================================================================

/// 1. 不传 PDF 也必须派号 + 建全部子件（本轮根因的主回归）。
#[tokio::test]
async fn create_without_pdf_dispatches_serial_and_children() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let body = create_body(
        fx.customer_l2_id,
        &[
            ("子件-1", Some("D-C-1"), None),
            ("子件-2", Some("D-C-2"), None),
        ],
    );

    let (s, env) = send(app, create_assembly_request(&body, None, None, &token)).await;
    assert_eq!(s, StatusCode::CREATED, "无 PDF 建单应成功: {env}");
    assert_eq!(env["code"], 0, "建单应成功: {env}");

    let asm_serial = assert_p_serial(
        env["data"]["assembly"]["serial_no"].as_str(),
        "P1000",
        "无 PDF 建单的父件",
    );

    let children = env["data"]["created_children"]
        .as_array()
        .expect("created_children 数组");
    assert_eq!(
        children.len(),
        2,
        "无 PDF 也应建出全部子件（不传 PDF 不再是「不建子件」的信号）: {env}"
    );
    for (i, c) in children.iter().enumerate() {
        let want = format!("{}-{:02}", asm_serial, i + 1);
        assert_eq!(
            c["serial_no"].as_str(),
            Some(want.as_str()),
            "子件序列号应为 {want}（1-based 两位）: {env}"
        );
    }

    // DB 侧：子件行 + assembly_id 指向父件 + 初始批次
    let asm_id: i64 = env["data"]["assembly"]["id"]
        .as_str()
        .expect("assembly.id 是字符串（雪花）")
        .parse()
        .expect("assembly.id 可解析为 i64");
    let rows: Vec<(String, Option<i64>, i32)> = sqlx::query_as(
        "SELECT serial_no, assembly_id, quantity FROM t_part \
         WHERE assembly_id = $1 ORDER BY serial_no ASC",
    )
    .bind(asm_id)
    .fetch_all(&pool)
    .await
    .expect("query 子件行");
    assert_eq!(
        rows,
        vec![
            (format!("{asm_serial}-01"), Some(asm_id), 1),
            (format!("{asm_serial}-02"), Some(asm_id), 1),
        ],
        "DB 子件行的 serial_no / assembly_id / quantity 应与响应一致"
    );
    let batches: Vec<i64> = sqlx::query_scalar(
        "SELECT count(*) FROM t_part_batch pb JOIN t_part p ON p.id = pb.part_id \
         WHERE p.assembly_id = $1",
    )
    .bind(asm_id)
    .fetch_all(&pool)
    .await
    .expect("count 子件初始批次");
    assert_eq!(batches, vec![2], "每个子件都应有 1 条初始批次");

    // counter 恰好推进 1 次（1 个父件号，子件号是派生字符串不占池）
    assert_eq!(
        counter_of(&pool, "P").await,
        1,
        "1 次建单只该派 1 个号，counter 应为 1"
    );
}

/// 2. 传 PDF（页数 = children.len()+1）：结果必须与不传 PDF 完全一致。
#[tokio::test]
async fn create_with_pdf_dispatches_serial_and_children() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let body = create_body(
        fx.customer_l2_id,
        &[
            ("子件-1", Some("D-P-1"), None),
            ("子件-2", Some("D-P-2"), None),
        ],
    );
    // 2 子件 → 首页 + 2 子件 = 3 页
    let pdf = make_fixture_pdf(3);

    let (s, env) = send(
        app,
        create_assembly_request(&body, Some("files"), Some(&pdf), &token),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "有 PDF 建单应成功: {env}");

    let asm_serial = assert_p_serial(
        env["data"]["assembly"]["serial_no"].as_str(),
        "P1000",
        "有 PDF 建单的父件",
    );
    let children = env["data"]["created_children"]
        .as_array()
        .expect("created_children 数组");
    assert_eq!(children.len(), 2, "有 PDF 应建 2 个子件: {env}");
    assert_eq!(
        children[0]["serial_no"].as_str(),
        Some(format!("{asm_serial}-01").as_str()),
        "子件序列号派生规则与无 PDF 路径一致: {env}"
    );

    // PDF 只用于页数校验、不入库：本端点不写 t_part_file
    let file_rows: i64 = sqlx::query_scalar("SELECT count(*) FROM t_part_file WHERE part_id = $1")
        .bind(
            env["data"]["assembly"]["id"]
                .as_str()
                .expect("assembly.id")
                .parse::<i64>()
                .expect("assembly.id 可解析"),
        )
        .fetch_one(&pool)
        .await
        .expect("count t_part_file");
    assert_eq!(
        file_rows, 0,
        "POST /assemblies 不入库 PDF（文件走 POST /{{id}}/files 单独上传）"
    );
}

// ===========================================================================
//  3. multipart 字段名 `file` / `files` 等价
// ===========================================================================

/// 3. 单数字段名 `file` 也必须真的参与页数校验。
///
/// 关键断言是**负向**那条：若 handler 只认 `files`，`file` 会被 `_` 分支静默丢弃，
/// 于是页数不符的 PDF 不再触发 20305，建单"成功" —— 表现为「传了 PDF 却没校验」。
/// 正向那条只证明 `file` 不至于让建单失败，证明力弱。
#[tokio::test]
async fn create_accepts_singular_file_field() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let body = create_body(
        fx.customer_l2_id,
        &[
            ("子件-1", Some("D-S-1"), None),
            ("子件-2", Some("D-S-2"), None),
        ],
    );

    // 负向：`file` 发 2 页（应要 3 页）→ 20305
    let bad_pdf = make_fixture_pdf(2);
    let (s, env) = send(
        app.clone(),
        create_assembly_request(&body, Some("file"), Some(&bad_pdf), &token),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::BAD_REQUEST,
        "单数字段名 `file` 也要做页数校验（2 页 vs children.len()+1=3）: {env}"
    );
    assert_eq!(
        env["code"],
        code::BIZ_ASSEMBLY_PDF_INVALID,
        "错误码应为 20305 BIZ_ASSEMBLY_PDF_INVALID: {env}"
    );
    assert_eq!(
        count_assemblies(&pool).await,
        0,
        "20305 fail-fast：t_assembly 行数必须不变"
    );

    // 正向：`file` 发 3 页 → 201，且派号建子件
    let good_pdf = make_fixture_pdf(3);
    let (s, env) = send(
        app,
        create_assembly_request(&body, Some("file"), Some(&good_pdf), &token),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::CREATED,
        "单数字段名 `file` 页数相符应成功: {env}"
    );
    assert_p_serial(
        env["data"]["assembly"]["serial_no"].as_str(),
        "P1000",
        "`file` 字段名路径的父件",
    );
    assert_eq!(
        env["data"]["created_children"].as_array().unwrap().len(),
        2,
        "`file` 字段名路径同样建出 2 个子件: {env}"
    );
}

// ===========================================================================
//  4. 子件单价 / 总价
// ===========================================================================

/// 4. 子件金额入参落库 + 回显；缺省落 0（不是 NULL）。
#[tokio::test]
async fn create_persists_child_prices() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    // 价格是 JSON **字符串**（rust_decimal 只开 `serde-with-str`，裸数字会 400）
    let body = create_body(
        fx.customer_l2_id,
        &[
            ("子件-带价", Some("D-PR-1"), Some("12.50")),
            ("子件-缺省", Some("D-PR-2"), None),
        ],
    );

    let (s, env) = send(app, create_assembly_request(&body, None, None, &token)).await;
    assert_eq!(s, StatusCode::CREATED, "建单应成功: {env}");

    let children = env["data"]["created_children"]
        .as_array()
        .expect("created_children 数组");
    assert_eq!(children.len(), 2, "应建 2 个子件: {env}");
    // Decimal 序列化成字符串且保精度（NUMERIC(12,2) → "12.50" 而不是 12.5）
    assert_eq!(
        children[0]["unit_price"].as_str(),
        Some("12.50"),
        "子件单价应回显入参值（Decimal 序列化成字符串）: {env}"
    );
    assert_eq!(
        children[0]["total_price"].as_str(),
        Some("12.50"),
        "子件总价应回显入参值: {env}"
    );
    assert_eq!(
        children[1]["unit_price"].as_str(),
        Some("0"),
        "子件单价缺省应落 0（不是 null）: {env}"
    );
    assert_eq!(
        children[1]["total_price"].as_str(),
        Some("0"),
        "子件总价缺省应落 0（不是 null）: {env}"
    );

    // DB 侧：两列 NOT NULL，缺省也必须是 0 而不是 NULL
    let asm_id: i64 = env["data"]["assembly"]["id"]
        .as_str()
        .expect("assembly.id")
        .parse()
        .expect("assembly.id 可解析");
    let rows: Vec<(rust_decimal::Decimal, rust_decimal::Decimal)> = sqlx::query_as(
        "SELECT unit_price, total_price FROM t_part WHERE assembly_id = $1 ORDER BY serial_no ASC",
    )
    .bind(asm_id)
    .fetch_all(&pool)
    .await
    .expect("query 子件价格");
    assert_eq!(rows.len(), 2, "应查到 2 个子件");
    assert_eq!(
        rows[0].0,
        rust_decimal::Decimal::from_str_exact("12.50").unwrap()
    );
    assert_eq!(
        rows[0].1,
        rust_decimal::Decimal::from_str_exact("12.50").unwrap()
    );
    assert_eq!(rows[1].0, rust_decimal::Decimal::ZERO, "缺省单价落 0");
    assert_eq!(rows[1].1, rust_decimal::Decimal::ZERO, "缺省总价落 0");
}

// ===========================================================================
//  5. 页数校验回归（放开门槛 ≠ 去掉校验）
// ===========================================================================

/// 5. 页数不符仍 20305；且校验排在派发前 —— 计数器不动、不落任何行。
#[tokio::test]
async fn create_pdf_page_mismatch_rejects_and_keeps_counter() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    // 2 子件 → 应要 3 页，实际给 4 页
    let pdf = make_fixture_pdf(4);
    let body = create_body(
        fx.customer_l2_id,
        &[
            ("子件-1", Some("D-M-1"), None),
            ("子件-2", Some("D-M-2"), None),
        ],
    );

    let (s, env) = send(
        app,
        create_assembly_request(&body, Some("files"), Some(&pdf), &token),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::BAD_REQUEST,
        "页数不符应 20305（放开派发门槛不能顺带去掉校验）: {env}"
    );
    assert_eq!(
        env["code"],
        code::BIZ_ASSEMBLY_PDF_INVALID,
        "错误码应为 20305 BIZ_ASSEMBLY_PDF_INVALID: {env}"
    );
    assert_eq!(count_assemblies(&pool).await, 0, "t_assembly 行数必须不变");
    assert_eq!(count_parts(&pool).await, 0, "t_part 行数必须不变");
    assert_eq!(
        counter_of(&pool, "P").await,
        0,
        "页数校验排在派发前 ⇒ 20305 不该消耗序列号计数器"
    );
}

// ===========================================================================
//  6-8. 派发前置条件的负向分支
// ===========================================================================

/// 6. L1 未配 `serial_prefix` → 20308，整单拒且不落行。
#[tokio::test]
async fn create_rejects_when_l1_serial_prefix_missing() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let l1 = insert_l1_customer(&pool, "无 prefix 的 L1", None).await;
    let l2 = insert_l2_customer(&pool, "无 prefix 的 L2", l1).await;
    let body = create_body(l2, &[("子件-1", Some("D-X-1"), None)]);

    let (s, env) = send(app, create_assembly_request(&body, None, None, &token)).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "L1 未配 prefix 应 20308: {env}");
    assert_eq!(
        env["code"],
        code::BIZ_CUSTOMER_NO_SERIAL_PREFIX,
        "错误码应为 20308 BIZ_CUSTOMER_NO_SERIAL_PREFIX: {env}"
    );
    assert_eq!(count_assemblies(&pool).await, 0, "t_assembly 行数必须不变");
    assert_eq!(count_parts(&pool).await, 0, "t_part 行数必须不变");
}

/// 7. `serial_prefix` 未在 `t_serial_counter` 注册 → 20108，整单拒且不落行。
#[tokio::test]
async fn create_rejects_when_serial_prefix_not_registered() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    // prefix 'Q' 在 t_serial_counter 里没有行（fixture 只预置 'P'）
    let l1 = insert_l1_customer(&pool, "未注册 prefix 的 L1", Some("Q")).await;
    let l2 = insert_l2_customer(&pool, "未注册 prefix 的 L2", l1).await;
    let body = create_body(l2, &[("子件-1", Some("D-Y-1"), None)]);

    let (s, env) = send(app, create_assembly_request(&body, None, None, &token)).await;
    assert_eq!(
        s,
        StatusCode::BAD_REQUEST,
        "prefix 未注册应 20108（2xxxx 映射 400）: {env}"
    );
    assert_eq!(
        env["code"],
        code::BIZ_SERIAL_PREFIX_UNKNOWN,
        "错误码应为 20108 BIZ_SERIAL_PREFIX_UNKNOWN: {env}"
    );
    assert_eq!(count_assemblies(&pool).await, 0, "t_assembly 行数必须不变");
    assert_eq!(count_parts(&pool).await, 0, "t_part 行数必须不变");
}

/// 8. `customer_id` 传 L1（集团节点）→ 20302，整单拒且不落行。
///
/// L1 自己有 `serial_prefix='P'` 且计数器可用，所以这个用例证明 20302 来自
/// 「必须是 L2 叶子」这道校验，而不是被后面的派发逻辑误伤。
#[tokio::test]
async fn create_rejects_when_customer_not_l2_leaf() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    let body = create_body(fx.customer_l1_id, &[("子件-1", Some("D-Z-1"), None)]);

    let (s, env) = send(app, create_assembly_request(&body, None, None, &token)).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "customer 传 L1 应 20302: {env}");
    assert_eq!(
        env["code"],
        code::BIZ_ASSEMBLY_BAD_CUSTOMER,
        "错误码应为 20302 BIZ_ASSEMBLY_BAD_CUSTOMER: {env}"
    );
    assert_eq!(count_assemblies(&pool).await, 0, "t_assembly 行数必须不变");
    assert_eq!(count_parts(&pool).await, 0, "t_part 行数必须不变");
    assert_eq!(
        counter_of(&pool, "P").await,
        0,
        "20302 排在派发前 ⇒ 计数器不应被推进"
    );
}

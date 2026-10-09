//! `wx::part_list` 子模块 + `wx::login` 响应契约 + wx 域旧 URL 硬切的集成测试
//! （2026-10-11 新增，wx BFF 重构 B2）
//!
//! 端点（测试 app 直接挂 `v2_router`，**不带** `/api/v2` 前缀）：
//! - `GET /wx/part-list` —— 首屏聚合（`counts` + `list` + `hasMore`）
//! - `GET /wx/part-list/page` —— 上拉增量（`list` + `hasMore`）
//! - `POST /wx/login/wecom` —— 企业微信登录（响应字段集断言）
//!
//! 两个列表端点共用 `?date=YYYY-MM-DD&status=<tab>&page=&size=`（2026-10-12
//! 新增 `?date=`：小程序 `date-nav-bar` 的日期此前是**纯装饰**的，本轮接成真筛选）。
//!
//! ## 覆盖清单（2026-10-12 重排：tab 4 → 7 个，新增日期维度）
//!
//! | # | 场景 | 用例 |
//! |---|---|---|
//! | 1 | 无 Authorization → 401 / 40100 | [`home_requires_auth`] |
//! | 2 | `?status=all` 与不传 `status` 结果一致，且都排除白名单外状态 | [`all_tab_equals_absent_status`] |
//! | 3 | 7 个 tab 值各自命中，且每张卡片 `status` == tab 名 | [`seven_tabs_each_return_matching_status`] |
//! | 3b | ★ `outsource` tab 只返 `OUTSOURCE` 行 | [`outsource_tab_returns_only_outsource_rows`] |
//! | 3c | ★ `noSystemDate` tab **忽略** `?date=`，`dueDate` 是 JSON `null` | [`no_system_date_tab_ignores_date_param`] |
//! | 3d | ★ `counts` 带**日期作用域** + ★ `all == 5 个 dated tab 之和` | [`counts_are_scoped_by_date`] |
//! | 3e | ★ `PROGRAMMING` / `COMPLETED` / `CANCELLED` 被 6 状态白名单排除 | [`programming_completed_cancelled_are_excluded_from_all`] |
//! | 4 | ★ 每个 tab 的 counts 桶 == 按该 tab 过滤后的**行数**（同口径交叉断言） | [`each_tab_counts_bucket_equals_its_list_length`] |
//! | 5 | ★ `delivered` 口径一致：翻到最后一页累计 == `counts.delivered` | [`delivered_tab_list_total_equals_counts_delivered`] |
//! | 6 | 非法 `status` → 422 + 40001 | [`invalid_status_is_validation_error`] |
//! | 6b | ★ 非法 `?date=` 格式 → **HTTP 400 纯文本**（提取器层，不走 `R<T>`） | [`malformed_date_is_rejected_by_the_query_extractor`] |
//! | 7 | **★ 尾斜杠形态钉死** | [`trailing_slash_form_is_pinned`] |
//! | 8 | 4 条旧路径全部 404 | [`legacy_wx_paths_are_all_gone`] |
//! | 9 | `/page?page=2` 与 `/?page=2` 的 `list` 逐字一致 | [`page_endpoint_matches_home_endpoint_at_same_page`] |
//! | 10 | 分页不重不漏 | [`pagination_has_no_overlap_or_gap`] |
//! | 11 | **★ `deliveredQty` 真值** | [`delivered_qty_reflects_delivered_batches`] |
//! | 12 | 登录响应只含 6 个字段 | [`wx_login_response_only_exposes_six_fields`] |
//!
//! 另有两个「形状」用例：[`home_response_shape_matches_contract`]（首屏响应逐字段
//! 形态、counts 7 键）、[`batch_kind_card_for_assembly_children`]（`kind=batch` 变体）。
//!
//! ## 串行化
//! `test_pool()` 每次 fresh database（DB 间 schema 完全独立），无需 Mutex /
//! `--test-threads=1`。每个测试造自己的差异行，互不污染。
//!
//! ## fixture
//! auth 用 `load_iam_fixture`（5 用户 + 2 角色，段 110+）；零件 / 批次 / 客户用
//! **运行时雪花 ID** 现场插（本域独享 raw SQL helper，见文件下半部），与任何常量
//! ID 段物理不相交。

use axum::http::StatusCode;
use serde_json::{Value, json};
use sqlx::PgPool;

use hsh_erp_test_support::{
    IamFixture, json_request, load_iam_fixture, login_token, pool_snowflake, send, send_raw,
    test_app, test_pool, test_state,
};

/// 首屏聚合端点（测试 app 无 `/api/v2` 前缀；**无尾斜杠**）
const HOME_URI: &str = "/wx/part-list";
/// 上拉增量端点
const PAGE_URI: &str = "/wx/part-list/page";
/// 企业微信登录端点
const LOGIN_URI: &str = "/wx/login/wecom";

// ===========================================================================
//  Bootstrap
// ===========================================================================

/// fresh database + iam fixture → 以 MANAGER 身份登录。
async fn bootstrap() -> (PgPool, axum::Router, String) {
    let pool = test_pool().await;
    let fx = load_iam_fixture(&pool).await;
    let app = test_app(test_state(pool.clone()).await);
    let token = login_token(&app, &fx.manager_username, IamFixture::PASSWORD).await;
    (pool, app, token)
}

// ===========================================================================
//  本域独享 raw SQL helper（造场景差异行）
// ===========================================================================

/// 下一个测试用雪花 ID（进程级 generator，per-process 隔离）。
fn next_id() -> i64 {
    pool_snowflake()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .next_id()
}

/// 插一个客户行（`t_part.customer_id` NOT NULL，必须先有）。
async fn insert_customer(pool: &PgPool, name: &str) -> i64 {
    let id = next_id();
    sqlx::query("INSERT INTO t_customer (id, name) VALUES ($1, $2)")
        .bind(id)
        .bind(name)
        .execute(pool)
        .await
        .expect("insert t_customer");
    id
}

/// 一个 `t_part` 行的构造参数（除 id 外全字段）。
#[derive(Debug, Clone)]
struct PartSpec<'a> {
    customer_id: i64,
    /// DB 原值状态（`PENDING` / `IN_PROCESS` / `OUTSOURCE` / `INSPECTION` /
    /// `READY_TO_SHIP` / `DELIVERED` / `PROGRAMMING` / `COMPLETED` / `CANCELLED`）
    status: &'a str,
    quantity: i32,
    /// `t_part.serial_no` 走**唯一索引** `uk_t_part_serial_no`；缺省 `None`
    /// （手工工单形态，顺带覆盖卡片 `serialNo: null` 的投影）。要非空必须每行
    /// 给**不同**的值。
    serial_no: Option<String>,
    /// `Some(x)` ⇒ `assembly_id = x` ⇒ 卡片按 `kind=batch` 呈现
    assembly_id: Option<i64>,
    /// `YYYY-MM-DD`（`t_part.planned_delivery_date`，NOT NULL）
    due: &'a str,
    /// `YYYY-MM-DD`（`t_part.system_delivery_date`，**可空**）。
    /// 2026-10-12 新增：端点 2/3 的日期筛选谓词与卡片 `dueDate` 都打这一列。
    /// `None` ⇒ 列写 NULL（「无交期」工单，卡片 `dueDate` 序列化成 JSON `null`）。
    ///
    /// ⚠️ 改 `due` **不会**顺带改 `system_due`（两者语义不同：一个是计划交期、
    /// 一个是系统交期）。本文件 `new()` 刻意给两者同一个缺省值，让既有用例不必改。
    system_due: Option<&'a str>,
    is_urgent: bool,
    deleted: bool,
}

impl<'a> PartSpec<'a> {
    fn new(customer_id: i64, status: &'a str) -> Self {
        Self {
            customer_id,
            status,
            quantity: 8,
            serial_no: None,
            assembly_id: None,
            due: "2026-08-04",
            system_due: Some("2026-08-04"),
            is_urgent: false,
            deleted: false,
        }
    }
}

/// 常用日期字面量（`?date=` 与 `system_delivery_date` 共用）。
const D1: &str = "2026-08-04";
const D2: &str = "2026-08-05";

/// 插一行 `t_part`（软删闸门用 `deleted` 控制）。
async fn insert_part(pool: &PgPool, s: &PartSpec<'_>) -> i64 {
    let id = next_id();
    let due: chrono::NaiveDate = s.due.parse().expect("due 是 YYYY-MM-DD");
    let system_due: Option<chrono::NaiveDate> = s
        .system_due
        .map(|d| d.parse().expect("system_due 是 YYYY-MM-DD"));
    sqlx::query(
        "INSERT INTO t_part (id, serial_no, name, drawing_no, applicant_name, quantity, \
         request_date, planned_delivery_date, system_delivery_date, status, is_urgent, \
         customer_id, assembly_id, version, created_at, updated_at, deleted_at) \
         VALUES ($1, $2, '测试件', $3, 'T', $4, $5, $5, $6, $7, $8, $9, $10, 0, now(), \
                 now(), CASE WHEN $11 THEN now() ELSE NULL END)",
    )
    .bind(id)
    .bind(s.serial_no.as_deref())
    .bind(format!("D-{id}"))
    .bind(s.quantity)
    .bind(due)
    .bind(system_due)
    .bind(s.status)
    .bind(s.is_urgent)
    .bind(s.customer_id)
    .bind(s.assembly_id)
    .bind(s.deleted)
    .execute(pool)
    .await
    .expect("insert t_part");
    id
}

/// 插一行 `t_part_batch`（`deliveredQty` 口径的输入）。
async fn insert_batch(
    pool: &PgPool,
    part_id: i64,
    batch_no: i32,
    quantity: i32,
    status: &str,
) -> i64 {
    let id = next_id();
    sqlx::query(
        "INSERT INTO t_part_batch (id, part_id, batch_no, quantity, status, version, \
         created_at, updated_at) VALUES ($1, $2, $3, $4, $5, 0, now(), now())",
    )
    .bind(id)
    .bind(part_id)
    .bind(batch_no)
    .bind(quantity)
    .bind(status)
    .execute(pool)
    .await
    .expect("insert t_part_batch");
    id
}

/// 插一个装配件行（`t_part.assembly_id` 指向它 ⇒ 子件按 `kind=batch` 呈现）。
async fn insert_assembly(pool: &PgPool, customer_id: i64) -> i64 {
    let id = next_id();
    sqlx::query(
        "INSERT INTO t_assembly (id, drawing_no, name, customer_id, request_date, \
         planned_delivery_date, status, version, created_at, updated_at) \
         VALUES ($1, 'ASM-1', '测试装配件', $2, current_date, current_date, \
                 'IN_PROCESS', 0, now(), now())",
    )
    .bind(id)
    .bind(customer_id)
    .execute(pool)
    .await
    .expect("insert t_assembly");
    id
}

// ===========================================================================
//  断言 helper
// ===========================================================================

/// GET 一个端点，断言 200 + `code == 0`，返回信封。
async fn get_ok(app: &axum::Router, uri: &str, token: &str) -> Value {
    let (status, env) = send(app.clone(), json_request("GET", uri, None, Some(token))).await;
    assert_eq!(status, StatusCode::OK, "GET {uri}: {env}");
    assert_eq!(env["code"], 0, "GET {uri}: {env}");
    env
}

/// 首屏端点（带 query 串）。
async fn home(app: &axum::Router, token: &str, query: &str) -> Value {
    let uri = if query.is_empty() {
        HOME_URI.to_string()
    } else {
        format!("{HOME_URI}?{query}")
    };
    get_ok(app, &uri, token).await
}

/// 增量端点（带 query 串）。
async fn page(app: &axum::Router, token: &str, query: &str) -> Value {
    let uri = if query.is_empty() {
        PAGE_URI.to_string()
    } else {
        format!("{PAGE_URI}?{query}")
    };
    get_ok(app, &uri, token).await
}

/// 取出 `data.list` 数组。
fn list_of(env: &Value) -> &Vec<Value> {
    env["data"]["list"]
        .as_array()
        .expect("data.list 必须是数组")
}

/// 取出卡片 id（JSON string 形态的雪花 i64）。
fn card_ids(list: &[Value]) -> Vec<String> {
    list.iter()
        .map(|c| {
            c["id"]
                .as_str()
                .expect("card.id 必须是 string(i64)")
                .to_string()
        })
        .collect()
}

/// 从 list 里按 id 取单张卡片。
fn card_by_id(list: &[Value], id: i64) -> &Value {
    let want = id.to_string();
    list.iter()
        .find(|c| c["id"].as_str() == Some(want.as_str()))
        .unwrap_or_else(|| panic!("id {id} 不在 list 里: {list:?}"))
}

// ===========================================================================
//  1. 无 Authorization → 401 / 40100
// ===========================================================================

#[tokio::test]
async fn home_requires_auth() {
    let (_pool, app, token) = bootstrap().await;

    // 无 Authorization 头
    let (status, env) = send(app.clone(), json_request("GET", HOME_URI, None, None)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "无 token 应 401: {env}");
    assert_eq!(env["code"], 40100, "无 token 的错误码应是 40100: {env}");

    // 增量端点同样受保护（nest 在同一层 auth middleware 下）
    let (status, env) = send(app.clone(), json_request("GET", PAGE_URI, None, None)).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "/page 无 token 应 401: {env}"
    );
    assert_eq!(env["code"], 40100);

    // 反向确认：带 token 才通
    let env = get_ok(&app, HOME_URI, &token).await;
    assert!(env["data"].is_object(), "带 token 应拿到 data 对象: {env}");
}

// ===========================================================================
//  2. `?status=all` 与不传 `status` 结果一致（且都落到 6 状态白名单）
// ===========================================================================

/// ⚠️ 2026-10-12 **语义变更**：`all` / 缺省不再表示「不过滤」，两者都落到
/// 6 状态白名单 —— 因此 `PROGRAMMING` / `COMPLETED` / `CANCELLED` **不出现**
/// 在「全部」里（旧实现会把它们一起漏出来）。
#[tokio::test]
async fn all_tab_equals_absent_status() {
    let (pool, app, token) = bootstrap().await;
    let cid = insert_customer(&pool, "六厂").await;
    for st in [
        "PENDING",
        "IN_PROCESS",
        "OUTSOURCE",
        "INSPECTION",
        "DELIVERED",
    ] {
        insert_part(&pool, &PartSpec::new(cid, st)).await;
    }
    // 白名单外的 3 个状态：即便 system_delivery_date 有值也不该出现
    let mut excluded = Vec::new();
    for st in ["PROGRAMMING", "COMPLETED", "CANCELLED"] {
        excluded.push(insert_part(&pool, &PartSpec::new(cid, st)).await);
    }

    let absent = home(&app, &token, "").await;
    let all = home(&app, &token, "status=all").await;

    assert_eq!(
        card_ids(list_of(&absent)),
        card_ids(list_of(&all)),
        "`?status=all` 与不传 status 必须逐字等价（all 不是 DB 状态）: absent={absent} all={all}"
    );
    assert_eq!(
        absent["data"]["counts"], all["data"]["counts"],
        "counts 同样与 ?status= 无关，两次必须一致"
    );
    assert_eq!(
        list_of(&all).len(),
        5,
        "5 行白名单状态全在「全部」里: {all}"
    );

    let ids = card_ids(list_of(&all));
    for id in excluded {
        assert!(
            !ids.contains(&id.to_string()),
            "PROGRAMMING / COMPLETED / CANCELLED 不在 6 状态白名单内（id={id}）: {all}"
        );
    }
    assert_eq!(
        all["data"]["counts"]["all"],
        json!(5),
        "counts.all 同样排除白名单外的 3 个状态: {all}"
    );
}

// ===========================================================================
//  3. 7 个 tab 值各自命中，且每张卡片 status == tab 名
// ===========================================================================

/// 2026-10-12：`inspecting` 吃两个 DB 状态（`INSPECTION` + `READY_TO_SHIP`）、
/// `delivered` 收窄成只剩 `DELIVERED`、新增 `outsource` 与 `noSystemDate`。
#[tokio::test]
async fn seven_tabs_each_return_matching_status() {
    let (pool, app, token) = bootstrap().await;
    let cid = insert_customer(&pool, "六厂").await;

    // 每个 DB 状态各 1 行
    for db in [
        "PENDING",
        "IN_PROCESS",
        "OUTSOURCE",
        "INSPECTION",
        "DELIVERED",
        "READY_TO_SHIP",
    ] {
        insert_part(&pool, &PartSpec::new(cid, db)).await;
    }
    // 「无交期」行：system_delivery_date IS NULL
    let mut undated = PartSpec::new(cid, "PENDING");
    undated.system_due = None;
    insert_part(&pool, &undated).await;

    for (tab, want_db) in [
        ("pendingProduction", vec!["PENDING"]),
        ("inProduction", vec!["IN_PROCESS"]),
        ("outsource", vec!["OUTSOURCE"]),
        ("inspecting", vec!["INSPECTION", "READY_TO_SHIP"]),
        ("delivered", vec!["DELIVERED"]),
    ] {
        let env = page(&app, &token, &format!("status={tab}&size=50")).await;
        let list = list_of(&env);
        assert!(!list.is_empty(), "[{tab}] 该 tab 必须有卡片: {env}");

        for c in list {
            assert_eq!(
                c["status"], tab,
                "[{tab}] 每张卡片的 status 字段必须 == tab 名: {c}"
            );
            assert_eq!(
                c["kind"], "workOrder",
                "[{tab}] 这些行都是独立件（assembly_id IS NULL）: {c}"
            );
            let id = c["id"].as_str().unwrap().parse::<i64>().unwrap();
            let (db_status,): (String,) = sqlx::query_as("SELECT status FROM t_part WHERE id = $1")
                .bind(id)
                .fetch_one(&pool)
                .await
                .expect("查回 DB 状态");
            assert!(
                want_db.contains(&db_status.as_str()),
                "[{tab}] DB 状态 {db_status} 不在期望集 {want_db:?} 内"
            );
        }
    }

    // noSystemDate tab：恒显示 system_delivery_date 为 NULL 的 6 状态行
    let ns = page(&app, &token, "status=noSystemDate&size=50").await;
    assert_eq!(list_of(&ns).len(), 1, "[noSystemDate] 只该有那 1 行: {ns}");
    assert_eq!(
        list_of(&ns)[0]["dueDate"],
        Value::Null,
        "无交期行的 dueDate 必须是 JSON null: {ns}"
    );

    // 反向确认：6 行有交期 + 1 行无交期 = 「全部」tab 的全部（缺省无日期谓词）
    let all = page(&app, &token, "size=50").await;
    assert_eq!(
        list_of(&all).len(),
        7,
        "「全部」= 6 有交期 + 1 无交期: {all}"
    );
}

// ===========================================================================
//  3b. ★ 外协中 tab 只返 OUTSOURCE 行（2026-10-12 新增 tab）
// ===========================================================================

#[tokio::test]
async fn outsource_tab_returns_only_outsource_rows() {
    let (pool, app, token) = bootstrap().await;
    let cid = insert_customer(&pool, "六厂").await;

    let out_id = insert_part(&pool, &PartSpec::new(cid, "OUTSOURCE")).await;
    let mut others = Vec::new();
    for db in [
        "PENDING",
        "IN_PROCESS",
        "INSPECTION",
        "READY_TO_SHIP",
        "DELIVERED",
        "PROGRAMMING",
        "COMPLETED",
        "CANCELLED",
    ] {
        others.push(insert_part(&pool, &PartSpec::new(cid, db)).await);
    }

    let env = page(&app, &token, "status=outsource&size=50").await;
    let ids = card_ids(list_of(&env));
    assert_eq!(
        ids,
        vec![out_id.to_string()],
        "outsource tab 只能有 OUTSOURCE 行: {env}"
    );
    assert_eq!(list_of(&env)[0]["status"], json!("outsource"));

    // 角标同样只有 1
    let counts = home(&app, &token, "").await;
    assert_eq!(counts["data"]["counts"]["outsource"], json!(1), "{counts}");
}

// ===========================================================================
//  3c. ★ 无交期 tab 忽略 ?date=（2026-10-12 新增 tab）
// ===========================================================================

/// `noSystemDate` 的谓词是 `system_delivery_date IS NULL`，与 `?date=` 选哪天
/// **无关**：两个不同日期各查一次，结果必须逐字相同；且**不能**混入任何有交期的行。
#[tokio::test]
async fn no_system_date_tab_ignores_date_param() {
    let (pool, app, token) = bootstrap().await;
    let cid = insert_customer(&pool, "六厂").await;

    // 无交期行：3 个白名单状态 + 1 个白名单外状态（后者不该出现）
    let mut undated_ids = Vec::new();
    for db in ["PENDING", "IN_PROCESS", "OUTSOURCE"] {
        let mut s = PartSpec::new(cid, db);
        s.system_due = None;
        undated_ids.push(insert_part(&pool, &s).await);
    }
    let mut banned = PartSpec::new(cid, "COMPLETED");
    banned.system_due = None;
    let banned_id = insert_part(&pool, &banned).await;

    // 有交期行（D1 / D2 各若干）：不该出现在 noSystemDate tab 里
    for (due, st) in [(D1, "PENDING"), (D2, "IN_PROCESS")] {
        let mut s = PartSpec::new(cid, st);
        s.system_due = Some(due);
        insert_part(&pool, &s).await;
    }

    let mut expect: Vec<String> = undated_ids.iter().map(|i| i.to_string()).collect();
    expect.sort();

    for q in [
        "status=noSystemDate&size=50",
        &format!("status=noSystemDate&size=50&date={D1}"),
        &format!("status=noSystemDate&size=50&date={D2}"),
    ] {
        let env = page(&app, &token, q).await;
        let mut got = card_ids(list_of(&env));
        got.sort();
        assert_eq!(got, expect, "[{q}] noSystemDate 必须忽略 ?date=: {env}");
        assert!(
            !got.contains(&banned_id.to_string()),
            "[{q}] 白名单外状态不该出现"
        );
        for c in list_of(&env) {
            assert_eq!(
                c["dueDate"],
                Value::Null,
                "[{q}] 无交期行 dueDate 必须是 null"
            );
        }
    }

    // 角标 noSystemDate 与 ?date= 无关，且不含白名单外状态
    for q in ["", &format!("date={D1}"), &format!("date={D2}")] {
        let counts = home(&app, &token, q).await;
        assert_eq!(
            counts["data"]["counts"]["noSystemDate"],
            json!(3),
            "[counts?{q}] noSystemDate 恒为 3（无交期 × 6 状态白名单）: {counts}"
        );
    }
}

// ===========================================================================
//  3d. ★ counts 带日期作用域（2026-10-12）
// ===========================================================================

/// 同一天造 5 个白名单状态 × 2 天，另加无交期行。逐天断言 `counts` 各桶，
/// 并钉死 **★ 不变量**：`counts.all == 5 个 dated tab 之和`。
#[tokio::test]
async fn counts_are_scoped_by_date() {
    let (pool, app, token) = bootstrap().await;
    let cid = insert_customer(&pool, "六厂").await;

    // D1：6 状态各 1 行；D2：6 状态各 2 行
    // ⚠️ `t_part.serial_no` 是 varchar(15) 且有唯一索引，序列号一律用**短串**。
    let mut seq = 0usize;
    for (due, n) in [(D1, 1usize), (D2, 2usize)] {
        for db in [
            "PENDING",
            "IN_PROCESS",
            "OUTSOURCE",
            "INSPECTION",
            "READY_TO_SHIP",
            "DELIVERED",
        ] {
            for _ in 0..n {
                let mut s = PartSpec::new(cid, db);
                s.system_due = Some(due);
                s.serial_no = Some(format!("FX-S{seq}"));
                seq += 1;
                insert_part(&pool, &s).await;
            }
        }
    }
    // 无交期行：另造 3 行（与日期无关，用来验 noSystemDate 桶）
    for _ in 0..3 {
        let mut s = PartSpec::new(cid, "PENDING");
        s.system_due = None;
        s.serial_no = Some(format!("FX-N{seq}"));
        seq += 1;
        insert_part(&pool, &s).await;
    }

    // 期望值（bucket, D1, D2）
    let expect: [(&str, i64, i64); 6] = [
        // 6 状态 D1 各 1、D2 各 2；inspecting 吃 INSPECTION + READY_TO_SHIP
        ("pendingProduction", 1, 2),
        ("inProduction", 1, 2),
        ("outsource", 1, 2),
        ("inspecting", 2, 4),
        ("delivered", 1, 2),
        ("all", 6, 12),
    ];

    for (due, col) in [(D1, 1usize), (D2, 2usize)] {
        let env = home(&app, &token, &format!("date={due}&status=all")).await;
        for (key, d1, d2) in expect {
            let want = if col == 1 { d1 } else { d2 };
            assert_eq!(
                env["data"]["counts"][key],
                json!(want),
                "[?date={due}] counts.{key} 应是 {want}: {env}"
            );
        }
        // ★ 不变量：all == 5 个 dated tab 之和（noSystemDate **不进**这条等式）
        let c = &env["data"]["counts"];
        let sum: i64 = [
            "pendingProduction",
            "inProduction",
            "outsource",
            "inspecting",
            "delivered",
        ]
        .iter()
        .map(|k| c[*k].as_i64().unwrap())
        .sum();
        assert_eq!(
            c["all"].as_i64().unwrap(),
            sum,
            "[?date={due}] counts.all 必须恒等于 5 个 dated tab 之和: {env}"
        );
        // noSystemDate 恒为 3（无交期行数），与 ?date= 无关
        assert_eq!(c["noSystemDate"], json!(3), "[?date={due}]: {env}");
    }

    // ?date= 缺省：日期谓词退化为无谓词 ⇒ counts.all 是 6 状态白名单的全量
    // （D1 6 行 + D2 12 行 + 3 行无交期 = 21；含无交期行，否则「不传 date 时
    // counts.all < ?status=all 的列表行数」正是 §3.3 记的那条裂缝）
    let none = home(&app, &token, "status=all").await;
    assert_eq!(
        none["data"]["counts"]["all"],
        json!(21),
        "不传 ?date 时 counts.all 必须是 6 状态白名单的全量行数: {none}"
    );
    assert_eq!(none["data"]["counts"]["noSystemDate"], json!(3), "{none}");

    // 列表同样受日期作用域约束
    let l1 = page(&app, &token, &format!("date={D1}&size=50")).await;
    assert_eq!(
        list_of(&l1).len(),
        6,
        "[?date={D1}] 列表只应是当天 6 行: {l1}"
    );
    let l2 = page(&app, &token, &format!("date={D2}&size=50")).await;
    assert_eq!(
        list_of(&l2).len(),
        12,
        "[?date={D2}] 列表只应是当天 12 行: {l2}"
    );
    let l0 = page(&app, &token, "size=50").await;
    assert_eq!(
        list_of(&l0).len(),
        21,
        "不传 ?date 时列表是全量 21 行（含 3 行无交期）: {l0}"
    );
}

// ===========================================================================
//  3e. ★ PROGRAMMING / COMPLETED / CANCELLED 被排除（2026-10-12 语义变更）
// ===========================================================================

#[tokio::test]
async fn programming_completed_cancelled_are_excluded_from_all() {
    let (pool, app, token) = bootstrap().await;
    let cid = insert_customer(&pool, "六厂").await;

    let mut kept = Vec::new();
    for db in [
        "PENDING",
        "IN_PROCESS",
        "OUTSOURCE",
        "INSPECTION",
        "READY_TO_SHIP",
        "DELIVERED",
    ] {
        kept.push(insert_part(&pool, &PartSpec::new(cid, db)).await);
    }
    let mut banned = Vec::new();
    for db in ["PROGRAMMING", "COMPLETED", "CANCELLED"] {
        banned.push(insert_part(&pool, &PartSpec::new(cid, db)).await);
    }

    // 7 个 tab（含 all / noSystemDate）逐个查：白名单外的 3 个状态一条都不许出现
    for tab in [
        "all",
        "pendingProduction",
        "inProduction",
        "outsource",
        "inspecting",
        "delivered",
        "noSystemDate",
    ] {
        let env = page(&app, &token, &format!("status={tab}&size=50")).await;
        let ids = card_ids(list_of(&env));
        for id in &banned {
            assert!(
                !ids.contains(&id.to_string()),
                "[{tab}] PROGRAMMING/COMPLETED/CANCELLED 已被 6 状态白名单排除 \
                 （泄漏 id={id}）: {env}"
            );
        }
    }

    // all 恰好是 6 行白名单状态
    let all = page(&app, &token, "status=all&size=50").await;
    let mut want: Vec<String> = kept.iter().map(|i| i.to_string()).collect();
    want.sort();
    let mut got = card_ids(list_of(&all));
    got.sort();
    assert_eq!(got, want, "[all] 应恰好是 6 个白名单状态的行: {all}");

    // 角标侧同样排除
    let counts = home(&app, &token, "").await;
    assert_eq!(counts["data"]["counts"]["all"], json!(6), "{counts}");
}

// ===========================================================================
//  4. ★ 每个 dated tab 的 counts 桶 == 按该 tab 过滤后的行数
// ===========================================================================

/// 逐 tab 交叉断言：**角标 == 列表实际行数**。这是本仓 §3.3 记的那类
/// 「角标 198、列表 126」裂缝的通用防线；本次由 6 状态白名单 + 日期作用域
/// 共同保证，故必须对 5 个 dated tab 与 `noSystemDate` 全部验一遍。
#[tokio::test]
async fn each_tab_counts_bucket_equals_its_list_length() {
    let (pool, app, token) = bootstrap().await;
    let cid = insert_customer(&pool, "六厂").await;

    // D1 上铺满 6 状态各 1 行，另造 1 行无交期 + 3 行白名单外状态做干扰
    for db in [
        "PENDING",
        "IN_PROCESS",
        "OUTSOURCE",
        "INSPECTION",
        "READY_TO_SHIP",
        "DELIVERED",
    ] {
        let mut s = PartSpec::new(cid, db);
        s.system_due = Some(D1);
        insert_part(&pool, &s).await;
    }
    let mut undated = PartSpec::new(cid, "PENDING");
    undated.system_due = None;
    insert_part(&pool, &undated).await;
    for db in ["PROGRAMMING", "COMPLETED", "CANCELLED"] {
        insert_part(&pool, &PartSpec::new(cid, db)).await;
    }
    // 另一天的行：验证日期作用域没有漏进角标
    let mut other = PartSpec::new(cid, "PENDING");
    other.system_due = Some(D2);
    insert_part(&pool, &other).await;

    let counts = home(&app, &token, &format!("date={D1}")).await;
    for (tab, key) in [
        ("all", "all"),
        ("pendingProduction", "pendingProduction"),
        ("inProduction", "inProduction"),
        ("outsource", "outsource"),
        ("inspecting", "inspecting"),
        ("delivered", "delivered"),
        ("noSystemDate", "noSystemDate"),
    ] {
        let env = page(&app, &token, &format!("date={D1}&status={tab}&size=50")).await;
        let want = counts["data"]["counts"][key]
            .as_i64()
            .expect("counts 桶必须是 number");
        assert_eq!(
            list_of(&env).len() as i64,
            want,
            "[?date={D1}&status={tab}] 列表行数必须 == counts.{key}: {env} vs {counts}"
        );
    }

    // ★ 核心不变量（同口径交叉断言）
    let c = &counts["data"]["counts"];
    let sum: i64 = [
        "pendingProduction",
        "inProduction",
        "outsource",
        "inspecting",
        "delivered",
    ]
    .iter()
    .map(|k| c[*k].as_i64().unwrap())
    .sum();
    assert_eq!(
        c["all"].as_i64().unwrap(),
        sum,
        "[?date={D1}] counts.all == 5 个 dated tab 之和: {counts}"
    );
}

// ===========================================================================
//  5. ★ delivered tab 口径一致：翻到最后一页累计 == counts.delivered
// ===========================================================================

/// 2026-10-12：`delivered` 收窄成**只有** `DELIVERED`（`READY_TO_SHIP` 被
/// `inspecting` 接管）。角标若是 `READY_TO_SHIP + DELIVERED` 就会与本用例的
/// 期望值 7 差出一截。
#[tokio::test]
async fn delivered_tab_list_total_equals_counts_delivered() {
    let (pool, app, token) = bootstrap().await;
    let cid = insert_customer(&pool, "六厂").await;

    // 造出「跨多页」的 delivered 桶：7 DELIVERED，page size=4 → 2 页。
    for i in 0..7 {
        let mut s = PartSpec::new(cid, "DELIVERED");
        s.serial_no = Some(format!("FX-D{i}"));
        insert_part(&pool, &s).await;
    }
    // 干扰项：READY_TO_SHIP 归 inspecting，不再进 delivered
    for i in 0..3 {
        let mut s = PartSpec::new(cid, "READY_TO_SHIP");
        s.serial_no = Some(format!("FX-R{i}"));
        insert_part(&pool, &s).await;
    }
    for st in ["PENDING", "IN_PROCESS", "INSPECTION"] {
        insert_part(&pool, &PartSpec::new(cid, st)).await;
    }

    let counts = home(&app, &token, "").await;
    let delivered_count = counts["data"]["counts"]["delivered"]
        .as_i64()
        .expect("counts.delivered 必须是 number");
    assert_eq!(
        delivered_count, 7,
        "角标 delivered 2026-10-12 起**只有** DELIVERED（READY_TO_SHIP 归 inspecting）: {counts}"
    );
    assert_eq!(
        counts["data"]["counts"]["inspecting"],
        json!(4),
        "READY_TO_SHIP 3 + INSPECTION 1 = 4: {counts}"
    );

    // 一直翻到 hasMore == false，累计条数必须等于角标
    let mut seen: Vec<String> = Vec::new();
    let mut page_no = 1;
    loop {
        let env = page(
            &app,
            &token,
            &format!("status=delivered&page={page_no}&size=4"),
        )
        .await;
        let list = list_of(&env);
        for id in card_ids(list) {
            assert!(
                !seen.contains(&id),
                "翻页出现重复 id {id}（page={page_no}）：{env}"
            );
            seen.push(id);
        }
        if !env["data"]["hasMore"]
            .as_bool()
            .expect("hasMore 必须是 bool")
        {
            break;
        }
        page_no += 1;
        assert!(page_no <= 10, "hasMore 未收敛（疑似死循环）");
    }

    assert_eq!(
        seen.len() as i64,
        delivered_count,
        "delivered tab 翻到最后一页的累计条数必须 == counts.delivered（旧实现的 198 vs 126 裂缝）"
    );
    assert_eq!(seen.len(), 7);
}

// ===========================================================================
//  6. 非法 `status` → 422 + 40001
// ===========================================================================

#[tokio::test]
async fn invalid_status_is_validation_error() {
    let (_pool, app, token) = bootstrap().await;

    for bad in [
        "PENDING", // DB 原值不被接受（前端传的是 tab 值）
        "IN_PROCESS",
        "OUTSOURCE",
        "INSPECTION",
        "READY_TO_SHIP",
        "DELIVERED",
        "GARBAGE",
        // 旧 tab 值：2026-10-12 品检 tab 更名 inspecting，外协 tab 是新增的
        "pendingInspection",
        // 注入串（**URL 编码**：空格 / 分号不是合法 URI 字符）
        "%3B%20DROP%20TABLE%20t_part",
        "pendingproduction", // 大小写敏感
    ] {
        let uri = format!("{HOME_URI}?status={bad}");
        let (status, env) = send(app.clone(), json_request("GET", &uri, None, Some(&token))).await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "[{bad}] 应 422: {env}"
        );
        assert_eq!(env["code"], 40001, "[{bad}] 应是 40001 VALIDATION: {env}");
    }

    // 增量端点同样守白名单
    let uri = format!("{PAGE_URI}?status=PENDING");
    let (status, env) = send(app.clone(), json_request("GET", &uri, None, Some(&token))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "/page: {env}");
    assert_eq!(env["code"], 40001);

    // ⚠️ 白名单校验**优先于**日期作用域：非法 status 即使带合法 ?date 也得 422
    let uri = format!("{HOME_URI}?status=GARBAGE&date={D1}");
    let (status, env) = send(app, json_request("GET", &uri, None, Some(&token))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{uri}: {env}");
    assert_eq!(env["code"], 40001);
}

// ===========================================================================
//  6b. 非法 `?date=` 格式 → HTTP 400 纯文本（提取器层，不走 R<T>）
// ===========================================================================

/// 非法 `?date=` 落 axum `Query` 提取器层 ⇒ **HTTP 400 纯文本**（与 `?page=abc`
/// 同档），**不是** `AppError::validation` 的 40001 / 422。
///
/// ⚠️ 2026-10-12 **实测**：chrono 的 `NaiveDate` 反序列化**不要求**月/日零填充，
/// `?date=2026-8-4` 是**合法**的（解析成 2026-08-04）⇒ 本域接受宽格式，
/// 只拒**解析不出日期**的串。契约见 `docs/api/wx.md` §4。
#[tokio::test]
async fn malformed_date_is_rejected_by_the_query_extractor() {
    let (pool, app, token) = bootstrap().await;
    let cid = insert_customer(&pool, "六厂").await;
    insert_part(&pool, &PartSpec::new(cid, "PENDING")).await;

    for bad in [
        "2026-02-30",
        "2026-13-01",
        "not-a-date",
        "20260804",
        "2026-08-32",
    ] {
        let uri = format!("{HOME_URI}?date={bad}");
        let (status, raw) =
            send_raw(app.clone(), json_request("GET", &uri, None, Some(&token))).await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "[?date={bad}] 应 400: raw={raw:?}"
        );
        assert!(
            !raw.contains("40001"),
            "[?date={bad}] 应是提取器层 400 纯文本、不是 R<T> 信封: raw={raw:?}"
        );
    }

    // 反向确认：合法日期正常放行。⚠️ `2026-8-4`（不零填充）chrono 也接受，
    // 本域据此**刻意不**对格式再收紧 —— 收紧会把小程序某一端可能发来的宽格式
    // 打成 400，是比「格式不严」严重得多的失败。
    for good in [D1, D2, "2026-8-4"] {
        get_ok(&app, &format!("{HOME_URI}?date={good}"), &token).await;
    }
}

// ===========================================================================
//  7. ★ 尾斜杠形态钉死
// ===========================================================================

/// 2026-10-11 **实测**：本仓 axum 版本下 `nest("/part-list")` + 内层 `route("/")`
/// **只匹配无尾斜杠**的 `/wx/part-list`；带尾斜杠的 `/wx/part-list/` 落到
/// axum 默认 fallback ⇒ **HTTP 404 空 body**。
///
/// 旧 `/wx/parts/?…`（带尾斜杠）同因 —— 小程序侧曾按**相反**的假设发请求并踩过
/// 404。本用例把两种形态都钉死，防 axum 升级 / nest 改写后行为漂移。
#[tokio::test]
async fn trailing_slash_form_is_pinned() {
    let (pool, app, token) = bootstrap().await;
    let cid = insert_customer(&pool, "六厂").await;
    insert_part(&pool, &PartSpec::new(cid, "PENDING")).await;

    // 无尾斜杠 → 200（业务形态）
    let env = get_ok(&app, HOME_URI, &token).await;
    assert_eq!(
        list_of(&env).len(),
        1,
        "无尾斜杠形态必须命中 handler: {env}"
    );

    // 带尾斜杠 → 实测 404（用 send_raw 拿裸 body：404 走 axum fallback，
    // body 是空的、不走 R<T> 信封，send 会 panic）
    let (status, raw) = send_raw(
        app.clone(),
        json_request("GET", "/wx/part-list/", None, Some(&token)),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "带尾斜杠形态实测是 404（实测结论登记在 docs/api/wx.md §5）：raw={raw:?}"
    );
    assert!(
        raw.is_empty(),
        "404 是 axum 默认 fallback（空 body），不是 R<T> 信封：raw={raw:?}"
    );

    // 再钉一次 `/page` 的两个形态（无尾斜杠命中、带尾斜杠 404）
    get_ok(&app, PAGE_URI, &token).await;
    let (status, _) = send_raw(
        app,
        json_request("GET", "/wx/part-list/page/", None, Some(&token)),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "/page/ 带尾斜杠同样 404");
}

// ===========================================================================
//  8. 旧路径全部 404（硬切无 alias）
// ===========================================================================

#[tokio::test]
async fn legacy_wx_paths_are_all_gone() {
    let (_pool, app, token) = bootstrap().await;

    for uri in [
        "/wx/parts/counts",
        "/wx/parts",
        "/wx/parts/",
        "/wx/parts/?status=PENDING",
        "/wx/parts/by-serial/F2256",
        "/wx/dashboard/home",
        // 旧登录路径（2026-10-11 硬切改名）
        "/wx/iam/wx-login",
        "/wx/iam/",
    ] {
        let (status, raw) =
            send_raw(app.clone(), json_request("GET", uri, None, Some(&token))).await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "旧路径 {uri} 必须 404（硬切无 alias）：raw={raw:?}"
        );
    }
}

// ===========================================================================
//  9. `/page?page=2` 与 `/?page=2` 的 list 逐字一致
// ===========================================================================

#[tokio::test]
async fn page_endpoint_matches_home_endpoint_at_same_page() {
    let (pool, app, token) = bootstrap().await;
    let cid = insert_customer(&pool, "六厂").await;
    for i in 0..7 {
        let mut s = PartSpec::new(cid, "IN_PROCESS");
        s.serial_no = Some(format!("FX-P{i}"));
        insert_part(&pool, &s).await;
    }

    for page_no in [1, 2, 3] {
        let h = home(&app, &token, &format!("page={page_no}&size=3")).await;
        let p = page(&app, &token, &format!("page={page_no}&size=3")).await;

        assert_eq!(
            list_of(&h),
            list_of(&p),
            "?page={page_no} 时首屏端点与增量端点的 list 必须逐字一致（两端点共用同一查询）"
        );
        assert_eq!(
            h["data"]["hasMore"], p["data"]["hasMore"],
            "?page={page_no} 时两端点的 hasMore 也必须一致"
        );
    }

    // 首屏端点比增量端点多一个 counts 字段 —— 这是两者**唯一**的结构差异
    let h = home(&app, &token, "page=1&size=3").await;
    let p = page(&app, &token, "page=1&size=3").await;
    assert!(h["data"]["counts"].is_object());
    assert!(
        p["data"].get("counts").is_none(),
        "增量端点不该重算角标：{p}"
    );
}

// ===========================================================================
//  10. 分页不重不漏
// ===========================================================================

#[tokio::test]
async fn pagination_has_no_overlap_or_gap() {
    let (pool, app, token) = bootstrap().await;
    let cid = insert_customer(&pool, "六厂").await;
    // 7 行 / size 3 → 3 页（3 + 3 + 1）
    let mut all_ids = Vec::new();
    for i in 0..7 {
        let mut s = PartSpec::new(cid, "PENDING");
        s.serial_no = Some(format!("FX-N{i}"));
        all_ids.push(insert_part(&pool, &s).await.to_string());
    }

    let mut seen: Vec<String> = Vec::new();
    let mut has_more = true;
    let mut page_no = 1;
    while has_more {
        let env = page(&app, &token, &format!("page={page_no}&size=3")).await;
        let list = list_of(&env);
        for id in card_ids(list) {
            assert!(!seen.contains(&id), "第 {page_no} 页出现重复 id {id}");
            seen.push(id);
        }
        has_more = env["data"]["hasMore"].as_bool().unwrap();
        page_no += 1;
        assert!(page_no <= 10, "hasMore 未收敛");
    }

    assert_eq!(
        seen.len(),
        7,
        "7 行 / size 3 → 累计 7 行（3+3+1），不重不漏"
    );
    seen.sort();
    all_ids.sort();
    assert_eq!(seen, all_ids, "翻页结果必须是全量的无重复集合");

    // 末页 hasMore 必须为 false、且末页行数 < size（否则就是差一行）
    let last = page(&app, &token, "page=3&size=3").await;
    assert_eq!(list_of(&last).len(), 1);
    assert_eq!(last["data"]["hasMore"], json!(false));

    // size clamp：size=999 → clamp 50；size=0 → clamp 1
    let big = page(&app, &token, "size=999").await;
    assert_eq!(list_of(&big).len(), 7, "clamp 后仍一次取全（7 < 50）");
    let zero = page(&app, &token, "size=0").await;
    assert_eq!(list_of(&zero).len(), 1, "size=0 → clamp 1");
    // page 边界：page=0 → max(1)
    let p0 = page(&app, &token, "page=0&size=3").await;
    let p1 = page(&app, &token, "page=1&size=3").await;
    assert_eq!(
        card_ids(list_of(&p0)),
        card_ids(list_of(&p1)),
        "page=0 → page=1"
    );
}

// ===========================================================================
//  11. ★ deliveredQty 真值
// ===========================================================================

#[tokio::test]
async fn delivered_qty_reflects_delivered_batches() {
    let (pool, app, token) = bootstrap().await;
    let cid = insert_customer(&pool, "六厂").await;

    // 工单 8 件：1 个 DELIVERED 批次 3 件 + 1 个 IN_PROCESS 批次 5 件
    //   ⇒ deliveredQty 应是 3（不是 0、不是 8）
    let mut spec = PartSpec::new(cid, "IN_PROCESS");
    spec.quantity = 8;
    spec.serial_no = Some("FX-PARTIAL".to_string());
    let part_id = insert_part(&pool, &spec).await;
    insert_batch(&pool, part_id, 1, 3, "DELIVERED").await;
    insert_batch(&pool, part_id, 2, 5, "IN_PROCESS").await;

    // 对照组：一件没交 → 0；全交 → == totalQty
    let mut none = PartSpec::new(cid, "PENDING");
    none.quantity = 4;
    none.serial_no = Some("FX-NONE".to_string());
    let none_id = insert_part(&pool, &none).await;
    insert_batch(&pool, none_id, 1, 4, "PENDING").await;

    let mut full = PartSpec::new(cid, "DELIVERED");
    full.quantity = 6;
    full.serial_no = Some("FX-FULL".to_string());
    let full_id = insert_part(&pool, &full).await;
    insert_batch(&pool, full_id, 1, 6, "DELIVERED").await;

    let env = page(&app, &token, "size=50").await;
    let list = list_of(&env);

    let c = card_by_id(list, part_id);
    assert_eq!(
        c["deliveredQty"],
        json!(3),
        "部分交付工单的 deliveredQty: {c}"
    );
    assert_eq!(c["totalQty"], json!(8), "totalQty 是 t_part.quantity: {c}");

    let c = card_by_id(list, none_id);
    assert_eq!(
        c["deliveredQty"],
        json!(0),
        "无已交批次 → deliveredQty = 0（COALESCE）: {c}"
    );
    assert_eq!(c["totalQty"], json!(4));

    let c = card_by_id(list, full_id);
    assert_eq!(
        c["deliveredQty"],
        json!(6),
        "全交工单 deliveredQty == totalQty: {c}"
    );

    // 软删批次不计入已交量
    let mut s = PartSpec::new(cid, "IN_PROCESS");
    s.quantity = 5;
    s.serial_no = Some("FX-SOFTDEL".to_string());
    let sd_id = insert_part(&pool, &s).await;
    let b = insert_batch(&pool, sd_id, 1, 5, "DELIVERED").await;
    sqlx::query("UPDATE t_part_batch SET deleted_at = now() WHERE id = $1")
        .bind(b)
        .execute(&pool)
        .await
        .expect("软删批次");
    let env2 = page(&app, &token, "size=50").await;
    let c = card_by_id(list_of(&env2), sd_id);
    assert_eq!(c["deliveredQty"], json!(0), "软删批次不计入已交量: {c}");
}

// ===========================================================================
//  12. 登录响应只含 6 个字段
// ===========================================================================

/// `POST /wx/login/wecom` 的响应体必须**只有** `{token, refresh_token, user{id,
/// username, full_name, roles}}` —— **不带** iam 域 `LoginResponse` 的
/// `expires_in` / `is_active` / `shelf_ids` / `menus`。
///
/// 本用例只验**形状**（白名单 + 禁列），成功路径的 token / 40106 / 40107 /
/// 40109 等 15 条既有用例在 `tests/wecom_login.rs`（同一端点、同一 mock）。
#[tokio::test]
async fn wx_login_response_only_exposes_six_fields() {
    use std::sync::Arc;

    use hsh_erp_rust::modules::wx::wecom_client::{
        MockWeComApiClient, WeComApiClient, WeComSession,
    };
    use hsh_erp_test_support::{WecomFixture, load_wecom_fixture, test_state_with_wecom};

    let pool = test_pool().await;
    let _fx = load_iam_fixture(&pool).await;
    let _wfx = load_wecom_fixture(&pool).await;

    let corp = WecomFixture::CORP_ID.to_string();
    let userid = WecomFixture::MANAGER_WX_USER_ID.to_string();
    let mut m = MockWeComApiClient::default();
    m.expect_code_to_session().returning(move |_code| {
        Ok(WeComSession {
            corp_id: corp.clone(),
            user_id: userid.clone(),
        })
    });
    let state = test_state_with_wecom(pool, Arc::new(m), WecomFixture::CORP_ID).await;
    let app = test_app(state);

    let (status, env) = send(
        app,
        json_request(
            "POST",
            LOGIN_URI,
            Some(json!({ "code": "valid-code" })),
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "wx-login 应 200: {env}");

    let data = env["data"].as_object().expect("data 必须是对象");
    let mut keys: Vec<&str> = data.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        vec!["refresh_token", "token", "user"],
        "wx 登录响应顶层只该有 3 个键（不得出现 expires_in / is_active / shelf_ids / menus）: {env}"
    );

    let user = data
        .get("user")
        .and_then(|u| u.as_object())
        .expect("user 必须是对象");
    let mut ukeys: Vec<&str> = user.keys().map(String::as_str).collect();
    ukeys.sort_unstable();
    assert_eq!(
        ukeys,
        vec!["full_name", "id", "roles", "username"],
        "user 只该有 4 个键（is_active / shelf_ids / menus 是 Web 端专用，必须缺席）: {env}"
    );

    for banned in ["expires_in", "is_active", "shelf_ids", "menus"] {
        assert!(
            !user.contains_key(banned),
            "user 不该含 {banned}（小程序零消费 + 多余负载）: {env}"
        );
        assert!(!data.contains_key(banned), "data 不该含 {banned}: {env}");
    }

    // 雪花 id 走 JSON string
    assert!(
        user["id"].as_str().is_some(),
        "user.id 必须是 string(i64)（防 JS 精度截断）: {env}"
    );
    assert!(data["token"].as_str().is_some(), "token 必填: {env}");
    assert!(
        data["refresh_token"].as_str().is_some(),
        "refresh_token 必填: {env}"
    );
    assert!(user["roles"].is_array(), "roles 必须是数组: {env}");
}

// ===========================================================================
//  13. 首屏响应形状（逐字段形态）
// ===========================================================================

#[tokio::test]
async fn home_response_shape_matches_contract() {
    let (pool, app, token) = bootstrap().await;
    let cid = insert_customer(&pool, "六厂").await;
    let mut s = PartSpec::new(cid, "IN_PROCESS");
    s.quantity = 8;
    s.serial_no = Some("F2256".to_string());
    let id = insert_part(&pool, &s).await;
    insert_batch(&pool, id, 1, 3, "DELIVERED").await;
    insert_batch(&pool, id, 2, 5, "IN_PROCESS").await;

    let env = home(&app, &token, "").await;

    // 顶层只有 counts / list / hasMore
    let data = env["data"].as_object().expect("data");
    let mut keys: Vec<&str> = data.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        vec!["counts", "hasMore", "list"],
        "首屏响应顶层只该有 counts / list / hasMore（无 total / page / size）: {env}"
    );

    // counts 的 7 个键
    let counts = data["counts"].as_object().expect("counts");
    let mut ckeys: Vec<&str> = counts.keys().map(String::as_str).collect();
    ckeys.sort_unstable();
    assert_eq!(
        ckeys,
        vec![
            "all",
            "delivered",
            "inProduction",
            "inspecting",
            "noSystemDate",
            "outsource",
            "pendingProduction"
        ],
        "counts 的键名必须是前端 tab 值（camelCase），2026-10-12 起是 7 个: {env}"
    );

    // 卡片形态
    let list = list_of(&env);
    assert_eq!(list.len(), 1);
    let c = &list[0];
    assert_eq!(
        c,
        &json!({
            "kind": "workOrder",
            "id": id.to_string(),
            "serialNo": "F2256",
            "name": "测试件",
            "code": format!("D-{id}"),
            "dueDate": "2026-08-04",
            "customer": "六厂",
            "deliveredQty": 3,
            "totalQty": 8,
            "status": "inProduction"
        }),
        "workOrder 卡片必须逐字长成前端 WorkOrderPartCard 形态（字段名 / 键序 / 类型）"
    );

    // ❌ 没有 drawingUrl（已知有意缺口，t_part 无图纸列）
    assert!(
        !c.as_object().unwrap().contains_key("drawingUrl"),
        "本轮刻意不产出 drawingUrl（等 COS 文件服务接入后单独 PR）：{c}"
    );

    // hasMore 是 camelCase
    assert_eq!(env["data"]["hasMore"], json!(false));
    assert!(!data.contains_key("has_more"));

    // 软删工单不出现
    let mut sd = PartSpec::new(cid, "PENDING");
    sd.deleted = true;
    let sd_id = insert_part(&pool, &sd).await;
    let env2 = home(&app, &token, "size=50").await;
    assert!(
        !card_ids(list_of(&env2)).contains(&sd_id.to_string()),
        "软删工单不该出现（deleted_at IS NULL 闸门）: {env2}"
    );
}

// ===========================================================================
//  14. kind=batch 变体（装配件子件）
// ===========================================================================

#[tokio::test]
async fn batch_kind_card_for_assembly_children() {
    let (pool, app, token) = bootstrap().await;
    let cid = insert_customer(&pool, "六厂").await;
    let asm = insert_assembly(&pool, cid).await;

    // 子件：assembly_id 非空 ⇒ kind=batch
    let mut child = PartSpec::new(cid, "IN_PROCESS");
    child.assembly_id = Some(asm);
    child.quantity = 5;
    child.serial_no = Some("FX-CHILD".to_string());
    let child_id = insert_part(&pool, &child).await;

    // 活跃批次（COMPLETED / CANCELLED 不算活跃）
    insert_batch(&pool, child_id, 1, 2, "IN_PROCESS").await;
    insert_batch(&pool, child_id, 2, 3, "COMPLETED").await;

    let env = page(&app, &token, "size=50").await;
    let list = list_of(&env);
    let c = card_by_id(list, child_id);

    assert_eq!(
        c,
        &json!({
            "kind": "batch",
            "id": child_id.to_string(),
            "serialNo": "FX-CHILD",
            "name": "测试件",
            "code": format!("D-{child_id}"),
            "dueDate": "2026-08-04",
            "batchNo": 1,
            "batchQty": 5
        }),
        "assembly_id 非空 ⇒ kind=batch；batchNo 取**活跃**批次（COMPLETED 不算）；\
         batchQty 是 t_part.quantity 而非批次量"
    );
    assert!(
        !c.as_object().unwrap().contains_key("status"),
        "batch 变体不含 status（前端 BatchPartCard 无此字段）: {c}"
    );
    assert!(
        !c.as_object().unwrap().contains_key("current_batch_id"),
        "current_batch_id 是字段级移除项，不得泄漏：{c}"
    );

    // 反向确认：独立件是 workOrder（kind 判定口径没被改坏）
    let mut solo = PartSpec::new(cid, "PENDING");
    solo.serial_no = Some("FX-SOLO".to_string());
    let solo_id = insert_part(&pool, &solo).await;
    let env2 = page(&app, &token, "size=50").await;
    assert_eq!(card_by_id(list_of(&env2), solo_id)["kind"], "workOrder");
}

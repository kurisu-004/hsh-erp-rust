//! `wx::production` 子模块的集成测试（2026-10-11 新增，wx BFF 重构 B3）
//!
//! 端点（测试 app 直接挂 `v2_router`，**不带** `/api/v2` 前缀）：
//! - `GET /wx/production` —— 首屏聚合（`worker` + `stats` + `counts` + `list` +
//!   `hasMore`）
//! - `GET /wx/production/page` —— 上拉增量（`list` + `hasMore`）
//!
//! ## 覆盖清单（与 B3 任务的 16 条验收标准逐条对应）
//!
//! | # | 场景 | 用例 |
//! |---|---|---|
//! | 1 | 无 Authorization → 401 / 40100 | [`production_requires_auth`] |
//! | 2 | 首屏响应 5 个键齐全 | [`home_response_has_all_five_keys`] |
//! | 3 | `/page?page=2` 与 `/?page=2` 的 `list` 逐字一致 | [`page_endpoint_matches_home_endpoint_at_same_page`] |
//! | 4 | 分页不重不漏 | [`pagination_has_no_overlap_or_gap`] |
//! | 5 | 两个 tab 各命中；**非法 tab** → 422 + 40001 | [`both_tabs_return_rows_and_invalid_tab_is_422`] |
//! | 6 | **非法 `period`** → 422 + 40001 | [`invalid_period_is_validation_error`] |
//! | 7 | `done` tab 翻到底 == `counts.done` | [`done_tab_list_total_equals_counts_done`] |
//! | 8 | **`worker_id` 未绑定** → `worker: null` + 零值 stats，仍 200 | [`unbound_worker_yields_null_worker_and_zero_stats`] |
//! | 9 | **`worker_id` 已绑定** → name / workType / batchCount | [`bound_worker_exposes_name_work_type_and_nonzero_stats`] |
//! | 10 | **★ `stats` 真按工人过滤**（本次 bug 修复的核心回归） | [`stats_are_filtered_by_the_logged_in_users_worker`] |
//! | 11 | `status` 按 tab 折叠成 2 类 | [`status_is_folded_per_tab`] |
//! | 12 | `assignedTo`：在工人手里有值、在货架上为 null | [`assigned_to_follows_batch_location`] |
//! | 13 | `workHours` / `finishedDate` 无值时是 **null** | [`missing_work_hours_and_finished_date_are_null`] |
//! | 14 | **尾斜杠形态钉死** | [`trailing_slash_form_is_pinned`] |
//! | 15 | 旧 `/wx/batches/*` + `/wx/worker/stats` 全 404 | [`legacy_batches_and_worker_paths_are_all_gone`] |
//! | 16 | `part_list` / `login` 端点不受影响 | [`part_list_and_login_endpoints_still_work`] |
//!
//! 另有 [`tab_is_required_and_bad_page_size_clamps`]（`?tab=` 必填 + 分页 clamp）与
//! [`counts_exclude_batches_of_soft_deleted_parts`]（review 第 1 轮 m5 回归）。
//!
//! ## ⚠️ period 固定为 `2026-10`（不依赖墙上时钟）
//! 角标与列表的 period 闸门是 `updated_at::text LIKE 'YYYY-MM%'` /
//! `created_at::text LIKE 'YYYY-MM%'`（逐字沿用旧实现）。若测试数据用 `now()` 而
//! query 参数用 `chrono::Local::now()`，两端一旦跨月界就会整片漏掉。故 fixture 的
//! 时间戳与 query 参数**都是字面量 `2026-10`**，二者物理同源。
//!
//! ## 串行化
//! `test_pool()` 每次 fresh database（DB 间 schema 完全独立），无需 Mutex /
//! `--test-threads=1`。每个测试造自己的差异行，互不污染。
//!
//! ## fixture
//! auth 用 `load_iam_fixture`（段 110+）；工人 / 工种 / 零件 / 批次 / 事件用
//! **运行时雪花 ID** 现场插（文件下半部的本地 `async fn` helper），与任何常量
//! ID 段物理不相交。

use axum::http::StatusCode;
use serde_json::{Value, json};
use sqlx::PgPool;

use hsh_erp_test_support::{
    IamFixture, json_request, load_iam_fixture, login_token, pool_snowflake, send, send_raw,
    test_app, test_pool, test_state,
};

/// 首屏聚合端点（测试 app 无 `/api/v2` 前缀；**无尾斜杠**）
const HOME_URI: &str = "/wx/production";
/// 上拉增量端点
const PAGE_URI: &str = "/wx/production/page";

/// 测试统一使用的 period（与 [`IN_PERIOD_TS`] 物理同源，见文件头「period 固定」）
const PERIOD: &str = "2026-10";
/// 落在 [`PERIOD`] 内的字面量时间戳（`timestamp without time zone`，正中月份，
/// 任何 ±12h 时区偏移都仍落在同一个月）
const IN_PERIOD_TS: &str = "2026-10-05 12:00:00";
/// 落在**上一个**月份的时间戳（负例用：不该被 [`PERIOD`] 的闸门命中）
const OUT_PERIOD_TS: &str = "2026-09-05 12:00:00";

// ===========================================================================
//  Bootstrap
// ===========================================================================

/// fresh database + iam fixture → 以 MANAGER 身份登录。
async fn bootstrap() -> (PgPool, axum::Router, String, IamFixture) {
    let pool = test_pool().await;
    let fx = load_iam_fixture(&pool).await;
    let app = test_app(test_state(pool.clone()).await);
    let token = login_token(&app, &fx.manager_username, IamFixture::PASSWORD).await;
    (pool, app, token, fx)
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

/// 插一行 `t_part`（时间戳固定在 [`IN_PERIOD_TS`]）。
///
/// ⚠️ `t_part` **不**参与本域的 period 闸门（闸门打在 `t_part_batch.updated_at` 与
/// `t_part_event.created_at` 上），这里的时间戳只是为了与批次/事件保持同一叙事。
async fn insert_part(pool: &PgPool, customer_id: i64) -> i64 {
    let id = next_id();
    sqlx::query(
        "INSERT INTO t_part (id, serial_no, name, drawing_no, applicant_name, quantity, \
         request_date, planned_delivery_date, status, is_urgent, customer_id, version, \
         created_at, updated_at) \
         VALUES ($1, NULL, '测试件', $2, 'T', 8, '2026-10-01', '2026-10-20', 'IN_PROCESS', \
                 false, $3, 0, $4::timestamp, $4::timestamp)",
    )
    .bind(id)
    .bind(format!("D-{id}"))
    .bind(customer_id)
    .bind(IN_PERIOD_TS)
    .execute(pool)
    .await
    .expect("insert t_part");
    id
}

/// 一个 `t_part_batch` 行的构造参数（除 id / part_id 外全字段）。
///
/// 用 spec struct 而不是 8 个位置参数：位置参数超过 7 个会触发 clippy
/// `too_many_arguments`，且 `location` / `holder_id` / `updated_at` 三者在调用点
/// 挨着，很容易传错顺序。
#[derive(Debug, Clone)]
struct BatchSpec<'a> {
    batch_no: i32,
    quantity: i32,
    /// DB 原值状态：`IN_PROCESS` / `DELIVERED` / `COMPLETED` …
    status: &'a str,
    /// `Some("WORKER")` ⇒ 批次在工人手里；`Some("PRODUCTION_SHELF")` ⇒ 在货架上
    location: Option<&'a str>,
    /// `current_holder_id`（可空；可与 `location` 组合出「在货架上但 holder 仍指向
    /// 某工人」这个 `assignedTo: null` 的关键场景）
    holder_id: Option<i64>,
    /// `created_at` / `updated_at` 同值；默认 [`IN_PERIOD_TS`]，负例传
    /// [`OUT_PERIOD_TS`]
    updated_at: &'a str,
}

impl<'a> BatchSpec<'a> {
    /// `IN_PROCESS` + 在工人手里 + 落在当月（本域最常见的形态）
    fn in_progress(quantity: i32) -> Self {
        Self {
            batch_no: 1,
            quantity,
            status: "IN_PROCESS",
            location: Some("WORKER"),
            holder_id: None,
            updated_at: IN_PERIOD_TS,
        }
    }

    /// 已完工（`status` 可给 `DELIVERED` 或 `COMPLETED`）+ 当月
    fn done(status: &'a str, quantity: i32) -> Self {
        Self {
            batch_no: 1,
            quantity,
            status,
            location: Some("WORKER"),
            holder_id: None,
            updated_at: IN_PERIOD_TS,
        }
    }

    /// 指定持有人（`location` 仍是 `'WORKER'`）
    fn with_holder(mut self, holder: i64) -> Self {
        self.holder_id = Some(holder);
        self
    }

    /// 指定批次位置（`'WORKER'` / `'PRODUCTION_SHELF'`）
    fn at(mut self, location: &'a str) -> Self {
        self.location = Some(location);
        self
    }

    /// 指定 `updated_at`（负例：把批次推到上个月）
    fn at_time(mut self, ts: &'a str) -> Self {
        self.updated_at = ts;
        self
    }
}

/// 插一行 `t_part_batch`（`updated_at` 显式可控，故 `in_progress` 桶的 period 闸门
/// 可断言）。
async fn insert_batch(pool: &PgPool, part_id: i64, s: &BatchSpec<'_>) -> i64 {
    let id = next_id();
    sqlx::query(
        "INSERT INTO t_part_batch (id, part_id, batch_no, quantity, status, location, \
         current_holder_id, version, created_at, updated_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, 0, $8::timestamp, $8::timestamp)",
    )
    .bind(id)
    .bind(part_id)
    .bind(s.batch_no)
    .bind(s.quantity)
    .bind(s.status)
    .bind(s.location)
    .bind(s.holder_id)
    .bind(s.updated_at)
    .execute(pool)
    .await
    .expect("insert t_part_batch");
    id
}

/// 插一行 `t_part_event`（`created_at` 固定，period 闸门的输入）。
async fn insert_event(
    pool: &PgPool,
    part_id: i64,
    batch_id: i64,
    worker_id: Option<i64>,
    event_type: &str,
    quantity: Option<i32>,
    created_at: &str,
) -> i64 {
    let id = next_id();
    sqlx::query(
        "INSERT INTO t_part_event (id, part_id, worker_id, event_type, batch_id, quantity, \
         created_at) VALUES ($1, $2, $3, $4, $5, $6, $7::timestamp)",
    )
    .bind(id)
    .bind(part_id)
    .bind(worker_id)
    .bind(event_type)
    .bind(batch_id)
    .bind(quantity)
    .bind(created_at)
    .execute(pool)
    .await
    .expect("insert t_part_event");
    id
}

/// 插一行 `t_work_type`（`workType` 的来源）。
async fn insert_work_type(pool: &PgPool, code: &str, name: &str) -> i64 {
    let id = next_id();
    sqlx::query(
        "INSERT INTO t_work_type (id, code, name, sort_order, version, created_at, updated_at) \
         VALUES ($1, $2, $3, 0, 0, $4::timestamp, $4::timestamp)",
    )
    .bind(id)
    .bind(code)
    .bind(name)
    .bind(IN_PERIOD_TS)
    .execute(pool)
    .await
    .expect("insert t_work_type");
    id
}

/// 软删一行 `t_work_type`（验证「工种已软删 ⇒ `workType` 退化成空串」）。
async fn soft_delete_work_type(pool: &PgPool, work_type_id: i64) {
    sqlx::query("UPDATE t_work_type SET deleted_at = $2::timestamp WHERE id = $1")
        .bind(work_type_id)
        .bind(IN_PERIOD_TS)
        .execute(pool)
        .await
        .expect("soft delete t_work_type");
}

/// 插一行 `t_worker`（`work_type_id` 可空 = 未分配工种）。
async fn insert_worker(pool: &PgPool, badge: &str, name: &str, work_type_id: Option<i64>) -> i64 {
    let id = next_id();
    sqlx::query(
        "INSERT INTO t_worker (id, badge_code, name, is_active, work_type_id, version, \
         created_at, updated_at) VALUES ($1, $2, $3, true, $4, 0, $5::timestamp, $5::timestamp)",
    )
    .bind(id)
    .bind(badge)
    .bind(name)
    .bind(work_type_id)
    .bind(IN_PERIOD_TS)
    .execute(pool)
    .await
    .expect("insert t_worker");
    id
}

/// 把系统账号绑到某个工人（B1 新增的 `t_user.worker_id` 列）。
async fn bind_user_worker(pool: &PgPool, user_id: i64, worker_id: i64) {
    sqlx::query("UPDATE t_user SET worker_id = $2 WHERE id = $1")
        .bind(user_id)
        .bind(worker_id)
        .execute(pool)
        .await
        .expect("bind t_user.worker_id");
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
    get_ok(app, &format!("{HOME_URI}?{query}"), token).await
}

/// 增量端点（带 query 串）。
async fn page(app: &axum::Router, token: &str, query: &str) -> Value {
    get_ok(app, &format!("{PAGE_URI}?{query}"), token).await
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
async fn production_requires_auth() {
    let (_pool, app, token, _fx) = bootstrap().await;

    for uri in [
        format!("{HOME_URI}?tab=in_progress"),
        format!("{PAGE_URI}?tab=in_progress"),
    ] {
        let (status, env) = send(app.clone(), json_request("GET", &uri, None, None)).await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "无 token 应 401: {uri} {env}"
        );
        assert_eq!(env["code"], 40100, "无 token 的错误码应是 40100: {env}");
    }

    // 反向确认：带 token 才通
    let env = get_ok(&app, &format!("{HOME_URI}?tab=done"), &token).await;
    assert!(env["data"].is_object(), "带 token 应拿到 data 对象: {env}");
}

// ===========================================================================
//  2. 首屏响应 5 个键齐全
// ===========================================================================

#[tokio::test]
async fn home_response_has_all_five_keys() {
    let (pool, app, token, _fx) = bootstrap().await;
    let cid = insert_customer(&pool, "六厂").await;
    let part_id = insert_part(&pool, cid).await;
    insert_batch(&pool, part_id, &BatchSpec::in_progress(5)).await;

    let env = home(&app, &token, "tab=in_progress").await;
    let obj = env["data"].as_object().expect("data 必须是对象");
    let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        vec!["counts", "hasMore", "list", "stats", "worker"],
        "首屏聚合响应恰好 5 个键: {env}"
    );

    // 四段的内部形状
    assert!(env["data"]["counts"].is_object());
    assert!(env["data"]["counts"].get("in_progress").is_some());
    assert!(env["data"]["counts"].get("done").is_some());
    assert!(env["data"]["stats"].is_object());
    assert!(env["data"]["stats"].get("batchCount").is_some());
    assert!(env["data"]["stats"].get("workHours").is_some());
    assert!(list_of(&env).len() == 1, "本域造的 1 个批次应命中: {env}");

    // 增量端点只有 2 个键（不重算角标 / 工人 / 统计）
    let p = page(&app, &token, "tab=in_progress").await;
    let pkeys: Vec<&str> = p["data"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(pkeys.len(), 2, "增量端点只该有 list + hasMore: {p}");
    assert!(p["data"].get("counts").is_none());
    assert!(p["data"].get("stats").is_none());
    assert!(p["data"].get("worker").is_none());
}

// ===========================================================================
//  3. `/page?page=2` 与 `/?page=2` 的 list 逐字一致
// ===========================================================================

#[tokio::test]
async fn page_endpoint_matches_home_endpoint_at_same_page() {
    let (pool, app, token, _fx) = bootstrap().await;
    let cid = insert_customer(&pool, "六厂").await;
    for i in 0..7 {
        let part_id = insert_part(&pool, cid).await;
        insert_batch(&pool, part_id, &BatchSpec::in_progress(3 + i)).await;
    }

    for page_no in [1, 2, 3] {
        let q = format!("tab=in_progress&period={PERIOD}&page={page_no}&size=3");
        let h = home(&app, &token, &q).await;
        let p = page(&app, &token, &q).await;

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
}

// ===========================================================================
//  4. 分页不重不漏
// ===========================================================================

#[tokio::test]
async fn pagination_has_no_overlap_or_gap() {
    let (pool, app, token, _fx) = bootstrap().await;
    let cid = insert_customer(&pool, "六厂").await;
    let mut all_ids = Vec::new();
    for i in 0..7 {
        let part_id = insert_part(&pool, cid).await;
        let b = insert_batch(&pool, part_id, &BatchSpec::in_progress(i + 1)).await;
        all_ids.push(b.to_string());
    }

    let mut seen: Vec<String> = Vec::new();
    let mut has_more = true;
    let mut page_no = 1;
    while has_more {
        let env = page(
            &app,
            &token,
            &format!("tab=in_progress&period={PERIOD}&page={page_no}&size=3"),
        )
        .await;
        for id in card_ids(list_of(&env)) {
            assert!(!seen.contains(&id), "第 {page_no} 页出现重复 id {id}");
            seen.push(id);
        }
        has_more = env["data"]["hasMore"].as_bool().unwrap();
        page_no += 1;
        assert!(page_no <= 10, "hasMore 未收敛（疑似死循环）");
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
    let last = page(&app, &token, "tab=in_progress&period=2026-10&page=3&size=3").await;
    assert_eq!(list_of(&last).len(), 1);
    assert_eq!(last["data"]["hasMore"], json!(false));

    // 边界：size=0 → clamp 1；size=999 → clamp 50（7 < 50，一次取全）
    let zero = page(&app, &token, "tab=in_progress&size=0").await;
    assert_eq!(list_of(&zero).len(), 1, "size=0 → clamp 1");
    let big = page(&app, &token, "tab=in_progress&size=999").await;
    assert_eq!(list_of(&big).len(), 7, "clamp 后仍一次取全（7 < 50）");
    // page=0 → max(1)
    let p0 = page(&app, &token, "tab=in_progress&page=0&size=3").await;
    let p1 = page(&app, &token, "tab=in_progress&page=1&size=3").await;
    assert_eq!(card_ids(list_of(&p0)), card_ids(list_of(&p1)), "page=0 → 1");
}

// ===========================================================================
//  5. 两个 tab 各命中；非法 tab → 422 + 40001
// ===========================================================================

#[tokio::test]
async fn both_tabs_return_rows_and_invalid_tab_is_422() {
    let (pool, app, token, _fx) = bootstrap().await;
    let cid = insert_customer(&pool, "六厂").await;

    // 进行中 2 个批次（走 `updated_at` 闸门）
    for i in 0..2 {
        let part_id = insert_part(&pool, cid).await;
        insert_batch(&pool, part_id, &BatchSpec::in_progress(i + 1)).await;
    }
    // 已完工 3 个批次（走「当月 DELIVERED 事件」闸门）；DELIVERED / COMPLETED 两种
    // 状态各覆盖一下
    for (i, st) in ["DELIVERED", "DELIVERED", "COMPLETED"].iter().enumerate() {
        let part_id = insert_part(&pool, cid).await;
        let b = insert_batch(&pool, part_id, &BatchSpec::done(st, i as i32 + 1)).await;
        insert_event(&pool, part_id, b, None, "DELIVERED", Some(2), IN_PERIOD_TS).await;
    }

    let ip = page(&app, &token, "tab=in_progress&period=2026-10&size=50").await;
    assert_eq!(list_of(&ip).len(), 2, "in_progress 桶 2 行: {ip}");

    let done = page(&app, &token, "tab=done&period=2026-10&size=50").await;
    assert_eq!(
        list_of(&done).len(),
        3,
        "done 桶是 DELIVERED + COMPLETED **两个**状态: {done}"
    );

    // 非法 tab（首屏 + 增量都验）
    for bad in [
        "IN_PROCESS", // DB 原值不被接受（前端传的是 tab 值）
        "DELIVERED",
        "GARBAGE",
        "inProgress", // part_list 的 tab 名在本域非法（两域 tab 值不同）
        // 注入串（**URL 编码**：空格 / 分号不是合法 URI 字符）
        "%3B%20DROP%20TABLE%20t_part_batch",
        "in_progress%20", // 尾随空格：大小写 / 空白都敏感（空格须 URL 编码）
    ] {
        for base in [HOME_URI, PAGE_URI] {
            let uri = format!("{base}?tab={bad}");
            let (status, env) =
                send(app.clone(), json_request("GET", &uri, None, Some(&token))).await;
            assert_eq!(
                status,
                StatusCode::UNPROCESSABLE_ENTITY,
                "[{bad}] @ {base} 应 422: {env}"
            );
            assert_eq!(env["code"], 40001, "[{bad}] @ {base} 应是 40001: {env}");
        }
    }
}

/// `?tab=` 是**必填**：缺字段走 axum 提取器层的 **HTTP 400 纯文本**（不走 `R<T>`），
/// 与传了但非法（422 + 40001）是**两种不同的失败**。见 `dto.rs` 的模块 doc。
#[tokio::test]
async fn tab_is_required_and_bad_page_size_clamps() {
    let (_pool, app, token, _fx) = bootstrap().await;

    let (status, raw) = send_raw(
        app.clone(),
        json_request(
            "GET",
            &format!("{HOME_URI}?period={PERIOD}"),
            None,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "缺 ?tab= 是 serde 缺字段 ⇒ 提取器层 400（不走 R<T>）: raw={raw:?}"
    );
    assert!(
        !raw.contains("\"code\""),
        "400 走纯文本、**不走** R<T> 信封（信封必有 code 键）: raw={raw:?}"
    );

    // `?page=abc` 同样是提取器层 400（仓库全局行为）
    let (status, _) = send_raw(
        app,
        json_request(
            "GET",
            &format!("{HOME_URI}?tab=done&page=abc"),
            None,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "?page=abc → 400 纯文本");
}

// ===========================================================================
//  6. 非法 period → 422 + 40001（HTTP 层）
// ===========================================================================

#[tokio::test]
async fn invalid_period_is_validation_error() {
    let (_pool, app, token, _fx) = bootstrap().await;

    // ⚠️ `2026/09` 里的 `/` 会把 query 串拆成两个参数，须 URL 编码成 `%2F`
    for bad in [
        "2026-9",    // 月份 1 位
        "2026%2F09", // 分隔符错（URL 编码）
        "2026-13",   // 月份 13
        "2026-00",   // 月份 0
        "26-09",     // 年份 2 位
    ] {
        for base in [HOME_URI, PAGE_URI] {
            let uri = format!("{base}?tab=done&period={bad}");
            let (status, env) =
                send(app.clone(), json_request("GET", &uri, None, Some(&token))).await;
            assert_eq!(
                status,
                StatusCode::UNPROCESSABLE_ENTITY,
                "[{bad}] @ {base} 应 422: {env}"
            );
            assert_eq!(env["code"], 40001, "[{bad}] @ {base} 应是 40001: {env}");
        }
    }

    // 反向确认：合法 period（2025-12）不报 400
    let env = page(&app, &token, "tab=done&period=2025-12").await;
    assert_eq!(env["code"], 0, "合法 period 应 200: {env}");
}

// ===========================================================================
//  7. done tab 翻到底 == counts.done
// ===========================================================================

#[tokio::test]
async fn done_tab_list_total_equals_counts_done() {
    let (pool, app, token, _fx) = bootstrap().await;
    let cid = insert_customer(&pool, "六厂").await;

    // 7 个已完工批次：4 DELIVERED + 3 COMPLETED，各带当月 DELIVERED 事件
    for i in 0..7 {
        let st = if i % 2 == 0 { "DELIVERED" } else { "COMPLETED" };
        let part_id = insert_part(&pool, cid).await;
        let b = insert_batch(&pool, part_id, &BatchSpec::done(st, i + 1)).await;
        insert_event(&pool, part_id, b, None, "DELIVERED", Some(3), IN_PERIOD_TS).await;
    }
    // 干扰项一：进行中批次（有 updated_at 但无 DELIVERED 事件）
    let p1 = insert_part(&pool, cid).await;
    insert_batch(&pool, p1, &BatchSpec::in_progress(1)).await;
    // 干扰项二：已完工但 DELIVERED 事件在上个月 ⇒ 不该计入当月 done
    let p2 = insert_part(&pool, cid).await;
    let b2 = insert_batch(&pool, p2, &BatchSpec::done("DELIVERED", 1)).await;
    insert_event(&pool, p2, b2, None, "DELIVERED", Some(3), OUT_PERIOD_TS).await;
    // 干扰项三：进行中但 `updated_at` 在上个月 ⇒ 不该计入当月 in_progress
    let p3 = insert_part(&pool, cid).await;
    insert_batch(&pool, p3, &BatchSpec::in_progress(1).at_time(OUT_PERIOD_TS)).await;

    let counts = home(&app, &token, "tab=done&period=2026-10").await;
    assert_eq!(
        counts["data"]["counts"]["done"],
        json!(7),
        "counts.done 应只算「当月有 DELIVERED 事件」的 7 行: {counts}"
    );
    assert_eq!(
        counts["data"]["counts"]["in_progress"],
        json!(1),
        "counts.in_progress 只算 updated_at 落当月的 1 行: {counts}"
    );

    // 一直翻到 hasMore == false，累计条数必须等于角标
    let mut seen: Vec<String> = Vec::new();
    let mut page_no = 1;
    loop {
        let env = page(
            &app,
            &token,
            &format!("tab=done&period={PERIOD}&page={page_no}&size=3"),
        )
        .await;
        for id in card_ids(list_of(&env)) {
            assert!(
                !seen.contains(&id),
                "翻页出现重复 id {id}（page={page_no}）"
            );
            seen.push(id);
        }
        if !env["data"]["hasMore"].as_bool().unwrap() {
            break;
        }
        page_no += 1;
        assert!(page_no <= 10, "hasMore 未收敛（疑似死循环）");
    }
    assert_eq!(
        seen.len() as i64,
        counts["data"]["counts"]["done"].as_i64().unwrap(),
        "done tab 翻到最后一页的累计条数必须 == counts.done"
    );
    assert_eq!(seen.len(), 7);
}

/// ★ review 第 1 轮 m5 回归：`counts` 不得计入**父工单已软删**的批次。
///
/// 该分叉从旧 `BatchCountsAgg::by_period` 逐字继承（旧的两条 count 标量查询只带
/// `b.deleted_at IS NULL`，**没有** list 侧 `JOIN t_part p` 的 `p.deleted_at IS NULL`）
/// ⇒ 修复前角标会**大于**列表实际行数。本轮给两条 count 各补了
/// `EXISTS(… p.deleted_at IS NULL)` 闸门。口径说明见 `docs/api/wx.md` §3.8 / §8.10。
#[tokio::test]
async fn counts_exclude_batches_of_soft_deleted_parts() {
    let (pool, app, token, _fx) = bootstrap().await;
    let cid = insert_customer(&pool, "六厂").await;

    // 3 个 `in_progress` 批次，全部落当月、批次自身未软删
    let p_live = insert_part(&pool, cid).await;
    insert_batch(&pool, p_live, &BatchSpec::in_progress(1)).await;

    let p_soft = insert_part(&pool, cid).await;
    insert_batch(&pool, p_soft, &BatchSpec::in_progress(1)).await;

    let p_batch_soft = insert_part(&pool, cid).await;
    let b_batch_soft = insert_batch(&pool, p_batch_soft, &BatchSpec::in_progress(1)).await;

    // ★ 关键干扰项：软删**父工单**，批次保持未软删。修复前 counts 会把它算进去，
    // 而 list 侧（INNER JOIN t_part + p.deleted_at IS NULL）看不到它。
    sqlx::query("UPDATE t_part SET deleted_at = now() WHERE id = $1")
        .bind(p_soft)
        .execute(&pool)
        .await
        .expect("soft delete t_part");

    // 另一个干扰项：批次自身已软删（两边都该排除）
    sqlx::query("UPDATE t_part_batch SET deleted_at = now() WHERE id = $1")
        .bind(b_batch_soft)
        .execute(&pool)
        .await
        .expect("soft delete t_part_batch");

    let counts = home(&app, &token, "tab=in_progress&period=2026-10").await;
    assert_eq!(
        counts["data"]["counts"]["in_progress"],
        json!(1),
        "counts.in_progress 只能算「父工单未软删 + 批次未软删」的 1 行: {counts}"
    );

    let list = page(&app, &token, "tab=in_progress&period=2026-10&size=50").await;
    let ids = card_ids(list_of(&list));
    assert_eq!(
        ids.len(),
        1,
        "list 侧只该出现 1 行，counts 必须与它一致: {list}"
    );
    assert!(
        !ids.iter().any(|id| *id == p_soft.to_string()),
        "父工单已软删的批次不该出现在 list 里: {list}"
    );
}

// ===========================================================================
//  8. ★ worker_id 未绑定 → worker: null + 零值 stats，HTTP 仍 200
// ===========================================================================

#[tokio::test]
async fn unbound_worker_yields_null_worker_and_zero_stats() {
    let (pool, app, token, fx) = bootstrap().await;
    let cid = insert_customer(&pool, "六厂").await;
    let part_id = insert_part(&pool, cid).await;
    let b = insert_batch(&pool, part_id, &BatchSpec::in_progress(5)).await;

    // 就算库里真有工人、有事件，只要 `t_user.worker_id` 是 NULL 就必须是未绑定形态
    let wt = insert_work_type(&pool, "CNC", "CNC 车工").await;
    let w = insert_worker(&pool, "UNBOUND-1", "游离工人", Some(wt)).await;
    insert_event(
        &pool,
        part_id,
        b,
        Some(w),
        "PICKED_UP",
        Some(9),
        IN_PERIOD_TS,
    )
    .await;

    // fixture 默认 `worker_id IS NULL`（B1 只加了列，没回填数据）
    let (worker_id,): (Option<i64>,) = sqlx::query_as("SELECT worker_id FROM t_user WHERE id = $1")
        .bind(fx.manager_user_id)
        .fetch_one(&pool)
        .await
        .expect("read t_user.worker_id");
    assert!(worker_id.is_none(), "fixture 账号本就该是未绑定状态");

    let (status, env) = send(
        app.clone(),
        json_request(
            "GET",
            &format!("{HOME_URI}?tab=in_progress&period={PERIOD}"),
            None,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "未绑定**不报错**，仍应 200: {env}");
    assert_eq!(
        env["data"]["worker"],
        Value::Null,
        "未绑定 ⇒ worker 是 null（不是 0/空对象）: {env}"
    );
    assert_eq!(
        env["data"]["stats"],
        json!({"batchCount": 0, "workHours": 0.0}),
        "未绑定 ⇒ 零值 stats: {env}"
    );
    // 列表数据**不受**工人绑定状态影响（非工人账号也能看批次）
    assert_eq!(list_of(&env).len(), 1, "列表不该受绑定状态影响: {env}");
}

// ===========================================================================
//  9. ★ worker_id 已绑定 → name / workType / batchCount
// ===========================================================================

#[tokio::test]
async fn bound_worker_exposes_name_work_type_and_nonzero_stats() {
    let (pool, app, token, fx) = bootstrap().await;
    let cid = insert_customer(&pool, "六厂").await;

    let wt = insert_work_type(&pool, "CNC", "CNC 车工").await;
    let w = insert_worker(&pool, "BOUND-1", "李润", Some(wt)).await;
    bind_user_worker(&pool, fx.manager_user_id, w).await;

    // 该工人本月 2 个批次各一条 PICKED_UP（qty 10 + 24.5 → 件数 34）
    let mut total_qty = 0i32;
    for _ in 0..2 {
        let part_id = insert_part(&pool, cid).await;
        let b = insert_batch(&pool, part_id, &BatchSpec::in_progress(4).with_holder(w)).await;
        insert_event(
            &pool,
            part_id,
            b,
            Some(w),
            "PICKED_UP",
            Some(17),
            IN_PERIOD_TS,
        )
        .await;
        total_qty += 17;
    }

    let env = home(&app, &token, &format!("tab=in_progress&period={PERIOD}")).await;
    assert_eq!(env["data"]["worker"]["name"], json!("李润"), "name: {env}");
    assert_eq!(
        env["data"]["worker"]["workType"],
        json!("CNC 车工"),
        "workType 来自 t_work_type.name（经 t_worker.work_type_id）: {env}"
    );
    assert_eq!(
        env["data"]["worker"]["avatar"],
        Value::Null,
        "avatar 恒 null（t_worker 无头像列）: {env}"
    );
    assert_eq!(
        env["data"]["stats"]["batchCount"],
        json!(2),
        "batchCount = 有事件的不同 batch_id 数: {env}"
    );
    assert_eq!(
        env["data"]["stats"]["workHours"],
        json!(f64::from(total_qty)),
        "workHours = PICKED_UP + RETURNED 的 SUM(quantity)（工作量估算）: {env}"
    );

    // ⚠️ 未分配工种 / 工种已软删 ⇒ workType 退化成**空串**（不是 null）。
    //    前端模板直接 {{worker.workType}} 渲染，空串渲染成空白、null 渲染成字面量。
    let w2 = insert_worker(&pool, "NO-WT", "无工种工人", None).await;
    bind_user_worker(&pool, fx.manager_user_id, w2).await;
    let env = home(&app, &token, &format!("tab=in_progress&period={PERIOD}")).await;
    assert_eq!(
        env["data"]["worker"]["workType"],
        json!(""),
        "work_type_id IS NULL ⇒ workType 是空串: {env}"
    );

    let wt_soft = insert_work_type(&pool, "OLD", "已软删工种").await;
    soft_delete_work_type(&pool, wt_soft).await;
    let w3 = insert_worker(&pool, "SOFT-WT", "软删工种工人", Some(wt_soft)).await;
    bind_user_worker(&pool, fx.manager_user_id, w3).await;
    let env = home(&app, &token, &format!("tab=in_progress&period={PERIOD}")).await;
    assert_eq!(
        env["data"]["worker"]["workType"],
        json!(""),
        "t_work_type 已软删 ⇒ workType 也是空串: {env}"
    );

    // ★ 反向：绑到**已软删的工人** ⇒ 收敛成「未绑定」（worker: null）
    sqlx::query("UPDATE t_worker SET deleted_at = $2::timestamp WHERE id = $1")
        .bind(w3)
        .bind(IN_PERIOD_TS)
        .execute(&pool)
        .await
        .expect("soft delete t_worker");
    let env = home(&app, &token, &format!("tab=in_progress&period={PERIOD}")).await;
    assert_eq!(
        env["data"]["worker"],
        Value::Null,
        "指向已软删工人的绑定按「未绑定」处理: {env}"
    );
}

// ===========================================================================
//  10. ★★ stats 真按工人过滤（本次 bug 修复的核心回归测试）
// ===========================================================================

/// **本次重构的核心**：旧 `GET /wx/worker/stats` 把 `CurrentUser.id`
/// （`t_user.id`）当 `t_part_event.worker_id`（`t_worker.id`）查，两表之间没有任何
/// 映射 ⇒ 对任何真实用户恒返 `batch_count: 0`。
///
/// 本用例造**两个**工人、各自有当月事件，且量刻意不同（A: 2 批 × 10 = 20 件；
/// B: 3 批 × 100 = 300 件），然后把登录账号绑到 A 上：
///
/// - 若仍按 `t_user.id` 查（旧 bug）⇒ 命中 0 条事件 ⇒ `batchCount == 0`
/// - 若忘了按 `worker_id` 过滤（查了全表）⇒ 命中 A + B ⇒ `batchCount == 5`
/// - 正确实现 ⇒ `batchCount == 2`、`workHours == 20.0`
#[tokio::test]
async fn stats_are_filtered_by_the_logged_in_users_worker() {
    let (pool, app, token, fx) = bootstrap().await;
    let cid = insert_customer(&pool, "六厂").await;
    let wt = insert_work_type(&pool, "CNC", "CNC 车工").await;

    let bound = insert_worker(&pool, "STAT-A", "甲工人", Some(wt)).await;
    let other = insert_worker(&pool, "STAT-B", "乙工人", Some(wt)).await;
    bind_user_worker(&pool, fx.manager_user_id, bound).await;

    // 甲（绑定的那位）：2 个不同批次，各一条 PICKED_UP qty 10 → batchCount 2 / 20 件
    for _ in 0..2 {
        let part_id = insert_part(&pool, cid).await;
        let b = insert_batch(
            &pool,
            part_id,
            &BatchSpec::in_progress(10).with_holder(bound),
        )
        .await;
        insert_event(
            &pool,
            part_id,
            b,
            Some(bound),
            "PICKED_UP",
            Some(10),
            IN_PERIOD_TS,
        )
        .await;
    }
    // 乙（未绑定的那位）：3 个不同批次，各一条 PICKED_UP qty 100 → batchCount 3 / 300 件
    for _ in 0..3 {
        let part_id = insert_part(&pool, cid).await;
        let b = insert_batch(
            &pool,
            part_id,
            &BatchSpec::in_progress(100).with_holder(other),
        )
        .await;
        insert_event(
            &pool,
            part_id,
            b,
            Some(other),
            "PICKED_UP",
            Some(100),
            IN_PERIOD_TS,
        )
        .await;
    }
    // 干扰：甲在上个月的事件（本 period 不该计入）
    let part_old = insert_part(&pool, cid).await;
    let b_old = insert_batch(
        &pool,
        part_old,
        &BatchSpec::in_progress(10).with_holder(bound),
    )
    .await;
    insert_event(
        &pool,
        part_old,
        b_old,
        Some(bound),
        "PICKED_UP",
        Some(999),
        OUT_PERIOD_TS,
    )
    .await;
    // 干扰：`quantity IS NULL` 的事件（不该进 workHours 求和，但进 batchCount）
    let part_null = insert_part(&pool, cid).await;
    let b_null = insert_batch(
        &pool,
        part_null,
        &BatchSpec::in_progress(10).with_holder(bound),
    )
    .await;
    insert_event(
        &pool,
        part_null,
        b_null,
        Some(bound),
        "PICKED_UP",
        None,
        IN_PERIOD_TS,
    )
    .await;

    let env = home(&app, &token, &format!("tab=in_progress&period={PERIOD}")).await;

    assert_eq!(
        env["data"]["worker"]["name"],
        json!("甲工人"),
        "worker 应是绑定的甲: {env}"
    );
    assert_eq!(
        env["data"]["stats"]["batchCount"],
        json!(3),
        "只统计**绑定工人**当月的 3 个不同 batch_id（2 个 qty=10 + 1 个 qty=NULL）；\
         旧实现恒返 0，漏过滤则返 5（甲 3 + 乙 2 上月不计入…）: {env}"
    );
    assert_eq!(
        env["data"]["stats"]["workHours"],
        json!(20.0),
        "workHours 只求和绑定工人的 PICKED_UP/RETURNED quantity（2×10；\
         上月的 999 与乙的 300 都不该进来）: {env}"
    );

    // ★ 对照组：把绑定切到乙，同一请求应只看到乙的量 ⇒ 证明过滤锚点是绑定关系，
    //   而不是「碰巧命中了某个固定 worker_id」
    bind_user_worker(&pool, fx.manager_user_id, other).await;
    let env2 = home(&app, &token, &format!("tab=in_progress&period={PERIOD}")).await;
    assert_eq!(env2["data"]["worker"]["name"], json!("乙工人"));
    assert_eq!(
        env2["data"]["stats"]["batchCount"],
        json!(3),
        "切到乙后应只统计乙的 3 个批次: {env2}"
    );
    assert_eq!(
        env2["data"]["stats"]["workHours"],
        json!(300.0),
        "切到乙后 workHours 应是 3×100: {env2}"
    );
}

// ===========================================================================
//  11. status 按 tab 折叠成 2 类
// ===========================================================================

#[tokio::test]
async fn status_is_folded_per_tab() {
    let (pool, app, token, _fx) = bootstrap().await;
    let cid = insert_customer(&pool, "六厂").await;

    // 进行中：IN_PROCESS + 一个「无 tab 状态」PENDING（兜底成 in_progress）
    let p_in = insert_part(&pool, cid).await;
    insert_batch(&pool, p_in, &BatchSpec::in_progress(1)).await;
    // 已完工：DELIVERED + COMPLETED 两种 DB 状态都必须折叠成 "done"
    let mut done_ids = Vec::new();
    for st in ["DELIVERED", "COMPLETED"] {
        let part_id = insert_part(&pool, cid).await;
        let b = insert_batch(&pool, part_id, &BatchSpec::done(st, 2)).await;
        insert_event(&pool, part_id, b, None, "DELIVERED", Some(1), IN_PERIOD_TS).await;
        done_ids.push(b);
    }

    let ip = page(&app, &token, "tab=in_progress&period=2026-10&size=50").await;
    for c in list_of(&ip) {
        assert_eq!(
            c["status"], "in_progress",
            "in_progress tab 的每张卡片 status 都必须是 in_progress: {c}"
        );
    }

    let done = page(&app, &token, "tab=done&period=2026-10&size=50").await;
    assert_eq!(
        list_of(&done).len(),
        2,
        "DELIVERED + COMPLETED 两行: {done}"
    );
    for c in list_of(&done) {
        assert_eq!(
            c["status"], "done",
            "done tab 的每张卡片 status 都必须是 done（DB 的 DELIVERED / COMPLETED \
             都折叠成 done）: {c}"
        );
    }
    for id in done_ids {
        let _ = card_by_id(list_of(&done), id);
    }
}

// ===========================================================================
//  12. assignedTo 跟随批次位置
// ===========================================================================

#[tokio::test]
async fn assigned_to_follows_batch_location() {
    let (pool, app, token, _fx) = bootstrap().await;
    let cid = insert_customer(&pool, "六厂").await;
    let wt = insert_work_type(&pool, "CNC", "CNC 车工").await;
    let w = insert_worker(&pool, "HOLDER-1", "持批工人", Some(wt)).await;

    // 在工人手里 ⇒ assignedTo = t_worker.name
    let p1 = insert_part(&pool, cid).await;
    let in_hand = insert_batch(&pool, p1, &BatchSpec::in_progress(3).with_holder(w)).await;
    // 在货架上（holder 仍指向该工人）⇒ assignedTo = null
    let p2 = insert_part(&pool, cid).await;
    let on_shelf = insert_batch(
        &pool,
        p2,
        &BatchSpec::in_progress(4)
            .with_holder(w)
            .at("PRODUCTION_SHELF"),
    )
    .await;

    let env = page(&app, &token, "tab=in_progress&period=2026-10&size=50").await;
    let list = list_of(&env);

    assert_eq!(
        card_by_id(list, in_hand)["assignedTo"],
        json!("持批工人"),
        "location='WORKER' ⇒ assignedTo 是持有人名: {env}"
    );
    assert_eq!(
        card_by_id(list, on_shelf)["assignedTo"],
        Value::Null,
        "批次在货架上（location≠'WORKER'）⇒ assignedTo 必须是 null（ON 上的 location \
         闸门，不能挪进 WHERE —— 那样整行会被过滤掉）: {env}"
    );
}

// ===========================================================================
//  13. workHours / finishedDate 无值时是 null（前端 != null 守门依赖）
// ===========================================================================

#[tokio::test]
async fn missing_work_hours_and_finished_date_are_null() {
    let (pool, app, token, _fx) = bootstrap().await;
    let cid = insert_customer(&pool, "六厂").await;

    // 进行中批次：零事件 ⇒ workHours 必须是 null（旧 SQL 的 COALESCE 会给 0，
    // 那会让前端把「没干过活」渲染成「干了 0 小时」）
    let p1 = insert_part(&pool, cid).await;
    let bare = insert_batch(&pool, p1, &BatchSpec::in_progress(3)).await;

    // 对照组：有 PICKED_UP 事件 ⇒ workHours 是真实数值
    let p2 = insert_part(&pool, cid).await;
    let with_hours = insert_batch(&pool, p2, &BatchSpec::in_progress(5)).await;
    insert_event(
        &pool,
        p2,
        with_hours,
        None,
        "PICKED_UP",
        Some(6),
        IN_PERIOD_TS,
    )
    .await;

    // 已完工批次：finishedDate 来自 DELIVERED 事件日；对照组无事件 ⇒ null
    let p3 = insert_part(&pool, cid).await;
    let done_with = insert_batch(&pool, p3, &BatchSpec::done("DELIVERED", 7)).await;
    insert_event(
        &pool,
        p3,
        done_with,
        None,
        "DELIVERED",
        Some(2),
        IN_PERIOD_TS,
    )
    .await;

    let ip = page(&app, &token, "tab=in_progress&period=2026-10&size=50").await;
    let list = list_of(&ip);

    let c = card_by_id(list, bare);
    assert_eq!(
        c["workHours"],
        Value::Null,
        "★ 零事件的批次 workHours 必须是 null 而不是 0（前端 wx:if 守门）: {c}"
    );
    assert_eq!(
        c["finishedDate"],
        Value::Null,
        "无 DELIVERED 事件 ⇒ finishedDate 是 null（不是空串）: {c}"
    );
    assert_eq!(c["workHours"].as_f64(), None, "不能是 0.0: {c}");

    let c = card_by_id(list, with_hours);
    assert_eq!(c["workHours"], json!(6.0), "有事件 ⇒ 真值: {c}");
    assert_eq!(c["finishedDate"], Value::Null, "进行中批次无完成日: {c}");

    let done = page(&app, &token, "tab=done&period=2026-10&size=50").await;
    let c = card_by_id(list_of(&done), done_with);
    assert_eq!(
        c["finishedDate"],
        json!("2026-10-05"),
        "finishedDate 取 DELIVERED 事件日、格式 YYYY-MM-DD: {c}"
    );

    // 卡片字段名逐字核对（camelCase + 禁列）
    let c = card_by_id(list_of(&ip), bare);
    let obj = c.as_object().unwrap();
    let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        vec![
            "assignedTo",
            "batchNo",
            "batchQty",
            "code",
            "dueDate",
            "finishedDate",
            "id",
            "name",
            "serialNo",
            "status",
            "workHours",
        ],
        "卡片恰好 11 个键（part_id / drawingUrl 已删）: {c}"
    );
    assert_eq!(c["serialNo"], Value::Null, "serialNo 无值时是 null: {c}");
    assert_eq!(
        c["batchNo"],
        json!(1),
        "batchNo 是数字（前端自己补零）: {c}"
    );
    assert_eq!(
        c["batchQty"],
        json!(3),
        "batchQty 是 t_part_batch.quantity: {c}"
    );
}

// ===========================================================================
//  14. ★ 尾斜杠形态钉死
// ===========================================================================

/// 2026-10-11 **实测**：本仓 axum 版本下 `nest("/production")` + 内层
/// `route("/")` **只匹配无尾斜杠**的 `/wx/production`；带尾斜杠的
/// `/wx/production/` 落到 axum 默认 fallback ⇒ **HTTP 404 空 body**。
///
/// ⚠️ 小程序侧旧代码发的恰恰是带尾斜杠的 `/wx/batches/`（见
/// `wx-app/miniprogram/services/production.ts` 的 2026-09-28 注释），切 URL 时
/// 顺手去掉尾斜杠。本用例把四种形态全钉死，防 axum 升级 / nest 改写后行为漂移。
#[tokio::test]
async fn trailing_slash_form_is_pinned() {
    let (pool, app, token, _fx) = bootstrap().await;
    let cid = insert_customer(&pool, "六厂").await;
    let part_id = insert_part(&pool, cid).await;
    insert_batch(&pool, part_id, &BatchSpec::in_progress(5)).await;

    // 无尾斜杠 → 200（业务形态）
    let env = get_ok(&app, &format!("{HOME_URI}?tab=in_progress"), &token).await;
    assert_eq!(
        list_of(&env).len(),
        1,
        "无尾斜杠形态必须命中 handler: {env}"
    );

    // 带尾斜杠 → 实测 404 空 body（用 send_raw 拿裸 body：404 走 axum fallback，
    // body 是空的、不走 R<T> 信封，send 会 panic）
    let (status, raw) = send_raw(
        app.clone(),
        json_request(
            "GET",
            &format!("{HOME_URI}/?tab=in_progress"),
            None,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "带尾斜杠形态实测是 404（实测结论登记在 docs/api/wx.md §5.1）：raw={raw:?}"
    );
    assert!(
        raw.is_empty(),
        "404 是 axum 默认 fallback（空 body），不是 R<T> 信封：raw={raw:?}"
    );

    // `/page` 的两个形态（无尾斜杠命中、带尾斜杠 404）
    get_ok(&app, &format!("{PAGE_URI}?tab=in_progress"), &token).await;
    let (status, _) = send_raw(
        app,
        json_request(
            "GET",
            &format!("{PAGE_URI}/?tab=in_progress"),
            None,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "/page/ 带尾斜杠同样 404");
}

// ===========================================================================
//  15. 旧路径全 404（硬切无 alias）
// ===========================================================================

#[tokio::test]
async fn legacy_batches_and_worker_paths_are_all_gone() {
    let (_pool, app, token, _fx) = bootstrap().await;

    for uri in [
        "/wx/batches/counts?period=2026-10",
        "/wx/batches?tab=in_progress",
        "/wx/batches/",
        "/wx/batches/?tab=in_progress&period=2026-10&page=1&size=10",
        "/wx/worker/stats?period=2026-10",
        "/wx/worker",
        "/wx/worker/",
        // B2 步已经删掉的（防整域 nest 误挂回来）
        "/wx/parts/counts",
        "/wx/parts/",
        "/wx/dashboard/home",
        "/wx/iam/wx-login",
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
//  16. part_list / login 端点不受影响（B2 交付物回归）
// ===========================================================================

/// B3 只动了 `/production` 前缀，`/part-list*` 与 `/login/wecom` 必须逐字不变。
/// 完整覆盖在 `tests/wx/part_list.rs`（13 条）与 `tests/wecom_login.rs`（15 条），
/// 这里各留一条冒烟。
#[tokio::test]
async fn part_list_and_login_endpoints_still_work() {
    let (pool, app, token, _fx) = bootstrap().await;
    let cid = insert_customer(&pool, "六厂").await;

    // 造一行零件，顺带证明两个域读的是同一份数据
    let part_id = next_id();
    sqlx::query(
        "INSERT INTO t_part (id, serial_no, name, drawing_no, applicant_name, quantity, \
         request_date, planned_delivery_date, status, is_urgent, customer_id, version, \
         created_at, updated_at) \
         VALUES ($1, 'FX-SMOKE', '冒烟件', 'SMOKE-1', 'T', 3, '2026-10-01', '2026-10-20', \
                 'PENDING', false, $2, 0, $3::timestamp, $3::timestamp)",
    )
    .bind(part_id)
    .bind(cid)
    .bind(IN_PERIOD_TS)
    .execute(&pool)
    .await
    .expect("insert t_part");

    // part-list 首屏 + 增量
    let env = get_ok(&app, "/wx/part-list?page=1&size=50", &token).await;
    assert!(env["data"]["counts"].is_object(), "part-list 首屏: {env}");
    let ids: Vec<String> = list_of(&env)
        .iter()
        .map(|c| c["id"].as_str().unwrap().to_string())
        .collect();
    assert!(
        ids.contains(&part_id.to_string()),
        "part-list 应能看到刚插的零件: {env}"
    );

    let p = get_ok(&app, "/wx/part-list/page?page=1&size=50", &token).await;
    assert!(p["data"]["list"].is_array(), "part-list/page: {p}");

    // login 是公开端点：无 token 时不该是 401（40021 = 缺 code，属校验失败）
    let (status, env) = send(
        app,
        json_request(
            "POST",
            "/wx/login/wecom",
            Some(json!({ "code": "whatever" })),
            None,
        ),
    )
    .await;
    assert_ne!(status, StatusCode::UNAUTHORIZED, "login 是公开端点: {env}");
}

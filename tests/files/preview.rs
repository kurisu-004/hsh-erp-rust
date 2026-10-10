//! `files::part_file::preview` 集成测试（2026-10-13 PR2 新建）
//!
//! 被测端点：`GET /api/v2/files/objects/{file_id}/preview`
//! （COS CI 图纸预览，惰性轮询、零后台任务）。
//!
//! ## 覆盖场景（12 条，与任务书逐条对应）
//!
//! | # | 用例 | 断言要点 |
//! |---|---|---|
//! | 1 | `preview_seed_uses_content_sha256_prefix` | 有 sha 时取**前 32 位 hex** |
//! | 2 | `preview_seed_falls_back_to_object_key_hash` | sha 为 NULL 时退回 `sha256(object_key)` 前 32 位 |
//! | 3 | `ready_cache_hit_never_calls_python` | READY 命中 → **mock 零调用** |
//! | 4 | `cache_miss_submits_once_and_returns_pending` | 未命中 → 提交 1 次 → PENDING |
//! | 5 | `pending_not_due_returns_pending_without_python` | 未到期 → 直接 PENDING（**不打 python**） |
//! | 6 | `pending_due_describe_success_marks_ready` | 到期 + Success → READY + `pages.len() == total_page_count` + URL 逐条正确 |
//! | 7 | `describe_failed_returns_21117` | Failed → 21117 / HTTP 502 |
//! | 8 | `submit_forward_failure_returns_21118` | 提交失败 → 21118 / HTTP 502 |
//! | 9 | `non_pdf_and_non_drawing_kind_return_21116` | 3D_MODEL / 非 PDF → 21116 / HTTP 400 |
//! | 10 | `concurrent_misses_submit_only_one_job` | 并发两次 → **只提交 1 个 job**（唯一索引闸门） |
//! | 11 | `failed_state_restarts_on_next_call` | FAILED → 下次调用自愈重启为 PENDING |
//! | 12 | `shelf_account_can_get_preview` | SHELF_ACCOUNT 能拿到 preview（锁选项 A 的角色矩阵） |
//! | 13 | `describe_not_exist_returns_21117_and_row_failed` | python 合成态 `NotExist` ⇒ 21117 + 行落 FAILED，**且本次调用零 submit** |
//! | 14 | `describe_not_exist_lowercase_also_recognized` | `notexist` 小写变体同判（大小写不敏感） |
//! | 15 | `describe_cancel_returns_21117` | CI 官方 `Cancel` ⇒ 21117 + 行落 FAILED |
//! | 16 | `describe_pause_returns_21117` | CI 官方 `Pause` ⇒ 21117 + 行落 FAILED |
//! | 17 | `describe_success_with_zero_pages_stays_pending` | **回归护栏**：`Success` 但 0 页仍当 Running（不落 READY） |
//! | 18 | `describe_reads_state_key_as_well_as_status` | 状态字段名 `state` / `status` 两种写法都认（跨仓兜底） |
//!
//! 另有 3 条护栏（不编号）：
//! - `preview_route_is_not_swallowed_by_sibling_catch_alls` —— **matchit 冲突的实测**
//!   （见下节）
//! - `preview_urls_shape_is_stable` —— `preview_urls` 纯函数断言
//! - `parse_ci_status_word_table_is_exhaustive` —— **跨仓 CI 终态词表的表驱动断言**
//!   （review 第 1 轮新增；含 `NotExist` / `Pause` / `Cancel` 与大小写别名）
//!
//! ## matchit 冲突的验证方式（本文件最重要的一条护栏）
//!
//! `CLAUDE.md` 路由声明规约第 9 条的老坑是「同段数静态段被 `Path<i64>` catch-all
//! 吞成 **400** 而不是 404」。`/objects/{file_id}/preview` 与既有的
//! `/objects/{file_id}/url` **段数相同、第 1 段都是捕获段**，只靠读源码无法排除
//! 「静态段被同级 catch-all 吞掉」。
//!
//! 故 `preview_route_is_not_swallowed_by_sibling_catch_alls` 做**双向对照**：
//! 同一个 `file_id` 分别打 `/preview` 与 `/url`，**两条都必须命中各自的 handler**
//! （`/preview` 返信封、`/url` 返信封），任一条变成 400 / 404 即红。
//!
//! ## 测试栈
//!
//! - HTTP 用例走 `test_app` + `login_token`（真实 JWT + Redis session，与生产同源）
//! - `MockPyBackendClient`（mockall automock）替换 `state.py_backend`，
//!   `.times(n)` / `.never()` 直接断言「打了几次 python」
//! - ⚠️ **时钟依赖**：`next_poll_at` 由 `now_naive()` 推导，故「未到期」用例必须
//!   在**同一次调用内**断言（首次登记返回的 PENDING 天然是未到期的）；
//!   「到期」用例则由 helper 把 `next_poll_at` **写死到过去**再触发，
//!   不硬编码绝对日期（`CLAUDE.md`「测试数据两条硬约定」第 2 条）。

use std::sync::Arc;

use axum::body::Bytes;
use axum::http::{HeaderMap, StatusCode};
use hsh_erp_rust::auth::rbac::{CurrentUser, Role};
use hsh_erp_rust::infra::clock::now_naive;
use hsh_erp_rust::infra::cos::NoopCos;
use hsh_erp_rust::infra::py_backend::{MockPyBackendClient, PyBackendClient, PyBackendResponse};
use hsh_erp_rust::modules::files::part_file::model::TPartFile;
use hsh_erp_rust::modules::files::part_file::preview::dto::PreviewOut;
use hsh_erp_rust::modules::files::part_file::preview::service::PreviewService;
use hsh_erp_rust::modules::files::part_file::service::PartFileService;
use hsh_erp_rust::state::AppState;
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;

use hsh_erp_test_support::{
    PartFixture, load_part_fixture, login_token, send, shared_test_snowflake, test_app, test_pool,
    test_state,
};

/// test-support `test_state` 里的 `cos.preview_base_url`（见 `test-support/src/state.rs`）。
const PREVIEW_BASE_URL: &str = "https://test-bucket.cos.ap-shanghai.myqcloud.com";
/// test-support `test_state` 里的 `cos.preview_prefix`。
const PREVIEW_PREFIX: &str = "previews";
/// 与 `part_file.rs` 同款：测试自造的 cfg 上传前缀。
const UPLOAD_UPLOAD_PREFIX: &str = "uploads";
const UPLOAD_MAX_FILE_SIZE: usize = 300 * 1024 * 1024;

/// 一份最小可预览 PDF 的字节（内容只要求「非空 + sha256 可算」，本端点不解析 PDF）。
const PDF_BYTES: &[u8] = b"%PDF-1.5\npreview fixture\n%%EOF";

// ===========================================================================
//  Helpers
// ===========================================================================

/// 造一个带指定角色的 `CurrentUser`（本域白名单是**纯角色判定**，不读货架 scope）。
fn test_user(roles: Vec<Role>) -> CurrentUser {
    CurrentUser {
        id: 1,
        username: "tester".into(),
        roles,
        shelf_ids: vec![],
        shelf_wildcard: false,
    }
}

/// 造一条 `t_part_file` 行（直接 INSERT，避开上传路径的 10 项守卫 ——
/// 本文件测的是预览子模块，不是上传校验）。
///
/// ⚠️ `part_id` 取**每次调用一个全新雪花**：该列是 polymorphic 且无物理外键，
/// 而 `uk_t_part_file_single (part_id, kind)` 要求同一 (owner, kind) 至多一行 ——
/// 用固定 part_id 时，同一测试里插第 2 行就会撞唯一索引（23505）。
async fn insert_part_file(
    pool: &PgPool,
    kind: &str,
    file_type: &str,
    filename: &str,
    content_sha256: Option<&str>,
) -> i64 {
    let id = shared_test_snowflake().next_id();
    let part_id = shared_test_snowflake().next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_part_file \
           (id, part_id, kind, file_type, object_key, original_filename, file_size, \
            content_type, upload_status, content_sha256, version, \
            created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, 'application/pdf', 'READY', $8, 0, $9, 1, $9, 1)",
    )
    .bind(id)
    .bind(part_id) // polymorphic owner，无物理外键；每次新值以避开 uk_t_part_file_single
    .bind(kind)
    .bind(file_type)
    .bind(format!("0123456789abcdef_{filename}"))
    .bind(filename)
    .bind(PDF_BYTES.len() as i64)
    .bind(content_sha256)
    .bind(now)
    .execute(pool)
    .await
    .expect("插入 t_part_file 行");
    id
}

/// 造 python 侧的 CI 响应信封。
fn ci_ok(data: Value) -> PyBackendResponse {
    let body = json!({"code": 0, "message": "ok", "data": data});
    let mut headers = HeaderMap::new();
    headers.insert("content-type", "application/json".parse().unwrap());
    PyBackendResponse {
        status: StatusCode::OK,
        headers,
        body: Bytes::from(serde_json::to_vec(&body).unwrap()),
    }
}

/// 把 `t_file_preview` 里指定行的 `next_poll_at` 写死到**过去**（模拟「已到期」）。
///
/// ⚠️ 用 `now_naive() - 1 分钟` 而非绝对日期：符合 `CLAUDE.md`「测试数据两条硬
/// 约定」第 2 条（不得硬编码绝对日期窗口）。
async fn force_poll_due(pool: &PgPool, seed: &str) {
    let past = now_naive() - chrono::Duration::minutes(1);
    sqlx::query(
        "UPDATE t_file_preview SET next_poll_at = $2 WHERE preview_seed = $1 AND deleted_at IS NULL",
    )
    .bind(seed)
    .bind(past)
    .execute(pool)
    .await
    .expect("把 next_poll_at 写死到过去");
}

/// 读回 `t_file_preview` 某 seed 的当前状态（供断言用）。
async fn read_state(pool: &PgPool, seed: &str) -> Option<(String, Option<String>, i32)> {
    let row: Option<(String, Option<String>, i32)> = sqlx::query_as(
        "SELECT state, ci_job_id, page_count FROM t_file_preview \
         WHERE preview_seed = $1 AND deleted_at IS NULL",
    )
    .bind(seed)
    .fetch_optional(pool)
    .await
    .expect("读回 t_file_preview");
    row
}

/// 把 `MockPyBackendClient` 装进 `state` 并造 app。
///
/// ⚠️ `Arc::get_mut` 要求 `state` 的强计数为 1 —— 故**必须**在 clone 之前替换
/// （`test_app` 会再 clone 一份），与 `tests/files/sts.rs` 同款纪律。
fn app_with_mock(state: &mut Arc<AppState>, mock: MockPyBackendClient) -> axum::Router {
    let py_backend: Arc<dyn PyBackendClient> = Arc::new(mock);
    let inner = Arc::get_mut(state).expect("state Arc 必须 unique");
    inner.py_backend = py_backend;
    test_app(state.clone())
}

// ===========================================================================
//  1 + 2：seed 推导的两条路径
// ===========================================================================

/// 场景 1：有 `content_sha256` ⇒ 取**前 32 位 hex**。
#[tokio::test]
async fn preview_seed_uses_content_sha256_prefix() {
    let pool = test_pool().await;
    let sha = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";
    let id = insert_part_file(&pool, "DRAWING", "PDF", "A00123_rev2.pdf", Some(sha)).await;
    let file = fetch_row(&pool, id).await;
    assert_eq!(
        PreviewService::preview_seed(&file),
        "abcdef0123456789abcdef0123456789"
    );
    assert_eq!(
        PreviewService::preview_seed(&file).len(),
        32,
        "种子必须是 32 位 hex（sha256 前 16 字节）"
    );
}

/// 场景 2：`content_sha256 IS NULL`（历史行）⇒ 退回 `sha256(object_key)` 前 32 位。
#[tokio::test]
async fn preview_seed_falls_back_to_object_key_hash() {
    let pool = test_pool().await;
    let id = insert_part_file(&pool, "DRAWING", "PDF", "B99999.pdf", None).await;
    let file = fetch_row(&pool, id).await;
    let seed = PreviewService::preview_seed(&file);
    assert_eq!(seed.len(), 32, "退回分支同样必须产出 32 位 hex");
    assert!(
        seed.chars().all(|c| c.is_ascii_hexdigit()),
        "种子必须是 hex：{seed}"
    );
    // ⚠️ 安全断言：退回分支也**不能**把文件名漏进种子（`previews/` 是 public-read，
    // 文件名=图号、可枚举）。这是本 PR 最该被钉死的一条安全不变量。
    assert!(
        !seed.contains("B99999"),
        "种子绝不能含原文件名（图号可枚举）：{seed}"
    );
    // 另一份同内容、不同文件名的行 ⇒ 种子不同（该分支非内容寻址，见已知偏差 #3）
    let id2 = insert_part_file(&pool, "DRAWING", "PDF", "C88888.pdf", None).await;
    let file2 = fetch_row(&pool, id2).await;
    assert_ne!(
        PreviewService::preview_seed(&file2),
        seed,
        "退回分支按 object_key 哈希 ⇒ 不同文件名 → 不同种子（已知偏差 #3）"
    );
}

/// 读回一行 `t_part_file`（走 `PartFileRepoTrait`，与生产读路径同一条 SQL）。
async fn fetch_row(pool: &PgPool, id: i64) -> TPartFile {
    use hsh_erp_rust::modules::files::part_file::repo::PartFileRepoTrait;
    let mut conn = pool.acquire().await.unwrap();
    let mut r: &mut sqlx::PgConnection = &mut conn;
    r.get_by_id(id, false)
        .await
        .unwrap_or_else(|e| panic!("读 t_part_file {id} 失败: {e}"))
        .unwrap_or_else(|| panic!("t_part_file {id} 应存在"))
}

// ===========================================================================
//  3：READY cache hit → 不打 python
// ===========================================================================

/// 场景 3：库里已有 READY 行 ⇒ 直接返 `pages`，**一次都不打 python**。
#[tokio::test]
async fn ready_cache_hit_never_calls_python() {
    let pool = test_pool().await;
    let file_id =
        insert_part_file(&pool, "DRAWING", "PDF", "ready.pdf", Some(&"a".repeat(64))).await;
    let file = fetch_row(&pool, file_id).await;
    let seed = PreviewService::preview_seed(&file);

    // 直接塞一条 READY 行（模拟上一次调用已收敛）
    sqlx::query(
        "INSERT INTO t_file_preview \
           (id, preview_seed, src_object_key, ci_job_id, state, page_count, \
            created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, $3, 'job-done', 'READY', 2, now(), 1, now(), 1)",
    )
    .bind(shared_test_snowflake().next_id())
    .bind(&seed)
    .bind(&file.object_key)
    .execute(&pool)
    .await
    .unwrap();

    // mock 两个方法都 `.never()`
    let mut mock = MockPyBackendClient::new();
    mock.expect_forward_ci_submit().never();
    mock.expect_forward_ci_describe().never();

    let mut state = test_state(pool.clone()).await;
    // ⚠️ 本条刻意不登录：走 service 直调，验的是「READY 命中不打 python」这一行为，
    // 角色判定由场景 12 与 HTTP 用例单独覆盖。
    let _app = app_with_mock(&mut state, mock);

    let out = call_preview(&pool, &state, &file).await;
    assert_eq!(out.state, "READY");
    assert_eq!(out.page_count, 2);
    assert_eq!(
        out.pages.len(),
        2,
        "READY 时 pages.len() 必须 == page_count"
    );
    assert_eq!(
        out.pages[0],
        format!("{PREVIEW_BASE_URL}/{PREVIEW_PREFIX}/{seed}/p1.jpg")
    );
}

/// 通过 service 直调预览（本文件多数用例走这条，省掉 JWT / Redis 开销）。
async fn call_preview(pool: &PgPool, state: &Arc<AppState>, file: &TPartFile) -> PreviewOut {
    PreviewService::new(shared_test_snowflake().clone())
        .get_preview(
            pool,
            &state.py_backend,
            PREVIEW_PREFIX,
            PREVIEW_BASE_URL,
            file,
            &test_user(vec![Role::Manager]),
        )
        .await
        .expect("预览调用应成功")
}

// ===========================================================================
//  4：cache miss → 提交一次 → PENDING
// ===========================================================================

/// 场景 4：未命中 ⇒ 提交**恰好一次** ⇒ PENDING。
#[tokio::test]
async fn cache_miss_submits_once_and_returns_pending() {
    let pool = test_pool().await;
    let file_id =
        insert_part_file(&pool, "DRAWING", "PDF", "miss.pdf", Some(&"b".repeat(64))).await;
    let file = fetch_row(&pool, file_id).await;
    let seed = PreviewService::preview_seed(&file);

    let mut mock = MockPyBackendClient::new();
    mock.expect_forward_ci_submit()
        .times(1)
        .returning(|_body, _headers| Ok(ci_ok(json!({"job_id": "job-001"}))));
    mock.expect_forward_ci_describe().never();

    let mut state = test_state(pool.clone()).await;
    let _app = app_with_mock(&mut state, mock);

    let out = call_preview(&pool, &state, &file).await;
    assert_eq!(out.state, "PENDING");
    assert_eq!(out.page_count, 0);
    assert!(out.pages.is_empty(), "PENDING 时不得有页 URL");
    assert!(out.retry_after_ms > 0, "PENDING 必须给前端节流值");

    let (state_s, job_id, pages) = read_state(&pool, &seed).await.expect("行应存在");
    assert_eq!(state_s, "PENDING");
    assert_eq!(job_id.as_deref(), Some("job-001"), "job id 必须落库");
    assert_eq!(pages, 0);
}

// ===========================================================================
//  5：PENDING 未到期 → 直接 PENDING，不打 python
// ===========================================================================

/// 场景 5：行已在 PENDING 且**未到期** ⇒ 直接返 PENDING，**不打 python**。
#[tokio::test]
async fn pending_not_due_returns_pending_without_python() {
    let pool = test_pool().await;
    let file_id = insert_part_file(
        &pool,
        "DRAWING",
        "PDF",
        "throttle.pdf",
        Some(&"c".repeat(64)),
    )
    .await;
    let file = fetch_row(&pool, file_id).await;
    let seed = PreviewService::preview_seed(&file);

    // 直接塞一条 PENDING 行，`next_poll_at` = **未来**（未到期）
    let future = now_naive() + chrono::Duration::minutes(5);
    sqlx::query(
        "INSERT INTO t_file_preview \
           (id, preview_seed, src_object_key, ci_job_id, state, page_count, next_poll_at, \
            created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, $3, 'job-002', 'PENDING', 0, $4, now(), 1, now(), 1)",
    )
    .bind(shared_test_snowflake().next_id())
    .bind(&seed)
    .bind(&file.object_key)
    .bind(future)
    .execute(&pool)
    .await
    .unwrap();

    // ★ 两个方法都 `.never()`：未到期时**任何**打 python 的行为都该让本条红
    let mut mock = MockPyBackendClient::new();
    mock.expect_forward_ci_submit().never();
    mock.expect_forward_ci_describe().never();

    let mut state = test_state(pool.clone()).await;
    let _app = app_with_mock(&mut state, mock);

    let out = call_preview(&pool, &state, &file).await;
    assert_eq!(out.state, "PENDING");
    assert!(out.retry_after_ms > 0);
}

// ===========================================================================
//  6：PENDING 到期 + describe Success → READY
// ===========================================================================

/// 场景 6：到期 + `Success` ⇒ READY，`pages.len() == total_page_count`，URL 逐条正确。
#[tokio::test]
async fn pending_due_describe_success_marks_ready() {
    let pool = test_pool().await;
    let file_id = insert_part_file(
        &pool,
        "ASSEMBLY_MASTER",
        "PDF",
        "ok.pdf",
        Some(&"d".repeat(64)),
    )
    .await;
    let file = fetch_row(&pool, file_id).await;
    let seed = PreviewService::preview_seed(&file);

    let mut mock = MockPyBackendClient::new();
    // 第 1 次调用是新 seed ⇒ 登记 + 提交一次；第 2 次（到期后）才 describe。
    mock.expect_forward_ci_submit()
        .times(1)
        .returning(|_body, _headers| Ok(ci_ok(json!({"job_id": "job-003"}))));
    mock.expect_forward_ci_describe()
        .times(1)
        .withf(|job_id, _h| job_id == "job-003")
        .returning(|_job_id, _headers| {
            Ok(ci_ok(json!({"status": "Success", "total_page_count": 3})))
        });

    let mut state = test_state(pool.clone()).await;
    let _app = app_with_mock(&mut state, mock);

    // 第 1 次：未到期 ⇒ PENDING（登记）
    let out1 = call_preview(&pool, &state, &file).await;
    assert_eq!(out1.state, "PENDING");

    // 把 next_poll_at 写死到过去 ⇒ 第 2 次到期
    force_poll_due(&pool, &seed).await;
    let out2 = call_preview(&pool, &state, &file).await;
    assert_eq!(out2.state, "READY");
    assert_eq!(out2.page_count, 3);
    assert_eq!(out2.pages.len(), 3, "pages.len() 必须 == page_count");
    for (i, url) in out2.pages.iter().enumerate() {
        assert_eq!(
            url,
            &format!("{PREVIEW_BASE_URL}/{PREVIEW_PREFIX}/{seed}/p{}.jpg", i + 1),
            "第 {} 页 URL 应为 .../p{}.jpg",
            i + 1,
            i + 1
        );
    }
    assert_eq!(out2.retry_after_ms, 0, "READY 不该再让前端重试");

    let (state_s, _job, pages) = read_state(&pool, &seed).await.unwrap();
    assert_eq!(state_s, "READY");
    assert_eq!(pages, 3);
}

// ===========================================================================
//  7：describe Failed → 21117
// ===========================================================================

/// 场景 7：CI 终态 `Failed` ⇒ 21117 / HTTP 502。
#[tokio::test]
async fn describe_failed_returns_21117() {
    let pool = test_pool().await;
    let file_id = insert_part_file(&pool, "DRAWING", "PDF", "bad.pdf", Some(&"e".repeat(64))).await;
    let file = fetch_row(&pool, file_id).await;
    let seed = PreviewService::preview_seed(&file);

    let mut mock = MockPyBackendClient::new();
    // 第 1 次调用是新 seed ⇒ 登记 + 提交一次（拿到 job id 才能 describe）
    mock.expect_forward_ci_submit()
        .times(1)
        .returning(|_body, _headers| Ok(ci_ok(json!({"job_id": "job-fail"}))));
    mock.expect_forward_ci_describe()
        .times(1)
        .withf(|job_id, _h| job_id == "job-fail")
        .returning(|_job_id, _headers| {
            Ok(ci_ok(json!({"status": "Failed", "message": "CI 渲染超时"})))
        });

    let mut state = test_state(pool.clone()).await;
    let _app = app_with_mock(&mut state, mock);

    // 先登记（未到期返 PENDING）
    let _ = call_preview(&pool, &state, &file).await;
    force_poll_due(&pool, &seed).await;

    let err = PreviewService::new(shared_test_snowflake().clone())
        .get_preview(
            &pool,
            &state.py_backend,
            PREVIEW_PREFIX,
            PREVIEW_BASE_URL,
            &file,
            &test_user(vec![Role::Manager]),
        )
        .await
        .expect_err("CI Failed 必须报错");
    assert_eq!(err.code(), 21117, "应返 21117 BIZ_PART_FILE_PREVIEW_FAILED");
    assert_eq!(
        err.http_status(),
        StatusCode::BAD_GATEWAY,
        "CI 终态失败是上游问题 ⇒ 502"
    );

    let (state_s, _, _) = read_state(&pool, &seed).await.unwrap();
    assert_eq!(state_s, "FAILED");
}

// ===========================================================================
//  8：提交失败 → 21118
// ===========================================================================

/// 场景 8：提交转发失败（网络层）⇒ 21118 / HTTP 502，且行**保持 PENDING**（自愈）。
#[tokio::test]
async fn submit_forward_failure_returns_21118() {
    let pool = test_pool().await;
    let file_id = insert_part_file(&pool, "DRAWING", "PDF", "fwd.pdf", Some(&"f".repeat(64))).await;
    let file = fetch_row(&pool, file_id).await;
    let seed = PreviewService::preview_seed(&file);

    let mut mock = MockPyBackendClient::new();
    mock.expect_forward_ci_submit()
        .times(1)
        .returning(|_b, _h| {
            Err(hsh_erp_rust::shared::error::AppError::biz(
                hsh_erp_rust::shared::error::code::BIZ_PART_FILE_PREVIEW_FORWARD_FAILED,
                "mock: 打 python 失败",
            ))
        });
    mock.expect_forward_ci_describe().never();

    let mut state = test_state(pool.clone()).await;
    let _app = app_with_mock(&mut state, mock);

    let err = PreviewService::new(shared_test_snowflake().clone())
        .get_preview(
            &pool,
            &state.py_backend,
            PREVIEW_PREFIX,
            PREVIEW_BASE_URL,
            &file,
            &test_user(vec![Role::Manager]),
        )
        .await
        .expect_err("提交失败必须报错");
    assert_eq!(err.code(), 21118);
    assert_eq!(err.http_status(), StatusCode::BAD_GATEWAY);

    let (state_s, job_id, _) = read_state(&pool, &seed).await.unwrap();
    assert_eq!(
        state_s, "PENDING",
        "「提交失败」不是 CI 终态，行必须保持 PENDING 以便自愈"
    );
    assert_eq!(job_id, None, "未拿到 job id");
}

// ===========================================================================
//  9：不可预览 → 21116
// ===========================================================================

/// 场景 9：非图纸 kind / 非 PDF ⇒ 21116 / HTTP 400，**不打 python**。
#[tokio::test]
async fn non_pdf_and_non_drawing_kind_return_21116() {
    let pool = test_pool().await;
    let three_d =
        insert_part_file(&pool, "3D_MODEL", "STEP", "x.step", Some(&"1".repeat(64))).await;
    let gcode = insert_part_file(&pool, "G_CODE", "TAP", "x.tap", Some(&"2".repeat(64))).await;
    let not_pdf = insert_part_file(&pool, "DRAWING", "DXF", "x.dxf", Some(&"3".repeat(64))).await;

    let mut mock = MockPyBackendClient::new();
    mock.expect_forward_ci_submit().never();
    mock.expect_forward_ci_describe().never();

    let mut state = test_state(pool.clone()).await;
    let _app = app_with_mock(&mut state, mock);

    for (label, id) in [
        ("3D_MODEL", three_d),
        ("G_CODE", gcode),
        ("非 PDF", not_pdf),
    ] {
        let file = fetch_row(&pool, id).await;
        let err = PreviewService::new(shared_test_snowflake().clone())
            .get_preview(
                &pool,
                &state.py_backend,
                PREVIEW_PREFIX,
                PREVIEW_BASE_URL,
                &file,
                &test_user(vec![Role::Manager]),
            )
            .await
            .err()
            .unwrap_or_else(|| panic!("{label} 应被判为不可预览，却成功了"));
        assert_eq!(err.code(), 21116, "{label} 应返 21116");
        assert_eq!(
            err.http_status(),
            StatusCode::BAD_REQUEST,
            "{label} 应是 400"
        );
    }
}

// ===========================================================================
//  10：并发两次只提交一个 job（唯一索引闸门）
// ===========================================================================

/// 场景 10：两个请求同时 cache miss ⇒ **只提交 1 个 CI job**。
///
/// 机制：`insert_pending` 的 `ON CONFLICT DO NOTHING` 撞 `uk_t_file_preview_seed`，
/// `rows_affected == 0` 的那个直接返 PENDING，不提交。CI 按页计费 ⇒ 这是省钱
/// 护栏，不是性能优化。
#[tokio::test]
async fn concurrent_misses_submit_only_one_job() {
    let pool = test_pool().await;
    let file_id =
        insert_part_file(&pool, "DRAWING", "PDF", "race.pdf", Some(&"4".repeat(64))).await;
    let file = fetch_row(&pool, file_id).await;

    // `.times(1)` 是本条的**全部**断言：并发下若提交了 2 次，mockall 会直接 panic。
    let mut mock = MockPyBackendClient::new();
    mock.expect_forward_ci_submit()
        .times(1)
        .returning(|_b, _h| Ok(ci_ok(json!({"job_id": "job-race"}))));
    mock.expect_forward_ci_describe().never();

    let mut state = test_state(pool.clone()).await;
    let _app = app_with_mock(&mut state, mock);
    let svc = PreviewService::new(shared_test_snowflake().clone());
    let user = test_user(vec![Role::Manager]);

    let (a, b) = tokio::join!(
        svc.get_preview(
            &pool,
            &state.py_backend,
            PREVIEW_PREFIX,
            PREVIEW_BASE_URL,
            &file,
            &user
        ),
        svc.get_preview(
            &pool,
            &state.py_backend,
            PREVIEW_PREFIX,
            PREVIEW_BASE_URL,
            &file,
            &user
        ),
    );
    // 两次都返 PENDING（本端点的正常中间态）
    assert_eq!(a.expect("并发 A").state, "PENDING");
    assert_eq!(b.expect("并发 B").state, "PENDING");

    // 且库里恒只有**一行**（唯一索引）
    let cnt: (i64,) =
        sqlx::query_as("SELECT count(*) FROM t_file_preview WHERE deleted_at IS NULL")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(cnt.0, 1, "同一 seed 必须只有一行（唯一索引闸门）");
}

// ===========================================================================
//  11：FAILED 自愈
// ===========================================================================

/// 场景 11：`FAILED` ⇒ 下次调用**自愈重启**（不再返 21117）。
#[tokio::test]
async fn failed_state_restarts_on_next_call() {
    let pool = test_pool().await;
    let file_id =
        insert_part_file(&pool, "DRAWING", "PDF", "heal.pdf", Some(&"5".repeat(64))).await;
    let file = fetch_row(&pool, file_id).await;
    let seed = PreviewService::preview_seed(&file);

    // 直接塞一条 FAILED 行
    sqlx::query(
        "INSERT INTO t_file_preview \
           (id, preview_seed, src_object_key, ci_job_id, state, page_count, error_message, \
            created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, $3, 'job-old', 'FAILED', 0, 'CI 挂了', now(), 1, now(), 1)",
    )
    .bind(shared_test_snowflake().next_id())
    .bind(&seed)
    .bind(&file.object_key)
    .execute(&pool)
    .await
    .unwrap();

    let mut mock = MockPyBackendClient::new();
    // 自愈后立刻重新提交一次（新 job id）
    mock.expect_forward_ci_submit()
        .times(1)
        .returning(|_b, _h| Ok(ci_ok(json!({"job_id": "job-healed"}))));
    mock.expect_forward_ci_describe().never();

    let mut state = test_state(pool.clone()).await;
    let _app = app_with_mock(&mut state, mock);

    let out = call_preview(&pool, &state, &file).await;
    assert_eq!(
        out.state, "PENDING",
        "FAILED 必须自愈重启为 PENDING，不能永远卡在失败态"
    );
    let (state_s, job_id, pages) = read_state(&pool, &seed).await.unwrap();
    assert_eq!(state_s, "PENDING");
    assert_eq!(
        job_id.as_deref(),
        Some("job-healed"),
        "自愈必须拿到新 job id（旧的已被清掉）"
    );
    assert_eq!(pages, 0);
}

// ===========================================================================
//  12：SHELF_ACCOUNT 能拿到 preview（锁选项 A 的角色矩阵）
// ===========================================================================

/// 场景 12：`SHELF_ACCOUNT` 调本端点**不被拒**（选项 A 的角色矩阵守卫）。
///
/// ⚠️ 与 `part_file.rs::rbac_shelf_account_get_file_url_still_forbidden` 方向
/// **相反**：那条守「`/url` 预签直链不给 SHELF」，本条守「预览端点给 SHELF」。
/// 两条并存是有意的安全取舍反转，取舍全文见
/// `src/modules/files/part_file/preview/mod.rs` 的「安全取舍反转」一节。
#[tokio::test]
async fn shelf_account_can_get_preview() {
    let pool = test_pool().await;
    let _fx = load_part_fixture(&pool).await;
    let file_id =
        insert_part_file(&pool, "DRAWING", "PDF", "shelf.pdf", Some(&"6".repeat(64))).await;
    let file = fetch_row(&pool, file_id).await;
    let seed = PreviewService::preview_seed(&file);

    let mut mock = MockPyBackendClient::new();
    mock.expect_forward_ci_submit().never();
    mock.expect_forward_ci_describe().never();

    let mut state = test_state(pool.clone()).await;
    let _app = app_with_mock(&mut state, mock);

    sqlx::query(
        "INSERT INTO t_file_preview \
           (id, preview_seed, src_object_key, ci_job_id, state, page_count, \
            created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, $3, 'job-shelf', 'READY', 1, now(), 1, now(), 1)",
    )
    .bind(shared_test_snowflake().next_id())
    .bind(&seed)
    .bind(&file.object_key)
    .execute(&pool)
    .await
    .unwrap();

    let out = PreviewService::new(shared_test_snowflake().clone())
        .get_preview(
            &pool,
            &state.py_backend,
            PREVIEW_PREFIX,
            PREVIEW_BASE_URL,
            &file,
            &test_user(vec![Role::ShelfAccount]),
        )
        .await
        .expect("SHELF_ACCOUNT 必须能拿到预览（选项 A）");
    assert_eq!(out.state, "READY");
    assert_eq!(out.pages.len(), 1);
}

// ===========================================================================
//  13-18：跨仓 CI 终态词表（2026-10-13 review 第 1 轮）
// ===========================================================================

/// 跑一遍完整的「登记 → 提交 → 强制到期 → describe」，返回**第二次**调用的结果。
///
/// ⚠️ `expect_forward_ci_submit().times(1)` 是本 helper 的**隐含断言**：两次调用
/// 合起来只允许提交 **1 个** CI job。若被测代码在终局失败时「静默重提」，这里
/// 会直接 panic —— 这正是 review 第 1 轮要求钉死的「不得变成无限提交 job」。
///
/// `describe_data` 原样作为 python `describe` 的 `data`，由各用例自己决定
/// `status` / `total_page_count` / `message` 三者的组合。
async fn describe_once(
    pool: &PgPool,
    file: &TPartFile,
    job_id: &str,
    describe_data: Value,
) -> Result<PreviewOut, hsh_erp_rust::shared::error::AppError> {
    // mockall 的 `returning` 闭包要求 `'static` ⇒ 参数收进 owned 值再 move 进去。
    let job_id = job_id.to_string();
    let submit_job_id = job_id.clone();
    let mut mock = MockPyBackendClient::new();
    mock.expect_forward_ci_submit()
        .times(1)
        .returning(move |_b, _h| Ok(ci_ok(json!({ "job_id": submit_job_id }))));
    mock.expect_forward_ci_describe()
        .times(1)
        .withf(move |jid, _h| jid == job_id)
        .returning(move |_jid, _h| Ok(ci_ok(describe_data.clone())));

    let mut state = test_state(pool.clone()).await;
    let _app = app_with_mock(&mut state, mock);
    let svc = PreviewService::new(shared_test_snowflake().clone());
    let user = test_user(vec![Role::Manager]);

    // 第 1 次：新 seed ⇒ 登记 + 提交 ⇒ PENDING
    svc.get_preview(
        pool,
        &state.py_backend,
        PREVIEW_PREFIX,
        PREVIEW_BASE_URL,
        file,
        &user,
    )
    .await?;

    // 把 `next_poll_at` 写死到过去 ⇒ 第 2 次到期（不硬编码绝对日期）
    force_poll_due(pool, &PreviewService::preview_seed(file)).await;

    // 第 2 次：到期 ⇒ describe
    svc.get_preview(
        pool,
        &state.py_backend,
        PREVIEW_PREFIX,
        PREVIEW_BASE_URL,
        file,
        &user,
    )
    .await
}

/// 读回 `t_file_preview` 某 seed 的 `error_message`（供终局失败原因断言）。
async fn read_error_message(pool: &PgPool, seed: &str) -> Option<String> {
    sqlx::query_scalar::<_, Option<String>>(
        "SELECT error_message FROM t_file_preview \
         WHERE preview_seed = $1 AND deleted_at IS NULL",
    )
    .bind(seed)
    .fetch_optional(pool)
    .await
    .expect("读回 error_message")
    .flatten()
}

/// 场景 13：python 合成的 `NotExist` ⇒ **21117 + 行落 FAILED**（不静默重提 job）。
///
/// 决策与论证见 `src/modules/files/part_file/preview/mod.rs` 的
/// 「CI 终局失败的落点」一节；此处只钉行为。
#[tokio::test]
async fn describe_not_exist_returns_21117_and_row_failed() {
    let pool = test_pool().await;
    let file_id = insert_part_file(
        &pool,
        "DRAWING",
        "PDF",
        "notexist.pdf",
        Some(&"8".repeat(64)),
    )
    .await;
    let file = fetch_row(&pool, file_id).await;
    let seed = PreviewService::preview_seed(&file);

    let err = describe_once(
        &pool,
        &file,
        "job-notexist",
        json!({"status": "NotExist", "total_page_count": 0}),
    )
    .await
    .expect_err("NotExist 是确定性终局，必须报错而不是继续返 PENDING（无限轮询的老坑）");

    assert_eq!(err.code(), 21117, "NotExist 应返 21117");
    assert_eq!(
        err.http_status(),
        StatusCode::BAD_GATEWAY,
        "CI 侧查不到 job 是上游问题 ⇒ 502"
    );

    let (state_s, _job, _pages) = read_state(&pool, &seed).await.unwrap();
    assert_eq!(
        state_s, "FAILED",
        "NotExist 必须落 FAILED（下次调用经 reset_to_pending 自愈重提）"
    );
    let msg = read_error_message(&pool, &seed)
        .await
        .expect("FAILED 行必须有原因");
    assert!(
        msg.contains("不存在"),
        "error_message 应说明是「任务记录不存在」，实际：{msg}"
    );
}

/// 场景 14：`notexist` 小写变体同样识别（大小写不敏感）。
#[tokio::test]
async fn describe_not_exist_lowercase_also_recognized() {
    let pool = test_pool().await;
    let file_id = insert_part_file(
        &pool,
        "DRAWING",
        "PDF",
        "notexist-lc.pdf",
        Some(&"9".repeat(64)),
    )
    .await;
    let file = fetch_row(&pool, file_id).await;

    let err = describe_once(
        &pool,
        &file,
        "job-notexist-lc",
        json!({"status": "notexist", "total_page_count": 0}),
    )
    .await
    .expect_err("小写 notexist 同样必须识别为终局");
    assert_eq!(err.code(), 21117);
}

/// 场景 15：CI 官方终态 `Cancel` ⇒ 21117 + 行落 FAILED。
#[tokio::test]
async fn describe_cancel_returns_21117() {
    let pool = test_pool().await;
    let file_id = insert_part_file(
        &pool,
        "DRAWING",
        "PDF",
        "cancel.pdf",
        Some(&"a1".repeat(32)),
    )
    .await;
    let file = fetch_row(&pool, file_id).await;
    let seed = PreviewService::preview_seed(&file);

    let err = describe_once(
        &pool,
        &file,
        "job-cancel",
        json!({"status": "Cancel", "total_page_count": 0}),
    )
    .await
    .expect_err("Cancel 是 CI 确定性终态，必须报错而不是永远转圈");
    assert_eq!(err.code(), 21117);
    assert_eq!(err.http_status(), StatusCode::BAD_GATEWAY);

    let (state_s, _, _) = read_state(&pool, &seed).await.unwrap();
    assert_eq!(state_s, "FAILED");
    let msg = read_error_message(&pool, &seed)
        .await
        .expect("FAILED 行必须有原因");
    assert!(
        msg.contains("取消"),
        "error_message 应说明被取消，实际：{msg}"
    );
}

/// 场景 16：CI 官方终态 `Pause`（队列暂停）⇒ 21117 + 行落 FAILED。
///
/// ⚠️ 与 `Cancel` 分开断言，因为两者的**理由不同**：`Cancel` 无论如何都不会再变；
/// `Pause` 是「CI 理论上可 Resume、但本系统两侧都不会去 Resume」，判终局是有意
/// 取舍（论证见模块 doc「CI 终局失败的落点」一节 + 已知偏差 #5）。
#[tokio::test]
async fn describe_pause_returns_21117() {
    let pool = test_pool().await;
    let file_id =
        insert_part_file(&pool, "DRAWING", "PDF", "pause.pdf", Some(&"b1".repeat(32))).await;
    let file = fetch_row(&pool, file_id).await;
    let seed = PreviewService::preview_seed(&file);

    let err = describe_once(
        &pool,
        &file,
        "job-pause",
        json!({"status": "Pause", "total_page_count": 0}),
    )
    .await
    .expect_err("Pause 在本系统等同终局，必须报错而不是永远转圈");
    assert_eq!(err.code(), 21117);

    let (state_s, _, _) = read_state(&pool, &seed).await.unwrap();
    assert_eq!(state_s, "FAILED");
    let msg = read_error_message(&pool, &seed)
        .await
        .expect("FAILED 行必须有原因");
    assert!(
        msg.contains("暂停"),
        "error_message 应说明被暂停，实际：{msg}"
    );
}

/// 场景 17（**回归护栏**）：`Success` 但 `total_page_count == 0` ⇒ **仍当进行中**。
///
/// ⚠️ 这是 implementor 特意做的判断，**本轮不得破坏**：CI 的 Success 快照可能
/// 先于对象落盘，此刻若判 Success，`mark_ready(0)` 会返回**空 pages 的 READY**，
/// 且该行已是终态、永远不会自愈。
#[tokio::test]
async fn describe_success_with_zero_pages_stays_pending() {
    let pool = test_pool().await;
    let file_id = insert_part_file(
        &pool,
        "DRAWING",
        "PDF",
        "zeropage.pdf",
        Some(&"c1".repeat(32)),
    )
    .await;
    let file = fetch_row(&pool, file_id).await;
    let seed = PreviewService::preview_seed(&file);

    let out = describe_once(
        &pool,
        &file,
        "job-zeropage",
        json!({"status": "Success", "total_page_count": 0}),
    )
    .await
    .expect("0 页的 Success 不该报错");
    assert_eq!(
        out.state, "PENDING",
        "Success 但 0 页必须继续轮询，绝不能落成空 pages 的 READY"
    );
    assert!(out.pages.is_empty());
    assert!(out.retry_after_ms > 0, "仍是进行中 ⇒ 必须给前端节流值");

    let (state_s, _, pages) = read_state(&pool, &seed).await.unwrap();
    assert_eq!(state_s, "PENDING", "行必须留在 PENDING 等下一轮");
    assert_eq!(pages, 0);
}

/// 场景 18：状态字段名写 `state`（而非 `status`）时同样识别。
///
/// ⚠️ 跨仓兜底：`forward_ci_describe` 的契约 doc 读的是 `status`，python 侧文档
/// 称 `state`，两侧未收敛。**键名读错的后果是空串 → 落 `Running` 兜底 → 无限
/// 轮询**，所以 rust 侧两个键都读（`status` 优先）。登记见模块 doc 已知偏差 #8。
#[tokio::test]
async fn describe_reads_state_key_as_well_as_status() {
    let pool = test_pool().await;
    let file_id = insert_part_file(
        &pool,
        "DRAWING",
        "PDF",
        "statekey.pdf",
        Some(&"d1".repeat(32)),
    )
    .await;
    let file = fetch_row(&pool, file_id).await;

    let err = describe_once(
        &pool,
        &file,
        "job-statekey",
        json!({"state": "NotExist", "total_page_count": 0}),
    )
    .await
    .expect_err("键名 `state` 也必须能读到状态词");
    assert_eq!(err.code(), 21117);
}

// ===========================================================================
//  护栏 A：matchit 冲突的实测
// ===========================================================================

/// **`/preview` 没有被同级的 `Path<i64>` catch-all 吞掉**（matchit 冲突验证）。
///
/// 双向对照：同一个 `file_id` 打 `/preview` 与 `/url`，两条都必须命中各自 handler。
/// 若 `/preview` 被 `{file_id}` 捕获段抢走，会落到 `/objects/{id}/url` 那类路径或
/// 直接 400 —— 任一种都让本条红。
#[tokio::test]
async fn preview_route_is_not_swallowed_by_sibling_catch_alls() {
    let pool = test_pool().await;
    let fx = load_part_fixture(&pool).await;
    let file_id =
        insert_part_file(&pool, "DRAWING", "PDF", "route.pdf", Some(&"7".repeat(64))).await;
    let file = fetch_row(&pool, file_id).await;
    let seed = PreviewService::preview_seed(&file);

    // 预置 READY，让 `/preview` 零 IO 返回 200（断言只看路由是否命中）
    sqlx::query(
        "INSERT INTO t_file_preview \
           (id, preview_seed, src_object_key, ci_job_id, state, page_count, \
            created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, $3, 'job-route', 'READY', 1, now(), 1, now(), 1)",
    )
    .bind(shared_test_snowflake().next_id())
    .bind(&seed)
    .bind(&file.object_key)
    .execute(&pool)
    .await
    .unwrap();

    let state = test_state(pool.clone()).await;
    let app = test_app(state.clone());
    let token = login_token(&app, &fx.manager_username, PartFixture::PASSWORD).await;

    // ① `/preview` 必须命中预览 handler（200 + 信封里有 state）。
    //    ⚠️ 用 `oneshot` 直驱并**自行解析** body，而不是 `send`：`send` 会对非 JSON
    //    body 直接 panic（而「路由没命中」正是本条要区分的那种情形，panic 会把
    //    404/400 伪装成测试栈错误、看不出真实状态码）。
    let (st_preview, body_raw) = raw_get(&app, &token, file_id, "preview").await;
    let body: Value = serde_json::from_slice(&body_raw).unwrap_or(Value::Null);
    assert_eq!(
        st_preview,
        StatusCode::OK,
        "/preview 被吞掉或未命中：status={st_preview} body={}",
        String::from_utf8_lossy(&body_raw)
    );
    assert_eq!(body["data"]["state"], "READY");

    // ② 同级的 `/url` 仍必须命中它自己的 handler（反向对照：证明不是「整段都 404」）
    let (st_url, _) = raw_get(&app, &token, file_id, "url").await;
    assert_ne!(
        st_url,
        StatusCode::BAD_REQUEST,
        "`/url` 被 `/preview` 的静态段挤掉（matchit 冲突）"
    );

    // ③ `/content`（另一个同级 catch-all 兄弟）也仍命中自己
    let (st_content, _) = raw_get(&app, &token, file_id, "content").await;
    assert_ne!(
        st_content,
        StatusCode::BAD_REQUEST,
        "`/content` 被挤掉（matchit 冲突）"
    );
}

/// 对 `/files/objects/{file_id}/{tail}` 发一次裸 GET，返回 `(status, 原始 body)`。
///
/// ⚠️ **URL 不带 `/api/v2` 前缀**：`test_app` 返回的是 `modules::v2_router(state)`
/// 本体，`/api/v2` 这一层 nest 是 `main.rs` 挂的、测试栈里没有（与
/// `tests/files/part_file.rs` 的硬切守卫同款）。写错前缀会得到 axum 的空 body 404，
/// 看起来极像「路由被 matchit 吞了」——排查时先确认前缀。
async fn raw_get(
    app: &axum::Router,
    token: &str,
    file_id: i64,
    tail: &str,
) -> (StatusCode, Vec<u8>) {
    let req = hsh_erp_test_support::json_request(
        "GET",
        &format!("/files/objects/{file_id}/{tail}"),
        None,
        Some(token),
    );
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let body = axum::body::to_bytes(resp.into_body(), 4 * 1024 * 1024)
        .await
        .unwrap()
        .to_vec();
    (status, body)
}

// ===========================================================================
//  护栏 B：preview_urls 纯函数
// ===========================================================================

/// `preview_urls` 的形状：页码 **1-based**、站点根的尾斜杠被吃掉、`n<=0` 返空。
#[test]
fn preview_urls_shape_is_stable() {
    let urls = PreviewService::preview_urls("https://cdn.example.com/", "previews", "abc123", 3);
    assert_eq!(
        urls,
        vec![
            "https://cdn.example.com/previews/abc123/p1.jpg".to_string(),
            "https://cdn.example.com/previews/abc123/p2.jpg".to_string(),
            "https://cdn.example.com/previews/abc123/p3.jpg".to_string(),
        ],
        "页码必须 1-based、站点根尾斜杠不能造成 //"
    );
    assert!(
        PreviewService::preview_urls("https://cdn.example.com", "previews", "abc123", 0).is_empty(),
        "0 页不得产出占位 URL"
    );
    assert!(
        PreviewService::preview_urls("https://cdn.example.com", "previews", "abc123", -1)
            .is_empty(),
        "负页数不得产出 URL"
    );
}

/// **跨仓 CI 终态词表**的表驱动断言（2026-10-13 review 第 1 轮）。
///
/// ⚠️ 本条是「python 改了 `state` 取值 / CI 改了官方词」时的**第一道响**：
/// 新词若不在表里，多半会落进最后一行的 `Running` 兜底 ⇒ 回到无限轮询的老坑。
/// 完整跨仓契约（哪个词是谁合成的、谁负责识别）见
/// `src/modules/files/part_file/preview/mod.rs` 的「跨仓 CI 终态词表」一节。
#[test]
fn parse_ci_status_word_table_is_exhaustive() {
    use hsh_erp_rust::modules::files::part_file::preview::service::CiStatus;

    /// `(原始词, total_page_count, 期望归一)`
    const CASES: &[(&str, i64, CiStatus)] = &[
        // ── CI 官方成功。⚠️ **0 页仍当进行中是回归护栏，勿改**
        ("Success", 3, CiStatus::Success),
        ("success", 3, CiStatus::Success),
        ("SUCCESS", 1, CiStatus::Success),
        ("  Success  ", 2, CiStatus::Success),
        ("Success", 0, CiStatus::Running),
        ("success", 0, CiStatus::Running),
        // ── CI 官方失败
        ("Failed", 0, CiStatus::Failed),
        ("failure", 0, CiStatus::Failed),
        ("FAILED", 0, CiStatus::Failed),
        // ── CI 官方终态：被取消（review 第 1 轮新增，此前落兜底 ⇒ 无限轮询）
        ("Cancel", 0, CiStatus::Canceled),
        ("canceled", 0, CiStatus::Canceled),
        ("CANCELLED", 0, CiStatus::Canceled),
        // ── CI 官方终态：队列暂停（同上）
        ("Pause", 0, CiStatus::Paused),
        ("paused", 0, CiStatus::Paused),
        // ── ⚠️ **python 自己合成**的 NotExist（CI 官方六态里没有），大小写 / 别名都要认
        ("NotExist", 0, CiStatus::NotExist),
        ("notexist", 0, CiStatus::NotExist),
        ("NOTEXIST", 0, CiStatus::NotExist),
        ("  notexist  ", 0, CiStatus::NotExist),
        ("Not_Exist", 0, CiStatus::NotExist),
        ("NonExist", 0, CiStatus::NotExist),
        // ── 进行中 + 兜底（⚠️ 兜底不认识跨仓新增的词，这是本表的维护理由）
        ("Submitted", 0, CiStatus::Running),
        ("Running", 0, CiStatus::Running),
        ("Processing", 0, CiStatus::Running),
        ("Pending", 0, CiStatus::Running),
        ("", 0, CiStatus::Running),
        ("   ", 0, CiStatus::Running),
        ("WhoKnowsThis", 0, CiStatus::Running),
    ];

    for (word, pages, want) in CASES {
        assert_eq!(
            PreviewService::parse_ci_status(word, *pages),
            *want,
            "原始词 {word:?}（{pages} 页）应归一为 {want:?}"
        );
    }

    // ⚠️ 一条反向断言：`Success` **带页数**时绝不能落 `Running`（否则预览永远
    // 收敛不了，只是上一条护栏的另一面）。
    assert_eq!(
        PreviewService::parse_ci_status("Success", 1),
        CiStatus::Success,
        "Success + 1 页必须收敛成 Success，不能被 0 页护栏误伤"
    );
}

//! `print_template` 域集成测试（**2026-10-16 新建 binary**，与新域同批）
//!
//! 本 binary 与 `src/modules/print_template/` 同批落地，覆盖 5 个端点的
//! HTTP 契约 + `t_part_file` 复用层的落库形态 + 角色 / 权限守卫 + 硬切 404。
//!
//! ## ⚠️ 入口**必须是 `main.rs`**
//!
//! cargo 1.98.1 **不识别** `tests/<dir>/mod.rs`，只识别 `tests/<dir>/main.rs`
//! （binary 名 = `<dir>`，即 `print_template`）。
//!
//! ## 拆分映射（3 个 sub-file）
//! - `upload.rs`  —— 端点 1：multipart 上传 / 替换语义 / 四道轻校验 / 403 守卫
//! - `resolve.rs` —— 端点 3 / 4：21109 未绑定 / 内容字节 / 权限点守卫 / 旧路径 404
//! - `delete.rs`  —— 端点 5：OCC（缺 version ⇒ 422 纯文本 / 陈旧 ⇒ 40901）+ 端点 2 列表
//!
//! ## 前置条件
//! - PG：`scripts/test_nextest.sh` 自带 session 容器与 per-test fresh database。
//! - **Redis**：全部走 `/iam/login` 拿 token ⇒ 本 binary 需要 Redis
//!   （同全仓其它 HTTP 契约测试同款前提）。跨 worktree 用
//!   `TEST_REDIS_URL=redis://localhost:<port>/0` 覆盖。
//!
//! ## COS 替身：`test-support` 的 `MockCos`（2026-10-16 review 第 1 轮 MINOR-6 改）
//!
//! 本 binary 早先自带一个 `MemoryCos`（`Mutex<HashMap>`），理由是
//! `NoopCos.get_object` 恒返空字节、而当时的 `MockCos.put_object` **不保存**
//! 字节 —— 两者都钉不到「`GET /{id}/content` 返回的字节与上传的字节逐字节相等」
//! 这条 ★ 核心回归。那是**重复造轮子**（仓内第 3 个 COS 替身），现已把能力上提到
//! `MockCos`（`put` 真存字节 + `head_object` 报真实 size、`delete` 真删字节），
//! 本 binary 直接复用它。
//!
//! ⚠️ **替身掩盖的两条路径仍然零覆盖**（登记在
//! `docs/api/print_template.md` §8.3）：① 真实 COS 的**错误分类**（替身对缺失
//! key 返 `BIZ_PART_FILE_UPLOAD_FAILED`，真 COS 是 `NoSuchKey`）；② **etag 语义**
//! （替身是拼出来的字符串）；③「CAS 命中跳过 COS PUT」那条省流路径本 binary 也
//! 完全没跑（替换语义下取不到，见该文档 §8.3 第 4 条）。

#![allow(dead_code)]

mod delete;
mod resolve;
mod upload;

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, Response, StatusCode, header};
use serde_json::{Value, json};
use sqlx::PgPool;

use hsh_erp_rust::infra::clock::now_naive;
use hsh_erp_test_support::{
    MockCos, PartFixture, load_part_fixture, login_token, shared_test_snowflake, test_app,
    test_pool, test_state_with_cos,
};

// ===========================================================================
//  测试上下文
// ===========================================================================

/// 一个用例的公共上下文：fresh database + fixture + 内存 COS + 四个角色的 token。
///
/// ⚠️ 四个 token 是本域的**硬需求**而非奢侈：本域的守卫分两档
/// （Manager-only 三条 / 权限点 `delivery_note.print` 两条），
/// 少一个 token 就钉不到「权限点放 INSPECTOR、但不放 SHELF_ACCOUNT」这条。
pub(crate) struct Ctx {
    pub pool: PgPool,
    pub app: axum::Router,
    /// 内存 COS 替身（断言「删除后 spawn 确实删了对象」用 `stored_bytes`）。
    pub cos: Arc<MockCos>,
    pub manager_token: String,
    pub clerk_token: String,
    pub inspector_token: String,
    pub shelf_token: String,
    /// 一个未软删的**一级客户**（`parent_id IS NULL`）。
    pub l1_id: i64,
    /// 另一个未软删的一级客户（列表 / resolve 的对照组）。
    pub other_l1_id: i64,
    /// `l1_id` 名下的一个**二级客户**（绑二级 ⇒ 20104）。
    pub l2_id: i64,
}

pub(crate) async fn bootstrap() -> Ctx {
    let pool = test_pool().await;
    let fx = load_part_fixture(&pool).await;
    let cos = Arc::new(MockCos::new());
    let state = test_state_with_cos(pool.clone(), cos.clone()).await;
    let app = test_app(state);

    let manager_token = login_token(&app, &fx.manager_username, PartFixture::PASSWORD).await;
    let clerk_token = login_token(&app, &fx.clerk_username, PartFixture::PASSWORD).await;
    let inspector_token = login_token(&app, &fx.inspector_username, PartFixture::PASSWORD).await;
    let shelf_token = login_token(&app, &fx.shelf_account_username, PartFixture::PASSWORD).await;

    let l1_id = insert_l1_customer(&pool, "模板测试甲公司", 'A').await;
    let other_l1_id = insert_l1_customer(&pool, "模板测试乙公司", 'B').await;
    let l2_id = insert_l2_customer(&pool, "模板测试甲公司-二级", l1_id).await;

    Ctx {
        pool,
        app,
        cos,
        manager_token,
        clerk_token,
        inspector_token,
        shelf_token,
        l1_id,
        other_l1_id,
        l2_id,
    }
}

// ===========================================================================
//  最小合法 zip 字节（xlsx 就是 zip）
// ===========================================================================

/// 一份**最小合法**的 xlsx 形状字节：zip 本地文件头 `PK\x03\x04` + 版本/标志/压缩
/// 字段全 0 + 文件名长度 0 + 压缩后长度 0 + 本地头长度 0 + 扩展字段长度 0，
/// 最后补一条中央目录结束记录（EOCD）`PK\x05\x06` + 16 个 0 字节。
///
/// 后端**只校验前 4 字节**（`dto::has_zip_magic`），所以最小形态其实只要
/// `PK\x03\x04` 就够；这里给到 EOCD 是为了让这份字节「万一将来后端加结构校验」
/// 时不至于立刻变成非法 zip。⚠️ 它**不是**一个能被 SheetJS 打开的工作簿 ——
/// 那正是「后端刻意不引 zip crate」这条取舍的已知代价（见
/// `docs/api/print_template.md` §8.3 第 3 条）。
pub(crate) fn minimal_xlsx_bytes(tag: &str) -> Vec<u8> {
    let mut v: Vec<u8> = Vec::new();
    // 本地文件头 30 字节：签名 4 + 版本 2 + 标志 2 + 压缩 2 + 时间 2 + 日期 2
    //                    + crc 4 + 压缩长度 4 + 原长度 4 + 文件名长度 2
    //                    + 扩展长度 2
    v.extend_from_slice(&[0x50, 0x4B, 0x03, 0x04]);
    v.extend_from_slice(&[0x14, 0x00]); // version needed = 20
    v.extend_from_slice(&[0x00, 0x00]); // flags
    v.extend_from_slice(&[0x00, 0x00]); // compression = stored
    v.extend_from_slice(&[0x00, 0x00]); // mod time
    v.extend_from_slice(&[0x00, 0x00]); // mod date
    v.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]); // crc32
    v.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]); // compressed size
    v.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]); // original size
    v.extend_from_slice(&[0x00, 0x00]); // filename length
    v.extend_from_slice(&[0x00, 0x00]); // extra field length
    // EOCD 22 字节：签名 4 + 磁盘号 2 + 起始磁盘 2 + 本盘条目 2 + 总条目 2
    //                 + 目录大小 4 + 目录偏移 4 + 注释长度 2
    v.extend_from_slice(&[0x50, 0x4B, 0x05, 0x06]);
    v.extend_from_slice(&[0x00; 16]);
    // 尾部的 tag 只是让不同用例的字节**不同**（⇒ sha 不同 ⇒ CAS key 不同）
    v.extend_from_slice(tag.as_bytes());
    v
}

// ===========================================================================
//  multipart 请求构造
// ===========================================================================

/// multipart 边界（wire 上不可见，仅用于分隔 part）。
pub(crate) const MP_BOUNDARY: &str = "hshPrintTemplateBoundary3xYq";

/// 手工拼一个 `multipart/form-data` body。
///
/// `file` 的四元组 = `(字段名, filename, content_type, 字节)`；`None` 即**不发**
/// 该字段（用来测「缺 file」分支）。⚠️ `filename` 也不能省 —— handler 用
/// `field.file_name()` 的 `None` 判「缺文件名」，那是另一条 20104 分支。
/// 照抄 `tests/files/upload.rs` 的同名 helper（请求构造不是 fixture）。
pub(crate) fn multipart_body(
    data: Option<&str>,
    file: Option<(&str, &str, &str, &[u8])>,
) -> Vec<u8> {
    let mut body: Vec<u8> = Vec::new();
    if let Some(d) = data {
        body.extend_from_slice(format!("--{MP_BOUNDARY}\r\n").as_bytes());
        body.extend_from_slice(b"Content-Disposition: form-data; name=\"data\"\r\n\r\n");
        body.extend_from_slice(d.as_bytes());
        body.extend_from_slice(b"\r\n");
    }
    if let Some((field, filename, content_type, bytes)) = file {
        body.extend_from_slice(format!("--{MP_BOUNDARY}\r\n").as_bytes());
        body.extend_from_slice(
            format!(
                "Content-Disposition: form-data; name=\"{field}\"; filename=\"{filename}\"\r\n"
            )
            .as_bytes(),
        );
        body.extend_from_slice(format!("Content-Type: {content_type}\r\n\r\n").as_bytes());
        body.extend_from_slice(bytes);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{MP_BOUNDARY}--\r\n").as_bytes());
    body
}

/// 构造一个 multipart 请求（`data` + `file`），带 Bearer。
///
/// ⚠️ **uri 固定是 `/print-templates`**（本域端点 1）。要打别的 multipart 端点
/// （典型是 `POST /files/objects` 那个**通用上传原语**，见 review 第 1 轮 MAJOR-4）
/// 必须显式调 [`multipart_request_to`] —— 2026-10-16 首版这条 helper 把 uri 写死、
/// 又被「CLERK 绕行」用例复用，于是那条用例其实打的是 `/print-templates`（被本域
/// 自己的 `require_role(Manager)` 挡下），**恒真绿**、什么也没钉到。
pub(crate) fn multipart_request(
    data: Option<&str>,
    file: Option<(&str, &str, &str, &[u8])>,
    bearer: Option<&str>,
) -> Request<Body> {
    multipart_request_to("/print-templates", data, file, bearer)
}

/// 同 [`multipart_request`]，但**显式给 uri**。
pub(crate) fn multipart_request_to(
    uri: &str,
    data: Option<&str>,
    file: Option<(&str, &str, &str, &[u8])>,
    bearer: Option<&str>,
) -> Request<Body> {
    let body = multipart_body(data, file);
    let mut builder = Request::builder().method("POST").uri(uri).header(
        header::CONTENT_TYPE,
        format!("multipart/form-data; boundary={MP_BOUNDARY}"),
    );
    if let Some(t) = bearer {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {t}"));
    }
    builder
        .body(Body::from(body))
        .expect("multipart_request_to: build request")
}

// ===========================================================================
//  请求 helper（业务端点）
// ===========================================================================

/// `POST /print-templates`（带完整 multipart）→ `(status, 信封)`。
pub(crate) async fn upload(
    ctx: &Ctx,
    customer_id: i64,
    filename: &str,
    bytes: &[u8],
    bearer: &str,
) -> (StatusCode, Value) {
    let data = json!({ "customer_id": customer_id.to_string() }).to_string();
    send_multipart(
        ctx,
        multipart_request(
            Some(&data),
            Some((
                "file",
                filename,
                "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
                bytes,
            )),
            Some(bearer),
        ),
    )
    .await
}

/// 打一个 multipart 请求并解析信封。
pub(crate) async fn send_multipart(ctx: &Ctx, req: Request<Body>) -> (StatusCode, Value) {
    use tower::ServiceExt;
    let resp: Response<Body> = ctx.app.clone().oneshot(req).await.expect("oneshot");
    let status = resp.status();
    let raw = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("read body");
    let env: Value = serde_json::from_slice(&raw)
        .unwrap_or_else(|e| panic!("parse JSON: {e}; raw = {}", String::from_utf8_lossy(&raw)));
    (status, env)
}

/// `GET /print-templates/{id}/content` → `(status, headers, 字节)`。
///
/// ⚠️ 返裸字节（不是信封）⇒ **不能**用 test-support 的 `send`（它会强制按 JSON
/// 解析并在失败处 panic）。这里自建 oneshot 驱动（同一个 `tower::ServiceExt`
/// 用法，只是自己拿 `HeaderMap`）。
pub(crate) async fn fetch_content(
    ctx: &Ctx,
    template_id: i64,
    bearer: &str,
) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
    use hsh_erp_test_support::json_request;
    use tower::ServiceExt;
    let req: Request<Body> = json_request(
        "GET",
        &format!("/print-templates/{template_id}/content"),
        None,
        Some(bearer),
    );
    let resp: Response<Body> = ctx.app.clone().oneshot(req).await.expect("oneshot");
    let status = resp.status();
    let headers = resp.headers().clone();
    let raw = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("read body");
    (status, headers, raw.to_vec())
}

/// `GET /print-templates/resolve?customer_id=` → `(status, 信封)`。
pub(crate) async fn resolve(ctx: &Ctx, customer_id: i64, bearer: &str) -> (StatusCode, Value) {
    hsh_erp_test_support::send(
        ctx.app.clone(),
        hsh_erp_test_support::json_request(
            "GET",
            &format!("/print-templates/resolve?customer_id={customer_id}"),
            None,
            Some(bearer),
        ),
    )
    .await
}

/// `GET /print-templates` 列表 → `(status, 信封)`。
pub(crate) async fn list(ctx: &Ctx, query: &str, bearer: &str) -> (StatusCode, Value) {
    hsh_erp_test_support::send(
        ctx.app.clone(),
        hsh_erp_test_support::json_request(
            "GET",
            &format!("/print-templates{query}"),
            None,
            Some(bearer),
        ),
    )
    .await
}

/// `POST /print-templates/{id}/delete` → `(status, 信封)`。
pub(crate) async fn delete(
    ctx: &Ctx,
    template_id: i64,
    body: Value,
    bearer: &str,
) -> (StatusCode, Value) {
    hsh_erp_test_support::send(
        ctx.app.clone(),
        hsh_erp_test_support::json_request(
            "POST",
            &format!("/print-templates/{template_id}/delete"),
            Some(body),
            Some(bearer),
        ),
    )
    .await
}

// ===========================================================================
//  断言 helper
// ===========================================================================

/// 断言信封里的业务码 + HTTP 状态。
pub(crate) fn assert_biz_code(
    status: StatusCode,
    env: &Value,
    want_code: i32,
    want_http: StatusCode,
) {
    assert_eq!(
        status, want_http,
        "HTTP 状态不符；信封={env}（want_http={want_http} want_code={want_code}）"
    );
    assert_eq!(
        env.get("code").and_then(|c| c.as_i64()),
        Some(want_code as i64),
        "业务码不符：{env}（want {want_code}）"
    );
}

/// 读 `t_part_file` 里该 owner + kind 的**全部**行（含已软删），按 id 升序。
///
/// 刻意 `include_deleted` —— 「同 L1 二次上传 = 替换」这条回归就是要断言
/// 旧行 `deleted_at` 非空。
pub(crate) async fn rows_for_owner_kind(
    pool: &PgPool,
    owner_id: i64,
    kind: &str,
) -> Vec<(i64, bool, String, String, i64, i32)> {
    sqlx::query_as::<_, (i64, Option<chrono::NaiveDateTime>, String, String, i64, i32)>(
        "SELECT id, deleted_at, file_type, object_key, file_size, version \
         FROM t_part_file WHERE part_id = $1 AND kind = $2 ORDER BY id ASC",
    )
    .bind(owner_id)
    .bind(kind)
    .fetch_all(pool)
    .await
    .expect("query t_part_file")
    .into_iter()
    .map(
        |(id, deleted_at, file_type, object_key, file_size, version)| {
            (
                id,
                deleted_at.is_some(),
                file_type,
                object_key,
                file_size,
                version,
            )
        },
    )
    .collect()
}

// ===========================================================================
//  数据 fixture helper（直插）
// ===========================================================================

/// 插一个**一级客户**（`parent_id IS NULL`）。
///
/// ⚠️ `serial_prefix` 是**必填**且有唯一索引 `uq_t_customer_root_prefix`
/// （一级客户专用），故每个用例要换前缀字母。
pub(crate) async fn insert_l1_customer(pool: &PgPool, name: &str, prefix: char) -> i64 {
    let id = shared_test_snowflake().next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_customer (id, name, parent_id, serial_prefix, version, \
         created_at, updated_at) VALUES ($1, $2, NULL, $3, 0, $4, $4)",
    )
    .bind(id)
    .bind(name)
    .bind(prefix.to_string())
    .bind(now)
    .execute(pool)
    .await
    .expect("insert L1 customer");
    id
}

/// 插一个**二级客户**（`parent_id` 指向某个 L1）。
pub(crate) async fn insert_l2_customer(pool: &PgPool, name: &str, l1_id: i64) -> i64 {
    let id = shared_test_snowflake().next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_customer (id, name, parent_id, serial_prefix, version, \
         created_at, updated_at) VALUES ($1, $2, $3, NULL, 0, $4, $4)",
    )
    .bind(id)
    .bind(name)
    .bind(l1_id)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert L2 customer");
    id
}

/// 插一个散件 `t_part`（`assembly_id = NULL`）—— 只为造一行 `DRAWING` 文件的 owner。
pub(crate) async fn insert_part_for_drawing(ctx: &Ctx) -> i64 {
    let id = shared_test_snowflake().next_id();
    let now = now_naive();
    let today = now.date();
    sqlx::query(
        "INSERT INTO t_part (id, serial_no, name, drawing_no, applicant_name, quantity, \
         unit_price, total_price, request_date, planned_delivery_date, system_delivery_date, \
         status, is_urgent, customer_id, version, created_at, updated_at) \
         VALUES ($1, NULL, '模板测试件', 'DWG-PT-1', 'tester', 1, 0, 0, $2, $2, $2, \
                 'PENDING', false, $3, 0, $4, $4)",
    )
    .bind(id)
    .bind(today)
    .bind(ctx.l1_id)
    .bind(now)
    .execute(&ctx.pool)
    .await
    .expect("insert t_part");
    id
}

/// 插一行 `t_part_file`（`kind='DRAWING'`），作为「传错 id 给 content 端点」的反例。
///
/// ⚠️ 直插 SQL 而不走 `POST /files/objects`：本用例要的是「库里存在一行**非
/// 模板**文件」这个前置，而走 API 得先造 part + 拼 multipart，会让这条守卫用例
/// 多三行样板。`object_key` 是**假**的 —— 本域的 kind 守卫跑在取字节**之前**，
/// 根本不会去访问 COS（这正是要钉的性质）。
pub(crate) async fn insert_drawing_row(ctx: &Ctx, part_id: i64) -> i64 {
    let id = shared_test_snowflake().next_id();
    sqlx::query(
        "INSERT INTO t_part_file (id, part_id, kind, file_type, object_key, \
         original_filename, file_size, content_type, upload_status, content_sha256, \
         version, created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, 'DRAWING', 'PDF', 'uploads/deadbeefdeadbeef_drawing.pdf', \
                 'drawing.pdf', 10, 'application/pdf', 'READY', NULL, 0, now(), 0, now(), 0)",
    )
    .bind(id)
    .bind(part_id)
    .execute(&ctx.pool)
    .await
    .expect("insert t_part_file DRAWING");
    id
}

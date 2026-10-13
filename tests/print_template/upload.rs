//! 端点 1 `POST /print-templates`：multipart 上传 + **替换**语义 + 四道轻校验
//!
//! 覆盖 8 组场景（★ = 核心回归）：
//! 1. ★ `upload_happy_path_writes_delivery_template_row` —— 201 Created +
//!    `t_part_file` 行的 `kind` / `part_id` / `file_type` / `object_key` 逐项钉死
//! 2. ★ `upload_twice_for_same_l1_replaces_old_row` —— 替换：旧行 `deleted_at`
//!    非空、唯一索引不炸、`resolve` 只命中新的一份
//! 3. ★ `upload_to_l2_customer_returns_20104` —— 只绑一级客户
//! 4. ★ `upload_rejects_non_xlsx_extension` / `upload_rejects_non_zip_magic` /
//!    `upload_rejects_empty_bytes` —— 四道轻校验
//! 5. ★ `clerk_cannot_upload_returns_403` —— 且**断言库里的行数没变**
//!    （闸门必须落在写入之前）
//! 6. `upload_with_missing_data_field_returns_20104`
//! 7. `upload_with_non_numeric_customer_id_returns_20104`
//! 8. `upload_to_two_different_l1_keeps_both` —— 两个 L1 各一份，互不干扰

use axum::http::StatusCode;

use hsh_erp_rust::util::cos_key;
use hsh_erp_test_support::json_request;

use crate::{
    Ctx, assert_biz_code, bootstrap, minimal_xlsx_bytes, multipart_request, rows_for_owner_kind,
    send_multipart, upload,
};

/// 端点 1 的路径（列表端点同前缀，但 method 不同）。
const LIST_URI: &str = "/print-templates";

// ===========================================================================
//  1. happy path
// ===========================================================================

/// ★ 上传成功 ⇒ **HTTP 201**，`t_part_file` 行的四个关键列逐项对上。
///
/// 钉的是「**复用 `t_part_file` 而不新建表**」这个决策的**可观测形态**：
/// - `kind = 'DELIVERY_TEMPLATE'`（migration 加的第 7 个 CHECK 值）；
/// - `part_id = <L1 客户 id>`（⚠️ 列名叫 `part_id`，装的是 `t_customer.id`
///   —— 库里无法区分「零件 123 的图纸」与「客户 123 的模板」，见
///   `docs/api/print_template.md` §8.3 第 2 条）；
/// - `file_type = 'XLSX'`（`policy::file_type_for_ext("xlsx")` 的推导结果）；
/// - `object_key` 由 `util::cos_key::build_cas_key` 生成（内容寻址：
///   `{prefix}{sha16}_{safe_filename}`），而不是某个新表自己发明的格式。
#[tokio::test]
async fn upload_happy_path_writes_delivery_template_row() {
    let ctx = bootstrap().await;
    let bytes = minimal_xlsx_bytes("happy path");
    let (status, env) = upload(
        &ctx,
        ctx.l1_id,
        "甲公司送货单模板.xlsx",
        &bytes,
        &ctx.manager_token,
    )
    .await;

    assert_eq!(status, StatusCode::CREATED, "上传成功必须是 201：{env}");
    let d = &env["data"];
    assert_eq!(d["kind"], "DELIVERY_TEMPLATE");
    assert_eq!(d["file_type"], "XLSX");
    assert_eq!(
        d["customer_id"],
        ctx.l1_id.to_string(),
        "雪花 id 必须是 JSON string"
    );
    assert_eq!(d["original_filename"], "甲公司送货单模板.xlsx");
    assert_eq!(d["file_size"], bytes.len() as i64);
    assert_eq!(
        d["content_type"],
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
    );
    assert!(d["version"].is_i64(), "version 是数字：{d}");

    // 库里逐列复核
    let rows = rows_for_owner_kind(&ctx.pool, ctx.l1_id, "DELIVERY_TEMPLATE").await;
    assert_eq!(rows.len(), 1, "应恰好一行：{rows:?}");
    let (id, deleted, file_type, object_key, file_size, version) = &rows[0];
    assert!(!deleted, "新行不得是软删态");
    assert_eq!(*file_type, "XLSX");
    assert_eq!(*file_size, bytes.len() as i64);
    assert_eq!(*version, 0, "新行 version 从 0 起（OCC 锚）");

    // object_key 是内容寻址的 CAS key（与图纸 / 3D 模型同一套生成逻辑）
    let sha = hsh_erp_rust::modules::files::part_file::repo::hash_bytes(&bytes);
    let expected = cos_key::build_cas_key(&ctx.pool_upload_prefix(), &sha, "甲公司送货单模板.xlsx");
    assert_eq!(
        *object_key, expected,
        "object_key 必须是 build_cas_key 的产物"
    );
    assert_eq!(d["id"], id.to_string(), "出参 id 与库里的行 id 一致");

    // 字节确实落进了 COS（内容寻址 ⇒ 重传幂等）
    assert_eq!(
        ctx.cos.get(object_key).as_deref(),
        Some(bytes.as_slice()),
        "上传的字节应落在 object_key 指向的对象上"
    );
}

// ===========================================================================
//  2. 替换语义
// ===========================================================================

/// ★ 同 L1 二次上传 = **替换**（不是新增、也不是 21108 撞索引）。
///
/// `uk_t_part_file_single (part_id, kind) WHERE deleted_at IS NULL` 表达的
/// 是「一个 L1 至多一份**活跃**模板」，所以替换在 DB 层只能靠
/// 「先软删旧行、再写新行」。本用例钉三件事：
/// ① 旧行 `deleted_at` 非空（**不是**被物理删除 —— 审计行留存）；
/// ② 新旧两行的 `object_key` **不同**（内容不同 ⇒ sha 不同 ⇒ key 不同），
///    证明没有走 CAS 复用；
/// ③ `resolve` 命中的是**新**那一份。
#[tokio::test]
async fn upload_twice_for_same_l1_replaces_old_row() {
    let ctx = bootstrap().await;
    let v1 = minimal_xlsx_bytes("v1");
    let v2 = minimal_xlsx_bytes("v2-different-content");

    let (st1, env1) = upload(&ctx, ctx.l1_id, "第一版.xlsx", &v1, &ctx.manager_token).await;
    assert_eq!(st1, StatusCode::CREATED, "{env1}");
    let first_id = env1["data"]["id"].as_str().unwrap().to_string();

    let (st2, env2) = upload(&ctx, ctx.l1_id, "第二版.xlsx", &v2, &ctx.manager_token).await;
    assert_eq!(st2, StatusCode::CREATED, "替换也是 201：{env2}");
    let second_id = env2["data"]["id"].as_str().unwrap().to_string();
    assert_ne!(first_id, second_id, "替换应产生新行（新 id）");

    let rows = rows_for_owner_kind(&ctx.pool, ctx.l1_id, "DELIVERY_TEMPLATE").await;
    assert_eq!(rows.len(), 2, "两行都应留存（旧行软删）：{rows:?}");
    assert!(rows[0].1, "旧行必须被软删");
    assert!(!rows[1].1, "新行不得是软删态");
    assert_ne!(
        rows[0].3, rows[1].3,
        "内容不同 ⇒ object_key 必须不同（没走 CAS 复用）"
    );

    // resolve 只命中活跃那一份
    let (status, env) = crate::resolve(&ctx, ctx.l1_id, &ctx.manager_token).await;
    assert_eq!(status, StatusCode::OK, "{env}");
    assert_eq!(
        env["data"]["template"]["id"], second_id,
        "resolve 必须给最新版"
    );
    assert_eq!(env["data"]["template"]["original_filename"], "第二版.xlsx");
}

// ===========================================================================
//  3. L1-only
// ===========================================================================

/// ★ 绑**二级**客户 ⇒ `20104 BIZ_INVALID_VALUE`（HTTP 400）。
///
/// 写侧**刻意不区分**「不存在 / 已软删 / 不是一级客户」三种情形 ——
/// 前端拿到的都是「你选的这个客户 id 不能绑模板」，message 里带原 id 让它自己改选，
/// 不要让前端为「到底是哪个原因」再发一次请求去查。
#[tokio::test]
async fn upload_to_l2_customer_returns_20104() {
    let ctx = bootstrap().await;
    let bytes = minimal_xlsx_bytes("l2");
    let (status, env) = upload(
        &ctx,
        ctx.l2_id,
        "二级客户模板.xlsx",
        &bytes,
        &ctx.manager_token,
    )
    .await;

    assert_biz_code(status, &env, 20104, StatusCode::BAD_REQUEST);
    let rows = rows_for_owner_kind(&ctx.pool, ctx.l2_id, "DELIVERY_TEMPLATE").await;
    assert!(rows.is_empty(), "被拒的上传不得留下任何行：{rows:?}");
    let msg = env["message"].as_str().unwrap_or_default();
    assert!(
        msg.contains("不是一级客户"),
        "message 应点明绑定粒度：{env}"
    );
}

/// 客户 id **不存在** ⇒ 同样 20104（见上条 doc 的「刻意不区分」）。
#[tokio::test]
async fn upload_to_nonexistent_customer_returns_20104() {
    let ctx = bootstrap().await;
    let bytes = minimal_xlsx_bytes("ghost");
    let (status, env) = upload(
        &ctx,
        -987_654_321,
        "幽灵客户.xlsx",
        &bytes,
        &ctx.manager_token,
    )
    .await;
    assert_biz_code(status, &env, 20104, StatusCode::BAD_REQUEST);
    assert!(
        rows_for_owner_kind(&ctx.pool, -987_654_321, "DELIVERY_TEMPLATE")
            .await
            .is_empty()
    );
}

// ===========================================================================
//  4. 四道轻校验
// ===========================================================================

/// ★ 扩展名非 xlsx ⇒ `21102 BIZ_PART_FILE_BAD_TYPE`（HTTP 400）。
///
/// ⚠️ 这是**本域的第一道格式守卫**（在调 service 之前），所以库里的行数必须不变。
#[tokio::test]
async fn upload_rejects_non_xlsx_extension() {
    let ctx = bootstrap().await;
    let bytes = minimal_xlsx_bytes("wrong ext");
    let (status, env) = upload(&ctx, ctx.l1_id, "模板.pdf", &bytes, &ctx.manager_token).await;

    assert_biz_code(status, &env, 21102, StatusCode::BAD_REQUEST);
    assert!(
        rows_for_owner_kind(&ctx.pool, ctx.l1_id, "DELIVERY_TEMPLATE")
            .await
            .is_empty(),
        "被拒的上传不得留下任何行"
    );
}

/// ★ 文件名是 `.xlsx` 但**前 4 字节不是 `PK\x03\x04`** ⇒ `21102`。
///
/// 这道挡的是「把 PDF / txt 改名成 `.xlsx`」—— 后端**刻意不引 zip crate**
/// 解析内部结构（见 `dto.rs` 末尾的取舍说明），但「至少是个 zip 容器」这一层
/// 是廉价的、能挡掉绝大多数误传。
#[tokio::test]
async fn upload_rejects_non_zip_magic() {
    let ctx = bootstrap().await;
    let fake = b"%PDF-1.5\nthis is a pdf renamed to .xlsx\n%%EOF".to_vec();
    let (status, env) = upload(&ctx, ctx.l1_id, "其实是pdf.xlsx", &fake, &ctx.manager_token).await;

    assert_biz_code(status, &env, 21102, StatusCode::BAD_REQUEST);
    let msg = env["message"].as_str().unwrap_or_default();
    assert!(msg.contains("PK"), "message 应点明缺的是 zip magic：{env}");
    assert!(
        rows_for_owner_kind(&ctx.pool, ctx.l1_id, "DELIVERY_TEMPLATE")
            .await
            .is_empty()
    );
}

/// ★ 0 字节 ⇒ `40001 VALIDATION_ERROR`（**HTTP 422**，不是 21103 / 400）。
///
/// 空文件是「无效输入」不是「太大」—— 与 `files` 域那两道 size 守卫的取舍逐字一致。
#[tokio::test]
async fn upload_rejects_empty_bytes() {
    let ctx = bootstrap().await;
    let (status, env) = upload(&ctx, ctx.l1_id, "空模板.xlsx", &[], &ctx.manager_token).await;

    assert_biz_code(status, &env, 40001, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(
        rows_for_owner_kind(&ctx.pool, ctx.l1_id, "DELIVERY_TEMPLATE")
            .await
            .is_empty()
    );
}

// ===========================================================================
//  5. 角色守卫
// ===========================================================================

/// ★ CLERK 上传 ⇒ **HTTP 403**（`40300`），且**库里不留下任何行**。
///
/// ⚠️ 钉「零副作用」而不只是 403：`upload_file_for_owner` 里那道
/// 「CUSTOMER 只放 Manager」的分派**排在软删之前**，否则 CLERK 打一发
/// 403 就把该客户原有的模板软删掉了 —— 那是数据损坏而不是拒绝。
/// 回归用例见 `resolve.rs::clerk_cannot_bypass_via_generic_upload_endpoint`
/// （它经 `POST /files/objects` 那条通用入口打，钉的是同一件事的另一个面）。
#[tokio::test]
async fn clerk_cannot_upload_returns_403() {
    let ctx = bootstrap().await;
    let bytes = minimal_xlsx_bytes("clerk attempt");

    // 先给该 L1 传一份合法模板（作为「原有模板」）
    let (st, env) = upload(&ctx, ctx.l1_id, "原模板.xlsx", &bytes, &ctx.manager_token).await;
    assert_eq!(st, StatusCode::CREATED, "{env}");
    let before = rows_for_owner_kind(&ctx.pool, ctx.l1_id, "DELIVERY_TEMPLATE").await;
    assert_eq!(before.len(), 1);
    assert!(!before[0].1, "原有模板应仍是活跃态");

    // CLERK 试图替换
    let v2 = minimal_xlsx_bytes("clerk version 2");
    let (status, env) = upload(&ctx, ctx.l1_id, "偷换.xlsx", &v2, &ctx.clerk_token).await;
    assert_biz_code(status, &env, 40300, StatusCode::FORBIDDEN);

    // 原有模板**原封不动**（没被软删、没有新行）
    let after = rows_for_owner_kind(&ctx.pool, ctx.l1_id, "DELIVERY_TEMPLATE").await;
    assert_eq!(after.len(), 1, "不得新增行：{after:?}");
    assert!(!after[0].1, "原有模板**不得**被软删（403 是零副作用）");
}

// ===========================================================================
//  6/7. multipart 契约
// ===========================================================================

/// 缺 `data` 字段 ⇒ `20104`。
#[tokio::test]
async fn upload_with_missing_data_field_returns_20104() {
    let ctx = bootstrap().await;
    let bytes = minimal_xlsx_bytes("no data field");
    let (status, env) = send_multipart(
        &ctx,
        multipart_request(
            None,
            Some((
                "file",
                "模板.xlsx",
                "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
                &bytes,
            )),
            Some(&ctx.manager_token),
        ),
    )
    .await;
    assert_biz_code(status, &env, 20104, StatusCode::BAD_REQUEST);
}

/// `customer_id` 非数字 ⇒ `20104`（雪花 id 出入参统一 **string** 形态）。
#[tokio::test]
async fn upload_with_non_numeric_customer_id_returns_20104() {
    let ctx = bootstrap().await;
    let bytes = minimal_xlsx_bytes("bad id");
    let (status, env) = send_multipart(
        &ctx,
        multipart_request(
            Some(r#"{"customer_id":"不是数字"}"#),
            Some((
                "file",
                "模板.xlsx",
                "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
                &bytes,
            )),
            Some(&ctx.manager_token),
        ),
    )
    .await;
    assert_biz_code(status, &env, 20104, StatusCode::BAD_REQUEST);
}

/// 缺 `file` 字段 ⇒ `20104`。
#[tokio::test]
async fn upload_without_file_field_returns_20104() {
    let ctx = bootstrap().await;
    let data = format!(r#"{{"customer_id":"{}"}}"#, ctx.l1_id);
    let (status, env) = send_multipart(
        &ctx,
        multipart_request(Some(&data), None, Some(&ctx.manager_token)),
    )
    .await;
    assert_biz_code(status, &env, 20104, StatusCode::BAD_REQUEST);
}

/// 未识别的额外字段被 **drain 掉忽略**（不报错）—— 与 `files` 域 multipart
/// 循环逐字同款。前端将来加个 CSRF 之类的隐藏字段不该让整个上传失败。
#[tokio::test]
async fn upload_ignores_unknown_multipart_fields() {
    let ctx = bootstrap().await;
    let bytes = minimal_xlsx_bytes("extra field");
    let data = format!(r#"{{"customer_id":"{}"}}"#, ctx.l1_id);
    // 复用 multipart_body 的两字段形态，再手工拼一个第三字段
    let mut body = crate::multipart_body(
        Some(&data),
        Some((
            "file",
            "模板.xlsx",
            "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
            &bytes,
        )),
    );
    let extra = format!(
        "--{}\r\nContent-Disposition: form-data; name=\"csrf\"\r\n\r\nwhatever\r\n",
        crate::MP_BOUNDARY
    );
    // 插到收尾 `--boundary--` 之前
    let tail = format!("--{}--\r\n", crate::MP_BOUNDARY);
    let pos = body
        .windows(tail.len())
        .rposition(|w| w == tail.as_bytes())
        .expect("收尾边界");
    body.splice(pos..pos, extra.into_bytes());

    let req = axum::http::Request::builder()
        .method("POST")
        .uri(LIST_URI)
        .header(
            axum::http::header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={}", crate::MP_BOUNDARY),
        )
        .header(
            axum::http::header::AUTHORIZATION,
            format!("Bearer {}", ctx.manager_token),
        )
        .body(axum::body::Body::from(body))
        .expect("build request");

    let (status, env) = send_multipart(&ctx, req).await;
    assert_eq!(status, StatusCode::CREATED, "{env}");
}

// ===========================================================================
//  8. 两个 L1 各自独立
// ===========================================================================

/// 两个不同 L1 各传一份 ⇒ **互不干扰**（唯一索引是按 `(part_id, kind)` 的）。
///
/// 这条钉的是「绑定关系 = 存在性」这个决策的**隔离性**：一个 L1 的上传
/// 不能软删掉另一个 L1 的模板。
#[tokio::test]
async fn upload_to_two_different_l1_keeps_both() {
    let ctx = bootstrap().await;
    let a = minimal_xlsx_bytes("customer A");
    let b = minimal_xlsx_bytes("customer B");

    let (st1, env1) = upload(&ctx, ctx.l1_id, "A.xlsx", &a, &ctx.manager_token).await;
    assert_eq!(st1, StatusCode::CREATED, "{env1}");
    let (st2, env2) = upload(&ctx, ctx.other_l1_id, "B.xlsx", &b, &ctx.manager_token).await;
    assert_eq!(st2, StatusCode::CREATED, "{env2}");

    let rows_a = rows_for_owner_kind(&ctx.pool, ctx.l1_id, "DELIVERY_TEMPLATE").await;
    let rows_b = rows_for_owner_kind(&ctx.pool, ctx.other_l1_id, "DELIVERY_TEMPLATE").await;
    assert_eq!(rows_a.len(), 1, "A 的模板不该被 B 的上传软删：{rows_a:?}");
    assert!(!rows_a[0].1);
    assert_eq!(rows_b.len(), 1, "{rows_b:?}");
    assert!(!rows_b[0].1);
}

// ===========================================================================
//  9. 未登录
// ===========================================================================

/// 未登录 ⇒ 401（`authenticate_middleware` 在 route_layer 上，优先于 handler 守卫）。
#[tokio::test]
async fn upload_without_token_returns_401() {
    let ctx = bootstrap().await;
    let bytes = minimal_xlsx_bytes("no token");
    let data = format!(r#"{{"customer_id":"{}"}}"#, ctx.l1_id);
    let (status, env) = send_multipart(
        &ctx,
        multipart_request(
            Some(&data),
            Some((
                "file",
                "模板.xlsx",
                "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
                &bytes,
            )),
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{env}");
    // 列表端点同样未登录 ⇒ 401
    let (status, env) =
        hsh_erp_test_support::send(ctx.app.clone(), json_request("GET", LIST_URI, None, None))
            .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{env}");
}

// ===========================================================================
//  test-support 的 helper（列表端点顺带在 delete.rs 里覆盖）
// ===========================================================================

impl Ctx {
    /// 测试态的 COS 上传前缀（`test_state_with_cos` 的 `CosConfig.upload_prefix`）。
    ///
    /// ⚠️ 值 `"uploads"` 来自 `test-support/src/state.rs` 里**测试自造**的 cfg
    /// （与 `config.rs` 的缺省 `obj/` 无关，也与生产 `.env` 无关）——
    /// 与 `tests/files/upload.rs` 的 `UPLOAD_UPLOAD_PREFIX` 同一份值。
    /// ⚠️ **改 `test-support` 的这一行必须同步改这里**，否则 object_key 断言会红。
    fn pool_upload_prefix(&self) -> String {
        "uploads".to_string()
    }
}

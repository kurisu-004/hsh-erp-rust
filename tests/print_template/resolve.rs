//! 端点 3 / 4：`GET /print-templates/resolve` 与 `GET /{id}/content`
//!
//! 覆盖 10 组场景（★ = 核心回归）：
//! 1. ★ `resolve_hit_returns_template_and_customer_name`
//! 2. ★ `resolve_miss_returns_21109_with_http_422` —— **HTTP 状态码是 422**
//!    （2026-10-16 新选，python 转发时代是 400）
//! 3. ★ `resolve_after_soft_delete_returns_21109`
//! 4. ★ `resolve_is_available_to_inspector` —— **不是 MANAGER-only**
//! 5. ★ `resolve_denied_for_shelf_account` —— 权限点矩阵不含 SHELF_ACCOUNT
//! 6. ★ `content_bytes_are_byte_identical_to_upload` + Content-Type 是 xlsx MIME
//! 7. ★ `content_of_non_template_file_returns_21101` —— 传一个图纸 id
//! 8. ★ `clerk_cannot_bypass_via_generic_upload_endpoint` —— CLERK 打
//!    `POST /files/objects`（通用上传原语）也造不出模板行
//! 9. ★ `old_paths_return_404` —— `/api/v2/com/delivery/template/*` 与
//!    `/api/v2/print-template/*`（少个 s）
//! 10. `resolve_without_static_path_swallowing` —— matchit 实测：静态段没被吞

use axum::http::StatusCode;
use hsh_erp_test_support::{json_request, send, send_raw};

use crate::{
    Ctx, assert_biz_code, bootstrap, fetch_content, minimal_xlsx_bytes, resolve,
    rows_for_owner_kind, upload,
};

/// `delivery_note.print` 权限点的默认 MIME 断言用常量。
const XLSX_MIME: &str = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet";

/// 造一份「上传成功」的模板，返回其 id（减少各用例样板）。
async fn seed_template(ctx: &Ctx, customer_id: i64) -> i64 {
    let bytes = minimal_xlsx_bytes(&format!("seed {customer_id}"));
    let (st, env) = upload(ctx, customer_id, "模板.xlsx", &bytes, &ctx.manager_token).await;
    assert_eq!(st, StatusCode::CREATED, "seed 上传失败：{env}");
    env["data"]["id"]
        .as_str()
        .expect("id 是 JSON string")
        .parse::<i64>()
        .expect("id 可解析为 i64")
}

// ===========================================================================
//  1. 命中
// ===========================================================================

/// ★ `resolve` 命中 ⇒ 200，回模板本体 + 该一级客户的名称。
#[tokio::test]
async fn resolve_hit_returns_template_and_customer_name() {
    let ctx = bootstrap().await;
    let id = seed_template(&ctx, ctx.l1_id).await;

    let (status, env) = resolve(&ctx, ctx.l1_id, &ctx.manager_token).await;
    assert_eq!(status, StatusCode::OK, "{env}");
    let t = &env["data"]["template"];
    assert_eq!(t["id"], id.to_string());
    assert_eq!(t["customer_id"], ctx.l1_id.to_string());
    assert_eq!(t["kind"], "DELIVERY_TEMPLATE");
    assert_eq!(t["file_type"], "XLSX");
    assert_eq!(
        env["data"]["customer_name"], "模板测试甲公司",
        "customer_name 靠本域的一条 t_customer 只读聚合：{env}"
    );
}

// ===========================================================================
//  2. 未绑定 ⇒ 21109 / HTTP 422
// ===========================================================================

/// ★ 未绑定 ⇒ **`21109 BIZ_DELIVERY_TEMPLATE_NOT_CONFIGURED` + HTTP 422**。
///
/// ⚠️ 422 是 2026-10-16 新选的 HTTP 映射（原先 python 转发时代是 400，
/// `src/shared/error.rs::status_from_code` 里**显式登记**）。取 422 的理由：
/// 请求本身完全合法（客户 id 存在、是一级客户、没有越权），**不满足的是
/// 「这个客户的配置」** —— 前端据此弹「去模板管理页绑定 / 或手动上传兜底」，
/// 而不是「你请求参数错了」。
#[tokio::test]
async fn resolve_miss_returns_21109_with_http_422() {
    let ctx = bootstrap().await;
    let (status, env) = resolve(&ctx, ctx.l1_id, &ctx.manager_token).await;
    assert_biz_code(status, &env, 21109, StatusCode::UNPROCESSABLE_ENTITY);
    let msg = env["message"].as_str().unwrap_or_default();
    assert!(
        msg.contains(&ctx.l1_id.to_string()),
        "message 应带客户 id：{env}"
    );
}

/// ★ 已软删（`POST /{id}/delete` 之后）⇒ **同样** 21109。
///
/// 这是「绑定关系 = 存在性」的直接后果：软删了行就等于解了绑。
#[tokio::test]
async fn resolve_after_soft_delete_returns_21109() {
    let ctx = bootstrap().await;
    let id = seed_template(&ctx, ctx.l1_id).await;

    let (st, env) = crate::delete(
        &ctx,
        id,
        serde_json::json!({"version": 0}),
        &ctx.manager_token,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "删除应成功：{env}");

    let (status, env) = resolve(&ctx, ctx.l1_id, &ctx.manager_token).await;
    assert_biz_code(status, &env, 21109, StatusCode::UNPROCESSABLE_ENTITY);
}

/// `resolve` 缺 `customer_id` ⇒ `20104`（**不是** 422 纯文本 —— 前端要拿到
/// `{code, message}` 才能弹提示）。
#[tokio::test]
async fn resolve_without_customer_id_returns_20104() {
    let ctx = bootstrap().await;
    let (status, env) = send(
        ctx.app.clone(),
        json_request(
            "GET",
            "/print-templates/resolve",
            None,
            Some(&ctx.manager_token),
        ),
    )
    .await;
    assert_biz_code(status, &env, 20104, StatusCode::BAD_REQUEST);
}

// ===========================================================================
//  4/5. 权限点守卫
// ===========================================================================

/// ★ `resolve` / `content` 对 **INSPECTOR 放行**（权限点 `delivery_note.print`
/// 的默认矩阵是 MANAGER / CLERK / INSPECTOR）。
///
/// 这条钉的是「两条读端点**绝不能** MANAGER-only」—— 打送货单是 CLERK /
/// INSPECTOR 的日常动作（备货、点检、装车），管理面与作业面不是同一批人。
#[tokio::test]
async fn resolve_is_available_to_inspector() {
    let ctx = bootstrap().await;
    let id = seed_template(&ctx, ctx.l1_id).await;

    let (status, env) = resolve(&ctx, ctx.l1_id, &ctx.inspector_token).await;
    assert_eq!(status, StatusCode::OK, "INSPECTOR 必须能用：{env}");

    let (status, headers, bytes) = fetch_content(&ctx, id, &ctx.inspector_token).await;
    assert_eq!(status, StatusCode::OK, "INSPECTOR 取字节：{status}");
    assert_eq!(
        headers.get("content-type").and_then(|v| v.to_str().ok()),
        Some(XLSX_MIME)
    );
    assert!(!bytes.is_empty());

    // CLERK 同款（矩阵里的第三格）
    let (status, env) = resolve(&ctx, ctx.l1_id, &ctx.clerk_token).await;
    assert_eq!(status, StatusCode::OK, "CLERK 必须能用：{env}");
}

/// ★ `SHELF_ACCOUNT` ⇒ **403**。
///
/// 权限点 `delivery_note.print` 的默认矩阵**不含** `SHELF_ACCOUNT`
/// （那是报工台工控机账号，只该看图纸）。本域刻意**不**把它放进来 ——
/// 工控机没有任何理由读客户的送货单抬头。
#[tokio::test]
async fn resolve_denied_for_shelf_account() {
    let ctx = bootstrap().await;
    let id = seed_template(&ctx, ctx.l1_id).await;

    let (status, env) = resolve(&ctx, ctx.l1_id, &ctx.shelf_token).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{env}");

    let (status, _, _) = fetch_content(&ctx, id, &ctx.shelf_token).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "content 同样拒绝工控机账号");
}

// ===========================================================================
//  6. content 字节
// ===========================================================================

/// ★ `GET /{id}/content` 返回的字节与上传的**逐字节相等**，Content-Type 是
/// xlsx 的规范 MIME，`Cache-Control` 是 `private, max-age=300`。
///
/// ⚠️ 300（5 分钟）刻意比 `files` 域 `/objects/{id}/content` 的 1200s **短**：
/// 图纸是长期不变的资产，而**模板会被重绑**（`POST /` 就是替换语义）。
#[tokio::test]
async fn content_bytes_are_byte_identical_to_upload() {
    let ctx = bootstrap().await;
    let bytes = minimal_xlsx_bytes("byte identical");
    let (st, env) = upload(&ctx, ctx.l1_id, "取字节.xlsx", &bytes, &ctx.manager_token).await;
    assert_eq!(st, StatusCode::CREATED, "{env}");
    let id = env["data"]["id"].as_str().unwrap().parse::<i64>().unwrap();

    let (status, headers, got) = fetch_content(&ctx, id, &ctx.manager_token).await;
    assert_eq!(status, StatusCode::OK, "{status}");
    assert_eq!(
        headers.get("content-type").and_then(|v| v.to_str().ok()),
        Some(XLSX_MIME),
        "Content-Type 恒为 xlsx MIME"
    );
    assert_eq!(
        headers.get("content-length").and_then(|v| v.to_str().ok()),
        Some(bytes.len().to_string().as_str()),
        "Content-Length 应与字节数一致"
    );
    assert_eq!(
        headers.get("cache-control").and_then(|v| v.to_str().ok()),
        Some("private, max-age=300"),
        "模板会被重绑 ⇒ 缓存窗口刻意比 files 域的 1200s 短"
    );
    assert_eq!(
        got, bytes,
        "★ 返回字节必须与上传字节逐字节相等（前后端都不能二次加工）"
    );
    // 出参是**裸字节**（不是 R<T> 信封）
    assert!(
        serde_json::from_slice::<serde_json::Value>(&got).is_err(),
        "content 端点返裸 xlsx，不该是 JSON 信封"
    );
}

// ===========================================================================
//  7. kind 守卫
// ===========================================================================

/// ★ 传一个**图纸 id**（`kind='DRAWING'`）给 `/{id}/content` ⇒ `21101`（404）。
///
/// kind 校验在 `print_template::service::get_template_content` 里，且**在取字节
/// 之前** —— 非法 kind 零 COS IO。
#[tokio::test]
async fn content_of_non_template_file_returns_21101() {
    let ctx = bootstrap().await;
    // 借 `POST /files/objects` 造一行 DRAWING（owner_kind=PART，绑本域无关的 part）
    let part_id = crate::insert_part_for_drawing(&ctx).await;
    let drawing_id = crate::insert_drawing_row(&ctx, part_id).await;

    let (status, _, _) = fetch_content(&ctx, drawing_id, &ctx.manager_token).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "非模板行应 404（21101）");
}

// ===========================================================================
//  8. 通用上传入口的绕行面
// ===========================================================================

/// ★ CLERK 经 `POST /api/v2/files/objects`（**files 域的通用上传原语**）
/// 也造不出 `DELIVERY_TEMPLATE` 行 ⇒ 403，且该 L1 名下**没有**模板行。
///
/// ⚠️ 这是本域最容易被绕开的一处：`POST /print-templates` 有 MANAGER 守卫，
/// 但 `POST /files/objects` 的 `owner_kind` / `kind` 都由 client 在 multipart
/// `data` 里给。只在域内 handler 守 MANAGER 是不够的 ——
/// 所以 `PartFileService::upload_file_for_owner` 里有那道按 `owner_kind`
/// 分派角色的闸门（2026-10-16 新增）。本用例从**外部**钉住它。
#[tokio::test]
async fn clerk_cannot_bypass_via_generic_upload_endpoint() {
    use crate::{multipart_request, send_multipart};
    let ctx = bootstrap().await;
    let bytes = minimal_xlsx_bytes("bypass attempt");

    let data = format!(
        r#"{{"owner_kind":"CUSTOMER","owner_id":"{}","kind":"DELIVERY_TEMPLATE"}}"#,
        ctx.l1_id
    );
    let (status, env) = send_multipart(
        &ctx,
        multipart_request(
            Some(&data),
            Some((
                "file",
                "绕行.xlsx",
                "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
                &bytes,
            )),
            Some(&ctx.clerk_token),
        ),
    )
    .await;

    assert_eq!(status, StatusCode::FORBIDDEN, "CLERK 绕行必须 403：{env}");
    assert!(
        rows_for_owner_kind(&ctx.pool, ctx.l1_id, "DELIVERY_TEMPLATE")
            .await
            .is_empty(),
        "绕行失败不得留下任何行"
    );
}

// ===========================================================================
//  9. 旧路径 404
// ===========================================================================

/// ★ 两条「曾经看起来该存在」的相近路径都是**干净 404**。
///
/// - `/api/v2/com/delivery/template/*` —— 送货单域内的模板路径。该域 17 端点里
///   **从来没有过**模板端点（见 `docs/api/delivery_note.md` §6 的移除记录：
///   `POST /{id}/print` / `print-labels` 整链路 2026-10-08 已下线，且本轮
///   **逐字不恢复**任何打印端点）。
/// - `/api/v2/print-template/*`（**少个 `s`**）—— 前端拼错 URL 时的形态。
///   刻意**不设 alias**：设 alias 会把一个拼写错误伪装成「服务正常」，
///   反而更难在前端发现。
///
/// ⚠️ 两者的形态都是「**干净 404 空 body**」：404 由 axum 的 fallback 产出，
/// **不**走 `R<T>` 信封（`route_layer` 只作用于已匹配路由 ⇒ 404 甚至不过
/// 鉴权层）。这与「同段数静态段被 catch-all 吞成 400 纯文本」是两回事 ——
/// 本域 `/print-template` 与 `/print-templates` 是**同级不同名**，
/// `/com/delivery/template` 与 `/com/delivery/note` 也是同级不同名，
/// 两者都不是 matchit 的静态/捕获之争。
///
/// ⇒ 必须用 `send_raw` 而不是 `send`：后者强制按 JSON 解析，会在空 body 上 panic。
#[tokio::test]
async fn old_paths_return_404() {
    let ctx = bootstrap().await;
    for uri in [
        "/com/delivery/template",
        "/com/delivery/template/1",
        "/com/delivery/template/resolve",
        "/print-template",
        "/print-template/resolve",
        "/print-template/1/content",
    ] {
        let (status, raw) = send_raw(
            ctx.app.clone(),
            json_request("GET", uri, None, Some(&ctx.manager_token)),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri} 应 404，实际：{raw}");
        assert!(
            raw.trim().is_empty(),
            "404 应是**干净空 body**（不是 R 信封，也不是 400 纯文本）：{uri} → {raw:?}"
        );
    }
}

// ===========================================================================
//  10. matchit 实测
// ===========================================================================

/// ★ `/resolve`（1 段静态）**没被**任何同段数 catch-all 吞成 400。
///
/// CLAUDE.md「路由声明规约」第 9 条的老踩坑是「同段数静态段被 `Path<i64>`
/// catch-all 吞成 400」。本域**没有** 1 段的捕获路由（无 `/{template_id}` 端点），
/// 但**别只靠读源码判断** —— 本用例实打三发：
/// `GET /resolve`（命中）、`GET /resolve?customer_id=`（命中）、
/// `GET /` （命中列表端点），任一变成 400/404 都会红。
#[tokio::test]
async fn resolve_and_static_paths_are_not_swallowed() {
    let ctx = bootstrap().await;
    seed_template(&ctx, ctx.l1_id).await;

    let (status, env) = resolve(&ctx, ctx.l1_id, &ctx.manager_token).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "/resolve 必须命中自己的 handler：{env}"
    );

    let (status, env) = crate::list(&ctx, "", &ctx.manager_token).await;
    assert_eq!(status, StatusCode::OK, "/ 必须命中列表 handler：{env}");

    // 2 段的两条平行路由也都命中
    let id = seed_template(&ctx, ctx.other_l1_id).await;
    let (status, _, _) = fetch_content(&ctx, id, &ctx.manager_token).await;
    assert_eq!(status, StatusCode::OK, "/{{id}}/content 必须命中：{status}");
    let (status, env) = crate::delete(
        &ctx,
        id,
        serde_json::json!({"version": 0}),
        &ctx.manager_token,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "/{{id}}/delete 必须命中：{env}");
}

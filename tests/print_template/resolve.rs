//! 端点 3 / 4：`GET /print-templates/resolve` 与 `GET /{id}/content`
//!
//! 覆盖 19 组场景（★ = 核心回归）：
//! 1. ★ `resolve_hit_returns_template_and_customer_name`
//! 2. ★ `customer_name_is_present_on_upload_list_and_resolve` ——
//!    **行 VO 的 `customer_name`**（前端 `.strict()` schema 的必填项，
//!    2026-10-16 review 第 2 轮 C-1：缺它 ⇒ 上传 / 列表 / 自动选用三条路径全废）
//! 3. ★ `customer_name_is_null_after_customer_soft_deleted` —— `null` 而不是报错
//! 4. ★ `resolve_miss_returns_21109_with_http_422` —— **HTTP 状态码是 422**
//!    （2026-10-16 新选，python 转发时代是 400）
//! 5. ★ `resolve_after_soft_delete_returns_21109`
//! 6. `resolve_without_customer_id_returns_20104`
//! 7. ★ `resolve_is_available_to_inspector` —— **不是 MANAGER-only**
//! 8. ★ `resolve_denied_for_shelf_account` —— 权限点矩阵不含 SHELF_ACCOUNT
//! 9. ★ `content_bytes_are_byte_identical_to_upload` + Content-Type 是 xlsx MIME
//! 10. ★ `content_of_non_template_file_returns_21101` —— 传一个图纸 id
//! 11. ★ `clerk_cannot_bypass_via_generic_upload_endpoint` —— CLERK 打
//!     `POST /files/objects`（通用上传原语）也造不出模板行
//! 12. ★ `manager_bypass_control_group_really_reaches_the_generic_endpoint` ——
//!     上一条的**对照组**（换个角色同一 body 就成功 ⇒ 被拒的确实是角色闸门）
//! 13. ★ `manager_cannot_bind_template_to_l2_customer_via_generic_endpoint` ——
//!     **同一个通用端点也不许绑二级客户**（review 第 2 轮 MAJOR-A：L1 判据下沉进
//!     `customer_owner_exists` ⇒ 21105 + 零副作用）
//! 14. ★ `clerk_cannot_bypass_via_generic_confirm_endpoint` —— **第二条写入口**
//!     （`POST /files/objects/confirm`）同样被收口
//! 15. ★ `manager_cannot_write_template_via_confirm_endpoint` —— confirm 端点
//!     **不接受** CUSTOMER（20104）
//! 16. ★ `clerk_cannot_get_delivery_template_presigned_url` —— 通用 `/url` 端点
//!     的 kind 白名单
//! 17. ★ `shelf_account_list_never_sees_delivery_template_rows` —— 通用列表端点
//!     永不返回模板行
//! 18. ★ `shelf_account_cannot_get_delivery_template_bytes` —— 通用 `/content`
//!     端点的 kind 白名单
//! 19. ★ `old_paths_return_404` —— `/api/v2/com/delivery/template/*` 与
//!     `/api/v2/print-template/*`（少个 s）
//! 20. `resolve_and_static_paths_are_not_swallowed` —— matchit 实测：静态段没被吞
//!
//! ⚠️ 出参形状（`customer_name`）相关的两条刻意放在**本文件**而不是 `upload.rs` /
//! `delete.rs`：它们横跨三个端点，而 resolve 那个是「顶层 + 行内**两个**字段」，
//! 放一起才看得见「刻意重复」这件事。

use axum::http::StatusCode;
use hsh_erp_test_support::{json_request, send, send_raw};
use serde_json::Value;

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
    assert_eq!(st, StatusCode::OK, "seed 模板写入失败：{env}");
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
//  1b. 出参形状：行 VO 的 `customer_name`（2026-10-16 review 第 2 轮 C-1）
// ===========================================================================

/// ★ **列表 / resolve 两个端点的行 VO 都带 `customer_name`** 且命中时等于那个
/// 一级客户的名。
///
/// ⚠️ **这是硬阻断级回归**：前端 `printTemplateSchema` 是 `.strict()` 且
/// `customer_name` **必填** —— 缺这一个键时列表每行都抛 ⇒ 页面恒空、
/// 「客户」列恒显「（已删除的客户）」，也就是对着真后端本域 100% 不可用，
/// 而当时用例全绿。
///
/// ⚠️ 2026-10-15：**写入侧的出参不再带 `customer_name`**。模板写入改走
/// `POST /files/objects/confirm`，它回的是 `R<PartFileOut>`（18 字段的文件行
/// 投影，**没有** `customer_name` —— `customer_name` 是本域给 `t_customer`
/// 补的列，files 域不出）。前端写完模板后**刷新列表**即可拿到名字；本用例的
/// ①② 两步 accordingly 钉的是「列表 / resolve 仍带」，写入侧只钉「落库」。
#[tokio::test]
async fn customer_name_is_present_on_list_and_resolve() {
    let ctx = bootstrap().await;

    // ① 写入侧：confirm 的出参是 `PartFileOut`（**无** customer_name 键）
    let bytes = minimal_xlsx_bytes("customer name shape");
    let (st, env) = upload(&ctx, ctx.l1_id, "带客户名.xlsx", &bytes, &ctx.manager_token).await;
    assert_eq!(st, StatusCode::OK, "{env}");
    assert_eq!(env["data"]["kind"], "DELIVERY_TEMPLATE", "{env}");
    assert!(
        env["data"].get("customer_name").is_none(),
        "confirm 回 `PartFileOut`，按契约不含 customer_name：{env}"
    );

    // ② 端点 2（列表）
    let (st, env) = crate::list(&ctx, "", &ctx.manager_token).await;
    assert_eq!(st, StatusCode::OK, "{env}");
    assert_eq!(env["data"]["items"].as_array().unwrap().len(), 1, "{env}");
    assert_eq!(
        env["data"]["items"][0]["customer_name"], "模板测试甲公司",
        "列表每行都必须带客户名：{env}"
    );

    // ③ 端点 3（resolve）：行内那个 + 顶层那个（刻意重复，两个都得在）
    let (st, env) = resolve(&ctx, ctx.l1_id, &ctx.manager_token).await;
    assert_eq!(st, StatusCode::OK, "{env}");
    assert_eq!(
        env["data"]["template"]["customer_name"], "模板测试甲公司",
        "resolve 的行 VO 也必须带客户名：{env}"
    );
    assert_eq!(
        env["data"]["customer_name"], "模板测试甲公司",
        "resolve 顶层那个保留不动：{env}"
    );
}

/// ★ 客户被软删后，`customer_name` 在两个端点都是 **`null`** 而不是报错、也不是
/// `（已删除的客户）` 之外的任何编造值。
///
/// ⚠️ 语义与模板行**解耦**：软删闸门只管 `t_part_file`，模板行还在 ⇒ 模板管理页
/// 与打印对话框都仍要展示模板，只是名字取不到（`docs/api/print_template.md`
/// §8.3 第 6 条）。
#[tokio::test]
async fn customer_name_is_null_after_customer_soft_deleted() {
    let ctx = bootstrap().await;
    seed_template(&ctx, ctx.l1_id).await;

    // 直插 fixture 的客户被别的域软删（本域只读 `t_customer`，不写）
    sqlx::query("UPDATE t_customer SET deleted_at = now() WHERE id = $1")
        .bind(ctx.l1_id)
        .execute(&ctx.pool)
        .await
        .expect("soft delete customer");

    let (st, env) = resolve(&ctx, ctx.l1_id, &ctx.manager_token).await;
    assert_eq!(st, StatusCode::OK, "客户软删不应让模板打不开：{env}");
    assert_eq!(env["data"]["customer_name"], Value::Null, "{env}");
    assert_eq!(
        env["data"]["template"]["customer_name"],
        Value::Null,
        "行 VO 那个同样退化成 null（两个字段刻意同源）：{env}"
    );

    let (st, env) = crate::list(&ctx, "", &ctx.manager_token).await;
    assert_eq!(st, StatusCode::OK, "{env}");
    assert_eq!(
        env["data"]["items"][0]["customer_name"],
        Value::Null,
        "列表同理：{env}"
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
    assert_eq!(st, StatusCode::OK, "{env}");
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
/// 之前** —— 非法 kind 零 COS IO（反例行的 `object_key` 是假的，真去拉就会炸）。
#[tokio::test]
async fn content_of_non_template_file_returns_21101() {
    let ctx = bootstrap().await;
    // 直插一行 DRAWING（owner 是本域无关的散件 part），见 `insert_drawing_row`
    // 的 doc：刻意不走 API，避免这条守卫用例多三行 multipart 样板。
    let part_id = crate::insert_part_for_drawing(&ctx).await;
    let drawing_id = crate::insert_drawing_row(&ctx, part_id).await;

    let (status, _, _) = fetch_content(&ctx, drawing_id, &ctx.manager_token).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "非模板行应 404（21101）");
}

// ===========================================================================
//  8. 通用写入口的绕行面（两条：upload-intents / confirm）
// ===========================================================================
//
// ⚠️ 2026-10-15：模板写入链路统一到 `POST /files/objects/upload-intents` →
// 前端直传 → `POST /files/objects/confirm` 之后，本域**没有任何自己的上传端点**
// ——「谁能写 DELIVERY_TEMPLATE」这道闸门只存在于这两条通用写入口里
// （`policy::write_roles_for` / `confirm_roles_for`）。因此绕行面必须钉。

/// ★ CLERK 经 `POST /files/objects/upload-intents`（**files 域的通用写入口**）
/// 造不出 `DELIVERY_TEMPLATE` 行 ⇒ **403**，且该 L1 名下**没有**模板行。
///
/// 断言的形态刻意挑在**任何外部 IO 之前**：请求只发到签发端点，拿到 403 就证明
/// 角色闸门排在派生 key / 签名之前。
#[tokio::test]
async fn clerk_cannot_bypass_via_generic_upload_intents_endpoint() {
    use crate::upload_intents;
    let ctx = bootstrap().await;
    let bytes = minimal_xlsx_bytes("bypass attempt");

    let (status, env) =
        upload_intents(&ctx, ctx.l1_id, "绕行.xlsx", &bytes, &ctx.clerk_token).await;

    assert_biz_code(status, &env, 40300, StatusCode::FORBIDDEN);
    assert!(
        rows_for_owner_kind(&ctx.pool, ctx.l1_id, "DELIVERY_TEMPLATE")
            .await
            .is_empty(),
        "绕行失败不得留下任何行：{env}"
    );
    assert!(
        ctx.cos.get_responses.lock().unwrap().is_empty(),
        "被拒的签发不得已往 COS 写过字节"
    );
}

/// ★ **正向对照**：同样的请求换成 **Manager** ⇒ 签发成功（200）**并能走完 confirm
/// 真正落库**。
///
/// ⚠️ 这条是上一条的**对照组**，缺了它上一条就没有意义：403 可能来自任何一层
/// （owner 存在性、`owner_kind` × `kind` 配对、扩展名、content_type……），
/// 只有「同样的 body 换个角色就成功」才能证明被拒的**唯一**原因是角色闸门。
#[tokio::test]
async fn manager_bypass_control_group_really_reaches_the_generic_endpoints() {
    use crate::upload;
    let ctx = bootstrap().await;
    let bytes = minimal_xlsx_bytes("bypass control");

    let (status, env) = upload(
        &ctx,
        ctx.other_l1_id,
        "对照.xlsx",
        &bytes,
        &ctx.manager_token,
    )
    .await;

    assert_eq!(
        status,
        StatusCode::OK,
        "Manager 走通用链路应成功落库：{env}"
    );
    assert_eq!(env["data"]["kind"], "DELIVERY_TEMPLATE");
    assert_eq!(env["data"]["owner_kind"], "CUSTOMER");
    assert_eq!(
        rows_for_owner_kind(&ctx.pool, ctx.other_l1_id, "DELIVERY_TEMPLATE")
            .await
            .len(),
        1,
        "对照组必须真的落库 —— 否则上面那条 403 用例是恒真绿"
    );
}

/// ★ **Manager 也绑不了二级客户**：`owner_id=<L2>` ⇒ **21105**（HTTP 404），
/// 且零副作用（库里无行 + 桶里无对象 + 父 L1 未被碰）。
///
/// ⚠️ L1-only 判据在 `PartFileRepoTrait::customer_owner_exists`（谓词含
/// `AND parent_id IS NULL`），两条写入口共用；错误码沿用 21105。
#[tokio::test]
async fn manager_cannot_bind_template_to_l2_customer_via_generic_endpoint() {
    use crate::upload;
    let ctx = bootstrap().await;
    let bytes = minimal_xlsx_bytes("bind to l2");

    let (status, env) = upload(&ctx, ctx.l2_id, "绑二级.xlsx", &bytes, &ctx.manager_token).await;

    // 21105 = BIZ_PART_FILE_OWNER_NOT_FOUND，`status_from_code` 映射成 404
    assert_biz_code(status, &env, 21105, StatusCode::NOT_FOUND);

    // 零副作用 ①：L2 名下没有模板行（否则就是「永不消费」的孤儿配置行）
    assert!(
        rows_for_owner_kind(&ctx.pool, ctx.l2_id, "DELIVERY_TEMPLATE")
            .await
            .is_empty(),
        "绑二级被拒 ⇒ L2 名下不得有任何行：{env}"
    );
    // 零副作用 ②：桶里也没有对象 —— owner 存在性校验排在任何 COS 交互**之前**
    assert!(
        ctx.cos.get_responses.lock().unwrap().is_empty(),
        "被拒的请求不得已往 COS 写过字节"
    );
    // 零副作用 ③：其父 L1 名下也没有被动过
    assert!(
        rows_for_owner_kind(&ctx.pool, ctx.l1_id, "DELIVERY_TEMPLATE")
            .await
            .is_empty(),
        "绑 L2 的请求不该碰到父 L1 的行：{env}"
    );
}

/// ★ CLERK 经 `POST /files/objects/confirm`（**第二条**写入口）同样造不出模板行
/// ⇒ 403，且零副作用。
///
/// 断言的形态刻意挑在**任何外部 IO 之前**：body 里的 `object_key` 是按
/// (sha, filename) 正确派生但桶里**不存在**的值 —— 若真走到 head 那一步会换成
/// 21114；拿到 403 就证明闸门排在它前面。
#[tokio::test]
async fn clerk_cannot_bypass_via_generic_confirm_endpoint() {
    use crate::{XLSX_MIME, confirm};
    use hsh_erp_rust::util::cos_key::build_cas_key;

    let ctx = bootstrap().await;
    let sha = "a".repeat(64);
    let object_key = build_cas_key("uploads", &sha, "confirm绕行.xlsx");
    let (status, env) = confirm(
        &ctx,
        ctx.l1_id,
        "confirm绕行.xlsx",
        1024,
        XLSX_MIME,
        &sha,
        &object_key,
        &ctx.clerk_token,
    )
    .await;

    assert_biz_code(status, &env, 40300, StatusCode::FORBIDDEN);
    assert!(
        rows_for_owner_kind(&ctx.pool, ctx.l1_id, "DELIVERY_TEMPLATE")
            .await
            .is_empty(),
        "confirm 绕行失败不得留下任何行：{env}"
    );
}

/// ★ **Manager** 经 confirm 写 `CUSTOMER` **是允许的**（2026-10-15 起）。
///
/// ⚠️ 这条与本文件里被删的 `manager_cannot_write_template_via_confirm_endpoint`
/// 正好相反 —— 打印模板直传化之后，模板写入**只**有 confirm 这一条路，
/// 「替换语义」（同 L1 至多一份活跃模板）由 confirm 事务内的
/// 「软删旧活跃行 + INSERT」承接。
///
/// 断言两件事：① 不再是 20104；② 走完链后 L1 名下恰好一条活跃行。
#[tokio::test]
async fn manager_can_write_template_via_confirm_endpoint() {
    use crate::upload;
    let ctx = bootstrap().await;
    let bytes = minimal_xlsx_bytes("manager confirm ok");

    let (status, env) = upload(&ctx, ctx.l1_id, "经理模板.xlsx", &bytes, &ctx.manager_token).await;
    assert_eq!(status, StatusCode::OK, "Manager 应能经 confirm 落库：{env}");
    assert_eq!(env["data"]["kind"], "DELIVERY_TEMPLATE");
    assert_eq!(env["data"]["owner_kind"], "CUSTOMER");
    let rows = rows_for_owner_kind(&ctx.pool, ctx.l1_id, "DELIVERY_TEMPLATE").await;
    assert_eq!(rows.len(), 1, "L1 名下应恰好一条模板行：{rows:?}");
    assert!(!rows[0].1, "该行必须是活跃行：{rows:?}");
}

// ===========================================================================
//  8b. 通用读端点的越权面（2026-10-16 review 第 1 轮 MAJOR-2 / MAJOR-3）
// ===========================================================================

/// ★ CLERK 打 `GET /api/v2/files/objects/{模板 id}/url` ⇒ **21102**（不是预签直链）。
///
/// ⚠️ `url` 与 `content` 是同一个「按 id 拿字节」动作，守卫分别是权限点
/// `part.drawing.print`（默认矩阵含 CLERK / INSPECTOR / CNC）与本域 5 角色。
/// 2026-10-16 首版只给 `content` 加了 kind 白名单 ⇒ CLERK / INSPECTOR / CNC
/// 能拿到任意客户送货单打印模板的 **1 小时 COS 预签直链**（拿到即可脱离本后端
/// 直接下载），完全绕过 content 侧那道守卫。现两条共用
/// `policy::is_file_content_kind`。
#[tokio::test]
async fn clerk_cannot_get_delivery_template_presigned_url() {
    use hsh_erp_test_support::{json_request, send};

    let ctx = bootstrap().await;
    let id = seed_template(&ctx, ctx.l1_id).await;

    // 对照组：同一个 token 打**图纸**行的同一条端点 ⇒ 200（证明守卫判的是 kind
    // 而不是「所有 file_id 都 21102」）。DRAWING 行由直插 helper 造。
    let part_id = crate::insert_part_for_drawing(&ctx).await;
    let drawing_id = crate::insert_drawing_row(&ctx, part_id).await;

    let (status, env) = send(
        ctx.app.clone(),
        json_request(
            "GET",
            &format!("/files/objects/{id}/url"),
            None,
            Some(&ctx.clerk_token),
        ),
    )
    .await;
    assert_biz_code(status, &env, 21102, StatusCode::BAD_REQUEST);

    let (status, env) = send(
        ctx.app.clone(),
        json_request(
            "GET",
            &format!("/files/objects/{drawing_id}/url"),
            None,
            Some(&ctx.clerk_token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "图纸行同端点仍应放行：{env}");
    assert!(
        env["data"]["download_url"].as_str().is_some(),
        "对照组应拿到预签直链：{env}"
    );
}

/// ★ SHELF_ACCOUNT 打 `GET /api/v2/files/objects`（通用列表）**列不出**模板元数据。
///
/// ⚠️ 出参含 `owner_id`(= 一级客户 id) / `object_key` / `original_filename` /
/// `content_sha256` —— 那正是「按 id 取字节」越权链的**发现**环节。2026-10-16 首版
/// 不传 `kind` 就没 kind 谓词 ⇒ 工控机账号能列出全部客户的模板行（review 第 1 轮
/// MAJOR-3）。现 `PartFileService::list_files` **无条件**排除 `DELIVERY_TEMPLATE`。
#[tokio::test]
async fn shelf_account_list_never_sees_delivery_template_rows() {
    use hsh_erp_test_support::{json_request, send};

    let ctx = bootstrap().await;
    let id = seed_template(&ctx, ctx.l1_id).await;
    seed_template(&ctx, ctx.other_l1_id).await;

    // 缺省（不带 kind）⇒ 0 条
    let (status, env) = send(
        ctx.app.clone(),
        json_request("GET", "/files/objects", None, Some(&ctx.shelf_token)),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{env}");
    assert_eq!(env["data"]["total"], 0, "模板行不得进通用列表：{env}");

    // ⚠️ 显式 `?kind=DELIVERY_TEMPLATE` **也**是 0 条（只在缺省时排除的话，
    // 攻击者补一个 kind 参数就绕回来了 —— `kind` 是自由文本、不做枚举校验）。
    let (status, env) = send(
        ctx.app.clone(),
        json_request(
            "GET",
            "/files/objects?kind=DELIVERY_TEMPLATE",
            None,
            Some(&ctx.shelf_token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{env}");
    assert_eq!(env["data"]["total"], 0, "显式指定 kind 也不得放行：{env}");

    // 对照组：SHELF_ACCOUNT 仍能列**图纸**行（不能把这条端点整体关死）
    let part_id = crate::insert_part_for_drawing(&ctx).await;
    let drawing_id = crate::insert_drawing_row(&ctx, part_id).await;
    let (status, env) = send(
        ctx.app.clone(),
        json_request(
            "GET",
            &format!("/files/objects?owner_id={part_id}"),
            None,
            Some(&ctx.shelf_token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{env}");
    let ids: Vec<&str> = env["data"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|it| it["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec![drawing_id.to_string()], "只应出图纸行：{env}");
    assert!(ids.iter().all(|s| *s != id.to_string()));
}

/// ★ `SHELF_ACCOUNT` 打 `GET /api/v2/files/objects/{模板 id}/content` ⇒ **21102**。
///
/// 这是 2026-10-16 首版就有的那道守卫（工控机账号不能读客户的送货单抬头），
/// 本轮把它与 `/url` 并成同一张 `policy::is_file_content_kind` 白名单后重新钉一遍，
/// 防止「提 predicate 时把 content 侧漏掉」。
#[tokio::test]
async fn shelf_account_cannot_get_delivery_template_bytes() {
    let ctx = bootstrap().await;
    let id = seed_template(&ctx, ctx.l1_id).await;

    let (status, _, _) = fetch_content(&ctx, id, &ctx.shelf_token).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "content 端点本就 403（角色）"
    );

    // 换成「有 content 权限的角色」打同一条路 ⇒ 角色过了、被 kind 白名单拦下。
    // 用 CLERK（本域 5 角色之一）走 files 域的 content 端点（不是本域的）。
    use hsh_erp_test_support::{json_request, send};
    let (status, env) = send(
        ctx.app.clone(),
        json_request(
            "GET",
            &format!("/files/objects/{id}/content"),
            None,
            Some(&ctx.clerk_token),
        ),
    )
    .await;
    assert_biz_code(status, &env, 21102, StatusCode::BAD_REQUEST);
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

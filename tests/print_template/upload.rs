//! 模板写入链路（2026-10-15 重写）：`POST /files/objects/upload-intents` →
//! 前端直传 COS → `POST /files/objects/confirm`
//!
//! ## 为什么本文件还在
//!
//! 本域**没有**上传端点了（`POST /print-templates` 已随统一链路下线，见
//! `src/modules/print_template/mod.rs`），但「模板行该长什么样」这件事仍是本域的
//! 核心契约 —— 它复用 `t_part_file`（不新建表），所以「哪个 kind / 哪个 owner 列装
//! 什么 / file_type 怎么推 / 替换语义怎么落」都归这里钉。
//!
//! ## 覆盖
//! 1. ★ `upload_happy_path_writes_delivery_template_row` —— `t_part_file` 行的
//!    `object_key` / `original_filename` / `file_size` / `content_type` 四列逐项对上
//! 2. ★ `upload_twice_for_same_l1_replaces_old_row` —— 替换：旧行 `deleted_at`
//!    非空、唯一索引不炸、`resolve` 只命中新的一份
//! 3. ★ `upload_to_l2_customer_returns_21105` —— 只绑一级客户
//! 4. ★ `clerk_cannot_upload_returns_403` —— 经 upload-intents 与经 confirm
//!    **两条**通用写入口都造不出模板行
//! 5. `upload_rejects_non_xlsx_extension` / `upload_rejects_non_octet_stream_content_type`
//!    —— 扩展名 / content_type 两道格式闸门
//! 6. `upload_to_nonexistent_customer_returns_21105`
//! 7. `upload_to_two_different_l1_keeps_both` —— 两个 L1 各一份，互不干扰
//! 8. `upload_without_token_returns_401`
//! 9. ★ `non_zip_xlsx_is_no_longer_rejected_by_backend` —— **有意弱化**：ZIP magic
//!    校验随 multipart 端点下线而消失（后端只有 head、拿不到字节），职责移交前端。
//!    这条把「后端不再拒绝」**钉成事实**，别让后人误以为漏了校验又补回来。

use axum::http::StatusCode;

use hsh_erp_rust::util::cos_key;

use hsh_erp_rust::infra::cos::CosClient;

use crate::{
    assert_biz_code, bootstrap, confirm, minimal_xlsx_bytes, rows_for_owner_kind, upload,
    upload_intents,
};

/// 端点 1 的路径（列表端点同前缀，但 method 不同）。
const LIST_URI: &str = "/print-templates";

// ===========================================================================
//  1. happy path
// ===========================================================================

/// ★ 链路走完 ⇒ `t_part_file` 行的**四个关键列**逐项对上。
///
/// 钉的是「**复用 `t_part_file` 而不新建表**」这个决策的**可观测形态**：
/// - `kind = 'DELIVERY_TEMPLATE'`；
/// - `part_id = <L1 客户 id>`（⚠️ 列名叫 `part_id`，装的是 `t_customer.id`
///   —— 库里无法区分「零件 123 的图纸」与「客户 123 的模板」）；
/// - `object_key` 由 `util::cos_key::build_cas_key` 生成（内容寻址：
///   `{prefix}{sha16}_{safe_filename}`）；
/// - `original_filename` / `file_size` / `content_type` 与前端声明逐字一致。
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

    assert_eq!(status, StatusCode::OK, "confirm 成功是 200：{env}");
    assert_eq!(env["code"], 0, "{env}");
    let d = &env["data"];
    assert_eq!(d["kind"], "DELIVERY_TEMPLATE");
    assert_eq!(d["file_type"], "XLSX");
    assert_eq!(d["owner_kind"], "CUSTOMER");
    assert_eq!(d["owner_id"], ctx.l1_id.to_string());
    assert_eq!(d["upload_status"], "READY");

    let rows = rows_for_owner_kind(&ctx.pool, ctx.l1_id, "DELIVERY_TEMPLATE").await;
    assert_eq!(rows.len(), 1, "应恰好落一行：{rows:?}");
    let (_id, deleted, file_type, object_key, _file_size, _v) = &rows[0];
    assert!(!*deleted, "新行必须是活跃行");
    assert_eq!(*file_type, "XLSX", "file_type 由扩展名推导");

    // object_key：内容寻址，前 16 位取 sha
    let sha = hsh_erp_rust::modules::files::part_file::repo::hash_bytes(&bytes);
    let expected = cos_key::build_cas_key("uploads", &sha, "甲公司送货单模板.xlsx");
    assert_eq!(
        *object_key, expected,
        "object_key 必须是 build_cas_key 派生的 CAS key"
    );
    assert!(
        ctx.cos.stored_bytes(object_key).is_some(),
        "字节应由「前端直传」落到那个正式 key 上"
    );

    let full: hsh_erp_rust::modules::files::part_file::model::TPartFile = {
        let mut conn = ctx.pool.acquire().await.unwrap();
        let mut repo: &mut sqlx::PgConnection = &mut conn;
        hsh_erp_rust::modules::files::part_file::repo::PartFileRepoTrait::get_by_id(
            &mut repo,
            d["id"].as_str().unwrap().parse().unwrap(),
            false,
        )
        .await
        .unwrap()
        .unwrap()
    };
    assert_eq!(
        full.original_filename, "甲公司送货单模板.xlsx",
        "original_filename 逐字保留前端声明"
    );
    assert_eq!(
        full.file_size,
        bytes.len() as i64,
        "file_size 是前端声明的长度"
    );
    assert_eq!(
        full.content_type,
        crate::XLSX_MIME,
        "content_type 是签发时回给前端、PUT 时原样带上来的那个"
    );
}

// ===========================================================================
//  2. 替换语义
// ===========================================================================

/// ★ 同 L1 二次上传 = **替换**：旧行 `deleted_at` 非空、活跃行恰好一条、
/// `resolve` 只命中新的一份。
#[tokio::test]
async fn upload_twice_for_same_l1_replaces_old_row() {
    let ctx = bootstrap().await;

    let (s1, e1) = upload(
        &ctx,
        ctx.l1_id,
        "模板v1.xlsx",
        &minimal_xlsx_bytes("v1"),
        &ctx.manager_token,
    )
    .await;
    assert_eq!(s1, StatusCode::OK, "{e1}");
    let first_id = e1["data"]["id"].as_str().unwrap().to_string();

    let (s2, e2) = upload(
        &ctx,
        ctx.l1_id,
        "模板v2.xlsx",
        &minimal_xlsx_bytes("v2"),
        &ctx.manager_token,
    )
    .await;
    assert_eq!(s2, StatusCode::OK, "替换不是错误：{e2}");
    let second_id = e2["data"]["id"].as_str().unwrap().to_string();
    assert_ne!(first_id, second_id, "替换应落新行（新 id）");

    let rows = rows_for_owner_kind(&ctx.pool, ctx.l1_id, "DELIVERY_TEMPLATE").await;
    assert_eq!(rows.len(), 2, "两行都在库（一条已软删）：{rows:?}");
    assert!(
        rows[0].1,
        "旧行 deleted_at 非空（uk_t_part_file_single 的替换落点）"
    );
    assert!(!rows[1].1, "新行必须活跃：{rows:?}");

    // resolve 只命中新的一份
    let (status, env) = crate::resolve(&ctx, ctx.l1_id, &ctx.manager_token).await;
    assert_eq!(status, StatusCode::OK, "{env}");
    assert_eq!(
        env["data"]["template"]["id"].as_str().unwrap(),
        second_id,
        "resolve 必须返回新模板：{env}"
    );
    assert_eq!(env["data"]["template"]["original_filename"], "模板v2.xlsx");
}

// ===========================================================================
//  3. owner 闸门
// ===========================================================================

/// ★ 绑**二级**客户被拒 ⇒ **21105**（HTTP 404）。
///
/// ⚠️ 2026-10-15 起错误码是 **21105**（OWNER_NOT_FOUND）而不是 20104：上传端点
/// 下线后模板写入只剩两条通用写入口，L1-only 判据在
/// `PartFileRepoTrait::customer_owner_exists` 里，代码统一。
#[tokio::test]
async fn upload_to_l2_customer_returns_21105() {
    let ctx = bootstrap().await;
    let bytes = minimal_xlsx_bytes("bind l2");
    let (status, env) = upload(&ctx, ctx.l2_id, "绑二级.xlsx", &bytes, &ctx.manager_token).await;
    assert_biz_code(status, &env, 21105, StatusCode::NOT_FOUND);
    assert!(
        rows_for_owner_kind(&ctx.pool, ctx.l2_id, "DELIVERY_TEMPLATE")
            .await
            .is_empty(),
        "绑二级被拒 ⇒ L2 名下不得有任何行：{env}"
    );
}

#[tokio::test]
async fn upload_to_nonexistent_customer_returns_21105() {
    let ctx = bootstrap().await;
    let bytes = minimal_xlsx_bytes("no such customer");
    let (status, env) = upload(
        &ctx,
        9_999_999_999_999,
        "查无此客.xlsx",
        &bytes,
        &ctx.manager_token,
    )
    .await;
    assert_biz_code(status, &env, 21105, StatusCode::NOT_FOUND);
}

// ===========================================================================
//  4. 角色闸门：两条通用写入口都要拒 CLERK
// ===========================================================================

/// ★ CLERK 经 `upload-intents` ⇒ 403，且库里无行。
#[tokio::test]
async fn clerk_cannot_upload_returns_403() {
    let ctx = bootstrap().await;
    let bytes = minimal_xlsx_bytes("clerk upload");
    let (status, env) =
        upload_intents(&ctx, ctx.l1_id, "文员模板.xlsx", &bytes, &ctx.clerk_token).await;
    assert_biz_code(status, &env, 40300, StatusCode::FORBIDDEN);
    assert!(
        rows_for_owner_kind(&ctx.pool, ctx.l1_id, "DELIVERY_TEMPLATE")
            .await
            .is_empty(),
        "闸门必须落在写入之前：{env}"
    );
}

/// ★ CLERK 经 **confirm**（第二条写入口）同样造不出模板行 ⇒ 403，且零副作用。
///
/// ⚠️ 这条与上一条**同等重要**：两条入口的角色闸门共用
/// `policy::write_roles_for`，少任一条就等于给 CLERK 留了一条绕行路。
#[tokio::test]
async fn clerk_cannot_confirm_returns_403() {
    use crate::XLSX_MIME;
    let ctx = bootstrap().await;
    let sha = "c".repeat(64);
    let object_key = cos_key::build_cas_key("uploads", &sha, "文员确认.xlsx");
    // 桶里**真的**有这个对象（PUT 已发生）—— 仍必须被角色闸门拦在写入之前
    ctx.cos
        .put_object(&object_key, minimal_xlsx_bytes("clerk confirm"), XLSX_MIME)
        .await
        .unwrap();
    let (status, env) = confirm(
        &ctx,
        ctx.l1_id,
        "文员确认.xlsx",
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
        "confirm 的角色闸门同样必须早于写入：{env}"
    );
}

// ===========================================================================
//  5. 格式闸门（扩展名 / content_type）
// ===========================================================================

/// ★ 扩展名非 xlsx（`.xls`）⇒ 21102（签发阶段就拒）。
#[tokio::test]
async fn upload_rejects_non_xlsx_extension() {
    let ctx = bootstrap().await;
    let bytes = minimal_xlsx_bytes("wrong ext");
    let (status, env) =
        upload_intents(&ctx, ctx.l1_id, "模板.xls", &bytes, &ctx.manager_token).await;
    assert_biz_code(status, &env, 21102, StatusCode::BAD_REQUEST);
}

/// ★ content_type 不是 OOXML 精确 MIME ⇒ 21102。
#[tokio::test]
async fn upload_rejects_wrong_content_type() {
    use hsh_erp_test_support::{json_request, send};
    let ctx = bootstrap().await;
    let bytes = minimal_xlsx_bytes("wrong ct");
    let sha = hsh_erp_rust::modules::files::part_file::repo::hash_bytes(&bytes);
    let body = serde_json::json!({
        "owner_kind": "CUSTOMER",
        "owner_id": ctx.l1_id.to_string(),
        "items": [{
            "client_ref": "ct",
            "kind": "DELIVERY_TEMPLATE",
            "original_filename": "模板.xlsx",
            "content_type": "application/octet-stream",
            "file_size": bytes.len().to_string(),
            "content_sha256": sha,
        }],
    });
    let (status, env) = send(
        ctx.app.clone(),
        json_request(
            "POST",
            "/files/objects/upload-intents",
            Some(body),
            Some(&ctx.manager_token),
        ),
    )
    .await;
    assert_biz_code(status, &env, 21102, StatusCode::BAD_REQUEST);
}

/// ★ **有意弱化**：不是 ZIP 的 `.xlsx` 现在**不再被后端拒绝**。
///
/// ⚠️ 统一链路后 confirm 只拿到 `head`（size / etag）、**拿不到字节** ⇒ ZIP magic
/// 判据在后端物理上无法回答。该职责移交前端（选文件时用 hucre 逐格解析）。
/// 这条把「后端不再拒绝」**钉成事实**：将来若有人想在 confirm 里加结构校验，
/// 必须先与前端 `assertTemplateMatches` 对齐判据清单，而不是随手加一道。
/// 取舍全文见 `src/modules/print_template/dto.rs` 末尾与
/// `docs/api/print_template.md`。
#[tokio::test]
async fn non_zip_xlsx_is_no_longer_rejected_by_backend() {
    let ctx = bootstrap().await;
    // ⚠️ 故意**不是** zip（无 PK\\x03\\x04 文件头）
    let bytes = b"this is definitely not a zip container".to_vec();
    let (status, env) = upload(&ctx, ctx.l1_id, "假模板.xlsx", &bytes, &ctx.manager_token).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "ZIP magic 校验已移交前端，后端不该再拒：{env}"
    );
    let rows = rows_for_owner_kind(&ctx.pool, ctx.l1_id, "DELIVERY_TEMPLATE").await;
    assert_eq!(
        rows.len(),
        1,
        "行仍然落库（内容合法性由前端保证）：{rows:?}"
    );
}

// ===========================================================================
//  6./7. 多客户并存
// ===========================================================================

/// ★ 两个不同 L1 各一份模板，互不干扰。
#[tokio::test]
async fn upload_to_two_different_l1_keeps_both() {
    let ctx = bootstrap().await;
    for (cid, tag) in [(ctx.l1_id, "甲"), (ctx.other_l1_id, "乙")] {
        let (status, env) = upload(
            &ctx,
            cid,
            &format!("{tag}公司模板.xlsx"),
            &minimal_xlsx_bytes(tag),
            &ctx.manager_token,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{tag}：{env}");
        assert_eq!(
            rows_for_owner_kind(&ctx.pool, cid, "DELIVERY_TEMPLATE")
                .await
                .len(),
            1,
            "{tag} 名下应恰好一行"
        );
    }
}

// ===========================================================================
//  8. 鉴权
// ===========================================================================

#[tokio::test]
async fn upload_without_token_returns_401() {
    let ctx = bootstrap().await;
    let bytes = minimal_xlsx_bytes("no token");
    let (status, env) = upload_intents(&ctx, ctx.l1_id, "无token.xlsx", &bytes, "").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "无 token 应 40100：{env}");
    assert_eq!(env["code"], 40100);
}

/// 硬切守卫：`POST /print-templates`（旧 multipart 上传端点）不再被路由，**无 alias**。
///
/// ⚠️ 断言的是 **405 Method Not Allowed 而不是 404**：路径 `/print-templates` 本身
/// 仍然存在（列表端点 `GET /print-templates` 挂在同一个 `.route("/", get(h))` 上），
/// matchit 先判路径、再判方法 ⇒ 只注册了 GET 的路径收到 POST 就是 405。
/// 同款先例见 `src/modules/part/handler/batch.rs` 模块 doc（「1 段静态路径落进
/// catch-all 有两种落点」）与 `tests/part/create_serial_price.rs` 的
/// `batch_with_pdfs_endpoint_is_gone_and_old_paths_are_no_longer_routed`。
///
/// 对照组：`GET /print-templates` 仍 200（证明「405 是方法不匹配、不是整段前缀没了」）。
#[tokio::test]
async fn old_upload_endpoint_is_gone_and_list_still_works() {
    use hsh_erp_test_support::{json_request, send_raw};
    let ctx = bootstrap().await;

    let (status, raw) = send_raw(
        ctx.app.clone(),
        json_request("POST", LIST_URI, None, Some(&ctx.manager_token)),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::METHOD_NOT_ALLOWED,
        "旧上传端点必须不再被路由（路径仍在，故是 405 不是 404）：{status} {raw}"
    );

    let (status, env) = crate::list(&ctx, "", &ctx.manager_token).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "对照组：同一路径的 GET 仍可用（证明 405 来自方法不匹配）：{env}"
    );
}

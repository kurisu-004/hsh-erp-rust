//! 端点 5 `POST /print-templates/{id}/delete`（OCC）+ 端点 2 `GET /print-templates`（列表）
//!
//! 覆盖 7 组场景（★ = 核心回归）：
//! 1. ★ `delete_without_version_field_returns_422_plain_text` —— OCC 锚点缺失 ⇒
//!    **HTTP 422 纯文本**（不是 `R<T>` 信封；CLAUDE.md §8 铁律）
//! 2. ★ `delete_with_stale_version_returns_40901`
//! 3. ★ `delete_then_resolve_returns_21109`
//! 4. ★ `delete_twice_returns_21101_not_409` —— 第二次删用已自增后的正确 version
//!    也拿不到 409：软删闸门让 `get_by_id(include_deleted=false)` 直接落 21101 / 404
//! 5. ★ `delete_spawns_cos_cleanup_after_commit`
//! 6. `clerk_cannot_delete_returns_403`
//! 7. ★ `list_supports_pagination_and_customer_filter`

use axum::http::StatusCode;

use hsh_erp_test_support::{json_request, send_raw};

use crate::{
    Ctx, assert_biz_code, bootstrap, delete, list, minimal_xlsx_bytes, rows_for_owner_kind, upload,
};

/// 造一份「上传成功」的模板，返回 `(id, bytes)`。
async fn seed(ctx: &Ctx, customer_id: i64, tag: &str) -> (i64, Vec<u8>) {
    let bytes = minimal_xlsx_bytes(tag);
    let (st, env) = upload(ctx, customer_id, "待删除.xlsx", &bytes, &ctx.manager_token).await;
    assert_eq!(st, StatusCode::CREATED, "seed 上传失败：{env}");
    let id = env["data"]["id"].as_str().unwrap().parse::<i64>().unwrap();
    let version = env["data"]["version"].as_i64().unwrap() as i32;
    assert_eq!(version, 0, "新行 version 从 0 起");
    (id, bytes)
}

// ===========================================================================
//  1. 缺 version ⇒ 422 纯文本
// ===========================================================================

/// ★ body 缺 `version` ⇒ **HTTP 422 纯文本**（axum `Json` 提取器），**不是**业务信封。
///
/// 断言点：body 不是 JSON（解析必失败）⇒ 说明前端拿不到 `{code, message, data}`，
/// 只能当成「请求体不合法」处理。`DeleteTemplateRequest.version` 上刻意**没有**
/// `#[serde(default)]` 就是为了守住这一条。
#[tokio::test]
async fn delete_without_version_field_returns_422_plain_text() {
    let ctx = bootstrap().await;
    let (id, _) = seed(&ctx, ctx.l1_id, "occ missing").await;

    let (status, raw) = send_raw(
        ctx.app.clone(),
        json_request(
            "POST",
            &format!("/print-templates/{id}/delete"),
            Some(serde_json::json!({})),
            Some(&ctx.manager_token),
        ),
    )
    .await;

    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "body={raw}");
    assert!(
        serde_json::from_str::<serde_json::Value>(&raw).is_err(),
        "422 必须是**纯文本**（不是 R 信封），实际 body={raw}"
    );
    assert!(!raw.contains("\"code\""), "422 body 不该带业务 code：{raw}");

    // 取证：不得留下任何副作用
    let rows = rows_for_owner_kind(&ctx.pool, ctx.l1_id, "DELIVERY_TEMPLATE").await;
    assert_eq!(rows.len(), 1);
    assert!(!rows[0].1, "被 422 拒的请求不得软删模板");
}

// ===========================================================================
//  2. 陈旧 version ⇒ 40901
// ===========================================================================

/// ★ `version` 与库里不符 ⇒ HTTP 409 + `40901 VERSION_CONFLICT`。
#[tokio::test]
async fn delete_with_stale_version_returns_40901() {
    let ctx = bootstrap().await;
    let (id, _) = seed(&ctx, ctx.l1_id, "occ stale").await;

    let (status, env) = delete(
        &ctx,
        id,
        serde_json::json!({"version": 99}),
        &ctx.manager_token,
    )
    .await;
    assert_biz_code(status, &env, 40901, StatusCode::CONFLICT);

    let rows = rows_for_owner_kind(&ctx.pool, ctx.l1_id, "DELIVERY_TEMPLATE").await;
    assert!(!rows[0].1, "409 冲突不得软删模板");
}

// ===========================================================================
//  3. 删完 resolve ⇒ 21109
// ===========================================================================

/// ★ 删成功后 `resolve` ⇒ `21109` / HTTP 422（解绑即「没绑」）。
#[tokio::test]
async fn delete_then_resolve_returns_21109() {
    let ctx = bootstrap().await;
    let (id, _) = seed(&ctx, ctx.l1_id, "delete then resolve").await;

    let (status, env) = delete(
        &ctx,
        id,
        serde_json::json!({"version": 0}),
        &ctx.manager_token,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{env}");

    let (status, env) = crate::resolve(&ctx, ctx.l1_id, &ctx.manager_token).await;
    assert_biz_code(status, &env, 21109, StatusCode::UNPROCESSABLE_ENTITY);
}

/// 删除后行仍在库里（软删，非物理删）且 `version` 自增 1 —— 审计行留存。
#[tokio::test]
async fn delete_soft_deletes_and_bumps_version() {
    let ctx = bootstrap().await;
    let (id, _) = seed(&ctx, ctx.l1_id, "soft delete bump").await;

    let (status, env) = delete(
        &ctx,
        id,
        serde_json::json!({"version": 0}),
        &ctx.manager_token,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{env}");

    let rows = rows_for_owner_kind(&ctx.pool, ctx.l1_id, "DELIVERY_TEMPLATE").await;
    assert_eq!(rows.len(), 1, "行必须留存（软删不是物理删）");
    assert!(rows[0].1, "deleted_at 必须非空");
    assert_eq!(rows[0].5, 1, "version 必须自增 0 → 1");
}

// ===========================================================================
//  4. 重复删除
// ===========================================================================

/// ★ 重复删除（第二次带**已自增后**的正确 version）仍 ⇒ `21101` / 404，**不是** 40901。
///
/// `PartFileService::soft_delete_file` 先 `get_by_id(id, false)`（软删闸门），
/// 第二次就查不到了 ⇒ 走 `21101 BIZ_PART_FILE_NOT_FOUND`（HTTP 404）。
/// 钉这条是因为「已删 + 正确 version」在直觉上是 409，**实际是 404** ——
/// 前端不能靠 code 区分「冲突」与「不存在」，只能靠「重试前刷新列表」。
#[tokio::test]
async fn delete_twice_returns_21101_not_409() {
    let ctx = bootstrap().await;
    let (id, _) = seed(&ctx, ctx.l1_id, "delete twice").await;

    let (st, env) = delete(
        &ctx,
        id,
        serde_json::json!({"version": 0}),
        &ctx.manager_token,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{env}");

    let (status, env) = delete(
        &ctx,
        id,
        serde_json::json!({"version": 1}),
        &ctx.manager_token,
    )
    .await;
    assert_biz_code(status, &env, 21101, StatusCode::NOT_FOUND);
}

// ===========================================================================
//  5. COS 清理在 commit 之后
// ===========================================================================

/// ★ 删除后 COS 对象被清掉（handler 在 `tx.commit()` **之后** spawn
/// `cos.delete_object`，files 域 2026-09-15 review A2 纪律）。
///
/// ⚠️ spawn 是异步的 ⇒ 这里**轮询**一小段（上限 ~2s）而不是直接断言。
/// 钉的是「确实删了」，不是「顺序」—— 顺序那条由代码注释 + review 保证。
#[tokio::test]
async fn delete_spawns_cos_cleanup_after_commit() {
    let ctx = bootstrap().await;
    let (id, bytes) = seed(&ctx, ctx.l1_id, "cos cleanup").await;

    // 先取一次 object_key（经出参拿不到 —— 刻意不出参，故查库）
    let rows = rows_for_owner_kind(&ctx.pool, ctx.l1_id, "DELIVERY_TEMPLATE").await;
    let object_key = rows[0].3.clone();
    assert_eq!(
        ctx.cos.stored_bytes(&object_key).as_deref(),
        Some(bytes.as_slice()),
        "删除前对象应在"
    );

    let (status, env) = delete(
        &ctx,
        id,
        serde_json::json!({"version": 0}),
        &ctx.manager_token,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{env}");

    let mut gone = false;
    for _ in 0..40 {
        if ctx.cos.stored_bytes(&object_key).is_none() {
            gone = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(
        gone,
        "commit 之后应 spawn cos.delete_object 清理对象 {object_key}"
    );
}

// ===========================================================================
//  6. 角色守卫
// ===========================================================================

/// CLERK 删除 ⇒ **403**，且模板仍是活跃态。
#[tokio::test]
async fn clerk_cannot_delete_returns_403() {
    let ctx = bootstrap().await;
    let (id, _) = seed(&ctx, ctx.l1_id, "clerk delete").await;

    let (status, env) = delete(
        &ctx,
        id,
        serde_json::json!({"version": 0}),
        &ctx.clerk_token,
    )
    .await;
    assert_biz_code(status, &env, 40300, StatusCode::FORBIDDEN);

    let rows = rows_for_owner_kind(&ctx.pool, ctx.l1_id, "DELIVERY_TEMPLATE").await;
    assert!(!rows[0].1, "403 不得软删模板");
}

// ===========================================================================
//  7. 列表端点
// ===========================================================================

/// ★ 列表分页 + `?customer_id=` 过滤；`total` 不受 `limit` 截断影响。
#[tokio::test]
async fn list_supports_pagination_and_customer_filter() {
    let ctx = bootstrap().await;
    // 甲客户 2 份（第二次是替换 ⇒ 仍是 1 活跃 + 1 软删，列表只出活跃那 1 条）
    seed(&ctx, ctx.l1_id, "list A1").await;
    let b = minimal_xlsx_bytes("list A2");
    upload(&ctx, ctx.l1_id, "第二版.xlsx", &b, &ctx.manager_token).await;
    // 乙客户 1 份
    seed(&ctx, ctx.other_l1_id, "list B1").await;

    // 全部（列表只看**活跃**行 ⇒ 甲 1 条 + 乙 1 条）
    let (status, env) = list(&ctx, "", &ctx.manager_token).await;
    assert_eq!(status, StatusCode::OK, "{env}");
    assert_eq!(env["data"]["total"], 2, "软删行不出现在列表：{env}");
    assert_eq!(env["data"]["items"].as_array().unwrap().len(), 2);

    // 按 customer_id 过滤
    let (status, env) = list(
        &ctx,
        &format!("?customer_id={}", ctx.other_l1_id),
        &ctx.manager_token,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{env}");
    assert_eq!(env["data"]["total"], 1, "{env}");
    assert_eq!(
        env["data"]["items"][0]["customer_id"],
        ctx.other_l1_id.to_string()
    );

    // 分页：limit=1 ⇒ items 1 条但 total 仍是该过滤下的总数
    let (status, env) = list(
        &ctx,
        &format!("?customer_id={}&limit=1", ctx.l1_id),
        &ctx.manager_token,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{env}");
    assert_eq!(env["data"]["items"].as_array().unwrap().len(), 1);
    assert_eq!(env["data"]["total"], 1, "甲客户当前只有 1 条活跃：{env}");

    // offset 越界 ⇒ items 空但 total 不变
    let (status, env) = list(
        &ctx,
        &format!("?customer_id={}&limit=1&offset=99", ctx.l1_id),
        &ctx.manager_token,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{env}");
    assert!(env["data"]["items"].as_array().unwrap().is_empty(), "{env}");
    assert_eq!(env["data"]["total"], 1, "total 不受 offset 影响：{env}");
}

/// ★ 列表**只出** `DELIVERY_TEMPLATE` 行 —— 零件图纸 / 装配件整图不得混进来。
///
/// ⚠️ 这是本域最重要的一条读侧隔离：`PartFileRepoTrait::list_with_filters` 的
/// `owner_kind` 形参**不进 SQL**（表上没有 owner 列），所以 `kind` 谓词是
/// **唯一**的隔离依据。漏传就会把别人的图纸列进「模板管理」页。
#[tokio::test]
async fn list_excludes_non_template_rows() {
    let ctx = bootstrap().await;
    seed(&ctx, ctx.l1_id, "only template").await;

    // 造一行同 owner id 的 DRAWING（零件号恰好等于客户号，制造最坏情况）
    sqlx::query(
        "INSERT INTO t_part_file (id, part_id, kind, file_type, object_key, \
         original_filename, file_size, content_type, upload_status, content_sha256, \
         version, created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, 'DRAWING', 'PDF', 'uploads/aaaa.pdf', 'a.pdf', 1, \
                 'application/pdf', 'READY', NULL, 0, now(), 0, now(), 0)",
    )
    .bind(shared_id())
    .bind(ctx.l1_id)
    .execute(&ctx.pool)
    .await
    .expect("insert DRAWING");

    let (status, env) = list(&ctx, "", &ctx.manager_token).await;
    assert_eq!(status, StatusCode::OK, "{env}");
    assert_eq!(env["data"]["total"], 1, "DRAWING 行不得进模板列表：{env}");
    for item in env["data"]["items"].as_array().unwrap() {
        assert_eq!(item["kind"], "DELIVERY_TEMPLATE", "{item}");
    }
}

/// 列表端点是 **Manager 独占** ⇒ CLERK / INSPECTOR 都 403。
#[tokio::test]
async fn list_is_manager_only() {
    let ctx = bootstrap().await;
    seed(&ctx, ctx.l1_id, "list rbac").await;
    for token in [&ctx.clerk_token, &ctx.inspector_token, &ctx.shelf_token] {
        let (status, env) = list(&ctx, "", token).await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "列表对非 Manager 应 403：{env}"
        );
    }
}

/// 删除端点的 404 形态：删一个**不存在**的 id ⇒ `21101`（HTTP 404）。
#[tokio::test]
async fn delete_nonexistent_id_returns_21101() {
    let ctx = bootstrap().await;
    let (status, env) = delete(
        &ctx,
        -424_242,
        serde_json::json!({"version": 0}),
        &ctx.manager_token,
    )
    .await;
    assert_biz_code(status, &env, 21101, StatusCode::NOT_FOUND);
}

/// 取一个新雪花 id（直插 fixture 用）。
fn shared_id() -> i64 {
    hsh_erp_test_support::shared_test_snowflake().next_id()
}

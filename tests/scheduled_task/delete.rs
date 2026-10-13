//! `POST /api/v2/mail/accounts/{id}/delete`（软删 + 被引用禁删）契约。
//!
//! ★ 核心回归：**被 mail_digest 配置引用的邮箱禁止删除**（21708 + 409）——
//! 否则删完之后交期汇总配置的 `sender_account_id` 会指向一条死行，页面看着正常、
//! 直到定时任务真发信才以 21701 暴露。

use axum::http::StatusCode;
use serde_json::json;

use crate::{
    assert_biz_code, bootstrap, create_account, get_as_manager, id_of, post_as_manager, post_raw,
    reference_from_digest_config, stored_secret, unique_name,
};

/// 软删成功：行从列表里消失、库里 `deleted_at` 非空。
#[tokio::test]
async fn delete_soft_deletes_and_hides_from_list() {
    let ctx = bootstrap().await;
    let created = create_account(&ctx, &unique_name("待删")).await;
    let id = id_of(&created);

    let (status, env) = post_as_manager(
        &ctx,
        &format!("/mail/accounts/{id}/delete"),
        json!({ "version": created["version"] }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{env}");
    assert_eq!(env["code"], 0, "{env}");

    let (_, env) = get_as_manager(&ctx, "/mail/accounts").await;
    let items = env["data"]["items"].as_array().expect("items 是数组");
    assert!(
        !items.iter().any(|i| i["id"] == json!(id.to_string())),
        "软删后不得再出现在列表：{env}"
    );

    let deleted_at: Option<chrono::NaiveDateTime> =
        sqlx::query_scalar("SELECT deleted_at FROM t_mail_account WHERE id = $1")
            .bind(id)
            .fetch_one(&ctx.pool)
            .await
            .expect("读 deleted_at");
    assert!(deleted_at.is_some(), "软删是置 deleted_at，不是物理删行");
}

/// ★ 被交期汇总配置引用 ⇒ `21708` + HTTP 409，行**仍在**库里。
#[tokio::test]
async fn delete_referenced_account_returns_21708() {
    let ctx = bootstrap().await;
    let created = create_account(&ctx, &unique_name("被引用")).await;
    let id = id_of(&created);
    reference_from_digest_config(&ctx.pool, id).await;

    let (status, env) = post_as_manager(
        &ctx,
        &format!("/mail/accounts/{id}/delete"),
        json!({ "version": created["version"] }),
    )
    .await;
    assert_biz_code(status, &env, 21708, StatusCode::CONFLICT);

    // 行必须还在（拒删，不是「删了再说」）
    let sealed = stored_secret(&ctx.pool, id).await;
    assert!(!sealed.is_empty(), "被引用的邮箱不得被软删");
    let (_, env) = get_as_manager(&ctx, "/mail/accounts").await;
    assert!(
        env["data"]["items"]
            .as_array()
            .expect("items 是数组")
            .iter()
            .any(|i| i["id"] == json!(id.to_string())),
        "被引用的邮箱必须仍在列表：{env}"
    );
}

/// **已软删**的 mail_digest 配置**不**算引用（软删行既不发信也不被配置页读到，
/// 拿它当占用会让管理员删不掉一个实际上谁也没在用的邮箱）。
#[tokio::test]
async fn delete_allowed_when_only_soft_deleted_config_references_it() {
    let ctx = bootstrap().await;
    let created = create_account(&ctx, &unique_name("死配置引用")).await;
    let id = id_of(&created);
    reference_from_digest_config(&ctx.pool, id).await;
    sqlx::query("UPDATE t_mail_digest_config SET deleted_at = now() WHERE sender_account_id = $1")
        .bind(id)
        .execute(&ctx.pool)
        .await
        .expect("软删那份配置");

    let (status, env) = post_as_manager(
        &ctx,
        &format!("/mail/accounts/{id}/delete"),
        json!({ "version": created["version"] }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "已软删的配置不该算占用：{env}");
}

/// 软删吃 OCC：body 缺 `version` ⇒ 422 纯文本（不是业务信封）。
#[tokio::test]
async fn delete_without_version_returns_422_plain_text() {
    let ctx = bootstrap().await;
    let created = create_account(&ctx, &unique_name("删缺版本")).await;

    let (status, raw) = post_raw(
        &ctx,
        "POST",
        &format!("/mail/accounts/{}/delete", id_of(&created)),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{raw}");
    assert!(!raw.contains("\"code\""), "422 是纯文本：{raw}");
    assert!(raw.contains("version"), "422 应说清缺哪个字段：{raw}");
}

/// 软删不存在 / 已软删的邮箱 ⇒ `21706` + HTTP 404。
#[tokio::test]
async fn delete_missing_account_returns_21706() {
    let ctx = bootstrap().await;
    let (status, env) = post_as_manager(
        &ctx,
        "/mail/accounts/999999999999999/delete",
        json!({ "version": 0 }),
    )
    .await;
    assert_biz_code(status, &env, 21706, StatusCode::NOT_FOUND);
}

/// 软删后**同名可以再建**（两个部分唯一索引都以 `deleted_at IS NULL` 为前提）。
#[tokio::test]
async fn name_can_be_reused_after_soft_delete() {
    let ctx = bootstrap().await;
    let name = unique_name("删后同名");
    let created = create_account(&ctx, &name).await;
    let id = id_of(&created);

    let (status, env) = post_as_manager(
        &ctx,
        &format!("/mail/accounts/{id}/delete"),
        json!({ "version": created["version"] }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{env}");

    let again = create_account(&ctx, &name).await;
    assert_ne!(id_of(&again), id, "应是一条新行，不是复活旧行：{again}");
}

/// 软删**默认**邮箱不自动把默认位让给别的邮箱（唯一索引允许「零个默认」）。
///
/// 刻意如此：把默认自动转给某个「别的邮箱」需要一条选择规则（最近建的？第一个？），
/// 而任何一条规则都会让「我以为 A 是默认」在某天悄悄变成 B —— 比「没有默认」危险得多。
#[tokio::test]
async fn deleting_the_default_leaves_no_default() {
    let ctx = bootstrap().await;
    let a = create_account(&ctx, &unique_name("默认被删")).await;
    let _b = create_account(&ctx, &unique_name("候选替补")).await;

    let (status, env) = post_as_manager(
        &ctx,
        &format!("/mail/accounts/{}/set-default", id_of(&a)),
        json!({ "version": a["version"] }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{env}");

    let (status, env) = post_as_manager(
        &ctx,
        &format!("/mail/accounts/{}/delete", id_of(&a)),
        json!({ "version": env["data"]["version"] }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{env}");

    let (_, env) = get_as_manager(&ctx, "/mail/accounts").await;
    let defaults = env["data"]["items"]
        .as_array()
        .expect("items 是数组")
        .iter()
        .filter(|i| i["is_default"] == json!(true))
        .count();
    assert_eq!(defaults, 0, "删掉默认邮箱后不应把默认转给别的邮箱：{env}");
}

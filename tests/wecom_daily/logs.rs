//! 端点 5：`GET /logs`（分页 + 快照列 + 参数 clamp）

use axum::http::StatusCode;

use super::*;

/// 分页：`total` 不受 `limit` 截断；`offset` 生效。
#[tokio::test]
async fn pagination_total_is_not_truncated_by_limit() {
    let ctx = bootstrap().await;
    let base = now_naive();
    for i in 0..5 {
        insert_log_row(
            &ctx.pool,
            0,
            "SCHEDULED",
            "SUCCESS",
            "通道",
            1,
            1,
            base + chrono::Duration::minutes(i),
        )
        .await;
    }

    let (status, env) = get_as_manager(&ctx, "/cron/wecom-daily/logs?limit=2").await;
    assert_eq!(status, StatusCode::OK, "{env}");
    let d = data(&env);
    assert_eq!(d["total"].as_i64(), Some(5));
    assert_eq!(d["items"].as_array().unwrap().len(), 2);
    assert_eq!(d["limit"].as_i64(), Some(2));
    assert_eq!(d["offset"].as_i64(), Some(0));

    let (_, env2) = get_as_manager(&ctx, "/cron/wecom-daily/logs?limit=2&offset=4").await;
    assert_eq!(data(&env2)["items"].as_array().unwrap().len(), 1);
    assert_eq!(data(&env2)["total"].as_i64(), Some(5));
}

/// 排序：`sent_at DESC, id DESC`（同刻按 id 兜底，保证确定性）。
#[tokio::test]
async fn ordered_by_sent_at_desc_then_id_desc() {
    let ctx = bootstrap().await;
    let base = now_naive();
    // 三个**不同时刻**的行 + 一个与第 0 行同刻但后插入的行（id 更大 ⇒ 排前面）
    let mut same_time_id = 0i64;
    let mut ids = Vec::new();
    for i in 0..3 {
        let id = insert_log_row(
            &ctx.pool,
            0,
            "MANUAL",
            "SUCCESS",
            "通道",
            1,
            1,
            base + chrono::Duration::minutes(i),
        )
        .await;
        if i == 0 {
            same_time_id = id;
        }
        ids.push(id);
    }
    let later_same_time_id =
        insert_log_row(&ctx.pool, 0, "MANUAL", "SUCCESS", "通道", 1, 1, base).await;
    assert!(
        later_same_time_id > same_time_id,
        "同刻后插入的那行 id 更大（雪花单调）"
    );

    let (_, env) = get_as_manager(&ctx, "/cron/wecom-daily/logs?limit=10").await;
    let items = data(&env)["items"].as_array().unwrap();
    let got: Vec<i64> = items.iter().map(|r| id_str(&r["log_id"])).collect();
    // 期望顺序：base+2min、base+1min、base（同刻里 id 大的在前）、……
    assert_eq!(
        got,
        vec![ids[2], ids[1], later_same_time_id, same_time_id],
        "sent_at DESC, id DESC"
    );
}

/// 参数 clamp：`limit` ∈ [1,200]、`offset` ≥ 0。
#[tokio::test]
async fn limit_and_offset_are_clamped() {
    let ctx = bootstrap().await;
    insert_log_row(&ctx.pool, 0, "MANUAL", "SUCCESS", "通道", 1, 1, now_naive()).await;

    // limit=0 / 负数 ⇒ 夹到 1
    for raw in ["limit=0", "limit=-5"] {
        let (status, env) = get_as_manager(&ctx, &format!("/cron/wecom-daily/logs?{raw}")).await;
        assert_eq!(status, StatusCode::OK, "{raw}：{env}");
        assert_eq!(data(&env)["limit"].as_i64(), Some(1), "{raw}");
    }
    // 超大 limit ⇒ 夹到 200
    let (_, env) = get_as_manager(&ctx, "/cron/wecom-daily/logs?limit=99999").await;
    assert_eq!(data(&env)["limit"].as_i64(), Some(200));
    // 负 offset ⇒ 夹到 0
    let (_, env) = get_as_manager(&ctx, "/cron/wecom-daily/logs?offset=-3").await;
    assert_eq!(data(&env)["offset"].as_i64(), Some(0));
    // 缺省 limit = 20
    let (_, env) = get_as_manager(&ctx, "/cron/wecom-daily/logs").await;
    assert_eq!(data(&env)["limit"].as_i64(), Some(20));
    assert_eq!(data(&env)["offset"].as_i64(), Some(0));
}

/// 信封是 4 个键（`items` / `total` / `limit` / `offset`）。
#[tokio::test]
async fn log_list_envelope_has_four_keys() {
    let ctx = bootstrap().await;
    let (_, env) = get_as_manager(&ctx, "/cron/wecom-daily/logs").await;
    let d = data(&env);
    assert_eq!(d.as_object().expect("data 是对象").len(), 4, "{d}");
    assert!(d["items"].is_array());
    assert!(d["total"].is_number());
    assert!(d["limit"].is_number());
    assert!(d["offset"].is_number());
}

/// `chunk_count` / `chunk_sent` 快照原样读回（含「部分失败」那一档）。
#[tokio::test]
async fn chunk_columns_are_snapshot() {
    let ctx = bootstrap().await;
    let base = now_naive();
    insert_log_row(&ctx.pool, 3, "SCHEDULED", "SUCCESS", "通道", 4, 4, base).await;
    insert_log_row(
        &ctx.pool,
        3,
        "SCHEDULED",
        "FAILED",
        "通道",
        4,
        1,
        base + chrono::Duration::minutes(1),
    )
    .await;
    // 准备阶段失败：0/0 但仍是 FAILED
    insert_log_row(
        &ctx.pool,
        3,
        "SCHEDULED",
        "FAILED",
        "",
        0,
        0,
        base + chrono::Duration::minutes(2),
    )
    .await;

    let (_, env) = get_as_manager(&ctx, "/cron/wecom-daily/logs?limit=10").await;
    let items = data(&env)["items"].as_array().unwrap();
    assert_eq!(items.len(), 3);

    let top = &items[0];
    assert_eq!(top["chunk_count"].as_i64(), Some(0));
    assert_eq!(top["chunk_sent"].as_i64(), Some(0));
    assert_eq!(top["status"].as_str(), Some("FAILED"));
    assert_eq!(top["config_version"].as_i64(), Some(3));

    let partial = &items[1];
    assert_eq!(partial["chunk_count"].as_i64(), Some(4));
    assert_eq!(partial["chunk_sent"].as_i64(), Some(1));
    assert_eq!(partial["status"].as_str(), Some("FAILED"));
    assert!(partial["error_message"].as_str().is_some());

    let ok = &items[2];
    assert_eq!(ok["status"].as_str(), Some("SUCCESS"));
    assert_eq!(ok["chunk_count"].as_i64(), Some(4));
    assert_eq!(ok["chunk_sent"].as_i64(), Some(4));
    assert!(
        ok["error_message"].is_null(),
        "成功行的 error_message 恒为 null：{}",
        ok["error_message"]
    );
}

/// `worker_row_total` 逐行独立（fixture 故意让它随 `chunk_sent` 变，避免成为常量）。
#[tokio::test]
async fn worker_row_total_column_is_readable() {
    let ctx = bootstrap().await;
    let base = now_naive();
    insert_log_row(&ctx.pool, 0, "MANUAL", "SUCCESS", "通道", 2, 2, base).await;
    let (_, env) = get_as_manager(&ctx, "/cron/wecom-daily/logs").await;
    let row = &data(&env)["items"].as_array().unwrap()[0];
    assert_eq!(row["worker_row_total"].as_i64(), Some(6));
}

/// CLERK 越权。
#[tokio::test]
async fn clerk_cannot_read_logs() {
    let ctx = bootstrap().await;
    let (status, env) = get_as_clerk(&ctx, "/cron/wecom-daily/logs").await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{env}");
}

/// 空库时 `total = 0` + `items = []`（不是 404）。
#[tokio::test]
async fn empty_log_table_returns_zero_total() {
    let ctx = bootstrap().await;
    let (status, env) = get_as_manager(&ctx, "/cron/wecom-daily/logs").await;
    assert_eq!(status, StatusCode::OK, "{env}");
    assert_eq!(data(&env)["total"].as_i64(), Some(0));
    assert_eq!(data(&env)["items"].as_array().unwrap().len(), 0);
}

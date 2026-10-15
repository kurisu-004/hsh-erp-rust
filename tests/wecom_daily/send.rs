//! 端点 4：`POST /send`（真发 + 落流水，成败都落）
//!
//! ⚠️ **覆盖空洞**：`/send` 的**成功路径**（`status='SUCCESS'` 的流水行）**无集成测试**
//! —— 测试环境没有真实可达的群机器人地址，且本仓**刻意不引入 webhook 的 mock
//! transport**。本文件覆盖的是「必失败」路径（指向不可达 key ⇒ 上游 `errcode != 0`
//! ⇒ `21802` + 一行 FAILED 流水）与 `SCHEDULED` 的准备阶段失败。登记见
//! `docs/api/wecom-daily.md` §8.3。

use axum::http::StatusCode;
use serde_json::json;

use super::*;

/// 把配置保存好（指向某个 webhook）。
async fn save_config(ctx: &Ctx, webhook_id: i64) -> Value {
    let (status, env) =
        post_as_manager(ctx, "/cron/wecom-daily/config", config_body(0, webhook_id)).await;
    assert_eq!(status, StatusCode::OK, "保存配置应成功：{env}");
    data(&env).clone()
}

/// 指向不可达 key ⇒ `21802` + **FAILED 流水行**（`chunk_sent = 0`）。
#[tokio::test]
async fn unreachable_webhook_is_21802_and_writes_a_failed_log_row() {
    let ctx = bootstrap().await;
    let bad = insert_webhook(&ctx.pool, &unique_name("不可达"), &unreachable_url()).await;
    save_config(&ctx, bad).await;

    let (status, env) = post_as_manager(&ctx, "/cron/wecom-daily/send", json!({})).await;
    assert_biz_code(status, &env, 21802, StatusCode::BAD_GATEWAY);
    assert!(
        env["message"].as_str().unwrap().contains("log_id="),
        "错误消息要带流水指针：{}",
        env["message"]
    );

    let n = count_logs(&ctx.pool).await;
    assert_eq!(n, 1, "投递失败也必须落一行流水");

    let (status, env) = get_as_manager(&ctx, "/cron/wecom-daily/logs").await;
    assert_eq!(status, StatusCode::OK);
    let items = data(&env)["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    let row = &items[0];
    assert_eq!(row["status"].as_str(), Some("FAILED"));
    assert_eq!(row["trigger_source"].as_str(), Some("MANUAL"));
    assert_eq!(row["chunk_sent"].as_i64(), Some(0));
    assert!(
        row["chunk_count"].as_i64().unwrap() >= 1,
        "渲染过了 ⇒ 片数 ≥1"
    );
    assert!(row["error_message"].as_str().is_some(), "失败行必须有原因");
    assert!(
        row["error_message"].as_str().unwrap().contains("条失败"),
        "错误消息形如「第 i/N 条失败：…」：{}",
        row["error_message"]
    );
}

/// 不变量：`chunk_sent < chunk_count ⟺ status == 'FAILED'`。
#[tokio::test]
async fn chunk_sent_lt_chunk_count_implies_failed() {
    let ctx = bootstrap().await;
    let bad = insert_webhook(&ctx.pool, &unique_name("不可达2"), &unreachable_url()).await;
    save_config(&ctx, bad).await;
    let _ = post_as_manager(&ctx, "/cron/wecom-daily/send", json!({})).await;

    let (_, env) = get_as_manager(&ctx, "/cron/wecom-daily/logs").await;
    let row = &data(&env)["items"].as_array().unwrap()[0];
    let sent = row["chunk_sent"].as_i64().unwrap();
    let count = row["chunk_count"].as_i64().unwrap();
    assert!(sent < count);
    assert_eq!(row["status"].as_str(), Some("FAILED"));
}

/// `report_date` 恒等于 `sent_at` 的日期（同一个 `now_naive()` 取值）。
#[tokio::test]
async fn report_date_equals_sent_at_date() {
    let ctx = bootstrap().await;
    let bad = insert_webhook(&ctx.pool, &unique_name("不可达3"), &unreachable_url()).await;
    save_config(&ctx, bad).await;
    let _ = post_as_manager(&ctx, "/cron/wecom-daily/send", json!({})).await;

    let (_, env) = get_as_manager(&ctx, "/cron/wecom-daily/logs").await;
    let row = &data(&env)["items"].as_array().unwrap()[0];
    let report = row["report_date"].as_str().unwrap();
    let sent_at = row["sent_at"].as_str().unwrap();
    assert_eq!(report, &sent_at[..10], "统计日恒等于发送时刻的日期");
    assert_eq!(report, &today().format("%Y-%m-%d").to_string());
}

/// `markdown_bytes` 是**全部片**的字节数之和（≥ 单片长度）。
#[tokio::test]
async fn markdown_bytes_is_the_sum_over_all_chunks() {
    let ctx = bootstrap().await;
    for i in 0..120 {
        let w = insert_worker(&ctx.pool, &format!("工{i:03}")).await;
        let p = insert_part(
            &ctx.pool,
            ctx.customer_id,
            &format!("S{i:04}"),
            "IN_PROCESS",
            None,
        )
        .await;
        let b = insert_batch(&ctx.pool, p, "IN_PROCESS").await;
        insert_event(
            &ctx.pool,
            p,
            Some(w),
            "RETURNED_TO_SHELF",
            Some(30 - i),
            Some(b),
            today_at(9, (i % 60) as u32),
        )
        .await;
    }
    let bad = insert_webhook(&ctx.pool, &unique_name("不可达4"), &unreachable_url()).await;
    // `worker_top_n: 0`（不限人）—— 否则默认 15 会把 120 个工人挡掉 105 个，两条路径
    // 渲出来的内容不同、字节数自然对不上（那是对比口径错了，不是实现错）。
    let (status, cfg) =
        post_as_manager(&ctx, "/cron/wecom-daily/config", config_body(0, bad)).await;
    assert_eq!(status, StatusCode::OK, "{cfg}");
    let v = data(&cfg)["version"].as_i64().unwrap() as i32;
    let mut body = config_body(v, bad);
    body["worker_top_n"] = json!(0);
    let (status, cfg) = post_as_manager(&ctx, "/cron/wecom-daily/config", body).await;
    assert_eq!(status, StatusCode::OK, "{cfg}");

    let _ = post_as_manager(&ctx, "/cron/wecom-daily/send", json!({})).await;

    // 用**同一份**配置预览一次（空体 = 完全用当前配置），拿到真切的字节数
    let (_, prev) = post_as_manager(&ctx, "/cron/wecom-daily/preview", json!({})).await;
    let sum: usize = chunks_of(&prev).iter().map(|c| c.len()).sum();

    let (_, env) = get_as_manager(&ctx, "/cron/wecom-daily/logs").await;
    let row = &data(&env)["items"].as_array().unwrap()[0];
    assert_eq!(
        row["markdown_bytes"].as_i64(),
        Some(sum as i64),
        "markdown_bytes = Σ chunk.len()（字节，不是字符）"
    );
    assert!(row["chunk_count"].as_i64().unwrap() >= 2, "这批数据是多片");
}

/// 未选通道 ⇒ `21801`，**且不落流水**（`MANUAL` 路径：失败有 HTTP 码给调用方看）。
#[tokio::test]
async fn no_webhook_selected_is_21801_and_writes_no_log() {
    let ctx = bootstrap().await;
    let (status, env) = post_as_manager(&ctx, "/cron/wecom-daily/send", json!({})).await;
    assert_biz_code(status, &env, 21801, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        count_logs(&ctx.pool).await,
        0,
        "准备阶段失败（MANUAL）不落流水"
    );
}

/// 通道指向已软删 ⇒ `21801` + 不落流水。
#[tokio::test]
async fn deleted_webhook_is_21801_and_writes_no_log() {
    let ctx = bootstrap().await;
    save_config(&ctx, ctx.webhook_id).await;
    sqlx::query("UPDATE t_wecom_webhook SET deleted_at = now() WHERE id = $1")
        .bind(ctx.webhook_id)
        .execute(&ctx.pool)
        .await
        .expect("软删");

    let (status, env) = post_as_manager(&ctx, "/cron/wecom-daily/send", json!({})).await;
    assert_biz_code(status, &env, 21801, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(count_logs(&ctx.pool).await, 0);
}

/// 渲染阶段的 `21804`（超 `max_chunks`）**不落流水** —— 前端正是靠「有没有流水行」
/// 区分「装不下」与「投递失败」这两类。
#[tokio::test]
async fn render_stage_21804_writes_no_log_row() {
    let ctx = bootstrap().await;
    for i in 0..120 {
        let w = insert_worker(&ctx.pool, &format!("工{i:03}")).await;
        let p = insert_part(
            &ctx.pool,
            ctx.customer_id,
            &format!("T{i:04}"),
            "IN_PROCESS",
            None,
        )
        .await;
        let b = insert_batch(&ctx.pool, p, "IN_PROCESS").await;
        insert_event(
            &ctx.pool,
            p,
            Some(w),
            "RETURNED_TO_SHELF",
            Some(40 - i),
            Some(b),
            today_at(9, (i % 60) as u32),
        )
        .await;
    }
    let bad = insert_webhook(&ctx.pool, &unique_name("不可达5"), &unreachable_url()).await;
    save_config(&ctx, bad).await;

    let (status, env) = post_as_manager(
        &ctx,
        "/cron/wecom-daily/send",
        json!({"worker_top_n": 0, "max_chunks": 1}),
    )
    .await;
    assert_biz_code(status, &env, 21804, StatusCode::BAD_REQUEST);
    assert_eq!(
        count_logs(&ctx.pool).await,
        0,
        "没发出去不是投递失败 ⇒ 不落流水"
    );
}

/// `SCHEDULED` 的**准备阶段失败也落流水**（`chunk_count = 0`）。
#[tokio::test]
async fn scheduled_prepare_failure_still_writes_a_log_row() {
    let ctx = bootstrap().await;
    // 故意不建配置行（`webhook_id = None`）⇒ 通道解析失败
    let before = count_logs(&ctx.pool).await;
    hsh_erp_rust::modules::wecom_daily::service::send_scheduled(&ctx.state).await;

    assert_eq!(
        count_logs(&ctx.pool).await,
        before + 1,
        "定时路径的准备阶段失败也必须留下痕迹"
    );
    let (_, env) = get_as_manager(&ctx, "/cron/wecom-daily/logs").await;
    let row = &data(&env)["items"].as_array().unwrap()[0];
    assert_eq!(row["status"].as_str(), Some("FAILED"));
    assert_eq!(row["trigger_source"].as_str(), Some("SCHEDULED"));
    assert_eq!(
        row["chunk_count"].as_i64(),
        Some(0),
        "chunk_count = 0 是「还没渲染」的合法值"
    );
    assert_eq!(row["chunk_sent"].as_i64(), Some(0));
    assert_eq!(row["markdown_bytes"].as_i64(), Some(0));
    assert_eq!(row["overview_batch_count"].as_i64(), Some(0));
    assert_eq!(row["worker_row_total"].as_i64(), Some(0));
    assert_eq!(
        row["report_date"].as_str(),
        Some(today().format("%Y-%m-%d").to_string().as_str())
    );
    assert!(row["error_message"].as_str().is_some());
}

/// 流水行 **18 个字段**（前端 `.strict()` + 键数断言钉死）。
#[tokio::test]
async fn log_row_has_eighteen_fields() {
    let ctx = bootstrap().await;
    let bad = insert_webhook(&ctx.pool, &unique_name("不可达6"), &unreachable_url()).await;
    save_config(&ctx, bad).await;
    let _ = post_as_manager(&ctx, "/cron/wecom-daily/send", json!({})).await;

    let (_, env) = get_as_manager(&ctx, "/cron/wecom-daily/logs").await;
    let row = &data(&env)["items"].as_array().unwrap()[0];
    let n = row.as_object().expect("行是对象").len();
    // ⚠️ 18 = 19 − `note_part_count`（2026-10-23 随 DB 列一并下线）。
    assert_eq!(n, 18, "流水行必须逐字 18 个字段：{row}");
    for k in [
        "log_id",
        "config_version",
        "trigger_source",
        "status",
        "webhook_name",
        "webhook_masked",
        "report_date",
        "markdown_bytes",
        "chunk_count",
        "chunk_sent",
        "overview_batch_count",
        "overview_quantity",
        "note_count",
        "overdue_count",
        "upcoming_count",
        "worker_row_total",
        "error_message",
        "sent_at",
    ] {
        assert!(row.get(k).is_some(), "缺键 {k}：{row}");
    }
}

/// 流水里的通道快照：名称 + **掩码**（明文 URL 一个字都不落库）。
#[tokio::test]
async fn log_row_snapshots_channel_name_and_masked_url() {
    let ctx = bootstrap().await;
    let name = unique_name("快照通道");
    let url = unique_url();
    let id = insert_webhook(&ctx.pool, &name, &url).await;
    save_config(&ctx, id).await;
    let _ = post_as_manager(&ctx, "/cron/wecom-daily/send", json!({})).await;

    let (_, env) = get_as_manager(&ctx, "/cron/wecom-daily/logs").await;
    let row = &data(&env)["items"].as_array().unwrap()[0];
    assert_eq!(row["webhook_name"].as_str(), Some(name.as_str()));
    let masked = row["webhook_masked"].as_str().unwrap();
    assert!(masked.contains("***"), "必须是掩码：{masked}");
    assert!(
        !masked.contains("test-"),
        "掩码里不该出现明文 key 段：{masked}"
    );

    // 库里那一列同样不含明文
    let stored: String = sqlx::query_scalar("SELECT webhook_masked FROM t_wecom_daily_log LIMIT 1")
        .fetch_one(&ctx.pool)
        .await
        .expect("读 webhook_masked");
    // ⚠️ 掩码**保留**完整 URL 前缀（`…/webhook/send?key=` + 前 4 位 + `***` + 后 4 位），
    // 这正是它的设计（肉眼确认「是不是同一个群」）⇒ 断言只能是「**完整 key** 不在里面」。
    assert!(!stored.contains("test-"), "{stored}");
    assert!(!stored.contains(&url), "掩码里出现了完整 key：{stored}");
}

/// 配置版本与通道是**快照**：发完之后改配置 / 软删通道，历史行不变。
#[tokio::test]
async fn log_row_is_a_snapshot_not_a_join() {
    let ctx = bootstrap().await;
    let name = unique_name("快照版通道");
    let id = insert_webhook(&ctx.pool, &name, &unreachable_url()).await;
    save_config(&ctx, id).await;
    let _ = post_as_manager(&ctx, "/cron/wecom-daily/send", json!({})).await;

    // 发完之后：把配置改到新的一版，并软删那个通道
    let (_, cfg) = get_as_manager(&ctx, "/cron/wecom-daily/config").await;
    let v = data(&cfg)["version"].as_i64().unwrap() as i32;
    let mut body = config_body(v, id);
    body["upcoming_days"] = json!(9);
    let (status, _) = post_as_manager(&ctx, "/cron/wecom-daily/config", body).await;
    assert_eq!(status, StatusCode::OK);
    sqlx::query("UPDATE t_wecom_webhook SET deleted_at = now() WHERE id = $1")
        .bind(id)
        .execute(&ctx.pool)
        .await
        .expect("软删");

    let (_, env) = get_as_manager(&ctx, "/cron/wecom-daily/logs").await;
    let row = &data(&env)["items"].as_array().unwrap()[0];
    assert_eq!(
        row["webhook_name"].as_str(),
        Some(name.as_str()),
        "通道软删后历史行仍要说得清发到哪个群"
    );
    assert_eq!(
        row["config_version"].as_i64(),
        Some(0),
        "config_version 是发那一版时的快照"
    );
}

/// CLERK 越权。
#[tokio::test]
async fn clerk_cannot_send() {
    let ctx = bootstrap().await;
    let (status, env) = post_as_clerk(&ctx, "/cron/wecom-daily/send", json!({})).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{env}");
}

/// 明文 webhook 地址永不进任何出参 / 错误文案。
#[tokio::test]
async fn no_plaintext_webhook_url_leaks() {
    let ctx = bootstrap().await;
    let url = unique_url();
    let id = insert_webhook(&ctx.pool, &unique_name("脱敏通道"), &url).await;
    let (status, env) = post_as_manager(&ctx, "/cron/wecom-daily/config", config_body(0, id)).await;
    assert_eq!(status, StatusCode::OK, "{env}");
    let text = serde_json::to_string(&env).unwrap();
    assert!(!text.contains("test-"), "出参里出现了明文 key 段：{text}");
    // 库里存的也是密文
    let cipher = stored_url_cipher(&ctx.pool, id).await;
    assert!(cipher != url, "库里存的是明文！");
    assert_eq!(open_with_test_aad(&cipher), url, "确实是测试主密钥下的密文");
}

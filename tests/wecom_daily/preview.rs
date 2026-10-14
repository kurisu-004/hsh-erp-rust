//! 端点 3：`POST /preview`（渲染分片；**不发送、不落流水**）
//!
//! 覆盖：窗口半开含今天 / 交期窗口收缩 / 已交量守卫 / 节开关 / 分片（≥2 片、逐片
//! ≤4096 字节、首行带「（i/N）」、`N==1` 无后缀）/ `worker_top_n` 脚注 /
//! 超 `max_chunks` 返 `21804` / **绝不写流水行**。

use axum::http::StatusCode;
use serde_json::{Value, json};

use super::*;

/// 造一个「今天有报工」的工人 + 工单 + 批次 + 事件（第一节与第四节的公共 fixture）。
async fn seed_worker_reports(
    ctx: &Ctx,
    name: &str,
    events: &[(&str, i32)],
) -> (i64, i64, Vec<i64>) {
    let worker_id = insert_worker(&ctx.pool, name).await;
    let part_id = insert_part(&ctx.pool, ctx.customer_id, "RPT-1", "IN_PROCESS", None).await;
    let mut batch_ids = Vec::new();
    for (i, (event_type, qty)) in events.iter().enumerate() {
        let batch_id = insert_batch(&ctx.pool, part_id, "IN_PROCESS").await;
        batch_ids.push(batch_id);
        insert_event(
            &ctx.pool,
            part_id,
            Some(worker_id),
            event_type,
            Some(*qty),
            Some(batch_id),
            today_at(9, (i % 60) as u32),
        )
        .await;
    }
    (worker_id, part_id, batch_ids)
}

/// 预览永远**不落流水**（端点的核心不变量之一）。
#[tokio::test]
async fn preview_never_writes_log_row() {
    let ctx = bootstrap().await;
    seed_worker_reports(&ctx, "张三", &[("RETURNED_TO_SHELF", 3)]).await;

    let (status, env) = post_as_manager(&ctx, "/cron/wecom-daily/preview", json!({})).await;
    assert_eq!(status, StatusCode::OK, "{env}");
    assert_eq!(count_logs(&ctx.pool).await, 0, "/preview 绝不落流水");
}

/// 空体 `{}` = 全用当前配置；出参是 **15 个字段**、**没有** `markdown` 单字段。
#[tokio::test]
async fn preview_out_has_fifteen_fields_and_no_markdown_singular() {
    let ctx = bootstrap().await;
    let (status, env) = post_as_manager(&ctx, "/cron/wecom-daily/preview", json!({})).await;
    assert_eq!(status, StatusCode::OK, "{env}");
    let d = data(&env);
    let n = d.as_object().expect("data 是对象").len();
    assert_eq!(n, 15, "PreviewOut 必须逐字 15 个字段：{d}");
    assert!(
        d.get("markdown").is_none(),
        "契约里没有 `markdown` 单字段（只有 chunks）：{d}"
    );
    assert!(d["chunks"].is_array());
    assert_eq!(
        d["chunk_count"].as_i64().unwrap(),
        chunks_of(&env).len() as i64,
        "chunk_count 是 chunks.len() 的冗余镜像"
    );
    assert_eq!(
        d["subject"].as_str(),
        Some(format!("# 生产日报 · {}", today().format("%Y-%m-%d")).as_str()),
        "缺省标题是「# 生产日报 · <date>」"
    );
    // 四个节全开（缺省）
    assert_eq!(
        d["sections"].as_array().unwrap(),
        &vec![
            Value::from("overview"),
            Value::from("delivery"),
            Value::from("orders"),
            Value::from("workers")
        ]
    );
}

/// 报工事件集是 **送检 + 放回**，不是 `PICKED_UP`。
#[tokio::test]
async fn report_event_set_is_returned_plus_sent_to_inspection() {
    let ctx = bootstrap().await;
    let worker_id = insert_worker(&ctx.pool, "李四").await;
    let part_id = insert_part(&ctx.pool, ctx.customer_id, "EV-1", "IN_PROCESS", None).await;
    let b1 = insert_batch(&ctx.pool, part_id, "IN_PROCESS").await;
    let b2 = insert_batch(&ctx.pool, part_id, "IN_PROCESS").await;
    let b3 = insert_batch(&ctx.pool, part_id, "IN_PROCESS").await;
    // 放回 2 件（批 1）+ 送检 5 件（批 2）+ 领取 9 件（批 3，**不该计入**）
    insert_event(
        &ctx.pool,
        part_id,
        Some(worker_id),
        "RETURNED_TO_SHELF",
        Some(2),
        Some(b1),
        today_at(9, 1),
    )
    .await;
    insert_event(
        &ctx.pool,
        part_id,
        Some(worker_id),
        "SENT_TO_INSPECTION",
        Some(5),
        Some(b2),
        today_at(9, 2),
    )
    .await;
    insert_event(
        &ctx.pool,
        part_id,
        Some(worker_id),
        "PICKED_UP",
        Some(9),
        Some(b3),
        today_at(9, 3),
    )
    .await;

    let (status, env) = post_as_manager(&ctx, "/cron/wecom-daily/preview", json!({})).await;
    assert_eq!(status, StatusCode::OK, "{env}");
    let d = data(&env);
    assert_eq!(
        d["overview_batch_count"].as_i64(),
        Some(2),
        "批次数 = COUNT(DISTINCT batch_id)（PICKED_UP 不计入）"
    );
    assert_eq!(
        d["overview_quantity"].as_i64(),
        Some(7),
        "送件数 = 放回 2 + 送检 5（不含领取 9）"
    );
    let chunks = chunks_of(&env).join("\n");
    assert!(chunks.contains("报工批次"));
    assert!(chunks.contains("<font color=\"info\">2</font> 批"));
}

/// 同一批次分两次放回 ⇒ 折叠成 **1 批**（`COUNT(DISTINCT batch_id)` 的正解）。
#[tokio::test]
async fn same_batch_returned_twice_counts_as_one_batch() {
    let ctx = bootstrap().await;
    let worker_id = insert_worker(&ctx.pool, "王五").await;
    let part_id = insert_part(&ctx.pool, ctx.customer_id, "EV-2", "IN_PROCESS", None).await;
    let batch_id = insert_batch(&ctx.pool, part_id, "IN_PROCESS").await;
    insert_event(
        &ctx.pool,
        part_id,
        Some(worker_id),
        "RETURNED_TO_SHELF",
        Some(4),
        Some(batch_id),
        today_at(10, 1),
    )
    .await;
    insert_event(
        &ctx.pool,
        part_id,
        Some(worker_id),
        "RETURNED_TO_SHELF",
        Some(6),
        Some(batch_id),
        today_at(10, 2),
    )
    .await;

    let (_, env) = post_as_manager(&ctx, "/cron/wecom-daily/preview", json!({})).await;
    assert_eq!(data(&env)["overview_batch_count"].as_i64(), Some(1));
    assert_eq!(data(&env)["overview_quantity"].as_i64(), Some(10));
}

/// 窗口半开 `[today, today+1)`：昨天的同型事件**不计入**，今天 00:00 与 23:59 都计入。
#[tokio::test]
async fn today_window_is_half_open_and_includes_today() {
    let ctx = bootstrap().await;
    let worker_id = insert_worker(&ctx.pool, "赵六").await;
    let part_id = insert_part(&ctx.pool, ctx.customer_id, "EV-3", "IN_PROCESS", None).await;
    // 昨天 23:59 ⇒ 窗口外
    let b_old = insert_batch(&ctx.pool, part_id, "IN_PROCESS").await;
    insert_event(
        &ctx.pool,
        part_id,
        Some(worker_id),
        "RETURNED_TO_SHELF",
        Some(11),
        Some(b_old),
        yesterday_at(23),
    )
    .await;
    // 今天 00:00 ⇒ 窗口内（下界闭合）
    let b_lo = insert_batch(&ctx.pool, part_id, "IN_PROCESS").await;
    insert_event(
        &ctx.pool,
        part_id,
        Some(worker_id),
        "RETURNED_TO_SHELF",
        Some(1),
        Some(b_lo),
        today_at(0, 0),
    )
    .await;
    // 今天 23:59 ⇒ 窗口内（上界是次日 00:00，仍然闭合）
    let b_hi = insert_batch(&ctx.pool, part_id, "IN_PROCESS").await;
    insert_event(
        &ctx.pool,
        part_id,
        Some(worker_id),
        "RETURNED_TO_SHELF",
        Some(1),
        Some(b_hi),
        today_at(23, 59),
    )
    .await;

    let (_, env) = post_as_manager(&ctx, "/cron/wecom-daily/preview", json!({})).await;
    assert_eq!(
        data(&env)["overview_quantity"].as_i64(),
        Some(2),
        "昨天 23:59 的 11 件必须被半开窗口排除"
    );
}

/// 第三节：逾期谓词 + 未来窗口**含今天** + `DELIVERED` / `COMPLETED` 都被排除。
#[tokio::test]
async fn overdue_and_upcoming_predicates() {
    let ctx = bootstrap().await;

    // 逾期：交期昨天、状态 IN_PROCESS、无已交批次 ⇒ 逾期 1
    let od_id = insert_part(
        &ctx.pool,
        ctx.customer_id,
        "OD-1",
        "IN_PROCESS",
        Some(today().pred_days(1)),
    )
    .await;
    // 逾期但**已交割**（DELIVERED）⇒ 排除
    let od_delivered = insert_part(
        &ctx.pool,
        ctx.customer_id,
        "OD-2",
        "IN_PROCESS",
        Some(today().pred_days(2)),
    )
    .await;
    insert_batch(&ctx.pool, od_delivered, "DELIVERED").await;
    // 逾期但 **COMPLETED** 批次（force-complete 端点强推的态）⇒ 同样排除
    let od_completed = insert_part(
        &ctx.pool,
        ctx.customer_id,
        "OD-3",
        "IN_PROCESS",
        Some(today().pred_days(3)),
    )
    .await;
    insert_batch(&ctx.pool, od_completed, "COMPLETED").await;
    // 状态不在 6 态白名单（DELIVERED）⇒ 排除
    let _ = insert_part(
        &ctx.pool,
        ctx.customer_id,
        "OD-4",
        "DELIVERED",
        Some(today().pred_days(4)),
    )
    .await;

    // 未来：今天到期（含今天）、+2 天到期 ⇒ 未来 2（upcoming_days=3）
    let _ = insert_part(
        &ctx.pool,
        ctx.customer_id,
        "UP-1",
        "IN_PROCESS",
        Some(today()),
    )
    .await;
    let _ = insert_part(
        &ctx.pool,
        ctx.customer_id,
        "UP-2",
        "IN_PROCESS",
        Some(today().succ_days(2)),
    )
    .await;
    // +3 天到期 ⇒ 窗口是半开的 `[today, today+3)`，恰好**排除**
    let _ = insert_part(
        &ctx.pool,
        ctx.customer_id,
        "UP-3",
        "IN_PROCESS",
        Some(today().succ_days(3)),
    )
    .await;
    let _ = od_id;

    let (status, env) = post_as_manager(&ctx, "/cron/wecom-daily/preview", json!({})).await;
    assert_eq!(status, StatusCode::OK, "{env}");
    assert_eq!(data(&env)["overdue_count"].as_i64(), Some(1));
    assert_eq!(
        data(&env)["upcoming_count"].as_i64(),
        Some(2),
        "窗口 `[today, today+3)` 含今天、不含 +3 天"
    );
}

/// `upcoming_days` 收缩时窗口跟着收缩。
#[tokio::test]
async fn upcoming_days_shrinks_the_window() {
    let ctx = bootstrap().await;
    let _ = insert_part(
        &ctx.pool,
        ctx.customer_id,
        "UP-1",
        "IN_PROCESS",
        Some(today()),
    )
    .await;
    let _ = insert_part(
        &ctx.pool,
        ctx.customer_id,
        "UP-2",
        "IN_PROCESS",
        Some(today().succ_days(2)),
    )
    .await;
    let _ = insert_part(
        &ctx.pool,
        ctx.customer_id,
        "UP-3",
        "IN_PROCESS",
        Some(today().succ_days(3)),
    )
    .await;

    let (_, env) = post_as_manager(
        &ctx,
        "/cron/wecom-daily/preview",
        json!({"upcoming_days": 1}),
    )
    .await;
    assert_eq!(
        data(&env)["upcoming_count"].as_i64(),
        Some(1),
        "upcoming_days=1 ⇒ 窗口 `[today, today+1)` 只含今天那一条"
    );

    // 覆盖值只在本次预览生效（**不落配置**）：缺省 3 天窗口含今天 / +2 天、不含 +3 天
    let (_, env2) = post_as_manager(&ctx, "/cron/wecom-daily/preview", json!({})).await;
    assert_eq!(data(&env2)["upcoming_count"].as_i64(), Some(2));
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM t_wecom_daily_config")
        .fetch_one(&ctx.pool)
        .await
        .expect("count config");
    assert_eq!(n, 0, "预览的覆盖值绝不落库");
}

/// 第二节：只按 `picked_up_at` 判，**已归档的单也要算**（硬约束 3）。
#[tokio::test]
async fn delivery_section_counts_archived_notes_by_picked_up_at() {
    let ctx = bootstrap().await;
    let part_id = insert_part(&ctx.pool, ctx.customer_id, "DN-P", "READY_TO_SHIP", None).await;

    // 今天取货、已归档的单 ⇒ 必须算进来
    let n1 = insert_delivery_note(
        &ctx.pool,
        ctx.customer_id,
        "ARCHIVED",
        Some(today_at(11, 0)),
    )
    .await;
    insert_batch_full(&ctx.pool, part_id, "READY_TO_SHIP", Some(n1)).await;
    // 今天取货、仍在 PICKED_UP 的单
    let n2 = insert_delivery_note(
        &ctx.pool,
        ctx.customer_id,
        "PICKED_UP",
        Some(today_at(12, 0)),
    )
    .await;
    let p2 = insert_part(&ctx.pool, ctx.customer_id, "DN-P2", "READY_TO_SHIP", None).await;
    insert_batch_full(&ctx.pool, p2, "READY_TO_SHIP", Some(n2)).await;
    insert_batch_full(&ctx.pool, p2, "READY_TO_SHIP", Some(n2)).await;
    // 昨天取货的单 ⇒ 窗口外
    let _ = insert_delivery_note(
        &ctx.pool,
        ctx.customer_id,
        "PICKED_UP",
        Some(yesterday_at(11)),
    )
    .await;
    // 从没取货的单（DRAFT，picked_up_at 为 NULL）⇒ 不算
    let _ = insert_delivery_note(&ctx.pool, ctx.customer_id, "DRAFT", None).await;

    let (status, env) = post_as_manager(&ctx, "/cron/wecom-daily/preview", json!({})).await;
    assert_eq!(status, StatusCode::OK, "{env}");
    assert_eq!(
        data(&env)["note_count"].as_i64(),
        Some(2),
        "已归档的单也要算（不写 status 谓词）"
    );
    assert_eq!(
        data(&env)["note_part_count"].as_i64(),
        Some(3),
        "件数 = 那些单名下未删批次数之和"
    );
}

/// 节开关：关掉的节**完全不出现**（不是「标题 + 0」）。
#[tokio::test]
async fn closed_sections_are_absent_from_every_chunk() {
    let ctx = bootstrap().await;
    seed_worker_reports(&ctx, "钱七", &[("RETURNED_TO_SHELF", 3)]).await;
    let part_id = insert_part(
        &ctx.pool,
        ctx.customer_id,
        "ORD",
        "IN_PROCESS",
        Some(today().pred_days(1)),
    )
    .await;
    let _ = part_id;

    let cases: Vec<(&str, Value, &str, &str)> = vec![
        (
            "include_overview",
            json!(false),
            "一、全厂报工总览",
            "报工批次",
        ),
        (
            "include_delivery",
            json!(false),
            "二、今日出货",
            "已取货送货单",
        ),
        ("include_orders", json!(false), "三、逾期未交", "逾期未交"),
        ("include_workers", json!(false), "四、工人报工", "工人报工"),
    ];
    for (key, value, heading, needle) in cases {
        let mut body = json!({});
        body[key] = value.clone();
        let (status, env) = post_as_manager(&ctx, "/cron/wecom-daily/preview", body).await;
        assert_eq!(status, StatusCode::OK, "{key}=false：{env}");
        let all = chunks_of(&env).join("\n");
        assert!(!all.contains(heading), "{key}=false 时标题不该出现：{all}");
        assert!(!all.contains(needle), "{key}=false 时内容不该出现：{all}");
        let sections: Vec<String> = data(&env)["sections"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert_eq!(sections.len(), 3, "{key}=false 时 sections 只剩 3 项");
        assert!(!sections.contains(&key.replace("include_", "")));
    }
}

/// 分片：造足够多工人逼出 ≥2 片，逐条断言 ≤4096 **字节**且首行带「（i/N）」。
#[tokio::test]
async fn many_workers_force_multiple_chunks_with_page_suffix() {
    let ctx = bootstrap().await;
    let customer_id = ctx.customer_id;
    // 120 个工人 × 每人 1 条事件；条形行每行约 50 字节、每片预算 4048 字节
    // ⇒ 约 72 行就装满，120 行必多片。
    for i in 0..120 {
        let worker_id = insert_worker(&ctx.pool, &format!("工人{i:02}号")).await;
        let part_id = insert_part(
            &ctx.pool,
            customer_id,
            &format!("P{i:03}"),
            "IN_PROCESS",
            None,
        )
        .await;
        let batch_id = insert_batch(&ctx.pool, part_id, "IN_PROCESS").await;
        insert_event(
            &ctx.pool,
            part_id,
            Some(worker_id),
            "RETURNED_TO_SHELF",
            Some(60 - i),
            Some(batch_id),
            today_at(8, (i % 60) as u32),
        )
        .await;
    }

    let (status, env) = post_as_manager(
        &ctx,
        "/cron/wecom-daily/preview",
        json!({"worker_top_n": 0, "max_chunks": 20}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{env}");
    let chunks = chunks_of(&env);
    assert!(
        chunks.len() >= 2,
        "120 个工人必须多片，实际 {}",
        chunks.len()
    );
    let n = chunks.len();
    for (i, c) in chunks.iter().enumerate() {
        assert!(c.len() <= 4096, "第 {} 片 {} 字节 > 4096", i + 1, c.len());
        let first = c.lines().next().unwrap_or_default();
        assert!(
            first.contains(&today().format("%Y-%m-%d").to_string()),
            "第 {} 片首行应是带日期的标题：{first}",
            i + 1
        );
        assert!(
            first.contains(&format!("（{}/{}）", i + 1, n)),
            "第 {} 片首行缺页码后缀：{first}",
            i + 1
        );
    }
    assert_eq!(data(&env)["chunk_count"].as_i64(), Some(n as i64));
    // 全量工人行都返回（120），且 worker_row_total 一致
    assert_eq!(
        data(&env)["worker_rows"].as_array().unwrap().len(),
        120,
        "worker_rows 是全量"
    );
    assert_eq!(data(&env)["worker_row_total"].as_i64(), Some(120));
}

/// `N == 1` 时**不含**「（1/1）」。
#[tokio::test]
async fn single_chunk_has_no_page_suffix_in_preview() {
    let ctx = bootstrap().await;
    let (status, env) = post_as_manager(&ctx, "/cron/wecom-daily/preview", json!({})).await;
    assert_eq!(status, StatusCode::OK, "{env}");
    let chunks = chunks_of(&env);
    assert_eq!(chunks.len(), 1);
    assert!(
        !chunks[0].contains("（1/1）"),
        "单条不该挂页码后缀：{}",
        &chunks[0][..chunks[0].len().min(200)]
    );
}

/// `worker_top_n` 脚注文案能分辨「管理员设的上限」与「字节截断」。
#[tokio::test]
async fn worker_top_n_footer_names_the_config_cap() {
    let ctx = bootstrap().await;
    for i in 0..10 {
        let worker_id = insert_worker(&ctx.pool, &format!("工{i}")).await;
        let part_id = insert_part(
            &ctx.pool,
            ctx.customer_id,
            &format!("F{i}"),
            "IN_PROCESS",
            None,
        )
        .await;
        let batch_id = insert_batch(&ctx.pool, part_id, "IN_PROCESS").await;
        insert_event(
            &ctx.pool,
            part_id,
            Some(worker_id),
            "RETURNED_TO_SHELF",
            Some(10 - i),
            Some(batch_id),
            today_at(9, (i % 60) as u32),
        )
        .await;
    }

    let (status, env) = post_as_manager(
        &ctx,
        "/cron/wecom-daily/preview",
        json!({"worker_top_n": 4}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{env}");
    let all = chunks_of(&env).join("\n");
    assert!(
        all.contains("另有 6 人在此省略（配置「最多列出工人」= 4）"),
        "{all}"
    );
    assert!(!all.contains("截断"), "本域分片不是截断：{all}");
    // 渲染进 chunks 的只有 4 行
    let bar_lines = all
        .lines()
        .filter(|l| l.contains('█') || l.contains('░'))
        .count();
    assert_eq!(bar_lines, 4, "只有前 4 行进了正文：{all}");
    assert_eq!(data(&env)["worker_row_total"].as_i64(), Some(10));
}

/// 超 `max_chunks` ⇒ `21804` 且**未发送**、**未落流水**。
#[tokio::test]
async fn exceeding_max_chunks_is_21804_and_sends_nothing() {
    let ctx = bootstrap().await;
    for i in 0..120 {
        let worker_id = insert_worker(&ctx.pool, &format!("工{i:03}")).await;
        let part_id = insert_part(
            &ctx.pool,
            ctx.customer_id,
            &format!("G{i:04}"),
            "IN_PROCESS",
            None,
        )
        .await;
        let batch_id = insert_batch(&ctx.pool, part_id, "IN_PROCESS").await;
        insert_event(
            &ctx.pool,
            part_id,
            Some(worker_id),
            "RETURNED_TO_SHELF",
            Some(40 - i),
            Some(batch_id),
            today_at(9, (i % 60) as u32),
        )
        .await;
    }

    let before = count_logs(&ctx.pool).await;
    let (status, env) = post_as_manager(
        &ctx,
        "/cron/wecom-daily/preview",
        json!({"worker_top_n": 0, "max_chunks": 1}),
    )
    .await;
    assert_biz_code(status, &env, 21804, StatusCode::BAD_REQUEST);
    let msg = env["message"].as_str().unwrap();
    assert!(msg.contains("最多拆几条"), "{msg}");
    assert!(msg.contains("最多列出工人"), "{msg}");
    assert_eq!(
        count_logs(&ctx.pool).await,
        before,
        "渲染阶段的 21804 不落流水"
    );
}

/// 四节全关 ⇒ `21804`（预览也要拦，不能渲一条只有标题的空消息）。
#[tokio::test]
async fn preview_rejects_four_sections_all_off() {
    let ctx = bootstrap().await;
    let (status, env) = post_as_manager(
        &ctx,
        "/cron/wecom-daily/preview",
        json!({
            "include_overview": false,
            "include_delivery": false,
            "include_orders": false,
            "include_workers": false
        }),
    )
    .await;
    assert_biz_code(status, &env, 21804, StatusCode::BAD_REQUEST);
}

/// 预览路径的**合并校验**：`max_chunks=99` / `worker_top_n=-1` / 未知 `worker_metric`
/// 都被 `21804` 拒（而不是悄悄 clamp）。
#[tokio::test]
async fn preview_overrides_are_validated() {
    let ctx = bootstrap().await;
    for body in [
        json!({"max_chunks": 99}),
        json!({"max_chunks": 0}),
        json!({"worker_top_n": -1}),
        json!({"worker_metric": "HOURS"}),
        json!({"frequency": "HOURLY"}),
        json!({"upcoming_days": -5}),
    ] {
        let (status, env) = post_as_manager(&ctx, "/cron/wecom-daily/preview", body.clone()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body} 应 400：{env}");
        assert_eq!(
            env.get("code").and_then(|c| c.as_i64()),
            Some(21804),
            "{body}"
        );
    }
}

/// `worker_metric` 切换条长依据与标题。
#[tokio::test]
async fn worker_metric_switches_heading_and_bar_metric() {
    let ctx = bootstrap().await;
    let w1 = insert_worker(&ctx.pool, "甲").await;
    let w2 = insert_worker(&ctx.pool, "乙").await;
    let p = insert_part(&ctx.pool, ctx.customer_id, "M-1", "IN_PROCESS", None).await;
    let b1 = insert_batch(&ctx.pool, p, "IN_PROCESS").await;
    let b2 = insert_batch(&ctx.pool, p, "IN_PROCESS").await;
    insert_event(
        &ctx.pool,
        p,
        Some(w1),
        "RETURNED_TO_SHELF",
        Some(100),
        Some(b1),
        today_at(9, 1),
    )
    .await;
    insert_event(
        &ctx.pool,
        p,
        Some(w2),
        "RETURNED_TO_SHELF",
        Some(10),
        Some(b2),
        today_at(9, 2),
    )
    .await;

    let (_, by_q) = post_as_manager(
        &ctx,
        "/cron/wecom-daily/preview",
        json!({"worker_metric": "QUANTITY"}),
    )
    .await;
    let all_q = chunks_of(&by_q).join("\n");
    assert!(all_q.contains("工人报工（送件数）"), "{all_q}");
    assert!(all_q.contains("> 100 甲 ████████████"), "{all_q}");
    assert!(all_q.contains("> 10 乙 █░░░░░░░░░░░"), "{all_q}");

    let (_, by_b) = post_as_manager(
        &ctx,
        "/cron/wecom-daily/preview",
        json!({"worker_metric": "BATCH"}),
    )
    .await;
    let all_b = chunks_of(&by_b).join("\n");
    assert!(all_b.contains("工人报工（报工批次数）"), "{all_b}");
    // 按批次数两人都是 1 ⇒ 两条都是满格
    assert!(all_b.contains("> 1 甲 ████████████"), "{all_b}");
    assert!(all_b.contains("> 1 乙 ████████████"), "{all_b}");
}

/// `worker_rows` **两条指标都返回**（前端预览弹窗那份不受 `worker_metric` 影响）。
#[tokio::test]
async fn worker_rows_carry_both_metrics() {
    let ctx = bootstrap().await;
    seed_worker_reports(
        &ctx,
        "孙八",
        &[("RETURNED_TO_SHELF", 7), ("SENT_TO_INSPECTION", 3)],
    )
    .await;

    let (_, env) = post_as_manager(
        &ctx,
        "/cron/wecom-daily/preview",
        json!({"worker_metric": "BATCH"}),
    )
    .await;
    let rows = data(&env)["worker_rows"]
        .as_array()
        .expect("worker_rows 是数组");
    assert_eq!(rows.len(), 1);
    let r = &rows[0];
    // 4 个键逐字对齐前端 schema
    assert_eq!(r.as_object().unwrap().len(), 4, "{r}");
    assert!(r["worker_name"].is_string());
    assert!(r["badge_code"].is_string());
    assert_eq!(
        r["batch_count"].as_i64(),
        Some(2),
        "两件级事件落在两个批次上"
    );
    assert_eq!(r["quantity"].as_i64(), Some(10));
}

/// 未选通道时 `/preview` 仍可用（**渲染与通道无关**）。
#[tokio::test]
async fn preview_works_without_a_webhook_selected() {
    let ctx = bootstrap().await;
    let (status, env) = post_as_manager(&ctx, "/cron/wecom-daily/preview", json!({})).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "未选通道不该影响预览（它不发送）：{env}"
    );
}

/// CLERK 越权。
#[tokio::test]
async fn clerk_cannot_preview() {
    let ctx = bootstrap().await;
    let (status, env) = post_as_clerk(&ctx, "/cron/wecom-daily/preview", json!({})).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{env}");
}

/// 「今天完工 / 当前在制 / 今日返修」三个数字都出现在第一节。
#[tokio::test]
async fn overview_section_carries_all_five_numbers() {
    let ctx = bootstrap().await;
    let w = insert_worker(&ctx.pool, "周九").await;
    let p_done = insert_part(&ctx.pool, ctx.customer_id, "CP-1", "COMPLETED", None).await;
    insert_event(
        &ctx.pool,
        p_done,
        Some(w),
        "COMPLETED",
        None,
        None,
        today_at(9, 0),
    )
    .await;
    let p_rep = insert_part(&ctx.pool, ctx.customer_id, "CP-2", "IN_PROCESS", None).await;
    insert_event(
        &ctx.pool,
        p_rep,
        Some(w),
        "REPAIR_STARTED",
        None,
        None,
        today_at(9, 1),
    )
    .await;
    // 在制：既没 COMPLETED 事件、状态又不在终态
    let _ = insert_part(&ctx.pool, ctx.customer_id, "IP-1", "IN_PROCESS", None).await;

    let (status, env) = post_as_manager(&ctx, "/cron/wecom-daily/preview", json!({})).await;
    assert_eq!(status, StatusCode::OK, "{env}");
    assert_eq!(data(&env)["completed_count"].as_i64(), Some(1));
    assert_eq!(data(&env)["repair_part_count"].as_i64(), Some(1));
    assert!(
        data(&env)["in_process_count"].as_i64().unwrap() >= 1,
        "在制数至少包含 CP-2 与 IP-1"
    );
    let all = chunks_of(&env).join("\n");
    assert!(all.contains("今日完工"));
    assert!(all.contains("当前在制"));
    assert!(all.contains("今日返修"));
}

// `NaiveDate` 的 ±N 天小工具（避免用例里出现 `2026-10-18` 这类字面量）。
trait DayMath {
    fn pred_days(self, n: i64) -> NaiveDate;
    fn succ_days(self, n: i64) -> NaiveDate;
}

impl DayMath for NaiveDate {
    fn pred_days(self, n: i64) -> NaiveDate {
        self - chrono::Duration::days(n)
    }
    fn succ_days(self, n: i64) -> NaiveDate {
        self + chrono::Duration::days(n)
    }
}

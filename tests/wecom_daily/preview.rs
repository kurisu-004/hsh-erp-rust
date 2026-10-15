//! 端点 3：`POST /preview`（渲染分片；**不发送、不落流水**）
//!
//! 覆盖：统计窗口半开且**跨天**（前一天 @ 锚点 ~ 当天 @ 锚点）/ 交期窗口收缩 /
//! 已交量守卫 / 节开关 / 分片（≥2 片、逐片 ≤4096 字节、首行带「（i/N）」、`N==1`
//! 无后缀）/ `worker_top_n` 脚注 / 超 `max_chunks` 返 `21804` / **绝不写流水行**。
//!
//! ⚠️ **所有「今天 xx:xx」的 fixture 都落在新窗口内**（缺省锚点 20:05 ⇒ 窗口是
//! `[昨天 20:05, 今天 20:05)`）；要造窗口外的点必须用 `yesterday_at()` 且时刻**早于
//! 20:05**，或今天 20:05 之后。

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

/// 空体 `{}` = 全用当前配置；出参是 **13 个字段**、**没有** `markdown` 单字段。
#[tokio::test]
async fn preview_out_has_thirteen_fields_and_no_markdown_singular() {
    let ctx = bootstrap().await;
    let (status, env) = post_as_manager(&ctx, "/cron/wecom-daily/preview", json!({})).await;
    assert_eq!(status, StatusCode::OK, "{env}");
    let d = data(&env);
    let n = d.as_object().expect("data 是对象").len();
    // ⚠️ 13 = 14 − `note_part_count`（2026-10-23 随 DB 列一并下线，出参键同步）。
    assert_eq!(n, 13, "PreviewOut 必须逐字 13 个字段：{d}");
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
    assert!(chunks.contains("报工："), "{chunks}");
    assert!(
        chunks.contains(
            "> 报工：<font color=\"info\">2</font> 批  共 <font color=\"info\">7</font> 件"
        ),
        "{chunks}"
    );
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

/// 统计窗口是**跨天半开** `[昨天 20:05, 今天 20:05)`（缺省锚点 = 缺省推送时刻）。
///
/// 四个点各钉一侧边界：昨天 19:59 在外、**昨天 20:05 之后在内**（这条是跨天窗口
/// 与旧日历日窗口的**唯一**差别：旧形态下昨天 23:59 是「昨天」的量）、今天 00:00 在内、
/// 今天 20:05 之后在外（上界不含）。
#[tokio::test]
async fn cross_day_window_is_half_open_around_the_anchor() {
    let ctx = bootstrap().await;
    let worker_id = insert_worker(&ctx.pool, "赵六").await;
    let part_id = insert_part(&ctx.pool, ctx.customer_id, "EV-3", "IN_PROCESS", None).await;
    // 昨天 19:59 ⇒ 窗口外（下界之前）
    let b_before = insert_batch(&ctx.pool, part_id, "IN_PROCESS").await;
    insert_event(
        &ctx.pool,
        part_id,
        Some(worker_id),
        "RETURNED_TO_SHELF",
        Some(11),
        Some(b_before),
        yesterday_at(19),
    )
    .await;
    // 昨天 23:59 ⇒ 窗口内（**跨天窗口的关键一条**）
    let b_prev = insert_batch(&ctx.pool, part_id, "IN_PROCESS").await;
    insert_event(
        &ctx.pool,
        part_id,
        Some(worker_id),
        "RETURNED_TO_SHELF",
        Some(13),
        Some(b_prev),
        yesterday_at(23),
    )
    .await;
    // 今天 00:00 ⇒ 窗口内
    let b_mid = insert_batch(&ctx.pool, part_id, "IN_PROCESS").await;
    insert_event(
        &ctx.pool,
        part_id,
        Some(worker_id),
        "RETURNED_TO_SHELF",
        Some(1),
        Some(b_mid),
        today_at(0, 0),
    )
    .await;
    // 今天 23:59 ⇒ 窗口外（上界不含）
    let b_after = insert_batch(&ctx.pool, part_id, "IN_PROCESS").await;
    insert_event(
        &ctx.pool,
        part_id,
        Some(worker_id),
        "RETURNED_TO_SHELF",
        Some(1),
        Some(b_after),
        today_at(23, 59),
    )
    .await;

    let (_, env) = post_as_manager(&ctx, "/cron/wecom-daily/preview", json!({})).await;
    assert_eq!(
        data(&env)["overview_quantity"].as_i64(),
        Some(14),
        "窗口 = 昨天 20:05 起算：昨天 19:59 的 11 件与今天 23:59 的 1 件都在外，\
         昨天 23:59 的 13 件 + 今天 00:00 的 1 件在内"
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

/// 第二节：只按 `picked_up_at` 判、**按工单去重**，**已归档的单也要算**（硬约束 3）。
#[tokio::test]
async fn delivery_section_counts_distinct_parts_by_picked_up_at() {
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
    // 昨天**上班时间**取货的单 ⇒ 窗口外（窗口下界是昨天 20:05）
    let n_old = insert_delivery_note(
        &ctx.pool,
        ctx.customer_id,
        "PICKED_UP",
        Some(yesterday_at(11)),
    )
    .await;
    let p_old = insert_part(&ctx.pool, ctx.customer_id, "DN-OLD", "READY_TO_SHIP", None).await;
    insert_batch_full(&ctx.pool, p_old, "READY_TO_SHIP", Some(n_old)).await;
    // 从没取货的单（DRAFT，picked_up_at 为 NULL）⇒ 不算
    let _ = insert_delivery_note(&ctx.pool, ctx.customer_id, "DRAFT", None).await;

    let (status, env) = post_as_manager(&ctx, "/cron/wecom-daily/preview", json!({})).await;
    assert_eq!(status, StatusCode::OK, "{env}");
    // ⚠️ 第二节只有一个数字，且 `note_count` 就是它（2026-10-23 起语义变更，
    // `note_part_count` 那一键随列下线）。
    assert_eq!(
        data(&env)["note_count"].as_i64(),
        Some(2),
        "出货 2 单：已归档的单也算（不写 status 谓词），p2 的两个批次折叠成 1 单，\
         昨天上班时间取货的单在窗口外"
    );
    assert!(
        data(&env).get("note_part_count").is_none(),
        "note_part_count 必须整个下线（前端 .strict() schema 按键名排版）"
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
            "报工：",
        ),
        ("include_delivery", json!(false), "二、本期出货", "出货："),
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

/// 第一节正文的三个口径：报工批数 / 送检批数（去重）/ 在制单数（`status='IN_PROCESS'`）。
#[tokio::test]
async fn overview_section_carries_batch_sent_and_wip_numbers() {
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
    // 同一批次送检**两次** ⇒ 送检批数按 `COUNT(DISTINCT batch_id)` 折叠成 1。
    let p_sent = insert_part(&ctx.pool, ctx.customer_id, "CP-2", "INSPECTION", None).await;
    let b_sent = insert_batch(&ctx.pool, p_sent, "INSPECTION").await;
    insert_event(
        &ctx.pool,
        p_sent,
        Some(w),
        "SENT_TO_INSPECTION",
        Some(3),
        Some(b_sent),
        today_at(9, 1),
    )
    .await;
    insert_event(
        &ctx.pool,
        p_sent,
        Some(w),
        "SENT_TO_INSPECTION",
        Some(4),
        Some(b_sent),
        today_at(9, 2),
    )
    .await;
    // 在制：`t_part.status = 'IN_PROCESS'`（**只**看派生列，不再有事件守卫）。
    let wip = insert_part(&ctx.pool, ctx.customer_id, "IP-1", "IN_PROCESS", None).await;
    insert_event(
        &ctx.pool,
        wip,
        Some(w),
        "REPAIR_STARTED",
        None,
        None,
        today_at(9, 3),
    )
    .await;

    let (status, env) = post_as_manager(&ctx, "/cron/wecom-daily/preview", json!({})).await;
    assert_eq!(status, StatusCode::OK, "{env}");
    assert_eq!(data(&env)["completed_count"].as_i64(), Some(1));
    assert_eq!(
        data(&env)["overview_batch_count"].as_i64(),
        Some(1),
        "报工批数 = 送检+放回的 distinct batch（同批两次送检折叠成 1 批）"
    );
    assert_eq!(
        data(&env)["overview_quantity"].as_i64(),
        Some(7),
        "报工件数 = 两次送检都算（3 + 4）"
    );
    assert_eq!(
        data(&env)["in_process_count"].as_i64(),
        Some(1),
        "在制 = t_part.status='IN_PROCESS' 的工单（IP-1）；REPAIR_STARTED 不进这个数"
    );
    let all = chunks_of(&env).join("\n");
    assert!(
        all.contains(
            "> 报工：<font color=\"info\">1</font> 批  共 <font color=\"info\">7</font> 件"
        ),
        "{all}"
    );
    assert!(
        all.contains(
            "> 送检 <font color=\"info\">1</font> 批  在制 <font color=\"info\">1</font> 单"
        ),
        "{all}"
    );
    // 「今日」措辞在跨天窗口下必然是错的
    assert!(!all.contains("今日"), "{all}");
}

// ===========================================================================
//  第三节：交期清单（分组 / 装配件子件 / 位置 / 截断）
// ===========================================================================

/// ★ 集成守卫：**半开日期窗口** + **红档 4 态** + **已交量守卫**三者在正文里的落点。
///
/// 逐个钉住（fixture 的每一行都有它的判据，少一行就会有判据没被测到）：
/// - `sdd = today - 2 / -1` ⇒ 逾期组；
/// - `sdd = today` ⇒ 进**第一个日期组**（半开区间的下界含今天）；
/// - `sdd = today + 1 / +2` ⇒ 对应日期组（`upcoming_days = 3`）；
/// - `sdd = today + 3` ⇒ **两组都不进**（半开区间的上界不含）；
/// - 逾期但 `status = 'INSPECTION'` / `'READY_TO_SHIP'` ⇒ **不进**（红档 = 品检前 4 态）；
/// - 逾期且有 `DELIVERED` / `COMPLETED` 批次 ⇒ **不进**（部分已交不显示）。
#[tokio::test]
async fn orders_section_grouping_uses_a_half_open_date_window() {
    let ctx = bootstrap().await;
    let cid = ctx.customer_id;

    // 逾期组：两天前、昨天（半开区间的左侧无上界）。
    insert_part_serialized(
        &ctx.pool,
        cid,
        Some("F1001"),
        "OD-1",
        "IN_PROCESS",
        2,
        Some(today().pred_days(2)),
    )
    .await;
    insert_part_serialized(
        &ctx.pool,
        cid,
        Some("F1002"),
        "OD-2",
        "PENDING",
        1,
        Some(today().pred_days(1)),
    )
    .await;
    // 品检后的两态：红档只收「品检前」，它们不进清单。
    insert_part_serialized(
        &ctx.pool,
        cid,
        Some("F9001"),
        "OD-INS",
        "INSPECTION",
        1,
        Some(today().pred_days(1)),
    )
    .await;
    insert_part_serialized(
        &ctx.pool,
        cid,
        Some("F9002"),
        "OD-RTS",
        "READY_TO_SHIP",
        1,
        Some(today().pred_days(1)),
    )
    .await;

    // 日期组：今天 / +1 / +2（上界 +3 不含）。
    for (i, serial) in [(0, "F2001"), (1, "F2002"), (2, "F2003"), (3, "F2004")] {
        insert_part_serialized(
            &ctx.pool,
            cid,
            Some(serial),
            &format!("UP-{i}"),
            "IN_PROCESS",
            1,
            Some(today().succ_days(i)),
        )
        .await;
    }

    // 部分已交（DELIVERED / COMPLETED 各一）⇒ 整张工单不进清单。
    let partial = insert_part_serialized(
        &ctx.pool,
        cid,
        Some("F3001"),
        "UP-PART",
        "IN_PROCESS",
        9,
        Some(today()),
    )
    .await;
    insert_batch(&ctx.pool, partial, "DELIVERED").await;
    let forced = insert_part_serialized(
        &ctx.pool,
        cid,
        Some("F3002"),
        "UP-FORCE",
        "IN_PROCESS",
        9,
        Some(today()),
    )
    .await;
    insert_batch(&ctx.pool, forced, "COMPLETED").await;

    let (status, env) = post_as_manager(&ctx, "/cron/wecom-daily/preview", json!({})).await;
    assert_eq!(status, StatusCode::OK, "{env}");
    let all = chunks_of(&env).join("\n");

    // 组标题：逾期组在前，日期组按日期升序接在后面。
    assert!(all.contains("## 三、逾期未交 / 3 日内交期"), "{all}");
    assert!(all.contains("> **逾期未交 · 2 条**"), "{all}");
    assert!(
        all.contains(&format!("> **{} · 1 条**", today().format("%Y-%m-%d"))),
        "{all}"
    );
    assert!(
        all.contains(&format!(
            "> **{} · 1 条**",
            today().succ_days(1).format("%Y-%m-%d")
        )),
        "{all}"
    );
    assert!(
        all.contains(&format!(
            "> **{} · 1 条**",
            today().succ_days(2).format("%Y-%m-%d")
        )),
        "{all}"
    );
    assert!(
        !all.contains(today().succ_days(3).format("%Y-%m-%d").to_string().as_str()),
        "`today + 3` 在半开区间外，不该出现该日期组：{all}"
    );

    // 计数：逾期 2 单 / 日期 3 单（**未截断**，与清单行一一对应）。
    assert_eq!(data(&env)["overdue_count"].as_i64(), Some(2));
    assert_eq!(data(&env)["upcoming_count"].as_i64(), Some(3));

    // 进清单的逐条出现。
    for serial in ["F1001", "F1002", "F2001", "F2002", "F2003"] {
        assert!(all.contains(serial), "{serial} 该在清单里：{all}");
    }
    // 被判据挡掉的逐条**不出现**（图号也不出现，正文里只有序列号）。
    for (serial, why) in [
        ("F9001", "INSPECTION 不属于红档（品检前）"),
        ("F9002", "READY_TO_SHIP 不属于红档（品检前）"),
        ("F3001", "已交割（DELIVERED）⇒ 部分已交不显示"),
        ("F3002", "已完工（COMPLETED，force-complete 强推）⇒ 不显示"),
        ("F2004", "`today + 3` 在半开区间外"),
    ] {
        assert!(!all.contains(serial), "{serial} 不该出现（{why}）：{all}");
    }
}

/// ★ 集成守卫：装配件行 + **缩进子件行**，子件各带自己的序列号 / 名称 / 数量 / 位置。
///
/// 同时覆盖位置的四个分支：`生产架` / `工人` / `外协`（三种能查到名字的）与
/// `OFFICE` / 无批次（两种查不到 ⇒ **整项不显示**，连分隔空格都不留）。
#[tokio::test]
async fn orders_section_renders_assembly_with_children_and_positions() {
    let ctx = bootstrap().await;
    let cid = ctx.customer_id;
    let shelf = insert_shelf(&ctx.pool, "A-03").await;
    let worker = insert_worker(&ctx.pool, "张三").await;
    let outsource = insert_outsource_company(&ctx.pool, "宏达").await;

    let asm = insert_assembly_serialized(
        &ctx.pool,
        cid,
        Some("F5000"),
        "ASM-1",
        2,
        Some(today().pred_days(1)),
    )
    .await;
    // 三个子件：分别落在生产架 / 工人手里 / 外协，另有一个在办公室、一个没有批次。
    let c1 = insert_child_part(&ctx.pool, cid, asm, Some("F5001"), "C-1", 4).await;
    let c2 = insert_child_part(&ctx.pool, cid, asm, Some("F5002"), "C-2", 5).await;
    let c3 = insert_child_part(&ctx.pool, cid, asm, Some("F5003"), "C-3", 6).await;
    let c4 = insert_child_part(&ctx.pool, cid, asm, Some("F5004"), "C-4", 7).await;
    let _c5 = insert_child_part(&ctx.pool, cid, asm, None, "C-5", 8).await;
    insert_batch_at(
        &ctx.pool,
        c1,
        "IN_PROCESS",
        Some("PRODUCTION_SHELF"),
        Some(shelf),
    )
    .await;
    insert_batch_at(&ctx.pool, c2, "IN_PROCESS", Some("WORKER"), Some(worker)).await;
    insert_batch_at(
        &ctx.pool,
        c3,
        "OUTSOURCE",
        Some("OUTSOURCE_COMPANY"),
        Some(outsource),
    )
    .await;
    insert_batch_at(&ctx.pool, c4, "PENDING", Some("OFFICE"), Some(shelf)).await;
    // c5 刻意**不造批次** ⇒ 无活跃批次 ⇒ 位置整项不显示。

    // 另有一张散件：序列号 NULL + 位置拿不到 ⇒ 两个「缺省字段」都在它身上。
    let loose = insert_part_serialized(
        &ctx.pool,
        cid,
        None,
        "LOOSE-1",
        "IN_PROCESS",
        3,
        Some(today().pred_days(3)),
    )
    .await;
    insert_batch_at(&ctx.pool, loose, "PENDING", None, None).await;

    let (status, env) = post_as_manager(&ctx, "/cron/wecom-daily/preview", json!({})).await;
    assert_eq!(status, StatusCode::OK, "{env}");
    let all = chunks_of(&env).join("\n");

    // 装配件行：它自己**没有批次**（批次挂在子件上）⇒ 位置整项不显示。
    assert!(all.contains("> F5000 装ASM-1 ×2"), "{all}");
    // 子件行：`>   └ ` 缩进 + 各自的位置。
    assert!(all.contains(">   └ F5001 件C-1 ×4  生产架:A-03"), "{all}");
    assert!(all.contains(">   └ F5002 件C-2 ×5  工人:张三"), "{all}");
    assert!(all.contains(">   └ F5003 件C-3 ×6  外协:宏达"), "{all}");
    // OFFICE ⇒ 整项不显示（不是「办公室」）；无批次 ⇒ 同样不显示。
    assert!(all.contains(">   └ F5004 件C-4 ×7"), "{all}");
    assert!(all.contains(">   └ 件C-5 ×8"), "{all}");
    // 序列号 NULL ⇒ 留空（不显示「无」、不回退图号）。
    assert!(all.contains("> 件LOOSE-1 ×3"), "{all}");
    assert!(!all.contains("无"), "序列号缺省不许编占位字：{all}");
    // 装配件**算 1 单**、子件不重复计入（工单级行单位）。
    assert_eq!(data(&env)["overdue_count"].as_i64(), Some(2));
}

/// ★ 集成守卫：`order_list_limit` 截断 + **组末一行**脚注，且子件与工单**同额**扣减；
/// `0` = 不限时不截断、不出脚注。
#[tokio::test]
async fn orders_section_truncates_at_order_list_limit_with_one_footer() {
    let ctx = bootstrap().await;
    let cid = ctx.customer_id;
    // 一套装配件（3 个子件）+ 2 张散件 = **6 行 / 3 单**（工单级行单位）。
    let asm = insert_assembly_serialized(
        &ctx.pool,
        cid,
        Some("F7000"),
        "ASM-T",
        1,
        Some(today().pred_days(1)),
    )
    .await;
    for (i, serial) in [(0, "F7001"), (1, "F7002"), (2, "F7003")] {
        let c = insert_child_part(&ctx.pool, cid, asm, Some(serial), &format!("T-{i}"), 1).await;
        insert_batch_at(
            &ctx.pool,
            c,
            "IN_PROCESS",
            Some("WORKER"),
            Some(insert_worker(&ctx.pool, &format!("工{i}")).await),
        )
        .await;
    }
    insert_part_serialized(
        &ctx.pool,
        cid,
        Some("F7004"),
        "T-A",
        "IN_PROCESS",
        1,
        Some(today().pred_days(1)),
    )
    .await;
    insert_part_serialized(
        &ctx.pool,
        cid,
        Some("F7005"),
        "T-B",
        "IN_PROCESS",
        1,
        Some(today().pred_days(1)),
    )
    .await;

    // ⚠️ 本用例**刻意走落库配置路径**（先 `POST /config` 再 `/preview`），不走
    // `PreviewRequest.order_list_limit` 的请求级覆盖：那才是管理员在配置页改完「最多
    // 列出工单」之后的真实链路（配置已落库、预览读的就是它）。
    let (st, env) = post_as_manager(
        &ctx,
        "/cron/wecom-daily/config",
        json!({
            "version": 0,
            "enabled": true,
            "frequency": "DAILY",
            "send_hour": 20,
            "send_minute": 5,
            "webhook_id": ctx.webhook_id.to_string(),
            "upcoming_days": 3,
            "worker_metric": "QUANTITY",
            "worker_top_n": 15,
            "order_list_limit": 3,
            "max_chunks": 6,
            "include_overview": true,
            "include_delivery": true,
            "include_orders": true,
            "include_workers": true,
            "include_drawings": true,
            "subject_template": null
        }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{env}");

    // 额度 3：装配件行 + 头两个子件 = 3 行，脚注报未列出的 3 **行**（第三个子件 +
    // 两张散件）。⚠️ 脚注数的是**行**不是工单：6 行 − 3 行 = 3。
    let (_, capped) = post_as_manager(&ctx, "/cron/wecom-daily/preview", json!({})).await;
    let all = chunks_of(&capped).join("\n");
    assert!(all.contains("> F7000 装ASM-T ×1"), "{all}");
    assert!(all.contains(">   └ F7001 件T-0 ×1"), "{all}");
    assert!(all.contains(">   └ F7002 件T-1 ×1"), "{all}");
    assert!(!all.contains("F7003"), "额度用完，后面的行不再出：{all}");
    assert!(
        all.contains("> <font color=\"comment\">另有 3 行未列出</font>"),
        "{all}"
    );
    assert_eq!(
        all.matches("另有").count(),
        1,
        "子件截断**不单独提示**，整组只出一行脚注：{all}"
    );
    assert_eq!(
        data(&capped)["overdue_count"].as_i64(),
        Some(3),
        "「N 条」是未截断的匹配工单数（装配件 1 + 散件 2），不受 order_list_limit 影响"
    );
    assert!(all.contains("> **逾期未交 · 3 条**"), "{all}");

    // 额度 0 = 不限：6 行全出、无脚注（改配置要走标准 OCC：先读回 version 再覆写）。
    let (cur, cur_env) = get_as_manager(&ctx, "/cron/wecom-daily/config").await;
    assert_eq!(cur, StatusCode::OK, "{cur_env}");
    let version = data(&cur_env)["version"].as_i64().expect("version 是整数");
    let (st, env2) = post_as_manager(
        &ctx,
        "/cron/wecom-daily/config",
        json!({
            "version": version,
            "enabled": true,
            "frequency": "DAILY",
            "send_hour": 20,
            "send_minute": 5,
            "webhook_id": ctx.webhook_id.to_string(),
            "upcoming_days": 3,
            "worker_metric": "QUANTITY",
            "worker_top_n": 15,
            "order_list_limit": 0,
            "max_chunks": 6,
            "include_overview": true,
            "include_delivery": true,
            "include_orders": true,
            "include_workers": true,
            "include_drawings": true,
            "subject_template": null
        }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{env2}");
    let (_, full) = post_as_manager(&ctx, "/cron/wecom-daily/preview", json!({})).await;
    let all = chunks_of(&full).join("\n");
    for serial in ["F7000", "F7001", "F7002", "F7003", "F7004", "F7005"] {
        assert!(all.contains(serial), "{serial} 在「不限」下必须出现：{all}");
    }
    assert!(!all.contains("另有"), "不限时不出脚注：{all}");
}

/// ★ 集成守卫：**空清单**时第三节只出小标题（不出组标题、不出「无」行）。
#[tokio::test]
async fn empty_orders_section_renders_heading_only() {
    let ctx = bootstrap().await;
    let (status, env) = post_as_manager(&ctx, "/cron/wecom-daily/preview", json!({})).await;
    assert_eq!(status, StatusCode::OK, "{env}");
    let all = chunks_of(&env).join("\n");
    assert!(all.contains("## 三、逾期未交 / 3 日内交期"), "{all}");
    assert!(!all.contains("逾期未交 ·"), "空清单不出组标题：{all}");
    assert!(!all.contains("另有"), "空清单不出脚注：{all}");
    // 节的 slug 仍然在 `sections[]` 里（管理员能看到这一节是开着的）。
    let sections: Vec<&str> = data(&env)["sections"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(sections.contains(&"orders"), "{sections:?}");
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

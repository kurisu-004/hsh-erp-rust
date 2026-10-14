//! `POST /api/v2/serials/release-terminal` 集成测试（**2026-10-19 新增**）
//!
//! 端点用途：把「已完成 / 已取消但仍占着序列号」的工单（装配件 + 零件 / 子件两张表）
//! 一键清号。序列号被 `uk_t_assembly_serial_no` / `uk_t_part_serial_no` 守着，
//! **泄漏即永久占位**，新工单取号会被直接挡住 ⇒ 这是个破坏性但必须有的维护端点。
//!
//! ## 用例清单
//!
//! | # | 用例 | 钉的是什么 |
//! |---|---|---|
//! | 1 | `dry_run_returns_counts_without_writing` | 预览**零写入**（两表 `serial_no` / `version` 原封不动、**无**新 `SERIAL_RELEASED` 行）+ 计数正确（装配件 1 / 零件 2）+ **IN_PROCESS 行不进候选** |
//! | 2 | `apply_releases_both_tables_and_archives_parts` | 两表都清、`version` 恰好 +1、**IN_PROCESS 行原封不动**、子件**恰好 2 条**归档事件且 note 含原序列号 / part_id 对得上 / to_status 等于释放时状态 |
//! | 3 | `apply_is_idempotent` | 连调两次，第二次 `released_total == 0`、`affected` 空、DB 无进一步变化 |
//! | 4 | `clerk_is_forbidden` | MANAGER **单角色** ⇒ 403 / 信封码 40300 / **零副作用** |
//! | 5 | `truncated_flag_and_limit` | `truncated == true` 且 `affected.len() <= 200`，**而 `released_total` 是真实总数**（计数与明细解耦）|
//!
//! ## 为什么要单独钉「IN_PROCESS 不被碰」
//!
//! 本端点的全部价值在于「只清终态」。守卫一旦漂移成「清所有持号行」，后果是
//! **正在生产的工单被抽走序列号**（工人扫码立刻对不上号）——这是本端点最严重的
//! 可能故障形态，而且它是**静默的**：行还在、status 没变，只有号没了。用例 1 / 2
//! 各埋一行 IN_PROCESS 装配件专门钉这条。
//!
//! ## 归档事件为什么必须断言 `note` 与 `to_status`
//!
//! 零件侧走的是「**先归档、再清列**」的既有原语。归档行的价值全在 `note`
//! （记着被释放的原序列号）与 `to_status`（释放时那行是什么态）—— 只断言
//! 「有 2 条 `SERIAL_RELEASED`」的话，原语退化成「直接清列、一条事件都不写」
//! 之外的任何**半吊子实现**（写了事件但内容是空的）都能通过。

use axum::http::StatusCode;
use serde_json::{Value, json};
use sqlx::PgPool;

use hsh_erp_rust::infra::clock::now_naive;
use hsh_erp_test_support::{json_request, send};

use crate::{
    Ctx, assert_envelope, insert_assembly_with_serial, insert_part_with_serial, snapshot_assembly,
    snapshot_part,
};

/// 本域明细行数上限（与 `dto::RELEASE_DETAIL_LIMIT` 同值）。
///
/// ⚠️ 这里是**测试自己写的字面量**而不是 import 被测常量 —— 理由：若测试直接
/// `use` 被测常量，将来有人把上限从 200 改成 10，用例 5 仍会绿（它种 201 行、
/// 按同一个常量判），而**前端与文档里承诺的「最多 200 行」已经变了**。写死在这里
/// 就是让它在上限被改时当场红。
const LIMIT: i64 = 200;

/// 打本端点。
async fn release(ctx: &Ctx, dry_run: bool, bearer: &str) -> (StatusCode, Value) {
    send(
        ctx.app.clone(),
        json_request(
            "POST",
            "/serials/release-terminal",
            Some(json!({ "dry_run": dry_run })),
            Some(bearer),
        ),
    )
    .await
}

/// 标准种子：**1 个 COMPLETED 装配件 + 1 个 COMPLETED 零件 + 1 个 CANCELLED 零件
/// + 1 个 IN_PROCESS 装配件（对照组）**。
///
/// 预期候选：装配件 1（IN_PROCESS 那行被守卫挡住）+ 零件 2 = `released_total 3`。
struct Seed {
    completed_assembly: i64,
    completed_part: i64,
    cancelled_part: i64,
    in_process_assembly: i64,
}

async fn seed(ctx: &Ctx) -> Seed {
    Seed {
        completed_assembly: insert_assembly_with_serial(
            ctx,
            "COMPLETED",
            "ASM-COMPL-001",
            "D-ASM-COMPLETED",
        )
        .await,
        completed_part: insert_part_with_serial(ctx, "COMPLETED", "PT-COMPL-001", "D-PT-1").await,
        cancelled_part: insert_part_with_serial(ctx, "CANCELLED", "PT-CANCEL-01", "D-PT-2").await,
        in_process_assembly: insert_assembly_with_serial(
            ctx,
            "IN_PROCESS",
            "ASM-INPROC-01",
            "D-ASM-IN-PROCESS",
        )
        .await,
    }
}

/// 全部 `SERIAL_RELEASED` 归档事件行：`(part_id, note, to_status)`。
async fn serial_released_events(pool: &PgPool) -> Vec<(i64, Option<String>, Option<String>)> {
    sqlx::query_as(
        "SELECT part_id, note, to_status FROM t_part_event \
         WHERE event_type = 'SERIAL_RELEASED' ORDER BY part_id ASC",
    )
    .fetch_all(pool)
    .await
    .expect("查 SERIAL_RELEASED 归档事件")
}

/// 一次成功响应的出参（断言形状用）。
fn data_of(env: &Value) -> &Value {
    assert_eq!(env["code"], 0, "调用应成功：{env}");
    &env["data"]
}

// ===========================================================================
//  1. dry_run：计数正确 + 零写入
// ===========================================================================

#[tokio::test]
async fn dry_run_returns_counts_without_writing() {
    let ctx = crate::bootstrap().await;
    let s = seed(&ctx).await;

    // 四行的「前」快照（`version` 初值恒为 7）
    let before_asm = snapshot_assembly(&ctx.pool, s.completed_assembly).await;
    let before_pt = snapshot_part(&ctx.pool, s.completed_part).await;
    let before_cancel = snapshot_part(&ctx.pool, s.cancelled_part).await;
    let before_in_proc = snapshot_assembly(&ctx.pool, s.in_process_assembly).await;
    assert_eq!(
        before_asm.0.as_deref(),
        Some("ASM-COMPL-001"),
        "种子前提：COMPLETED 装配件应持号"
    );

    let (status, env) = release(&ctx, true, &ctx.manager_token).await;
    assert_envelope(status, &env, StatusCode::OK, 0);
    let data = data_of(&env);

    // 计数：装配件 1（IN_PROCESS 被守卫挡住）+ 零件 2（COMPLETED + CANCELLED）
    assert_eq!(data["assembly_count"], 1, "装配件候选应恰为 1：{data}");
    assert_eq!(data["part_count"], 2, "零件候选应恰为 2：{data}");
    assert_eq!(data["released_total"], 3, "总数应为 3：{data}");
    assert_eq!(data["dry_run"], true, "dry_run 应回显 true：{data}");
    assert_eq!(data["truncated"], false, "3 行远未到上限：{data}");

    // 明细：3 条，装配件在前
    let affected = data["affected"].as_array().expect("affected 是数组");
    assert_eq!(affected.len(), 3, "明细应有 3 行：{data}");
    assert_eq!(affected[0]["kind"], "ASSEMBLY", "明细装配件在前：{data}");
    assert_eq!(affected[0]["serial_no"], "ASM-COMPL-001");
    assert_eq!(affected[0]["status"], "COMPLETED");
    assert_eq!(affected[1]["kind"], "PART", "明细子件在后：{data}");
    assert_eq!(affected[2]["kind"], "PART");
    // 雪花 id 序列化成字符串（不能是 JSON 数字）
    assert!(
        affected[0]["id"].is_string(),
        "雪花 id 必须序列化成字符串：{data}"
    );

    // ★ 零写入：两表的 serial_no / version 全部原封不动
    assert_eq!(
        snapshot_assembly(&ctx.pool, s.completed_assembly).await,
        before_asm,
        "dry_run 不得改动 COMPLETED 装配件"
    );
    assert_eq!(
        snapshot_part(&ctx.pool, s.completed_part).await,
        before_pt,
        "dry_run 不得改动 COMPLETED 零件"
    );
    assert_eq!(
        snapshot_part(&ctx.pool, s.cancelled_part).await,
        before_cancel,
        "dry_run 不得改动 CANCELLED 零件"
    );
    assert_eq!(
        snapshot_assembly(&ctx.pool, s.in_process_assembly).await,
        before_in_proc,
        "dry_run 不得改动 IN_PROCESS 装配件"
    );

    // ★ 零归档事件
    assert!(
        serial_released_events(&ctx.pool).await.is_empty(),
        "dry_run 绝不能写 SERIAL_RELEASED 归档事件（它是只读预览）"
    );
}

// ===========================================================================
//  2. apply：两表清空 + 子件归档
// ===========================================================================

#[tokio::test]
async fn apply_releases_both_tables_and_archives_parts() {
    let ctx = crate::bootstrap().await;
    let s = seed(&ctx).await;

    let (status, env) = release(&ctx, false, &ctx.manager_token).await;
    assert_envelope(status, &env, StatusCode::OK, 0);
    let data = data_of(&env);
    assert_eq!(data["dry_run"], false);
    assert_eq!(data["assembly_count"], 1, "装配件应清 1 行：{data}");
    assert_eq!(data["part_count"], 2, "零件应清 2 行：{data}");
    assert_eq!(data["released_total"], 3);

    // 两表三行：serial_no 清空、version 恰好 +1（初值 7 ⇒ 8）
    for (id, label) in [
        (s.completed_assembly, "COMPLETED 装配件"),
        (s.completed_part, "COMPLETED 零件"),
        (s.cancelled_part, "CANCELLED 零件"),
    ] {
        let is_asm = id == s.completed_assembly;
        let (serial, version) = if is_asm {
            snapshot_assembly(&ctx.pool, id).await
        } else {
            snapshot_part(&ctx.pool, id).await
        };
        assert_eq!(serial, None, "{label} 的序列号应已清空");
        assert_eq!(version, 8, "{label} 的 version 应恰好 +1（初值 7）");
    }

    // ★ IN_PROCESS 装配件原封不动（守卫回归）
    let (in_proc_serial, in_proc_version) =
        snapshot_assembly(&ctx.pool, s.in_process_assembly).await;
    assert_eq!(
        in_proc_serial.as_deref(),
        Some("ASM-INPROC-01"),
        "★ IN_PROCESS 装配件的序列号必须原封不动（守卫漂移会抽走生产中工单的号）"
    );
    assert_eq!(in_proc_version, 7, "IN_PROCESS 装配件的 version 不该动");

    // ★ 归档事件：恰好 2 条，part_id / note / to_status 逐项对得上
    let events = serial_released_events(&ctx.pool).await;
    assert_eq!(
        events.len(),
        2,
        "每个被清号的零件应恰好 1 条归档事件：{events:?}"
    );

    let by_part: std::collections::HashMap<i64, (Option<String>, Option<String>)> = events
        .into_iter()
        .map(|(pid, note, to)| (pid, (note, to)))
        .collect();

    for (pid, serial, status) in [
        (s.completed_part, "PT-COMPL-001", "COMPLETED"),
        (s.cancelled_part, "PT-CANCEL-01", "CANCELLED"),
    ] {
        let (note, to_status) = by_part
            .get(&pid)
            .unwrap_or_else(|| panic!("零件 {pid} 缺 SERIAL_RELEASED 归档事件：{by_part:?}"));
        assert!(
            note.as_deref().unwrap_or_default().contains(serial),
            "零件 {pid} 的归档 note 应含被释放的原序列号 {serial}，实际 {note:?}"
        );
        assert_eq!(
            to_status.as_deref(),
            Some(status),
            "零件 {pid} 的归档 to_status 应等于它释放时的状态"
        );
    }
}

// ===========================================================================
//  3. 幂等
// ===========================================================================

#[tokio::test]
async fn apply_is_idempotent() {
    let ctx = crate::bootstrap().await;
    seed(&ctx).await;

    let (status, first) = release(&ctx, false, &ctx.manager_token).await;
    assert_envelope(status, &first, StatusCode::OK, 0);
    assert_eq!(data_of(&first)["released_total"], 3, "首次应清 3 行");

    // 第一次跑完的库状态快照（第二次不该再动任何东西）
    let assemblies_after_first: Vec<(Option<String>, i32)> =
        sqlx::query_as("SELECT serial_no, version FROM t_assembly ORDER BY id")
            .fetch_all(&ctx.pool)
            .await
            .expect("快照装配件");
    let parts_after_first: Vec<(Option<String>, i32)> =
        sqlx::query_as("SELECT serial_no, version FROM t_part ORDER BY id")
            .fetch_all(&ctx.pool)
            .await
            .expect("快照零件");
    let events_after_first = serial_released_events(&ctx.pool).await;
    assert_eq!(events_after_first.len(), 2, "首次应各写 1 条归档事件");

    // 第二次：无候选 ⇒ 计数 0、明细空、DB 无进一步变化
    let (status, second) = release(&ctx, false, &ctx.manager_token).await;
    assert_envelope(status, &second, StatusCode::OK, 0);
    let data = data_of(&second);
    assert_eq!(
        data["released_total"], 0,
        "★ 第二次应 0 行（幂等守卫：谓词带 serial_no IS NOT NULL）：{data}"
    );
    assert_eq!(data["assembly_count"], 0);
    assert_eq!(data["part_count"], 0);
    assert_eq!(data["truncated"], false);
    assert_eq!(
        data["affected"].as_array().map(|a| a.len()),
        Some(0),
        "第二次 affected 应为空：{data}"
    );

    let assemblies_now: Vec<(Option<String>, i32)> =
        sqlx::query_as("SELECT serial_no, version FROM t_assembly ORDER BY id")
            .fetch_all(&ctx.pool)
            .await
            .expect("复查装配件");
    let parts_now: Vec<(Option<String>, i32)> =
        sqlx::query_as("SELECT serial_no, version FROM t_part ORDER BY id")
            .fetch_all(&ctx.pool)
            .await
            .expect("复查零件");
    assert_eq!(
        assemblies_now, assemblies_after_first,
        "第二次不该再动装配件"
    );
    assert_eq!(parts_now, parts_after_first, "第二次不该再动零件");
    assert_eq!(
        serial_released_events(&ctx.pool).await.len(),
        events_after_first.len(),
        "第二次不该再写归档事件（版本不能再 +1）"
    );
}

// ===========================================================================
//  4. MANAGER 单角色守卫
// ===========================================================================

#[tokio::test]
async fn clerk_is_forbidden() {
    let ctx = crate::bootstrap().await;
    let s = seed(&ctx).await;

    let before_asm = snapshot_assembly(&ctx.pool, s.completed_assembly).await;
    let before_pt = snapshot_part(&ctx.pool, s.completed_part).await;

    let (status, env) = release(&ctx, false, &ctx.clerk_token).await;
    assert_envelope(status, &env, StatusCode::FORBIDDEN, 40300);

    // 零副作用：守卫必须**早于**任何全表扫与写入
    assert_eq!(
        snapshot_assembly(&ctx.pool, s.completed_assembly).await,
        before_asm,
        "被拒后装配件不该有任何变化"
    );
    assert_eq!(
        snapshot_part(&ctx.pool, s.completed_part).await,
        before_pt,
        "被拒后零件不该有任何变化"
    );
    assert!(
        serial_released_events(&ctx.pool).await.is_empty(),
        "被拒后不该写任何归档事件"
    );
}

// ===========================================================================
//  5. 明细截断，但计数是真实总数
// ===========================================================================

#[tokio::test]
async fn truncated_flag_and_limit() {
    let ctx = crate::bootstrap().await;

    // 种 LIMIT + 1 条终态持号零件 ⇒ 真实总数 201 > 200 ⇒ 必须截断
    let total = LIMIT + 1;
    let mut qb = sqlx::QueryBuilder::<sqlx::Postgres>::new(
        "INSERT INTO t_part (id, serial_no, name, drawing_no, applicant_name, quantity, \
         unit_price, total_price, request_date, planned_delivery_date, system_delivery_date, \
         status, is_urgent, customer_id, version, created_at, updated_at) ",
    );
    let now = now_naive();
    let today = now.date();
    qb.push_values(
        (1..=total).map(|i| {
            (
                crate::shared_id(),
                format!("BULK-{i:05}"),
                format!("批量件-{i:04}"),
                today,
                "COMPLETED",
                now,
            )
        }),
        |mut b, (id, serial, drawing, day, status, now)| {
            b.push_bind(id)
                .push_bind(serial)
                .push_bind("批量截断件")
                .push_bind(drawing)
                .push_bind("tester")
                .push_bind(1)
                .push_bind(0)
                .push_bind(0)
                .push_bind(day)
                .push_bind(day)
                .push_bind(day)
                .push_bind(status)
                .push_bind(false)
                .push_bind(ctx.customer_id)
                .push_bind(0)
                .push_bind(now)
                .push_bind(now);
        },
    );
    let inserted = qb
        .build()
        .execute(&ctx.pool)
        .await
        .expect("批量插 201 行")
        .rows_affected();
    assert_eq!(inserted as i64, total, "应插入 {total} 行");

    // ---- 预览：计数真实、明细截断 ----
    let (status, env) = release(&ctx, true, &ctx.manager_token).await;
    assert_envelope(status, &env, StatusCode::OK, 0);
    let data = data_of(&env);
    assert_eq!(
        data["part_count"], total,
        "★ part_count 必须是真实总数 201，不能被明细上限截断：{data}"
    );
    assert_eq!(
        data["released_total"], total,
        "★ released_total 必须是真实总数：{data}"
    );
    assert_eq!(
        data["truncated"], true,
        "★ 超过上限必须置 truncated：{data}"
    );
    let affected = data["affected"].as_array().expect("affected 是数组");
    assert_eq!(
        affected.len() as i64,
        LIMIT,
        "★ 明细必须恰好封顶 {LIMIT} 行：{data}"
    );

    // ---- 实跑：201 行真的全清了（明细截断**不得**连带截断写入）----
    let (status, env) = release(&ctx, false, &ctx.manager_token).await;
    assert_envelope(status, &env, StatusCode::OK, 0);
    let data = data_of(&env);
    assert_eq!(
        data["part_count"], total,
        "★ 实跑必须真的清掉全部 {total} 行（明细上限只约束回显，不约束写入）：{data}"
    );
    let still_held: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM t_part WHERE serial_no IS NOT NULL")
            .fetch_one(&ctx.pool)
            .await
            .expect("复查残留持号行");
    assert_eq!(still_held, 0, "实跑后不应再有持号零件行");
    assert_eq!(
        serial_released_events(&ctx.pool).await.len() as i64,
        total,
        "每个被清号的零件都应有 1 条归档事件"
    );
}

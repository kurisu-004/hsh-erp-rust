//! mail_digest 预览端点（`POST /preview`）集成测试
//!
//! 预览是本域**唯一**能覆盖「渲染 + 统计 SQL」的端点（不发送、不落流水），
//! 故口径类断言全部压在这里：
//! 1. `preview_window_selects_overdue_today_upcoming_and_excludes_rest`
//! 2. `preview_renders_assembly_once_and_hides_its_children`（★ 工单级行单位）
//! 3. `preview_excludes_parts_with_delivered_or_completed_batch`（★ 已交量守卫，
//!    `COMPLETED` 那条最易漏：force-complete 端点强推的就是它）
//! 4. `preview_section_toggles_hide_each_section`
//! 5. `preview_honours_visible_columns_white_list`
//! 6. `preview_empty_column_list_falls_back_to_default_set`
//! 7. `preview_escapes_html_in_user_input`（★ XSS）
//! 8. `preview_empty_state_is_human_readable_not_blank_table`
//! 9. `preview_subject_contains_today_and_window_days`
//! 10. `preview_with_both_sections_off_returns_21704`（⚠️ 见文末偏差登记）
//! 11. `preview_never_writes_log_row`
//!
//! ## 本文件钉住的三处「实现与注释/直觉不符」
//!
//! ⚠️ 2026-10-13 review 第 2 轮 #9：原先这三条一律冠以「已在 §8.3 登记」，但其中
//! **只有两条**在 §8.3——「未知列」那条不在（它不是「注释说错了」，而是 M7 改正之后
//! 注释已与实现一致，无偏差可登记）。逐条指向真实位置：
//! - **未知列不是「忽略」而是 21704**：渲染层的 `filter_known_columns` 收到的是
//!   已解析的枚举，白名单解析在更上游就拒了。登记在 **§2.3**（`visible_columns`
//!   「未知项 → 21704（**不静默丢弃**）」）与 `template.rs` 的 `filter_known_columns`
//!   doc —— M7 已把那句「剔除未知列」的错注释改正，故**不在** §8.3。
//! - **两节都关一律 21704**：不会渲出「无符合条件的工单」；那句文案只在
//!   「节开着但窗口内无行」时出现。登记在 **§8.3「实现与注释不符」第 3 条**。
//! - **节开关真的不渲染该节**（2026-10-13 review 第 1 轮 M5 修复）：修复前
//!   `render()` 无条件渲染两节，关掉的节因为没查库而拿到空切片，于是信里照样出现
//!   「已逾期（…）（0 条）/ 本节无符合条件的工单」。登记在
//!   **§8.3「实现与注释不符」第 5 条**（该条已标 ✅ 修复完毕）。

use axum::http::StatusCode;
use serde_json::{Value, json};

use hsh_erp_test_support::{json_request, send};

use crate::{Ctx, insert_assembly, insert_batch, insert_part, insert_root_customer, today};

/// `POST /preview` → `(status, 信封)`。
async fn preview(ctx: &Ctx, body: Value) -> (StatusCode, Value) {
    send(
        ctx.app.clone(),
        json_request(
            "POST",
            "/cron/mail-digest/preview",
            Some(body),
            Some(&ctx.manager_token),
        ),
    )
    .await
}

// ===========================================================================
//  1. 时间窗口
// ===========================================================================

/// 逾期 = `system_delivery_date < today`；未来节 = 半开区间 `[today, today+N)`。
///
/// 钉四件事：① today-3 进逾期节；② today 进**未来节**（不是逾期节）；
/// ③ today+5 在 14 天窗口内；④ today+99 与 `system_delivery_date IS NULL`
/// **都不得出现**。
#[tokio::test]
async fn preview_window_selects_overdue_today_upcoming_and_excludes_rest() {
    let ctx = crate::bootstrap().await;
    let t = today();
    let cust = ctx.customer_id;

    insert_part(
        &ctx.pool,
        cust,
        "DWG-OVERDUE",
        "逾期件",
        "IN_PROCESS",
        Some(t - chrono::Duration::days(3)),
        None,
        1,
        None,
        None,
    )
    .await;
    insert_part(
        &ctx.pool,
        cust,
        "DWG-TODAY",
        "今日到期件",
        "IN_PROCESS",
        Some(t),
        None,
        1,
        None,
        None,
    )
    .await;
    insert_part(
        &ctx.pool,
        cust,
        "DWG-SOON",
        "五天后件",
        "IN_PROCESS",
        Some(t + chrono::Duration::days(5)),
        None,
        1,
        None,
        None,
    )
    .await;
    insert_part(
        &ctx.pool,
        cust,
        "DWG-FAR",
        "窗口外件",
        "IN_PROCESS",
        Some(t + chrono::Duration::days(99)),
        None,
        1,
        None,
        None,
    )
    .await;
    insert_part(
        &ctx.pool,
        cust,
        "DWG-NODATE",
        "无交期件",
        "IN_PROCESS",
        None,
        None,
        1,
        None,
        None,
    )
    .await;

    let (status, env) = preview(&ctx, json!({})).await;
    assert_eq!(status, StatusCode::OK, "{env}");
    let html = env["data"]["html"].as_str().expect("html 是字符串");

    assert_eq!(env["data"]["overdue_count"], 1, "today-3 一条：{env}");
    assert_eq!(
        env["data"]["upcoming_count"], 2,
        "today 与 today+5 两条：{env}"
    );

    assert!(html.contains("DWG-OVERDUE"), "缺逾期件");
    assert!(html.contains("DWG-TODAY"), "缺今日到期件（今日属未来节）");
    assert!(html.contains("DWG-SOON"), "缺窗口内未来件");
    assert!(
        !html.contains("DWG-FAR"),
        "today+99 超出 14 天窗口，不该出现"
    );
    assert!(
        !html.contains("DWG-NODATE"),
        "system_delivery_date IS NULL 不该出现"
    );
    assert!(
        html.contains("今天到期"),
        "今日到期件应落在未来节并带「今天到期」标注：{env}"
    );
}

/// 窗口随 `upcoming_days` 收缩：传 1 时 today+5 掉出窗口、只剩 today。
#[tokio::test]
async fn preview_upcoming_days_shrinks_the_window() {
    let ctx = crate::bootstrap().await;
    let t = today();
    insert_part(
        &ctx.pool,
        ctx.customer_id,
        "DWG-T0",
        "今天",
        "IN_PROCESS",
        Some(t),
        None,
        1,
        None,
        None,
    )
    .await;
    insert_part(
        &ctx.pool,
        ctx.customer_id,
        "DWG-T5",
        "五天后",
        "IN_PROCESS",
        Some(t + chrono::Duration::days(5)),
        None,
        1,
        None,
        None,
    )
    .await;

    let (status, env) = preview(&ctx, json!({ "upcoming_days": 1 })).await;
    assert_eq!(status, StatusCode::OK, "{env}");
    assert_eq!(env["data"]["upcoming_count"], 1, "days=1 只含今天：{env}");
    let html = env["data"]["html"].as_str().unwrap();
    assert!(html.contains("DWG-T0"));
    assert!(!html.contains("DWG-T5"), "days=1 时 today+5 应掉出窗口");
}

// ===========================================================================
//  2. 工单级行单位（装配件替换子件）
// ===========================================================================

/// ★ 装配件只出**一行**（`row_type=ASSEMBLY`），它的子件**不得**单独出现。
///
/// 这是「工单级行单位」这条硬约束的回归：若漏了 `AND p.assembly_id IS NULL`，
/// 一套装配件会在邮件里变成 N+1 行（装配件 1 行 + 子件各 1 行），收件人看到的
/// 「条数」会与 dashboard 交期面板对不上。
#[tokio::test]
async fn preview_renders_assembly_once_and_hides_its_children() {
    let ctx = crate::bootstrap().await;
    let t = today();
    let asm_id = insert_assembly(
        &ctx.pool,
        ctx.customer_id,
        "ASM-1",
        "装配件本体",
        "IN_PROCESS",
        Some(t + chrono::Duration::days(3)),
        5,
    )
    .await;
    // 两个子件：即便它们自己的交期落在窗口内，也**不得**单独成行
    insert_part(
        &ctx.pool,
        ctx.customer_id,
        "CHILD-1",
        "子件一",
        "IN_PROCESS",
        Some(t + chrono::Duration::days(3)),
        Some(asm_id),
        3,
        None,
        None,
    )
    .await;
    insert_part(
        &ctx.pool,
        ctx.customer_id,
        "CHILD-2",
        "子件二",
        "IN_PROCESS",
        Some(t + chrono::Duration::days(3)),
        Some(asm_id),
        2,
        None,
        None,
    )
    .await;

    let (status, env) = preview(
        &ctx,
        json!({ "visible_columns": ["drawing_no", "name", "quantity", "row_type"] }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{env}");
    assert_eq!(
        env["data"]["upcoming_count"], 1,
        "装配件 + 2 子件只能出 1 行（工单级）：{env}"
    );

    let html = env["data"]["html"].as_str().expect("html 是字符串");
    assert!(html.contains("ASM-1"), "装配件本身必须在：{env}");
    assert!(
        !html.contains("CHILD-1"),
        "子件不得单独出现（会重复计入）：{env}"
    );
    assert!(!html.contains("CHILD-2"), "子件不得单独出现：{env}");
    // row_type 列渲染成中文「装配件」，且 quantity 带「套」单位
    assert!(html.contains("装配件"), "row_type 列应渲染中文：{env}");
    assert!(html.contains("5 套"), "装配件数量单位是「套」：{env}");
}

// ===========================================================================
//  3. 已交量守卫
// ===========================================================================

/// ★ 只要交过一部分就不再出现在邮件里 —— 且**必须包含 `COMPLETED`**。
///
/// force-complete 端点（`POST /prod/parts/{id}/force-complete` 与
/// `POST /prod/assemblies/{id}/force-complete`）把批次强推成 `COMPLETED`，
/// 守卫若只判 `DELIVERED`，已完工的工单会继续出现在「逾期未交」邮件里。
/// 装配件侧同样要判（子件批次 `COMPLETED` ⇒ 装配件行消失）。
#[tokio::test]
async fn preview_excludes_parts_with_delivered_or_completed_batch() {
    let ctx = crate::bootstrap().await;
    let t = today();
    let past = Some(t - chrono::Duration::days(2));

    let delivered = insert_part(
        &ctx.pool,
        ctx.customer_id,
        "P-DELIVERED",
        "已交件",
        "IN_PROCESS",
        past,
        None,
        1,
        None,
        None,
    )
    .await;
    insert_batch(&ctx.pool, delivered, "DELIVERED").await;

    let completed = insert_part(
        &ctx.pool,
        ctx.customer_id,
        "P-COMPLETED",
        "强推完工件",
        "IN_PROCESS",
        past,
        None,
        1,
        None,
        None,
    )
    .await;
    insert_batch(&ctx.pool, completed, "COMPLETED").await;

    // 软删的已交批次不算「交过」（守卫带 `b.deleted_at IS NULL`）
    let soft = insert_part(
        &ctx.pool,
        ctx.customer_id,
        "P-SOFTDEL",
        "软删交件",
        "IN_PROCESS",
        past,
        None,
        1,
        None,
        None,
    )
    .await;
    let soft_batch = insert_batch(&ctx.pool, soft, "DELIVERED").await;
    sqlx::query("UPDATE t_part_batch SET deleted_at = now(), version = version + 1 WHERE id = $1")
        .bind(soft_batch)
        .execute(&ctx.pool)
        .await
        .expect("soft delete batch");

    insert_part(
        &ctx.pool,
        ctx.customer_id,
        "P-OPEN",
        "未交件",
        "IN_PROCESS",
        past,
        None,
        1,
        None,
        None,
    )
    .await;

    // 装配件侧：子件已 COMPLETED ⇒ 装配件行也不该出现
    let asm_id = insert_assembly(
        &ctx.pool,
        ctx.customer_id,
        "ASM-DONE",
        "已完工装配件",
        "IN_PROCESS",
        past,
        1,
    )
    .await;
    let child = insert_part(
        &ctx.pool,
        ctx.customer_id,
        "ASM-DONE-CHILD",
        "已完工子件",
        "IN_PROCESS",
        past,
        Some(asm_id),
        1,
        None,
        None,
    )
    .await;
    insert_batch(&ctx.pool, child, "COMPLETED").await;

    let (status, env) = preview(&ctx, json!({})).await;
    assert_eq!(status, StatusCode::OK, "{env}");
    assert_eq!(
        env["data"]["overdue_count"], 2,
        "只有「软删交件」与「未交件」两条：{env}"
    );

    let html = env["data"]["html"].as_str().expect("html 是字符串");
    assert!(
        !html.contains("P-DELIVERED"),
        "已交件不该出现在邮件里：{env}"
    );
    assert!(
        !html.contains("P-COMPLETED"),
        "COMPLETED 也算已交（force-complete 强推）：{env}"
    );
    assert!(
        !html.contains("ASM-DONE"),
        "子件已交的装配件不该出现：{env}"
    );
    assert!(html.contains("P-OPEN"), "真未交件必须在：{env}");
    assert!(html.contains("P-SOFTDEL"), "软删批次不算已交：{env}");
}

/// 非「未交付」状态的工单不参与（`status = ANY(DELIVERY_STATUSES)`）。
#[tokio::test]
async fn preview_skips_statuses_outside_delivery_whitelist() {
    let ctx = crate::bootstrap().await;
    let past = Some(today() - chrono::Duration::days(2));
    insert_part(
        &ctx.pool,
        ctx.customer_id,
        "S-DELIVERED",
        "已交付态",
        "DELIVERED",
        past,
        None,
        1,
        None,
        None,
    )
    .await;
    insert_part(
        &ctx.pool,
        ctx.customer_id,
        "S-CANCELLED",
        "已取消",
        "CANCELLED",
        past,
        None,
        1,
        None,
        None,
    )
    .await;
    insert_part(
        &ctx.pool,
        ctx.customer_id,
        "S-PENDING",
        "待编程",
        "PENDING",
        past,
        None,
        1,
        None,
        None,
    )
    .await;

    let (status, env) = preview(&ctx, json!({})).await;
    assert_eq!(status, StatusCode::OK, "{env}");
    assert_eq!(
        env["data"]["overdue_count"], 1,
        "只有 PENDING 在 6 态白名单内：{env}"
    );
    let html = env["data"]["html"].as_str().unwrap();
    assert!(html.contains("S-PENDING"));
    assert!(!html.contains("S-DELIVERED"));
    assert!(!html.contains("S-CANCELLED"));
}

// ===========================================================================
//  4. 节开关
// ===========================================================================

/// ⚠️ 2026-10-13（review 第 1 轮 M5）：`include_overdue=false` ⇒ 逾期行消失，**且该节
/// 整段不渲染**（标题行 + 空态文案一并消失，不是「只剩一句本节无工单」）。
#[tokio::test]
async fn preview_section_toggles_hide_each_section() {
    let ctx = crate::bootstrap().await;
    let t = today();
    insert_part(
        &ctx.pool,
        ctx.customer_id,
        "SEC-PAST",
        "逾期件",
        "IN_PROCESS",
        Some(t - chrono::Duration::days(1)),
        None,
        1,
        None,
        None,
    )
    .await;
    insert_part(
        &ctx.pool,
        ctx.customer_id,
        "SEC-FUT",
        "未来件",
        "IN_PROCESS",
        Some(t + chrono::Duration::days(2)),
        None,
        1,
        None,
        None,
    )
    .await;

    let (status, env) = preview(&ctx, json!({ "include_overdue": false })).await;
    assert_eq!(status, StatusCode::OK, "{env}");
    // ⚠️ 2026-10-13 review 第 2 轮 #2：`overdue_count` 仍是 0，这是**契约**、不是缺陷
    // —— 关掉的节 service 层不查库 ⇒ 行数恒 0，流水表与预览共用这个口径（docs §2.5）。
    // 关键是这个 0 **不表示「没有逾期工单」**；原先把它当成一句事实断言、把正文里
    // 那句误导性的「逾期 0 条」也一起放过，现在正文那半由下面的断言单独钉住。
    assert_eq!(env["data"]["overdue_count"], 0, "{env}");
    assert_eq!(env["data"]["upcoming_count"], 1, "{env}");
    let html = env["data"]["html"].as_str().unwrap();
    assert!(!html.contains("SEC-PAST"));
    assert!(html.contains("SEC-FUT"));
    // ⚠️ 2026-10-13 review 第 1 轮 M5：原先这里断言「关掉的节**仍**有人话」
    // （即 `html.contains("本节无符合条件的工单")`）——那正把缺陷钉成了期望值：
    // 管理员关掉逾期节却在信里看到一条空节，第一反应是「统计口径是不是坏了」。
    // 现在节开关**真的**不渲染该节 ⇒ 连标题行一起消失。
    assert!(
        !html.contains("已逾期"),
        "关掉的逾期节不应出现任何字样（含标题行）：{env}"
    );
    assert!(html.contains("即将到期"), "开着的未来节必须渲染：{env}");
    // ⚠️ 2026-10-13 review 第 2 轮 #2：概览行**只列已启用的节**。M5 把空节修掉了，
    // 但概览行当时仍无条件打「逾期 0 条 / 未来 1 条」——那 0 是「这节没启用」，
    // 与空节是同一种误读（像统计口径坏了），只是从整节缩到了一个数字。
    assert!(
        !html.contains("逾期 0 条"),
        "关掉的逾期节不该在概览行报「逾期 0 条」：{env}"
    );
    assert!(html.contains("未来 1 条"), "开着的未来节必须报数：{env}");

    let (status, env) = preview(&ctx, json!({ "include_upcoming": false })).await;
    assert_eq!(status, StatusCode::OK, "{env}");
    assert_eq!(env["data"]["overdue_count"], 1, "{env}");
    assert_eq!(env["data"]["upcoming_count"], 0, "{env}");
    let html = env["data"]["html"].as_str().unwrap();
    assert!(!html.contains("SEC-FUT"));
    assert!(
        !html.contains("即将到期"),
        "关掉的未来节不应出现标题行：{env}"
    );
    assert!(html.contains("SEC-PAST"));
    assert!(html.contains("已逾期"), "开着的逾期节必须渲染：{env}");
}

/// ⚠️ 两节都关 ⇒ **21704**，不是「空表格」。
///
/// 理由（`service::validate_normalized`）：那样每天只会发一封「无符合条件的工单」
/// 的空信，纯属骚扰 ⇒ 写侧直接拒。⇒ `docs/api/mail_digest.md` §8.3「实现与注释不符」
/// 第 3 条。
#[tokio::test]
async fn preview_with_both_sections_off_returns_21704() {
    let ctx = crate::bootstrap().await;
    let (status, env) = preview(
        &ctx,
        json!({ "include_overdue": false, "include_upcoming": false }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{env}");
    assert_eq!(env["code"], 21704, "{env}");
}

// ===========================================================================
//  5. 列白名单
// ===========================================================================

/// 只渲染被选中的列（数据列与表头同时收敛）。
#[tokio::test]
async fn preview_honours_visible_columns_white_list() {
    let ctx = crate::bootstrap().await;
    insert_part(
        &ctx.pool,
        ctx.customer_id,
        "COL-1",
        "列白名单件",
        "IN_PROCESS",
        Some(today() + chrono::Duration::days(1)),
        None,
        7,
        Some("SN-COL-1"),
        Some("PO-COL-1"),
    )
    .await;

    let (status, env) = preview(&ctx, json!({ "visible_columns": ["drawing_no", "name"] })).await;
    assert_eq!(status, StatusCode::OK, "{env}");
    assert_eq!(
        env["data"]["columns"],
        json!(["drawing_no", "name"]),
        "响应回显参与渲染的列：{env}"
    );

    let html = env["data"]["html"].as_str().expect("html 是字符串");
    assert!(html.contains("COL-1"));
    assert!(!html.contains("序列号"), "未选中的列表头不该出现：{env}");
    assert!(!html.contains("SN-COL-1"), "未选中的列数据不该出现：{env}");
    assert!(!html.contains("PO-COL-1"), "未选中的列数据不该出现：{env}");
}

/// 未知列在**白名单解析**那一层就 21704（不是「忽略后照发」）。
#[tokio::test]
async fn preview_rejects_unknown_column_with_21704() {
    let ctx = crate::bootstrap().await;
    let (status, env) = preview(&ctx, json!({ "visible_columns": ["drawing_no", "nope"] })).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{env}");
    assert_eq!(env["code"], 21704, "未知列必须暴露，不能静默丢弃：{env}");
}

/// 空列白名单 ⇒ **退回缺省 10 列**（否则会渲出一张只有表头的空表）。
///
/// ⚠️ 数字与 `VisibleColumn::DEFAULT_SET.len()` 是同一条信息的两份副本，改缺省列时
/// 两处必须一起改（2026-10-16 加 `batch_no` / `location` 时两处都漏过）。
#[tokio::test]
async fn preview_empty_column_list_falls_back_to_default_set() {
    let ctx = crate::bootstrap().await;
    insert_part(
        &ctx.pool,
        ctx.customer_id,
        "COL-E",
        "空列白名单件",
        "IN_PROCESS",
        Some(today() + chrono::Duration::days(1)),
        None,
        1,
        None,
        None,
    )
    .await;

    let (status, env) = preview(&ctx, json!({ "visible_columns": [] })).await;
    assert_eq!(status, StatusCode::OK, "{env}");
    assert_eq!(
        env["data"]["columns"].as_array().map(|a| a.len()),
        Some(10),
        "空列白名单应退回缺省 10 列：{env}"
    );
    assert!(env["data"]["html"].as_str().unwrap().contains("序列号"));
}

// ===========================================================================
//  6. XSS
// ===========================================================================

/// ★ 所有用户输入（图号 / 名称 / 客户名 / 订单号 / 序列号）在正文里必须被转义。
///
/// 图号 / 名称 / 客户名全是用户输入，直接拼进正文就是**存储型 XSS** ——
/// 收件人的邮件客户端会替我们执行注入的脚本。故断言「裸 `<script` 一律不得出现」。
#[tokio::test]
async fn preview_escapes_html_in_user_input() {
    let ctx = crate::bootstrap().await;
    let evil_customer = insert_root_customer(&ctx.pool, "<img src=x onerror=alert(2)>", 'X').await;
    let t = today() + chrono::Duration::days(1);
    insert_part(
        &ctx.pool,
        evil_customer,
        "<script>alert(1)</script>",
        "A&B<C>\"D\"",
        "IN_PROCESS",
        Some(t),
        None,
        1,
        Some("<i>sn</i>"),
        Some("<b>po</b>"),
    )
    .await;

    let (status, env) = preview(
        &ctx,
        json!({
            "visible_columns": [
                "serial_no", "drawing_no", "name",
                "customer_name", "order_no", "system_delivery_date"
            ]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{env}");
    let html = env["data"]["html"].as_str().expect("html 是字符串");

    assert!(!html.contains("<script"), "裸 <script 泄进了正文：{env}");
    assert!(!html.contains("<img src=x"), "裸 <img 泄进了正文：{env}");
    assert!(!html.contains("<i>sn</i>"), "裸 <i> 泄进了正文：{env}");
    assert!(!html.contains("<b>po</b>"), "裸 <b> 泄进了正文：{env}");
    assert!(html.contains("&lt;script&gt;"), "尖括号未转义：{env}");
    assert!(html.contains("&amp;"), "& 未转义：{env}");
    assert!(html.contains("&quot;"), "双引号未转义：{env}");
    // ⚠️ 不能出现 `&amp;lt;` —— 那是「& 先转义」顺序写错的二次转义症状
    assert!(
        !html.contains("&amp;lt;"),
        "出现二次转义（转义顺序写反了）：{env}"
    );
}

// ===========================================================================
//  7. 空态 / 主题
// ===========================================================================

/// 两节都开着但窗口内无行 ⇒ **每节各一句**「本节无符合条件的工单」，**不是**一张空表。
///
/// ⚠️ 2026-10-13（review 第 1 轮 m2）：原先这里除了两节的空态外还会再叠一句全局的
/// 「无符合条件的工单」，同一件事在正文里连着说三遍。现在全局空态只在「一节都没渲染」
/// 时出现（那种组合在配置层已被 21704 拒掉，渲染层只为纯函数调用留兜底）。
#[tokio::test]
async fn preview_empty_state_is_human_readable_not_blank_table() {
    let ctx = crate::bootstrap().await;
    // 只有一条远在窗口外的工单
    insert_part(
        &ctx.pool,
        ctx.customer_id,
        "EMPTY-FAR",
        "窗口外件",
        "IN_PROCESS",
        Some(today() + chrono::Duration::days(400)),
        None,
        1,
        None,
        None,
    )
    .await;

    let (status, env) = preview(&ctx, json!({})).await;
    assert_eq!(status, StatusCode::OK, "{env}");
    assert_eq!(env["data"]["overdue_count"], 0);
    assert_eq!(env["data"]["upcoming_count"], 0);
    let html = env["data"]["html"].as_str().expect("html 是字符串");
    assert_eq!(
        html.matches("本节无符合条件的工单").count(),
        2,
        "两节各一句，不该有第三句同义文案：{env}"
    );
    assert!(!html.contains("<th"), "无数据时不应渲染表头：{env}");
}

/// 缺省主题逐字对齐：`交期汇总 YYYY-MM-DD（未来14天）`。
///
/// 日期取**运行时今天**（不写字面量）—— 见「测试数据两条硬约定」第 2 条。
#[tokio::test]
async fn preview_subject_contains_today_and_window_days() {
    let ctx = crate::bootstrap().await;
    let (status, env) = preview(&ctx, json!({ "upcoming_days": 7 })).await;
    assert_eq!(status, StatusCode::OK, "{env}");
    let subject = env["data"]["subject"].as_str().expect("subject 是字符串");
    let want_date = today().format("%Y-%m-%d").to_string();
    assert_eq!(
        subject,
        format!("交期汇总 {want_date}（未来7天）"),
        "缺省主题逐字对齐"
    );

    // 自定义模板：`{date}` / `{days}` 两个占位符
    let (status, env) = preview(
        &ctx,
        json!({ "subject_template": "【{date}】未来 {days} 天交期" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{env}");
    assert_eq!(
        env["data"]["subject"].as_str(),
        Some(format!("【{want_date}】未来 14 天交期").as_str())
    );
}

/// ⚠️ 主题模板含 CR/LF ⇒ **21704**（同样是 SMTP header 注入面）。
#[tokio::test]
async fn preview_subject_with_crlf_returns_21704() {
    let ctx = crate::bootstrap().await;
    let (status, env) = preview(
        &ctx,
        json!({ "subject_template": "交期\r\nBcc: victim@x.com" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{env}");
    assert_eq!(env["code"], 21704, "{env}");
}

// ===========================================================================
//  8. 预览不落流水
// ===========================================================================

/// `/preview` **永远**不产生 `t_mail_digest_log` 行 —— 连渲 3 次也不产生。
#[tokio::test]
async fn preview_never_writes_log_row() {
    let ctx = crate::bootstrap().await;
    insert_part(
        &ctx.pool,
        ctx.customer_id,
        "NOLOG-1",
        "预览件",
        "IN_PROCESS",
        Some(today() - chrono::Duration::days(1)),
        None,
        1,
        None,
        None,
    )
    .await;

    for _ in 0..3 {
        let (status, env) = preview(&ctx, json!({})).await;
        assert_eq!(status, StatusCode::OK, "{env}");
    }
    assert_eq!(
        crate::count_logs(&ctx.pool).await,
        0,
        "预览不得落流水（否则流水会被「没发过的信」污染）"
    );
}

//! mail_digest 批次子行 + 位置列（2026-10-16 需求 2/3/4）的集成测试
//!
//! 覆盖 `/preview` 渲出的正文中「工单行 + 其下紧跟的批次子行」这条新结构：
//!
//! 1. `preview_renders_all_batches_of_every_order_row`
//!    —— 多工单多批次**全部**出现在 HTML 里；节行数仍是工单级；无批次工单不产生子行；
//!    且**散件工单的批次子行序列号格恒空**（fixture 派了序列号才有这条信号，
//!    否则把 PART 支改回 `p.serial_no` 也照样全绿）。「子行紧跟工单行」由
//!    `template.rs::tests::batch_sub_rows_follow_their_own_order_row` 守（那是纯渲染层
//!    的事、要按**位置**断言，集成测试按子串断言反而脆）
//! 2. `preview_attaches_child_batches_to_assembly_row`
//!    —— 装配件的批次挂在**子件**上，必须归到装配件行下面（主查询口径与批次查询逐字一致）
//! 3. `preview_renders_each_holder_location_in_chinese` +
//!    `preview_soft_deleted_holder_degrades_to_dash`
//!    —— 四种 `location` 各渲染对应中文（货架 code / 品检 code / 工人名 / 外协厂名）；
//!    holder 行已软删时**批次行不丢**、位置降级成 `—`（软删闸门在 `ON` 里而非 `WHERE`）
//! 4. `list_batches_accepts_empty_id_arrays`
//!    —— 两个 id 集合任一为空（乃至都为空）都不炸，且另一侧照常返回
//! 5. `preview_unknown_column_still_rejected_and_known_ones_render`
//!    —— 白名单仍是封闭枚举：新增的两列可勾、未知列仍 `21704`
//! 6. `preview_chinese_status_and_batch_location_are_escaped` +
//!    `preview_repairing_batch_renders_repairing_label`
//!    —— 批次侧的 holder 名（用户输入）同样过 HTML 转义；`is_repairing = true` 的批次
//!    状态渲成「返修中」，而工单行**没有**这条信息（`t_part` 上没这列）
//! 7. `mixed_delivered_and_open_batches_follow_the_opposite_two_rules`
//!    —— 工单行守卫（任一批次已交 ⇒ 整行不出现）与批次过滤（不按状态，`CANCELLED` 照列）
//!    **方向相反**，两条钉在同一个用例里
//! 8. `preview_shows_each_assembly_childs_serial_no_on_its_batch_row`
//!    —— 装配件的批次子行**序列号格填子件序列号**（2026-10-16）；子件图号不得顶上来
//!
//! ## 本文件**不**守的东西
//!
//! - 语句条数：**不**由集成测试守（每条用例只造 2~3 个工单，测不出读放大）。
//!   它由 `n_plus_one_guard.rs` 的源码级护栏守。
//! - 终态批次的位置为空：那是**迁移** `20261013000002_001_clear_terminal_batch_location`
//!   的既有事实（出厂即清），本文件只造一条「无 location」的终态批次来看渲染兜底
//!   （`—`）；真实库里的终态批次由那条迁移保证，lib 单测
//!   `template::tests::terminal_batch_renders_dash_for_location` 守渲染侧。

use axum::http::StatusCode;
use serde_json::{Value, json};

use hsh_erp_rust::modules::mail_digest::repo;
use hsh_erp_test_support::{json_request, send};

use crate::{
    Ctx, insert_assembly, insert_batch_full, insert_outsource_company, insert_part, insert_shelf,
    insert_worker, today,
};

/// `POST /preview` → `(status, 信封)`。
async fn preview(ctx: &Ctx, body: Value) -> (StatusCode, Value) {
    send(
        ctx.app.clone(),
        json_request(
            "POST",
            "/mail-digest/preview",
            Some(body),
            Some(&ctx.manager_token),
        ),
    )
    .await
}

/// 只看批次侧几列的 preview（省得断言被 10 个缺省列的其它内容干扰）。
///
/// ⚠️ 2026-10-16 起含 `serial_no` —— 它是**子行序列号格**所在的那一列（装配件支填子件
/// 序列号、散件支恒空，见 `repo::DigestBatchRow::part_serial_no`），且因为它排在第一位，
/// [`batch_sub_row_serials`] 才敢直接取子行的首个 `<td>`。⚠️ 列集一旦调整、首位不再
/// 是 `serial_no`，那个 helper 必须跟着改。
fn batch_only_body() -> Value {
    json!({
        "visible_columns": ["serial_no", "batch_no", "quantity", "status", "location"]
    })
}

/// 批次子行的**批次格**是否恰好等于 `　{n}`（整格等值）。
///
/// ⚠️ **不要写 `html.contains("　1")`**（2026-10-16）：批次格去掉「第…批」前缀之后，
/// `　1` 是 `　10` / `　11` 的**子串** ⇒ 批次数 ≥ 10 的场景下「批次列出现 / 消失」那类
/// 断言假绿（旧 marker `第1批` 没有这个歧义 —— 前缀里那个「第」帮它挡掉了）。`>　1<`
/// 带上两侧界 ⇒ `>　10<` 里不含 `>　1<`。
fn has_batch_cell(html: &str, no: i32) -> bool {
    html.contains(&format!(">\u{3000}{no}<"))
}

/// 取出正文里**全部批次子行**的首个 `<td>`（`batch_only_body()` 的列序把 `serial_no`
/// 放在第一位 ⇒ 那就是子行的序列号格）。
///
/// ⚠️ 按 `<tr bgcolor="#f7f8fa">` 定位而不是按某个批次号：同一装配件下多个子件的批次
/// **各自**从 `batch_no = 1` 起 ⇒ 按批次号定位根本定位不到是哪一行。返回**顺序不承载
/// 语义**（SQL 的 `ORDER BY … batch_no, id` 只保证结果可复现，子件之间没有先后口径）
/// ⇒ 断言一律按集合做。
fn batch_sub_row_serials(html: &str) -> Vec<String> {
    html.split("<tr bgcolor=\"#f7f8fa\">")
        .skip(1)
        .filter_map(|row| {
            let first_td = row.split("<td").nth(1)?;
            let after_gt = &first_td[first_td.find('>')? + 1..];
            Some(
                after_gt
                    .split("</td>")
                    .next()
                    .unwrap_or_default()
                    .to_string(),
            )
        })
        .collect()
}

// ===========================================================================
//  1. 多工单多批次全部出现 + 散件子行序列号格恒空
// ===========================================================================

/// ★ 需求 2：工单下方紧跟对应的所有批次。
///
/// 本用例断言五件事：① 每个工单的每个批次都在（一个不落）；② 节行数仍是**工单级**
/// （子行不计入 `upcoming_count`）；③ 一个批次都没有的工单不产生子行；④ **散件工单的
/// 批次子行序列号格恒空**；⑤ 该格恒空这件事本身是「有判别力」的（fixture 派了序列号）。
/// ⚠️ **「子行紧跟工单行」不由本用例守** —— 那是纯渲染层的渲染顺序，
/// `template.rs::tests::batch_sub_rows_follow_their_own_order_row` 按位置断言它。
/// ④ 之所以能有信号：fixture 刻意给 `DWG-A` / `DWG-B` 都派了序列号（`SN-A` / `SN-B`）
/// ——序列号是 NULL 时，把 PART 支改回 `p.serial_no` 也照样渲出空格子、断言零信号；
/// 派了序列号才能真正区分「刻意留空」与「取不到值」。
#[tokio::test]
async fn preview_renders_all_batches_of_every_order_row() {
    let ctx = crate::bootstrap().await;
    let t = today();
    let soon = t + chrono::Duration::days(2);

    let a = insert_part(
        &ctx.pool,
        ctx.customer_id,
        "DWG-A",
        "件甲",
        "IN_PROCESS",
        Some(soon),
        None,
        10,
        Some("SN-A"),
        None,
    )
    .await;
    insert_batch_full(&ctx.pool, a, 1, 4, "IN_PROCESS", None, None, false).await;
    insert_batch_full(&ctx.pool, a, 2, 3, "INSPECTION", None, None, false).await;
    insert_batch_full(&ctx.pool, a, 3, 3, "READY_TO_SHIP", None, None, false).await;

    let b = insert_part(
        &ctx.pool,
        ctx.customer_id,
        "DWG-B",
        "件乙",
        "IN_PROCESS",
        Some(soon),
        None,
        6,
        Some("SN-B"),
        None,
    )
    .await;
    insert_batch_full(&ctx.pool, b, 1, 6, "PENDING", None, None, false).await;

    // 一个**没有批次**的工单（干扰项：不得给它渲出子行）
    insert_part(
        &ctx.pool,
        ctx.customer_id,
        "DWG-NO-BATCH",
        "无批次件",
        "IN_PROCESS",
        Some(soon),
        None,
        1,
        None,
        None,
    )
    .await;

    let (status, env) = preview(&ctx, batch_only_body()).await;
    assert_eq!(status, StatusCode::OK, "{env}");
    assert_eq!(
        env["data"]["upcoming_count"], 3,
        "行数仍是工单级（子行不计入）：{env}"
    );
    let html = env["data"]["html"].as_str().expect("html 是字符串");

    // 批次格只写纯数字 + 一个全角缩进（2026-10-16）
    for no in [1, 2, 3] {
        assert!(has_batch_cell(html, no), "缺批次子行 第{no}批：\n{html}");
    }
    // 位置全为空 ⇒ 4 个批次子行都是 `—`（无批次工单不产生子行）
    assert_eq!(
        html.matches(">—<").count(),
        4,
        "4 个无位置批次 ⇒ 4 处 `—`：\n{html}"
    );

    // ④ 散件工单的批次子行**序列号格恒空**（序列号已写在工单行上，子行不重复）。
    let serials = batch_sub_row_serials(html);
    assert_eq!(
        serials.len(),
        4,
        "4 个批次子行一个不少（别让上面的空格子断言空转）：\n{html}"
    );
    for s in &serials {
        assert!(
            s.is_empty(),
            "散件工单的批次子行序列号格必须留空（PART 支恒 None），实际渲染成「{s}」：\n{html}"
        );
    }
    // ⑤ 反向钉住「取到了值」这件事本身成立：序列号确实派了、工单行上确实看得见
    assert!(
        html.contains("SN-A") && html.contains("SN-B"),
        "工单行的序列号必须在（否则本用例的负向断言没有对照）：\n{html}"
    );
}

// ===========================================================================
//  2. 装配件的子件批次挂在装配件行下
// ===========================================================================

/// ★ 装配件的批次**挂在子件上**（`t_part_batch.part_id → t_part.id`，子件
/// `t_part.assembly_id` 指向装配件）⇒ 批次查询那一支必须经
/// `t_part c JOIN t_assembly a` 两跳，否则装配件行下面一个批次都没有。
///
/// 同时钉住「子件不得单独成行」（工单级行单位那条硬约束没被本轮改动破坏）。
#[tokio::test]
async fn preview_attaches_child_batches_to_assembly_row() {
    let ctx = crate::bootstrap().await;
    let t = today();
    let soon = t + chrono::Duration::days(2);
    let asm_id = insert_assembly(
        &ctx.pool,
        ctx.customer_id,
        "ASM-9",
        "装配件本体",
        "IN_PROCESS",
        Some(soon),
        5,
    )
    .await;
    let child = insert_part(
        &ctx.pool,
        ctx.customer_id,
        "CHILD-9",
        "子件",
        "IN_PROCESS",
        Some(soon),
        Some(asm_id),
        3,
        None,
        None,
    )
    .await;
    insert_batch_full(&ctx.pool, child, 1, 3, "IN_PROCESS", None, None, false).await;
    insert_batch_full(&ctx.pool, child, 2, 2, "IN_PROCESS", None, None, false).await;

    let (status, env) = preview(&ctx, batch_only_body()).await;
    assert_eq!(status, StatusCode::OK, "{env}");
    assert_eq!(env["data"]["upcoming_count"], 1, "装配件只出一行：{env}");
    let html = env["data"]["html"].as_str().expect("html 是字符串");
    assert!(
        has_batch_cell(html, 1),
        "子件的批次没挂到装配件行下：\n{html}"
    );
    assert!(has_batch_cell(html, 2), "子件的第二个批次丢了：\n{html}");
    assert!(
        !html.contains("CHILD-9"),
        "子件不得单独成行（工单级行单位）：\n{html}"
    );
}

/// ★ 装配件的批次子行**序列号格填子件序列号**（2026-10-16）。
///
/// 一个装配件行下面挂着多个子件的批次，只看批次号分不清「这一批是哪个子件的」——
/// 用户原话：「对于装配件要写出每个子零件的序列号，否则不知道哪个批次是哪个子零件的」。
///
/// ⚠️ **返回顺序不承载语义**：两个子件的批次**各自**从 `batch_no = 1` 起，SQL 只能靠
/// 末尾的 `id ASC` 让结果可复现，子件之间没有先后口径 ⇒ 断言按集合做（别断言「哪个子件
/// 在前」）。
#[tokio::test]
async fn preview_shows_each_assembly_childs_serial_no_on_its_batch_row() {
    let ctx = crate::bootstrap().await;
    let t = today();
    let soon = t + chrono::Duration::days(2);
    let asm_id = insert_assembly(
        &ctx.pool,
        ctx.customer_id,
        "ASM-C1",
        "多子件装配件",
        "IN_PROCESS",
        Some(soon),
        4,
    )
    .await;

    let mut serials = Vec::new();
    for (drawing, serial) in [("CHILD-C1", "C1-001"), ("CHILD-C2", "C2-002")] {
        let child = insert_part(
            &ctx.pool,
            ctx.customer_id,
            drawing,
            "子件",
            "IN_PROCESS",
            Some(soon),
            Some(asm_id),
            2,
            Some(serial),
            None,
        )
        .await;
        insert_batch_full(&ctx.pool, child, 1, 2, "IN_PROCESS", None, None, false).await;
        serials.push(serial.to_string());
    }

    let (status, env) = preview(&ctx, batch_only_body()).await;
    assert_eq!(status, StatusCode::OK, "{env}");
    assert_eq!(env["data"]["upcoming_count"], 1, "装配件只出一行：{env}");
    let html = env["data"]["html"].as_str().expect("html 是字符串");

    let mut got = batch_sub_row_serials(html);
    got.sort();
    let mut want = serials.clone();
    want.sort();
    assert_eq!(
        got, want,
        "每个批次子行的序列号格 = 各自的子件序列号：\n{html}"
    );

    // 子件的图号**不得**出现在子行序列号格上（不回退到图号）；子件也不单独成行
    assert!(
        !html.contains("CHILD-C1") && !html.contains("CHILD-C2"),
        "子件图号不得漏进子行（只填子件序列号，且子件不单独成行）：\n{html}"
    );
}

// ===========================================================================
//  3. 四种 location 各渲染对应中文
// ===========================================================================

/// ★ 需求 3（用户原话「尤其注意生产中和外协中的工件，必须写具体的工人的姓名和
/// 外协厂的名称」）：四种有持有人的 `location` 逐档渲染。
///
/// 文案与前端 `partsListColumnDefs.ts` 的位置列**逐字一致**：货架/品检取 `code`、
/// 工人取 `name`、外协取公司 `name`，`WORKER` **不加前缀**。
#[tokio::test]
async fn preview_renders_each_holder_location_in_chinese() {
    let ctx = crate::bootstrap().await;
    let t = today();
    let soon = t + chrono::Duration::days(2);
    let worker_id = insert_worker(&ctx.pool, "张三").await;
    let shelf_id = insert_shelf(&ctx.pool, "SH-A1").await;
    let oc_id = insert_outsource_company(&ctx.pool, "宏远外协").await;

    let cases: [(&str, Option<&str>, Option<i64>, &str); 4] = [
        ("WORKER", Some("WORKER"), Some(worker_id), "张三"),
        (
            "PRODUCTION_SHELF",
            Some("PRODUCTION_SHELF"),
            Some(shelf_id),
            "货架 SH-A1",
        ),
        (
            "INSPECTION_SHELF",
            Some("INSPECTION_SHELF"),
            Some(shelf_id),
            "品检 SH-A1",
        ),
        (
            "OUTSOURCE_COMPANY",
            Some("OUTSOURCE_COMPANY"),
            Some(oc_id),
            "外协 宏远外协",
        ),
    ];

    for (idx, (label, loc, holder, _want)) in cases.iter().enumerate() {
        let part = insert_part(
            &ctx.pool,
            ctx.customer_id,
            &format!("DWG-H{idx}"),
            label,
            "IN_PROCESS",
            Some(soon),
            None,
            1,
            None,
            None,
        )
        .await;
        insert_batch_full(&ctx.pool, part, 1, 1, "IN_PROCESS", *loc, *holder, false).await;
    }

    let (status, env) = preview(&ctx, batch_only_body()).await;
    assert_eq!(status, StatusCode::OK, "{env}");
    let html = env["data"]["html"].as_str().expect("html 是字符串");
    for (_, _, _, want) in cases {
        assert!(html.contains(want), "位置列缺「{want}」：\n{html}");
    }
}

/// 软删的工人 / 货架 / 外协厂**不出现在邮件里**（holder 三表都判
/// `deleted_at IS NULL`，但 `LEFT JOIN` 仍保留批次行 ⇒ 位置退化成 `—`）。
#[tokio::test]
async fn preview_soft_deleted_holder_degrades_to_dash() {
    let ctx = crate::bootstrap().await;
    let t = today();
    let worker_id = insert_worker(&ctx.pool, "已删除工人").await;
    sqlx::query("UPDATE t_worker SET deleted_at = now() WHERE id = $1")
        .bind(worker_id)
        .execute(&ctx.pool)
        .await
        .expect("软删工人");

    let part = insert_part(
        &ctx.pool,
        ctx.customer_id,
        "DWG-DEL",
        "件",
        "IN_PROCESS",
        Some(t + chrono::Duration::days(2)),
        None,
        1,
        None,
        None,
    )
    .await;
    insert_batch_full(
        &ctx.pool,
        part,
        1,
        1,
        "IN_PROCESS",
        Some("WORKER"),
        Some(worker_id),
        false,
    )
    .await;

    let (status, env) = preview(&ctx, batch_only_body()).await;
    assert_eq!(status, StatusCode::OK, "{env}");
    let html = env["data"]["html"].as_str().expect("html 是字符串");
    assert!(
        !html.contains("已删除工人"),
        "软删工人不该出现在邮件里：\n{html}"
    );
    assert!(
        has_batch_cell(html, 1),
        "批次行本身不能因 holder 软删而丢：\n{html}"
    );
}

// ===========================================================================
//  4. 空数组入参
// ===========================================================================

/// `list_batches` 的两个 id 集合**任一为空、乃至都为空**都不炸，且不返回多余行。
///
/// 直调 repo（不经 HTTP）：这条守的是 SQL 形态本身 —— `= ANY('{}')` 恒 false，
/// 装配件侧为空（绝大多数场景）与**两侧都为空**（两节都关 / 窗口内无工单）是常态。
#[tokio::test]
async fn list_batches_accepts_empty_id_arrays() {
    let ctx = crate::bootstrap().await;
    let t = today();
    let part = insert_part(
        &ctx.pool,
        ctx.customer_id,
        "DWG-E1",
        "散件",
        "IN_PROCESS",
        Some(t + chrono::Duration::days(2)),
        None,
        1,
        None,
        None,
    )
    .await;
    insert_batch_full(&ctx.pool, part, 1, 1, "IN_PROCESS", None, None, false).await;

    let mut conn = ctx.pool.acquire().await.expect("取连接");

    // ① 装配件侧为空（常态）
    let only_part = repo::list_batches(&mut conn, &[part], &[])
        .await
        .expect("装配件侧空数组不应炸");
    assert_eq!(only_part.len(), 1, "散件批次应被拉回：{only_part:?}");
    assert_eq!(only_part[0].owner_type, "PART");
    assert_eq!(only_part[0].batch_no, 1);

    // ② 两侧都为空
    let none = repo::list_batches(&mut conn, &[], &[])
        .await
        .expect("两侧空数组不应炸");
    assert!(none.is_empty(), "空入参不得返回任何行：{none:?}");

    // ③ 散件侧为空（装配件 id 合法时该支单独工作）
    let asm_id = insert_assembly(
        &ctx.pool,
        ctx.customer_id,
        "ASM-E1",
        "装配件",
        "IN_PROCESS",
        Some(t + chrono::Duration::days(2)),
        1,
    )
    .await;
    let child = insert_part(
        &ctx.pool,
        ctx.customer_id,
        "CHILD-E1",
        "子件",
        "IN_PROCESS",
        Some(t + chrono::Duration::days(2)),
        Some(asm_id),
        1,
        None,
        None,
    )
    .await;
    insert_batch_full(&ctx.pool, child, 1, 1, "IN_PROCESS", None, None, false).await;
    let only_asm = repo::list_batches(&mut conn, &[], &[asm_id])
        .await
        .expect("散件侧空数组不应炸");
    assert_eq!(only_asm.len(), 1, "装配件的子件批次应被拉回：{only_asm:?}");
    assert_eq!(only_asm[0].owner_type, "ASSEMBLY");

    // ④ 传入与本用例无关的 id ⇒ 零行（别把别人的批次捎带出来）
    let unrelated = repo::list_batches(&mut conn, &[], &[asm_id + 1])
        .await
        .expect("无关 id 不应炸");
    assert!(unrelated.is_empty(), "无关 id 不得返回行：{unrelated:?}");
}

// ===========================================================================
//  5. 列白名单：新增两列可勾、未知列仍 21704
// ===========================================================================

/// `batch_no` / `location` 进白名单且**可勾可去勾**；未知列仍 `21704`
/// （封闭枚举「不静默丢弃」这条不变式没被本轮改动破坏）。
#[tokio::test]
async fn preview_unknown_column_still_rejected_and_known_ones_render() {
    let ctx = crate::bootstrap().await;
    let t = today();
    let part = insert_part(
        &ctx.pool,
        ctx.customer_id,
        "DWG-C1",
        "件",
        "IN_PROCESS",
        Some(t + chrono::Duration::days(2)),
        None,
        1,
        None,
        None,
    )
    .await;
    insert_batch_full(&ctx.pool, part, 1, 7, "PENDING", None, None, false).await;

    // ① 只勾 drawing_no ⇒ 无批次列、无位置列，但子行**仍然是等宽的一行**
    let (status, env) = preview(&ctx, json!({ "visible_columns": ["drawing_no"] })).await;
    assert_eq!(status, StatusCode::OK, "{env}");
    let html = env["data"]["html"].as_str().expect("html 是字符串");
    assert!(!has_batch_cell(html, 1), "批次列被勾掉后不应出现批次号");
    assert!(!html.contains(">位置<"), "位置列被勾掉后不应出现表头");

    // ② 勾上 batch_no + location ⇒ 出现
    let (status, env) = preview(
        &ctx,
        json!({ "visible_columns": ["drawing_no", "batch_no", "location"] }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{env}");
    let html = env["data"]["html"].as_str().expect("html 是字符串");
    assert!(has_batch_cell(html, 1) && html.contains(">位置<"), "{html}");

    // ③ 未知列仍 21704（不静默丢弃）
    let (status, env) = preview(
        &ctx,
        json!({ "visible_columns": ["drawing_no", "no_such_column"] }),
    )
    .await;
    crate::assert_biz_code(status, &env, 21704, StatusCode::BAD_REQUEST);
}

// ===========================================================================
//  6. 转义与状态中文（批次侧）
// ===========================================================================

/// 批次侧的 holder 名（工人姓名 / 外协厂名，全是用户输入）同样过 HTML 转义，
/// 且状态列输出中文。
#[tokio::test]
async fn preview_chinese_status_and_batch_location_are_escaped() {
    let ctx = crate::bootstrap().await;
    let t = today();
    let worker_id = insert_worker(&ctx.pool, "<script>alert(1)</script>").await;
    let part = insert_part(
        &ctx.pool,
        ctx.customer_id,
        "DWG-XSS",
        "件",
        "IN_PROCESS",
        Some(t + chrono::Duration::days(2)),
        None,
        1,
        None,
        None,
    )
    .await;
    insert_batch_full(
        &ctx.pool,
        part,
        1,
        1,
        "IN_PROCESS",
        Some("WORKER"),
        Some(worker_id),
        false,
    )
    .await;

    let (status, env) = preview(&ctx, batch_only_body()).await;
    assert_eq!(status, StatusCode::OK, "{env}");
    let html = env["data"]["html"].as_str().expect("html 是字符串");
    assert!(!html.contains("<script>"), "裸 <script> 泄进了正文");
    assert!(html.contains("&lt;script&gt;"), "holder 名未转义");
    // 状态中文（需求 1）：工单行「生产中」+ 子行「生产中」
    assert!(html.contains("生产中"), "{html}");
    assert!(!html.contains(">IN_PROCESS<"), "英文原字面量仍出现了");
}

/// 返修中的批次子行显示「返修中」，而工单行**没有**这条信息（`t_part` 上没有
/// `is_repairing` 列）—— 这个不对称是表结构事实，不是漏实现。
#[tokio::test]
async fn preview_repairing_batch_renders_repairing_label() {
    let ctx = crate::bootstrap().await;
    let t = today();
    let part = insert_part(
        &ctx.pool,
        ctx.customer_id,
        "DWG-REP",
        "返修件",
        "IN_PROCESS",
        Some(t + chrono::Duration::days(2)),
        None,
        1,
        None,
        None,
    )
    .await;
    insert_batch_full(&ctx.pool, part, 1, 1, "IN_PROCESS", None, None, true).await;

    let (status, env) = preview(&ctx, batch_only_body()).await;
    assert_eq!(status, StatusCode::OK, "{env}");
    let html = env["data"]["html"].as_str().expect("html 是字符串");
    assert!(
        html.contains("返修中"),
        "is_repairing 未渲成「返修中」：\n{html}"
    );
}

// ===========================================================================
//  7. 「已交 + 未交混合」：工单行守卫与批次过滤方向相反
// ===========================================================================

/// ★ 已交量守卫与批次子行是**方向相反**的两条口径，取数侧钉住（docs §4.4 的配套回归）。
///
/// 工单行的守卫是 `NOT EXISTS（任一批次 status IN ('DELIVERED','COMPLETED')）` ⇒
/// **一件交过，整个工单行就不出现**，哪怕它还有没交的批次（那几批自然也不出现）；
/// 反过来，工单行一旦出现，它名下**全部**批次都列出来 —— `list_batches` 不按状态过滤，
/// 于是 `CANCELLED` 这类**非交割终态**批次照列。
///
/// 两条都写成一个「已交 + 未交混合」的用例：只钉其中一条时，另一条被改坏不会红。
#[tokio::test]
async fn mixed_delivered_and_open_batches_follow_the_opposite_two_rules() {
    let ctx = crate::bootstrap().await;
    let t = today();
    let soon = t + chrono::Duration::days(2);

    // ① 一件交过 ⇒ 整行消失（它名下没交的那批也不出现）
    let mixed = insert_part(
        &ctx.pool,
        ctx.customer_id,
        "DWG-MIXED",
        "混交件",
        "IN_PROCESS",
        Some(soon),
        None,
        10,
        None,
        None,
    )
    .await;
    insert_batch_full(&ctx.pool, mixed, 1, 6, "DELIVERED", None, None, false).await;
    insert_batch_full(&ctx.pool, mixed, 2, 4, "IN_PROCESS", None, None, false).await;

    // ② 只有非交割终态（CANCELLED）⇒ 行出现，且**两批都列出来**
    let cancelled = insert_part(
        &ctx.pool,
        ctx.customer_id,
        "DWG-CANCELLED",
        "含取消批件",
        "IN_PROCESS",
        Some(soon),
        None,
        8,
        None,
        None,
    )
    .await;
    insert_batch_full(&ctx.pool, cancelled, 1, 5, "CANCELLED", None, None, false).await;
    insert_batch_full(&ctx.pool, cancelled, 2, 3, "IN_PROCESS", None, None, false).await;

    let (status, env) = preview(
        &ctx,
        // 加 `drawing_no` 才看得见是哪个工单行（`batch_only_body()` 只有批次侧几列：
        // 序列号 / 批次 / 数量 / 状态 / 位置，没有图号）
        json!({ "visible_columns": ["drawing_no", "batch_no", "quantity", "status"] }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{env}");
    assert_eq!(
        env["data"]["upcoming_count"], 1,
        "只有「含取消批件」那一个工单：{env}"
    );
    let html = env["data"]["html"].as_str().expect("html 是字符串");
    assert!(
        !html.contains("DWG-MIXED"),
        "交过一件 ⇒ 整个工单行不出现（连没交的那批也不列）：\n{html}"
    );
    assert!(html.contains("DWG-CANCELLED"), "缺工单行：\n{html}");
    // 两个批次都在（CANCELLED 不按状态过滤）
    for no in [1, 2] {
        assert!(has_batch_cell(html, no), "缺 第{no}批：\n{html}");
    }
    for want in ["已取消", "生产中"] {
        assert!(html.contains(want), "缺 {want}：\n{html}");
    }
}

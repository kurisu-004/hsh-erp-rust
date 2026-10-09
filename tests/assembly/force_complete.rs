//! 装配件级强制完成端点 `POST /api/v2/prod/assemblies/{id}/force-complete` 集成测试
//! （2026-10-11 新增）
//!
//! 端点用途：把「实际早已送货、但没在系统录入」的装配件整体判为已交。验收核心是
//! **`t_assembly.status` 真被写成 `COMPLETED`** —— dashboard 大屏的三个交期桶
//! （`upcoming` / `overdue` / `partial`，判据 `status = ANY(DELIVERY_STATUSES)`）与柱状图
//! `by_status`（判据 `status NOT IN ('COMPLETED','CANCELLED')`）都以该列为准，写对了这行
//! 就必然从所有切片消失。
//!
//! ## 用例清单
//! 1. `force_complete_pushes_assembly_children_and_batches_to_completed`
//!    —— 主路径：2 子件 + 每件 1 条初始批次 → 装配件 / 子件 / 批次三处 status 全
//!    `COMPLETED`，装配件与子件的 `serial_no` 全清空，`t_part_event` 有
//!    `FORCE_COMPLETED` 归档行。
//! 2. `force_complete_is_idempotent_reject_on_completed_assembly`
//!    —— 第一次 200、第二次 409 `BIZ_ASSEMBLY_ALREADY_COMPLETED`。
//! 3. `force_complete_rejects_cancelled_assembly`
//!    —— 先走既有 cancel 端点，再调本端点 → 409 `BIZ_ASSEMBLY_ALREADY_CANCELLED`。
//! 4. `force_complete_returns_not_found_for_unknown_assembly`
//!    —— 不存在的 id → 404 `BIZ_ASSEMBLY_NOT_FOUND`。
//! 5. `force_complete_rejects_clerk_role`
//!    —— CLERK → 403（MANAGER **单角色**，不下放 Clerk）。
//! 6. `force_complete_completes_children_with_zero_non_cancelled_batches`
//!    —— **本题最关键的边角回归**：把两个子件的初始批次直接置 `CANCELLED`（该子件
//!    于是「非取消批次为 0 条」），断言端点仍返回 200 且**子件与装配件都被写成
//!    `COMPLETED`**、被取消的批次不被拉回。
//!
//! 用例 6 锁的是「批量 UPDATE 命中 0 行时派生循环一次都不跑」那个缺口
//! （`src/shared/batch/status.rs` 里的 TODO）：若装配件级照抄「推批次 → 靠派生追平
//! 子件」的形态，子件在批次为 0 条时永远追不平，父装配件也永远留在大屏列表里。
//!
//! ## fixture 口径
//! - 全程走 HTTP：`test_pool` / `load_part_fixture` / `test_app` / `test_state` /
//!   `login_token` / `send` / `json_request` 全部取自 `hsh_erp_test_support::*`，
//!   本文件不重复声明任何一条（也不就地 `new` 雪花 generator，取号走仓库登记的唯一源
//!   `shared_test_snowflake()` —— 本文件实际上一个号都不用取）。
//! - 建单走既有 `POST /api/v2/assemblies`（multipart，只发 `data` 不发 PDF）：该端点
//!   建子件的同时会给每个子件建 1 条 `PENDING` 初始批次，正是本端点步骤①要覆盖的对象。
//!   用它而不是直插 SQL，是为了不给测试引入「只有测试才有的数据形态」。
//! - 唯一的直写 SQL 是用例 6 把批次置 `CANCELLED`：走 `POST /prod/batches/{id}/cancel`
//!   会连带把子件经派生翻成 `CANCELLED`（那是 rollup 的正确行为，但会让本用例想验的
//!   「子件仍是 PENDING、只是没有可强推的批次」这一形态无法构造）。置终态要的是
//!   「批次侧无活对象」这一形状，直接改列最直接。

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use serde_json::{Value, json};
use sqlx::PgPool;

use hsh_erp_rust::shared::error::code;
use hsh_erp_test_support::*;

// ===========================================================================
//  本文件私有 helper（请求拼装 / 只读断言 / 用例 6 的直写）
// ===========================================================================

fn today() -> chrono::NaiveDate {
    chrono::Utc::now()
        .with_timezone(&chrono::FixedOffset::east_opt(8 * 3600).unwrap())
        .date_naive()
}

/// 建单入参：固定 1 套装配件 + N 个子件（各 1 件）。
///
/// 不传 PDF：`POST /api/v2/assemblies` 的「无 PDF 仍建全部子件 + 每子件 1 条初始批次」
/// 行为已有 `create_serial_price.rs` 覆盖，本文件复用同一形态。
fn assembly_create_body(customer_id: i64, children: &[&str]) -> Value {
    let t = today().to_string();
    json!({
        "drawing_no": "D-ASM-FORCE",
        "name": "装配件-强制完成",
        "applicant_name": "甲",
        "customer_id": customer_id.to_string(),
        "request_date": t,
        "planned_delivery_date": t,
        "is_urgent": false,
        "quantity": 1,
        "children": children.iter().enumerate().map(|(i, name)| json!({
            "name": name,
            "drawing_no": format!("D-FORCE-{i:02}"),
            "quantity": 1,
        })).collect::<Vec<_>>(),
    })
}

/// 拼 `POST /assemblies` 的 multipart 请求（只发 `data` 字段，不发 PDF）。
fn assembly_create_request(data_json: &Value, token: &str) -> Request<Body> {
    const BOUNDARY: &str = "----hshassemblyforce7d3b";
    let body = format!(
        "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"data\"\r\n\
         Content-Type: application/json\r\n\r\n{data_json}\r\n--{BOUNDARY}--\r\n"
    );
    Request::builder()
        .method("POST")
        .uri("/assemblies")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(
            header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .body(Body::from(body))
        .expect("build multipart request")
}

/// 打本端点（body 只有 `note` 可选）。
fn force_complete_request(assembly_id: i64, token: &str, note: Option<&str>) -> Request<Body> {
    json_request(
        "POST",
        &format!("/prod/assemblies/{assembly_id}/force-complete"),
        Some(json!({ "note": note })),
        Some(token),
    )
}

/// 从建单响应里取装配体 id（雪花在响应里是字符串）。
fn assembly_id_of(env: &Value) -> i64 {
    env["data"]["assembly"]["id"]
        .as_str()
        .expect("assembly.id 是字符串（雪花）")
        .parse()
        .expect("assembly.id 可解析为 i64")
}

/// 该装配件下全部未软删子件的 `(id, serial_no, status)`。
async fn child_rows(pool: &PgPool, assembly_id: i64) -> Vec<(i64, Option<String>, String)> {
    sqlx::query_as(
        "SELECT id, serial_no, status FROM t_part \
                    WHERE assembly_id = $1 AND deleted_at IS NULL ORDER BY serial_no ASC",
    )
    .bind(assembly_id)
    .fetch_all(pool)
    .await
    .expect("查子件行")
}

/// 该装配件下全部未软删批次的 `(id, status)`。
async fn child_batch_rows(pool: &PgPool, assembly_id: i64) -> Vec<(i64, String)> {
    sqlx::query_as(
        "SELECT pb.id, pb.status FROM t_part_batch pb \
         JOIN t_part p ON p.id = pb.part_id \
         WHERE p.assembly_id = $1 AND pb.deleted_at IS NULL ORDER BY pb.id ASC",
    )
    .bind(assembly_id)
    .fetch_all(pool)
    .await
    .expect("查子件批次行")
}

/// `t_assembly` 的 `(status, serial_no)`。
async fn assembly_row(pool: &PgPool, assembly_id: i64) -> (String, Option<String>) {
    sqlx::query_as("SELECT status, serial_no FROM t_assembly WHERE id = $1")
        .bind(assembly_id)
        .fetch_one(pool)
        .await
        .expect("查装配件行")
}

// ===========================================================================
//  1. 主路径
// ===========================================================================

#[tokio::test]
async fn force_complete_pushes_assembly_children_and_batches_to_completed() {
    let pool = test_pool().await;
    let fx = load_part_fixture(&pool).await;
    let app = test_app(test_state(pool.clone()).await);
    let token = login_token(&app, &fx.manager_username, PartFixture::PASSWORD).await;

    // 建单：2 子件 → 每件 1 条 PENDING 初始批次
    let body = assembly_create_body(fx.customer_l2_id, &["子件-1", "子件-2"]);
    let (s, created) = send(app.clone(), assembly_create_request(&body, &token)).await;
    assert_eq!(s, StatusCode::CREATED, "建单应成功: {created}");
    let assembly_id = assembly_id_of(&created);

    let before_batches = child_batch_rows(&pool, assembly_id).await;
    assert_eq!(
        before_batches.len(),
        2,
        "建单后每个子件应有 1 条初始批次: {before_batches:?}"
    );
    let before_children = child_rows(&pool, assembly_id).await;
    assert_eq!(before_children.len(), 2, "应有 2 个子件");
    for (pid, serial, status) in &before_children {
        assert!(
            serial.is_some(),
            "子件 {pid} 建单时已派序列号（后续要断言它被清空）"
        );
        assert_eq!(status, "PENDING", "子件 {pid} 建单时应为 PENDING");
    }
    let (asm_status_before, asm_serial_before) = assembly_row(&pool, assembly_id).await;
    assert_eq!(asm_status_before, "PENDING");
    assert!(asm_serial_before.is_some(), "父件建单时已派序列号");

    // 打本端点
    let (s, env) = send(
        app.clone(),
        force_complete_request(assembly_id, &token, Some("早已送货，补录")),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "force-complete 应成功: {env}");
    assert_eq!(env["code"], 0, "force-complete 应成功: {env}");
    assert_eq!(
        env["data"]["status"], "COMPLETED",
        "响应体应回读为已 COMPLETED: {env}"
    );
    assert_eq!(
        env["data"]["serial_no"],
        Value::Null,
        "响应体里的序列号应已清空: {env}"
    );

    // 装配件侧
    let (asm_status, asm_serial) = assembly_row(&pool, assembly_id).await;
    assert_eq!(
        asm_status, "COMPLETED",
        "验收核心：t_assembly.status 必须真被写成 COMPLETED（否则该行仍留在大屏切片）"
    );
    assert_eq!(asm_serial, None, "装配件终态序列号应已清空");

    // 子件侧
    for (pid, serial, status) in child_rows(&pool, assembly_id).await {
        assert_eq!(status, "COMPLETED", "子件 {pid} 应被写成 COMPLETED");
        assert_eq!(serial, None, "子件 {pid} 的序列号应已清空");
    }

    // 批次侧
    for (bid, status) in child_batch_rows(&pool, assembly_id).await {
        assert_eq!(status, "COMPLETED", "批次 {bid} 应被强推为 COMPLETED");
    }

    // 事件日志：`FORCE_COMPLETED` 逐子件一行 + `SERIAL_RELEASED` 归档一行
    let forced_events: Vec<(i64, String)> = sqlx::query_as(
        "SELECT part_id, note FROM t_part_event e \
         WHERE e.event_type = 'FORCE_COMPLETED' \
           AND e.part_id IN (SELECT id FROM t_part WHERE assembly_id = $1)",
    )
    .bind(assembly_id)
    .fetch_all(&pool)
    .await
    .expect("查 FORCE_COMPLETED 事件");
    assert_eq!(
        forced_events.len(),
        2,
        "每个被改动的子件应有 1 条 FORCE_COMPLETED 归档行: {forced_events:?}"
    );
    for (pid, note) in &forced_events {
        assert!(
            note.starts_with("[FORCE_ASSEMBLY] "),
            "子件 {pid} 的 FORCE_COMPLETED note 应带 [FORCE_ASSEMBLY] 前缀便于审计区分，实际 {note:?}"
        );
    }
    let released_events: Vec<i64> = sqlx::query_scalar(
        "SELECT part_id FROM t_part_event e \
         WHERE e.event_type = 'SERIAL_RELEASED' \
           AND e.part_id IN (SELECT id FROM t_part WHERE assembly_id = $1)",
    )
    .bind(assembly_id)
    .fetch_all(&pool)
    .await
    .expect("查 SERIAL_RELEASED 事件");
    assert_eq!(
        released_events.len(),
        2,
        "清序列号前应各写 1 条归档事件（先归档后清）: {released_events:?}"
    );
}

// ===========================================================================
//  2. 幂等拒绝：已 COMPLETED
// ===========================================================================

#[tokio::test]
async fn force_complete_is_idempotent_reject_on_completed_assembly() {
    let pool = test_pool().await;
    let fx = load_part_fixture(&pool).await;
    let app = test_app(test_state(pool.clone()).await);
    let token = login_token(&app, &fx.manager_username, PartFixture::PASSWORD).await;

    let body = assembly_create_body(fx.customer_l2_id, &["子件-1"]);
    let (s, created) = send(app.clone(), assembly_create_request(&body, &token)).await;
    assert_eq!(s, StatusCode::CREATED, "建单应成功: {created}");
    let assembly_id = assembly_id_of(&created);

    let (s1, env1) = send(
        app.clone(),
        force_complete_request(assembly_id, &token, None),
    )
    .await;
    assert_eq!(s1, StatusCode::OK, "首次 force-complete 应成功: {env1}");

    let (s2, env2) = send(
        app.clone(),
        force_complete_request(assembly_id, &token, None),
    )
    .await;
    assert_eq!(s2, StatusCode::CONFLICT, "重复强推应 409: {env2}");
    assert_eq!(
        env2["code"],
        code::BIZ_ASSEMBLY_ALREADY_COMPLETED,
        "重复强推应报 BIZ_ASSEMBLY_ALREADY_COMPLETED: {env2}"
    );
}

// ===========================================================================
//  3. 终态守护：已 CANCELLED
// ===========================================================================

#[tokio::test]
async fn force_complete_rejects_cancelled_assembly() {
    let pool = test_pool().await;
    let fx = load_part_fixture(&pool).await;
    let app = test_app(test_state(pool.clone()).await);
    let token = login_token(&app, &fx.manager_username, PartFixture::PASSWORD).await;

    let body = assembly_create_body(fx.customer_l2_id, &["子件-1"]);
    let (s, created) = send(app.clone(), assembly_create_request(&body, &token)).await;
    assert_eq!(s, StatusCode::CREATED, "建单应成功: {created}");
    let assembly_id = assembly_id_of(&created);

    // 先走既有 cancel 端点把装配件打成 CANCELLED
    let (s, cancelled) = send(
        app.clone(),
        json_request(
            "POST",
            &format!("/assemblies/{assembly_id}/cancel"),
            None,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "cancel 应成功: {cancelled}");
    assert_eq!(
        cancelled["data"]["status"], "CANCELLED",
        "cancel 后应为 CANCELLED"
    );

    let (s2, env2) = send(
        app.clone(),
        force_complete_request(assembly_id, &token, None),
    )
    .await;
    assert_eq!(s2, StatusCode::CONFLICT, "已 CANCELLED 应 409: {env2}");
    assert_eq!(
        env2["code"],
        code::BIZ_ASSEMBLY_ALREADY_CANCELLED,
        "已 CANCELLED 应报 BIZ_ASSEMBLY_ALREADY_CANCELLED: {env2}"
    );
    // 拒绝路径不应改任何状态
    let (status, _) = assembly_row(&pool, assembly_id).await;
    assert_eq!(status, "CANCELLED", "守卫拒绝后装配件状态不应被改动");
}

// ===========================================================================
//  4. 不存在的 id
// ===========================================================================

#[tokio::test]
async fn force_complete_returns_not_found_for_unknown_assembly() {
    let pool = test_pool().await;
    let fx = load_part_fixture(&pool).await;
    let app = test_app(test_state(pool.clone()).await);
    let token = login_token(&app, &fx.manager_username, PartFixture::PASSWORD).await;

    // fixture 的 L1 客户 id 落在 fixture 段位，必然不存在于 t_assembly
    let (s, env) = send(
        app,
        force_complete_request(PartFixture::CUSTOMER_L1_ID, &token, None),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "不存在的 id 应 404: {env}");
    assert_eq!(
        env["code"],
        code::BIZ_ASSEMBLY_NOT_FOUND,
        "不存在的 id 应报 BIZ_ASSEMBLY_NOT_FOUND: {env}"
    );
}

// ===========================================================================
//  5. MANAGER 单角色守卫
// ===========================================================================

#[tokio::test]
async fn force_complete_rejects_clerk_role() {
    let pool = test_pool().await;
    let fx = load_part_fixture(&pool).await;
    let app = test_app(test_state(pool.clone()).await);
    let token = login_token(&app, &fx.clerk_username, PartFixture::PASSWORD).await;

    let body = assembly_create_body(fx.customer_l2_id, &["子件-1"]);
    let (s, created) = send(app.clone(), assembly_create_request(&body, &token)).await;
    assert_eq!(
        s,
        StatusCode::CREATED,
        "建单应成功（Clerk 有建单权限）: {created}"
    );
    let assembly_id = assembly_id_of(&created);

    let (s2, env2) = send(app, force_complete_request(assembly_id, &token, None)).await;
    assert_eq!(s2, StatusCode::FORBIDDEN, "CLERK 应 403: {env2}");
    assert_eq!(
        env2["code"],
        code::FORBIDDEN,
        "CLERK 应报 FORBIDDEN（MANAGER 单角色，不委派 Clerk）: {env2}"
    );
    let (status, _) = assembly_row(&pool, assembly_id).await;
    assert_eq!(status, "PENDING", "守卫拒绝后不应改动任何状态");
}

// ===========================================================================
//  6. 边角回归：子件的非取消批次为 0 条
// ===========================================================================

#[tokio::test]
async fn force_complete_completes_children_with_zero_non_cancelled_batches() {
    let pool = test_pool().await;
    let fx = load_part_fixture(&pool).await;
    let app = test_app(test_state(pool.clone()).await);
    let token = login_token(&app, &fx.manager_username, PartFixture::PASSWORD).await;

    let body = assembly_create_body(fx.customer_l2_id, &["子件-1", "子件-2"]);
    let (s, created) = send(app.clone(), assembly_create_request(&body, &token)).await;
    assert_eq!(s, StatusCode::CREATED, "建单应成功: {created}");
    let assembly_id = assembly_id_of(&created);

    // 把两个子件的初始批次直接置 CANCELLED，但**不动子件本身的状态**（仍是
    // PENDING）—— 于是步骤①那条批量 UPDATE 命中 0 行，正是派生链追不平父件的
    // 那个缺口。
    let flipped = sqlx::query(
        "UPDATE t_part_batch SET status = 'CANCELLED' WHERE part_id IN \
         (SELECT id FROM t_part WHERE assembly_id = $1) AND deleted_at IS NULL",
    )
    .bind(assembly_id)
    .execute(&pool)
    .await
    .expect("把子件批次置 CANCELLED")
    .rows_affected();
    assert_eq!(flipped, 2, "应有 2 条初始批次被置 CANCELLED");

    let (s2, env2) = send(app, force_complete_request(assembly_id, &token, None)).await;
    assert_eq!(s2, StatusCode::OK, "批次为 0 条也应 200: {env2}");
    assert_eq!(
        env2["data"]["status"], "COMPLETED",
        "响应体应回读为 COMPLETED: {env2}"
    );

    let (asm_status, asm_serial) = assembly_row(&pool, assembly_id).await;
    assert_eq!(
        asm_status, "COMPLETED",
        "批次 0 条时装配件也必须被写成 COMPLETED（这正是派生路径追不平的那一行）"
    );
    assert_eq!(asm_serial, None, "装配件终态序列号应已清空");

    for (pid, serial, status) in child_rows(&pool, assembly_id).await {
        assert_eq!(
            status, "COMPLETED",
            "批次为 0 条的子件 {pid} 也必须被写成 COMPLETED"
        );
        assert_eq!(serial, None, "子件 {pid} 的序列号应已清空");
    }

    // 终态守卫：已取消的批次不被拉回已完成（与零件级口径一致）
    for (bid, status) in child_batch_rows(&pool, assembly_id).await {
        assert_eq!(
            status, "CANCELLED",
            "批次 {bid} 已是 CANCELLED，不应被本端点拉回 COMPLETED"
        );
    }
}

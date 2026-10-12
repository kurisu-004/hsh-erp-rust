//! `prod::scan` 扫码找件 · 诊断端点（`GET /prod/scan/locate`）集成测试
//!
//! 2026-10-15 新增（第 6 条端点，S1 只读诊断）。覆盖：
//! - 命中 / 错误路径：未知序列号、空白序列号、**装配件序列号不回退**、未知工人、
//!   停用工人；
//! - 「无 `worker_id` ⇒ action / disabled_reason 均 null」的纯位置事实形态
//!   （含终态批次）；
//! - action 决策表（与写侧守卫同源的 16 个形态，逐条见各用例 doc）；
//! - `worker_processes`（2026-10-16 新增出参）：工种有映射时按 `sort_order` 返回、
//!   无映射 / 未传 `worker_id` 时是空数组；
//! - 2026-10-16 review 第 1 轮：闸门候选集用**宽**集合（与写侧 `list_process_ids`
//!   逐字同源、含已软删工序）、出参用窄集合（`locate_take_over_with_only_soft_deleted_process_mapping_still_returns_action`）；
//!   `outsource_gate` 的零候选闸门单独覆盖（`locate_outsource_empty_work_type_candidate_returns_none_with_reason`，
//!   两个 gate 是两个独立函数）；
//! - 字段契约（逐键 + 雪花 string + `chain_state` 三字面量）。
//!
//! ## fixture 复用
//! 全部从 `super::queue` 借（`bootstrap_as_manager` / `insert_work_type` /
//! `link_work_type_to_process` / `insert_worker` / `insert_worker_held_part` /
//! `insert_pending_part_with_chain` / `insert_pool_part` / `append_chain_step` /
//! `part_chain_id` / `insert_shelf` 等），不在本文件复制同款 fixture 避免漂移。
//! 本文件独享的 raw SQL 构造（批次任意状态直插 / 外协公司 / shipment / 装配件）
//! 按仓库惯例保留为本地 `async fn`。

use axum::http::StatusCode;
use serde_json::{Value, json};
use sqlx::PgPool;

use hsh_erp_test_support::fixture::PartFixture;
use hsh_erp_test_support::{
    ProductionFixture, json_request, load_production_fixture, login_token, pool_snowflake, send,
    send_raw, test_app, test_pool, test_state,
};

use super::queue::{
    append_chain_step, bootstrap_as_manager, insert_customer_l2, insert_pending_part_with_chain,
    insert_pool_part, insert_work_type, insert_worker, insert_worker_held_part,
    link_work_type_to_process, part_chain_id, seed_process,
};

/// locate 路径（测试 app 不带 `/api/v2`）。
const LOCATE_URI: &str = "/prod/scan/locate";

// ===========================================================================
//  本 sub-file 独享 raw SQL 构造
// ===========================================================================

fn next_id() -> i64 {
    pool_snowflake()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .next_id()
}

/// 直插一条任意状态 / 位置 / 持有人的批次（诊断端点要覆盖全状态矩阵，
/// `insert_pool_part` / `insert_worker_held_part` 只给两个固定形态）。
#[allow(clippy::too_many_arguments)]
async fn insert_batch(
    pool: &PgPool,
    part_id: i64,
    batch_no: i32,
    status: &str,
    location: Option<&str>,
    holder_id: Option<i64>,
    process_id: Option<i64>,
    step_id: Option<i64>,
    quantity: i32,
) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;
    let id = next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_part_batch (id, part_id, batch_no, quantity, status, location, \
         current_holder_id, current_process_id, current_process_step_id, version, \
         created_at, updated_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, 0, $10, $10)",
    )
    .bind(id)
    .bind(part_id)
    .bind(batch_no)
    .bind(quantity)
    .bind(status)
    .bind(location)
    .bind(holder_id)
    .bind(process_id)
    .bind(step_id)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_part_batch");
    id
}

/// 把既有批次改成任意状态 / 位置 / 持有人（外协 / 品检 / 终态场景用；
/// 只动状态列，不碰工序指针）。
async fn force_batch_state(
    pool: &PgPool,
    batch_id: i64,
    status: &str,
    location: Option<&str>,
    holder_id: Option<i64>,
) {
    sqlx::query(
        "UPDATE t_part_batch SET status = $2, location = $3, current_holder_id = $4 \
         WHERE id = $1",
    )
    .bind(batch_id)
    .bind(status)
    .bind(location)
    .bind(holder_id)
    .execute(pool)
    .await
    .expect("force batch state");
}

/// 直插一条外协公司。
async fn insert_outsource_company(pool: &PgPool, name: &str) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;
    let id = next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_outsource_company (id, name, is_active, version, created_at, updated_at) \
         VALUES ($1, $2, true, 0, $3, $3)",
    )
    .bind(id)
    .bind(name)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_outsource_company");
    id
}

/// 直插一条开口（`OUTSOURCING`）外协 shipment；`quote_id` 不与任何行关联
/// （表上无物理外键，本用例只读 `sent_at`）。
async fn insert_open_shipment(
    pool: &PgPool,
    part_id: i64,
    batch_id: i64,
    company_id: i64,
    process_id: i64,
    sent_at: chrono::NaiveDateTime,
) -> i64 {
    let id = next_id();
    sqlx::query(
        "INSERT INTO t_outsource_shipment (id, quote_id, part_id, batch_id, \
         outsource_company_id, process_id, quantity, unit_price, status, sent_at, \
         created_by, updated_by) \
         VALUES ($1, $2, $3, $4, $5, $6, 1, 1.00, 'OUTSOURCING', $7, 0, 0)",
    )
    .bind(id)
    .bind(next_id())
    .bind(part_id)
    .bind(batch_id)
    .bind(company_id)
    .bind(process_id)
    .bind(sent_at)
    .execute(pool)
    .await
    .expect("insert t_outsource_shipment");
    id
}

/// 直插一个带序列号的装配件（「不回退 t_assembly」用例要有一个真实可命中的
/// assembly 行，才能证明 locate 是**刻意**不查它）。
async fn insert_assembly(pool: &PgPool, customer_id: i64, serial_no: &str) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;
    let id = next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_assembly (id, drawing_no, name, customer_id, request_date, \
         planned_delivery_date, serial_no, version, created_at, updated_at) \
         VALUES ($1, 'D-ASM', '装配件', $2, $3, $3, $4, 0, $5, $5)",
    )
    .bind(id)
    .bind(customer_id)
    .bind(now.date())
    .bind(serial_no)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_assembly");
    id
}

/// 插一个 `t_user` + 指定角色并登录，返回 access token（与
/// `tests/production/pending_programming.rs::login_user_with_role` 同款写法）。
///
/// 403 用例需要一个**非白名单**角色：fixture 预置的 4 个账号（Manager / Clerk /
/// Inspector / ShelfAccount）全部命中 `SCAN_LIST_ROLES`，故现场造 `CNC_PROGRAMMER`。
async fn login_user_with_role(
    pool: &PgPool,
    app: &axum::Router,
    username: &str,
    role: &str,
) -> String {
    use hsh_erp_rust::auth::password;
    use hsh_erp_rust::infra::clock::now_naive;

    let hash = password::hash(ProductionFixture::PASSWORD).expect("bcrypt hash");
    let user_id = next_id();
    let role_id = next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_user (id, username, password_hash, full_name, is_active, \
         refresh_token_version, version, created_at, updated_at) \
         VALUES ($1, $2, $3, $2, true, 0, 0, $4, $4)",
    )
    .bind(user_id)
    .bind(username)
    .bind(hash)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_user");
    sqlx::query(
        "INSERT INTO t_user_role (id, user_id, role, scope_type, scope_id, version, \
         created_at, updated_at) VALUES ($1, $2, $3, NULL, NULL, 0, $4, $4)",
    )
    .bind(role_id)
    .bind(user_id)
    .bind(role)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_user_role");

    login_token(app, username, ProductionFixture::PASSWORD).await
}

/// 把既有映射的 `sort_order` 改成指定值（`link_work_type_to_process` 恒写 0，
/// 排序用例需要非零值来验证「返回序 ≠ 插入序」）。
async fn set_mapping_sort_order(pool: &PgPool, work_type_id: i64, process_id: i64, sort_order: i32) {
    sqlx::query(
        "UPDATE t_work_type_process SET sort_order = $3 \
         WHERE work_type_id = $1 AND process_id = $2",
    )
    .bind(work_type_id)
    .bind(process_id)
    .bind(sort_order)
    .execute(pool)
    .await
    .expect("update t_work_type_process.sort_order");
}

// ===========================================================================
//  HTTP helper
// ===========================================================================

/// 打一次 locate（`worker_id` 传 `Some` 时附上字符串形态）。
async fn locate(
    app: &axum::Router,
    token: &str,
    serial_no: &str,
    worker_id: Option<i64>,
) -> (StatusCode, Value) {
    let uri = match worker_id {
        Some(w) => format!("{LOCATE_URI}?serial_no={serial_no}&worker_id={w}"),
        None => format!("{LOCATE_URI}?serial_no={serial_no}"),
    };
    send(app.clone(), json_request("GET", &uri, None, Some(token))).await
}

/// 取响应里 `data.batches[0]`。
fn first_batch(env: &Value) -> &Value {
    env["data"]["batches"]
        .as_array()
        .and_then(|b| b.first())
        .unwrap_or_else(|| panic!("响应缺 data.batches[0]: {env}"))
}

// ===========================================================================
//  命中 / 错误路径
// ===========================================================================

/// 未知序列号 → `20101` / HTTP 404（不进 `t_assembly` 回退）。
#[tokio::test]
async fn locate_unknown_serial_returns_20101() {
    let (_pool, app, token, _fx) = bootstrap_as_manager().await;
    let (s, env) = locate(&app, &token, "SN-DOES-NOT-EXIST", None).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{env}");
    assert_eq!(env["code"], 20101);
}

/// `serial_no` trim 后为空 → 同样按未命中 `20101`（不是 400）。
#[tokio::test]
async fn locate_blank_serial_returns_20101() {
    let (_pool, app, token, _fx) = bootstrap_as_manager().await;
    // `%20%20` = 两个空格：走 trim 分支而不是「字段缺失」的 400
    let (s, env) = locate(&app, &token, "%20%20", None).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{env}");
    assert_eq!(env["code"], 20101);
}

/// 装配件序列号**不回退 `t_assembly`**：即使 assembly 行真实存在也返 `20101`
/// （报工台的找件对象始终是可作业的子件批次；口径差登记在 `docs/api/scan.md` §8）。
#[tokio::test]
async fn locate_assembly_serial_not_fallback_returns_20101() {
    let (pool, app, token, fx) = bootstrap_as_manager().await;
    insert_assembly(&pool, fx.part_customer_l1_id, "ASM-LOCATE-1").await;
    let (s, env) = locate(&app, &token, "ASM-LOCATE-1", None).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{env}");
    assert_eq!(env["code"], 20101, "locate 不得回退 t_assembly: {env}");
}

/// `worker_id` 查不到（不存在或已软删）→ `20201` / HTTP 404。
#[tokio::test]
async fn locate_unknown_worker_returns_20201() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let customer = insert_customer_l2(&pool, "LOCATE-UNK-W").await;
    let proc = seed_process(&pool, "LOC-UNK-W", "工序").await;
    let (_part, _batch) = insert_pool_part(
        &pool,
        customer,
        "SN-UNK-W",
        PartFixture::PRODUCTION_SHELF_ID,
        proc,
        1,
    )
    .await;
    let (s, env) = locate(&app, &token, "SN-UNK-W", Some(999_999_999)).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{env}");
    assert_eq!(env["code"], 20201);
}

/// 工人存在但已停用 → `20202` / HTTP 400（与写路径对齐：诊断不能诊断出一个
/// 「点了就会失败」的按钮）。
#[tokio::test]
async fn locate_inactive_worker_returns_20202() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let customer = insert_customer_l2(&pool, "LOCATE-INACT").await;
    let proc = seed_process(&pool, "LOC-INACT", "工序").await;
    let wt = insert_work_type(&pool, "WT-INACT", "工种", Some(5)).await;
    link_work_type_to_process(&pool, wt, proc).await;
    let worker = insert_worker(&pool, "BC-INACT", "停用工", Some(wt)).await;
    sqlx::query("UPDATE t_worker SET is_active = false WHERE id = $1")
        .bind(worker)
        .execute(&pool)
        .await
        .expect("deactivate worker");
    let (_part, _batch) = insert_pool_part(
        &pool,
        customer,
        "SN-INACT",
        PartFixture::PRODUCTION_SHELF_ID,
        proc,
        1,
    )
    .await;
    let (s, env) = locate(&app, &token, "SN-INACT", Some(worker)).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{env}");
    assert_eq!(env["code"], 20202);
}

/// 角色门禁：非白名单角色（`CNC_PROGRAMMER`）→ `40300` / HTTP 403。
///
/// `locate` 只读诊断也守白名单（Manager / Clerk / Inspector / ShelfAccount）——
/// 同批 `take-over` / `outsource-receive` 都有 403 用例，本端点此前缺位；本用例
/// 钉住「漏掉守卫」或「白名单被放宽」都会让它变红。
#[tokio::test]
async fn locate_non_whitelisted_role_returns_40300() {
    let (pool, app, _manager, _fx) = bootstrap_as_manager().await;
    // fixture 预置的 4 个账号都是白名单角色；现场造一个 CNC_PROGRAMMER 账号。
    let token = login_user_with_role(&pool, &app, "locate_cnc_programmer", "CNC_PROGRAMMER").await;

    let (s, env) = locate(&app, &token, "SN-LOCATE-403", None).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "CNC_PROGRAMMER 应 403: {env}");
    assert_eq!(env["code"], 40300, "FORBIDDEN: {env}");
}

/// 提取器层 rejection：`worker_id` 非数字（`abc`）/ 漏传 `serial_no`
/// → `QueryRejection` 400 **纯文本**（不进 `R<T>` 信封）。
///
/// ⚠️ 这里是「非数字」而不是「发 JSON number」：query string 里所有值天然是
/// 字符串，`worker_id=123` 会被 `deserialize_i64_opt` 正常解析成 `123`
/// （本文件其余用例正是这么传的）；能触达 rejection 的只有**解析失败**与
/// **必填字段缺失**两种。
#[tokio::test]
async fn locate_bad_query_returns_400_plain_text() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let customer = insert_customer_l2(&pool, "LOCATE-NUM").await;
    let proc = seed_process(&pool, "LOC-NUM", "工序").await;
    let (_part, _batch) = insert_pool_part(
        &pool,
        customer,
        "SN-NUM",
        PartFixture::PRODUCTION_SHELF_ID,
        proc,
        1,
    )
    .await;
    for uri in [
        format!("{LOCATE_URI}?serial_no=SN-NUM&worker_id=abc"),
        // 漏传必填的 serial_no
        format!("{LOCATE_URI}?worker_id=123"),
    ] {
        let (s, body) = send_raw(app.clone(), json_request("GET", &uri, None, Some(&token))).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{uri}: {body}");
        assert!(
            !body.starts_with('{'),
            "应是不进信封的纯文本 400（{uri}），实得: {body}"
        );
    }
}

// ===========================================================================
//  无 worker_id：纯位置事实
// ===========================================================================

/// 不传 `worker_id` ⇒ 全部批次的 `action` / `disabled_reason` 均为 `null`；
/// 批次含**终态**（CANCELLED），并按 `batch_no ASC` 排序。
#[tokio::test]
async fn locate_returns_all_batches_including_terminal_with_null_action_when_no_worker() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let customer = insert_customer_l2(&pool, "LOCATE-ALL").await;
    let proc = seed_process(&pool, "LOC-ALL", "工序").await;
    let worker = insert_worker(&pool, "BC-ALL", "工", None).await;
    // 1 号批：IN_PROCESS+WORKER（正常持有），2 号批：CANCELLED（终态，仍要出现在结果里）
    let (part, batch1, _step) =
        insert_worker_held_part(&pool, customer, "SN-ALL", worker, proc, 2, true).await;
    let batch2 = insert_batch(&pool, part, 2, "CANCELLED", None, None, None, None, 1).await;

    let (s, env) = locate(&app, &token, "SN-ALL", None).await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(env["code"], 0);
    assert_eq!(env["data"]["part"]["id"], part.to_string());
    assert_eq!(env["data"]["part"]["serial_no"], "SN-ALL");
    assert_eq!(env["data"]["part"]["name"], "held-item");
    assert_eq!(env["data"]["part"]["drawing_no"], "D-HELD");
    let batches = env["data"]["batches"].as_array().expect("batches");
    assert_eq!(batches.len(), 2, "含终态批次: {env}");
    assert_eq!(batches[0]["batch_id"], batch1.to_string());
    assert_eq!(batches[1]["batch_id"], batch2.to_string());
    assert_eq!(batches[1]["status"], "CANCELLED");
    for b in batches {
        assert_eq!(b["action"], Value::Null, "未传 worker_id 时 action 恒 null");
        assert_eq!(
            b["disabled_reason"],
            Value::Null,
            "未传 worker_id 时 disabled_reason 恒 null"
        );
    }
}

// ===========================================================================
//  worker_processes：目标工序 picker（2026-10-16 新增）
// ===========================================================================

/// `worker_processes` 返回该工种映射的**全部**工序，逐键 + 雪花 string + 排序。
///
/// 排序判据是 `t_work_type_process.sort_order ASC, wtp.id ASC`；本用例把第二条映射
/// 的 `sort_order` 调成 0、第一条调成 10，从而让「插入顺序」与「返回顺序」相反 ——
/// 这样断言的是排序真被应用，而不是碰巧与插入序一致。
#[tokio::test]
async fn locate_worker_processes_lists_work_type_mapping_in_sort_order() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let customer = insert_customer_l2(&pool, "LOCATE-WP").await;
    let proc_a = seed_process(&pool, "LOC-WP-A", "工序甲").await;
    let proc_b = seed_process(&pool, "LOC-WP-B", "工序乙").await;
    let wt = insert_work_type(&pool, "WT-WP", "工种", Some(5)).await;
    // 先插 A(sort=10) 再插 B(sort=0) ⇒ 返回序应是 [B, A]
    link_work_type_to_process(&pool, wt, proc_a).await;
    link_work_type_to_process(&pool, wt, proc_b).await;
    set_mapping_sort_order(&pool, wt, proc_a, 10).await;
    set_mapping_sort_order(&pool, wt, proc_b, 0).await;
    let worker = insert_worker(&pool, "BC-WP", "工", Some(wt)).await;
    let (part, batch, _step) =
        insert_worker_held_part(&pool, customer, "SN-WP", worker, proc_a, 1, true).await;

    let (s, env) = locate(&app, &token, "SN-WP", Some(worker)).await;
    assert_eq!(s, StatusCode::OK, "{env}");
    let wp = env["data"]["worker_processes"]
        .as_array()
        .expect("worker_processes 是数组");
    assert_eq!(wp.len(), 2, "两条映射都要出现: {env}");
    assert_eq!(wp[0]["process_id"], proc_b.to_string(), "sort_order=0 在前");
    assert_eq!(wp[0]["code"], "LOC-WP-B");
    assert_eq!(wp[0]["name"], "工序乙");
    assert_eq!(wp[0]["category"], "INHOUSE");
    assert_eq!(wp[1]["process_id"], proc_a.to_string());
    assert_eq!(wp[1]["code"], "LOC-WP-A");
    for row in wp {
        assert!(
            row["process_id"].is_string(),
            "process_id 必须是 JSON string（雪花精度）: {env}"
        );
        let mut keys: Vec<&str> = row
            .as_object()
            .expect("候选工序行是对象")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            ["category", "code", "name", "process_id"],
            "LocateProcessBriefOut 字段集变了（同步前端 schema）: {env}"
        );
    }
    // 该响应同时带着诊断行 —— 两块信息在同一次调用里。
    assert_eq!(
        first_batch(&env)["batch_id"],
        batch.to_string(),
        "worker_processes 与 batches 同在一次响应: {env}"
    );
    let _ = part;
}

/// 工种未映射任何工序 ⇒ `worker_processes` 是**空数组**（不是 null、不是省略键），
/// 且「非链推导形态」下的批次落 `NONE + PROCESS_NOT_IN_WORK_TYPE`（对位写侧 2026-10-16
/// 新增分支 C 的 `20104` —— 零候选时写侧必拒，读侧必须先藏掉按钮）。
#[tokio::test]
async fn locate_worker_processes_empty_when_work_type_has_no_mapping() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let customer = insert_customer_l2(&pool, "LOCATE-WP0").await;
    let proc = seed_process(&pool, "LOC-WP0", "工序").await;
    // 工种存在但**不 link** 任何工序
    let wt = insert_work_type(&pool, "WT-WP0", "空工种", Some(5)).await;
    let holder = insert_worker(&pool, "BC-WP0-H", "持有人", Some(wt)).await;
    let taker = insert_worker(&pool, "BC-WP0-T", "接管人", Some(wt)).await;
    let (_part, batch, _step) =
        insert_worker_held_part(&pool, customer, "SN-WP0", holder, proc, 1, true).await;
    // 指针漂移 ⇒ 非链推导形态（会走「工种候选」那条判据）
    sqlx::query("UPDATE t_part_batch SET current_process_step_id = NULL WHERE id = $1")
        .bind(batch)
        .execute(&pool)
        .await
        .expect("clear step pointer");

    let (s, env) = locate(&app, &token, "SN-WP0", Some(taker)).await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(
        env["data"]["worker_processes"],
        json!([]),
        "零候选必须是空数组（前端 picker 无选项）: {env}"
    );
    let b = first_batch(&env);
    assert_eq!(b["action"], Value::Null, "零候选时不得给可点按钮: {env}");
    assert_eq!(b["disabled_reason"], "PROCESS_NOT_IN_WORK_TYPE");
}

/// 未传 `worker_id` ⇒ `worker_processes` 是空数组（无工人 = 无工种 = 无候选）。
#[tokio::test]
async fn locate_worker_processes_empty_without_worker_id() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let customer = insert_customer_l2(&pool, "LOCATE-WPN").await;
    let proc = seed_process(&pool, "LOC-WPN", "工序").await;
    let wt = insert_work_type(&pool, "WT-WPN", "工种", Some(5)).await;
    link_work_type_to_process(&pool, wt, proc).await;
    let worker = insert_worker(&pool, "BC-WPN", "工", Some(wt)).await;
    insert_worker_held_part(&pool, customer, "SN-WPN", worker, proc, 1, true).await;

    let (s, env) = locate(&app, &token, "SN-WPN", None).await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(
        env["data"]["worker_processes"],
        json!([]),
        "未传 worker_id 时不查工种映射，恒空数组: {env}"
    );
}

// ===========================================================================
//  action 决策表
// ===========================================================================

/// 架上批次（`IN_PROCESS + PRODUCTION_SHELF`）→ `PICK_UP`。
#[tokio::test]
async fn locate_shelf_batch_in_work_type_returns_pick_up() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let customer = insert_customer_l2(&pool, "LOCATE-SHELF").await;
    let proc = seed_process(&pool, "LOC-SHELF", "工序").await;
    let wt = insert_work_type(&pool, "WT-SHELF", "工种", Some(5)).await;
    link_work_type_to_process(&pool, wt, proc).await;
    let worker = insert_worker(&pool, "BC-SHELF", "工", Some(wt)).await;
    insert_pool_part(
        &pool,
        customer,
        "SN-SHELF",
        PartFixture::PRODUCTION_SHELF_ID,
        proc,
        2,
    )
    .await;

    let (s, env) = locate(&app, &token, "SN-SHELF", Some(worker)).await;
    assert_eq!(s, StatusCode::OK, "{env}");
    let b = first_batch(&env);
    assert_eq!(b["action"], "PICK_UP");
    assert_eq!(b["disabled_reason"], Value::Null);
    assert_eq!(b["location"], "PRODUCTION_SHELF");
}

/// 架上批次的工序**不在**我工种映射里 → **仍是 `PICK_UP`**（写侧 pick-up 的
/// IN_PROCESS 分支不校验工序归属，读侧也不许校验 —— 防「假阻挡」）。
#[tokio::test]
async fn locate_shelf_batch_outside_work_type_still_returns_pick_up() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let customer = insert_customer_l2(&pool, "LOCATE-OUT-WT").await;
    let proc_a = seed_process(&pool, "LOC-A", "工序A").await;
    let proc_b = seed_process(&pool, "LOC-B", "工序B").await;
    let wt = insert_work_type(&pool, "WT-OUT", "工种", Some(5)).await;
    // 工种只映射 proc_b，批次当前工序是 proc_a
    link_work_type_to_process(&pool, wt, proc_b).await;
    let worker = insert_worker(&pool, "BC-OUT", "工", Some(wt)).await;
    insert_pool_part(
        &pool,
        customer,
        "SN-OUT-WT",
        PartFixture::PRODUCTION_SHELF_ID,
        proc_a,
        1,
    )
    .await;

    let (s, env) = locate(&app, &token, "SN-OUT-WT", Some(worker)).await;
    assert_eq!(s, StatusCode::OK, "{env}");
    let b = first_batch(&env);
    assert_eq!(
        b["action"], "PICK_UP",
        "架上批次不许因工序不匹配假阻挡: {env}"
    );
    assert_eq!(b["disabled_reason"], Value::Null);
}

/// 件在我手上（`IN_PROCESS + WORKER` + holder==我）→ `MINE`，
/// **且不受任何闸门影响**（本例工人甚至没绑工种）。
#[tokio::test]
async fn locate_held_by_me_returns_mine() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let customer = insert_customer_l2(&pool, "LOCATE-MINE").await;
    let proc = seed_process(&pool, "LOC-MINE", "工序").await;
    // 刻意不绑工种：MINE 必须绕过 WORK_TYPE_NOT_SET
    let worker = insert_worker(&pool, "BC-MINE", "工", None).await;
    insert_worker_held_part(&pool, customer, "SN-MINE", worker, proc, 1, true).await;

    let (s, env) = locate(&app, &token, "SN-MINE", Some(worker)).await;
    assert_eq!(s, StatusCode::OK, "{env}");
    let b = first_batch(&env);
    assert_eq!(b["action"], "MINE");
    assert_eq!(
        b["disabled_reason"],
        Value::Null,
        "MINE 不受任何闸门影响: {env}"
    );
}

/// 件在别的工人手上 → `TAKE_OVER`（单链 TAIL、推不出下一道 ⇒ 仅过容量闸门）。
#[tokio::test]
async fn locate_held_by_other_returns_take_over() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let customer = insert_customer_l2(&pool, "LOCATE-TK").await;
    let proc = seed_process(&pool, "LOC-TK", "工序").await;
    let wt = insert_work_type(&pool, "WT-TK", "工种", Some(5)).await;
    link_work_type_to_process(&pool, wt, proc).await;
    let holder = insert_worker(&pool, "BC-TK-H", "持有人", Some(wt)).await;
    let other = insert_worker(&pool, "BC-TK-O", "接手人", Some(wt)).await;
    insert_worker_held_part(&pool, customer, "SN-TK", holder, proc, 1, true).await;

    let (s, env) = locate(&app, &token, "SN-TK", Some(other)).await;
    assert_eq!(s, StatusCode::OK, "{env}");
    let b = first_batch(&env);
    assert_eq!(b["action"], "TAKE_OVER");
    assert_eq!(b["disabled_reason"], Value::Null);
}

/// `IN_PROCESS + WORKER` 但 **holder 为 NULL**（数据异常）→ `NONE + UNKNOWN`。
///
/// 2026-10-15 S2 订正：写侧 `take_over` 对同形态拒 `20103`（「挂在别人手上」是接管
/// 的前提），读侧因此**不得**给 `TAKE_OVER`（否则是「写侧会拒绝的却显示可点按钮」）。
#[tokio::test]
async fn locate_worker_location_without_holder_returns_none_unknown() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let customer = insert_customer_l2(&pool, "LOCATE-NOH").await;
    let proc = seed_process(&pool, "LOC-NOH", "工序").await;
    let wt = insert_work_type(&pool, "WT-NOH", "工种", Some(5)).await;
    link_work_type_to_process(&pool, wt, proc).await;
    let holder = insert_worker(&pool, "BC-NOH", "工", Some(wt)).await;
    let (_part, batch, _step) =
        insert_worker_held_part(&pool, customer, "SN-NOH", holder, proc, 1, true).await;
    sqlx::query("UPDATE t_part_batch SET current_holder_id = NULL WHERE id = $1")
        .bind(batch)
        .execute(&pool)
        .await
        .expect("clear holder");

    let (s, env) = locate(&app, &token, "SN-NOH", Some(holder)).await;
    assert_eq!(s, StatusCode::OK, "{env}");
    let b = first_batch(&env);
    assert_eq!(b["location"], "WORKER");
    assert_eq!(b["action"], Value::Null);
    assert_eq!(b["disabled_reason"], "UNKNOWN");
}

/// `TAKE_OVER` 的工序闸门：链 `NEXT` 且指针一致、推导出的下一道**不**在我工种
/// 映射里 → `NONE + PROCESS_NOT_IN_WORK_TYPE`（对位写侧 S2 派生分支的 20104）。
#[tokio::test]
async fn locate_take_over_derived_next_not_in_work_type_returns_none_with_reason() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let customer = insert_customer_l2(&pool, "LOCATE-TK-NEXT").await;
    let proc1 = seed_process(&pool, "LOC-TK-1", "工序1").await;
    let proc2 = seed_process(&pool, "LOC-TK-2", "工序2").await;
    // 接手人只映射 proc1；链 [proc1, proc2] 的下一道 proc2 不在映射里
    let wt = insert_work_type(&pool, "WT-TK-N", "工种", Some(5)).await;
    link_work_type_to_process(&pool, wt, proc1).await;
    let holder = insert_worker(&pool, "BC-TKN-H", "持有人", None).await;
    let other = insert_worker(&pool, "BC-TKN-O", "接手人", Some(wt)).await;
    let (part, _batch, _step) =
        insert_worker_held_part(&pool, customer, "SN-TK-NEXT", holder, proc1, 1, true).await;
    let chain = part_chain_id(&pool, part).await.expect("part 应有链");
    append_chain_step(&pool, chain, proc2, 20).await;

    let (s, env) = locate(&app, &token, "SN-TK-NEXT", Some(other)).await;
    assert_eq!(s, StatusCode::OK, "{env}");
    let b = first_batch(&env);
    assert_eq!(b["chain_state"], "NEXT", "前置：链应推导出下一道: {env}");
    assert_eq!(b["next_process_id"], proc2.to_string());
    assert_eq!(b["action"], Value::Null);
    assert_eq!(b["disabled_reason"], "PROCESS_NOT_IN_WORK_TYPE");
}

/// PENDING + 无链 → `PICK_UP`（链首判定不适用，仅过容量闸门）。
#[tokio::test]
async fn locate_pending_without_chain_returns_pick_up() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let customer = insert_customer_l2(&pool, "LOCATE-P-NOCH").await;
    let proc = seed_process(&pool, "LOC-P-NC", "工序").await;
    let wt = insert_work_type(&pool, "WT-P-NC", "工种", Some(5)).await;
    link_work_type_to_process(&pool, wt, proc).await;
    let worker = insert_worker(&pool, "BC-P-NC", "工", Some(wt)).await;
    let (part, _batch, _steps) =
        insert_pending_part_with_chain(&pool, customer, "SN-P-NOCH", &[proc]).await;
    // 把工单的链解绑（模拟「手写工单没制定工艺链」的常态）
    sqlx::query("UPDATE t_part SET process_chain_id = NULL WHERE id = $1")
        .bind(part)
        .execute(&pool)
        .await
        .expect("unbind chain");

    let (s, env) = locate(&app, &token, "SN-P-NOCH", Some(worker)).await;
    assert_eq!(s, StatusCode::OK, "{env}");
    let b = first_batch(&env);
    assert_eq!(b["status"], "PENDING");
    assert_eq!(b["action"], "PICK_UP");
    assert_eq!(b["disabled_reason"], Value::Null);
}

/// PENDING + 有链、链首 ∈ 我工种映射 → `PICK_UP`。
#[tokio::test]
async fn locate_pending_with_chain_head_in_work_type_returns_pick_up() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let customer = insert_customer_l2(&pool, "LOCATE-P-OK").await;
    let proc1 = seed_process(&pool, "LOC-P-1", "工序1").await;
    let proc2 = seed_process(&pool, "LOC-P-2", "工序2").await;
    let wt = insert_work_type(&pool, "WT-P-OK", "工种", Some(5)).await;
    link_work_type_to_process(&pool, wt, proc1).await;
    let worker = insert_worker(&pool, "BC-P-OK", "工", Some(wt)).await;
    insert_pending_part_with_chain(&pool, customer, "SN-P-OK", &[proc1, proc2]).await;

    let (s, env) = locate(&app, &token, "SN-P-OK", Some(worker)).await;
    assert_eq!(s, StatusCode::OK, "{env}");
    let b = first_batch(&env);
    assert_eq!(b["action"], "PICK_UP");
    assert_eq!(b["disabled_reason"], Value::Null);
}

/// PENDING + 有链、链首 ∉ 我工种映射 → `NONE + PROCESS_NOT_IN_WORK_TYPE`
/// （对位写侧 S2 将加的 20104 闸门）。
#[tokio::test]
async fn locate_pending_with_chain_head_not_in_work_type_returns_none_with_reason() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let customer = insert_customer_l2(&pool, "LOCATE-P-BAD").await;
    let proc1 = seed_process(&pool, "LOC-P-B1", "工序1").await;
    let proc2 = seed_process(&pool, "LOC-P-B2", "工序2").await;
    // 工种只映射 proc2；链首是 proc1
    let wt = insert_work_type(&pool, "WT-P-BAD", "工种", Some(5)).await;
    link_work_type_to_process(&pool, wt, proc2).await;
    let worker = insert_worker(&pool, "BC-P-BAD", "工", Some(wt)).await;
    insert_pending_part_with_chain(&pool, customer, "SN-P-BAD", &[proc1, proc2]).await;

    let (s, env) = locate(&app, &token, "SN-P-BAD", Some(worker)).await;
    assert_eq!(s, StatusCode::OK, "{env}");
    let b = first_batch(&env);
    assert_eq!(b["action"], Value::Null);
    assert_eq!(b["disabled_reason"], "PROCESS_NOT_IN_WORK_TYPE");
}

/// PENDING + 有链但**链内无活跃 step** → `NONE + PROCESS_CHAIN_STEP_NOT_FOUND`
/// （对位写侧 20702：有链却无处可落）。
#[tokio::test]
async fn locate_pending_with_empty_chain_returns_none_with_step_not_found() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let customer = insert_customer_l2(&pool, "LOCATE-P-EMPTY").await;
    let proc = seed_process(&pool, "LOC-P-EM", "工序").await;
    let wt = insert_work_type(&pool, "WT-P-EM", "工种", Some(5)).await;
    link_work_type_to_process(&pool, wt, proc).await;
    let worker = insert_worker(&pool, "BC-P-EM", "工", Some(wt)).await;
    // 空链：part 绑了链但链内一个 step 都没有
    insert_pending_part_with_chain(&pool, customer, "SN-P-EMPTY", &[]).await;

    let (s, env) = locate(&app, &token, "SN-P-EMPTY", Some(worker)).await;
    assert_eq!(s, StatusCode::OK, "{env}");
    let b = first_batch(&env);
    assert_eq!(b["chain_state"], "NONE");
    assert_eq!(b["action"], Value::Null);
    assert_eq!(b["disabled_reason"], "PROCESS_CHAIN_STEP_NOT_FOUND");
}

/// PENDING + 链行**已软删**但链内 step 仍活跃 → `NONE + PROCESS_CHAIN_STEP_NOT_FOUND`。
///
/// 2026-10-15 S2-B 订正的分叉回归：写侧 PENDING 落位走
/// `ProcessChainRepo::first_step_in_chain`，它**带链行软删闸门**（链行已删 ⇒ `None`
/// ⇒ `20702`）；读侧原用 `HAS_PROCESS_CHAIN_EXPR`（无链行闸门）会把该形态判成
/// 可领取。本用例钉住两者重新对齐：链行软删时读侧也不给 `PICK_UP`。
#[tokio::test]
async fn locate_pending_with_soft_deleted_chain_returns_none_with_step_not_found() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let customer = insert_customer_l2(&pool, "LOCATE-P-DELCH").await;
    let proc = seed_process(&pool, "LOC-P-DC", "工序").await;
    let wt = insert_work_type(&pool, "WT-P-DC", "工种", Some(5)).await;
    link_work_type_to_process(&pool, wt, proc).await;
    let worker = insert_worker(&pool, "BC-P-DC", "工", Some(wt)).await;
    let (part, _batch, _steps) =
        insert_pending_part_with_chain(&pool, customer, "SN-P-DELCH", &[proc]).await;
    // 软删链行（step 保持活跃）：写侧 first_step_in_chain 的链行闸门会返 None。
    let chain = part_chain_id(&pool, part).await.expect("part 应有链");
    sqlx::query("UPDATE t_part_process_chain SET deleted_at = now() WHERE id = $1")
        .bind(chain)
        .execute(&pool)
        .await
        .expect("soft delete chain");

    let (s, env) = locate(&app, &token, "SN-P-DELCH", Some(worker)).await;
    assert_eq!(s, StatusCode::OK, "{env}");
    let b = first_batch(&env);
    assert_eq!(
        b["chain_state"], "NONE",
        "锚链软删 ⇒ 链位置派生落 NONE: {env}"
    );
    assert_eq!(
        b["action"],
        Value::Null,
        "写侧必拒 20702，读侧不得给按钮: {env}"
    );
    assert_eq!(b["disabled_reason"], "PROCESS_CHAIN_STEP_NOT_FOUND");
}

/// 工人已持有数 ≥ 工种上限 → `NONE + WORKER_HOLD_LIMIT_EXCEEDED`（对位写侧 20204）。
#[tokio::test]
async fn locate_when_worker_full_returns_none_with_hold_limit_reason() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let customer = insert_customer_l2(&pool, "LOCATE-FULL").await;
    let proc = seed_process(&pool, "LOC-FULL", "工序").await;
    let wt = insert_work_type(&pool, "WT-FULL", "工种", Some(1)).await;
    link_work_type_to_process(&pool, wt, proc).await;
    let worker = insert_worker(&pool, "BC-FULL", "工", Some(wt)).await;
    // 工人已经拿满 1/1
    insert_worker_held_part(&pool, customer, "SN-FULL-HELD", worker, proc, 1, true).await;
    // 目标：另一件架上可领批次
    insert_pool_part(
        &pool,
        customer,
        "SN-FULL-TARGET",
        PartFixture::PRODUCTION_SHELF_ID,
        proc,
        1,
    )
    .await;

    let (s, env) = locate(&app, &token, "SN-FULL-TARGET", Some(worker)).await;
    assert_eq!(s, StatusCode::OK, "{env}");
    let b = first_batch(&env);
    assert_eq!(b["action"], Value::Null);
    assert_eq!(b["disabled_reason"], "WORKER_HOLD_LIMIT_EXCEEDED");
}

/// 工种未配 `max_held_batches`（`NULL` = 未配置）→ `NONE + WORK_TYPE_MAX_HELD_NOT_SET`
/// （对位写侧 20904）。
#[tokio::test]
async fn locate_when_work_type_max_not_set_returns_none_with_reason() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let customer = insert_customer_l2(&pool, "LOCATE-NO-MAX").await;
    let proc = seed_process(&pool, "LOC-NM", "工序").await;
    let wt = insert_work_type(&pool, "WT-NM", "工种", None).await;
    link_work_type_to_process(&pool, wt, proc).await;
    let worker = insert_worker(&pool, "BC-NM", "工", Some(wt)).await;
    insert_pool_part(
        &pool,
        customer,
        "SN-NO-MAX",
        PartFixture::PRODUCTION_SHELF_ID,
        proc,
        1,
    )
    .await;

    let (s, env) = locate(&app, &token, "SN-NO-MAX", Some(worker)).await;
    assert_eq!(s, StatusCode::OK, "{env}");
    let b = first_batch(&env);
    assert_eq!(b["action"], Value::Null);
    assert_eq!(b["disabled_reason"], "WORK_TYPE_MAX_HELD_NOT_SET");
}

/// 工人未绑工种 → 除 `MINE` 外一切可行动作落 `NONE + WORK_TYPE_NOT_SET`。
#[tokio::test]
async fn locate_worker_without_work_type_returns_none_with_reason() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let customer = insert_customer_l2(&pool, "LOCATE-NO-WT").await;
    let proc = seed_process(&pool, "LOC-NW", "工序").await;
    let worker = insert_worker(&pool, "BC-NW", "工", None).await;
    insert_pool_part(
        &pool,
        customer,
        "SN-NO-WT",
        PartFixture::PRODUCTION_SHELF_ID,
        proc,
        1,
    )
    .await;

    let (s, env) = locate(&app, &token, "SN-NO-WT", Some(worker)).await;
    assert_eq!(s, StatusCode::OK, "{env}");
    let b = first_batch(&env);
    assert_eq!(b["action"], Value::Null);
    assert_eq!(b["disabled_reason"], "WORK_TYPE_NOT_SET");
}

/// 外协批次（`OUTSOURCE + OUTSOURCE_COMPANY`、无链）→ `RECEIVE_FROM_OUTSOURCE`，
/// 且 `company_name` / `sent_at` 带上在途公司与开口 shipment 的发出时间。
#[tokio::test]
async fn locate_outsource_batch_returns_receive_action_with_company_and_sent_at() {
    use chrono::NaiveDate;
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let customer = insert_customer_l2(&pool, "LOCATE-OS").await;
    let proc = seed_process(&pool, "LOC-OS", "工序").await;
    let wt = insert_work_type(&pool, "WT-OS", "工种", Some(5)).await;
    link_work_type_to_process(&pool, wt, proc).await;
    let worker = insert_worker(&pool, "BC-OS", "工", Some(wt)).await;
    let company = insert_outsource_company(&pool, "外协公司-甲").await;
    let (part, batch, _step) =
        insert_worker_held_part(&pool, customer, "SN-OS", worker, proc, 1, false).await;
    force_batch_state(
        &pool,
        batch,
        "OUTSOURCE",
        Some("OUTSOURCE_COMPANY"),
        Some(company),
    )
    .await;
    let sent_at = NaiveDate::from_ymd_opt(2026, 6, 1)
        .unwrap()
        .and_hms_opt(8, 30, 0)
        .unwrap();
    insert_open_shipment(&pool, part, batch, company, proc, sent_at).await;

    let (s, env) = locate(&app, &token, "SN-OS", Some(worker)).await;
    assert_eq!(s, StatusCode::OK, "{env}");
    let b = first_batch(&env);
    assert_eq!(b["action"], "RECEIVE_FROM_OUTSOURCE");
    assert_eq!(b["disabled_reason"], Value::Null);
    assert_eq!(b["company_name"], "外协公司-甲");
    assert_eq!(b["holder_display"], "外协公司-甲");
    assert_eq!(
        b["sent_at"],
        serde_json::to_value(sent_at).unwrap(),
        "sent_at 必须是开口 shipment 的发出时间: {env}"
    );
}

/// 外协批次 `OUTSOURCE + OUTSOURCE_COMPANY` 但 **holder 为 NULL**（数据异常）
/// → `NONE + UNKNOWN`（2026-10-15 收尾订正）。
///
/// 写侧 `outsource_receive` 从 holder 取 `company_id`，同形态取不到 ⇒ 拒 `20122`；
/// 读侧此前只看 location、仍给 `RECEIVE_FROM_OUTSOURCE`（假按钮）。本用例钉住两侧
/// 对齐：读侧同样不给按钮，落保守出口 `UNKNOWN`（与
/// `locate_worker_location_without_holder_returns_none_unknown` 同款纪律）。
#[tokio::test]
async fn locate_outsource_holder_null_returns_unknown() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let customer = insert_customer_l2(&pool, "LOCATE-OS-NOH").await;
    let proc = seed_process(&pool, "LOC-OS-NOH", "工序").await;
    let wt = insert_work_type(&pool, "WT-OS-NOH", "工种", Some(5)).await;
    link_work_type_to_process(&pool, wt, proc).await;
    let worker = insert_worker(&pool, "BC-OS-NOH", "工", Some(wt)).await;
    let company = insert_outsource_company(&pool, "外协公司-戊").await;
    let (_part, batch, _step) =
        insert_worker_held_part(&pool, customer, "SN-OS-NOH", worker, proc, 1, false).await;
    force_batch_state(
        &pool,
        batch,
        "OUTSOURCE",
        Some("OUTSOURCE_COMPANY"),
        Some(company),
    )
    .await;
    // 制造数据异常形态：外协在途但无持有人（写侧从此处取不到 company_id）。
    sqlx::query("UPDATE t_part_batch SET current_holder_id = NULL WHERE id = $1")
        .bind(batch)
        .execute(&pool)
        .await
        .expect("clear holder");

    let (s, env) = locate(&app, &token, "SN-OS-NOH", Some(worker)).await;
    assert_eq!(s, StatusCode::OK, "{env}");
    let b = first_batch(&env);
    assert_eq!(b["status"], "OUTSOURCE");
    assert_eq!(b["location"], "OUTSOURCE_COMPANY");
    assert_eq!(
        b["action"],
        Value::Null,
        "holder 为 NULL 时写侧拒 20122，读侧不得给按钮: {env}"
    );
    assert_eq!(b["disabled_reason"], "UNKNOWN");
}

/// 外协批次 + 有链且 `NEXT`、推导出的下一道 ∉ 我工种映射
/// → `NONE + PROCESS_NOT_IN_WORK_TYPE`（对位 S3 写侧将加的闸门）。
#[tokio::test]
async fn locate_outsource_chain_next_not_in_work_type_returns_none_with_reason() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let customer = insert_customer_l2(&pool, "LOCATE-OS-NEXT").await;
    let proc1 = seed_process(&pool, "LOC-OS-1", "工序1").await;
    let proc2 = seed_process(&pool, "LOC-OS-2", "工序2").await;
    let wt = insert_work_type(&pool, "WT-OS-N", "工种", Some(5)).await;
    link_work_type_to_process(&pool, wt, proc1).await;
    let worker = insert_worker(&pool, "BC-OS-N", "工", Some(wt)).await;
    let company = insert_outsource_company(&pool, "外协公司-乙").await;
    // 有链 [proc1, proc2]、批次当前工序 proc1 + 指针一致 ⇒ NEXT
    let (part, batch, _step) =
        insert_worker_held_part(&pool, customer, "SN-OS-NEXT", worker, proc1, 1, true).await;
    let chain = part_chain_id(&pool, part).await.expect("part 应有链");
    append_chain_step(&pool, chain, proc2, 20).await;
    force_batch_state(
        &pool,
        batch,
        "OUTSOURCE",
        Some("OUTSOURCE_COMPANY"),
        Some(company),
    )
    .await;

    let (s, env) = locate(&app, &token, "SN-OS-NEXT", Some(worker)).await;
    assert_eq!(s, StatusCode::OK, "{env}");
    let b = first_batch(&env);
    assert_eq!(b["chain_state"], "NEXT", "前置：链应推导出下一道: {env}");
    assert_eq!(b["action"], Value::Null);
    assert_eq!(b["disabled_reason"], "PROCESS_NOT_IN_WORK_TYPE");
}

/// 外协批次 + 有链但推导不出 `NEXT`（当前工序已被清空）
/// → **仍给 `RECEIVE_FROM_OUTSOURCE`**（2026-10-15 S3 订正：此前给
/// `NONE + PROCESS_CHAIN_REQUIRED`，是**假阻挡** —— 写端点
/// `POST /scan/outsource-receive` 支持前端手填 `next_process_id`，
/// 推导不出只是「前端要弹手填」，不是「不能点」）。
#[tokio::test]
async fn locate_outsource_chain_without_position_still_returns_receive_action() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let customer = insert_customer_l2(&pool, "LOCATE-OS-NOPOS").await;
    let proc = seed_process(&pool, "LOC-OS-NP", "工序").await;
    let wt = insert_work_type(&pool, "WT-OS-NP", "工种", Some(5)).await;
    link_work_type_to_process(&pool, wt, proc).await;
    let worker = insert_worker(&pool, "BC-OS-NP", "工", Some(wt)).await;
    let company = insert_outsource_company(&pool, "外协公司-丙").await;
    let (part, batch, _step) =
        insert_worker_held_part(&pool, customer, "SN-OS-NOPOS", worker, proc, 1, true).await;
    force_batch_state(
        &pool,
        batch,
        "OUTSOURCE",
        Some("OUTSOURCE_COMPANY"),
        Some(company),
    )
    .await;
    // 出池清工序不变式：外协批次的当前工序与 step 指针都为 NULL
    sqlx::query(
        "UPDATE t_part_batch SET current_process_id = NULL, current_process_step_id = NULL \
         WHERE id = $1",
    )
    .bind(batch)
    .execute(&pool)
    .await
    .expect("clear process columns");
    // 前置：工单仍然绑着链
    assert!(part_chain_id(&pool, part).await.is_some());

    let (s, env) = locate(&app, &token, "SN-OS-NOPOS", Some(worker)).await;
    assert_eq!(s, StatusCode::OK, "{env}");
    let b = first_batch(&env);
    assert_eq!(b["chain_state"], "NONE");
    assert_eq!(
        b["action"], "RECEIVE_FROM_OUTSOURCE",
        "推不出下一道不是不能点（写侧支持手填 next_process_id）: {env}"
    );
    assert_eq!(b["disabled_reason"], Value::Null);
}

/// 外协批次 + **无链** + 工人容量已满 → `NONE + WORKER_HOLD_LIMIT_EXCEEDED`。
///
/// 2026-10-15 S3 回归：放开「推不出下一道」的链判定后，容量闸门仍必须在 ——
/// 无链形态走「前端手填下一道」的流，但手填之前工人得还有额度
/// （对位写侧 `POST /scan/outsource-receive` 的容量预检 `20204` / `20904`）。
#[tokio::test]
async fn locate_outsource_manual_next_capacity_gate_still_applies() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let customer = insert_customer_l2(&pool, "LOCATE-OS-MAN").await;
    let proc = seed_process(&pool, "LOC-OS-MAN", "工序").await;
    let wt = insert_work_type(&pool, "WT-OS-MAN", "工种", Some(1)).await;
    link_work_type_to_process(&pool, wt, proc).await;
    let worker = insert_worker(&pool, "BC-OS-MAN", "工", Some(wt)).await;
    let company = insert_outsource_company(&pool, "外协公司-丁").await;
    // 工人已经拿满 1/1
    insert_worker_held_part(&pool, customer, "SN-OS-MAN-HELD", worker, proc, 1, true).await;
    // 目标：无链外协在途批次（手填下一道形态）
    let (_part, batch, _step) =
        insert_worker_held_part(&pool, customer, "SN-OS-MAN", worker, proc, 1, false).await;
    force_batch_state(
        &pool,
        batch,
        "OUTSOURCE",
        Some("OUTSOURCE_COMPANY"),
        Some(company),
    )
    .await;

    let (s, env) = locate(&app, &token, "SN-OS-MAN", Some(worker)).await;
    assert_eq!(s, StatusCode::OK, "{env}");
    let b = first_batch(&env);
    assert_eq!(
        b["action"],
        Value::Null,
        "容量触顶时不得给可点按钮（哪怕链判定已放开）: {env}"
    );
    assert_eq!(b["disabled_reason"], "WORKER_HOLD_LIMIT_EXCEEDED");
}

/// 2026-10-16 review 第 1 轮 MINOR-1 回归：工种映射**只含一条已软删工序**时，
/// 读侧仍给 `TAKE_OVER`（闸门候选集与写侧 `WorkTypeRepo::list_process_ids` 逐字
/// 同源，含已软删工序），但出参 `worker_processes` 是**空数组**（软删工序没有可
/// 展示的名字）。
///
/// ⚠️ 改前 `fetch_worker_processes` 用的是 `JOIN t_process`，该行被软删闸门滤掉
/// ⇒ 闸门判 0 候选 ⇒ 落 `PROCESS_NOT_IN_WORK_TYPE` 藏掉按钮，而写侧此时
/// `list_process_ids` 返 1 条 ⇒ **单候选自动推导会成功** —— 一个「写侧会成功、
/// 读侧说做不了」的**假阻挡**（本仓明令禁止，`listing/service.rs` 2026-10-15 S3
/// 那次订正就是为消灭它）。
#[tokio::test]
async fn locate_take_over_with_only_soft_deleted_process_mapping_still_returns_action() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let customer = insert_customer_l2(&pool, "LOCATE-WPDEL").await;
    let proc = seed_process(&pool, "LOC-WPDEL", "已软删工序").await;
    let wt = insert_work_type(&pool, "WT-WPDEL", "工种", Some(5)).await;
    link_work_type_to_process(&pool, wt, proc).await;
    // 工序作废，但工种↔工序映射仍在（`wtp.deleted_at IS NULL`）—— 写侧看得到它。
    sqlx::query("UPDATE t_process SET deleted_at = now() WHERE id = $1")
        .bind(proc)
        .execute(&pool)
        .await
        .expect("soft delete t_process");
    let holder = insert_worker(&pool, "BC-WPDEL-H", "持有人", Some(wt)).await;
    let taker = insert_worker(&pool, "BC-WPDEL-T", "接管人", Some(wt)).await;
    // ⚠️ `with_chain=false` **不等于「无链」**：fixture 仍建链 + step，并把批次的
    // `current_process_step_id` 指过去；只把 part 的 `process_chain_id` 留空 ⇒ 读侧
    // 锚链表达式是 `COALESCE(p.process_chain_id, cur.chain_id)`，会回退捞到那条
    // **未绑链** ⇒ 落 `chain_state == "TAIL"`（**不是 `NONE`**）。本用例要的只是
    // 「非链推导形态」（`take_over_gate` 改判「工种有没有候选」），TAIL / NONE 两种
    // 非链推导形态走**同一分支**，故断言结果不受影响。
    // ⚠️ 想要真正的 `NONE` 形态（指针为 NULL）请照
    // `locate_outsource_empty_work_type_candidate_returns_none_with_reason` 的显式
    // 清列写法（那条同时清了 `current_process_id` 与 `current_process_step_id` 并
    // 断言 `chain_state == "NONE"`），别照注释字面理解 `with_chain=false` 的语义。
    let (_part, batch, _step) =
        insert_worker_held_part(&pool, customer, "SN-WPDEL", holder, proc, 1, false).await;

    let (s, env) = locate(&app, &token, "SN-WPDEL", Some(taker)).await;
    assert_eq!(s, StatusCode::OK, "{env}");
    let b = first_batch(&env);
    assert_eq!(b["batch_id"], batch.to_string(), "前置：本用例针对该批次: {env}");
    assert_eq!(
        b["action"], "TAKE_OVER",
        "写侧单候选自动推导会成功，读侧不得藏按钮（假阻挡）: {env}"
    );
    assert_eq!(b["disabled_reason"], Value::Null);
    assert_eq!(
        env["data"]["worker_processes"],
        json!([]),
        "出参是窄集合：软删工序没有可展示的名字，不进 picker: {env}"
    );
}

/// 2026-10-16 review 第 1 轮 MINOR-4 补齐：`outsource_gate` 的「工种零候选 ⇒
/// `PROCESS_NOT_IN_WORK_TYPE`」**读侧**用例（此前只测了 `TAKE_OVER` 一侧；两个
/// gate 是两个独立函数，只测一个等于放过另一个）。
///
/// 对位写侧：`POST /scan/outsource-receive` 判据 5 的「0 条 ⇒ `20104` 工人 X 的
/// 工种未映射任何工序」。无链形态（`chain_state=NONE`）走「改判工种候选」那条。
#[tokio::test]
async fn locate_outsource_empty_work_type_candidate_returns_none_with_reason() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let customer = insert_customer_l2(&pool, "LOCATE-OS-EMPTY").await;
    let proc = seed_process(&pool, "LOC-OS-EMPTY", "工序").await;
    // 工种存在但**不 link** 任何工序 ⇒ 写侧判据 5 的候选集为空
    let wt = insert_work_type(&pool, "WT-OS-EMPTY", "空工种", Some(5)).await;
    let worker = insert_worker(&pool, "BC-OS-EMPTY", "工", Some(wt)).await;
    let company = insert_outsource_company(&pool, "外协公司-己").await;
    let (_part, batch, _step) =
        insert_worker_held_part(&pool, customer, "SN-OS-EMPTY", worker, proc, 1, false).await;
    force_batch_state(
        &pool,
        batch,
        "OUTSOURCE",
        Some("OUTSOURCE_COMPANY"),
        Some(company),
    )
    .await;
    // 出池清工序不变式：外协批次的当前工序与 step 指针都清空 ⇒ 链推导整体不进
    // （⚠️ 否则 `insert_worker_held_part` 造的「未绑链 step」会被锚链回退
    // `COALESCE(p.process_chain_id, cur.chain_id)` 捞回来，落 `TAIL` 而非 `NONE`）。
    sqlx::query(
        "UPDATE t_part_batch SET current_process_id = NULL, current_process_step_id = NULL \
         WHERE id = $1",
    )
    .bind(batch)
    .execute(&pool)
    .await
    .expect("clear process columns");

    let (s, env) = locate(&app, &token, "SN-OS-EMPTY", Some(worker)).await;
    assert_eq!(s, StatusCode::OK, "{env}");
    let b = first_batch(&env);
    assert_eq!(b["chain_state"], "NONE", "前置：无链形态: {env}");
    assert_eq!(
        b["action"],
        Value::Null,
        "零候选时写侧必拒 20104，读侧不得给可点按钮: {env}"
    );
    assert_eq!(b["disabled_reason"], "PROCESS_NOT_IN_WORK_TYPE");
    assert_eq!(env["data"]["worker_processes"], json!([]));
}

/// 品检中批次 → `NONE + INSPECTION`（指引找品检）。
#[tokio::test]
async fn locate_inspection_batch_returns_none_inspection_reason() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let customer = insert_customer_l2(&pool, "LOCATE-INSP").await;
    let proc = seed_process(&pool, "LOC-INSP", "工序").await;
    let wt = insert_work_type(&pool, "WT-INSP", "工种", Some(5)).await;
    link_work_type_to_process(&pool, wt, proc).await;
    let worker = insert_worker(&pool, "BC-INSP", "工", Some(wt)).await;
    let (_part, batch, _step) =
        insert_worker_held_part(&pool, customer, "SN-INSP", worker, proc, 1, true).await;
    force_batch_state(&pool, batch, "INSPECTION", Some("INSPECTION_SHELF"), None).await;

    let (s, env) = locate(&app, &token, "SN-INSP", Some(worker)).await;
    assert_eq!(s, StatusCode::OK, "{env}");
    let b = first_batch(&env);
    assert_eq!(b["status"], "INSPECTION");
    assert_eq!(b["action"], Value::Null);
    assert_eq!(b["disabled_reason"], "INSPECTION");
}

/// 终态批次（CANCELLED）→ `NONE + TERMINAL`。
#[tokio::test]
async fn locate_terminal_batch_returns_none_terminal_reason() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let customer = insert_customer_l2(&pool, "LOCATE-TERM").await;
    let proc = seed_process(&pool, "LOC-TERM", "工序").await;
    let wt = insert_work_type(&pool, "WT-TERM", "工种", Some(5)).await;
    link_work_type_to_process(&pool, wt, proc).await;
    let worker = insert_worker(&pool, "BC-TERM", "工", Some(wt)).await;
    let (part, _batch, _step) =
        insert_worker_held_part(&pool, customer, "SN-TERM", worker, proc, 1, true).await;
    insert_batch(&pool, part, 2, "CANCELLED", None, None, None, None, 1).await;

    let (s, env) = locate(&app, &token, "SN-TERM", Some(worker)).await;
    assert_eq!(s, StatusCode::OK, "{env}");
    let batches = env["data"]["batches"].as_array().expect("batches");
    let cancelled = batches
        .iter()
        .find(|b| b["status"] == "CANCELLED")
        .expect("应有 CANCELLED 行");
    assert_eq!(cancelled["action"], Value::Null);
    assert_eq!(cancelled["disabled_reason"], "TERMINAL");
}

/// 待编程批次 → `NONE + PROGRAMMING`（指引找 CNC 编程）。
#[tokio::test]
async fn locate_programming_batch_returns_none_programming_reason() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let customer = insert_customer_l2(&pool, "LOCATE-PROG").await;
    let proc = seed_process(&pool, "LOC-PROG", "工序").await;
    let wt = insert_work_type(&pool, "WT-PROG", "工种", Some(5)).await;
    link_work_type_to_process(&pool, wt, proc).await;
    let worker = insert_worker(&pool, "BC-PROG", "工", Some(wt)).await;
    let (_part, batch, _step) =
        insert_worker_held_part(&pool, customer, "SN-PROG", worker, proc, 1, true).await;
    force_batch_state(&pool, batch, "PROGRAMMING", None, None).await;

    let (s, env) = locate(&app, &token, "SN-PROG", Some(worker)).await;
    assert_eq!(s, StatusCode::OK, "{env}");
    let b = first_batch(&env);
    assert_eq!(b["status"], "PROGRAMMING");
    assert_eq!(b["action"], Value::Null);
    assert_eq!(b["disabled_reason"], "PROGRAMMING");
}

// ===========================================================================
//  字段契约
// ===========================================================================

/// 逐键断言 `LocatePartBrief` / `LocateBatchItem` 的全部键 + 雪花 id 的
/// string 形态 + `chain_state` 的三个字面量全部可达（NONE / TAIL / NEXT）。
#[tokio::test]
async fn locate_field_contract_pins_every_key() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let customer = insert_customer_l2(&pool, "LOCATE-KEYS").await;
    let proc1 = seed_process(&pool, "LOC-K1", "工序1").await;
    let proc2 = seed_process(&pool, "LOC-K2", "工序2").await;
    let wt = insert_work_type(&pool, "WT-KEYS", "工种", Some(5)).await;
    link_work_type_to_process(&pool, wt, proc1).await;
    let worker = insert_worker(&pool, "BC-KEYS", "工", Some(wt)).await;

    // 三形态：NONE（清掉 step 指针）/ TAIL（单链）/ NEXT（双链且当前在第一道）
    let (part_none, batch_none, _s1) =
        insert_worker_held_part(&pool, customer, "SN-KEYS-NONE", worker, proc1, 1, true).await;
    // ⚠️ fixture 的 `with_chain=false` 会留下**孤儿链 + step 指针**，而链位置派生的
    // 锚链是 `COALESCE(p.process_chain_id, cur.chain_id)`（回退到指针所属链）⇒ 那样
    // 反而得 TAIL。真正的 `NONE` 形态是「指针为 NULL」：清掉它。
    sqlx::query("UPDATE t_part_batch SET current_process_step_id = NULL WHERE id = $1")
        .bind(batch_none)
        .execute(&pool)
        .await
        .expect("clear step pointer");
    let (_part_tail, batch_tail, _s2) =
        insert_worker_held_part(&pool, customer, "SN-KEYS-TAIL", worker, proc1, 1, true).await;
    let (part_next, batch_next, _s3) =
        insert_worker_held_part(&pool, customer, "SN-KEYS-NEXT", worker, proc1, 1, true).await;
    let chain_next = part_chain_id(&pool, part_next).await.expect("part 应有链");
    append_chain_step(&pool, chain_next, proc2, 20).await;

    // ---- 逐键断言（取无链那一件，action 非空也要逐键出现）----
    let (s, env) = locate(&app, &token, "SN-KEYS-NONE", Some(worker)).await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(env["data"]["part"]["id"], part_none.to_string());
    assert_eq!(env["data"]["part"]["serial_no"], "SN-KEYS-NONE");
    assert_eq!(env["data"]["part"]["name"], "held-item");
    assert_eq!(env["data"]["part"]["drawing_no"], "D-HELD");
    assert_eq!(env["data"]["part"]["is_urgent"], false);
    assert!(env["data"]["part"]["planned_delivery_date"].is_string());
    // fixture（`insert_worker_held_part`）把系统交期写成当天 ⇒ 非 null
    assert!(env["data"]["part"]["system_delivery_date"].is_string());
    let mut part_keys: Vec<&str> = env["data"]["part"]
        .as_object()
        .expect("part 是对象")
        .keys()
        .map(String::as_str)
        .collect();
    part_keys.sort_unstable();
    assert_eq!(
        part_keys,
        [
            "drawing_no",
            "id",
            "is_urgent",
            "name",
            "planned_delivery_date",
            "serial_no",
            "system_delivery_date",
        ],
        "LocatePartBrief 字段集变了（同步前端 schema）: {env}"
    );

    let b = first_batch(&env);
    assert_eq!(b["batch_id"], batch_none.to_string());
    assert_eq!(b["batch_no"], 1);
    assert_eq!(b["quantity"], 1);
    assert_eq!(b["status"], "IN_PROCESS");
    assert_eq!(b["version"], 0);
    assert_eq!(b["location"], "WORKER");
    assert_eq!(b["holder_display"], "工");
    assert_eq!(b["current_process_name"], "工序1");
    assert_eq!(b["chain_state"], "NONE");
    assert_eq!(b["next_process_id"], Value::Null);
    assert_eq!(b["next_process_name"], Value::Null);
    assert!(b["updated_at"].is_string());
    assert_eq!(b["company_name"], Value::Null);
    assert_eq!(b["sent_at"], Value::Null);
    assert_eq!(b["action"], "MINE");
    assert_eq!(b["disabled_reason"], Value::Null);
    let mut item_keys: Vec<&str> = b
        .as_object()
        .expect("batch 行是对象")
        .keys()
        .map(String::as_str)
        .collect();
    item_keys.sort_unstable();
    assert_eq!(
        item_keys,
        [
            "action",
            "batch_id",
            "batch_no",
            "chain_state",
            "company_name",
            "current_process_name",
            "disabled_reason",
            "holder_display",
            "location",
            "next_process_id",
            "next_process_name",
            "quantity",
            "sent_at",
            "status",
            "updated_at",
            "version",
        ],
        "LocateBatchItem 字段集变了（同步前端 schema）: {env}"
    );

    // ---- 顶层键集合（2026-10-16 起 3 键：多了 `worker_processes`）----
    let mut out_keys: Vec<&str> = env["data"]
        .as_object()
        .expect("data 是对象")
        .keys()
        .map(String::as_str)
        .collect();
    out_keys.sort_unstable();
    assert_eq!(
        out_keys,
        ["batches", "part", "worker_processes"],
        "LocateOut 顶层键集变了（同步前端 schema）: {env}"
    );

    // ---- chain_state 三字面量全部可达 ----
    let mut seen = Vec::new();
    for (serial, batch_id) in [
        ("SN-KEYS-NONE", batch_none),
        ("SN-KEYS-TAIL", batch_tail),
        ("SN-KEYS-NEXT", batch_next),
    ] {
        let (s, env) = locate(&app, &token, serial, Some(worker)).await;
        assert_eq!(s, StatusCode::OK, "{serial}: {env}");
        let b = first_batch(&env);
        assert_eq!(b["batch_id"], batch_id.to_string());
        seen.push(b["chain_state"].as_str().unwrap_or_default().to_string());
    }
    seen.sort();
    assert_eq!(
        seen,
        ["NEXT", "NONE", "TAIL"],
        "三字面量必须都可达: {seen:?}"
    );
}

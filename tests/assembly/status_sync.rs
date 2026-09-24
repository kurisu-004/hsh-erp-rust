//! 父装配件 status 自动聚合（auto-rollup）集成测试
//!
//! 覆盖 `assembly-status-auto-sync` 特性 Task 6 验收点：
//!   1. `single_part_to_inspection_flips_assembly_to_in_process`
//!      —— 单件 PENDING → INSPECTION 触发父 asm PENDING → IN_PROCESS
//!   2. `mixed_children_assembly_rolls_up_to_min_progress`
//!      —— 一子 COMPLETED + 一子 INSPECTION → 父 = IN_PROCESS（min progress 非 0）
//!   3. `all_children_cancelled_flips_assembly_to_cancelled`
//!      —— 2 件子全部 CANCELLED → 父 = CANCELLED（cancel 端点不触发 sync，所以用
//!      `AssemblyService::sync_from_part_change` 直接驱动 sync 来覆盖该路径）
//!   4. `terminal_assembly_is_not_modified_by_child_change`
//!      —— 父已是终态（COMPLETED / CANCELLED）→ 子变更不触发 version 自增
//!   5. `batch_to_inspection_emits_per_assembly_update`
//!      —— 批量 to-inspection 跨两个 assembly，每个 assembly 仅在「target ≠ current」
//!      那一项返回 `synced_assembly_id: Some(...)`；handler 侧用 HashSet 去重
//!      保证每个 asm 仅广播一次 `ASSEMBLY_UPDATED`。
//!
//! ## 并行 / 认证
//! 进程级 test_pool 每次 fresh database（plan 2 2026-09-20），DB 间 schema
//! 完全独立，无需 Mutex 串行化。
//!
//! ## Fixture 范本化（2026-09-24 PR13 Phase C.Final）
//! 本文件原 `#[path = "../part/helpers.rs"] mod helpers;` + `use helpers::*;`
//! 改走 `use hsh_erp_test_support::*` + `load_assembly_fixture(&pool)` +
//! `AssemblyFixture` + `bootstrap_as_inspector` 样板。
//!
//! `load_assembly_fixture` 内部先调 `load_part_fixture`（复用 part 域基线
//! INSPECTION / PRODUCTION 货架 + INSPECTOR 用户），再加 1 行 `t_serial_counter`
//! （'F' prefix）。本测试域独享的 helper（`insert_l1` / `insert_l2` /
//! `insert_part_with_status` / `insert_batch` / `batch_version` /
//! `setup_inspection_and_production_shelves`）保留为本地函数——这些是 part 域
//! 测试模式的具体实现，不属于跨域共享资产。字面请求 / 断言逐字保留。

use axum::http::StatusCode;
use serde_json::json;
use sqlx::PgPool;

use hsh_erp_rust::auth::rbac::{CurrentUser, Role};
use hsh_erp_rust::infra::clock::now_naive;
use hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator;
use hsh_erp_rust::modules::assembly::service::{AssemblyService, SyncOutcome};

// 2026-09-24 PR13 Phase C.Final：fixture 范本化入口。
// `load_assembly_fixture(&pool)` 加载 part 域基线（INSPECTOR 用户 +
// INSPECTION/PRODUCTION 货架）+ 1 行 t_serial_counter(prefix='F'); 多数测试
// 用本地 `insert_l1` / `insert_l2` 等 helper 创建专属测试数据。
use hsh_erp_test_support::{
    AssemblyFixture, PartFixture, json_request, load_assembly_fixture, login_token, pool_snowflake,
    send, test_app, test_pool, test_state,
};

// ===========================================================================
//  本地 domain helpers（part 域测试模式的具体实现，不属于跨域共享资产）
// ===========================================================================

/// 插 L1 客户（一级；带 serial_prefix）。原 `tests/part/helpers.rs::insert_l1`。
async fn insert_l1(pool: &PgPool, name: &str, prefix: &str) -> i64 {
    let id = pool_snowflake().lock().unwrap().next_id();
    let now = now_naive();
    sqlx::query!(
        "INSERT INTO t_customer (id, name, parent_id, serial_prefix, version, \
         created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, NULL, $3, 0, $4, NULL, $4, NULL)",
        id,
        name,
        prefix,
        now,
    )
    .execute(pool)
    .await
    .expect("insert L1");
    id
}

/// 插 L2 客户（二级；挂在 L1 下，无 prefix）。原 `tests/part/helpers.rs::insert_l2`。
async fn insert_l2(pool: &PgPool, name: &str, l1_id: i64) -> i64 {
    let id = pool_snowflake().lock().unwrap().next_id();
    let now = now_naive();
    sqlx::query!(
        "INSERT INTO t_customer (id, name, parent_id, serial_prefix, version, \
         created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, $3, NULL, 0, $4, NULL, $4, NULL)",
        id,
        name,
        l1_id,
        now,
    )
    .execute(pool)
    .await
    .expect("insert L2");
    id
}

/// 带 status 参数的 part 插入。参数化 status 适配 INSPECTION / IN_PROCESS /
/// PENDING / PROGRAMMING / READY_TO_SHIP（与 to_inspection / to_ship 等端点
/// 所需起始 status 形状保持一致）。
async fn insert_part_with_status(
    pool: &PgPool,
    name: &str,
    customer_id: i64,
    serial_no: Option<&str>,
    assembly_id: Option<i64>,
    status: &str,
) -> i64 {
    let id = pool_snowflake().lock().unwrap().next_id();
    let now = now_naive();
    let today = now.date();
    // 2026-09-16 PR-2（migration 027）：t_part 删 `has_been_repaired` 等 6 个批次
    // 依附列；从 INSERT 列名与 VALUES 占位符同步移除。
    sqlx::query!(
        "INSERT INTO t_part (id, serial_no, name, drawing_no, customer_id, status, \
         applicant_name, request_date, planned_delivery_date, \
         quantity, version, created_at, created_by, updated_at, updated_by, \
         assembly_id) \
         VALUES ($1, $2, $3, 'D-001', $4, $8, $3, $6, $6, 1, 0, $5, NULL, $5, NULL, $7)",
        id,
        serial_no,
        name,
        customer_id,
        now,
        today,
        assembly_id,
        status,
    )
    .execute(pool)
    .await
    .expect("insert part");
    id
}

/// 插一个 part_batch（带 status 参数，与 part.status 通常对齐）。
async fn insert_batch(
    pool: &PgPool,
    part_id: i64,
    batch_no: i32,
    qty: i32,
    status: &str,
) -> i64 {
    let id = pool_snowflake().lock().unwrap().next_id();
    let now = now_naive();
    // 2026-09-16 PR-2（migration 027）：t_part_batch 删 `has_been_repaired`；
    // INSERT 列名与 VALUES 占位符同步移除 `false` 字面量。
    sqlx::query!(
        "INSERT INTO t_part_batch (id, part_id, batch_no, quantity, status, \
         version, created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, $3, $4, $5, 0, $6, NULL, $6, NULL)",
        id,
        part_id,
        batch_no,
        qty,
        status,
        now,
    )
    .execute(pool)
    .await
    .expect("insert batch");
    id
}

/// 读取批次当前乐观锁版本（测试构造 to-XXX / batch-to-XXX payload 用）。
async fn batch_version(pool: &PgPool, batch_id: i64) -> i32 {
    sqlx::query_scalar::<_, i32>("SELECT version FROM t_part_batch WHERE id = $1")
        .bind(batch_id)
        .fetch_one(pool)
        .await
        .expect("batch not found")
}

/// 取 fixture 预置的 INSPECTION / PRODUCTION shelf ID + 关联 process ID。
///
/// 原 `tests/part/helpers.rs::setup_inspection_and_production_shelves` 走真实
/// INSERT（INSP-001 / PROD-001 + process + shelf_process 映射）。AssemblyFixture
/// 通过 `load_part_fixture` 复用 part 域基线 shelf + process 行，ID 直接
/// 取 `PartFixture::*_SHELF_ID / PROCESS_ID` 常量。
fn setup_inspection_and_production_shelves() -> (i64, i64, i64) {
    (
        PartFixture::INSPECTION_SHELF_ID,
        PartFixture::PRODUCTION_SHELF_ID,
        PartFixture::PROCESS_ID,
    )
}

// ===========================================================================
//  Bootstrap helpers（PR13 Phase F 风格 B：抽出公共样板）
// ===========================================================================

/// 起一份 fresh database + 加载 assembly fixture（复用 part 域基线）+ 构造
/// axum Router + 以 INSPECTOR 身份登录。
///
/// 返回 `(pool, app, token, fx)`。`fx` 提供 `part_manager_username` 等强类型
/// 句柄；INSPECTOR 用户走 `PartFixture::INSPECTOR_USERNAME`。
async fn bootstrap_as_inspector() -> (PgPool, axum::Router, String, AssemblyFixture) {
    let pool = test_pool().await;
    let fx = load_assembly_fixture(&pool).await;
    let app = test_app(test_state(pool.clone()).await);
    let token = login_token(&app, PartFixture::INSPECTOR_USERNAME, PartFixture::PASSWORD).await;
    (pool, app, token, fx)
}

// ===========================================================================
//  私有 fixture helpers（assembly 域独有）
// ===========================================================================

/// 插一个 `t_assembly` 行（PENDING 状态）。
///
/// 复刻 `tests/assembly_api.rs::insert_*` 风格但**不**复用 master 的
/// `tests/delivery_scan_api.rs::insert_assembly`（后者写死 status='ACTIVE'，
/// 不适合 rollup 测试）。
async fn insert_assembly(
    pool: &sqlx::PgPool,
    customer_id: i64,
    drawing_no: &str,
    name: &str,
) -> i64 {
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let id = snowflake.next_id();
    let now = now_naive();
    let today = now.date();
    sqlx::query!(
        "INSERT INTO t_assembly (id, drawing_no, name, applicant_name, customer_id, \
         request_date, planned_delivery_date, status, quantity, unit_price, total_price, \
         version, created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, $3, '', $4, $5, $5, 'PENDING', 1, 0, 0, 0, $6, NULL, $6, NULL)",
        id,
        drawing_no,
        name,
        customer_id,
        today,
        now,
    )
    .execute(pool)
    .await
    .expect("insert t_assembly");
    id
}

/// 直接 UPDATE `t_assembly.status`（fixture 场景用）。
///
/// 仿 `tests/assembly_api.rs:469` 的 `UPDATE t_assembly SET status='COMPLETED'`：
/// 跳过 service 层（`update_partial` 不接受 status 字段），用裸 SQL 强制终态。
async fn force_assembly_status(pool: &sqlx::PgPool, asm_id: i64, status: &str) {
    sqlx::query("UPDATE t_assembly SET status = $1 WHERE id = $2")
        .bind(status)
        .bind(asm_id)
        .execute(pool)
        .await
        .expect("force t_assembly status");
}

/// 取当前 `t_assembly` (status, version)。
async fn get_assembly_status_version(pool: &sqlx::PgPool, asm_id: i64) -> (String, i32) {
    let row: (String, i32) = sqlx::query_as("SELECT status, version FROM t_assembly WHERE id = $1")
        .bind(asm_id)
        .fetch_one(pool)
        .await
        .expect("query t_assembly");
    row
}

/// service 直调所需的 `CurrentUser`（绕开 HTTP / JWT / Redis）。
/// INSPECTOR 即可（manager/clerk 也可，但 to_inspection 走 inspector 路径最自然）。
fn test_current_user() -> CurrentUser {
    CurrentUser {
        id: 1,
        username: "asm_sync_tester".into(),
        roles: vec![Role::Inspector],
        shelf_ids: vec![],
        shelf_wildcard: false,
    }
}

// ===========================================================================
//  Tests
// ===========================================================================

/// 1. 单件 PENDING → INSPECTION 触发父 asm PENDING → IN_PROCESS。
///
/// 期望：`data.synced_assembly_id == Some(asm.id)`；DB `t_assembly.status == 'IN_PROCESS'`。
#[tokio::test]
async fn single_part_to_inspection_flips_assembly_to_in_process() {
    let (pool, app, token, _fx) = bootstrap_as_inspector().await;
    let l1 = insert_l1(&pool, "F", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let (insp_shelf, _prod_shelf, _proc) = setup_inspection_and_production_shelves();

    let asm_id = insert_assembly(&pool, l2, "D-A1", "总成A1").await;
    let part_id =
        insert_part_with_status(&pool, "P0", l2, Some("PA1-01"), Some(asm_id), "PENDING").await;
    let batch_id = insert_batch(&pool, part_id, 1, 5, "PENDING").await;
    let v = batch_version(&pool, batch_id).await;

    let (status, body) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{part_id}/to-inspection"),
            Some(json!({
                "target_inspection_shelf_id": insp_shelf.to_string(),
                "batch_id": batch_id.to_string(),
                "version": v,
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body={body}");
    assert_eq!(body["code"], 0, "envelope code 应为 0");
    assert_eq!(body["data"]["part"]["status"], "INSPECTION");
    let synced_aid = body["data"]["synced_assembly_id"]
        .as_str()
        .expect("synced_assembly_id 应为非 null 字符串");
    assert_eq!(
        synced_aid,
        asm_id.to_string(),
        "data.synced_assembly_id 应等于父 asm.id"
    );

    // DB 端断言父 assembly 已翻转到 IN_PROCESS
    let (asm_status, asm_version) = get_assembly_status_version(&pool, asm_id).await;
    assert_eq!(asm_status, "IN_PROCESS", "父 asm 应已翻转到 IN_PROCESS");
    assert_eq!(asm_version, 1, "version 应自增一次（0 → 1）");
}

/// 2. 混合子件：1 子 COMPLETED + 1 子 INSPECTION → 父 = IN_PROCESS。
///
/// 期望：父 asm 起 PENDING，第二个子 INSPECTION 后聚合算法
/// （`compute_assembly_target`）取 `non_terminal_non_cancelled` 的
/// `min(progress)` → INSPECTION 的 progress=4 → `IN_PROCESS`。
#[tokio::test]
async fn mixed_children_assembly_rolls_up_to_min_progress() {
    let (pool, app, token, _fx) = bootstrap_as_inspector().await;
    let l1 = insert_l1(&pool, "F", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let (insp_shelf, _prod_shelf, _proc) = setup_inspection_and_production_shelves();

    let asm_id = insert_assembly(&pool, l2, "D-A2", "总成A2").await;
    // 子 1：COMPLETED（fixture 直插；COMPLETED 是终态但 rollup 不视作 cancelled）
    let p1 =
        insert_part_with_status(&pool, "P1", l2, Some("PA2-01"), Some(asm_id), "COMPLETED").await;
    insert_batch(&pool, p1, 1, 5, "COMPLETED").await;
    // 子 2：PENDING → INSPECTION（驱动 sync）
    let p2 =
        insert_part_with_status(&pool, "P2", l2, Some("PA2-02"), Some(asm_id), "PENDING").await;
    let b2 = insert_batch(&pool, p2, 1, 5, "PENDING").await;
    let v2 = batch_version(&pool, b2).await;

    let (status, body) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{p2}/to-inspection"),
            Some(json!({
                "target_inspection_shelf_id": insp_shelf.to_string(),
                "batch_id": b2.to_string(),
                "version": v2,
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body={body}");
    let synced_aid = body["data"]["synced_assembly_id"]
        .as_str()
        .expect("synced_assembly_id 应为非 null：父 PENDING → IN_PROCESS 真发生了 flip");
    assert_eq!(synced_aid, asm_id.to_string());

    // 子状态应为 [COMPLETED, INSPECTION]；min progress over non-terminal =
    // min(INSPECTION=4) = 4 → IN_PROCESS
    let (asm_status, _asm_version) = get_assembly_status_version(&pool, asm_id).await;
    assert_eq!(
        asm_status, "IN_PROCESS",
        "子 [COMPLETED, INSPECTION] → 父应 = IN_PROCESS（min progress=4 非 0）"
    );
}

/// 3. 全部子件 CANCELLED → 父 = CANCELLED。
///
/// 测试策略：
///   - 直接用 SQL UPDATE 把两件子推到 CANCELLED（绕开 `/parts/{id}/cancel` 端点的
///     role 守卫——cancel 需要 Manager/Clerk，本测试不引入额外 user fixture）。
///   - 再用 service 直调 `AssemblyService::sync_from_part_change` 驱动 rollup，
///     验证算法对 ALL_CANCELLED → CANCELLED 的契约。
///
/// 备注：`/parts/{id}/cancel` 端点**不**触发 `sync_from_part_change`（sync 仅挂在
/// to-inspection / to-ship / to-process / worker-scan 4 个流上）。这是有意的设计
/// 决策（cancel 不属于「业务流转」范畴），与本测试的算法正确性正交。
#[tokio::test]
async fn all_children_cancelled_flips_assembly_to_cancelled() {
    let pool = test_pool().await;
    let _fx = load_assembly_fixture(&pool).await;
    let l1 = insert_l1(&pool, "F", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;

    let asm_id = insert_assembly(&pool, l2, "D-A3", "总成A3").await;
    let p1 =
        insert_part_with_status(&pool, "P1", l2, Some("PA3-01"), Some(asm_id), "PENDING").await;
    insert_batch(&pool, p1, 1, 5, "PENDING").await;
    let p2 =
        insert_part_with_status(&pool, "P2", l2, Some("PA3-02"), Some(asm_id), "PENDING").await;
    insert_batch(&pool, p2, 1, 5, "PENDING").await;

    // 直接 UPDATE 把两件子推 CANCELLED（绕开 HTTP + role 守卫）
    sqlx::query("UPDATE t_part SET status = 'CANCELLED' WHERE id = ANY($1)")
        .bind(&[p1, p2][..])
        .execute(&pool)
        .await
        .expect("force t_part status=CANCELLED");

    // 父 asm 应仍 PENDING（sync 未触发）
    let (asm_status_before, asm_version_before) = get_assembly_status_version(&pool, asm_id).await;
    assert_eq!(asm_status_before, "PENDING");
    assert_eq!(asm_version_before, 0, "无 sync → version 应保持 0");

    // service 直调 sync_from_part_change 驱动 rollup
    let current = test_current_user();
    let mut tx = pool.begin().await.expect("begin tx");
    let synced = AssemblyService::sync_from_part_change(&mut tx, p1, &current)
        .await
        .expect("sync_from_part_change 应 OK");
    tx.commit().await.expect("commit tx");
    assert_eq!(
        synced,
        SyncOutcome::Changed(asm_id),
        "ALL_CANCELLED → 父应从 PENDING 翻转到 CANCELLED，synced = Changed"
    );

    let (asm_status_after, asm_version_after) = get_assembly_status_version(&pool, asm_id).await;
    assert_eq!(asm_status_after, "CANCELLED");
    assert_eq!(asm_version_after, 1, "version 应自增一次（0 → 1）");
}

/// 4. 父已是终态（COMPLETED / CANCELLED）→ 子状态变更**不**修改父；无 version bump。
///
/// 父预置 COMPLETED；PENDING 子 → INSPECTION（触发 sync）→ sync 短路返回 NoChange。
/// 断言：`status` 不变；`version` 不变；响应 `data.synced_assembly_id == null`。
#[tokio::test]
async fn terminal_assembly_is_not_modified_by_child_change() {
    let (pool, app, token, _fx) = bootstrap_as_inspector().await;
    let l1 = insert_l1(&pool, "F", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let (insp_shelf, _prod_shelf, _proc) = setup_inspection_and_production_shelves();

    let asm_id = insert_assembly(&pool, l2, "D-A4", "总成A4").await;
    force_assembly_status(&pool, asm_id, "COMPLETED").await;
    // 用一次额外 UPDATE 自增 version 模拟「自然流转到 COMPLETED 的版本号」，
    // 这样断言「无 version bump」时起点是 version=1 而不是 0
    sqlx::query("UPDATE t_assembly SET version = version + 1 WHERE id = $1")
        .bind(asm_id)
        .execute(&pool)
        .await
        .expect("bump version");
    let (_st, version_before) = get_assembly_status_version(&pool, asm_id).await;
    assert!(version_before >= 1);

    let part_id =
        insert_part_with_status(&pool, "P1", l2, Some("PA4-01"), Some(asm_id), "PENDING").await;
    let batch_id = insert_batch(&pool, part_id, 1, 5, "PENDING").await;
    let v = batch_version(&pool, batch_id).await;

    let (status, body) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{part_id}/to-inspection"),
            Some(json!({
                "target_inspection_shelf_id": insp_shelf.to_string(),
                "batch_id": batch_id.to_string(),
                "version": v,
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body={body}");
    assert_eq!(body["data"]["part"]["status"], "INSPECTION");
    // 父终态短路 → synced_assembly_id = null
    assert!(
        body["data"]["synced_assembly_id"].is_null(),
        "父终态时应短路：synced_assembly_id 应为 null；body={body}"
    );

    let (st, version_after) = get_assembly_status_version(&pool, asm_id).await;
    assert_eq!(
        st, "COMPLETED",
        "父 asm 应保持 COMPLETED，不被子状态变更改写"
    );
    assert_eq!(
        version_after, version_before,
        "父终态时 sync 不应产生 UPDATE → version 必须 0 自增"
    );
}

/// 5. 批量 to-inspection 跨 2 个 assembly；handler 侧 dedup 广播。
///
/// 3 件子：2 件在 asm-A、1 件在 asm-B；全部 PENDING → INSPECTION。
///
/// sync 语义：每个 item 调一次 `sync_from_part_change`。
/// - asm-A 子：[PENDING, PENDING]，item 1 后 [INSPECTION, PENDING]，target=PENDING
///   （asm-A 是 PENDING）→ NoChange。item 2 后 [INSPECTION, INSPECTION]，target=IN_PROCESS
///   ≠ PENDING → Changed(asm-A)。
/// - asm-B 子：[PENDING]，item 3 后 [INSPECTION]，target=IN_PROCESS ≠ PENDING
///   → Changed(asm-B)。
///
/// 期望：
///   - `submitted[0].synced_assembly_id == null`（asm-A 还未到 IN_PROCESS）
///   - `submitted[1].synced_assembly_id == Some(asm-A)`（asm-A 翻转）
///   - `submitted[2].synced_assembly_id == Some(asm-B)`（asm-B 翻转）
///   - 两个 asm 在 DB 端均 = IN_PROCESS
///
/// handler 侧：`let mut seen = HashSet::new(); for item in submitted { if Some(aid) && seen.insert(aid) { broadcast } }`
/// → asm-A / asm-B 各广播一次。本测试**不**验 WS 通道（WS 需独立订阅路径）；
/// handler 侧的 dedup 逻辑通过 service 直调 `sync_from_part_changes` 覆盖（见下）。
#[tokio::test]
async fn batch_to_inspection_emits_per_assembly_update() {
    let (pool, app, token, _fx) = bootstrap_as_inspector().await;
    let l1 = insert_l1(&pool, "F", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let (insp_shelf, _prod_shelf, _proc) = setup_inspection_and_production_shelves();

    let asm_a = insert_assembly(&pool, l2, "D-A5A", "总成A5-A").await;
    let asm_b = insert_assembly(&pool, l2, "D-A5B", "总成A5-B").await;
    let pa1 =
        insert_part_with_status(&pool, "P-A1", l2, Some("PA5-01"), Some(asm_a), "PENDING").await;
    let pa2 =
        insert_part_with_status(&pool, "P-A2", l2, Some("PA5-02"), Some(asm_a), "PENDING").await;
    let pb1 =
        insert_part_with_status(&pool, "P-B1", l2, Some("PB5-01"), Some(asm_b), "PENDING").await;
    let ba1 = insert_batch(&pool, pa1, 1, 5, "PENDING").await;
    let ba2 = insert_batch(&pool, pa2, 1, 5, "PENDING").await;
    let bb1 = insert_batch(&pool, pb1, 1, 5, "PENDING").await;
    let (va1, va2, vb1) = (
        batch_version(&pool, ba1).await,
        batch_version(&pool, ba2).await,
        batch_version(&pool, bb1).await,
    );

    let (status, body) = send(
        app,
        json_request(
            "POST",
            "/parts/batch-to-inspection",
            Some(json!({
                "target_inspection_shelf_id": insp_shelf.to_string(),
                "items": [
                    { "batch_id": ba1.to_string(), "version": va1 },
                    { "batch_id": ba2.to_string(), "version": va2 },
                    { "batch_id": bb1.to_string(), "version": vb1 },
                ],
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body={body}");
    assert_eq!(body["code"], 0, "envelope code 应为 0");
    let submitted = body["data"]["submitted"].as_array().expect("submitted");
    assert_eq!(submitted.len(), 3, "3 件全部成功 → submitted=3");

    // per-item synced_assembly_id 断言：
    //   - item 1 (asm-A 子)：target=PENDING，asm-A=PENDING → NoChange → null
    //   - item 2 (asm-A 子)：target=IN_PROCESS ≠ PENDING → Changed(asm-A)
    //   - item 3 (asm-B 子)：target=IN_PROCESS ≠ PENDING → Changed(asm-B)
    let s0_sid = submitted[0]["synced_assembly_id"].as_str();
    let s1_sid = submitted[1]["synced_assembly_id"].as_str();
    let s2_sid = submitted[2]["synced_assembly_id"].as_str();
    assert!(
        s0_sid.is_none(),
        "item 1 (asm-A) 父还在 PENDING（target=PENDING==current），synced_assembly_id 应为 null"
    );
    assert_eq!(
        s1_sid.expect("item 2 应 Some"),
        asm_a.to_string(),
        "item 2 (asm-A) 父翻到 IN_PROCESS，synced_assembly_id 应 = asm-A"
    );
    assert_eq!(
        s2_sid.expect("item 3 应 Some"),
        asm_b.to_string(),
        "item 3 (asm-B) 父翻到 IN_PROCESS，synced_assembly_id 应 = asm-B"
    );

    // DB 端：两 asm 均已 = IN_PROCESS
    let (st_a, _v_a) = get_assembly_status_version(&pool, asm_a).await;
    let (st_b, _v_b) = get_assembly_status_version(&pool, asm_b).await;
    assert_eq!(st_a, "IN_PROCESS");
    assert_eq!(st_b, "IN_PROCESS");

    // handler 侧 dedup 证据：用 service `sync_from_part_changes` 模拟再次批量驱动；
    // 应返回 `Vec<SyncOutcome>`，且全部 = NoChange（target 已 == current）。
    let current = test_current_user();
    let mut tx = pool.clone().begin().await.expect("begin tx");
    let outcomes = AssemblyService::sync_from_part_changes(&mut tx, &[pa1, pa2, pb1], &current)
        .await
        .expect("sync_from_part_changes 应 OK");
    tx.commit().await.expect("commit tx");
    assert_eq!(
        outcomes.len(),
        2,
        "distinct assembly_ids 去重后 = 2（asm-A + asm-B）"
    );
    assert!(
        outcomes.iter().all(|o| matches!(o, SyncOutcome::NoChange)),
        "再次 sync 时 target 已 = current → 全部 NoChange；got={outcomes:?}"
    );
}
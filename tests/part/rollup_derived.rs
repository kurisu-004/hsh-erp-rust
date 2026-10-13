//! `shared::batch::status::rollup_part_derived`（批次 → 工单派生）集成测试
//!
//! **2026-10-13 新建**，取代已随 admin 域下线的 `tests/part/rollup_recompute.rs`
//! 用例 1（`recompute_rollup_fixes_drifted_part_and_is_idempotent`）。
//!
//! ## 为什么形态变了
//!
//! 被取代的那条用例走的是 `POST /api/v2/admin/recompute-rollup` **端点**（HTTP +
//! 响应信封 + 分表游标 + 终态跳过计数）。admin 域下线后，那条用例守的东西里
//! **只有派生算法本身**还值得留，端点层的部分（游标 / 上报字段 / RBAC）随端点
//! 一起消失、不再有任何生产方。本文件因此**直调函数、不走 HTTP**：
//!
//! - 守的点从「端点报了什么」变成「派生算法算出什么、库端落成什么」；
//! - `rollup_assembly_derived` / `sync_assembly_status` 那一跳的 mock 单测见
//!   `src/shared/batch/status.rs` 的 `sync_assembly_status_tests` 模块，本文件
//!   只补「真库下三层链真的连起来了」。
//!
//! ## 覆盖场景
//!
//! 1. `rollup_fixes_drifted_part_and_is_idempotent`
//!    —— **核心**：注入「真源（`t_part_batch`）已 COMPLETED、派生缓存（`t_part`）
//!    还 PENDING」的漂移 → 修正 → **立刻重跑必须 0 变化**（幂等契约，任何一次
//!    「派生写不幂等」的实现都会在第二次调用时立刻暴露）。
//! 2. `rollup_releases_and_archives_serial_no_when_part_turns_terminal`
//!    —— rollup step 4：part 被派生进终态时 `serial_no` 被释放，且**先**归档一条
//!    `SERIAL_RELEASED` 事件（归档与清空的次序是契约，见 `release_part_serial_no`）。
//! 3. `rollup_cascades_to_parent_assembly`
//!    —— step 3 + step 5：part 状态真变了才会继续派生父装配件，且父件进终态时
//!    序列号被清（父件**不**归档事件 —— `t_assembly` 没有事件表）。
//! 4. `rollup_only_materializing_next_process_id_does_not_touch_assembly`
//!    —— step 3 的 m8 修正：`part_status_changed == false`（只物化
//!    `next_process_id`）时**必须**回 `NoChange`，否则 WS 的 `synced_assembly_id`
//!    里会出现一个 part id 冒充 assembly_id。
//! 5. `rollup_with_no_active_batches_is_nochange`
//!    —— 防御分支：**一条批次都没有** ⇒ `NoChange`，一个字节都不写。
//!    ⚠️「活跃」= **未软删**、与状态无关（见 `list_active_batches_by_part_id` 的 doc），
//!    所以终态批次照样参与 min-progress —— 那是下一条要钉的事实。
//! 6. `rollup_with_only_terminal_batches_follows_min_progress_not_the_guard`
//!    —— 终态批次**参与**派生（DELIVERED 批次把 part 推成 DELIVERED）；反向形态
//!    （part 已终态、派生算出别的值）被**终态守卫**拦下并显式上抛 `TerminalSkip`
//!    —— 那是「派生层不得覆盖主操作」的可观测出口（2026-10-01 review 第 2 轮 MAJOR-1：
//!    只留 `warn!` 的话调用方无法把它与「数据本来就一致」区分开）。
//! 7. `rollup_on_missing_part_is_a_business_error`
//!    —— part 有批次但行已软删 → `BIZ_PART_NOT_FOUND`，不得静默 `NoChange`。
//! 8. `rollup_on_fully_unknown_part_is_nochange_by_the_empty_batch_shortcircuit`
//!    —— 对照项：**从未存在过**的 part id（无批次）走空集短路返 `NoChange` 而非
//!    `Err`。既有行为，钉住它免得被后人当 bug「修」。
//!
//! ## Fixture 范本
//!
//! 共享基线走 `load_part_fixture` / `load_assembly_fixture`（test-support crate），
//! 取号走 `shared_test_snowflake()`。漂移行由本文件底部的本地 `insert_*` helper
//! 直插 —— 与 `tests/part/crud.rs` 等其余 sub-file 同形（把它们统一迁进
//! `test-support` 是独立重构，不在本轮范围）。
//!
//! ⚠️ 本文件**不硬编码任何绝对日期窗口**：所有业务日期都由 `now_naive()` 推导
//! （见 CLAUDE.md「测试数据两条硬约定」第 2 条）。

use sqlx::PgPool;

use hsh_erp_rust::infra::clock::now_naive;
use hsh_erp_rust::modules::assembly::service::SyncOutcome;
use hsh_erp_rust::shared::batch::status::{RollupOutcome, rollup_part_derived};
use hsh_erp_rust::shared::error::code;
use hsh_erp_test_support::{
    load_assembly_fixture, load_part_fixture, shared_test_snowflake, test_pool,
};

/// 取号：走进程内唯一 generator（禁止在测试里现建 `SnowflakeIdGenerator`，
/// 见 CLAUDE.md「测试取号：进程内唯一 generator」一节）。
fn next_test_id() -> i64 {
    shared_test_snowflake().next_id()
}

/// 插一个 `t_part` 行（`status` 由调用方指定 —— 用来造漂移）。
#[allow(clippy::too_many_arguments)]
async fn insert_part(
    pool: &PgPool,
    customer_id: i64,
    drawing_no: &str,
    serial_no: Option<&str>,
    status: &str,
    assembly_id: Option<i64>,
) -> i64 {
    let id = next_test_id();
    let now = now_naive();
    let today = now.date();
    sqlx::query(
        "INSERT INTO t_part (id, serial_no, name, drawing_no, customer_id, status, \
         applicant_name, request_date, planned_delivery_date, quantity, version, \
         assembly_id, created_at, updated_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $3, $7, $7, 1, 0, $8, $9, $9)",
    )
    .bind(id)
    .bind(serial_no)
    .bind(drawing_no)
    .bind(drawing_no)
    .bind(customer_id)
    .bind(status)
    .bind(today)
    .bind(assembly_id)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert part");
    id
}

/// 插一条 `t_part_batch`（`status` 由调用方指定 —— **真源侧**的值）。
async fn insert_batch(pool: &PgPool, part_id: i64, status: &str) -> i64 {
    let id = next_test_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_part_batch (id, part_id, batch_no, quantity, status, version, \
         created_at, updated_at) \
         VALUES ($1, $2, 1, 1, $3, 0, $4, $4)",
    )
    .bind(id)
    .bind(part_id)
    .bind(status)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert batch");
    id
}

/// 插一个 `t_assembly` 行（`status` 由调用方指定 —— 用来造父件漂移）。
async fn insert_assembly(pool: &PgPool, customer_id: i64, status: &str) -> i64 {
    let id = next_test_id();
    let now = now_naive();
    let today = now.date();
    sqlx::query(
        "INSERT INTO t_assembly (id, drawing_no, name, applicant_name, customer_id, \
         request_date, planned_delivery_date, status, quantity, unit_price, total_price, \
         version, created_at, updated_at) \
         VALUES ($1, $2, '总成-派生', '', $3, $4, $4, $5, 1, 0, 0, 0, $6, $6)",
    )
    .bind(id)
    .bind(format!("D-DERIVED-{id}"))
    .bind(customer_id)
    .bind(today)
    .bind(status)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert assembly");
    id
}

async fn part_status_str(pool: &PgPool, part_id: i64) -> String {
    let (st,): (String,) =
        sqlx::query_as::<_, (String,)>("SELECT status FROM t_part WHERE id = $1")
            .bind(part_id)
            .fetch_one(pool)
            .await
            .expect("read part status");
    st
}

async fn part_serial_and_version(pool: &PgPool, part_id: i64) -> (Option<String>, i32) {
    let (serial, version): (Option<String>, i32) = sqlx::query_as(
        "SELECT serial_no, version FROM t_part WHERE id = $1",
    )
    .bind(part_id)
    .fetch_one(pool)
    .await
    .expect("read part serial/version");
    (serial, version)
}

async fn assembly_status_str(pool: &PgPool, assembly_id: i64) -> String {
    let (st,): (String,) =
        sqlx::query_as::<_, (String,)>("SELECT status FROM t_assembly WHERE id = $1")
            .bind(assembly_id)
            .fetch_one(pool)
            .await
            .expect("read assembly status");
    st
}

async fn assembly_serial(pool: &PgPool, assembly_id: i64) -> Option<String> {
    let (st,): (Option<String>,) =
        sqlx::query_as::<_, (Option<String>,)>("SELECT serial_no FROM t_assembly WHERE id = $1")
            .bind(assembly_id)
            .fetch_one(pool)
            .await
            .expect("read assembly serial");
    st
}

/// 在一个**自己的**事务里跑一次 `rollup_part_derived` 并提交
/// （派生链读批次、写 part、写父装配件、归档事件，必须同一事务 —— 这正是
/// 「事务边界在 handler / 派生与写焊死」的约定在直调场景下的样子）。
async fn run_rollup(pool: &PgPool, part_id: i64, updated_by: i64, event_id: Option<i64>) -> RollupOutcome {
    let mut tx = pool.begin().await.expect("begin tx");
    let out = rollup_part_derived(&mut tx, part_id, updated_by, event_id)
        .await
        .expect("rollup_part_derived 应 OK");
    tx.commit().await.expect("commit tx");
    out
}

// =============================================================================
//  1. 核心：漂移修正 + 幂等
// =============================================================================

#[tokio::test]
async fn rollup_fixes_drifted_part_and_is_idempotent() {
    let pool = test_pool().await;
    let fx = load_part_fixture(&pool).await;

    // 漂移形态：**真源**（`t_part_batch`）已 COMPLETED，派生缓存（`t_part`）还 PENDING。
    // 这正是 `status_gate` 收口之前「漏调 sync」留下的历史形态。
    let part_id = insert_part(
        &pool,
        fx.customer_l2_id,
        "漂移件-派生",
        Some("RD-0001"),
        "PENDING",
        None,
    )
    .await;
    insert_batch(&pool, part_id, "COMPLETED").await;
    assert_eq!(part_status_str(&pool, part_id).await, "PENDING", "前置：确实漂移");

    // ---- 第一次：修正 ----
    // 目标终态 ⇒ 必须传真实雪花 `event_id`（归档事件的 id；见 `StatusChange::event_id`
    // 的 rationale —— 拿 part_id 之类顶替会与「每个 part 最多 1 条 SERIAL_RELEASED」
    // 冲突并在二次进终态时 pkey 冲突）。
    let out = run_rollup(
        &pool,
        part_id,
        fx.manager_user_id,
        Some(next_test_id()),
    )
    .await;
    assert!(
        out.part_status_changed,
        "批次 COMPLETED / part PENDING ⇒ 本次必须真的改了 part.status；out={out:?}"
    );
    assert_eq!(part_status_str(&pool, part_id).await, "COMPLETED");
    assert!(
        out.terminal_skip.is_none(),
        "本次是正常修正，不是被终态守卫拦下；out={out:?}"
    );

    // ---- 第二次：**幂等** ----
    let out2 = run_rollup(&pool, part_id, fx.manager_user_id, Some(next_test_id())).await;
    assert!(
        !out2.part_status_changed && out2.sync == SyncOutcome::NoChange && out2.terminal_skip.is_none(),
        "派生已收敛 ⇒ 重跑必须 0 变化（幂等契约）；out={out2:?}"
    );
    assert_eq!(part_status_str(&pool, part_id).await, "COMPLETED");

    // version 只在第一次涨（第二次 target == current 直接短路，连 UPDATE 都不发）
    let (_, v_after_1) = part_serial_and_version(&pool, part_id).await;
    run_rollup(&pool, part_id, fx.manager_user_id, Some(next_test_id())).await;
    let (_, v_after_2) = part_serial_and_version(&pool, part_id).await;
    assert_eq!(
        v_after_1, v_after_2,
        "幂等重跑不得再 bump version（v_after_1={v_after_1} v_after_2={v_after_2}）"
    );
}

// =============================================================================
//  2. rollup step 4：终态序列号释放 + 归档
// =============================================================================

#[tokio::test]
async fn rollup_releases_and_archives_serial_no_when_part_turns_terminal() {
    let pool = test_pool().await;
    let fx = load_part_fixture(&pool).await;

    let part_id = insert_part(
        &pool,
        fx.customer_l2_id,
        "序列号释放-派生",
        Some("RD-0002"),
        "PENDING",
        None,
    )
    .await;
    insert_batch(&pool, part_id, "COMPLETED").await;
    let (serial_before, _) = part_serial_and_version(&pool, part_id).await;
    assert_eq!(serial_before.as_deref(), Some("RD-0002"), "前置：序列号在占用");

    let out = run_rollup(
        &pool,
        part_id,
        fx.manager_user_id,
        Some(next_test_id()),
    )
    .await;
    assert!(out.part_status_changed);

    // 清空
    let (serial_after, _) = part_serial_and_version(&pool, part_id).await;
    assert_eq!(serial_after, None, "part 进 COMPLETED 后序列号应被释放");

    // 归档（每个 part 最多 1 条 —— 这是「归档事件 id 必须是真实雪花」那条纪律的
    // 可观测后果；传 part_id 顶替时第二次进终态就会 pkey 冲突）
    let (n,): (i64,) = sqlx::query_as::<_, (i64,)>(
        "SELECT count(*) FROM t_part_event WHERE part_id = $1 AND event_type = 'SERIAL_RELEASED'",
    )
    .bind(part_id)
    .fetch_one(&pool)
    .await
    .expect("count serial released event");
    assert_eq!(n, 1, "序列号释放应归档恰好一条事件");
}

// =============================================================================
//  3. step 3 + step 5：级联父装配件
// =============================================================================

#[tokio::test]
async fn rollup_cascades_to_parent_assembly() {
    let pool = test_pool().await;
    let fx = load_assembly_fixture(&pool).await;

    let asm_id = insert_assembly(&pool, fx.part_customer_l1_id, "PENDING").await;
    // 给父件占一个序列号，好观察 step 5 的「父件进终态直接清、不归档」
    sqlx::query("UPDATE t_assembly SET serial_no = $2 WHERE id = $1")
        .bind(asm_id)
        .bind("RD-A-0001")
        .execute(&pool)
        .await
        .expect("set assembly serial");

    let part_id = insert_part(
        &pool,
        fx.part_customer_l1_id,
        "子件-派生",
        Some("RD-0003"),
        "PENDING",
        Some(asm_id),
    )
    .await;
    insert_batch(&pool, part_id, "COMPLETED").await;

    let out = run_rollup(
        &pool,
        part_id,
        fx.part_manager_user_id,
        Some(next_test_id()),
    )
    .await;
    assert!(out.part_status_changed, "子件应被派生进 COMPLETED");
    assert_eq!(
        out.sync,
        SyncOutcome::Changed(asm_id),
        "part.status 真变了 ⇒ 必须继续派生父装配件（step 3）；out={out:?}"
    );
    assert_eq!(
        assembly_status_str(&pool, asm_id).await,
        "COMPLETED",
        "唯一子件 COMPLETED ⇒ 父装配件跟随"
    );
    assert_eq!(
        assembly_serial(&pool, asm_id).await,
        None,
        "step 5：父件进终态 ⇒ 序列号直接清"
    );
    // 父件**不**归档事件（`t_assembly` 没有事件表，`note` 是用户可编辑的业务备注）
    let (n,): (i64,) = sqlx::query_as::<_, (i64,)>(
        "SELECT count(*) FROM t_part_event WHERE event_type = 'SERIAL_RELEASED' \
           AND part_id = ANY(SELECT id FROM t_part WHERE assembly_id = $1)",
    )
    .bind(asm_id)
    .fetch_one(&pool)
    .await
    .expect("count child serial released events");
    assert_eq!(n, 1, "只有子件那一条；父件不得被当成分享号持有者去归档");
}


// =============================================================================
//  4. step 3 的 m8 修正：只物化 next_process_id 时不得碰父件
// =============================================================================

#[tokio::test]
async fn rollup_only_materializing_next_process_id_does_not_touch_assembly() {
    let pool = test_pool().await;
    let fx = load_part_fixture(&pool).await;

    // part.status 已经是 INSPECTION（与批次自洽），但 `next_process_id` 还是 NULL
    // ⇒ 派生只物化派生列，**status 没变** ⇒ step 3 必须回 `NoChange`。
    let part_id = insert_part(
        &pool,
        fx.customer_l2_id,
        "只物化派生列",
        None,
        "INSPECTION",
        None,
    )
    .await;
    insert_batch(&pool, part_id, "INSPECTION").await;
    sqlx::query("UPDATE t_part SET next_process_id = NULL WHERE id = $1")
        .bind(part_id)
        .execute(&pool)
        .await
        .expect("clear next_process_id");
    assert_eq!(
        part_status_str(&pool, part_id).await,
        "INSPECTION",
        "前置：status 与批次自洽"
    );

    let out = run_rollup(&pool, part_id, fx.manager_user_id, None).await;
    assert_eq!(
        out.sync,
        SyncOutcome::NoChange,
        "只物化 next_process_id 时必须回 NoChange —— 旧实现回 Changed(part_id)，\
         会被 caller 当 assembly_id 塞进响应并广播出假 ASSEMBLY_UPDATED（m8）"
    );
    assert!(
        !out.part_status_changed,
        "status 本来就没变；out={out:?}"
    );
}

// =============================================================================
//  5 + 6. 防御分支
// =============================================================================

#[tokio::test]
async fn rollup_with_no_active_batches_is_nochange() {
    let pool = test_pool().await;
    let fx = load_part_fixture(&pool).await;

    // ⚠️ 「活跃」在 `list_active_batches_by_part_id` 里的含义是「**未软删**」，
    // 与状态无关（终态批次照样参与 min-progress）。所以要命中空集分支，
    // 得是**一条批次都没有**（或全部软删）—— 这正是 PR-B1 说的「防御」：
    // 正常写路径保证每个 part 至少有 1 条活跃批次。
    let part_id = insert_part(
        &pool,
        fx.customer_l2_id,
        "无活跃批次",
        None,
        "PENDING",
        None,
    )
    .await;

    let out = run_rollup(&pool, part_id, fx.manager_user_id, None).await;
    assert!(
        !out.part_status_changed && out.sync == SyncOutcome::NoChange && out.terminal_skip.is_none(),
        "空批次集 → NoChange；out={out:?}"
    );
    assert_eq!(
        part_status_str(&pool, part_id).await,
        "PENDING",
        "一个字节都不该写（空集分支连 UPDATE 都不发）"
    );
}

#[tokio::test]
async fn rollup_with_only_terminal_batches_follows_min_progress_not_the_guard() {
    let pool = test_pool().await;
    let fx = load_part_fixture(&pool).await;

    // 只有 DELIVERED 批次、part 还 PENDING ⇒ 派生值 DELIVERED（终态批次**参与**
    // min-progress，这是「取不到活跃行就当无事发生」与「派生值应是 DELIVERED」
    // 两种读法里唯一正确的后者 —— 见 `list_active_batches_by_part_id` 的 doc）。
    let part_id = insert_part(
        &pool,
        fx.customer_l2_id,
        "终态批次参与派生",
        Some("RD-0006"),
        "PENDING",
        None,
    )
    .await;
    insert_batch(&pool, part_id, "DELIVERED").await;

    let out = run_rollup(&pool, part_id, fx.manager_user_id, Some(next_test_id())).await;
    assert!(
        out.part_status_changed,
        "DELIVERED 批次应把 part 推到 DELIVERED；out={out:?}"
    );
    assert_eq!(part_status_str(&pool, part_id).await, "DELIVERED");
    assert!(
        out.terminal_skip.is_none(),
        "本次是正常修正，不是被终态守卫拦下"
    );

    // 反过来：part 已被主操作打成终态、而派生算出别的值 ⇒ 终态守卫拦下并
    // **上抛诊断信息**（2026-10-01 review 第 2 轮 MAJOR-1：只留 warn 的话，
    // 调用方无法把它与「数据本来就一致」区分开）。
    let guard_part = insert_part(
        &pool,
        fx.customer_l2_id,
        "终态但错",
        Some("RD-0007"),
        "COMPLETED",
        None,
    )
    .await;
    insert_batch(&pool, guard_part, "INSPECTION").await;
    let out2 = run_rollup(&pool, guard_part, fx.manager_user_id, None).await;
    assert!(
        !out2.part_status_changed && out2.sync == SyncOutcome::NoChange,
        "终态 part 不得被派生覆盖；out={out2:?}"
    );
    let skip = out2.terminal_skip.as_ref().unwrap_or_else(|| {
        panic!("被终态守卫拦下必须显式上抛 TerminalSkip（只留 warn! 的话调用方分不清「被拦下」与「数据本来就一致」）；out={out2:?}")
    });
    assert_eq!(skip.current, "COMPLETED", "current = 库里那个被保住的终态");
    assert_eq!(skip.derived, "INSPECTION", "derived = 算出来但没写进去的值");
    assert_eq!(
        part_status_str(&pool, guard_part).await,
        "COMPLETED",
        "守卫仍生效：part 的终态一个字节都没动"
    );
}

#[tokio::test]
async fn rollup_on_missing_part_is_a_business_error() {
    let pool = test_pool().await;
    let fx = load_part_fixture(&pool).await;

    // 「part 不存在」这条 `BIZ_PART_NOT_FOUND` **只在 part 有批次时可达** ——
    // 空批次集会先在 `compute_part_target` 处短路成 NoChange，根本读不到 part 行。
    // 故这里造「part 已被软删、批次还在」：批次的 `part_id` 无物理外键（DB 约定），
    // 这正是它能存在的形态。
    let part_id = insert_part(
        &pool,
        fx.customer_l2_id,
        "已软删",
        None,
        "PENDING",
        None,
    )
    .await;
    insert_batch(&pool, part_id, "INSPECTION").await;
    sqlx::query("UPDATE t_part SET deleted_at = now() WHERE id = $1")
        .bind(part_id)
        .execute(&pool)
        .await
        .expect("soft delete part");

    let mut tx = pool.begin().await.expect("begin tx");
    let err = rollup_part_derived(&mut tx, part_id, fx.manager_user_id, None)
        .await
        .expect_err("part 不可见（有批次但行已软删）必须 Err");
    tx.rollback().await.ok();
    assert_eq!(err.code(), code::BIZ_PART_NOT_FOUND, "err={err:?}");
}

#[tokio::test]
async fn rollup_on_fully_unknown_part_is_nochange_by_the_empty_batch_shortcircuit() {
    let pool = test_pool().await;
    let _fx = load_part_fixture(&pool).await;

    // 对照上一条：**从未存在过**的 part id（没有批次）走的是空集短路，
    // 返回 NoChange 而不是 Err —— 这是既有行为、本用例把它钉住，
    // 免得后人把「静默 NoChange」误当成 bug 去「修」。
    let mut tx = pool.begin().await.expect("begin tx");
    let out = rollup_part_derived(&mut tx, 9_999_999_999, 1, None)
        .await
        .expect("空批次集分支不得 Err");
    tx.commit().await.ok();
    assert!(
        !out.part_status_changed && out.sync == SyncOutcome::NoChange,
        "空批次集 ⇒ NoChange；out={out:?}"
    );
}
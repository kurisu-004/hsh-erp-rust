//! prod::scan 报工台主入口（worker-scan）集成测试
//!
//! 2026-10-10 自 `tests/production/queue.rs` 搬来：`POST /prod/scan/worker-scan`
//! 随端点迁往 `POST /prod/scan/worker-scan`（硬切无 alias），测试归属随之按
//! **被测端点**而不是「它顺手触发了 refill」搬家 —— refill 那一半仍由
//! `queue.rs` 守（`refill_*` / `WORKER_POOL_*` 事件）。
//!
//! 本文件搬的是 11 个用例：
//! - `worker_scan_inspected_triggers_refill` — INSPECTED → 自动 refill
//! - `worker_scan_returned_triggers_refill` — RETURNED → 自动 refill
//! - `worker_scan_returned_advances_current_process_id` — RETURNED 推进
//!   `current_process_id`（批次落进**下一道**工序池而非原池）
//! - `worker_scan_returned_advances_step_pointer_when_process_chain_is_consistent`
//!   — 顺应工序时 step 指针成对推进
//! - `worker_scan_returned_without_process_chain_succeeds` — part **无工艺链**
//!   （`t_part.process_chain_id` 可空）时 RETURNED 仍须成功
//! - `worker_scan_returned_requires_next_process_id_when_not_consistent` —
//!   非顺应工序时 `next_process_id` 必填，缺失 → 40001
//! - `worker_scan_shelf_scope_violation_403` — 越权 shelf → 40301
//! - `refill_failure_rolls_back_worker_scan` [`#[ignore]`：DB 故障注入缺基建]
//! - `worker_scan_returned_picks_least_loaded_shelf`（2026-10-10）— 自动选架
//! - `worker_scan_returned_at_chain_tail_auto_sends_to_inspection`（2026-10-10）—
//!   链尾自动送检（发 RETURNED 收 `WORKER_SCAN_INSPECTED`）
//! - `refill_takes_across_all_shelves_without_shelf_anchor`（2026-10-10）—
//!   refill 跨全部映射架取料、候选池不限架
//!
//! 2026-10-11 追加（部分数量 `WorkerScanRequest::quantity`，6 个用例）：
//! - `worker_scan_returned_without_quantity_keeps_whole_batch` —— 缺省 = 整批的
//!   回归基线
//! - `worker_scan_returned_quantity_equal_batch_quantity_is_whole_batch` ——
//!   `==` 是「显式整批」，不拆批
//! - `worker_scan_returned_partial_splits_batch_and_keeps_remainder_on_worker` ——
//!   RETURNED 部分放回
//! - `worker_scan_inspected_partial_splits_batch_and_keeps_remainder_on_worker` ——
//!   INSPECTED 部分送检
//! - `worker_scan_returned_at_chain_tail_with_partial_quantity_sends_only_split_part`
//!   —— TAIL early-return 分支下的部分放回（最容易漏的那条）
//! - `worker_scan_invalid_quantity_returns_20111` —— `<= 0` / `>` 两条边界
//!
//! ## 复用的 fixture
//! 全部从 `super::queue` 借（那边是 worker_pool 场景的 fixture 大本营）：本文件
//! 不复制一份，避免同款 fixture 在两处漂移。

use axum::http::StatusCode;
use serde_json::{Value, json};
use sqlx::PgPool;

use hsh_erp_test_support::{json_request, login_token, send};

use super::queue::{
    append_chain_step, bootstrap_as_manager, count_held_by_worker, insert_customer_l2,
    insert_pending_part_with_chain, insert_pool_part, insert_shelf, insert_work_type,
    insert_worker, insert_worker_held_part, link_shelf_to_process, link_work_type_to_process,
    login_manager_with_username, login_shelf_account, part_chain_id, seed_process,
};

/// 场景 1: worker-scan INSPECTED → 自动 refill
#[tokio::test]
async fn worker_scan_inspected_triggers_refill() {
    let (pool, _app, _token, _fx) = bootstrap_as_manager().await;
    let customer = insert_customer_l2(&pool, "POOL").await;
    let proc = seed_process(&pool, "PROC-A", "工序A").await;
    let wt = insert_work_type(&pool, "WT-A", "工种A", Some(5)).await;
    link_work_type_to_process(&pool, wt, proc).await;
    let prod_shelf = insert_shelf(&pool, "PROD-A", "PROD-A", "PRODUCTION").await;
    let insp_shelf = insert_shelf(&pool, "INSP-A", "INSP-A", "INSPECTION").await;
    link_shelf_to_process(&pool, prod_shelf, proc).await;

    let worker = insert_worker(&pool, "BC001", "工1", Some(wt)).await;
    // worker 当前持 1 件；池里 1 件待 refill
    let (held_part, _held_batch, _step) =
        insert_worker_held_part(&pool, customer, "H-001", worker, proc, 1, true).await;
    let (_pool_part, _pool_batch) =
        insert_pool_part(&pool, customer, "P-001", prod_shelf, proc, 1).await;

    let (app, token, pool) =
        login_shelf_account(pool.clone(), "user1", &[prod_shelf, insp_shelf]).await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/prod/scan/worker-scan",
            Some(json!({
                "serial_no": "H-001",
                "badge_code": "BC001",
                "event_type": "INSPECTED",
                "shelf_id": prod_shelf.to_string(),
                "target_inspection_shelf_id": insp_shelf.to_string(),
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "scan INSPECTED: {env}");
    assert_eq!(env["code"], 0);
    // scan 出参：worker_id / part_id / event_type 都在 scan 字段里
    assert_eq!(env["data"]["scan"]["worker_id"], worker.to_string());
    assert_eq!(env["data"]["scan"]["part_id"], held_part.to_string());
    assert_eq!(env["data"]["scan"]["event_type"], "WORKER_SCAN_INSPECTED");
    // refill 出参：从池里抢到 1 件 + pool_empty=false
    let taken = env["data"]["refill"]["taken"]
        .as_array()
        .expect("refill.taken");
    assert_eq!(taken.len(), 1, "refill 应抢到 1 件: {env}");
    assert_eq!(env["data"]["refill"]["pool_empty"], false);

    // 验证 worker 持有数 = 1（放回 1 + 抢到 1）
    let held = count_held_by_worker(&pool, worker).await;
    assert_eq!(held, 1, "worker 应持有 1 件（refill 后）");
}

/// 场景 2: worker-scan RETURNED → 自动 refill
#[tokio::test]
async fn worker_scan_returned_triggers_refill() {
    let (pool, _app, _token, _fx) = bootstrap_as_manager().await;
    let customer = insert_customer_l2(&pool, "POOL2").await;
    let proc = seed_process(&pool, "PROC-B", "工序B").await;
    let wt = insert_work_type(&pool, "WT-B", "工种B", Some(5)).await;
    link_work_type_to_process(&pool, wt, proc).await;
    let prod_shelf = insert_shelf(&pool, "PROD-B", "PROD-B", "PRODUCTION").await;
    link_shelf_to_process(&pool, prod_shelf, proc).await;

    let worker = insert_worker(&pool, "BC002", "工2", Some(wt)).await;
    let (_held_part, held_batch, _step) =
        insert_worker_held_part(&pool, customer, "H-002", worker, proc, 1, true).await;
    // 2026-10-10：清掉 step 指针，把批次压到「非顺应 ⇒ 用请求里的 `next_process_id`」
    // 那条分支。
    //
    // 为什么要清：fixture 造的是**单 step 链**，指针一致时 `chain_state == "TAIL"`
    // ⇒ RETURNED 会被「链尾自动送检」接管（那正是本用例**不**想测的路径 —— 本用例测
    // 「放回生产架 → refill」，链尾自动送检由
    // `worker_scan_returned_at_chain_tail_auto_sends_to_inspection` 专门覆盖）。
    // 清指针让 `is_pointer_consistent = false`，既绕开 TAIL 又落到显式分支。
    sqlx::query("UPDATE t_part_batch SET current_process_step_id = NULL WHERE id = $1")
        .bind(held_batch)
        .execute(&pool)
        .await
        .expect("clear step pointer");
    let (_pool_part, _pool_batch) =
        insert_pool_part(&pool, customer, "P-002", prod_shelf, proc, 1).await;

    let (app, token, _pool) = login_shelf_account(pool.clone(), "user2", &[prod_shelf]).await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/prod/scan/worker-scan",
            Some(json!({
                "serial_no": "H-002",
                "badge_code": "BC002",
                "event_type": "RETURNED",
                "shelf_id": prod_shelf.to_string(),
                "next_process_id": proc.to_string(),
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "scan RETURNED: {env}");
    assert_eq!(env["code"], 0);
    assert_eq!(env["data"]["scan"]["event_type"], "WORKER_SCAN_RETURNED");
    let taken = env["data"]["refill"]["taken"]
        .as_array()
        .expect("refill.taken");
    // REFILL 抢满 max=5；池里放回 1 件（H-002）+ 原 1 件 → refill 抢 2 件
    assert_eq!(
        taken.len(),
        2,
        "refill 应抢到 2 件（returned 1 + pool 1）: {env}"
    );
}

/// 场景 2b（2026-09-30 回归测试）: worker-scan RETURNED 推进工序
/// → 批次落进**下一道**工序的候选池，而不是落回原工序池。
///
/// 背景：`mark_batch_returned` 此前既不写 `current_process_id` 也不写
/// `current_process_step_id`，而 RETURNED 是全仓唯一的**工序推进**路径 ——
/// 工人在 PROC-B 完工、扫 RETURNED 传 `next_process_id=PROC-C`，批次归还货架后
/// 仍带 `current_process_id=PROC-B` → 落回 **PROC-B** 池。这正是 migration 004
/// 确立的「唯一权威依据」在主干流程上说谎。
///
/// 2026-10-09：fixture 补一道链 step（`append_chain_step`）。RETURNED 的目标工序
/// 现在必须在链内 —— 链内找不到就以 `20702` 拒收（`optional_step_id` 的「真数据
/// 错误」判定），而本用例要验的是「推进 `current_process_id`」，前提就是目标工序
/// 在链内。断言本身逐字未动。
///
/// ⚠️ **本用例覆盖的是「非顺应 + 显式指定链内工序」这条分支**（2026-10-09 review
/// 第 1 轮订正）：补了 `append_chain_step(proc_c, 20)` 之后，批次形态是「指针指向
/// `proc_b` 的 step ∧ `current_process_id = proc_b`」= 顺应，且链上下一道恰好就是
/// `proc_c` —— 于是自动推进分支会成立，请求里的 `next_process_id` **被完全忽略**
/// （推进到 `proc_c` 只是因为链上下一道恰好是它，不是本用例传的值）。那会让
/// 「显式分支在集成层的唯一覆盖」消失，而断言逐字未动、照样全绿。
/// 修法：把批次指针置 `NULL` 造成非顺应（`is_pointer_consistent = false`），让
/// `next_process_id` 真正参与决策、step 由 `optional_step_id` 从链内解析。
/// 「顺应 + 自动推进」分支由场景 2e 的
/// `worker_scan_returned_advances_step_pointer_when_process_chain_is_consistent`
/// 覆盖（它刻意**不传** `next_process_id`），两条分支各有归属。
///
/// 本测试直接打用户报告的那个症状面：扫完后分别查 PROC-B / PROC-C 两个池，
/// 断言批次只在 PROC-C 池里。
#[tokio::test]
async fn worker_scan_returned_advances_current_process_id() {
    let (pool, _app, _token, _fx) = bootstrap_as_manager().await;
    let customer = insert_customer_l2(&pool, "POOL2B").await;
    // 工序 B（起点）→ 工序 C（RETURNED 传的目标）
    let proc_b = seed_process(&pool, "PROC-B2", "工序B2").await;
    let proc_c = seed_process(&pool, "PROC-C2", "工序C2").await;
    let wt = insert_work_type(&pool, "WT-B2", "工种B2", Some(5)).await;
    // 工种**只**映射 proc_b（起点工序）。
    //
    // 关键：RETURNED 成功后同事务会调 `refill_for_worker`，而 refill 按
    // 「工种可加工工序池」抢批。若工种也映射 proc_c，refill 会立刻把刚归还的
    // 批次再抢回工人（location=WORKER）→ 断言「批次在 proc_c 池」必然失败。
    // 工种不含 proc_c → refill 抢不动它，批次留在 proc_c 候选池里可被端点查到。
    // RETURNED 本身不校验工种资格（只校验 `t_shelf_process` 货架映射，见下）。
    link_work_type_to_process(&pool, wt, proc_b).await;
    let prod_shelf = insert_shelf(&pool, "PROD-B2", "PROD-B2", "PRODUCTION").await;
    // 同一货架同时映射 B / C —— RETURNED 的 t_shelf_process 校验要求 shelf 映射 next_process_id
    link_shelf_to_process(&pool, prod_shelf, proc_b).await;
    link_shelf_to_process(&pool, prod_shelf, proc_c).await;

    let worker = insert_worker(&pool, "BC002B", "工2B", Some(wt)).await;
    // 工人持有 1 件 IN_PROCESS+WORKER 批次，current_process_id = proc_b（起点工序）
    let (held_part, held_batch, _step) =
        insert_worker_held_part(&pool, customer, "H-002B", worker, proc_b, 1, true).await;
    // 2026-10-09：把 RETURNED 的目标工序 proc_c 补进链里（fixture 只建一道 step）。
    // 新不变式下「链是有的，却没把目标工序登记进链内」是**真数据错误**（`20702`
    // `BIZ_PROCESS_CHAIN_STEP_NOT_FOUND`），而本用例要验的是「RETURNED 推进
    // current_process_id」，前提必须是目标工序在链内。
    let chain_id = part_chain_id(&pool, held_part)
        .await
        .expect("fixture 应给该 part 绑了链");
    let proc_c_step = append_chain_step(&pool, chain_id, proc_c, 20).await;
    // ⚠️ 把批次指针置 NULL ⇒ 非顺应（`is_pointer_consistent = false`）。不做这一步，
    // 本用例会退化到自动推进分支、请求里的 `next_process_id` 被忽略（见 fn doc）。
    sqlx::query("UPDATE t_part_batch SET current_process_step_id = NULL WHERE id = $1")
        .bind(held_batch)
        .execute(&pool)
        .await
        .expect("clear batch step pointer to force explicit branch");

    let (app, token, _pool) = login_shelf_account(pool.clone(), "user2b", &[prod_shelf]).await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/prod/scan/worker-scan",
            Some(json!({
                "serial_no": "H-002B",
                "badge_code": "BC002B",
                "event_type": "RETURNED",
                "shelf_id": prod_shelf.to_string(),
                "next_process_id": proc_c.to_string(),
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "scan RETURNED: {env}");
    assert_eq!(env["code"], 0);

    // DB 层：权威列必须被推进到目标工序
    let process_after: Option<i64> =
        sqlx::query_scalar("SELECT current_process_id FROM t_part_batch WHERE id = $1")
            .bind(held_batch)
            .fetch_one(&pool)
            .await
            .expect("query current_process_id after RETURNED");
    assert_eq!(
        process_after,
        Some(proc_c),
        "RETURNED 应把 current_process_id 推进到 next_process_id（{proc_c}），\
         实际 {process_after:?} —— 不推进会让批次落回原工序池"
    );

    // step 指针必须由**显式分支**的 `optional_step_id` 解析成链内那道 proc_c 的
    // step（自动推进分支取的是链上下一道 step，值相同但来源不同 —— 本用例钉的是
    // 显式分支，所以顺带钉住「step 由请求里那道工序在链内解析出来」）。
    let step_after: Option<i64> =
        sqlx::query_scalar("SELECT current_process_step_id FROM t_part_batch WHERE id = $1")
            .bind(held_batch)
            .fetch_one(&pool)
            .await
            .expect("query current_process_step_id after RETURNED");
    assert_eq!(
        step_after,
        Some(proc_c_step),
        "显式分支应由 optional_step_id 把 step 解析成 proc_c 在链内那道（{proc_c_step}），\
         实际 {step_after:?}"
    );

    // 端点层：批次只应出现在 PROC-C 池，不应再出现在 PROC-B 池
    // （login_manager_with_username 会 INSERT t_user，只能调一次，后续复用 token）
    let (app, mgr) = login_manager_with_username(&pool, "admin_pool2b").await;
    let (sb, eb) = send(
        app.clone(),
        json_request(
            "GET",
            &format!("/prod/queue/processes/{proc_b}"),
            None,
            Some(&mgr),
        ),
    )
    .await;
    assert_eq!(sb, StatusCode::OK, "GET pool/{proc_b}: {eb}");
    let in_b = eb["data"]["items"]
        .as_array()
        .expect("items")
        .iter()
        .any(|it| it["batch_id"] == json!(held_batch.to_string()));
    assert!(
        !in_b,
        "RETURNED 推进到 {proc_c} 后，批次不应再出现在原工序 {proc_b} 池: {eb}"
    );

    let (sc, ec) = send(
        app,
        json_request(
            "GET",
            &format!("/prod/queue/processes/{proc_c}"),
            None,
            Some(&mgr),
        ),
    )
    .await;
    assert_eq!(sc, StatusCode::OK, "GET pool/{proc_c}: {ec}");
    let in_c = ec["data"]["items"]
        .as_array()
        .expect("items")
        .iter()
        .any(|it| it["batch_id"] == json!(held_batch.to_string()));
    assert!(
        in_c,
        "RETURNED 推进后批次应出现在目标工序 {proc_c} 池: {ec}"
    );
}

/// 场景 2d（2026-10-04 回归）: worker-scan RETURNED 在 part **无工艺链**时也必须成功。
///
/// ## 这条测试补的是哪个洞
/// `t_part.process_chain_id` 是可空列（baseline 列 COMMENT：「NULL = 未制定工艺链」），
/// `worker_scan.rs` 必须按 `Option` 收它 —— 按 `i64` 解码会让工人归还手写工单（无链，
/// 工单域的常态）时必撞
/// `error occurred while decoding column 0: unexpected null; try decoding as an Option`
/// 整笔 500。本测试走 fixture 的 `with_chain=false` 分支（`t_part.process_chain_id`
/// 保持 NULL）覆盖这条主路径。
///
/// ## 断言
/// 1. 前置：`t_part.process_chain_id IS NULL`（防 fixture 未来被改成有链而假绿）
/// 2. `POST /prod/scan/worker-scan`（RETURNED）→ HTTP 200 + `code=0`
/// 3. `current_process_id` 推进到 `next_process_id`（RETURNED 的主状态变更）
/// 4. `current_process_step_id` 保留扫描前的值（else 分支解出的 step 是 `None`，
///    SQL 的 `COALESCE($5, current_process_step_id)` 必须保住原指针）
///
/// ## 2026-10-10：为什么本用例要造「指针漂移」而不是「清空指针」
/// 锚链解析会**回退**到批次的 step 指针所属链（`COALESCE(p.process_chain_id,
/// cur.chain_id)`），所以「part 无链 + 指针指向本链的 step」这条形态**仍然能解析出链**；
/// 而 fixture 造的是单 step 链 ⇒ 指针一致时 `chain_state == "TAIL"` ⇒ RETURNED 会被
/// 「链尾自动送检」接管。
///
/// 所以要落 else 分支（非顺应 ⇒ 用请求里的 `next_process_id`），必须让
/// `is_pointer_consistent = false`。**不能靠把指针清成 NULL**：那样断言 4 恒成立
/// （本来就是 NULL），`COALESCE` 保留语义就失去覆盖。改把指针指向**另一条链**上挂
/// 了**别的工序**的 step —— 锚链回退到那条链、按 `pb.current_process_id` 重定位落空、
/// `is_pointer_consistent` 为 false；`chain_state` 落 `NONE`（`cur2` 是内连接 LATERAL，
/// 0 行会丢掉整个 joined 行，取不到 TAIL 那条臂），链尾自动送检要求
/// `is_pointer_consistent && TAIL`，两个条件都不满足，于是落 else 分支并解出
/// `step = None`。
///
/// ## 断言 4 是承重的，不是护栏
/// `mark_batch_returned` 的 SQL 写的是
/// `current_process_step_id = COALESCE($5::bigint, current_process_step_id)`。把它改成
/// `= $5` 时本断言会红，而后果是每一次非顺应 RETURNED 都把批次链位置静默清空 ——
/// 所以断言 4 必须留着一个**非 NULL 的原值**可保。
#[tokio::test]
async fn worker_scan_returned_without_process_chain_succeeds() {
    let (pool, _app, _token, _fx) = bootstrap_as_manager().await;
    let customer = insert_customer_l2(&pool, "POOL2D").await;
    // 起点工序（工种可加工）→ RETURNED 传的目标工序（工种**不含**，否则同事务的
    // refill 会把刚归还的批次又抢回工人，干扰断言）
    let proc_b = seed_process(&pool, "PROC-B3", "工序B3").await;
    let proc_c = seed_process(&pool, "PROC-C3", "工序C3").await;
    let wt = insert_work_type(&pool, "WT-B3", "工种B3", Some(5)).await;
    link_work_type_to_process(&pool, wt, proc_b).await;
    let prod_shelf = insert_shelf(&pool, "PROD-B3", "PROD-B3", "PRODUCTION").await;
    // RETURNED 的目标架由选架按 `next_process_id`（= proc_c）挑 ⇒ 候选集要求存在
    // 「映射了 proc_c」的活跃 PRODUCTION 架，缺它则 20508（20507 那条事后校验已被选架覆盖）
    link_shelf_to_process(&pool, prod_shelf, proc_c).await;

    let worker = insert_worker(&pool, "BC002D", "工2D", Some(wt)).await;
    // with_chain = false ⇒ t_part.process_chain_id 为 NULL；批次仍带一个**非 NULL**
    // 的旧 current_process_step_id（它属于 fixture 自建的那条单 step 链），断言 4
    // 用它当「写入不变式」的护栏。
    let (_held_part, held_batch, old_step) =
        insert_worker_held_part(&pool, customer, "H-002D", worker, proc_b, 1, false).await;
    // 2026-10-10：把批次指针改指到**另一条链**的 step 上（指针漂移），而不是清空它。
    //
    // 为什么必须造漂移而不是清空：本用例要测的是「非顺应 ⇒ 按前端指定的
    // `next_process_id` 推进」，而进入那条分支要求 `is_pointer_consistent = false`。
    // 两条路子都能造出非顺应，清空最省事 —— 但那样断言 4 就退化成「本来就是 NULL
    // 所以还是 NULL」，**恒成立**，`mark_batch_returned` 的
    // `current_process_step_id = COALESCE($5, current_process_step_id)` 这条不变式
    // 就没人守了（把它改成 `= $5` 测试也不会红，而后果是每一次非顺应 RETURNED 都把
    // 批次链位置静默清空）。
    //
    // 造漂移后本用例的形态：锚链解析回退到指针所属链（`COALESCE(p.process_chain_id,
    // cur.chain_id)`，本 part 无链 ⇒ 取指针所属的 foreign 链），而那条链里**没有**
    // `pb.current_process_id = proc_b` 这个工序 ⇒ 按工序重定位落空 ⇒
    // `current_step_id` 为 NULL ⇒ `is_pointer_consistent = false`。
    //
    // ⚠️ 此时 `chain_state` 落 **`NONE`** 而不是 `TAIL`：`cur2` 在
    // `CHAIN_POSITION_LATERAL_SQL` 里是 `JOIN LATERAL (…) cur2 ON TRUE`（**内**
    // 连接），重定位 0 行会把整个 joined 行丢掉，于是取不到 `nsp.id IS NULL ⇒ TAIL`
    // 那条臂，只能由外层 `COALESCE(nx.chain_state, 'NONE')` 兜底成 `NONE`。
    // 落 `NONE` 同样不进「链尾自动送检」（那条要求 `is_pointer_consistent && TAIL`，
    // 两个条件都不满足），所以照样落 else 分支 ⇒ `step_id_opt = None` 绑进 SQL，
    // 断言 4 真正钉住 COALESCE 保留语义。
    //
    // ⚠️ foreign 链的那道 step **必须**挂 `proc_c`（≠ 批次当前工序 `proc_b`）：若挂上
    // `proc_b`，按工序重定位会正好命中它，`current_step_id == 指针` ⇒ 指针反而变成
    // 一致的，`chain_state` 与 `is_pointer_consistent` 同时为真 ⇒ 请求被「链尾自动
    // 送检」接管，本用例就测不到 else 分支了。
    let foreign_chain = {
        use hsh_erp_test_support::shared_test_snowflake;
        shared_test_snowflake().next_id()
    };
    sqlx::query(
        "INSERT INTO t_part_process_chain (id, name, version, created_at, created_by, \
         updated_at, updated_by) VALUES ($1, $2, 0, now(), 0, now(), 0)",
    )
    .bind(foreign_chain)
    .bind("chain-H-002D-foreign")
    .execute(&pool)
    .await
    .expect("insert foreign chain");
    let foreign_step = append_chain_step(&pool, foreign_chain, proc_c, 10).await;
    assert_ne!(
        foreign_step, old_step,
        "foreign step 必须与 fixture 原指针不同，否则造不出指针漂移"
    );
    sqlx::query("UPDATE t_part_batch SET current_process_step_id = $2 WHERE id = $1")
        .bind(held_batch)
        .bind(foreign_step)
        .execute(&pool)
        .await
        .expect("point batch at a foreign chain step");

    // 前置守卫：fixture 的 `with_chain=false` 失效的话，本测试会变成假绿，先在这里 fail
    let chain_id: Option<i64> = sqlx::query_scalar(
        "SELECT p.process_chain_id FROM t_part p \
         JOIN t_part_batch b ON b.part_id = p.id WHERE b.id = $1",
    )
    .bind(held_batch)
    .fetch_one(&pool)
    .await
    .expect("query part process_chain_id");
    assert!(
        chain_id.is_none(),
        "fixture 前置不成立：part 绑了工艺链 {chain_id:?}，本测试就测不到无链分支了"
    );

    let (app, token, _pool) = login_shelf_account(pool.clone(), "user2d", &[prod_shelf]).await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/prod/scan/worker-scan",
            Some(json!({
                "serial_no": "H-002D",
                "badge_code": "BC002D",
                "event_type": "RETURNED",
                "shelf_id": prod_shelf.to_string(),
                "next_process_id": proc_c.to_string(),
            })),
            Some(&token),
        ),
    )
    .await;
    // 这是本测试的核心断言：按 `i64` 解码 `process_chain_id` 时这里会 500
    // （`decoding column 0: unexpected null`，part 无工艺链）
    assert_eq!(s, StatusCode::OK, "scan RETURNED（part 无工艺链）: {env}");
    assert_eq!(env["code"], 0);
    assert_eq!(env["data"]["scan"]["event_type"], "WORKER_SCAN_RETURNED");

    let after: (Option<i64>, Option<i64>) = sqlx::query_as(
        "SELECT current_process_id, current_process_step_id \
         FROM t_part_batch WHERE id = $1",
    )
    .bind(held_batch)
    .fetch_one(&pool)
    .await
    .expect("query batch after RETURNED");
    assert_eq!(
        after.0,
        Some(proc_c),
        "RETURNED 应把 current_process_id 推进到 next_process_id({proc_c})，实际 {:?}",
        after.0
    );
    assert_eq!(
        after.1,
        Some(foreign_step),
        "else 分支解出的 step 是 None，RETURNED 后 SQL 的 COALESCE 必须保住扫描前的指针 \
         {foreign_step}（把 COALESCE 改成直接写 $5 会让这里变 None），实际 {:?}",
        after.1
    );
    assert_ne!(
        foreign_step, old_step,
        "指针漂移的前提：foreign step 必须不同于 fixture 原指针"
    );
}

/// 场景 2e（2026-10-09 新增）：**顺应工序**时 RETURNED 自动按链推进 —— 请求体
/// **不带** `next_process_id`，后端自己查链上下一道，两列同时推进。
///
/// ## 为什么这条用例是端到端的
/// 它把三条写点串成一条真实主干流：
/// 1. `POST /prod/queue/dispatch` —— 落**链首** step（`current_process_id` =
///    链首工序、`current_process_step_id` = 链首 step）；
/// 2. `POST /prod/queue/move` POOL→WORKER —— 批次压到工人手上（工序与指针都不动）；
/// 3. `POST /prod/scan/worker-scan` RETURNED —— **不带** `next_process_id`。
///
/// 只有第 1 步把指针落到链首 step，第 3 步的 `ChainPosition::is_pointer_consistent`
/// 才会为真、自动推进分支才可达。缺任一步本用例都会退化成「要求前端显式指定」
/// 那条分支（返回 40001）。
///
/// ## 断言
/// 推进后 `current_process_id` = 第二道工序、`current_process_step_id` =
/// 第二道 step（**两列一起动**，这正是 `mark_batch_returned` 本轮恢复写 step 的
/// 目的）。指针停在链首 step 而工序推进了那种「两列不同步」的形态，正是本次要消灭
/// 的状态。
#[tokio::test]
async fn worker_scan_returned_advances_step_pointer_when_process_chain_is_consistent() {
    let (pool, _app, _token, _fx) = bootstrap_as_manager().await;
    let customer = insert_customer_l2(&pool, "POOL2E").await;
    // 链 = [PROC-2E1(10), PROC-2E2(20)]；工种只映射第一道（否则 refill 会把刚
    // 归还的批次又抢回工人，干扰「落进第二道池」的断言）
    let proc_1 = seed_process(&pool, "PROC-2E1", "工序2E1").await;
    let proc_2 = seed_process(&pool, "PROC-2E2", "工序2E2").await;
    let wt = insert_work_type(&pool, "WT-2E", "工种2E", Some(5)).await;
    link_work_type_to_process(&pool, wt, proc_1).await;
    // 同一货架映射两道工序：dispatch 按链首解析货架、RETURNED 按推导出的第二道
    // 校验货架映射，两者都要过各自的货架守卫
    let prod_shelf = insert_shelf(&pool, "PROD-2E", "PROD-2E", "PRODUCTION").await;
    link_shelf_to_process(&pool, prod_shelf, proc_1).await;
    link_shelf_to_process(&pool, prod_shelf, proc_2).await;

    let worker = insert_worker(&pool, "BC002E", "工2E", Some(wt)).await;
    let (_part_id, batch_id, step_ids) =
        insert_pending_part_with_chain(&pool, customer, "P-002E", &[proc_1, proc_2]).await;
    let head_step = step_ids[0];
    let second_step = step_ids[1];

    // 前置：dispatch 之前指针与工序都必须是 NULL（真实输入形态）
    let before: (Option<i64>, Option<i64>) = sqlx::query_as(
        "SELECT current_process_id, current_process_step_id FROM t_part_batch WHERE id = $1",
    )
    .bind(batch_id)
    .fetch_one(&pool)
    .await
    .expect("query pending batch");
    assert_eq!(
        before,
        (None, None),
        "前置：PENDING 批次的工序与链内指针都应为 NULL"
    );

    let (app, mgr) = login_manager_with_username(&pool, "admin_pool2e").await;

    // 1. dispatch —— 落链首 step（请求里刻意传第二道工序，它必须被忽略）
    let (s1, env1) = send(
        app.clone(),
        json_request(
            "POST",
            "/prod/queue/dispatch",
            Some(json!({
                "targets": [{
                    "batch_id": batch_id.to_string(),
                    "target_process_id": proc_2.to_string(),
                }]
            })),
            Some(&mgr),
        ),
    )
    .await;
    assert_eq!(s1, StatusCode::OK, "dispatch: {env1}");
    let after_dispatch: (Option<i64>, Option<i64>) = sqlx::query_as(
        "SELECT current_process_id, current_process_step_id FROM t_part_batch WHERE id = $1",
    )
    .bind(batch_id)
    .fetch_one(&pool)
    .await
    .expect("query after dispatch");
    assert_eq!(
        after_dispatch,
        (Some(proc_1), Some(head_step)),
        "dispatch 应落链首工序 + 链首 step（≠ 请求里的 proc_2）: {env1}"
    );

    // 2. move POOL → WORKER（工序不动，指针不动）
    let (s2, env2) = send(
        app.clone(),
        json_request(
            "POST",
            "/prod/queue/move",
            Some(json!({
                "batch_id": batch_id.to_string(),
                // dispatch 刚把 version 从 0 推到 1
                "version": 1,
                "from": { "kind": "POOL",   "shelf_id": prod_shelf.to_string() },
                "to":   { "kind": "WORKER", "worker_id": worker.to_string() },
            })),
            Some(&mgr),
        ),
    )
    .await;
    assert_eq!(s2, StatusCode::OK, "move POOL→WORKER: {env2}");

    // 3. worker-scan RETURNED —— **不带** next_process_id
    let (app, token, _pool) = login_shelf_account(pool.clone(), "user2e", &[prod_shelf]).await;
    let (s3, env3) = send(
        app,
        json_request(
            "POST",
            "/prod/scan/worker-scan",
            Some(json!({
                "serial_no": "P-002E",
                "badge_code": "BC002E",
                "event_type": "RETURNED",
                "shelf_id": prod_shelf.to_string(),
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(
        s3,
        StatusCode::OK,
        "顺应工序时不该要求前端传 next_process_id: {env3}"
    );

    // 断言：两列一起推进到第二道
    let after: (Option<i64>, Option<i64>) = sqlx::query_as(
        "SELECT current_process_id, current_process_step_id FROM t_part_batch WHERE id = $1",
    )
    .bind(batch_id)
    .fetch_one(&pool)
    .await
    .expect("query after RETURNED");
    assert_eq!(after.0, Some(proc_2), "RETURNED 应自动推进到链上下一道工序");
    assert_eq!(
        after.1,
        Some(second_step),
        "RETURNED 应把链内位置指针一起推进到下一 step（两列必须同步）"
    );
    assert_ne!(
        after.1,
        Some(head_step),
        "指针不能停在链首 step —— 那样下次放回就会按错误位置推导下一道"
    );
}

/// 场景 2f（2026-10-09 新增，**行为变更**）：非顺应工序 + 请求体不带
/// `next_process_id` ⇒ `40001 VALIDATION_ERROR`，文案点明成因。
///
/// 「非顺应」的成因共四种（无链 / 链已软删 / 指针漂移 / 链内工序重复）与链尾，
/// 全落这一个分支。这里用场景 2d 的 fixture 形态（`with_chain=false`：part 无链、
/// 批次带一个孤儿 step 指针）—— 指针非 NULL 但它所属的链不是锚链，判据同样为
/// false。
#[tokio::test]
async fn worker_scan_returned_requires_next_process_id_when_not_consistent() {
    let (pool, _app, _token, _fx) = bootstrap_as_manager().await;
    let customer = insert_customer_l2(&pool, "POOL2F").await;
    let proc_b = seed_process(&pool, "PROC-2F1", "工序2F1").await;
    let proc_c = seed_process(&pool, "PROC-2F2", "工序2F2").await;
    let wt = insert_work_type(&pool, "WT-2F", "工种2F", Some(5)).await;
    link_work_type_to_process(&pool, wt, proc_b).await;
    let prod_shelf = insert_shelf(&pool, "PROD-2F", "PROD-2F", "PRODUCTION").await;
    link_shelf_to_process(&pool, prod_shelf, proc_c).await;

    let worker = insert_worker(&pool, "BC002F", "工2F", Some(wt)).await;
    let (_held_part, held_batch, _step) =
        insert_worker_held_part(&pool, customer, "H-002F", worker, proc_b, 1, false).await;
    // 2026-10-10：把 step 指针清成 NULL，造出**真的**非顺应形态。
    //
    // fixture 造的链是单 step 链，而锚链解析会回退到 `cur.chain_id`（批次的 step 指针
    // 所属链）—— 于是 `with_chain=false` 并不足以让本批次落到「非顺应」：锚链仍能解析、
    // 指针仍一致、`chain_state` 落 `TAIL`。指针清空后 `is_pointer_consistent = false`，
    // 三条闸门（非顺应 / 指针漂移）都成立，本用例才真正测到它声称测的那条分支。
    sqlx::query("UPDATE t_part_batch SET current_process_step_id = NULL WHERE id = $1")
        .bind(held_batch)
        .execute(&pool)
        .await
        .expect("clear step pointer");

    let (app, token, _pool) = login_shelf_account(pool.clone(), "user2f", &[prod_shelf]).await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/prod/scan/worker-scan",
            Some(json!({
                "serial_no": "H-002F",
                "badge_code": "BC002F",
                "event_type": "RETURNED",
                "shelf_id": prod_shelf.to_string(),
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::UNPROCESSABLE_ENTITY,
        "非顺应工序缺参应 422: {env}"
    );
    assert_eq!(env["code"], 40001, "应为 VALIDATION_ERROR: {env}");
    let msg = env["message"].as_str().unwrap_or_default();
    assert!(
        msg.contains("next_process_id") && msg.contains("非顺应工序"),
        "文案要同时点名「缺哪个字段」与「为什么必须填」，否则运营只会看到 422 去查前端: {env}"
    );

    // 批次不该被写脏
    let (location, holder): (Option<String>, Option<i64>) =
        sqlx::query_as("SELECT location, current_holder_id FROM t_part_batch WHERE id = $1")
            .bind(held_batch)
            .fetch_one(&pool)
            .await
            .expect("query batch after rejected scan");
    assert_eq!(
        location.as_deref(),
        Some("WORKER"),
        "拒收时批次仍应留在工人手上"
    );
    assert_eq!(holder, Some(worker));
}

/// 场景 8: worker-scan 越权 shelf → 40301 SHELF_MISMATCH
#[tokio::test]
async fn worker_scan_shelf_scope_violation_403() {
    let (pool, _app, _token, _fx) = bootstrap_as_manager().await;
    let customer = insert_customer_l2(&pool, "POOL8").await;
    let proc = seed_process(&pool, "PROC-H", "工序H").await;
    let wt = insert_work_type(&pool, "WT-H", "工种H", Some(5)).await;
    link_work_type_to_process(&pool, wt, proc).await;
    let shelf_x = insert_shelf(&pool, "PROD-H1", "PROD-H1", "PRODUCTION").await;
    let shelf_y = insert_shelf(&pool, "PROD-H2", "PROD-H2", "PRODUCTION").await;
    link_shelf_to_process(&pool, shelf_x, proc).await;
    link_shelf_to_process(&pool, shelf_y, proc).await;

    let worker = insert_worker(&pool, "BC008", "工8", Some(wt)).await;
    let (_held_part, _held_batch, _step) =
        insert_worker_held_part(&pool, customer, "H-008", worker, proc, 1, true).await;

    // user 只绑定 shelf_x，请求扫描到 shelf_y → 40301 SHELF_MISMATCH
    let (app, token, _pool) = login_shelf_account(pool.clone(), "user8", &[shelf_x]).await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/prod/scan/worker-scan",
            Some(json!({
                "serial_no": "H-008",
                "badge_code": "BC008",
                "event_type": "RETURNED",
                "shelf_id": shelf_y.to_string(),
                "next_process_id": proc.to_string(),
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "shelf 越权: {env}");
    assert_eq!(env["code"], 40301, "SHELF_MISMATCH: {env}");
}

/// 场景 14: refill 失败回滚 worker-scan（`#[ignore]`：DB 故障注入缺基建）
#[tokio::test]
#[ignore = "需要 DB 故障注入（人为断网 / 临时约束 / DROP TABLE mid-tx）来制造 refill 失败而 scan 成功的窗口；当前 test harness 无法注入"]
async fn refill_failure_rolls_back_worker_scan() {
    // 设计意图：refill_for_worker 与 worker_scan_event 共享 handler 内
    // begin() 的同一事务，refill 抛错 → 事务自动回滚 → scan 的
    // RETURNED_TO_SHELF / SENT_TO_INSPECTION 事件日志 + 状态翻转都应一并撤销。
    // 测试需要一种可控方式让 refill 内部失败（其它分支成功），例如：
    //   1. 先把 work_type 的 process 映射删掉（清空 t_work_type_process）
    //      → refill 进入「BIZ_WORK_TYPE_NO_PROCESS_MAPPING」分支抛错；
    //      但 scan 部分已写入事件日志；
    //   2. 验证：events 表里应无该 part 的新事件日志；
    //      part.location / current_holder_id 应保持原状。
    //   实现这一窗口需要在 scan 与 refill 中间时点突变 work_type 映射，
    //   而当前 refill 流程在 worker_scan_event *之后*（handler 层）调用，
    //   中间插入 mutation 需要重构或并发 tx。
}

/// RETURNED：目标架由服务端按负载选出，请求里**没有** `shelf_id`。
///
/// 两个映射架按 `display_order` 排成 A / B，但 `capacity` + 在架件数排成另一条次序
/// （A 80%、B 20%），断言落 B —— 于是「按物理顺序取第一个」与「按负载取最空」两种
/// 口径被分开。批次 `current_holder_id` 就是被断言的那个架 id。
#[tokio::test]
async fn worker_scan_returned_picks_least_loaded_shelf() {
    let (pool, _app, _token, _fx) = bootstrap_as_manager().await;
    let customer = insert_customer_l2(&pool, "AUTO-PICK").await;
    let proc_a = seed_process(&pool, "AP-P1", "AP1").await;
    let proc_b = seed_process(&pool, "AP-P2", "AP2").await;
    let wt = insert_work_type(&pool, "AP-WT", "AP工种", Some(0)).await;
    link_work_type_to_process(&pool, wt, proc_a).await;

    let shelf_a = insert_shelf(&pool, "AP-SH-A", "AP架A", "PRODUCTION").await;
    let shelf_b = insert_shelf(&pool, "AP-SH-B", "AP架B", "PRODUCTION").await;
    for shelf in [shelf_a, shelf_b] {
        sqlx::query("UPDATE t_shelf SET capacity = 100, display_order = $2 WHERE id = $1")
            .bind(shelf)
            .bind(if shelf == shelf_a { 0_i32 } else { 1 })
            .execute(&pool)
            .await
            .expect("set capacity/display_order");
        link_shelf_to_process(&pool, shelf, proc_b).await;
    }
    // 在架负载：A 80 件、B 20 件（`SUM(quantity)` 件数口径）
    let (_pa, _ba) = insert_pool_part(&pool, customer, "AP-LOAD-A", shelf_a, proc_b, 80).await;
    let (_pb, _bb) = insert_pool_part(&pool, customer, "AP-LOAD-B", shelf_b, proc_b, 20).await;

    let worker = insert_worker(&pool, "AP-W1", "AP工人", Some(wt)).await;
    let (_held_part, held_batch, _step) =
        insert_worker_held_part(&pool, customer, "AP-HELD", worker, proc_a, 1, false).await;
    // 清 step 指针压到「非顺应 ⇒ 显式 `next_process_id`」分支：fixture 的链是单
    // step 链，指针一致时会被判成 `TAIL` 并被「链尾自动送检」接管（那条路径由
    // `worker_scan_returned_at_chain_tail_auto_sends_to_inspection` 覆盖）
    sqlx::query("UPDATE t_part_batch SET current_process_step_id = NULL WHERE id = $1")
        .bind(held_batch)
        .execute(&pool)
        .await
        .expect("clear step pointer");

    let (app, token, _pool) =
        login_shelf_account(pool.clone(), "ap-user", &[shelf_a, shelf_b]).await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/prod/scan/worker-scan",
            Some(json!({
                "serial_no": "AP-HELD",
                "badge_code": "AP-W1",
                "event_type": "RETURNED",
                "next_process_id": proc_b.to_string(),
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "scan RETURNED: {env}");
    assert_eq!(env["data"]["scan"]["event_type"], "WORKER_SCAN_RETURNED");

    let (holder, location, current_process): (Option<i64>, Option<String>, Option<i64>) =
        sqlx::query_as(
            "SELECT current_holder_id, location, current_process_id FROM t_part_batch WHERE id = $1",
        )
        .bind(held_batch)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        holder,
        Some(shelf_b),
        "应落负载比例最低的架（20%），不是 display_order 最小的 A（80%）"
    );
    assert_eq!(location.as_deref(), Some("PRODUCTION_SHELF"));
    assert_eq!(current_process, Some(proc_b), "工序应推进到目标工序");
}

/// 链尾自动送检：单 step 链 + 指针一致 ⇒ RETURNED 直接送检。
///
/// 断言四件事：HTTP 200、响应 `event_type = "WORKER_SCAN_INSPECTED"`（**与请求的
/// `RETURNED` 不同** —— 这是前端必须按响应分支的那条语义）、批次
/// `status = INSPECTION` + `location = INSPECTION_SHELF`、holder 是服务端选出的
/// 品检架。
#[tokio::test]
async fn worker_scan_returned_at_chain_tail_auto_sends_to_inspection() {
    let (pool, _app, _token, _fx) = bootstrap_as_manager().await;
    let customer = insert_customer_l2(&pool, "TAIL").await;
    let proc_only = seed_process(&pool, "TAIL-P", "链尾唯一工序").await;
    let wt = insert_work_type(&pool, "TAIL-WT", "TAIL工种", Some(0)).await;
    link_work_type_to_process(&pool, wt, proc_only).await;

    let prod_shelf = insert_shelf(&pool, "TAIL-SH", "TAIL架", "PRODUCTION").await;
    link_shelf_to_process(&pool, prod_shelf, proc_only).await;
    let insp_shelf = insert_shelf(&pool, "TAIL-INSP", "TAIL品检架", "INSPECTION").await;

    // `with_chain = true` ⇒ part 绑链，链内**只有一道** step（= 链尾），且批次指针
    // 指向它 ⇒ `is_pointer_consistent = true` ∧ `chain_state == "TAIL"`
    let worker = insert_worker(&pool, "TAIL-W1", "TAIL工人", Some(wt)).await;
    let (_held_part, held_batch, _step) =
        insert_worker_held_part(&pool, customer, "TAIL-HELD", worker, proc_only, 1, true).await;

    let (app, token, _pool) =
        login_shelf_account(pool.clone(), "tail-user", &[prod_shelf, insp_shelf]).await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/prod/scan/worker-scan",
            // 刻意**不传** `next_process_id`：链尾没有下一道，它本来就该可省
            Some(json!({
                "serial_no": "TAIL-HELD",
                "badge_code": "TAIL-W1",
                "event_type": "RETURNED",
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "链尾 RETURNED 应自动送检: {env}");
    assert_eq!(
        env["data"]["scan"]["event_type"], "WORKER_SCAN_INSPECTED",
        "响应的 event_type 必须反映实际发生的动作（链尾 ⇒ 送检）: {env}"
    );

    let (status, location, holder): (String, Option<String>, Option<i64>) = sqlx::query_as(
        "SELECT status, location, current_holder_id FROM t_part_batch WHERE id = $1",
    )
    .bind(held_batch)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(status, "INSPECTION");
    assert_eq!(location.as_deref(), Some("INSPECTION_SHELF"));
    assert_eq!(holder, Some(insp_shelf), "应落服务端自动选出的品检架");
}

/// refill 跨架取料：worker-scan 路径传 `shelf_id = None`，候选池**不限架**。
///
/// 两个生产架各有一个在架批次；放回时落 A 架（只给 A 架配了映射），随后的 refill 若
/// 还按架取料就只能拿到 A 架那一批 —— 断言它拿到了**两个架**的批次即证明跨架生效。
#[tokio::test]
async fn refill_takes_across_all_shelves_without_shelf_anchor() {
    let (pool, _app, _token, _fx) = bootstrap_as_manager().await;
    let customer = insert_customer_l2(&pool, "XSHELF").await;
    let proc_a = seed_process(&pool, "XS-P1", "XS1").await;
    let proc_b = seed_process(&pool, "XS-P2", "XS2").await;
    let wt = insert_work_type(&pool, "XS-WT", "XS工种", Some(5)).await;
    link_work_type_to_process(&pool, wt, proc_a).await;
    link_work_type_to_process(&pool, wt, proc_b).await;

    // A 架只映射 proc_a（RETURNED 的目标架）；B 架映射 proc_b，且预置一个批次
    let shelf_a = insert_shelf(&pool, "XS-SH-A", "XS架A", "PRODUCTION").await;
    let shelf_b = insert_shelf(&pool, "XS-SH-B", "XS架B", "PRODUCTION").await;
    link_shelf_to_process(&pool, shelf_a, proc_a).await;
    link_shelf_to_process(&pool, shelf_b, proc_b).await;
    let (_pb, batch_b) = insert_pool_part(&pool, customer, "XS-POOL-B", shelf_b, proc_b, 1).await;

    let worker = insert_worker(&pool, "XS-W1", "XS工人", Some(wt)).await;
    let (_held_part, held_batch, _step) =
        insert_worker_held_part(&pool, customer, "XS-HELD", worker, proc_a, 1, false).await;
    sqlx::query("UPDATE t_part_batch SET current_process_step_id = NULL WHERE id = $1")
        .bind(held_batch)
        .execute(&pool)
        .await
        .expect("clear step pointer");

    let (app, token, _pool) = login_shelf_account(pool.clone(), "xs-user", &[shelf_a]).await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/prod/scan/worker-scan",
            Some(json!({
                "serial_no": "XS-HELD",
                "badge_code": "XS-W1",
                "event_type": "RETURNED",
                "next_process_id": proc_a.to_string(),
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "scan RETURNED: {env}");
    let taken = env["data"]["refill"]["taken"]
        .as_array()
        .expect("refill.taken");
    let batch_ids: Vec<String> = taken
        .iter()
        .map(|t| t["batch_id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        taken.len(),
        2,
        "refill 应跨两个架各取一件（放回那件 + B 架原有那件）: {env}"
    );
    assert!(
        batch_ids.contains(&batch_b.to_string()),
        "必须取到 B 架的批次 {batch_b}（证明不限架）: {env}"
    );
}

// ===========================================================================
//  2026-10-11：部分数量（`WorkerScanRequest::quantity`）
//
//  与同域 `pickup.rs` 的部分领取同款三种落法（缺省 / `==` 整批、`0 < q <
//  batch.quantity` 拆批、`<= 0` 或 `>` 报 20111），**唯一差别**是余量的去向：
//  worker-scan 的余量继承 `current_holder_id` **留在工人手上**，而不是像 pick-up
//  那样留在原处（架上）。下面每条拆批用例都逐字断言这一点。
// ===========================================================================

/// 读批次的 5 个关键列（quantity / status / location / holder / version）。
async fn read_batch_row(
    pool: &PgPool,
    batch_id: i64,
) -> (i32, String, Option<String>, Option<i64>, i32) {
    sqlx::query_as(
        "SELECT quantity, status, location, current_holder_id, version \
         FROM t_part_batch WHERE id = $1",
    )
    .bind(batch_id)
    .fetch_one(pool)
    .await
    .expect("read t_part_batch")
}

/// 该 part 名下未软删的批次行数（拆批残留检查）。
async fn count_batches(pool: &PgPool, part_id: i64) -> i64 {
    sqlx::query_scalar(
        "SELECT COUNT(*) FROM t_part_batch WHERE part_id = $1 AND deleted_at IS NULL",
    )
    .bind(part_id)
    .fetch_one(pool)
    .await
    .expect("count t_part_batch")
}

/// 拆批产出：新批次（`id <> source_id` 的那一个）。
async fn find_split_batch(pool: &PgPool, part_id: i64, source_id: i64) -> i64 {
    sqlx::query_scalar(
        "SELECT id FROM t_part_batch \
         WHERE part_id = $1 AND deleted_at IS NULL AND id <> $2",
    )
    .bind(part_id)
    .bind(source_id)
    .fetch_one(pool)
    .await
    .expect("部分数量应拆出一个新批次")
}

/// 按 `event_type` 取该 part 名下事件的 `quantity`（没有则 panic）。
async fn event_quantity(pool: &PgPool, part_id: i64, event_type: &str) -> Option<i32> {
    sqlx::query_scalar(
        "SELECT quantity FROM t_part_event \
         WHERE part_id = $1 AND event_type = $2 ORDER BY id DESC",
    )
    .bind(part_id)
    .bind(event_type)
    .fetch_optional(pool)
    .await
    .expect("query t_part_event")
}

async fn event_count(pool: &PgPool, part_id: i64, event_type: &str) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM t_part_event WHERE part_id = $1 AND event_type = $2")
        .bind(part_id)
        .bind(event_type)
        .fetch_one(pool)
        .await
        .expect("count t_part_event")
}

/// 造「非顺应 ⇒ 显式 `next_process_id`」的形态：清掉 step 指针。
///
/// fixture 造的链是单 step 链，指针一致时 `chain_state == "TAIL"` ⇒ RETURNED 会被
/// 「链尾自动送检」接管（那条由本节最后一条用例专门覆盖）。
async fn clear_step_pointer(pool: &PgPool, batch_id: i64) {
    sqlx::query("UPDATE t_part_batch SET current_process_step_id = NULL WHERE id = $1")
        .bind(batch_id)
        .execute(pool)
        .await
        .expect("clear step pointer");
}

/// 缺省 `quantity` ⇒ 整批，行为与该字段引入前**逐字一致**。
///
/// 钉住三件事：响应 `scan.batch_id` 仍是源批次（不是某个新拆出来的）、批次数仍为 1
/// （没拆）、数量不变（10 → 10）。
#[tokio::test]
async fn worker_scan_returned_without_quantity_keeps_whole_batch() {
    let (pool, _app, _token, _fx) = bootstrap_as_manager().await;
    let customer = insert_customer_l2(&pool, "QDEF").await;
    let proc_a = seed_process(&pool, "QD-P1", "QD1").await;
    let proc_b = seed_process(&pool, "QD-P2", "QD2").await;
    let wt = insert_work_type(&pool, "QD-WT", "QD工种", Some(0)).await;
    link_work_type_to_process(&pool, wt, proc_a).await;
    let prod_shelf = insert_shelf(&pool, "QD-SH", "QD架", "PRODUCTION").await;
    link_shelf_to_process(&pool, prod_shelf, proc_b).await;

    let worker = insert_worker(&pool, "QD-W1", "QD工人", Some(wt)).await;
    let (held_part, held_batch, _step) =
        insert_worker_held_part(&pool, customer, "QD-HELD", worker, proc_a, 10, false).await;
    clear_step_pointer(&pool, held_batch).await;

    let (app, token, _pool) = login_shelf_account(pool.clone(), "qd-user", &[prod_shelf]).await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/prod/scan/worker-scan",
            // body 里**没有** quantity 这个 key（不是 null）
            Some(json!({
                "serial_no": "QD-HELD",
                "badge_code": "QD-W1",
                "event_type": "RETURNED",
                "next_process_id": proc_b.to_string(),
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "缺省 quantity 应整批放回: {env}");
    assert_eq!(
        env["data"]["scan"]["batch_id"],
        json!(held_batch.to_string()),
        "整批路径的 scan.batch_id 应仍是工人手上那批: {env}"
    );
    assert_eq!(
        count_batches(&pool, held_part).await,
        1,
        "缺省 quantity 不得拆批"
    );
    let (qty, status, location, holder, _v) = read_batch_row(&pool, held_batch).await;
    assert_eq!(qty, 10, "整批放回不改数量");
    assert_eq!(status, "IN_PROCESS");
    assert_eq!(location.as_deref(), Some("PRODUCTION_SHELF"));
    assert_eq!(holder, Some(prod_shelf));
    assert_eq!(
        event_count(&pool, held_part, "SPLIT").await,
        0,
        "整批路径不得写 SPLIT 事件"
    );
    assert_eq!(
        event_quantity(&pool, held_part, "RETURNED_TO_SHELF").await,
        Some(10),
        "RETURNED_TO_SHELF.quantity = 整批量"
    );
}

/// `quantity == batch.quantity` ⇒ **合法**，等同整批（不拆批）。
///
/// 与 `POST /api/v2/batches/split`（要求严格小于）的语义差异必须钉住：`==` 在
/// worker-scan / pick-up 上是「显式整批」的写法，在那条拆批端点上才是非法。
#[tokio::test]
async fn worker_scan_returned_quantity_equal_batch_quantity_is_whole_batch() {
    let (pool, _app, _token, _fx) = bootstrap_as_manager().await;
    let customer = insert_customer_l2(&pool, "QEQ").await;
    let proc_a = seed_process(&pool, "QE-P1", "QE1").await;
    let proc_b = seed_process(&pool, "QE-P2", "QE2").await;
    let wt = insert_work_type(&pool, "QE-WT", "QE工种", Some(0)).await;
    link_work_type_to_process(&pool, wt, proc_a).await;
    let prod_shelf = insert_shelf(&pool, "QE-SH", "QE架", "PRODUCTION").await;
    link_shelf_to_process(&pool, prod_shelf, proc_b).await;

    let worker = insert_worker(&pool, "QE-W1", "QE工人", Some(wt)).await;
    let (held_part, held_batch, _step) =
        insert_worker_held_part(&pool, customer, "QE-HELD", worker, proc_a, 8, false).await;
    clear_step_pointer(&pool, held_batch).await;

    let (app, token, _pool) = login_shelf_account(pool.clone(), "qe-user", &[prod_shelf]).await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/prod/scan/worker-scan",
            Some(json!({
                "serial_no": "QE-HELD",
                "badge_code": "QE-W1",
                "event_type": "RETURNED",
                "next_process_id": proc_b.to_string(),
                "quantity": "8",
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::OK,
        "quantity == batch.quantity 应等同整批放回（不是非法值）: {env}"
    );
    assert_eq!(
        env["data"]["scan"]["batch_id"],
        json!(held_batch.to_string())
    );
    assert_eq!(
        count_batches(&pool, held_part).await,
        1,
        "quantity == 总量时不得拆批"
    );
    let (qty, _s, location, holder, _v) = read_batch_row(&pool, held_batch).await;
    assert_eq!(qty, 8, "整批放回不改数量");
    assert_eq!(location.as_deref(), Some("PRODUCTION_SHELF"));
    assert_eq!(holder, Some(prod_shelf));
    assert_eq!(event_count(&pool, held_part, "SPLIT").await, 0);
}

/// RETURNED 部分放回：拆出 4 件走放回，余下 6 件**留在工人手上**。
///
/// 断言清单：
/// - 响应 `scan.batch_id` = **新批次**（不是源批次）
/// - 新批次：quantity 4、location=PRODUCTION_SHELF、holder=选出的生产架、
///   `current_process_id` 推进到 `next_process_id`
/// - 源批次：quantity 6、**location 仍是 WORKER**、holder 仍是该工人、version +1
///   （拆批 OCC 写）；与 pick-up 的「余量留架上」形成对照
/// - 批次数 = 2、两批数量之和 == 10
/// - 事件：`SPLIT`（quantity 4）+ `RETURNED_TO_SHELF`（quantity 4）都指向新批次
#[tokio::test]
async fn worker_scan_returned_partial_splits_batch_and_keeps_remainder_on_worker() {
    let (pool, _app, _token, _fx) = bootstrap_as_manager().await;
    let customer = insert_customer_l2(&pool, "QPR").await;
    let proc_a = seed_process(&pool, "QP-P1", "QP1").await;
    let proc_b = seed_process(&pool, "QP-P2", "QP2").await;
    let wt = insert_work_type(&pool, "QP-WT", "QP工种", Some(0)).await;
    link_work_type_to_process(&pool, wt, proc_a).await;
    let prod_shelf = insert_shelf(&pool, "QP-SH", "QP架", "PRODUCTION").await;
    link_shelf_to_process(&pool, prod_shelf, proc_b).await;

    let worker = insert_worker(&pool, "QP-W1", "QP工人", Some(wt)).await;
    let (held_part, held_batch, _step) =
        insert_worker_held_part(&pool, customer, "QP-HELD", worker, proc_a, 10, false).await;
    clear_step_pointer(&pool, held_batch).await;

    let (app, token, _pool) = login_shelf_account(pool.clone(), "qp-user", &[prod_shelf]).await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/prod/scan/worker-scan",
            Some(json!({
                "serial_no": "QP-HELD",
                "badge_code": "QP-W1",
                "event_type": "RETURNED",
                "next_process_id": proc_b.to_string(),
                "quantity": "4",
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "部分放回应 200: {env}");

    let new_id = find_split_batch(&pool, held_part, held_batch).await;
    assert_eq!(
        env["data"]["scan"]["batch_id"],
        json!(new_id.to_string()),
        "响应 scan.batch_id 必须是**被处理的那一个新批次**: {env}"
    );
    assert_ne!(new_id, held_batch, "新批次不能是源批次本身");

    // 新批次：拆走的那 4 件被放回生产架并推进到下一道工序
    let (n_qty, n_status, n_loc, n_holder, _n_v) = read_batch_row(&pool, new_id).await;
    assert_eq!(n_qty, 4, "新批次数量 = 本次实际操作量");
    assert_eq!(n_status, "IN_PROCESS");
    assert_eq!(n_loc.as_deref(), Some("PRODUCTION_SHELF"));
    assert_eq!(n_holder, Some(prod_shelf));
    let n_proc: Option<i64> =
        sqlx::query_scalar("SELECT current_process_id FROM t_part_batch WHERE id = $1")
            .bind(new_id)
            .fetch_one(&pool)
            .await
            .expect("query new batch process");
    assert_eq!(
        n_proc,
        Some(proc_b),
        "RETURNED 的工序推进必须作用在新批次上（源批次不进 P2 池）"
    );

    // 源批次：余量**留在工人手上**
    let (s_qty, s_status, s_loc, s_holder, s_ver) = read_batch_row(&pool, held_batch).await;
    assert_eq!(s_qty, 6, "源批次应剩 10 - 4 = 6");
    assert_eq!(s_status, "IN_PROCESS");
    assert_eq!(
        s_loc.as_deref(),
        Some("WORKER"),
        "余量必须留在工人手上（worker-scan 与 pick-up 的唯一差别）"
    );
    assert_eq!(
        s_holder,
        Some(worker),
        "余量的 current_holder_id 不变 ⇒ 仍出现在报工台「已持有」列表"
    );
    assert_eq!(s_ver, 1, "拆批那笔 UPDATE 的 version = version + 1");
    assert_eq!(
        count_batches(&pool, held_part).await,
        2,
        "部分数量应恰好拆成两批"
    );
    assert_eq!(n_qty + s_qty, 10, "两批数量之和必须等于原数量");

    // 余量确实还在「已持有」列表的取行口径里（location=WORKER + holder=worker）
    assert_eq!(
        count_held_by_worker(&pool, worker).await,
        1,
        "余量批次仍算工人持有件"
    );

    // 事件：SPLIT + RETURNED_TO_SHELF 都挂新批次、数量都是 4
    assert_eq!(
        event_count(&pool, held_part, "SPLIT").await,
        1,
        "拆批必须留痕（SPLIT）"
    );
    assert_eq!(event_quantity(&pool, held_part, "SPLIT").await, Some(4));
    assert_eq!(
        event_quantity(&pool, held_part, "RETURNED_TO_SHELF").await,
        Some(4),
        "RETURNED_TO_SHELF.quantity = 本次实际放回量（不是批次全量）"
    );
    let ev_batch: Vec<i64> = sqlx::query_scalar(
        "SELECT DISTINCT batch_id FROM t_part_event \
         WHERE part_id = $1 AND event_type IN ('SPLIT', 'RETURNED_TO_SHELF')",
    )
    .bind(held_part)
    .fetch_all(&pool)
    .await
    .expect("query event batch ids");
    assert_eq!(ev_batch, vec![new_id], "两条事件都应挂在被处理的新批次上");
}

/// INSPECTED 部分送检：拆出 3 件去品检，余下 5 件**留在工人手上**。
#[tokio::test]
async fn worker_scan_inspected_partial_splits_batch_and_keeps_remainder_on_worker() {
    let (pool, _app, _token, _fx) = bootstrap_as_manager().await;
    let customer = insert_customer_l2(&pool, "QPI").await;
    let proc_a = seed_process(&pool, "QI-P1", "QI1").await;
    let wt = insert_work_type(&pool, "QI-WT", "QI工种", Some(0)).await;
    link_work_type_to_process(&pool, wt, proc_a).await;
    let prod_shelf = insert_shelf(&pool, "QI-SH", "QI架", "PRODUCTION").await;
    let insp_shelf = insert_shelf(&pool, "QI-INSP", "QI品检架", "INSPECTION").await;

    let worker = insert_worker(&pool, "QI-W1", "QI工人", Some(wt)).await;
    let (held_part, held_batch, _step) =
        insert_worker_held_part(&pool, customer, "QI-HELD", worker, proc_a, 8, false).await;

    let (app, token, _pool) =
        login_shelf_account(pool.clone(), "qi-user", &[prod_shelf, insp_shelf]).await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/prod/scan/worker-scan",
            Some(json!({
                "serial_no": "QI-HELD",
                "badge_code": "QI-W1",
                "event_type": "INSPECTED",
                "quantity": "3",
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "部分送检应 200: {env}");
    assert_eq!(env["data"]["scan"]["event_type"], "WORKER_SCAN_INSPECTED");

    let new_id = find_split_batch(&pool, held_part, held_batch).await;
    assert_eq!(
        env["data"]["scan"]["batch_id"],
        json!(new_id.to_string()),
        "响应 scan.batch_id 必须是送检的那一个新批次: {env}"
    );

    let (n_qty, n_status, n_loc, n_holder, _n_v) = read_batch_row(&pool, new_id).await;
    assert_eq!(n_qty, 3, "新批次数量 = 本次实际送检量");
    assert_eq!(n_status, "INSPECTION");
    assert_eq!(n_loc.as_deref(), Some("INSPECTION_SHELF"));
    assert_eq!(n_holder, Some(insp_shelf), "应落服务端自动选出的品检架");

    // 源批次：余量仍 IN_PROCESS + WORKER
    let (s_qty, s_status, s_loc, s_holder, _s_v) = read_batch_row(&pool, held_batch).await;
    assert_eq!(s_qty, 5, "源批次应剩 8 - 3 = 5");
    assert_eq!(s_status, "IN_PROCESS", "余量不进品检流");
    assert_eq!(s_loc.as_deref(), Some("WORKER"));
    assert_eq!(s_holder, Some(worker));
    assert_eq!(count_batches(&pool, held_part).await, 2);
    assert_eq!(n_qty + s_qty, 8);

    assert_eq!(event_count(&pool, held_part, "SPLIT").await, 1);
    assert_eq!(
        event_quantity(&pool, held_part, "SENT_TO_INSPECTION").await,
        Some(3),
        "SENT_TO_INSPECTION.quantity = 本次实际送检量"
    );
}

/// **TAIL（链尾自动送检）分支**下的部分放回 —— 最容易漏的那条。
///
/// `worker_scan_event` 的 RETURNED 臂里 TAIL 是一条 **early-return**（链位置判定
/// 之后、`mark_batch_returned` 之前就返回）。若拆批插在它之后，这条 early-return
/// 拿到的仍是源批次 ⇒ 送检的是整批，本次指定的 4 件被静默吞掉。本用例钉死：
/// 送检的是新批次（4 件），余下 6 件仍留在工人手上。
///
/// fixture 用**单 step 链 + 指针一致**造出 TAIL（`with_chain = true`）。
#[tokio::test]
async fn worker_scan_returned_at_chain_tail_with_partial_quantity_sends_only_split_part() {
    let (pool, _app, _token, _fx) = bootstrap_as_manager().await;
    let customer = insert_customer_l2(&pool, "QT").await;
    let proc_only = seed_process(&pool, "QT-P", "QT唯一工序").await;
    let wt = insert_work_type(&pool, "QT-WT", "QT工种", Some(0)).await;
    link_work_type_to_process(&pool, wt, proc_only).await;

    let prod_shelf = insert_shelf(&pool, "QT-SH", "QT架", "PRODUCTION").await;
    link_shelf_to_process(&pool, prod_shelf, proc_only).await;
    let insp_shelf = insert_shelf(&pool, "QT-INSP", "QT品检架", "INSPECTION").await;

    let worker = insert_worker(&pool, "QT-W1", "QT工人", Some(wt)).await;
    let (held_part, held_batch, _step) =
        insert_worker_held_part(&pool, customer, "QT-HELD", worker, proc_only, 10, true).await;

    let (app, token, _pool) =
        login_shelf_account(pool.clone(), "qt-user", &[prod_shelf, insp_shelf]).await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/prod/scan/worker-scan",
            // 刻意不传 next_process_id：链尾没有下一道，它本来就该可省
            Some(json!({
                "serial_no": "QT-HELD",
                "badge_code": "QT-W1",
                "event_type": "RETURNED",
                "quantity": "4",
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "链尾部分放回应自动送检: {env}");
    assert_eq!(
        env["data"]["scan"]["event_type"], "WORKER_SCAN_INSPECTED",
        "链尾放回仍按响应分支（与请求的 RETURNED 不同）: {env}"
    );

    let new_id = find_split_batch(&pool, held_part, held_batch).await;
    assert_eq!(
        env["data"]["scan"]["batch_id"],
        json!(new_id.to_string()),
        "TAIL early-return 必须作用在拆出来的新批次上: {env}"
    );

    let (n_qty, n_status, n_loc, n_holder, _n_v) = read_batch_row(&pool, new_id).await;
    assert_eq!(n_qty, 4, "送检的应只是本次指定的 4 件");
    assert_eq!(n_status, "INSPECTION");
    assert_eq!(n_loc.as_deref(), Some("INSPECTION_SHELF"));
    assert_eq!(n_holder, Some(insp_shelf));

    let (s_qty, s_status, s_loc, s_holder, _s_v) = read_batch_row(&pool, held_batch).await;
    assert_eq!(s_qty, 6, "源批次应剩 10 - 4 = 6（若整批被送检这里会是 10）");
    assert_eq!(s_status, "IN_PROCESS", "余量不能跟着进品检流");
    assert_eq!(s_loc.as_deref(), Some("WORKER"));
    assert_eq!(s_holder, Some(worker));
    assert_eq!(count_batches(&pool, held_part).await, 2);
    assert_eq!(event_count(&pool, held_part, "SPLIT").await, 1);
}

/// `quantity <= 0` / `> batch.quantity` ⇒ `20111 BIZ_PART_BATCH_INVALID_QUANTITY`，
/// 且**一个字节都不许写**（不拆批、不写事件、批次原封不动）。
///
/// 逐条覆盖两个 `event_type` × 三种非法值。用单 step 链（TAIL 形态）跑 RETURNED
/// 的理由：那条路径在拆批前就通过了链位置判定，不需要 `next_process_id` ⇒ 非法
/// `quantity` 一定是被数量校验拦下，而不是被 40001 抢先拒掉。
#[tokio::test]
async fn worker_scan_invalid_quantity_returns_20111() {
    let (pool, _app, _token, _fx) = bootstrap_as_manager().await;
    let customer = insert_customer_l2(&pool, "QIV").await;
    let proc_only = seed_process(&pool, "QV-P", "QV唯一工序").await;
    let wt = insert_work_type(&pool, "QV-WT", "QV工种", Some(0)).await;
    link_work_type_to_process(&pool, wt, proc_only).await;
    let prod_shelf = insert_shelf(&pool, "QV-SH", "QV架", "PRODUCTION").await;
    link_shelf_to_process(&pool, prod_shelf, proc_only).await;
    let insp_shelf = insert_shelf(&pool, "QV-INSP", "QV品检架", "INSPECTION").await;

    let worker = insert_worker(&pool, "QV-W1", "QV工人", Some(wt)).await;
    let (held_part, held_batch, _step) =
        insert_worker_held_part(&pool, customer, "QV-HELD", worker, proc_only, 10, true).await;

    let (app, token, _pool) =
        login_shelf_account(pool.clone(), "qv-user", &[prod_shelf, insp_shelf]).await;
    for (idx, (event_type, qty)) in [
        ("RETURNED", json!("0")),
        ("RETURNED", json!("-3")),
        ("RETURNED", json!("11")),
        ("INSPECTED", json!("0")),
        ("INSPECTED", json!("-3")),
        ("INSPECTED", json!("11")),
    ]
    .into_iter()
    .enumerate()
    {
        let (s, env) = send(
            app.clone(),
            json_request(
                "POST",
                "/prod/scan/worker-scan",
                Some(json!({
                    "serial_no": "QV-HELD",
                    "badge_code": "QV-W1",
                    "event_type": event_type,
                    "quantity": qty,
                })),
                Some(&token),
            ),
        )
        .await;
        // 20111 在本仓映射 HTTP 400（与 POST .../split / pick-up 的数量非法同码同状态）
        assert_eq!(
            s,
            StatusCode::BAD_REQUEST,
            "[{idx}] {event_type} quantity={qty} 应 400: {env}"
        );
        assert_eq!(
            env["code"], 20111,
            "[{idx}] {event_type} quantity={qty} 应 BIZ_PART_BATCH_INVALID_QUANTITY: {env}"
        );

        // 校验失败 ⇒ 事务回滚，一个字节都不许写
        assert_eq!(
            count_batches(&pool, held_part).await,
            1,
            "[{idx}] {event_type} quantity={qty}：非法数量不得拆批"
        );
        let (bq, bs, bl, bh, bv) = read_batch_row(&pool, held_batch).await;
        assert_eq!(bq, 10, "[{idx}] {event_type} quantity={qty}：数量不应被改");
        assert_eq!(bs, "IN_PROCESS");
        assert_eq!(bl.as_deref(), Some("WORKER"));
        assert_eq!(bh, Some(worker));
        assert_eq!(bv, 0, "[{idx}] version 不应被改");
        assert_eq!(
            event_count(&pool, held_part, "SPLIT").await,
            0,
            "[{idx}] 非法数量不得留 SPLIT 事件"
        );
    }
    assert_eq!(
        event_count(&pool, held_part, "SENT_TO_INSPECTION").await,
        0,
        "非法数量不得写送检事件"
    );
}

//! `POST /api/v2/admin/recompute-rollup`（对账 / 派生缓存修数端点）集成测试
//!
//! 2026-10-01 新增。
//!
//! ## 覆盖场景
//! 1. `recompute_rollup_fixes_drifted_part_and_is_idempotent`
//!    —— 注入「批次 COMPLETED、part 还 PENDING」的漂移 → 定点对账修正 + 报告
//!    before→after；**立刻重跑必须 0 变化**（幂等契约）；顺带验证 part 进终态时
//!    序列号被释放并归档（rollup step 4 也是对账的一部分）。
//! 2. `recompute_rollup_fixes_drifted_assembly`
//!    —— 反向漂移（子件状态正确、父装配件没跟上）→ 走 `assembly_ids` 定点修正。
//! 3. `recompute_rollup_requires_manager`
//!    —— 非 Manager（INSPECTOR）→ 403 `FORBIDDEN`，且**没动任何数据**。
//! 4. `recompute_rollup_without_body_is_full_scope`
//!    —— 完全不带 body（无 `Content-Type` 头）→ `scope="ALL"` 且 200。
//! 5. `recompute_rollup_rejects_limit_over_cap`
//!    —— `limit` 超上限 → 400 / `20104`（防止单请求锁住全表）。
//! 6. `recompute_rollup_full_scope_advances_by_after_id_cursor`
//!    —— 全量对账的 `part_after_id` 游标续扫：单轮 `limit=1` + 回传
//!    `next_part_after_id` 直到 `truncated=false`，必须**单调推进**且把所有漂移
//!    各修正一次（2026-10-01 review 第 1 轮 M7：改造前没有游标，重复调用永远从
//!    最小 id 重扫）。
//! 7. `recompute_rollup_full_scope_covers_interleaved_part_and_assembly_ids`
//!    —— **两表 id 交错**的续扫（2026-10-01 review 第 2 轮 MAJOR-2）：`t_part` 与
//!    `t_assembly` 的 id 来自同一个雪花流、按时间序交错，共用一个游标会永久跳过
//!    `(assembly_max, part_max]` 那段装配件却仍报 `truncated=false`。
//! 8. `recompute_rollup_reports_parts_skipped_by_terminal_guard`
//!    —— 终态守卫跳过必须**显式上报**（第 2 轮 MAJOR-1）：`parts_skipped_terminal`
//!    + `skipped_terminal[{id,current,derived}]`，与「数据已一致」可区分。
//!
//! ## 为什么这组测试重要
//!
//! 对账端点**不重写**派生算法（复用 `status_gate::rollup_part_derived` 与
//! `assembly` 域的 `sync_assembly_status`），它最大的风险不是「算错」而是
//! 「算得和业务流不一样」/「跑第二次又改一遍」。用例 1、2 的幂等断言正是守住
//! 后者：任何一次「派生写没有真正幂等」的实现都会在第二次调用时立刻暴露。
//!
//! ## Fixture
//!
//! 与 part 域其余 sub-file 一致：`load_part_fixture` 只提供「不可变共享基线」
//! （客户 / 工序 / 货架 / 用户 / 角色），**不预置 part**；漂移行由本文件
//! 下面的 `insert_drifted_part` / `insert_drifted_assembly` 按用例直插
//! （与 `tests/part/crud.rs` 等 11 个 sub-file 的做法同形；把它们统一迁进
//! `test-support` 是 PR-C 末的独立重构，不在本轮范围）。
//!
//! ## 并行 / 认证
//! 进程级 test_pool 每次 fresh database，无需 Mutex 串行化。

use axum::http::StatusCode;
use serde_json::{Value, json};
use sqlx::PgPool;

use hsh_erp_rust::infra::clock::now_naive;
use hsh_erp_rust::shared::error::code;
use hsh_erp_test_support::{
    PartFixture, json_request, load_part_fixture, login_token, pool_snowflake, send,
    shared_test_snowflake, test_app, test_pool, test_state,
};

const URL: &str = "/admin/recompute-rollup";

/// 进程级共享雪花生成器：转发到 test-support 的 `shared_test_snowflake()`。
///
/// 每次 `SnowflakeIdGenerator::new(...)` 只调一次 `next_id()` 会让同毫秒插入的
/// 多行拿到相同 ID（首次 `next_id()` 固定返回 `compose(now_ms, seq=0)`），
/// 表现为 `duplicate key ... t_process_pkey` 之类的偶发失败。共享单例让 sequence
/// 在进程内单调递增。
///
/// 2026-10-09 改用 test-support 的共享 generator（review 第 1 轮 Q1）：本文件原域内
/// 单例写死 `instance = 1`，与 `tests/part/crud.rs` 的域内单例**完全同 instance**，
/// 两者同属 `part` 一个 binary —— 在 `cargo test --test part` 的同进程多线程下即
/// 「同 instance + 同毫秒 + 同 seq」的原 bug 复现形态（23505）。改为全进程共享后，
/// 这些 ID 与 `pool_snowflake()` 发出的 fixture ID 同属一条流。
fn next_test_id() -> i64 {
    shared_test_snowflake().next_id()
}

/// 插一个 `t_part` 行，`status` 由调用方指定（用来造漂移）。
#[allow(clippy::too_many_arguments)]
async fn insert_drifted_part(
    pool: &PgPool,
    customer_id: i64,
    name: &str,
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
         VALUES ($1, $2, $3, 'D-RECOMP', $4, $5, $3, $6, $6, 1, 0, $7, $8, $8)",
    )
    .bind(id)
    .bind(serial_no)
    .bind(name)
    .bind(customer_id)
    .bind(status)
    .bind(today)
    .bind(assembly_id)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert drifted part");
    id
}

/// 插一条 `t_part_batch`（`status` 由调用方指定 —— 真源侧的值）。
async fn insert_batch_with_status(pool: &PgPool, part_id: i64, status: &str) -> i64 {
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

/// 插一个 `t_assembly` 行（`status` 由调用方指定，用来造父件漂移）。
async fn insert_drifted_assembly(pool: &PgPool, customer_id: i64, status: &str) -> i64 {
    let id = next_test_id();
    let now = now_naive();
    let today = now.date();
    sqlx::query(
        "INSERT INTO t_assembly (id, drawing_no, name, applicant_name, customer_id, \
         request_date, planned_delivery_date, status, quantity, unit_price, total_price, \
         version, created_at, updated_at) \
         VALUES ($1, 'D-RECOMP-A', '总成-对账', '', $2, $3, $3, $4, 1, 0, 0, 0, $5, $5)",
    )
    .bind(id)
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

async fn assembly_status_str(pool: &PgPool, assembly_id: i64) -> String {
    let (st,): (String,) =
        sqlx::query_as::<_, (String,)>("SELECT status FROM t_assembly WHERE id = $1")
            .bind(assembly_id)
            .fetch_one(pool)
            .await
            .expect("read assembly status");
    st
}

/// 起一份 fresh database + part fixture + Router，并返回
/// `(pool, app, manager_token, inspector_token, PartFixture)`。
async fn bootstrap() -> (PgPool, axum::Router, String, String, PartFixture) {
    let pool = test_pool().await;
    let fx = load_part_fixture(&pool).await;
    let app = test_app(test_state(pool.clone()).await);
    let manager = login_token(&app, PartFixture::MANAGER_USERNAME, PartFixture::PASSWORD).await;
    let inspector = login_token(&app, PartFixture::INSPECTOR_USERNAME, PartFixture::PASSWORD).await;
    (pool, app, manager, inspector, fx)
}

/// 断言 `data.changes` 恰好是给定的那一条。
fn assert_single_change(data: &Value, level: &str, id: i64, from: &str, to: &str) {
    let changes = data["changes"].as_array().expect("changes 必须是数组");
    assert_eq!(changes.len(), 1, "应恰好 1 条变化；got={changes:?}");
    let c = &changes[0];
    assert_eq!(c["level"], level);
    assert_eq!(c["id"], id.to_string(), "i64 主键序列化为 JSON string");
    assert_eq!(c["from"], from);
    assert_eq!(c["to"], to);
}

/// 1. part 侧漂移定点修正 + 幂等（并验证终态序列号释放被一并补做）。
#[tokio::test]
async fn recompute_rollup_fixes_drifted_part_and_is_idempotent() {
    let (pool, app, token, _inspector, fx) = bootstrap().await;
    // 漂移形态：**真源**（t_part_batch）已 COMPLETED，派生缓存（t_part）还 PENDING。
    // 这正是 status_gate 收口之前「漏调 sync」留下的历史形态。
    let part_id = insert_drifted_part(
        &pool,
        fx.customer_l2_id,
        "漂移件-对账",
        Some("RC-0001"),
        "PENDING",
        None,
    )
    .await;
    insert_batch_with_status(&pool, part_id, "COMPLETED").await;
    assert_eq!(part_status_str(&pool, part_id).await, "PENDING");

    // ---- 第一次：定点对账 ----
    let (st, body) = send(
        app.clone(),
        json_request(
            "POST",
            URL,
            Some(json!({ "part_ids": [part_id.to_string()] })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "body={body}");
    assert_eq!(body["code"], 0, "envelope code 应为 0");
    let data = &body["data"];
    assert_eq!(data["scope"], "PART_IDS");
    assert_eq!(data["parts_examined"], 1);
    assert_eq!(data["parts_changed"], 1);
    assert_eq!(
        data["assemblies_examined"], 0,
        "只给了 part_ids → 不该碰装配件"
    );
    assert_single_change(data, "PART", part_id, "PENDING", "COMPLETED");
    assert_eq!(part_status_str(&pool, part_id).await, "COMPLETED");

    // 派生进终态 → rollup step 4 的序列号释放也要一并补做（归档到 t_part_event）
    let (serial,): (Option<String>,) =
        sqlx::query_as::<_, (Option<String>,)>("SELECT serial_no FROM t_part WHERE id = $1")
            .bind(part_id)
            .fetch_one(&pool)
            .await
            .expect("read serial");
    assert_eq!(serial, None, "part 进 COMPLETED 后序列号应被释放");
    let archived: (i64,) = sqlx::query_as::<_, (i64,)>(
        "SELECT count(*) FROM t_part_event WHERE part_id = $1 AND event_type = 'SERIAL_RELEASED'",
    )
    .bind(part_id)
    .fetch_one(&pool)
    .await
    .expect("count serial released event");
    assert_eq!(archived.0, 1, "序列号释放应先归档一条事件");

    // ---- 第二次：**幂等** ----
    let (st, body) = send(
        app,
        json_request(
            "POST",
            URL,
            Some(json!({ "part_ids": [part_id.to_string()] })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "重复对账不得报错；body={body}");
    let data = &body["data"];
    assert_eq!(data["parts_examined"], 1, "仍会检查同一行");
    assert_eq!(data["parts_changed"], 0, "派生已收敛 → 0 变化（幂等契约）");
    assert_eq!(data["changes"].as_array().map(Vec::len), Some(0));
}

/// 2. 父装配件侧漂移（反向）：子件状态正确、父件没跟上。
#[tokio::test]
async fn recompute_rollup_fixes_drifted_assembly() {
    let (pool, app, token, _inspector, fx) = bootstrap().await;
    let asm_id = insert_drifted_assembly(&pool, fx.customer_l2_id, "PENDING").await;
    // 子件侧**没有**漂移：part 与 batch 都是 INSPECTION（自洽）
    let part_id = insert_drifted_part(
        &pool,
        fx.customer_l2_id,
        "自洽子件-对账",
        Some("RC-0002"),
        "INSPECTION",
        Some(asm_id),
    )
    .await;
    insert_batch_with_status(&pool, part_id, "INSPECTION").await;
    assert_eq!(
        assembly_status_str(&pool, asm_id).await,
        "PENDING",
        "造漂移：父件还 PENDING"
    );

    let (st, body) = send(
        app.clone(),
        json_request(
            "POST",
            URL,
            Some(json!({ "assembly_ids": [asm_id.to_string()] })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "body={body}");
    let data = &body["data"];
    assert_eq!(data["scope"], "ASSEMBLY_IDS");
    assert_eq!(
        data["parts_examined"], 0,
        "只给了 assembly_ids → 不该重算 part"
    );
    assert_eq!(data["assemblies_examined"], 1);
    assert_eq!(data["assemblies_changed"], 1);
    // 子件 INSPECTION → progress 4 → 7 态映射 4 => INSPECTION
    assert_single_change(data, "ASSEMBLY", asm_id, "PENDING", "INSPECTION");
    assert_eq!(assembly_status_str(&pool, asm_id).await, "INSPECTION");

    // 幂等：再跑一次必须是 0 变化
    let (st, body) = send(
        app,
        json_request(
            "POST",
            URL,
            Some(json!({ "assembly_ids": [asm_id.to_string()] })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "body={body}");
    assert_eq!(body["data"]["assemblies_changed"], 0, "幂等契约");
    assert_eq!(body["data"]["changes"].as_array().map(Vec::len), Some(0));
}

/// 3. RBAC：非 Manager 一律拒，且不得动数据。
#[tokio::test]
async fn recompute_rollup_requires_manager() {
    let (pool, app, _token, inspector, fx) = bootstrap().await;
    let part_id = insert_drifted_part(
        &pool,
        fx.customer_l2_id,
        "漂移件-越权",
        Some("RC-0003"),
        "PENDING",
        None,
    )
    .await;
    insert_batch_with_status(&pool, part_id, "COMPLETED").await;

    let (st, body) = send(
        app,
        json_request(
            "POST",
            URL,
            Some(json!({ "part_ids": [part_id.to_string()] })),
            Some(&inspector),
        ),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN, "body={body}");
    assert_eq!(body["code"], code::FORBIDDEN);
    assert_eq!(
        part_status_str(&pool, part_id).await,
        "PENDING",
        "被拒的请求不得改任何数据"
    );
}

/// 4. 不带 body（无 `Content-Type`）→ 全量对账。
///
/// axum 的 `Option<Json<T>>` 走 `OptionalFromRequest`：没有 `Content-Type` 头时
/// 返回 `None`，handler 把它当 `RecomputeRollupRequest::default()`（= 全量）。
#[tokio::test]
async fn recompute_rollup_without_body_is_full_scope() {
    let (pool, app, token, _inspector, fx) = bootstrap().await;
    // 造一条漂移，让全量扫描有活干
    let part_id = insert_drifted_part(
        &pool,
        fx.customer_l2_id,
        "漂移件-全量",
        None,
        "PENDING",
        None,
    )
    .await;
    insert_batch_with_status(&pool, part_id, "INSPECTION").await;

    let (st, body) = send(app.clone(), json_request("POST", URL, None, Some(&token))).await;
    assert_eq!(st, StatusCode::OK, "body={body}");
    assert_eq!(body["code"], 0);
    let data = &body["data"];
    assert_eq!(data["scope"], "ALL");
    assert!(data["parts_examined"].as_u64().unwrap() >= 1);
    assert_eq!(data["parts_changed"], 1, "全量扫描应发现并修正那 1 条漂移");
    assert_eq!(part_status_str(&pool, part_id).await, "INSPECTION");
    assert_eq!(data["truncated"], false, "fixture 量级远小于 limit");

    // 第二次全量：0 变化
    let (st, body) = send(app, json_request("POST", URL, None, Some(&token))).await;
    assert_eq!(st, StatusCode::OK, "body={body}");
    assert_eq!(body["data"]["parts_changed"], 0, "全量对账也必须幂等");
}

/// 5. `limit` 超上限 → 400 / 20104（防止单请求把整表锁在长事务里）。
#[tokio::test]
async fn recompute_rollup_rejects_limit_over_cap() {
    let (_pool, app, token, _inspector, _fx) = bootstrap().await;
    let (st, body) = send(
        app,
        json_request(
            "POST",
            URL,
            Some(json!({ "limit": 999_999i64 })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "body={body}");
    assert_eq!(body["code"], code::BIZ_INVALID_VALUE);
}

/// 6. 全量对账的**游标续扫**（2026-10-01 review 第 1 轮 M7，第 2 轮改名分表）。
///
/// 改造前 `list_part_ids` 只有 `ORDER BY id LIMIT limit+1`、**既无游标也无
/// offset**：`truncated = true` 时运维再调一次仍然从最小的 `limit` 行开始扫 ——
/// 一个兜底端点在生产数据量下永远兜不住底。
///
/// 断言链：
/// 1. `limit=1` → `truncated=true` 且 `next_part_after_id` 非 null；
/// 2. 把 `next_part_after_id` 回传为 `part_after_id` → `parts_examined` 落在**下一批**
///    id 上（与第 1 轮不重叠）；
/// 3. 一直续扫到 `truncated=false`，所有漂移都被修正（真收敛，不是原地打转）。
#[tokio::test]
async fn recompute_rollup_full_scope_advances_by_after_id_cursor() {
    let (pool, app, token, _inspector, fx) = bootstrap().await;
    // 造 3 条漂移，id 递增（next_test_id 单调）
    let mut drifted = Vec::new();
    for i in 0..3 {
        let pid = insert_drifted_part(
            &pool,
            fx.customer_l2_id,
            &format!("漂移件-游标-{i}"),
            None,
            "PENDING",
            None,
        )
        .await;
        insert_batch_with_status(&pool, pid, "INSPECTION").await;
        drifted.push(pid);
    }
    drifted.sort_unstable();

    let mut part_after_id: Option<String> = None;
    let mut rounds = 0usize;
    let mut seen: Vec<i64> = Vec::new();
    loop {
        rounds += 1;
        assert!(rounds <= 20, "续扫 20 轮仍未收敛 = 游标没生效");
        let body_json = match &part_after_id {
            Some(a) => json!({ "limit": 1, "part_after_id": a }),
            None => json!({ "limit": 1 }),
        };
        let (st, body) = send(
            app.clone(),
            json_request("POST", URL, Some(body_json), Some(&token)),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "round {rounds}: {body}");
        assert_eq!(body["code"], 0);
        let data = &body["data"];
        assert_eq!(data["scope"], "ALL", "round {rounds}");
        assert_eq!(
            data["parts_examined"], 1,
            "round {rounds}: limit=1 每轮只查 1 行"
        );
        for c in data["changes"].as_array().expect("changes") {
            if c["level"] == "PART" {
                seen.push(c["id"].as_str().unwrap().parse().unwrap());
            }
        }
        if data["truncated"] == false {
            break;
        }
        let next = data["next_part_after_id"]
            .as_str()
            .unwrap_or_else(|| {
                panic!("round {rounds}: truncated=true 必须回 next_part_after_id（本段取满了 limit 行）")
            })
            .to_string();
        if let Some(prev) = &part_after_id {
            assert!(
                next > *prev,
                "游标必须单调推进：{prev} → {next}（否则永远收敛不了）"
            );
        }
        part_after_id = Some(next);
    }
    seen.sort_unstable();
    seen.dedup();
    assert_eq!(
        seen, drifted,
        "续扫若干轮后，3 条漂移必须都被修正且各只报一次（真收敛而非原地打转）"
    );
    for pid in &drifted {
        assert_eq!(
            part_status_str(&pool, *pid).await,
            "INSPECTION",
            "part {pid} 应已被对账修正"
        );
    }
}

/// 7. **两表 id 交错**的全量续扫（2026-10-01 review 第 2 轮 MAJOR-2 回归）。
///
/// `t_part` 与 `t_assembly` 的 id 来自**同一个** `state.snowflake`（建父装配件与
/// 建子件都用它），两表 id 在时间序上**交错**。第 1 轮的实现让两段共用一个
/// `after_id`、回一个 `next_after_id = max(part_max, assembly_max)`：本用例的
/// id 布局是 `A1 < A2 < P1 < P2 < P3`，第 1 轮的单游标会直接跳到 `P1`，
/// 于是 **`A2` 永远扫不到**，而循环仍以 `truncated=false` 收尾 —— 报告谎称
/// 「已覆盖全表」。
///
/// 断言：
/// 1. 两个游标**独立**推进（响应同时给 `next_part_after_id` / `next_assembly_after_id`）；
/// 2. 续扫到 `truncated=false` 后，`A1`/`A2` 两条装配件漂移**都**被修正
///    （旧实现下 `A2` 仍是 PENDING ⇒ 本用例红）；
/// 3. 3 条 part 漂移也都被修正。
#[tokio::test]
async fn recompute_rollup_full_scope_covers_interleaved_part_and_assembly_ids() {
    let (pool, app, token, _inspector, fx) = bootstrap().await;
    // ⚠️ 插入顺序 = id 顺序（`next_test_id` 单调）：先两个**父装配件**，再它们的
    // 子件（子件要引用父件 id，DB 无物理外键，故 id 可以先于父件分配）。
    let asm1 = insert_drifted_assembly(&pool, fx.customer_l2_id, "PENDING").await;
    let asm2 = insert_drifted_assembly(&pool, fx.customer_l2_id, "PENDING").await;
    // 子件侧自洽（part 与 batch 都 INSPECTION）→ 父件派生值 = INSPECTION
    for (asm, tag) in [(asm1, "A1"), (asm2, "A2")] {
        let pid = insert_drifted_part(
            &pool,
            fx.customer_l2_id,
            &format!("自洽子件-{tag}"),
            None,
            "INSPECTION",
            Some(asm),
        )
        .await;
        insert_batch_with_status(&pool, pid, "INSPECTION").await;
    }
    // 第 3 条 part 自身漂移（part PENDING / batch INSPECTION）
    let drifted_part = insert_drifted_part(
        &pool,
        fx.customer_l2_id,
        "漂移件-交错",
        None,
        "PENDING",
        None,
    )
    .await;
    insert_batch_with_status(&pool, drifted_part, "INSPECTION").await;

    // 续扫：每轮 limit=1，两个游标各自推进
    let mut part_cursor: Option<String> = None;
    let mut asm_cursor: Option<String> = None;
    let mut rounds = 0usize;
    let mut seen_assemblies: Vec<i64> = Vec::new();
    loop {
        rounds += 1;
        assert!(rounds <= 20, "续扫 20 轮仍未收敛 = 游标没生效");
        let mut body_json = json!({ "limit": 1 });
        if let Some(c) = &part_cursor {
            body_json["part_after_id"] = json!(c);
        }
        if let Some(c) = &asm_cursor {
            body_json["assembly_after_id"] = json!(c);
        }
        let (st, body) = send(
            app.clone(),
            json_request("POST", URL, Some(body_json), Some(&token)),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "round {rounds}: {body}");
        let data = &body["data"];
        for c in data["changes"].as_array().expect("changes") {
            if c["level"] == "ASSEMBLY" {
                seen_assemblies.push(c["id"].as_str().unwrap().parse().unwrap());
            }
        }
        if data["truncated"] == false {
            break;
        }
        // 至少一段本轮取满了 limit 行 ⇒ 至少一个游标必须非 null 且前进
        let mut advanced = false;
        for (field, cursor) in [
            ("next_part_after_id", &mut part_cursor),
            ("next_assembly_after_id", &mut asm_cursor),
        ] {
            if let Some(next) = data[field].as_str() {
                if cursor.as_deref() != Some(next) {
                    advanced = true;
                }
                *cursor = Some(next.to_string());
            }
        }
        assert!(
            advanced,
            "round {rounds}: truncated=true 但两个游标都没前进 = 死循环"
        );
    }
    seen_assemblies.sort_unstable();
    seen_assemblies.dedup();
    let mut expected = vec![asm1, asm2];
    expected.sort_unstable();
    assert_eq!(
        seen_assemblies, expected,
        "两条装配件漂移都必须被修正（MAJOR-2：旧单游标会永久跳过 A2）"
    );
    assert_eq!(assembly_status_str(&pool, asm1).await, "INSPECTION");
    assert_eq!(
        assembly_status_str(&pool, asm2).await,
        "INSPECTION",
        "A2 的 id 落在 (asm1, part_max] 区间：共用游标时它永远扫不到"
    );
    assert_eq!(part_status_str(&pool, drifted_part).await, "INSPECTION");
}

/// 8. 终态守卫跳过的行必须**显式上报**（2026-10-01 review 第 2 轮 MAJOR-1 回归）。
///
/// 守卫命中时库里的值一个字节都没动，报告若不区分，运维看到的就是
/// `parts_examined=1 / parts_changed=0` —— 与「数据本来就一致」完全同形，
/// 即「兜底修数工具给假干净报告」。
///
/// 本用例一次调用里放两行：
/// - `terminal_part`：`t_part.status='COMPLETED'` 而批次还在 INSPECTION
///   （历史脏数据）→ 派生值 INSPECTION 被终态守卫拦下；
/// - `fixable_part`：part PENDING / 批次 INSPECTION → 正常被修正。
///
/// 断言：`parts_changed=1`（只有可修的那行）、`parts_skipped_terminal=1`、
/// `skipped_terminal[0] = {id, current: COMPLETED, derived: INSPECTION}`、
/// `changes` 里**只有**可修那条，且终态行的库值没被改。
#[tokio::test]
async fn recompute_rollup_reports_parts_skipped_by_terminal_guard() {
    let (pool, app, token, _inspector, fx) = bootstrap().await;
    let terminal_part = insert_drifted_part(
        &pool,
        fx.customer_l2_id,
        "终态但错-对账",
        Some("RC-T1"),
        "COMPLETED",
        None,
    )
    .await;
    insert_batch_with_status(&pool, terminal_part, "INSPECTION").await;
    let fixable_part = insert_drifted_part(
        &pool,
        fx.customer_l2_id,
        "可修-对账",
        Some("RC-T2"),
        "PENDING",
        None,
    )
    .await;
    insert_batch_with_status(&pool, fixable_part, "INSPECTION").await;

    let (st, body) = send(
        app.clone(),
        json_request(
            "POST",
            URL,
            Some(json!({
                "part_ids": [terminal_part.to_string(), fixable_part.to_string()],
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "body={body}");
    let data = &body["data"];
    assert_eq!(data["parts_examined"], 2);
    assert_eq!(data["parts_changed"], 1, "只有非终态那行可被修正");
    assert_eq!(
        data["parts_skipped_terminal"], 1,
        "终态被守卫跳过必须单独计数：{data}"
    );
    let skipped = data["skipped_terminal"]
        .as_array()
        .expect("skipped_terminal");
    assert_eq!(skipped.len(), 1);
    assert_eq!(skipped[0]["id"], terminal_part.to_string());
    assert_eq!(skipped[0]["current"], "COMPLETED");
    assert_eq!(
        skipped[0]["derived"], "INSPECTION",
        "min-progress 派生值应上抛"
    );
    // changes 里**只有**真被改的那行（被守卫跳过的行不进 changes）
    assert_single_change(data, "PART", fixable_part, "PENDING", "INSPECTION");
    // 终态行的值没被动过（守卫仍生效）
    assert_eq!(part_status_str(&pool, terminal_part).await, "COMPLETED");

    // 幂等：重跑仍是同一份报告（终态行仍被跳过、仍显式计数）
    let (st, body) = send(
        app,
        json_request(
            "POST",
            URL,
            Some(json!({
                "part_ids": [terminal_part.to_string(), fixable_part.to_string()],
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "重复对账不得报错；body={body}");
    let data = &body["data"];
    assert_eq!(data["parts_changed"], 0, "幂等契约");
    assert_eq!(
        data["parts_skipped_terminal"], 1,
        "被守卫跳过的行不因重跑而消失（它需要人工决策）"
    );
}

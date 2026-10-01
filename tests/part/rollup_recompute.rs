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

use std::sync::OnceLock;

use axum::http::StatusCode;
use serde_json::{Value, json};
use sqlx::PgPool;

use hsh_erp_rust::infra::clock::now_naive;
use hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator;
use hsh_erp_rust::shared::error::code;
use hsh_erp_test_support::{
    PartFixture, json_request, load_part_fixture, login_token, pool_snowflake, send, test_app,
    test_pool, test_state,
};

const URL: &str = "/admin/recompute-rollup";

/// 进程级共享雪花生成器（同 `tests/part/crud.rs` 的做法）。
///
/// 每次 `SnowflakeIdGenerator::new(...)` 只调一次 `next_id()` 会让同毫秒插入的
/// 多行拿到相同 ID（首次 `next_id()` 固定返回 `compose(now_ms, seq=0)`），
/// 表现为 `duplicate key ... t_process_pkey` 之类的偶发失败。单例让 sequence
/// 在进程内单调递增。
static SHARED_TEST_SNOWFLAKE: OnceLock<SnowflakeIdGenerator> = OnceLock::new();

fn next_test_id() -> i64 {
    SHARED_TEST_SNOWFLAKE
        .get_or_init(|| SnowflakeIdGenerator::new(1_577_836_800_000, 1))
        .next_id()
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

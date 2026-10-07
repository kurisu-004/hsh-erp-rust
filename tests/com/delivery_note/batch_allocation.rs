//! DP 批次分配算法的端到端测试（`POST /com/delivery/note/scan` 的分配层）
//!
//! 算法本体（`service/batch_allocation.rs`）有 10 个纯函数单测；本文件走真实 HTTP +
//! 真实数据库，验证「算法 + 拆批 + 挂单」三者在同一事务里拼起来的行为，特别是：
//!
//! 1. **零拆批优先**：target 恰等于某批数量 ⇒ 不拆批，直接挂原批次
//! 2. **多个零拆批解取字典序最小**：候选 `{2,3,5,10}` / target 10 ⇒ 取 `{2,3,5}`
//!    而不是 `{10}`（原则 2「优先入单小批」）
//! 3. **零拆批不可达时拆 1 个最大批次**：候选 `{3,4}` / target 5 ⇒ 排除 4、差额 1
//!    从 4 里拆
//! 4. **target 大于全部候选总量 ⇒ 21405**，不部分挂单
//! 5. 多个批次累加：target 跨批次时拆批后**总件数守恒**（`Σ 入单量 == target`）

use axum::http::StatusCode;
use serde_json::{Value, json};
use sqlx::PgPool;

use hsh_erp_test_support::{
    DeliveryFixture, json_request, load_delivery_fixture, login_token, send, test_app, test_pool,
    test_state,
};

use hsh_erp_rust::infra::clock::now_naive;
use hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator;

async fn bootstrap_as_manager() -> (PgPool, axum::Router, String, DeliveryFixture) {
    let pool = test_pool().await;
    let fx = load_delivery_fixture(&pool).await;
    let app = test_app(test_state(pool.clone()).await);
    let token = login_token(&app, &fx.part_manager_username, DeliveryFixture::PASSWORD).await;
    (pool, app, token, fx)
}

async fn insert_l1(pool: &PgPool, name: &str) -> i64 {
    let id = SnowflakeIdGenerator::new(1_577_836_800_000, 23).next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_customer (id, name, parent_id, serial_prefix, version, \
         created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, NULL, NULL, 0, $3, NULL, $3, NULL)",
    )
    .bind(id)
    .bind(name)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert L1");
    id
}

async fn insert_l2(pool: &PgPool, name: &str, l1_id: i64) -> i64 {
    let id = SnowflakeIdGenerator::new(1_577_836_800_000, 23).next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_customer (id, name, parent_id, serial_prefix, version, \
         created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, $3, NULL, 0, $4, NULL, $4, NULL)",
    )
    .bind(id)
    .bind(name)
    .bind(l1_id)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert L2");
    id
}

async fn insert_part(pool: &PgPool, name: &str, serial_no: &str, customer_id: i64) -> i64 {
    let id = SnowflakeIdGenerator::new(1_577_836_800_000, 23).next_id();
    let now = now_naive();
    let today = now.date();
    sqlx::query(
        "INSERT INTO t_part (id, serial_no, name, drawing_no, customer_id, status, \
         applicant_name, request_date, planned_delivery_date, quantity, version, \
         created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, $3, 'D-DP', $4, 'READY_TO_SHIP', 'DP 测试', $5, $5, 100, 0, \
         $6, NULL, $6, NULL)",
    )
    .bind(id)
    .bind(Some(serial_no.to_string()))
    .bind(name)
    .bind(customer_id)
    .bind(today)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert part");
    id
}

/// 造一组 `READY_TO_SHIP` 且未占用的批次（`quantities[i]` ⇒ `batch_no = i + 1`）。
async fn insert_batches(pool: &PgPool, part_id: i64, quantities: &[i32]) -> Vec<i64> {
    let mut ids = Vec::with_capacity(quantities.len());
    for (i, q) in quantities.iter().enumerate() {
        let id = SnowflakeIdGenerator::new(1_577_836_800_000, 23).next_id();
        let now = now_naive();
        sqlx::query(
            "INSERT INTO t_part_batch (id, part_id, batch_no, quantity, status, location, \
             delivery_note_id, version, created_at, created_by, updated_at, updated_by) \
             VALUES ($1, $2, $3, $4, 'READY_TO_SHIP', 'PRODUCTION_SHELF', NULL, 0, $5, \
             NULL, $5, NULL)",
        )
        .bind(id)
        .bind(part_id)
        .bind(i as i32 + 1)
        .bind(q)
        .bind(now)
        .execute(pool)
        .await
        .expect("insert batch");
        ids.push(id);
    }
    ids
}

async fn scan_entry(
    app: &axum::Router,
    token: &str,
    serial_no: &str,
    part_id: i64,
    quantity: i32,
) -> (StatusCode, Value) {
    send(
        app.clone(),
        json_request(
            "POST",
            "/com/delivery/note/scan",
            Some(json!({
                "serial_no": serial_no,
                "note_version": null,
                "entries": [{"node_kind": "PART", "node_id": part_id.to_string(), "quantity": quantity}],
            })),
            Some(token),
        ),
    )
    .await
}

/// 本单上行项的件数合计。
fn total_on_note(data: &Value) -> i64 {
    data["line_items"]
        .as_array()
        .expect("line_items 必须是数组")
        .iter()
        .map(|li| li["quantity"].as_i64().unwrap_or(0))
        .sum()
}

// ===========================================================================
//  1. 零拆批优先
// ===========================================================================

/// target 恰等于某批数量 ⇒ 零拆批：行项里就是原批次 id，库内不新增批次。
#[tokio::test]
async fn exact_batch_match_is_preferred_over_splitting() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let l1 = insert_l1(&pool, "DP 零拆批").await;
    let l2 = insert_l2(&pool, "DP 零拆批二厂", l1).await;
    let part = insert_part(&pool, "零拆批件", "DP-A1", l2).await;
    let ids = insert_batches(&pool, part, &[2, 3, 9, 10]).await;

    // 规格例 1：target 10，唯一零拆批解是那批 10 件的
    let (s, env) = scan_entry(&app, &token, "DP-A1", part, 10).await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(
        env["data"]["line_items"].as_array().unwrap().len(),
        1,
        "零拆批 ⇒ 只有 1 行: {env}"
    );
    let picked = env["data"]["line_items"][0]["id"].as_str().unwrap();
    assert_eq!(
        picked,
        ids[3].to_string(),
        "必须选恰好 10 件的那批（原则 1：如无必要不拆批）"
    );

    let n: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM t_part_batch WHERE part_id = $1 AND deleted_at IS NULL",
    )
    .bind(part)
    .fetch_one(&pool)
    .await
    .expect("count");
    assert_eq!(n, 4, "零拆批不新增批次");
}

// ===========================================================================
//  2. 多个零拆批解 → 字典序最小
// ===========================================================================

/// 规格例 2：候选 `{2,3,5,10}` / target 10 有两个零拆批解（`{10}` 与 `{2,3,5}`），
/// 取**升序数量序列字典序更小**的 `{2,3,5}`（原则 2：优先入单小批）。
#[tokio::test]
async fn multiple_exact_solutions_pick_smallest_batches() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let l1 = insert_l1(&pool, "DP 小批优先").await;
    let l2 = insert_l2(&pool, "DP 小批优先二厂", l1).await;
    let part = insert_part(&pool, "小批优先件", "DP-A2", l2).await;
    let ids = insert_batches(&pool, part, &[2, 3, 5, 10]).await;

    let (s, env) = scan_entry(&app, &token, "DP-A2", part, 10).await;
    assert_eq!(s, StatusCode::OK, "{env}");
    let items = env["data"]["line_items"].as_array().unwrap();
    assert_eq!(items.len(), 3, "取 3 个小批而不是 1 个大批: {env}");
    let picked: Vec<String> = items
        .iter()
        .map(|li| li["id"].as_str().unwrap().to_string())
        .collect();
    let mut expected = vec![ids[0].to_string(), ids[1].to_string(), ids[2].to_string()];
    expected.sort();
    let mut got = picked.clone();
    got.sort();
    assert_eq!(
        got, expected,
        "必须取 {{2, 3, 5}} 三个小批，而不是 {{10}} 那一个大批"
    );
    assert!(
        !picked.contains(&ids[3].to_string()),
        "10 件那批不该被选中: {env}"
    );
    assert_eq!(total_on_note(&env["data"]), 10, "入单总数守恒");
}

// ===========================================================================
//  3. 零拆批不可达 → 拆 1 个最大批次
// ===========================================================================

/// 候选 `{3, 4}` / target 5：没有子集和为 5 ⇒ 排除最大的 4、其余凑 3、差额 2 从 4 拆。
///
/// 拆批语义（照 `PartBatchRepo::split_batch`）：新批次 `quantity = qty`、随后挂单；
/// 源批次 `quantity -= qty`、**保持未挂单**。
#[tokio::test]
async fn unreachable_exact_match_splits_largest_batch_once() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let l1 = insert_l1(&pool, "DP 拆批").await;
    let l2 = insert_l2(&pool, "DP 拆批二厂", l1).await;
    let part = insert_part(&pool, "拆批件", "DP-A3", l2).await;
    let ids = insert_batches(&pool, part, &[3, 4]).await;

    let (s, env) = scan_entry(&app, &token, "DP-A3", part, 5).await;
    assert_eq!(s, StatusCode::OK, "{env}");
    let items = env["data"]["line_items"].as_array().unwrap();
    assert_eq!(items.len(), 2, "3 件整批 + 2 件拆批: {env}");
    assert_eq!(total_on_note(&env["data"]), 5, "入单总数 == target");

    // 3 件那批整批挂单
    let three = items
        .iter()
        .find(|li| li["quantity"] == 3)
        .expect("应有 3 件行");
    assert_eq!(three["id"].as_str().unwrap(), ids[0].to_string());
    // 4 件那批拆成 2
    let two = items
        .iter()
        .find(|li| li["quantity"] == 2)
        .expect("应有 2 件行");
    assert_ne!(
        two["id"].as_str().unwrap(),
        ids[1].to_string(),
        "2 件那行必须是拆出来的新批次"
    );
    // 源批次 4 → 2 且未挂单
    let (qty, note_of_src): (i32, Option<i64>) =
        sqlx::query_as("SELECT quantity, delivery_note_id FROM t_part_batch WHERE id = $1")
            .bind(ids[1])
            .fetch_one(&pool)
            .await
            .expect("read source batch");
    assert_eq!(qty, 2, "源批次数量被减去差额");
    assert!(note_of_src.is_none(), "源批次不该被挂单");
    let (qty2, note2): (i32, Option<i64>) =
        sqlx::query_as("SELECT quantity, delivery_note_id FROM t_part_batch WHERE id = $1")
            .bind(two["id"].as_str().unwrap().parse::<i64>().unwrap())
            .fetch_one(&pool)
            .await
            .expect("read split batch");
    assert_eq!(qty2, 2);
    assert!(note2.is_some(), "拆出的批次必须挂单");
}

// ===========================================================================
//  4. 不可行
// ===========================================================================

/// target 大于全部候选总量 ⇒ 21405，零写入（不做「先挂能挂的」）。
#[tokio::test]
async fn target_beyond_total_is_rejected_without_partial_write() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let l1 = insert_l1(&pool, "DP 不可行").await;
    let l2 = insert_l2(&pool, "DP 不可行二厂", l1).await;
    let part = insert_part(&pool, "不可行件", "DP-A4", l2).await;
    insert_batches(&pool, part, &[3, 4]).await;

    let (s, env) = scan_entry(&app, &token, "DP-A4", part, 11).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{env}");
    assert_eq!(env["code"], 21405);
    let attached: i64 =
        sqlx::query_scalar("SELECT count(*) FROM t_part_batch WHERE delivery_note_id IS NOT NULL")
            .fetch_one(&pool)
            .await
            .expect("count");
    assert_eq!(attached, 0, "不可行时零写入");
}

// ===========================================================================
//  5. 累加
// ===========================================================================

/// 同一零件第二次入单：DP 在**剩余可入单批次**上再跑一次，两次入单累加、总量守恒。
#[tokio::test]
async fn second_entry_runs_dp_on_remaining_batches_and_totals_conserve() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let l1 = insert_l1(&pool, "DP 累加").await;
    let l2 = insert_l2(&pool, "DP 累加二厂", l1).await;
    let part = insert_part(&pool, "累加件", "DP-A5", l2).await;
    insert_batches(&pool, part, &[6, 7]).await;

    let (s1, env1) = scan_entry(&app, &token, "DP-A5", part, 6).await;
    assert_eq!(s1, StatusCode::OK, "{env1}");
    let v = env1["data"]["version"].as_i64().unwrap() as i32;

    let (s2, env2) = send(
        app.clone(),
        json_request(
            "POST",
            "/com/delivery/note/scan",
            Some(json!({
                "serial_no": "DP-A5",
                "note_version": v,
                "entries": [{"node_kind": "PART", "node_id": part.to_string(), "quantity": 7}],
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s2, StatusCode::OK, "第二次入单: {env2}");
    assert_eq!(
        env2["data"]["id"].as_str().unwrap(),
        env1["data"]["id"].as_str().unwrap(),
        "同 L1 复用同一张单"
    );
    assert_eq!(total_on_note(&env2["data"]), 13, "两次入单累加：6 + 7");
}

/// 跨批次凑数：候选 `{4, 6, 9}` / target 15 ⇒ 零拆批解 `{6, 9}`，不拆批。
#[tokio::test]
async fn cross_batch_sum_needs_no_split() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let l1 = insert_l1(&pool, "DP 跨批").await;
    let l2 = insert_l2(&pool, "DP 跨批二厂", l1).await;
    let part = insert_part(&pool, "跨批件", "DP-A6", l2).await;
    let ids = insert_batches(&pool, part, &[4, 6, 9]).await;

    let (s, env) = scan_entry(&app, &token, "DP-A6", part, 15).await;
    assert_eq!(s, StatusCode::OK, "{env}");
    let items = env["data"]["line_items"].as_array().unwrap();
    assert_eq!(items.len(), 2, "零拆批解是两个批次（6 + 9）: {env}");
    let picked: Vec<String> = items
        .iter()
        .map(|li| li["id"].as_str().unwrap().to_string())
        .collect();
    assert!(picked.contains(&ids[1].to_string()), "6 件那批: {env}");
    assert!(picked.contains(&ids[2].to_string()), "9 件那批: {env}");
    assert!(
        !picked.contains(&ids[0].to_string()),
        "4 件那批不该选: {env}"
    );
    let n: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM t_part_batch WHERE part_id = $1 AND deleted_at IS NULL",
    )
    .bind(part)
    .fetch_one(&pool)
    .await
    .expect("count");
    assert_eq!(n, 3, "零拆批不新增批次");
}

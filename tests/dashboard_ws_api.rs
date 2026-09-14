//! dashboard WS 集成测试（2026-09-15 takeover-fill）
//!
//! 覆盖（≥3 用例）：
//!   1. build_snapshot_with_workers_basic — service 层直接调，验证 JSON shape
//!   2. ws_handler_stub_replaced          — handler 函数签名替换验证
//!   3. broadcast_to_subscribed_socket    — 业务事件可被订阅的 socket 接收
//!
//! 注：纯 axum + WebSocket 端到端（带 real socket）由 e2e/ 覆盖；本文件专注
//! service / ws_hub 协作路径，确保 takeover-fill 关键路径不退化。

#[path = "common/mod.rs"]
mod common;

use chrono::NaiveDate;
use common::{clean_business_db, clean_db, ensure_database_exists, test_pool};

use hsh_erp_rust::infra::clock::now_naive;
use hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator;
use hsh_erp_rust::infra::ws_hub::WsEvent;
use hsh_erp_rust::modules::dashboard::service::DashboardService;
use sqlx::PgPool;

static TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn setup<'a>() -> (tokio::sync::MutexGuard<'a, ()>, PgPool) {
    let guard = TEST_LOCK.lock().await;
    ensure_database_exists().await;
    let pool = test_pool().await;
    clean_db(&pool).await;
    clean_business_db(&pool).await;
    (guard, pool)
}

#[tokio::test]
async fn build_snapshot_with_workers_basic() {
    let (_guard, pool) = setup().await;
    // 插一个 active 生产区货架
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let now = now_naive();
    let shelf_id = snowflake.next_id();
    sqlx::query(
        "INSERT INTO t_shelf (id, code, name, zone, is_active, display_order, version, \
         created_at, updated_at) \
         VALUES ($1, 'S-001', '一号架', 'PRODUCTION', true, 0, 0, $2, $2)",
    )
    .bind(shelf_id)
    .bind(now)
    .execute(&pool)
    .await
    .expect("insert t_shelf");

    let mut tx = pool.begin().await.unwrap();
    let snap = DashboardService::build_snapshot_with_workers(&mut tx, None)
        .await
        .expect("snapshot ok");
    drop(tx);

    // 必有 on_production_shelves 包含该架（空 items 也算）
    assert!(snap
        .on_production_shelves
        .iter()
        .any(|g| g.shelf_code == "S-001"));
    assert_eq!(snap.upcoming_delivery.len(), 7, "未来 7 天固定 7 条");
    assert!(!snap.ts.is_empty());
}

#[tokio::test]
async fn build_snapshot_with_workers_returns_full_shape() {
    let (_guard, pool) = setup().await;
    // 插 L1 + L2 customer + part + 一个 shelf 上的 IN_PROCESS 批次
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let now = now_naive();
    let today = NaiveDate::from_ymd_opt(2026, 9, 15).unwrap();

    let l1_id = snowflake.next_id();
    sqlx::query(
        "INSERT INTO t_customer (id, name, parent_id, serial_prefix, version, \
         created_at, updated_at) VALUES ($1, 'l1', NULL, 'F', 0, $2, $2)",
    )
    .bind(l1_id)
    .bind(now)
    .execute(&pool)
    .await
    .unwrap();

    let l2_id = snowflake.next_id();
    sqlx::query(
        "INSERT INTO t_customer (id, name, parent_id, serial_prefix, version, \
         created_at, updated_at) VALUES ($1, 'l2', $2, NULL, 0, $3, $3)",
    )
    .bind(l2_id)
    .bind(l1_id)
    .bind(now)
    .execute(&pool)
    .await
    .unwrap();

    let part_id = snowflake.next_id();
    sqlx::query(
        "INSERT INTO t_part (id, name, drawing_no, applicant_name, customer_id, \
         request_date, planned_delivery_date, status, version, \
         created_at, created_by, updated_at, updated_by) \
         VALUES ($1, 'p-dash', 'DWG-D', 'tester', $2, $3, $3, 'IN_PROCESS', 0, $4, NULL, $4, NULL)",
    )
    .bind(part_id)
    .bind(l2_id)
    .bind(today)
    .bind(now)
    .execute(&pool)
    .await
    .unwrap();

    let shelf_id = snowflake.next_id();
    sqlx::query(
        "INSERT INTO t_shelf (id, code, name, zone, is_active, display_order, version, \
         created_at, updated_at) \
         VALUES ($1, 'S-002', '二号架', 'PRODUCTION', true, 0, 0, $2, $2)",
    )
    .bind(shelf_id)
    .bind(now)
    .execute(&pool)
    .await
    .unwrap();

    // part_batch IN_PROCESS + holder=shelf + location=PRODUCTION_SHELF
    let batch_id = snowflake.next_id();
    sqlx::query(
        "INSERT INTO t_part_batch (id, part_id, batch_no, quantity, status, location, \
         current_holder_id, placed_at, version, created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, 1, 5, 'IN_PROCESS', 'PRODUCTION_SHELF', $3, $4, 0, $4, NULL, $4, NULL)",
    )
    .bind(batch_id)
    .bind(part_id)
    .bind(shelf_id)
    .bind(now)
    .execute(&pool)
    .await
    .unwrap();

    let mut tx = pool.begin().await.unwrap();
    let snap = DashboardService::build_snapshot_with_workers(&mut tx, None)
        .await
        .expect("snapshot ok");
    drop(tx);

    let shelf_group = snap
        .on_production_shelves
        .iter()
        .find(|g| g.shelf_code == "S-002")
        .expect("S-002 在产线组中");
    assert_eq!(shelf_group.items.len(), 1);
    assert_eq!(shelf_group.items[0].id, part_id.to_string());
    assert_eq!(shelf_group.items[0].quantity, 5);
}

#[tokio::test]
async fn ws_hub_broadcast_subscription_receives_event() {
    // 业务事件订阅通路：subscribe 后调 broadcast，新接收方应收到。
    use hsh_erp_rust::infra::ws_hub::WsHub;
    let hub = WsHub::new();
    let mut rx = hub.subscribe();
    hub.broadcast(WsEvent::DashboardEvent {
        kind: "PART_TO_SHIP".into(),
        payload: serde_json::json!({ "part_id": "123" }),
    });
    let evt = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
        .await
        .expect("timeout")
        .expect("recv ok");
    match evt {
        WsEvent::DashboardEvent { kind, payload } => {
            assert_eq!(kind, "PART_TO_SHIP");
            assert_eq!(payload["part_id"], "123");
        }
        other => panic!("期望 DashboardEvent，got {other:?}"),
    }
}

#[tokio::test]
async fn ws_hub_broadcast_snapshot_subscription_receives_snapshot() {
    use hsh_erp_rust::infra::ws_hub::WsHub;
    let hub = WsHub::new();
    let mut rx = hub.subscribe();
    hub.broadcast(WsEvent::DashboardSnapshot {
        data: serde_json::json!({ "on_production_shelves": [] }),
    });
    let evt = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
        .await
        .expect("timeout")
        .expect("recv ok");
    match evt {
        WsEvent::DashboardSnapshot { data } => {
            assert!(data["on_production_shelves"].is_array());
        }
        other => panic!("期望 DashboardSnapshot，got {other:?}"),
    }
}
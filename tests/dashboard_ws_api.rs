//! dashboard WS 集成测试（2026-09-15 takeover-fill + followup-cleanup A4）
//!
//! 覆盖：
//!   service / ws_hub 协作（takeover-fill）：
//!     1. build_snapshot_with_workers_basic — service 层直接调，验证 JSON shape
//!     2. build_snapshot_with_workers_returns_full_shape — shape 含 batch_no / batch_id
//!     3. ws_hub_broadcast_subscription_receives_event — 业务事件订阅通路
//!     4. ws_hub_broadcast_snapshot_subscription_receives_snapshot — snapshot 订阅通路
//!
//!   真实 socket E2E（followup-cleanup A4）：
//!     5. ws_e2e_invalid_token_rejected        — 40101（JWT 验签失败）/ 40100（缺 token）
//!     6. ws_e2e_valid_token_receives_snapshot — 握手后 ≤ 5s 收首条 snapshot text
//!     7. ws_e2e_valid_token_receives_heartbeat_text — ≤ 心跳间隔 + 5s 收 WsHeartbeatMsg text
//!                                                  （非 protocol-level Ping 帧；
//!                                                  浏览器 JS `onmessage` 可直接收到）

#[path = "common/mod.rs"]
mod common;

use chrono::NaiveDate;
use common::{
    clean_business_db, clean_db, ensure_database_exists, test_pool, test_state, test_ws_app,
};

use futures_util::StreamExt;
use hsh_erp_rust::auth::jwt::encode_access;
use hsh_erp_rust::auth::rbac::{Claims, Role};
use hsh_erp_rust::auth::session::hash_token;
use hsh_erp_rust::infra::clock::now_naive;
use hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator;
use hsh_erp_rust::infra::ws_hub::WsEvent;
use hsh_erp_rust::modules::dashboard::service::DashboardService;
use sqlx::PgPool;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message as WsMessage;

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
    assert!(
        snap.on_production_shelves
            .iter()
            .any(|g| g.shelf_code == "S-001")
    );
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
    // 2026-09-16 PR-3 批次 step 化：t_part_batch 删 `placed_at` 列，INSERT 列名/占位符同步移除。
    let batch_id = snowflake.next_id();
    sqlx::query(
        "INSERT INTO t_part_batch (id, part_id, batch_no, quantity, status, location, \
         current_holder_id, version, created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, 1, 5, 'IN_PROCESS', 'PRODUCTION_SHELF', $3, 0, $4, NULL, $4, NULL)",
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
    // 2026-09-15 review 修：batch_no 必须从 SQL 传到 DTO（之前硬编 None）
    assert_eq!(
        shelf_group.items[0].batch_no,
        Some(1),
        "batch_no 应为 INSERT 时填的 1，不应为 None"
    );
    assert_eq!(
        shelf_group.items[0].batch_id.as_deref(),
        Some(batch_id.to_string().as_str())
    );
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

// ===========================================================================
// 2026-09-15 followup-cleanup A4：dashboard WS 真实 socket E2E
// ===========================================================================

/// 启动 axum 服务端（绑定 127.0.0.1:0 随机端口），返回 (`base_url`, `state`)。
async fn spawn_ws_server() -> (String, Arc<hsh_erp_rust::state::AppState>) {
    let (_guard, pool) = setup().await;
    let state = test_state(pool.clone()).await;
    let app = test_ws_app(state.clone());
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind random port");
    let addr = listener.local_addr().expect("local_addr");
    // graceful_shutdown 等不到 cancel 时持续；这里用 select 包一层 on cancel drop。
    let shutdown = state.shutdown.clone();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app)
            .with_graceful_shutdown(async move { shutdown.cancelled().await })
            .await;
    });
    (format!("ws://127.0.0.1:{}", addr.port()), state)
}

/// 签发合法 access token + 写入 Redis session，使 dashboard WS 握手通过。
async fn mint_test_token(state: &Arc<hsh_erp_rust::state::AppState>, user_id: i64) -> String {
    use hsh_erp_rust::auth::session::{CachedCurrentUser, TokenKind};
    let claims = Claims {
        sub: user_id,
        username: "ws-tester".to_string(),
        roles: vec![Role::Manager],
        shelf_ids: vec![],
        shelf_wildcard: true,
        ver: 0,
        typ: "access".into(),
        iss: state.config.jwt.issuer.clone(),
        exp: 0, // 由 encode_access 覆盖
    };
    let (token, _exp) = encode_access(
        &claims,
        &state.config.jwt.secret,
        &state.config.jwt.issuer,
        state.config.jwt.access_ttl_hours,
    )
    .expect("encode_access");
    // 写 Redis session，让 session_check_enabled=true 时 ws_dashboard 不返 40105。
    let cached = CachedCurrentUser {
        id: user_id,
        username: "ws-tester".to_string(),
        roles: vec!["MANAGER".into()],
        shelf_ids: vec![],
        shelf_wildcard: true,
    };
    state
        .session
        .create_session(
            &hash_token(&token),
            user_id,
            TokenKind::Access,
            state.config.redis.session_ttl_seconds,
            &cached,
        )
        .await
        .expect("create_session");
    token
}

#[tokio::test]
async fn ws_e2e_invalid_token_rejected() {
    let (base, _state) = spawn_ws_server().await;
    let url = format!("{base}/dashboard?token=not-a-jwt-at-all");
    // 用 connect_async 返回的 HTTP response 状态码断言 401（UNAUTHORIZED）
    let result = tokio_tungstenite::connect_async(&url).await;
    match result {
        Err(tokio_tungstenite::tungstenite::Error::Http(resp)) => {
            assert_eq!(
                resp.status(),
                axum::http::StatusCode::UNAUTHORIZED,
                "非法 JWT 应返 401"
            );
        }
        Err(other) => panic!("期望 HTTP 401 错误，got tungstenite err: {other:?}"),
        Ok(_) => panic!("不应成功升级 WS"),
    }
}

#[tokio::test]
async fn ws_e2e_valid_token_receives_snapshot() {
    let (base, state) = spawn_ws_server().await;
    // 插一个 active 货架，让 snapshot 非空
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_shelf (id, code, name, zone, is_active, display_order, version, \
         created_at, updated_at) \
         VALUES ($1, 'S-WS1', 'WS一号架', 'PRODUCTION', true, 0, 0, $2, $2)",
    )
    .bind(snowflake.next_id())
    .bind(now)
    .execute(&state.pool)
    .await
    .expect("insert t_shelf");

    let user_id = snowflake.next_id();
    let token = mint_test_token(&state, user_id).await;
    let url = format!("{base}/dashboard?token={token}");

    let (mut ws, _resp) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("WS upgrade must succeed for valid token");

    // 收首条 snapshot，≤ 5s
    let frame = tokio::time::timeout(Duration::from_secs(5), ws.next())
        .await
        .expect("snapshot 超时")
        .expect("ws stream closed")
        .expect("ws frame err");
    let text = match frame {
        WsMessage::Text(t) => t,
        other => panic!("首条 frame 应为 text，got {other:?}"),
    };
    let v: serde_json::Value = serde_json::from_str(&text).expect("snapshot JSON parse");
    assert_eq!(v["type"], "snapshot", "首条 frame 应为 snapshot envelope");
    assert!(v["data"]["on_production_shelves"].is_array());
    assert!(
        !v["data"]["upcoming_delivery"]
            .as_array()
            .unwrap()
            .is_empty()
    );

    // 主动关 socket 避免 graceful_shutdown 死等
    let _ = ws.close(None).await;
}

#[tokio::test]
async fn ws_e2e_valid_token_receives_heartbeat_text() {
    let (base, state) = spawn_ws_server().await;
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let user_id = snowflake.next_id();
    let token = mint_test_token(&state, user_id).await;
    let url = format!("{base}/dashboard?token={token}");

    let (mut ws, _resp) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("WS upgrade must succeed for valid token");

    // 收首条 snapshot（先把它消耗掉）
    let _ = tokio::time::timeout(Duration::from_secs(5), ws.next())
        .await
        .expect("snapshot 超时")
        .expect("ws stream closed")
        .expect("ws frame err");

    // 等心跳：测试 config 把 ws_heartbeat_interval_seconds 设为 1；
    // 给 1s + 5s slack 总 6s 上限避免 CI 抖动。期望收到 `WsHeartbeatMsg` text 帧
    // （**不是** protocol-level Ping 帧）。
    let heartbeat_interval = state.config.ws_heartbeat_interval_seconds;
    let wait = Duration::from_secs(heartbeat_interval + 5);
    let mut got_heartbeat = false;
    let deadline = tokio::time::Instant::now() + wait;
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(500), ws.next()).await {
            Ok(Some(Ok(WsMessage::Text(text)))) => {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text)
                    && v["type"] == "heartbeat"
                    && v["ts"].is_number()
                {
                    got_heartbeat = true;
                    break;
                }
            }
            Ok(Some(Ok(WsMessage::Ping(_)))) => {
                panic!("不应再收到 protocol-level Ping 帧；心跳应走 text（followup A6）");
            }
            Ok(Some(Ok(WsMessage::Pong(_)))) => continue,
            Ok(Some(Ok(WsMessage::Close(_)))) => break,
            Ok(Some(Ok(_))) => continue, // binary / 其它
            Ok(Some(Err(e))) => panic!("ws frame err: {e}"),
            Ok(None) => break,
            Err(_) => continue, // 500ms 内无帧 → 继续轮询直到 deadline
        }
    }
    let _ = ws.close(None).await;
    assert!(
        got_heartbeat,
        "未在 {wait:?} 内收到 heartbeat text 帧（interval={heartbeat_interval}s）"
    );
}

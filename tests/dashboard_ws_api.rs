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
//!     7. ws_e2e_valid_token_receives_heartbeat_text — ≤ 心跳间隔 + 5s 同时收齐
//!                                                  ① `WsHeartbeatMsg` text 帧（前端 JS
//!                                                  `onmessage` 感知）② 服务端 protocol-level
//!                                                  Ping（2026-10-01 B4：服务端判活用，JS 不可见）
//!                                                  ③ 客户端 Ping 的 Pong 回声
//!
//!   WS 健壮性加固 E2E（2026-10-01 B1-B4）：
//!    12. ws_e2e_lagged_client_gets_4003_close  — 慢消费方 Lagged → 4003 lagged Close 帧
//!    13. ws_e2e_pong_timeout_closes_dead_peer  — 不回任何帧 → 1011 pong timeout Close 帧
//!    14. ws_e2e_conn_registry_counts           — 连接表 register/unregister 计数
//!    15. ws_e2e_server_shutdown_sends_1012     — shutdown.cancel() → 1012 server restart
//!
//!   HTTP `GET /api/v2/dashboard/snapshot` 集成测试（2026-09-28 新增 + 2026-09-30 扩 query）：
//!     8. http_snapshot_unauthenticated_returns_401   — 无 Bearer token 应返 401（中间件）
//!     9. http_snapshot_happy_path_returns_full_shape  — 登录后 GET 返回 200 + 完整 shape（默认 14 天）
//!    10. http_snapshot_default_14_days_returns_14_buckets — 缺省 ?upcoming_days → 14 条桶
//!    11. http_snapshot_custom_7_days_returns_7_buckets   — ?upcoming_days=7 → 7 条桶（向后兼容老契约）
//!
//! 测试栈：必须建 Redis pool，session 写入才算「已吊销」
//!
//! ## Fixture 范本化（2026-09-24 PR13 Phase I）
//! 本文件原 `#[path = "common/mod.rs"] mod common;` + `use common::{...};` 改走
//! `use hsh_erp_test_support::*` + `load_dashboard_ws_fixture(&pool)` +
//! `DashboardWsFixture` + 局部 helper。fixture 提供 1 WS 验签 user baseline；
//! snapshot 数据（t_customer / t_part / t_part_batch / t_shelf）每个用例现场插，
//! 避免 fixture 占用 shelf code / customer prefix 字面与测试现场冲突（snapshot
//! 按 shelf.code 查找）。

use chrono::NaiveDate;
use futures_util::{SinkExt, StreamExt};
use hsh_erp_rust::auth::jwt::encode_access;
use hsh_erp_rust::infra::clock::now_naive;
use hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator;
use hsh_erp_rust::infra::ws_hub::WsEvent;
use hsh_erp_rust::modules::dashboard::service::DashboardService;
use hsh_erp_test_support::{
    DashboardWsFixture, json_request, load_dashboard_ws_fixture, send as ts_send, test_app,
    test_pool, test_state, test_ws_app,
};
use sqlx::PgPool;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;

async fn setup() -> PgPool {
    let pool = test_pool().await;
    let _fx = load_dashboard_ws_fixture(&pool).await;
    pool
}

#[tokio::test]
async fn build_snapshot_with_workers_basic() {
    let pool = setup().await;
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
    // 2026-09-22 Group E 重构：`build_snapshot_with_workers` 改 `<R: DashboardRepoTrait>(&self, mut repo: R)`
    // by-value；`DashboardService` 是 unit struct，`DashboardService::new()` 构造实例；
    // handler / 直调方都借 `&mut *tx` 喂给 trait（trait 已直接 `impl for &mut PgConnection`，
    // `Transaction` deref 到 `PgConnection`）。
    // 2026-09-30 新增 days 形参（默认 14）：service 层兜底 unwrap_or(14).clamp(1, 60)；
    // service-level 直调沿用 `None` 走默认 14 天，与 HTTP 端点缺省值对齐。
    let snap = DashboardService::new()
        .build_snapshot_with_workers(&mut *tx, None, None)
        .await
        .expect("snapshot ok");
    drop(tx);

    // 必有 on_production_shelves 包含该架（空 items 也算）
    assert!(
        snap.on_production_shelves
            .iter()
            .any(|g| g.shelf_code == "S-001")
    );
    // 2026-09-30 修改：原 7 天写死改为 service 默认 14 天（None → 14）
    assert_eq!(snap.upcoming_delivery.len(), 14, "默认 14 天固定 14 条");
    assert!(!snap.ts.is_empty());
}

#[tokio::test]
async fn build_snapshot_with_workers_returns_full_shape() {
    let pool = setup().await;
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
    // 2026-09-22 Group E 重构：`build_snapshot_with_workers` 改 `<R: DashboardRepoTrait>(&self, mut repo: R)`
    // by-value；handler / 直调方都借 `&mut *tx` 喂给 trait（trait 已直接 `impl for &mut PgConnection`）。
    // 2026-09-30 新增 days 形参：本用例继续 None 走默认 14 天（保持 JSON shape / by_status
    // 断言沿用 build_snapshot_with_workers_basic 同形）。
    let snap = DashboardService::new()
        .build_snapshot_with_workers(&mut *tx, None, None)
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

// 2026-09-30 新增：dashboard upcoming_delivery 桶按 OrderStatus 细分计数集成测试
// （覆盖 plan §1.2 SQL `GROUP BY (date, status)` + §1.1 VO `by_status` 字段）。
#[tokio::test]
async fn snapshot_counters_by_status_returns_per_status_breakdown() {
    let pool = setup().await;
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let now = now_naive();
    // 沿 SQL 内 `CURRENT_DATE`（= Local::now().date_naive()）口径，避免本地日期漂移
    let today = chrono::Local::now().date_naive();
    let day_after_2 = today + chrono::Duration::days(2);

    // 1 个 customer（t_part.customer_id NOT NULL 强制；serial_prefix varchar(1) 限 1 字符）
    let cust_id = snowflake.next_id();
    sqlx::query(
        "INSERT INTO t_customer (id, name, parent_id, serial_prefix, version, \
         created_at, updated_at) VALUES ($1, 'by_status_cust', NULL, 'B', 0, $2, $2)",
    )
    .bind(cust_id)
    .bind(now)
    .execute(&pool)
    .await
    .expect("insert t_customer");

    // today：3 PENDING + 2 INSPECTION + 1 DELIVERED（count=6）
    for status in &[
        "PENDING",
        "PENDING",
        "PENDING",
        "INSPECTION",
        "INSPECTION",
        "DELIVERED",
    ] {
        sqlx::query(
            "INSERT INTO t_part (id, name, drawing_no, applicant_name, customer_id, \
             request_date, planned_delivery_date, status, version, \
             created_at, created_by, updated_at, updated_by) \
             VALUES ($1, 'p-bs', 'DWG-BS', 'tester', $2, $3, $3, $4, 0, $5, NULL, $5, NULL)",
        )
        .bind(snowflake.next_id())
        .bind(cust_id)
        .bind(today)
        .bind(*status)
        .bind(now)
        .execute(&pool)
        .await
        .expect("insert t_part today");
    }

    // today+2：1 PROGRAMMING（count=1）
    sqlx::query(
        "INSERT INTO t_part (id, name, drawing_no, applicant_name, customer_id, \
         request_date, planned_delivery_date, status, version, \
         created_at, created_by, updated_at, updated_by) \
         VALUES ($1, 'p-bs', 'DWG-BS', 'tester', $2, $3, $4, 'PROGRAMMING', 0, $5, NULL, $5, NULL)",
    )
    .bind(snowflake.next_id())
    .bind(cust_id)
    .bind(today)
    .bind(day_after_2)
    .bind(now)
    .execute(&pool)
    .await
    .expect("insert t_part day+2");

    let mut tx = pool.begin().await.unwrap();
    let snap = DashboardService::new()
        .build_snapshot_with_workers(&mut *tx, None, None)
        .await
        .expect("snapshot ok");
    drop(tx);

    // 必有 14 桶（默认 14 天；2026-09-30 原 7 改 14）
    assert_eq!(snap.upcoming_delivery.len(), 14);

    // today 桶：count=6，by_status 三 key
    let today_bucket = &snap.upcoming_delivery[0];
    assert_eq!(today_bucket.date, today.format("%Y-%m-%d").to_string());
    assert_eq!(today_bucket.count, 6);
    assert_eq!(today_bucket.by_status.get("PENDING"), Some(&3));
    assert_eq!(today_bucket.by_status.get("INSPECTION"), Some(&2));
    assert_eq!(today_bucket.by_status.get("DELIVERED"), Some(&1));
    assert_eq!(
        today_bucket.by_status.len(),
        3,
        "today 桶仅 3 个状态 key 入库（COMPLETED/CANCELLED 已被 SQL WHERE 排除）"
    );

    // today+2 桶：count=1，by_status = {"PROGRAMMING": 1}
    let d2_bucket = &snap.upcoming_delivery[2];
    assert_eq!(d2_bucket.date, day_after_2.format("%Y-%m-%d").to_string());
    assert_eq!(d2_bucket.count, 1);
    assert_eq!(d2_bucket.by_status.get("PROGRAMMING"), Some(&1));
    assert_eq!(d2_bucket.by_status.len(), 1);

    // 其它 12 天桶（默认 14 - today/today+2 = 12）：count=0，by_status 空 map
    for (idx, b) in snap.upcoming_delivery.iter().enumerate() {
        if idx == 0 || idx == 2 {
            continue;
        }
        assert_eq!(b.count, 0, "day idx={idx} count 应为 0");
        assert!(b.by_status.is_empty(), "day idx={idx} by_status 应为空 map");
    }
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
    let pool = setup().await;
    let state = test_state(pool.clone()).await;
    (serve_ws(state.clone()).await, state)
}

/// 2026-10-01 新增：起一个**自定义广播容量** hub 的 WS 服务端
/// （`ws_e2e_lagged_client_gets_4003_close` 用——`WsHub::new()` 硬编码容量 1024，
/// 灌 1025 条才复现慢消费方太丑，故测试侧用 `WsHub::with_capacity(1)` 造小环）。
///
/// 实现取舍：`AppState::new` 的 10 个入参与 `AppState` 的**全部字段都是 `pub`**，
/// 所以这里直接拿 `test_state` 的产物「换掉 `ws_hub` 重装一份」——**不必改 test-support**
/// （改动面最小的方案：只在测试文件内多一个局部 helper）。
async fn spawn_ws_server_with_hub_cap(
    broadcast_cap: usize,
) -> (String, Arc<hsh_erp_rust::state::AppState>) {
    use hsh_erp_rust::infra::ws_hub::WsHub;
    use hsh_erp_rust::state::AppState;

    let pool = setup().await;
    let base = test_state(pool).await;
    let state = Arc::new(AppState::new(
        base.pool.clone(),
        base.config.clone(),
        base.snowflake.clone(),
        Arc::new(WsHub::with_capacity(broadcast_cap)),
        base.cos.clone(),
        base.py_backend.clone(),
        base.shutdown.clone(),
        base.session.clone(),
        base.idempotency_store.clone(),
        base.wecom.clone(),
    ));
    (serve_ws(state.clone()).await, state)
}

/// 2026-10-01 抽出：把 `test_ws_app` 挂到随机端口真跑起来（两个 spawn helper 共用）。
async fn serve_ws(state: Arc<hsh_erp_rust::state::AppState>) -> String {
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
    format!("ws://127.0.0.1:{}", addr.port())
}

/// 2026-10-01 新增：轮询直到服务端发来带指定 code 的 Close 帧（跳过其间的 text/Ping 等帧）。
///
/// B2 之后 Close 帧是**主动**发的，带业务 code；测试要的就是这个 code。
/// 返回 `reason` 便于断言 message 文本。超时（`wait` 用尽）返回 `None` 由调用方断言失败。
async fn wait_for_close(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    want_code: u16,
    wait: Duration,
) -> Option<String> {
    let deadline = tokio::time::Instant::now() + wait;
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(500), ws.next()).await {
            Ok(Some(Ok(WsMessage::Close(Some(frame))))) => {
                // tungstenite 会把 u16 归类成 `CloseCode::{Normal,Error,Restart,…}`（IANA 段）
                // 或 `CloseCode::Library(code)`（4000-4999 应用私有段），故两侧都走
                // `From` 转换后比较，不能直接拿 `CloseCode::Library(..)` 硬套。
                if frame.code == CloseCode::from(want_code) {
                    return Some(frame.reason.to_string());
                }
            }
            Ok(Some(Ok(_))) => continue,
            Ok(Some(Err(e))) => panic!("ws frame err: {e}"),
            Ok(None) => return None,
            Err(_) => continue, // 500ms 内无帧 → 继续轮询直到 deadline
        }
    }
    None
}

/// 签发合法 access token + 写入 Redis session，使 dashboard WS 握手通过。
async fn mint_test_token(state: &Arc<hsh_erp_rust::state::AppState>, user_id: i64) -> String {
    use hsh_erp_rust::auth::session::{CachedUserProfile, TokenKind};
    // 2026-09-23 重构：encode_access 第 2-7 参数改为 `(private_key, signing_kid, issuer, audience, subject, ttl_seconds)` —— RS256 + kid 多密钥轮换；
    // 返回三元组 `(token, jti, exp)`，jti 即为 Redis session key 后缀来源（`session:tok:<jti>`），无需再调用 `hash_token`。
    let (token, jti, _exp) = encode_access(
        &state.config.jwt.private_key,
        &state.config.jwt.signing_kid,
        &state.config.jwt.issuer,
        &state.config.jwt.audience,
        user_id,
        state.config.jwt.access_ttl_seconds,
    )
    .expect("encode_access");
    // 写 Redis session，让 ws_dashboard 握手时 session 校验通过（不返 40105）。
    // 2026-09-22 重构：`CachedCurrentUser` → `CachedUserProfile`（删 id 字段）。
    let profile = CachedUserProfile {
        username: DashboardWsFixture::WS_USERNAME.to_string(),
        roles: vec!["MANAGER".into()],
        shelf_ids: vec![],
        shelf_wildcard: true,
    };
    state
        .session
        .create_session(
            &jti,
            user_id,
            TokenKind::Access,
            state.config.redis.session_ttl_seconds,
            &profile,
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

    // 2026-10-01 B4 改：原用例在收到 protocol-level Ping 时 panic，理由是
    // 「心跳应走 text、不用 Ping（followup A6）」。该判断**已被 B4 推翻**——Ping 现在
    // 承担**服务端存活检测**职责（浏览器协议栈自动回 Pong，服务端据此续 `last_seen`），
    // 与 text 心跳**并存、职责分离、互不替代**：
    // - text 心跳帧 → 浏览器 JS `onmessage` 收得到，给**前端**感知用（原判断依然成立，保留断言）；
    // - `Message::Ping` → JS 完全不可见，给**服务端**判活对端用（新增）。
    // 故本用例改为同时断言三者：text 心跳仍在、服务端 Ping 在、客户端 Ping 收到 Pong 回声。
    let probe = b"probe-pong".to_vec();
    ws.send(WsMessage::Ping(probe.clone().into()))
        .await
        .expect("send client ping");

    // 等心跳：测试 config 把 ws_heartbeat_interval_seconds 设为 1；
    // 给 1s + 5s slack 总 6s 上限避免 CI 抖动。
    let heartbeat_interval = state.config.ws_heartbeat_interval_seconds;
    let wait = Duration::from_secs(heartbeat_interval + 5);
    let mut got_heartbeat = false;
    let mut got_server_ping = false;
    let mut got_pong_echo = false;
    let deadline = tokio::time::Instant::now() + wait;
    // 三项全齐才退出（text 心跳 / 服务端 Ping / 客户端 Ping 的 Pong 回声）——text 心跳与
    // Ping 都从 1s 起发，若只等前两项会在同一轮 poll 里提前退出、漏读同一批的 Ping 帧。
    while tokio::time::Instant::now() < deadline
        && !(got_heartbeat && got_pong_echo && got_server_ping)
    {
        match tokio::time::timeout(Duration::from_millis(500), ws.next()).await {
            Ok(Some(Ok(WsMessage::Text(text)))) => {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text)
                    && v["type"] == "heartbeat"
                    && v["ts"].is_number()
                {
                    got_heartbeat = true;
                }
            }
            // 服务端 protocol-level Ping：2026-10-01 B4 起合法（存活检测）。
            // 客户端 tungstenite 会在协议栈自动回 Pong（JS 侧不可见的同款机制）。
            Ok(Some(Ok(WsMessage::Ping(_)))) => {
                got_server_ping = true;
            }
            Ok(Some(Ok(WsMessage::Pong(p)))) => {
                if p.as_ref() == probe.as_slice() {
                    got_pong_echo = true;
                }
            }
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
    assert!(
        got_pong_echo,
        "客户端 Ping 未收到服务端 Pong 回声（handler.rs 的 Ping→Pong 分支）"
    );
    assert!(
        got_server_ping,
        "未在 {wait:?} 内收到服务端 protocol-level Ping（B4 存活检测 ping_interval={}s）",
        state.config.ws_ping_interval_seconds
    );
}

// ===========================================================================
// 2026-10-01 WS 健壮性加固 E2E（B1 Lagged / B2 Close 帧 / B3 连接表 / B4 存活检测）
// ===========================================================================

/// B1 + B2：慢消费方导致 `broadcast::RecvError::Lagged(n)` → 服务端发 `4003 lagged`
/// Close 帧并断开（前端据此重连 + 全量 HTTP 重取）。
#[tokio::test]
async fn ws_e2e_lagged_client_gets_4003_close() {
    // 容量 1 的广播环：连发 3 条必然溢出（tokio ring buffer 覆盖最旧值 → 下次 recv 返 Lagged）。
    let (base, state) = spawn_ws_server_with_hub_cap(1).await;
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let user_id = snowflake.next_id();
    let token = mint_test_token(&state, user_id).await;
    let url = format!("{base}/dashboard?token={token}");

    let (mut ws, _resp) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("WS upgrade must succeed for valid token");

    // 消耗首条 snapshot（顺带确保服务端已进主循环、已 subscribe）
    let _ = tokio::time::timeout(Duration::from_secs(5), ws.next())
        .await
        .expect("snapshot 超时")
        .expect("ws stream closed")
        .expect("ws frame err");

    // 2026-10-01：`broadcast::Sender::send` 是**同步**的，这里是 `#[tokio::test]`
    // （current_thread runtime）的紧循环 —— 循环内不让出执行权，服务端 handler 根本没
    // 机会 poll `rx.recv()`，因此 3 条必溢出（Lagged）。`broadcast_tx` 本就是 `pub` 字段，
    // 无需新增只读 accessor。
    let tx = state.ws_hub.broadcast_tx.clone();
    for i in 0..3i64 {
        tx.send(WsEvent::DashboardEvent {
            kind: "PART_TO_SHIP".into(),
            payload: serde_json::json!({ "part_id": i.to_string() }),
        })
        .expect("broadcast send");
    }

    let reason = wait_for_close(&mut ws, 4003, Duration::from_secs(10)).await;
    assert_eq!(
        reason.as_deref(),
        Some("lagged"),
        "慢消费方应收到 4003 / reason=lagged"
    );
}

/// B2 + B4：连上后**一个帧都不发**（连服务端 protocol-level Ping 的 Pong 都不回），
/// 超过 `ws_pong_timeout_seconds`（测试配置 3s）没等到任何入站帧 → 判定对端已死，
/// 发 `1011 pong timeout` Close 帧。
#[tokio::test]
async fn ws_e2e_pong_timeout_closes_dead_peer() {
    let (base, state) = spawn_ws_server().await;
    let pong_timeout = state.config.ws_pong_timeout_seconds;
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let user_id = snowflake.next_id();
    let token = mint_test_token(&state, user_id).await;
    let url = format!("{base}/dashboard?token={token}");

    let (mut ws, _resp) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("WS upgrade must succeed for valid token");

    // 关键：接下来**绝不 poll `ws.next()`**。tokio-tungstenite 是拉驱动（没有后台读任务），
    // 不 poll 就不会读 socket、也就不会自动回 Pong —— 等价于浏览器/客户端掉线（半开连接）。
    // 只 sleep 让服务端推进自己的定时器（timeout 分支走绝对 deadline，与 text 心跳共存）。
    tokio::time::sleep(Duration::from_secs(pong_timeout + 2)).await;

    let reason = wait_for_close(&mut ws, 1011, Duration::from_secs(10)).await;
    assert_eq!(
        reason.as_deref(),
        Some("pong timeout"),
        "超过 {pong_timeout}s 无入站帧应收到 1011 / reason=pong timeout"
    );
}

/// B3：连接表登记 / 注销计数正确。
/// 前半段直接打 `WsHub` accessor（含「同一用户 2 条连接」这个 `user_sinks` 时代的老坑）；
/// 后半段走真实 socket 验 `handle_socket` 真的在两端维护了连接表。
#[tokio::test]
async fn ws_e2e_conn_registry_counts() {
    use hsh_erp_rust::infra::ws_hub::WsHub;

    // --- 前半段：accessor 语义（无需 DB / socket） ---
    let hub = WsHub::new();
    assert_eq!(hub.conn_count(), 0, "初始无连接");
    let c1 = hub.register_conn(1, "alice");
    let c2 = hub.register_conn(1, "bob");
    let _c3 = hub.register_conn(2, "carol");
    assert_eq!(hub.conn_count(), 3, "3 条连接");
    assert_eq!(
        hub.user_conn_count(1),
        2,
        "同一用户 2 条连接不能互相覆盖（user_sinks 单槽的老坑）"
    );
    assert_eq!(hub.user_conn_count(2), 1);
    assert_ne!(c1, c2, "conn_id 必须互异");
    assert!(hub.unregister_conn(c1).is_some());
    assert_eq!(hub.conn_count(), 2);
    assert!(hub.unregister_conn(c1).is_none(), "重复注销返回 None");
    assert_eq!(hub.user_conn_count(1), 1);

    // --- 后半段：真实连接进 / 出表 ---
    let (base, state) = spawn_ws_server().await;
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let user_id = snowflake.next_id();
    let token = mint_test_token(&state, user_id).await;
    let url = format!("{base}/dashboard?token={token}");

    let (mut ws, _resp) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("WS upgrade must succeed for valid token");
    // 收首条 snapshot：此时 handle_socket 必然已 register_conn（register 在推快照之前）
    let _ = tokio::time::timeout(Duration::from_secs(5), ws.next())
        .await
        .expect("snapshot 超时")
        .expect("ws stream closed")
        .expect("ws frame err");
    assert_eq!(state.ws_hub.conn_count(), 1, "连接建立后应在连接表内");
    assert_eq!(state.ws_hub.user_conn_count(user_id), 1);

    // 客户端主动关 → 服务端读到 Close 后 break → unregister_conn
    let _ = ws.close(None).await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline && state.ws_hub.conn_count() != 0 {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(state.ws_hub.conn_count(), 0, "连接关闭后应已出表");
}

/// B2 + B4：`state.shutdown.cancel()`（生产 = Ctrl-C 优雅退出）→ 服务端发
/// `1012 server restart` Close 帧，让前端立刻重连而不是干等 TCP 超时。
#[tokio::test]
async fn ws_e2e_server_shutdown_sends_1012() {
    let (base, state) = spawn_ws_server().await;
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let user_id = snowflake.next_id();
    let token = mint_test_token(&state, user_id).await;
    let url = format!("{base}/dashboard?token={token}");

    let (mut ws, _resp) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("WS upgrade must succeed for valid token");
    // 先收快照，确保 handler 已进 select 主循环（否则 cancel 可能赶在 subscribe 之前）
    let _ = tokio::time::timeout(Duration::from_secs(5), ws.next())
        .await
        .expect("snapshot 超时")
        .expect("ws stream closed")
        .expect("ws frame err");

    state.shutdown.cancel();

    let reason = wait_for_close(&mut ws, 1012, Duration::from_secs(10)).await;
    assert_eq!(
        reason.as_deref(),
        Some("server restart"),
        "优雅退出应发 1012 / reason=server restart"
    );
}

// ===========================================================================
// 2026-09-28 新增：HTTP `GET /api/v2/dashboard/snapshot` 端点集成测试
// ===========================================================================
//
// 覆盖：
//   1. http_snapshot_unauthenticated_returns_401   — 无 Bearer token 应返 401（中间件）
//   2. http_snapshot_happy_path_returns_full_shape  — 登录后 GET 返回 200 + 完整 shape
//
// 与 WS 端点共用 `load_dashboard_ws_fixture`（baseline fx_dashboard_ws_user，
// 密码 "changeme"；fixture 用户的 t_user_role 在 DB 内为空，故走 `mint_test_token`
// 直接写 Redis session + Manager 角色 profile，绕过 `/iam/login` 业务层 20606
// 角色校验；与 ws_e2e_* 风格一致）。snapshot 数据每个用例现场插，避免 fixture
// 占用 shelf code 字面。

/// 局部 send 别名（与 ws_e2e_* 同款，避免 oneshot 消耗 Router 后 caller 无法再发请求）
async fn send(
    app: axum::Router,
    req: axum::http::Request<axum::body::Body>,
) -> (axum::http::StatusCode, serde_json::Value) {
    ts_send(app, req).await
}

#[tokio::test]
async fn http_snapshot_unauthenticated_returns_401() {
    // 不登录直接 GET，应被 authenticate_middleware 拦截返 401
    // 注意：`test_app` 不挂 `/api/v2` 前缀（main.rs 才挂；测试走 v2_router 原生路径）
    let pool = test_pool().await;
    let app = test_app(test_state(pool.clone()).await);
    let (status, _envelope) =
        send(app, json_request("GET", "/dashboard/snapshot", None, None)).await;
    assert_eq!(
        status,
        axum::http::StatusCode::UNAUTHORIZED,
        "无 Bearer token 应返 401"
    );
}

#[tokio::test]
async fn http_snapshot_happy_path_returns_full_shape() {
    // 走「mint access token + 写 Redis session」路径（与本文件 ws_e2e_* 风格一致，
    // 跳过 `/iam/login` 业务层 20606 角色校验——dashboard_ws fixture 用户不带角色，
    // login 路径会返 403）。
    //
    // 注意：`test_app` 不挂 `/api/v2` 前缀（main.rs 才挂；测试走 v2_router 原生路径）
    let pool = setup().await;
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let now = now_naive();

    // 插一个 active 货架，让 snapshot 含该架组
    sqlx::query(
        "INSERT INTO t_shelf (id, code, name, zone, is_active, display_order, version, \
         created_at, updated_at) \
         VALUES ($1, 'S-HTTP1', 'HTTP一号架', 'PRODUCTION', true, 0, 0, $2, $2)",
    )
    .bind(snowflake.next_id())
    .bind(now)
    .execute(&pool)
    .await
    .expect("insert t_shelf");

    // 走 dashboard_ws fixture 用户的 snowflake id（fixture 写死），mint 合法 token
    let state = test_state(pool.clone()).await;
    let user_id = DashboardWsFixture::WS_USER_ID;
    let token = mint_test_token(&state, user_id).await;

    let app = test_app(state.clone());
    let (status, envelope) = send(
        app,
        json_request("GET", "/dashboard/snapshot", None, Some(&token)),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK, "登录后 GET 应返 200");
    assert_eq!(envelope["code"], 0, "信封 code 应为 0；envelope={envelope}");
    let data = &envelope["data"];
    assert!(
        data["on_production_shelves"].is_array(),
        "data.on_production_shelves 应为数组"
    );
    assert!(
        data["on_inspection_shelves"].is_array(),
        "data.on_inspection_shelves 应为数组"
    );
    assert!(data["in_process"].is_array(), "data.in_process 应为数组");
    assert!(
        data["upcoming_delivery"].is_array(),
        "data.upcoming_delivery 应为数组"
    );
    // 默认 14 天固定 14 条（2026-09-30 新增：原 7 改 14，与 service 默认天数对齐）
    assert_eq!(
        data["upcoming_delivery"].as_array().unwrap().len(),
        14,
        "默认 14 天固定 14 条"
    );
    // S-HTTP1 应在产线组里（即使 items 空也算，因为 fixture 期望该架被 snapshot 选中）
    let on_prod = data["on_production_shelves"].as_array().unwrap();
    assert!(
        on_prod.iter().any(|g| g["shelf_code"] == "S-HTTP1"),
        "S-HTTP1 应在 on_production_shelves 中；got={on_prod:?}"
    );
    assert!(
        !data["ts"].as_str().unwrap_or("").is_empty(),
        "data.ts 应非空"
    );
}

// ===========================================================================
// 2026-09-30 新增：HTTP `GET /api/v2/dashboard/snapshot?upcoming_days=` query 参数
// ===========================================================================
//
// 覆盖 service 层 DASHBOARD_DEFAULT_DAYS=14 + clamp(1, 60) + handler 层
// SnapshotQuery.deserialize_i64_opt 解析：
//   - default_14_days_returns_14_buckets — 缺省 query 走 14 天
//   - custom_7_days_returns_7_buckets   — ?upcoming_days=7 显式 7 天（向后兼容老契约）
//
// 注意：`test_app` 不挂 `/api/v2` 前缀（main.rs 才挂；测试走 v2_router 原生路径）；
// 走 `mint_test_token` 直接写 Redis session 跳过 `/iam/login` 业务层 20606 角色校验，
// 与同文件 ws_e2e_* / http_snapshot_* 风格一致。

#[tokio::test]
async fn http_snapshot_default_14_days_returns_14_buckets() {
    // 缺省 query（无 `?upcoming_days=`）：service 层 DASHBOARD_DEFAULT_DAYS=14 兜底，
    // 响应 `data.upcoming_delivery` 应含 14 条桶（today + 未来 13 天）。
    let pool = setup().await;
    let state = test_state(pool.clone()).await;
    let token = mint_test_token(&state, DashboardWsFixture::WS_USER_ID).await;
    let app = test_app(state.clone());

    let (status, envelope) = send(
        app,
        json_request("GET", "/dashboard/snapshot", None, Some(&token)),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(envelope["code"], 0);
    let buckets = envelope["data"]["upcoming_delivery"]
        .as_array()
        .expect("upcoming_delivery 必为 array");
    assert_eq!(
        buckets.len(),
        14,
        "缺省 query 走 service DASHBOARD_DEFAULT_DAYS=14，应返 14 条桶"
    );

    // 第 0 条 date = today（YYYY-MM-DD，与 Local::now().date_naive() 对齐）
    let today_str = chrono::Local::now()
        .date_naive()
        .format("%Y-%m-%d")
        .to_string();
    assert_eq!(
        buckets[0]["date"].as_str(),
        Some(today_str.as_str()),
        "首桶日期应为今天"
    );
    // 每条都含 by_status 字段（必填；空对象 = 当日 0 件）
    for (idx, b) in buckets.iter().enumerate() {
        assert!(
            b["by_status"].is_object(),
            "第 {idx} 桶 by_status 应为 object"
        );
        assert!(
            b["count"].is_number(),
            "第 {idx} 桶 count 应为 number（JSON wire 不走字符串化）"
        );
    }
}

#[tokio::test]
async fn http_snapshot_custom_7_days_returns_7_buckets() {
    // `?upcoming_days=7`：service 层 unwrap_or(14) 路径不触发，clamp(1,60) 命中
    // 7，响应 `data.upcoming_delivery` 应含 7 条桶（向后兼容原 Python v1 dashboard 契约）。
    let pool = setup().await;
    let state = test_state(pool.clone()).await;
    let token = mint_test_token(&state, DashboardWsFixture::WS_USER_ID).await;
    let app = test_app(state.clone());

    let (status, envelope) = send(
        app,
        json_request(
            "GET",
            "/dashboard/snapshot?upcoming_days=7",
            None,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(envelope["code"], 0);
    let buckets = envelope["data"]["upcoming_delivery"]
        .as_array()
        .expect("upcoming_delivery 必为 array");
    assert_eq!(
        buckets.len(),
        7,
        "?upcoming_days=7 显式应返 7 条桶（向后兼容 v1 Python 契约）"
    );

    // 第 6 条 date = today + 6 天（与 SQL 内 CURRENT_DATE + $1 days 对齐）
    let today = chrono::Local::now().date_naive();
    let day6_str = (today + chrono::Duration::days(6))
        .format("%Y-%m-%d")
        .to_string();
    assert_eq!(
        buckets[6]["date"].as_str(),
        Some(day6_str.as_str()),
        "末桶日期应为 today+6 天"
    );
}

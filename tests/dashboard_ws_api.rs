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
//!    16. ws_e2e_reauth_failure_sends_4001_close — 2026-10-02 新增（Minor 7）：
//!         吊销 session → 周期性 re-auth 失败 → 4001 auth expired Close 帧
//!
//!   HTTP `GET /api/v2/dashboard/snapshot` 集成测试（2026-09-28 新增 + 2026-09-30 扩 query）：
//!     8. http_snapshot_unauthenticated_returns_401   — 无 Bearer token 应返 401（中间件）
//!     9. http_snapshot_happy_path_returns_full_shape  — 登录后 GET 返回 200 + 完整 shape（默认 14 天）
//!    10. http_snapshot_default_14_days_returns_14_buckets — 缺省 ?upcoming_days → 14 条桶
//!    11. http_snapshot_custom_7_days_returns_7_buckets   — ?upcoming_days=7 → 7 条桶（向后兼容老契约）
//!
//!   HTTP `?basis=` query 参数（2026-10-04 新增）：
//!    17. http_snapshot_basis_switches_delivery_date_column — 同库同数据下
//!        ?basis=planned 落 today+3 桶、?basis=system 落 today+9 桶（钉死交期列切换）
//!    18. http_snapshot_basis_invalid_value_returns_400  — ?basis=xxx → 400（Query 反序列化，纯文本体）
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
    DashboardWsFixture, json_request, load_dashboard_ws_fixture, send as ts_send, send_raw,
    test_app, test_pool, test_state, test_ws_app,
};
use sqlx::PgPool;
use std::sync::Arc;
use std::time::Duration;
// 2026-10-02（review 第 2 轮 Major-A）：`ws_e2e_pong_timeout_closes_dead_peer` 绕开
// tungstenite、直接读裸 socket 断言 Close 帧线路字节，需要 `read_to_end`。
use tokio::io::AsyncReadExt;
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
    // 2026-10-04 新增 basis 形参（默认 planned）：service 层 unwrap_or_default()；
    // 本用例直调沿用 `None` → 计划交期口径，断言 shape 不受口径影响。
    let snap = DashboardService::new()
        .build_snapshot_with_workers(&mut *tx, None, None, None)
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
        .build_snapshot_with_workers(&mut *tx, None, None, None)
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
        .build_snapshot_with_workers(&mut *tx, None, None, None)
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
///
/// 返回 `Ok(reason)` = 读到了目标 code 的 Close 帧，附 reason 文案（调用方再断言文案）；
/// 返回 `Err(诊断)` = 没拿到，诊断串说明**是哪一类**没拿到。
///
/// ## 2026-10-02（review 第 2 轮 Major-A）：为什么读帧出错不再 `panic!`
/// 旧实现是 `Ok(Some(Err(e))) => panic!("ws frame err: {e}")`。在「**服务端判对端已死**」
/// 这一类用例里，读帧出错是**预期内**的：服务端发完 Close 帧就 drop socket，而客户端
/// tungstenite 读到队列里的 Ping 会**自动回 Pong**（`tungstenite` `protocol/mod.rs` 的
/// `read()` 在读下一帧前先 flush `additional_send`）⇒ 往已关闭的 socket 写 → `EPIPE`；
/// `tokio-tungstenite` `lib.rs::poll_next` 随即把 `ended = true`，**之后所有 poll 返 `None`**，
/// Close 帧再也读不到。旧 `panic!` 抛出的 `ws frame err: Broken pipe` 看起来像
/// 「服务端把 socket 写坏了」的产品 bug，实际是测试自己的客户端在写自动 Pong，
/// **会误导将来 on-call**。
///
/// 故改为：把「超时未读到」与「流提前结束/报错」分别写进 `Err`，由调用方断言。
/// 真正的产品故障（服务端该发的 Close 帧没发）依然会红，只是报错信息不再撒谎。
async fn wait_for_close(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    want_code: u16,
    wait: Duration,
) -> Result<String, String> {
    let deadline = tokio::time::Instant::now() + wait;
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(500), ws.next()).await {
            Ok(Some(Ok(WsMessage::Close(Some(frame))))) => {
                // tungstenite 会把 u16 归类成 `CloseCode::{Normal,Error,Restart,…}`（IANA 段）
                // 或 `CloseCode::Library(code)`（4000-4999 应用私有段），故两侧都走
                // `From` 转换后比较，不能直接拿 `CloseCode::Library(..)` 硬套。
                if frame.code == CloseCode::from(want_code) {
                    return Ok(frame.reason.to_string());
                }
            }
            Ok(Some(Ok(_))) => continue,
            // 2026-10-02：不再 panic，见上方 doc。`Some(Err)` 与 `None` 合并成「流结束」。
            Ok(Some(Err(e))) => {
                return Err(format!(
                    "读帧报错后流终止（{e}）——**对端已死时读到 I/O 错误属预期**：\
                     tungstenite 读到 Ping 会自动回 Pong，往服务端已 drop 的 socket 写会 EPIPE，\
                     且 poll_next 置 ended=true 后 Close 帧无法再读到。\
                     也就是说本用例的 {want_code} Close 帧**没被客户端读到**（服务端是否真的发了，\
                     要看服务端日志或改用裸 socket 断言）"
                ));
            }
            Ok(None) => {
                return Err(format!(
                    "流在读到 {want_code} Close 帧之前就结束（无更多帧）——\
                     同上，服务端已关闭连接，本用例没读到 {want_code} Close 帧"
                ));
            }
            Err(_) => continue, // 500ms 内无帧 → 继续轮询直到 deadline
        }
    }
    Err(format!(
        "在 {wait:?} 内没读到 {want_code} Close 帧（其间只收到别的帧）"
    ))
}

/// 签发合法 access token + 写入 Redis session，使 dashboard WS 握手通过。
/// 只返回 token（多数用例只需要它）。
async fn mint_test_token(state: &Arc<hsh_erp_rust::state::AppState>, user_id: i64) -> String {
    mint_test_token_with_jti(state, user_id).await.0
}

/// 2026-10-02 新增：同上，但**一并返回 jti**（= Redis session key 后缀）。
///
/// 为什么需要 jti：本文件所有用例的 snowflake generator 都写死 `instance = 1`
/// （`SnowflakeIdGenerator::new(1_577_836_800_000, 1)`），所以**并行执行的用例之间
/// user_id 会撞**。`ws_e2e_reauth_failure_sends_4001_close` 若用
/// `delete_all_user_sessions(user_id)` 吊销 session，会把并发用例（撞到同一 user_id）
/// 的 session 一起删掉 → 那些用例的 re-auth 无端失败，表现为莫名其妙的 flake。
/// 精确到 jti 的 `delete_session(&jti)` 没有这个副作用。
async fn mint_test_token_with_jti(
    state: &Arc<hsh_erp_rust::state::AppState>,
    user_id: i64,
) -> (String, String) {
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
    (token, jti)
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

    let reason = wait_for_close(&mut ws, 4003, Duration::from_secs(10))
        .await
        .unwrap_or_else(|e| panic!("慢消费方应收到 4003/lagged Close 帧，但：{e}"));
    assert_eq!(reason, "lagged", "4003 Close 帧的 reason 应为 lagged");
}

/// B2 + B4：连上后**一个帧都不发**（连服务端 protocol-level Ping 的 Pong 都不回），
/// 超过 `ws_pong_timeout_seconds`（测试配置 3s）没等到任何入站帧 → 判定对端已死，
/// 发 `1011 pong timeout` Close 帧。
///
/// ## 2026-10-02（review 第 2 轮 Major-A）：这里读**裸 socket**，不用 tungstenite 读帧
/// 本用例约 1/8 概率红，报 `ws frame err: Broken pipe`——看起来像产品 bug，实际是测试自己
/// 的客户端在写自动 Pong。链路（reviewer 已核 tungstenite 源码）：
///
/// 1. 不变式强制 `pong_timeout >= 2 × ping_interval`，所以任何「沉默客户端」在被判死之前
///    **必定至少收到 1 个服务端 Ping**；
/// 2. tungstenite 收到 Ping 会 `set_additional(Frame::pong(..))`，而 `read()` 在**读下一帧
///    之前**先 flush `additional_send`（`protocol/mod.rs:449-470`）⇒ 客户端必然向 socket
///    写一次 Pong；
/// 3. 但服务端在 t≈`pong_timeout` 已发完 Close 帧并 drop 了 socket ⇒ 该 Pong 写进黑洞，
///    对端回 RST ⇒ `EPIPE`；
/// 4. `tokio-tungstenite` `lib.rs::poll_next` 把错误映射成 `Poll::Ready(Some(Err(e)))` 并
///    **置 `ended = true`**，之后所有 poll 返 `None` ⇒ **Close 帧再也读不到**。
///
/// 即：这不是运气问题，而是结构性必然（步骤 1 保证 Ping 必到，步骤 4 保证读到就废）。
/// 本用例真正要验的只是「服务端确实发了带 1011 + `pong timeout` 的 Close 帧」，而
/// sleep 盲等期间该帧已静静躺在**内核接收缓冲**里 —— 故 `into_inner()` 取裸 socket 直接读，
/// 彻底绕开 tungstenite 的自动 Pong / `ended` 逻辑，且顺带**同时断言 code 与 reason**
/// （比原来只断言 reason 更强）。
#[tokio::test]
async fn ws_e2e_pong_timeout_closes_dead_peer() {
    let (base, state) = spawn_ws_server().await;
    let pong_timeout = state.config.ws_pong_timeout_seconds;
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let user_id = snowflake.next_id();
    let token = mint_test_token(&state, user_id).await;
    let url = format!("{base}/dashboard?token={token}");

    // 不加 `mut`：本用例**一次都不 poll**（`mut` 会因未使用告警）。
    let (ws, _resp) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("WS upgrade must succeed for valid token");

    // 关键：接下来**绝不 poll `ws.next()`**。tokio-tungstenite 是拉驱动（没有后台读任务），
    // 不 poll 就不会读 socket、也就不会自动回 Pong —— 等价于浏览器/客户端掉线（半开连接）。
    // 只 sleep 让服务端推进自己的定时器（timeout 分支走绝对 deadline，与 text 心跳共存）。
    tokio::time::sleep(Duration::from_secs(pong_timeout + 2)).await;

    // `into_inner()` 是同步的（tokio-tungstenite 0.29：`pub fn into_inner(self) -> S`），
    // 只取回底层 stream，**不动内核接收缓冲** —— 未 poll 过的数据完好无损。
    // 返回的 `MaybeTlsStream` 直接实现 `AsyncRead`（本用例是 `ws://` ⇒ `Plain`），
    // 故 `read_to_end` 拿到的就是原始线路字节（含 WS 帧头）。
    let mut sock = ws.into_inner();
    let mut buf = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), sock.read_to_end(&mut buf))
        .await
        .expect("读裸 socket 超时：服务端应已发完 Close 帧并关闭连接")
        .expect("读裸 socket 失败");

    // Close 帧线路字节：`0x88` = FIN|Close(opcode 8)、`0x0e` = payload 14 字节
    // （= 2 字节 code + reason 12 字节；注意 reason "pong timeout" 是 **12** 个字符：
    // pong(4) + 空格(1) + timeout(7)）、`0x03f3` = 1011（大端 u16）。
    let want: &[u8] = b"\x88\x0e\x03\xf3pong timeout";
    assert!(
        buf.windows(want.len()).any(|w| w == want),
        "裸字节里未找到 1011 / reason=pong timeout 的 Close 帧；共读到 {} 字节。         （{pong_timeout}s 无入站帧应触发该 Close 帧）末尾 48 字节十六进制：{:02X?}",
        buf.len(),
        &buf[buf.len().saturating_sub(48)..]
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

/// B4 + Minor-7：周期性 re-auth 失败（session 被吊销）→ 服务端发 `4001 auth expired`
/// Close 帧（前端契约：清本地 token 跳登录页，**不要**重连）。
///
/// 依赖「re-auth 周期可注入」（`AppConfig::ws_reauth_every_n_heartbeats`，review 第 1 轮
/// Minor 7 改动）：test-support 默认 `2` + text 心跳 1s → 每 2s 验一次，~2s 内即可验到。
/// 改之前是硬编码 `const 10`，触发一次要跑 >10s，这条**安全核心路径**在 CI 上永远覆盖不到。
///
/// 对应的另一半契约（基础设施故障 → `1011 re-auth unavailable`，而不是 4001）由
/// `src/modules/dashboard/handler.rs` 的单测 `reauth_infra_failure_maps_to_1011_not_4001`
/// 钉死（要端到端造「Redis 故障」需自定义 `SessionStore` 实现，代价远大于收益）。
#[tokio::test]
async fn ws_e2e_reauth_failure_sends_4001_close() {
    let (base, state) = spawn_ws_server().await;
    let reauth_every = state.config.ws_reauth_every_n_heartbeats;
    let heartbeat = state.config.ws_heartbeat_interval_seconds;
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let user_id = snowflake.next_id();
    let (token, jti) = mint_test_token_with_jti(&state, user_id).await;
    let url = format!("{base}/dashboard?token={token}");

    let (mut ws, _resp) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("WS upgrade must succeed for valid token");
    // 收首条 snapshot，确保 handler 已进主循环（否则吊销可能赶在 subscribe 之前）
    let _ = tokio::time::timeout(Duration::from_secs(5), ws.next())
        .await
        .expect("snapshot 超时")
        .expect("ws stream closed")
        .expect("ws frame err");

    // 精确吊销**本条** session（等价于「用户登出 / 管理员踢」）→ 下一轮 re-auth 的
    // `get_session` 返 None → `verify_session_token` 返 40105 SESSION_REVOKED。
    //
    // ⚠️ 必须按 jti 删，不能用 `delete_all_user_sessions(user_id)`：本文件所有用例的
    // snowflake generator 都写死 instance=1，并行用例之间 user_id 会撞，用 user_id
    // 删会把并发用例的 session 一起干掉 → 那些用例的 re-auth 无端失败（见 helper 注释）。
    state
        .session
        .delete_session(&jti)
        .await
        .expect("delete_session（模拟登出）");

    let wait = Duration::from_secs(heartbeat * u64::from(reauth_every) + 8);
    let reason = wait_for_close(&mut ws, 4001, wait)
        .await
        .unwrap_or_else(|e| {
            panic!(
                "session 被吊销后应在 ~{}s 内收到 4001/auth expired Close 帧（heartbeat={heartbeat}s \
                 × reauth_every={reauth_every}），但：{e}",
                heartbeat * u64::from(reauth_every)
            )
        });
    assert_eq!(
        reason, "auth expired",
        "4001 Close 帧的 reason 应为 auth expired"
    );
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

    let reason = wait_for_close(&mut ws, 1012, Duration::from_secs(10))
        .await
        .unwrap_or_else(|e| panic!("优雅退出应发 1012/server restart Close 帧，但：{e}"));
    assert_eq!(
        reason, "server restart",
        "1012 Close 帧的 reason 应为 server restart"
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

// ===========================================================================
// 2026-10-04 新增：HTTP `GET /api/v2/dashboard/snapshot?basis=` query 参数
// ===========================================================================
//
// 覆盖 handler 层 `SnapshotQuery.basis` 的 `Query` 反序列化 + repo 层两段 SQL 的
// 交期列切换：
//   - basis_switches_delivery_date_column  — 同库同数据下 `?basis=planned` 与
//     `?basis=system` 的桶内容落在不同日期下标，把「交期列换了」钉死
//   - basis_invalid_value_returns_400      — ?basis=xxx → 400（纯文本 body，不走信封）
//
// 非法取值的响应体不是 `R<T>` JSON，故用 `send_raw` 取原始文本（`send` 会在 JSON
// 解析处 panic）。

#[tokio::test]
async fn http_snapshot_basis_switches_delivery_date_column() {
    // 鉴别力设计：本库只插 **1 行** t_part，且它的两列交期分处窗口内不同下标
    // （planned = today+3 在 14 天窗口内，system = today+9 也在窗口内）——单看
    // 「桶数 = 14」两口径不可区分，必须断言**同一行落进不同的桶**才说明
    // `SQL_COUNTERS_PLANNED` / `SQL_COUNTERS_SYSTEM` 真的换了列。
    let pool = setup().await;
    let state = test_state(pool.clone()).await;
    let token = mint_test_token(&state, DashboardWsFixture::WS_USER_ID).await;
    let app = test_app(state.clone());

    // 沿 SQL 内 `CURRENT_DATE`（= Local::now().date_naive()）口径，避免本地日期漂移
    let today = chrono::Local::now().date_naive();
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let now = now_naive();

    // t_part.customer_id 是逻辑外键（无 DB 级 FK 约束），仍随仓内既有写法插一条
    // t_customer 保证引用自洽（serial_prefix varchar(1) 且须大写字母）。
    let cust_id = snowflake.next_id();
    sqlx::query(
        "INSERT INTO t_customer (id, name, parent_id, serial_prefix, version, \
         created_at, updated_at) VALUES ($1, 'basis_cust', NULL, 'Z', 0, $2, $2)",
    )
    .bind(cust_id)
    .bind(now)
    .execute(&pool)
    .await
    .expect("insert t_customer");

    // 唯一 1 行 part：两列交期**故意错开**——planned 落 today+3 桶、system 落
    // today+9 桶，两者都在 14 天窗口内，故「只插 NULL system 交期」那种数据无法
    // 区分口径。status 取 PENDING（SQL 已排除 COMPLETED / CANCELLED）。
    sqlx::query(
        "INSERT INTO t_part (id, name, drawing_no, applicant_name, customer_id, \
         request_date, planned_delivery_date, system_delivery_date, status, version, \
         created_at, created_by, updated_at, updated_by) \
         VALUES ($1, 'p-basis', 'DWG-BASIS', 'tester', $2, $3, $4, $5, 'PENDING', 0, $6, NULL, $6, NULL)",
    )
    .bind(snowflake.next_id())
    .bind(cust_id)
    .bind(today)
    .bind(today + chrono::Duration::days(3))
    .bind(today + chrono::Duration::days(9))
    .bind(now)
    .execute(&pool)
    .await
    .expect("insert t_part");

    // ── ?basis=planned：只认 planned_delivery_date → 落 today+3（idx 3） ──
    let (status, envelope) = send(
        app.clone(),
        json_request(
            "GET",
            "/dashboard/snapshot?basis=planned",
            None,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(
        status,
        axum::http::StatusCode::OK,
        "?basis=planned 应返 200"
    );
    assert_eq!(envelope["code"], 0);
    let buckets = envelope["data"]["upcoming_delivery"]
        .as_array()
        .expect("upcoming_delivery 必为 array");
    assert_eq!(buckets.len(), 14, "缺省 N = 14");
    assert_eq!(
        buckets[3]["count"].as_i64(),
        Some(1),
        "planned 口径应把该行计入 today+3 桶；got={}",
        buckets[3]
    );
    assert_eq!(
        buckets[9]["count"].as_i64(),
        Some(0),
        "planned 口径不该认 system 交期，today+9 桶应为 0；got={}",
        buckets[9]
    );
    assert_eq!(
        buckets[3]["by_status"]["PENDING"].as_i64(),
        Some(1),
        "planned 口径 today+3 桶 by_status 应含 PENDING=1"
    );

    // ── ?basis=system：只认 system_delivery_date → 落 today+9（idx 9） ──
    // 同一行、同一库，只改 query 口径：桶数与日期序列不动（两口径共用装配逻辑），
    // 变的只是命中哪一桶。
    let (status, envelope) = send(
        app.clone(),
        json_request(
            "GET",
            "/dashboard/snapshot?basis=system",
            None,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK, "?basis=system 应返 200");
    assert_eq!(envelope["code"], 0);
    let buckets = envelope["data"]["upcoming_delivery"]
        .as_array()
        .expect("upcoming_delivery 必为 array");
    assert_eq!(
        buckets.len(),
        14,
        "?basis=system 不改变桶的日期序列，缺省 N 仍为 14"
    );
    // 首桶日期仍为 today（口径只换交期列，不换分桶锚点）
    let today_str = today.format("%Y-%m-%d").to_string();
    assert_eq!(
        buckets[0]["date"].as_str(),
        Some(today_str.as_str()),
        "system 口径首桶日期仍应为今天"
    );
    assert_eq!(
        buckets[3]["count"].as_i64(),
        Some(0),
        "system 口径不该认 planned 交期，today+3 桶应为 0；got={}",
        buckets[3]
    );
    assert_eq!(
        buckets[9]["count"].as_i64(),
        Some(1),
        "system 口径应把该行计入 today+9 桶；got={}",
        buckets[9]
    );
    assert_eq!(
        buckets[9]["by_status"]["PENDING"].as_i64(),
        Some(1),
        "system 口径 today+9 桶 by_status 应含 PENDING=1"
    );
    for (idx, b) in buckets.iter().enumerate() {
        assert!(
            b["by_status"].is_object(),
            "第 {idx} 桶 by_status 应为 object（system 口径同契约）"
        );
    }
}

#[tokio::test]
async fn http_snapshot_basis_invalid_value_returns_400() {
    // `?basis=xxx` 不在 `DeliveryBasis` 的 `rename_all = "lowercase"` 变体里，
    // axum `Query` 反序列化直接返 400（纯文本 body，不走 `R<T>` 信封）。
    let pool = setup().await;
    let state = test_state(pool.clone()).await;
    let token = mint_test_token(&state, DashboardWsFixture::WS_USER_ID).await;
    let app = test_app(state.clone());

    let (status, body) = send_raw(
        app,
        json_request("GET", "/dashboard/snapshot?basis=xxx", None, Some(&token)),
    )
    .await;
    assert_eq!(
        status,
        axum::http::StatusCode::BAD_REQUEST,
        "非法 ?basis 取值应返 400；body={body}"
    );
    // 契约要点：axum 提取器层的 rejection 返纯文本，**不走 `R<T>` 信封**
    // （`docs/api/dashboard.md` 错误码段有对应说明）。
    assert!(
        serde_json::from_str::<serde_json::Value>(&body).is_err(),
        "400 body 应为纯文本而非 JSON 信封；body={body}"
    );
    // 光「非 JSON」定不出是哪个 query 参数解析失败的（只传 ?basis=xxx、
    // upcoming_days 缺省，故此断言同时把失败原因钉在 basis 上）。
    assert!(
        body.contains("basis"),
        "400 应由 basis 参数反序列化失败触发；body={body}"
    );
}

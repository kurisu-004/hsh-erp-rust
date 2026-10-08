//! dashboard 集成测试（2026-10-07 VO 重构后重排）
//!
//! 覆盖：
//!   service 层 snapshot 直调：
//!     1. build_snapshot_basic_shape              — 空库下 4 个字段的形状
//!     2. build_snapshot_in_process_carries_worker_held_batch_shape
//!                                                  — in_process 行 7 字段（核心回归）
//!
//!   service 层交期三方法：
//!     3. snapshot_counters_by_status_returns_per_status_breakdown
//!     4. snapshot_counters_window_anchors_on_passed_today_not_current_date
//!                                                  — 分桶窗口下界取自传入 `today`
//!                                                    而非 SQL `CURRENT_DATE`（核心回归）
//!     5. overdue_count_* （6 组口径，见下方小节标题）
//!     6. system_delivery_orders_* （分桶 / 截断 / 窗口边界）
//!     7. delivery_order_details_* （单日 / 状态 / total 与截断 / 两口径）
//!
//!   ws_hub 协作：
//!     8. ws_hub_broadcast_subscription_receives_event — 业务事件订阅通路
//!
//!   真实 socket E2E：
//!     9. ws_e2e_invalid_token_rejected        — 40101（JWT 验签失败）/ 40100（缺 token）
//!    10. ws_e2e_valid_token_receives_snapshot — 握手后 ≤ 5s 收首条 snapshot text；
//!                                                  同时断言外层 / 内层 `ts` 同为 +08:00
//!    11. ws_e2e_valid_token_receives_heartbeat_text — ≤ 心跳间隔 + 5s 同时收齐
//!                                                  ① `WsHeartbeatMsg` text 帧 ② 服务端 protocol-level
//!                                                  Ping ③ 客户端 Ping 的 Pong 回声
//!    12. ws_e2e_lagged_client_gets_4003_close  — 慢消费方 Lagged → 4003 lagged Close 帧
//!    13. ws_e2e_pong_timeout_closes_dead_peer  — 不回任何帧 → 1011 pong timeout Close 帧
//!    14. ws_e2e_conn_registry_counts           — 连接表 register/unregister 计数
//!    15. ws_e2e_server_shutdown_sends_1012     — shutdown.cancel() → 1012 server restart
//!    16. ws_e2e_reauth_failure_sends_4001_close — 吊销 session → 周期 re-auth 失败 → 4001
//!                                                  （reason `auth expired`）
//!    17. ws_e2e_access_token_expiry_sends_4001_with_access_token_expired
//!                                                  — access JWT 过期（session 仍活）→
//!                                                    4001（reason `access token expired`）
//!
//!   HTTP 端点：
//!    18. http_snapshot_unauthenticated_returns_401
//!    19. http_snapshot_happy_path_returns_full_shape
//!    20-23. `GET /dashboard/upcoming-delivery`：缺省 days=14 / days=7 / basis 切换 /
//!         basis 非法值 → 400 + today 字段
//!    24-26. `GET /dashboard/delivery-orders`：date / statuses 的 40001 契约 + 正常返回
//!
//! （`statuses` 的限长闸门 `STATUSES_MAX_ITEMS` / `STATUSES_MAX_RAW_LEN` 由 lib 单测
//!   `handler::tests::statuses_filter_*` 覆盖，不进本 binary。）
//!
//! 测试栈：必须建 Redis pool，session 写入才算「已吊销」
//!
//! ## Fixture 范本化（2026-09-24 PR13 Phase I）
//! 本文件走 `use hsh_erp_test_support::*` + `load_dashboard_ws_fixture(&pool)` +
//! `DashboardWsFixture` + 局部 helper。fixture 只提供 1 个 WS 验签 user baseline；
//! 其余业务数据每个用例现场插。
//!
//! ## 雪花 ID 统一走全进程共享 generator（2026-10-09）
//! 24 处用例内 `SnowflakeIdGenerator::new(1_577_836_800_000, 1)` 全部删除，改为
//! `shared_test_snowflake().next_id()`。根因：位布局 `ts << 22 | instance << 12 | seq`
//! 里 `last_ms` / `sequence` 是 generator **对象私有**字段，`new()` 从 0 起步 ⇒ 任意
//! 两个 instance 相同、对象不同的 generator 在同一毫秒各取第 0 号就发出逐字节相同的
//! id ⇒ `t_*_pkey` 23505。本文件是全仓最密的受害者：每个用例都 `new` 一个 instance=1
//! 的 generator，`insert_customer` / `insert_part` 只取 1 个号，正是「两个不同 helper
//! 各调一次就撞」的典型形态。
//!
//! 两个 helper 的 `snowflake: &SnowflakeIdGenerator` 形参（只为传号而存在）一并删除，
//! 改为函数内部自取号；38 处调用点的 `&snowflake` 实参同步删除。`mint_test_token*` 与
//! `delete_session(&jti)` 的清理粒度决定不变、其 doc 已订正（见
//! `mint_test_token_with_jti`）。

use chrono::NaiveDate;
use futures_util::{SinkExt, StreamExt};
use hsh_erp_rust::auth::jwt::encode_access;
use hsh_erp_rust::infra::clock::now_naive;
use hsh_erp_rust::infra::ws_hub::WsEvent;
use hsh_erp_rust::modules::dashboard::dto::DeliveryBasis;
use hsh_erp_rust::modules::dashboard::repo::{
    DELIVERY_BUCKET_LIMIT, DELIVERY_DETAIL_LIMIT, DELIVERY_STATUSES, DashboardRepo,
};
use hsh_erp_rust::modules::dashboard::service::DashboardService;
use hsh_erp_test_support::{
    DashboardWsFixture, json_request, load_dashboard_ws_fixture, send as ts_send, send_raw,
    shared_test_snowflake, test_app, test_pool, test_state, test_ws_app,
};
use sqlx::{Acquire, PgPool};
use std::collections::HashSet;
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

/// 插一条根客户，返回其 id（`t_part.customer_id` 是逻辑外键但 NOT NULL）。
///
/// `prefix` 必传且互不相同：`uq_t_customer_root_prefix` 对「未软删 + 根客户」的
/// `serial_prefix` 建了唯一索引，同一用例里插第二个根客户必须换前缀。
async fn insert_customer(pool: &PgPool, name: &str, prefix: &str) -> i64 {
    let id = shared_test_snowflake().next_id();
    sqlx::query(
        "INSERT INTO t_customer (id, name, parent_id, serial_prefix, version, \
         created_at, updated_at) VALUES ($1, $2, NULL, $3, 0, $4, $4)",
    )
    .bind(id)
    .bind(name)
    .bind(prefix)
    .bind(now_naive())
    .execute(pool)
    .await
    .expect("insert t_customer");
    id
}

/// 插一条工单（`t_part`），返回其 id。
///
/// `system_delivery_date` 传 `None` 即 NULL；`planned_delivery_date` 是 NOT NULL 列，
/// `request_date` 复用同一个值（本文件只关心交期两列）。
async fn insert_part(
    pool: &PgPool,
    customer_id: i64,
    status: &str,
    system_delivery_date: Option<NaiveDate>,
    planned_delivery_date: NaiveDate,
) -> i64 {
    let id = shared_test_snowflake().next_id();
    sqlx::query(
        "INSERT INTO t_part (id, name, drawing_no, applicant_name, customer_id, \
         request_date, planned_delivery_date, system_delivery_date, status, version, \
         created_at, created_by, updated_at, updated_by) \
         VALUES ($1, 'p-dash', 'DWG-D', 'tester', $2, $3, $4, $5, $6, 0, $7, NULL, $7, NULL)",
    )
    .bind(id)
    .bind(customer_id)
    .bind(planned_delivery_date)
    .bind(planned_delivery_date)
    .bind(system_delivery_date)
    .bind(status)
    .bind(now_naive())
    .execute(pool)
    .await
    .expect("insert t_part");
    id
}

#[tokio::test]
async fn build_snapshot_basic_shape() {
    // 空业务数据下：4 个字段各自的「空形态」都必须成立（overdue/in_inspection 是数字 0，
    // in_process 与 system_delivery_orders 两桶是空数组）。
    let pool = setup().await;
    let mut tx = pool.begin().await.unwrap();
    let snap = DashboardService::new()
        .build_snapshot(&mut *tx)
        .await
        .expect("snapshot ok");
    drop(tx);

    assert_eq!(snap.overdue_count, 0, "空库无逾期工单");
    assert_eq!(snap.in_inspection_count, 0, "空库无待品检批次");
    assert!(snap.in_process.is_empty(), "空库无在加工批次");
    assert!(
        snap.system_delivery_orders.urgent.is_empty(),
        "空库无最紧急工单"
    );
    assert!(
        snap.system_delivery_orders.partial.is_empty(),
        "空库无部分已交工单"
    );
    assert!(!snap.ts.is_empty());
}

#[tokio::test]
async fn build_snapshot_in_process_carries_worker_held_batch_shape() {
    // 核心回归：`in_process` 行的 7 字段按前端实际渲染装配。
    // `quantity` 取自 t_part_batch 而非 t_part（两者不同值才能钉死取列来源）。
    let pool = setup().await;
    let now = now_naive();
    let today = now.date();

    let cust_id = insert_customer(&pool, "worker_held_cust", "Z").await;
    let part_id = insert_part(&pool, cust_id, "IN_PROCESS", None, today).await;

    let worker_id = shared_test_snowflake().next_id();
    sqlx::query(
        "INSERT INTO t_worker (id, badge_code, name, is_active, version, created_at, updated_at) \
         VALUES ($1, 'B-WH', '王五', true, 0, $2, $2)",
    )
    .bind(worker_id)
    .bind(now)
    .execute(&pool)
    .await
    .expect("insert t_worker");

    let batch_id = shared_test_snowflake().next_id();
    sqlx::query(
        "INSERT INTO t_part_batch (id, part_id, batch_no, quantity, status, location, \
         current_holder_id, version, created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, 1, 7, 'IN_PROCESS', 'WORKER', $3, 0, $4, NULL, $4, NULL)",
    )
    .bind(batch_id)
    .bind(part_id)
    .bind(worker_id)
    .bind(now)
    .execute(&pool)
    .await
    .expect("insert t_part_batch");

    let mut tx = pool.begin().await.unwrap();
    let snap = DashboardService::new()
        .build_snapshot(&mut *tx)
        .await
        .expect("snapshot ok");
    drop(tx);

    assert_eq!(snap.in_process.len(), 1, "应有 1 行在加工批次");
    let row = &snap.in_process[0];
    assert_eq!(row.id, part_id.to_string());
    assert_eq!(
        row.batch_id.as_deref(),
        Some(batch_id.to_string()).as_deref()
    );
    assert_eq!(row.quantity, 7, "quantity 应取 t_part_batch.quantity");
    assert_eq!(
        row.current_holder_id.as_deref(),
        Some(worker_id.to_string()).as_deref()
    );
    assert_eq!(row.worker_name.as_deref(), Some("王五"));
    assert!(!row.is_urgent);
}

// upcoming_delivery 桶按 OrderStatus 细分计数集成测试
// （覆盖 repo 层 SQL `GROUP BY (date, status)` + VO `by_status` 字段）。
#[tokio::test]
async fn snapshot_counters_by_status_returns_per_status_breakdown() {
    let pool = setup().await;
    let now = now_naive();
    // 沿「今天」的服务端口径（now_naive = Asia/Shanghai），避免本地时区漂移
    let today = now.date();
    let day_after_2 = today + chrono::Duration::days(2);

    // 1 个 customer（t_part.customer_id NOT NULL 强制）
    let cust_id = insert_customer(&pool, "by_status_cust", "B").await;

    // today：3 PENDING + 2 INSPECTION + 1 DELIVERED（count=6）
    for status in &[
        "PENDING",
        "PENDING",
        "PENDING",
        "INSPECTION",
        "INSPECTION",
        "DELIVERED",
    ] {
        insert_part(&pool, cust_id, status, Some(today), today).await;
    }

    // today+2：1 PROGRAMMING（count=1）
    insert_part(&pool, cust_id, "PROGRAMMING", Some(day_after_2), today).await;

    let mut tx = pool.begin().await.unwrap();
    let out = DashboardService::new()
        .build_upcoming_buckets(&mut *tx, None, None)
        .await
        .expect("buckets ok");
    drop(tx);

    // 必有 14 桶（默认 14 天）
    assert_eq!(out.buckets.len(), 14);
    assert_eq!(
        out.today,
        today.format("%Y-%m-%d").to_string(),
        "VO 的 today 应与桶序列起点同源"
    );
    assert_eq!(
        out.buckets[0].date, out.today,
        "首桶日期必须等于 today（同一次时钟取值）"
    );

    // today 桶：count=6，by_status 三 key
    let today_bucket = &out.buckets[0];
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
    let d2_bucket = &out.buckets[2];
    assert_eq!(d2_bucket.date, day_after_2.format("%Y-%m-%d").to_string());
    assert_eq!(d2_bucket.count, 1);
    assert_eq!(d2_bucket.by_status.get("PROGRAMMING"), Some(&1));
    assert_eq!(d2_bucket.by_status.len(), 1);

    // 其它 12 天桶（14 - today/today+2 = 12）：count=0，by_status 空 map
    for (idx, b) in out.buckets.iter().enumerate() {
        if idx == 0 || idx == 2 {
            continue;
        }
        assert_eq!(b.count, 0, "day idx={idx} count 应为 0");
        assert!(b.by_status.is_empty(), "day idx={idx} by_status 应为空 map");
    }
}

/// `snapshot_counters` 的窗口下界必须锚在**传入的 `today`** 上，不能是 SQL 里的
/// `CURRENT_DATE`（2026-10-07 补）。
///
/// ## 怎么构造出可证伪的场景
/// 会话时区显式锁成 UTC（`SET LOCAL TIME ZONE`，与容器默认值、宿主时区都无关），
/// 然后传入一个**严格早于 DB 今日 3 天**的 `today`。此刻 `CURRENT_DATE` 与传入
/// `today` 相差 3 天，两种实现给出的分桶完全不同：
/// - 窗口下界写 `CURRENT_DATE`：锚在 `today` 的行被 WHERE 排除 → 桶 0 恒 0，
///   DB 今日的行反落进桶 0，末桶恒空；
/// - `today` 绑进 `$2`：桶 0 拿到锚在 `today` 的行，DB 今日的行落到对应 offset。
///
/// 断言里刻意避开「桶日期序列首元素 == today」这种两套实现都能过的弱断言，
/// 改用**每桶计数**——只有窗口下界真取自 `today` 时才成立。
#[tokio::test]
async fn snapshot_counters_window_anchors_on_passed_today_not_current_date() {
    const DAYS: i64 = 7;
    let pool = setup().await;
    // serial_prefix 是 varchar(1)，根客户前缀按用例取单字符（每个用例独立库，无冲突）
    let cust_id = insert_customer(&pool, "anchor_cust", "N").await;

    let mut conn = pool.acquire().await.unwrap();
    let mut tx = conn.begin().await.unwrap();
    // 锁死会话时区，让 `CURRENT_DATE` 与传入 today 的关系可控、可复现
    sqlx::query("SET LOCAL TIME ZONE 'UTC'")
        .execute(&mut *tx)
        .await
        .unwrap();
    let db_today: NaiveDate = sqlx::query_scalar("SELECT CURRENT_DATE")
        .fetch_one(&mut *tx)
        .await
        .unwrap();

    // 传入一个严格早于 DB 今日 3 天的 today（= DB 今日 - 3）
    let today = db_today - chrono::Duration::days(3);

    // 四行数据，锚点分别落在：桶 0（today）、桶 3（DB 今日 = today+3）、
    // 末桶（today+DAYS-1 = today+6）、以及窗口右开边界外（today+DAYS）。
    // 状态取 IN_PROCESS（在 `status NOT IN ('COMPLETED','CANCELLED')` 白名单内）。
    let anchors = [
        today,                                    // 桶 0
        today + chrono::Duration::days(3),        // 桶 3（= DB 今日）
        today + chrono::Duration::days(DAYS - 1), // 末桶 6
        today + chrono::Duration::days(DAYS),     // 窗口右开边界外
    ];
    for anchor in anchors {
        insert_part(&pool, cust_id, "IN_PROCESS", Some(anchor), today).await;
    }

    let out = DashboardRepo::snapshot_counters(&mut tx, today, DAYS, DeliveryBasis::System)
        .await
        .expect("snapshot_counters ok");
    drop(tx);

    assert_eq!(out.len(), DAYS as usize, "桶数恒等于传入的 days");
    assert_eq!(
        out[0].date,
        today.format("%Y-%m-%d").to_string(),
        "桶序列起点必须是传入的 today"
    );

    // 核心断言：窗口下界取自传入 today。若 SQL 仍用 CURRENT_DATE（= today+3），
    // 锚在 today 的这行会被排除、这里就变成 0。
    assert_eq!(
        out[0].count, 1,
        "桶 0 必须收到锚在传入 today 上的行（SQL 窗口下界取自 today 而非 CURRENT_DATE）"
    );

    // DB 今日的行必须落在「today+3」这一桶，而不是像 CURRENT_DATE 实现那样落进桶 0。
    assert_eq!(
        out[3].date,
        (today + chrono::Duration::days(3))
            .format("%Y-%m-%d")
            .to_string()
    );
    assert_eq!(
        out[3].count, 1,
        "锚在 DB 今日的行必须按 today 锚点落到 offset=3 的桶，而不是 CURRENT_DATE 锚点的桶 0"
    );

    // 末桶非零 ⇒ 窗口右端同样是 `today + DAYS`（若右端跟 CURRENT_DATE 前移，
    // `today+6` 会在窗口外、末桶恒 0）。
    assert_eq!(
        out[(DAYS - 1) as usize].count,
        1,
        "末桶必须收到锚在 today+DAYS-1 上的行（窗口右端 = today + days）"
    );

    // 窗口右开：`today+DAYS` 那一行不在任何桶里（sum 应为 3 而非 4）
    let sum: i64 = out.iter().map(|b| b.count).sum();
    assert_eq!(sum, 3, "窗口右开，today+DAYS 那行不应计入任何桶");
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
/// 为什么按 jti 精确定位（本文件唯一使用 `mint_test_token_with_jti` 的用例是
/// `ws_e2e_reauth_failure_sends_4001_close`，它要**只吊销自己那条** session）：
/// session 的 Redis key 是 `session:tok:<jti>`，jti 由 `encode_access` 从 token
/// 派生。`delete_all_user_sessions(user_id)` 是「按用户批量清空」——清理粒度粗到
/// 会把同 user_id 名下**所有** session 一并删掉，即便本轮各用例拿到的 user_id 已
/// 互不相同，它仍然把「本测试造的那条 session」和「未来/其它路径造的同 user_id
/// session」混为一谈。按 jti 删是**更精确的清理粒度**：只动本 token 派生出的那一个
/// key，不依赖 user_id 是否唯一、也不受同 user_id 其它 session 影响。
///
/// 2026-10-09 订正本 doc 的成因描述：本文件 24 处 `SnowflakeIdGenerator::new(
/// 1_577_836_800_000, 1)`（各自 `instance = 1` 的独立 generator 对象）已全部改为
/// `shared_test_snowflake().next_id()`。旧文字把撞 id 的成因说成「所有用例 generator
/// 写死 `instance = 1`，所以并行用例之间 user_id 会撞」——成因层级说错了：撞号发生在
/// **同一进程内的多个 generator 对象**之间（`last_ms` / `sequence` 是对象私有字段，
/// `new()` 从 0 起步 ⇒ 同 instance + 同毫秒 + 同 seq ⇒ 逐字节相同的 id），并非跨进程
/// instance 冲突。改用全进程共享 generator 后各用例的 user_id 由共享对象按
/// `next_id()` 调用顺序串行发号、进程内天然唯一，**该撞号成因已消除**；按 jti 精确删除
/// 的做法保留下来 —— 它本身是对的（粒度更精确），只是不再是「绕开撞号」的手段。
async fn mint_test_token_with_jti(
    state: &Arc<hsh_erp_rust::state::AppState>,
    user_id: i64,
) -> (String, String) {
    mint_test_token_with_ttl(state, user_id, state.config.jwt.access_ttl_seconds).await
}

/// 2026-10-09 新增：`mint_test_token_with_jti` 的 TTL 可注入版本（默认 TTL = 配置里的
/// `jwt.access_ttl_seconds`）。
///
/// 存在理由：`ws_e2e_access_token_expiry_sends_4001_with_access_token_expired` 要复现
/// 「**握手时有效、握手后过期**」的 access JWT（线上空闲用户被踢下线的成因），只能靠
/// 一枚极短 TTL 的 token —— 缺省 900s 等不起。其余语义（写 Redis session、返回
/// `(token, jti)`）与 `mint_test_token_with_jti` 完全一致。
///
/// ⚠️ 短 TTL token 在 `decode_access` 的 30s exp leeway 内**仍然有效**（见该常量的
/// doc），所以「过期」不是 ttl 那一刻发生，而是 ttl + 30s 之后。
async fn mint_test_token_with_ttl(
    state: &Arc<hsh_erp_rust::state::AppState>,
    user_id: i64,
    ttl_seconds: i64,
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
        ttl_seconds,
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
    let user_id = shared_test_snowflake().next_id();
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
    let data = &v["data"];
    assert!(
        data["overdue_count"].is_number(),
        "overdue_count 应为 number"
    );
    assert!(
        data["in_inspection_count"].is_number(),
        "in_inspection_count 应为 number"
    );
    assert!(data["in_process"].is_array(), "in_process 应为 array");
    assert!(
        data["system_delivery_orders"]["urgent"].is_array(),
        "system_delivery_orders.urgent 应为 array"
    );
    assert!(
        data["system_delivery_orders"]["partial"].is_array(),
        "system_delivery_orders.partial 应为 array"
    );

    // 2026-10-07：外层 envelope 的 `ts` 与嵌套 `data.ts` 必须同格式、都锁死
    // Asia/Shanghai。回归点是外层曾用 `chrono::Local::now()`——它跟**宿主**时区走，
    // 于是同一帧里两层 `ts` 可能给出两种时区表示（CI 宿主非 +08 时即刻现形）。
    for (label, ts) in [("外层 ts", &v["ts"]), ("data.ts", &data["ts"])] {
        let ts = ts.as_str().unwrap_or_else(|| panic!("{label} 应为 string"));
        assert!(
            ts.ends_with("+08:00"),
            "{label} 必须带 Asia/Shanghai 固定偏移 +08:00，实际 {ts}"
        );
        assert!(
            ts.contains('T') && ts.rfind('+') > ts.find('T'),
            "{label} 应为 RFC3339（date<T>time+offset），实际 {ts}"
        );
    }

    // 主动关 socket 避免 graceful_shutdown 死等
    let _ = ws.close(None).await;
}

#[tokio::test]
async fn ws_e2e_valid_token_receives_heartbeat_text() {
    let (base, state) = spawn_ws_server().await;
    let user_id = shared_test_snowflake().next_id();
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
    let user_id = shared_test_snowflake().next_id();
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
    let user_id = shared_test_snowflake().next_id();
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
    let user_id = shared_test_snowflake().next_id();
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

/// 周期性 re-auth 失败（session 被吊销）→ 服务端发 `4001 auth expired`
/// Close 帧（前端契约：清本地 token 跳登录页，**不要**重连）。
///
/// 依赖「re-auth 周期可注入」（`AppConfig::ws_reauth_every_n_heartbeats`）：test-support
/// 默认 `2` + text 心跳 1s → 每 2s 验一次，~2s 内即可验到（生产缺省是 30s × 10 ≈ 5min，
/// 那条安全核心路径在 CI 上就永远覆盖不到）。
///
/// 对应的另一半契约（基础设施故障 → `1011 re-auth unavailable`，而不是 4001）由
/// `src/modules/dashboard/handler.rs` 的单测 `reauth_infra_failure_maps_to_1011_not_4001`
/// 钉死（要端到端造「Redis 故障」需自定义 `SessionStore` 实现，代价远大于收益）。
/// 同族的另一半「access JWT 过期但 session 仍活 → reason `access token expired`」由
/// `ws_e2e_access_token_expiry_sends_4001_with_access_token_expired` 覆盖。
#[tokio::test]
async fn ws_e2e_reauth_failure_sends_4001_close() {
    let (base, state) = spawn_ws_server().await;
    let reauth_every = state.config.ws_reauth_every_n_heartbeats;
    let heartbeat = state.config.ws_heartbeat_interval_seconds;
    let user_id = shared_test_snowflake().next_id();
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
    // ⚠️ 必须按 jti 删，不能用 `delete_all_user_sessions(user_id)`：后者是「按用户批量
    // 清空」，会把同 user_id 名下所有 session 一起删掉；按 jti 删只动本 token 派生的
    // 那一个 Redis key，是更精确的清理粒度，也不依赖 user_id 是否唯一
    // （成因与 2026-10-09 订正见 `mint_test_token_with_jti` 的 doc）。
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
        "4001 Close 帧的 reason 应为 auth expired（删掉 session ⇒ 40105，是真·会话吊销）"
    );
}

/// 2026-10-09 新增：access JWT 在连接期间**自然过期**（Redis session 仍活着）→ 服务端发
/// `4001 access token expired` Close 帧。
///
/// 与 `ws_e2e_reauth_failure_sends_4001_close`（删 session ⇒ 40105 ⇒ reason
/// `auth expired`）构成本仓 re-auth 分流的**两段语义**：40102 与 40105 共用关闭码 4001、
/// 只靠 reason 串区分，前端据此分流「先 refresh 再重连」 vs 「终止会话」。这个 reason
/// 串是**前后端联合契约**，改它必须同步 `docs/api/dashboard.md` 的 WS 关闭码表。
///
/// 复现的线上形态：大屏页开着 ⇒ 用户零 HTTP 流量 ⇒ access token 自然过期 ⇒ 周期
/// re-auth 拿到 40102。此时 Redis session 因 `verify_session_token` 的滑动续期而**依旧
/// 活着**（用例末尾直接查 Redis 断言这一点），所以这条 4001 不等于「会话没了」。
///
/// 时长由两条配置/实现事实决定（都不可省，否则等不到或等太久）：
/// - `decode_access` 的 `Validation::leeway = 30`（`src/auth/jwt.rs`，容忍跨节点时钟
///   漂移）⇒ 短 TTL 的 access token 要到 `ttl + 30s` 之后才被判 `TOKEN_EXPIRED`。
/// - re-auth 周期 = `ws_heartbeat_interval_seconds`（测试配置 1s）×
///   `ws_reauth_every_n_heartbeats`（测试配置 2）= 2s；判过期后再等最多一个周期即踢连接。
/// 故本用例实际耗时 ~34s（nextest 的 `slow-timeout` 是 60s 告警 / 120s 杀，留有余量）。
#[tokio::test]
async fn ws_e2e_access_token_expiry_sends_4001_with_access_token_expired() {
    /// 短 TTL：握手时有效、几秒后过期。取值只需「远小于 leeway（30s）」即可让用例在
    /// 半分钟内跑完，同时又长到握手期（编码后 + 建连 + 首帧快照）绝无可能落在过期之后。
    const ACCESS_TOKEN_TTL_SECONDS: i64 = 2;
    /// `decode_access` 的 exp leeway（`src/auth/jwt.rs`）。此处**复制**该常量而不是
    /// import：它是「被测行为的输入」，写死让用例对 leeway 变化敏感（leeway 调大只会
    /// 让本用例等更久，不会误判；调小则等更短）。
    const JWT_EXP_LEEWAY_SECONDS: u64 = 30;

    let (base, state) = spawn_ws_server().await;
    let heartbeat = state.config.ws_heartbeat_interval_seconds;
    let reauth_every = u64::from(state.config.ws_reauth_every_n_heartbeats);
    let user_id = shared_test_snowflake().next_id();
    let (token, jti) = mint_test_token_with_ttl(&state, user_id, ACCESS_TOKEN_TTL_SECONDS).await;
    let url = format!("{base}/dashboard?token={token}");

    let (mut ws, _resp) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("WS upgrade must succeed：此刻 token 尚未过期");
    // 收首条 snapshot，确保 handler 已进主循环（re-auth 只在心跳分支里跑，早于此刻
    // 发生的任何踢出都不会被本用例观察到）
    let _ = tokio::time::timeout(Duration::from_secs(5), ws.next())
        .await
        .expect("snapshot 超时")
        .expect("ws stream closed")
        .expect("ws frame err");

    let wait = Duration::from_secs(
        ACCESS_TOKEN_TTL_SECONDS as u64 + JWT_EXP_LEEWAY_SECONDS + heartbeat * reauth_every + 8,
    );
    let reason = wait_for_close(&mut ws, 4001, wait)
        .await
        .unwrap_or_else(|e| {
            panic!(
                "access token 在连接期间过期后应在 ~{}s 内收到 4001/access token expired Close 帧\
                 （ttl={ACCESS_TOKEN_TTL_SECONDS}s + leeway={JWT_EXP_LEEWAY_SECONDS}s + \
                 heartbeat={heartbeat}s × reauth_every={reauth_every}），但：{e}",
                wait.as_secs()
            )
        });
    assert_eq!(
        reason, "access token expired",
        "40102 只说明 access JWT 过期，reason 必须与「会话被吊销」的 auth expired 分开，\
         否则前端会清掉仍有效的 refresh token 把空闲用户踢去登录页"
    );

    // 佐证这条 4001 的成因确实是「JWT 过期」而非「会话被吊销」：被踢时 Redis session
    // 仍在（前面的 re-auth 成功轮次已把它滑动续期）。若这里拿不到 session，用例就变成
    // 在测「吊销」路径，reason 断言再对也没有意义。
    assert!(
        state
            .session
            .get_session(&jti)
            .await
            .expect("get_session")
            .is_some(),
        "被踢连接时 Redis session 应仍然存在（滑动续期），否则本用例测的不是「access JWT 过期」"
    );
}

/// B2 + B4：`state.shutdown.cancel()`（生产 = Ctrl-C 优雅退出）→ 服务端发
/// `1012 server restart` Close 帧，让前端立刻重连而不是干等 TCP 超时。
#[tokio::test]
async fn ws_e2e_server_shutdown_sends_1012() {
    let (base, state) = spawn_ws_server().await;
    let user_id = shared_test_snowflake().next_id();
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
        data["overdue_count"].is_number(),
        "data.overdue_count 应为 number"
    );
    assert!(
        data["in_inspection_count"].is_number(),
        "data.in_inspection_count 应为 number"
    );
    assert!(data["in_process"].is_array(), "data.in_process 应为数组");
    assert!(
        data["system_delivery_orders"]["urgent"].is_array(),
        "data.system_delivery_orders.urgent 应为数组"
    );
    assert!(
        data["system_delivery_orders"]["partial"].is_array(),
        "data.system_delivery_orders.partial 应为数组"
    );
    assert!(
        !data["ts"].as_str().unwrap_or("").is_empty(),
        "data.ts 应非空"
    );
    // 快照端点**不再**接受 upcoming_days / basis 两个入参：分桶已拆到独立端点。
    // 显式传旧参数不应报错（Query 提取器对未知字段宽容），但也不会影响任何字段。
    let app = test_app(state.clone());
    let (status, _envelope) = send(
        app,
        json_request(
            "GET",
            "/dashboard/snapshot?upcoming_days=7&basis=planned",
            None,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK, "旧入参被忽略而不是报错");
}

// ===========================================================================
// HTTP `GET /api/v2/dashboard/upcoming-delivery?days=` / `?basis=` 端点
// ===========================================================================
//
// 覆盖 service 层 DASHBOARD_DEFAULT_DAYS=14 + clamp(1, 60) + handler 层
// UpcomingQuery.deserialize_i64_opt 解析：
//   - default_14_days_returns_14_buckets — 缺省 query 走 14 天
//   - custom_7_days_returns_7_buckets   — ?days=7 显式 7 天
//   - days_clamps_to_min_and_max        — ?days=0 → 1 / ?days=100 → 60
//   - basis_switches_delivery_date_column — 同库同数据下两口径落不同桶下标
//   - basis_invalid_value_returns_400   — ?basis=xxx → 400（纯文本 body，不走信封）
//
// 注意：`test_app` 不挂 `/api/v2` 前缀（main.rs 才挂；测试走 v2_router 原生路径）。

#[tokio::test]
async fn http_upcoming_default_14_days_returns_14_buckets() {
    // 缺省 query（无 `?days=`）：service 层 DASHBOARD_DEFAULT_DAYS=14 兜底。
    let pool = setup().await;
    let state = test_state(pool.clone()).await;
    let token = mint_test_token(&state, DashboardWsFixture::WS_USER_ID).await;
    let app = test_app(state.clone());

    let (status, envelope) = send(
        app,
        json_request("GET", "/dashboard/upcoming-delivery", None, Some(&token)),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(envelope["code"], 0);
    let buckets = envelope["data"]["buckets"]
        .as_array()
        .expect("buckets 必为 array");
    assert_eq!(
        buckets.len(),
        14,
        "缺省 query 走 service DASHBOARD_DEFAULT_DAYS=14，应返 14 条桶"
    );

    // `today` 由后端下发（2026-10-07 起前端不再用 new Date() 自算），且必须等于
    // 桶序列的起点。
    let today_str = now_naive().date().format("%Y-%m-%d").to_string();
    assert_eq!(
        envelope["data"]["today"].as_str(),
        Some(today_str.as_str()),
        "today 应为服务端口径的今天（Asia/Shanghai）"
    );
    assert_eq!(buckets[0]["date"].as_str(), Some(today_str.as_str()));
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
async fn http_upcoming_custom_7_days_returns_7_buckets() {
    // `?days=7`：clamp(1,60) 命中 7。
    let pool = setup().await;
    let state = test_state(pool.clone()).await;
    let token = mint_test_token(&state, DashboardWsFixture::WS_USER_ID).await;
    let app = test_app(state.clone());

    let (status, envelope) = send(
        app,
        json_request(
            "GET",
            "/dashboard/upcoming-delivery?days=7",
            None,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(envelope["code"], 0);
    let buckets = envelope["data"]["buckets"]
        .as_array()
        .expect("buckets 必为 array");
    assert_eq!(buckets.len(), 7, "?days=7 显式应返 7 条桶");

    let today = now_naive().date();
    let day6_str = (today + chrono::Duration::days(6))
        .format("%Y-%m-%d")
        .to_string();
    assert_eq!(buckets[6]["date"].as_str(), Some(day6_str.as_str()));
}

#[tokio::test]
async fn http_upcoming_days_clamps_to_min_and_max() {
    // ?days=0 → clamp 到 1；?days=100 → clamp 到 60。
    let pool = setup().await;
    let state = test_state(pool.clone()).await;
    let token = mint_test_token(&state, DashboardWsFixture::WS_USER_ID).await;

    let app = test_app(state.clone());
    let (status, envelope) = send(
        app,
        json_request(
            "GET",
            "/dashboard/upcoming-delivery?days=0",
            None,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(
        envelope["data"]["buckets"].as_array().unwrap().len(),
        1,
        "days=0 应 clamp 到 1"
    );

    let app = test_app(state.clone());
    let (status, envelope) = send(
        app,
        json_request(
            "GET",
            "/dashboard/upcoming-delivery?days=100",
            None,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(
        envelope["data"]["buckets"].as_array().unwrap().len(),
        60,
        "days=100 应 clamp 到 60"
    );
}

#[tokio::test]
async fn http_upcoming_basis_switches_delivery_date_column() {
    // 鉴别力设计：本库只插 **1 行** t_part，且它的两列交期分处窗口内不同下标
    // （planned = today+3 在 14 天窗口内，system = today+9 也在窗口内）——单看
    // 「桶数 = 14」两口径不可区分，必须断言**同一行落进不同的桶**才说明
    // `SQL_COUNTERS_PLANNED` / `SQL_COUNTERS_SYSTEM` 真的换了列。
    let pool = setup().await;
    let state = test_state(pool.clone()).await;
    let token = mint_test_token(&state, DashboardWsFixture::WS_USER_ID).await;
    let app = test_app(state.clone());

    let today = now_naive().date();
    let cust_id = insert_customer(&pool, "basis_cust", "Z").await;

    // 唯一 1 行 part：两列交期**故意错开**——planned 落 today+3、system 落 today+9。
    // status 取 PENDING（SQL 已排除 COMPLETED / CANCELLED）。
    insert_part(
        &pool,
        cust_id,
        "PENDING",
        Some(today + chrono::Duration::days(9)),
        today + chrono::Duration::days(3),
    )
    .await;

    // ── ?basis=planned：只认 planned_delivery_date → 落 today+3（idx 3） ──
    let (status, envelope) = send(
        app.clone(),
        json_request(
            "GET",
            "/dashboard/upcoming-delivery?basis=planned",
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
    let buckets = envelope["data"]["buckets"]
        .as_array()
        .expect("buckets 必为 array");
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
    assert_eq!(buckets[3]["by_status"]["PENDING"].as_i64(), Some(1));

    // ── ?basis=system（也是缺省口径）：只认 system_delivery_date → 落 today+9 ──
    let (status, envelope) = send(
        app.clone(),
        json_request(
            "GET",
            "/dashboard/upcoming-delivery?basis=system",
            None,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK, "?basis=system 应返 200");
    assert_eq!(envelope["code"], 0);
    let buckets = envelope["data"]["buckets"]
        .as_array()
        .expect("buckets 必为 array");
    assert_eq!(buckets.len(), 14);
    assert_eq!(
        buckets[9]["count"].as_i64(),
        Some(1),
        "system 口径应把该行计入 today+9 桶；got={}",
        buckets[9]
    );
    assert_eq!(buckets[3]["count"].as_i64(), Some(0));

    // ── 缺省（不传 basis）必须与显式 system 完全一致（DeliveryBasis::Default = System）──
    let app = test_app(state.clone());
    let (status, envelope) = send(
        app,
        json_request("GET", "/dashboard/upcoming-delivery", None, Some(&token)),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);
    let buckets = envelope["data"]["buckets"]
        .as_array()
        .expect("buckets 必为 array");
    assert_eq!(
        buckets[9]["count"].as_i64(),
        Some(1),
        "缺省 basis 应为 system（否则前端默认显示的计划交期是错的）"
    );
    assert_eq!(buckets[3]["count"].as_i64(), Some(0));
}

#[tokio::test]
async fn http_upcoming_basis_invalid_value_returns_400() {
    // `?basis=xxx` 不在 `DeliveryBasis` 的 `rename_all = "lowercase"` 变体里，
    // axum `Query` 反序列化直接返 400（纯文本 body，不走 `R<T>` 信封）。
    let pool = setup().await;
    let state = test_state(pool.clone()).await;
    let token = mint_test_token(&state, DashboardWsFixture::WS_USER_ID).await;
    let app = test_app(state.clone());

    let (status, body) = send_raw(
        app,
        json_request(
            "GET",
            "/dashboard/upcoming-delivery?basis=xxx",
            None,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(
        status,
        axum::http::StatusCode::BAD_REQUEST,
        "非法 ?basis 取值应返 400；body={body}"
    );
    assert!(
        serde_json::from_str::<serde_json::Value>(&body).is_err(),
        "400 body 应为纯文本而非 JSON 信封；body={body}"
    );
    assert!(
        body.contains("basis"),
        "400 应由 basis 参数反序列化失败触发；body={body}"
    );
}

// ===========================================================================
// 2026-10-07 新增：`snapshot.overdue_count` 逾期口径（6 组）
// ===========================================================================
//
// 逾期是**工单级**计数：装配件算 1 条，子件不重复计入（`t_part` 侧
// `assembly_id IS NULL` 排除，`t_assembly` 侧直接查装配件表）。窗口是
// `system_delivery_date < today`，状态白名单 6 态。

/// 直调 service 取逾期数（少一层 HTTP，便于逐条钉口径）。
async fn overdue_of(pool: &PgPool) -> i64 {
    let mut tx = pool.begin().await.unwrap();
    let n = DashboardService::new()
        .build_snapshot(&mut *tx)
        .await
        .expect("snapshot ok")
        .overdue_count;
    drop(tx);
    n
}

#[tokio::test]
async fn overdue_counts_assembly_once_and_skips_children() {
    // 行单位差异的核心：1 个装配件 + 2 个子件 → 逾期数 = 1（不是 2 也不是 3）。
    let pool = setup().await;
    let now = now_naive();
    let today = now.date();
    let overdue_day = today - chrono::Duration::days(3);

    let l1_id = shared_test_snowflake().next_id();
    sqlx::query(
        "INSERT INTO t_customer (id, name, parent_id, serial_prefix, version, \
         created_at, updated_at) VALUES ($1, 'asm_l1', NULL, 'Q', 0, $2, $2)",
    )
    .bind(l1_id)
    .bind(now)
    .execute(&pool)
    .await
    .unwrap();
    let l2_id = shared_test_snowflake().next_id();
    sqlx::query(
        "INSERT INTO t_customer (id, name, parent_id, serial_prefix, version, \
         created_at, updated_at) VALUES ($1, 'asm_l2', $2, NULL, 0, $3, $3)",
    )
    .bind(l2_id)
    .bind(l1_id)
    .bind(now)
    .execute(&pool)
    .await
    .unwrap();

    // 装配件本身
    let asm_id = shared_test_snowflake().next_id();
    sqlx::query(
        "INSERT INTO t_assembly (id, drawing_no, name, applicant_name, customer_id, \
         request_date, planned_delivery_date, system_delivery_date, status, version, \
         created_at, created_by, updated_at, updated_by) \
         VALUES ($1, 'DWG-A', '装配件', 'tester', $2, $3, $4, $4, 'IN_PROCESS', 0, $5, NULL, $5, NULL)",
    )
    .bind(asm_id)
    .bind(l2_id)
    .bind(overdue_day)
    .bind(overdue_day)
    .bind(now)
    .execute(&pool)
    .await
    .unwrap();

    // 2 个子件（assembly_id 非空），同样逾期
    for _ in 0..2 {
        let id = shared_test_snowflake().next_id();
        sqlx::query(
            "INSERT INTO t_part (id, name, drawing_no, applicant_name, customer_id, \
             assembly_id, request_date, planned_delivery_date, system_delivery_date, status, \
             version, created_at, created_by, updated_at, updated_by) \
             VALUES ($1, 'child', 'DWG-C', 'tester', $2, $3, $4, $5, $5, 'IN_PROCESS', 0, $6, NULL, $6, NULL)",
        )
        .bind(id)
        .bind(l2_id)
        .bind(asm_id)
        .bind(overdue_day)
        .bind(overdue_day)
        .bind(now)
        .execute(&pool)
        .await
        .unwrap();
    }

    assert_eq!(
        overdue_of(&pool).await,
        1,
        "1 个装配件 + 2 个子件 = 逾期 1 条（装配件算 1，子件不重复计入）"
    );
}

#[tokio::test]
async fn overdue_skips_null_system_delivery_date() {
    // `system_delivery_date IS NULL` 的工单不计入（无论 planned 是哪天）。
    let pool = setup().await;
    let today = now_naive().date();
    let long_ago = today - chrono::Duration::days(90);
    let cust_id = insert_customer(&pool, "null_sdd_cust", "N").await;
    insert_part(&pool, cust_id, "IN_PROCESS", None, long_ago).await;

    assert_eq!(
        overdue_of(&pool).await,
        0,
        "system_delivery_date 为 NULL 的工单不计入逾期"
    );
}

#[tokio::test]
async fn overdue_skips_soft_deleted_parts() {
    // 软删闸门：part / assembly 两侧都验。
    let pool = setup().await;
    let now = now_naive();
    let today = now.date();
    let overdue_day = today - chrono::Duration::days(3);
    let cust_id = insert_customer(&pool, "softdel_cust", "S").await;

    let part_id = insert_part(&pool, cust_id, "IN_PROCESS", Some(overdue_day), overdue_day).await;
    sqlx::query("UPDATE t_part SET deleted_at = $1 WHERE id = $2")
        .bind(now)
        .bind(part_id)
        .execute(&pool)
        .await
        .unwrap();

    let asm_id = shared_test_snowflake().next_id();
    sqlx::query(
        "INSERT INTO t_assembly (id, drawing_no, name, applicant_name, customer_id, \
         request_date, planned_delivery_date, system_delivery_date, status, version, \
         created_at, created_by, updated_at, updated_by, deleted_at) \
         VALUES ($1, 'DWG-SD', '已删装配件', 'tester', $2, $3, $4, $4, 'IN_PROCESS', 0, $5, NULL, $5, NULL, $5)",
    )
    .bind(asm_id)
    .bind(cust_id)
    .bind(overdue_day)
    .bind(overdue_day)
    .bind(now)
    .execute(&pool)
    .await
    .unwrap();

    assert_eq!(
        overdue_of(&pool).await,
        0,
        "软删的 part / assembly 都不计入逾期"
    );
}

#[tokio::test]
async fn overdue_accepts_all_six_delivery_statuses() {
    // 6 态白名单逐态计入，其中 READY_TO_SHIP 最容易漏（它在 part / assembly 两侧都合法）。
    let pool = setup().await;
    let now = now_naive();
    let today = now.date();
    let overdue_day = today - chrono::Duration::days(2);
    let cust_id = insert_customer(&pool, "six_status_cust", "W").await;

    for status in [
        "PENDING",
        "PROGRAMMING",
        "IN_PROCESS",
        "OUTSOURCE",
        "INSPECTION",
        "READY_TO_SHIP",
    ] {
        insert_part(&pool, cust_id, status, Some(overdue_day), overdue_day).await;
    }

    assert_eq!(
        overdue_of(&pool).await,
        6,
        "DELIVERY_STATUSES 的 6 个状态都应计入逾期"
    );
    // 常量字面值本身也要钉死：它与前端柱状图 `LAYERS[].statuses`
    // 是**人工同步**关系（无编译期保障），漂了不会编译失败，
    // 只会让「逾期数」与「柱状图层数」互相矛盾。
    assert_eq!(
        DELIVERY_STATUSES,
        [
            "PENDING",
            "PROGRAMMING",
            "IN_PROCESS",
            "OUTSOURCE",
            "INSPECTION",
            "READY_TO_SHIP"
        ],
        "DELIVERY_STATUSES 字面量不得漂移（与前端状态域人工同步）"
    );
}

#[tokio::test]
async fn overdue_excludes_delivered_completed_cancelled() {
    let pool = setup().await;
    let today = now_naive().date();
    let overdue_day = today - chrono::Duration::days(2);
    let cust_id = insert_customer(&pool, "terminal_cust", "T").await;

    for status in ["DELIVERED", "COMPLETED", "CANCELLED"] {
        insert_part(&pool, cust_id, status, Some(overdue_day), overdue_day).await;
    }

    assert_eq!(
        overdue_of(&pool).await,
        0,
        "DELIVERED / COMPLETED / CANCELLED 三个终态都应排除"
    );
}

#[tokio::test]
async fn overdue_excludes_today_boundary() {
    // 窗口是严格小于：`system_delivery_date == today` 不算逾期（它归面板/柱状图）。
    let pool = setup().await;
    let today = now_naive().date();
    let cust_id = insert_customer(&pool, "boundary_cust", "Y").await;

    insert_part(&pool, cust_id, "IN_PROCESS", Some(today), today).await;
    assert_eq!(
        overdue_of(&pool).await,
        0,
        "system_delivery_date == today 不计入逾期（窗口是 < today）"
    );
}

// ===========================================================================
// 2026-10-07 新增：`snapshot.system_delivery_orders` 两桶
// ===========================================================================

#[tokio::test]
async fn system_delivery_orders_split_by_delivered_quantity() {
    // 分桶判据是 `delivered_quantity`：0 → urgent，> 0 → partial。
    let pool = setup().await;
    let now = now_naive();
    let today = now.date();
    let sdd = today + chrono::Duration::days(2);
    let cust_id = insert_customer(&pool, "bucket_cust", "U").await;

    // urgent：完全没交过
    let urgent_part = insert_part(&pool, cust_id, "IN_PROCESS", Some(sdd), sdd).await;
    // partial：已交过一部分（DELIVERED 批次 quantity=4）
    let partial_part = insert_part(&pool, cust_id, "IN_PROCESS", Some(sdd), sdd).await;
    sqlx::query(
        "INSERT INTO t_part_batch (id, part_id, batch_no, quantity, status, version, \
         created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, 1, 4, 'DELIVERED', 0, $3, NULL, $3, NULL)",
    )
    .bind(shared_test_snowflake().next_id())
    .bind(partial_part)
    .bind(now)
    .execute(&pool)
    .await
    .unwrap();

    let mut tx = pool.begin().await.unwrap();
    let snap = DashboardService::new()
        .build_snapshot(&mut *tx)
        .await
        .expect("snapshot ok");
    drop(tx);

    let orders = snap.system_delivery_orders;
    assert_eq!(orders.urgent.len(), 1, "未交过的工单应落在 urgent 桶");
    assert_eq!(orders.urgent[0].id, urgent_part.to_string());
    assert_eq!(orders.urgent[0].delivered_quantity, 0);
    assert_eq!(
        orders.urgent[0].customer_name.as_deref(),
        Some("bucket_cust"),
        "客户名应被批量填上（防 N+1 的可观测结果）"
    );
    assert_eq!(orders.partial.len(), 1, "交过一部分的应落在 partial 桶");
    assert_eq!(orders.partial[0].id, partial_part.to_string());
    assert_eq!(orders.partial[0].delivered_quantity, 4);
}

#[tokio::test]
async fn system_delivery_orders_window_boundary() {
    // 窗口 `[today, today + 7)`：`== today` 计入，`== today - 1` 不计入，
    // `== today + 7` 也不计入。
    let pool = setup().await;
    let today = now_naive().date();
    let cust_id = insert_customer(&pool, "window_cust", "I").await;

    let in_today = insert_part(&pool, cust_id, "IN_PROCESS", Some(today), today).await;
    let yesterday = insert_part(
        &pool,
        cust_id,
        "IN_PROCESS",
        Some(today - chrono::Duration::days(1)),
        today,
    )
    .await;
    let day7 = insert_part(
        &pool,
        cust_id,
        "IN_PROCESS",
        Some(today + chrono::Duration::days(7)),
        today,
    )
    .await;

    let mut tx = pool.begin().await.unwrap();
    let snap = DashboardService::new()
        .build_snapshot(&mut *tx)
        .await
        .expect("snapshot ok");
    drop(tx);

    let ids: HashSet<String> = snap
        .system_delivery_orders
        .urgent
        .iter()
        .map(|o| o.id.clone())
        .collect();
    assert!(
        ids.contains(&in_today.to_string()),
        "system_delivery_date == today 应计入面板"
    );
    assert!(
        !ids.contains(&yesterday.to_string()),
        "system_delivery_date == today-1 属逾期窗口，不进面板"
    );
    assert!(!ids.contains(&day7.to_string()), "窗口右开：today+7 不计入");
}

#[tokio::test]
async fn system_delivery_orders_caps_each_bucket() {
    // 每桶独立截断到 DELIVERY_BUCKET_LIMIT。
    let pool = setup().await;
    let today = now_naive().date();
    let sdd = today + chrono::Duration::days(1);
    let cust_id = insert_customer(&pool, "cap_cust", "O").await;

    let n = DELIVERY_BUCKET_LIMIT + 5;
    let mut first_id = String::new();
    for i in 0..n {
        let id = insert_part(&pool, cust_id, "IN_PROCESS", Some(sdd), sdd).await;
        if i == 0 {
            first_id = id.to_string();
        }
    }

    let mut tx = pool.begin().await.unwrap();
    let snap = DashboardService::new()
        .build_snapshot(&mut *tx)
        .await
        .expect("snapshot ok");
    drop(tx);

    let orders = snap.system_delivery_orders;
    assert_eq!(
        orders.urgent.len(),
        DELIVERY_BUCKET_LIMIT,
        "urgent 桶应被截断到 {DELIVERY_BUCKET_LIMIT}"
    );
    assert!(orders.partial.is_empty(), "无已交批次 → partial 为空");
    // 截断取的是 SQL 排序后的前 N 条（id ASC 作为 tiebreaker，雪花 ID 单调）
    assert_eq!(orders.urgent[0].id, first_id, "截断应保留最早的一批");
}

// ===========================================================================
// 2026-10-07 新增：`GET /api/v2/dashboard/delivery-orders` 抽屉
// ===========================================================================

#[tokio::test]
async fn delivery_order_details_filters_by_date_and_statuses() {
    let pool = setup().await;
    let today = now_naive().date();
    let l1_id = insert_customer(&pool, "drawer_l1", "D").await;
    let l2_id = shared_test_snowflake().next_id();
    sqlx::query(
        "INSERT INTO t_customer (id, name, parent_id, serial_prefix, version, \
         created_at, updated_at) VALUES ($1, 'drawer_l2', $2, NULL, 0, $3, $3)",
    )
    .bind(l2_id)
    .bind(l1_id)
    .bind(now_naive())
    .execute(&pool)
    .await
    .unwrap();

    let hit = insert_part(&pool, l2_id, "IN_PROCESS", Some(today), today).await;
    // 同日但状态不在 filters 里
    insert_part(&pool, l2_id, "PENDING", Some(today), today).await;
    // 状态命中但不同日
    let other_day = today + chrono::Duration::days(1);
    insert_part(&pool, l2_id, "IN_PROCESS", Some(other_day), other_day).await;

    let mut tx = pool.begin().await.unwrap();
    let out = DashboardService::new()
        .build_delivery_order_details(&mut *tx, today, vec!["IN_PROCESS".to_string()], None)
        .await
        .expect("details ok");
    drop(tx);

    assert_eq!(out.date, today.format("%Y-%m-%d").to_string());
    assert_eq!(out.basis, "system", "缺省口径应为 system");
    assert_eq!(out.total, 1, "单日 + 单状态只应命中 1 条");
    assert_eq!(out.items.len(), 1);
    assert_eq!(out.items[0].id, hit.to_string());
    assert_eq!(
        out.items[0].customer_name.as_deref(),
        Some("drawer_l2"),
        "叶子客户名"
    );
    assert_eq!(
        out.items[0].l1_customer_name.as_deref(),
        Some("drawer_l1"),
        "L1 客户名应走 parent_id 两级批量查"
    );
    assert_eq!(
        out.items[0].planned_delivery_date,
        today.format("%Y-%m-%d").to_string(),
        "planned 列恒返回（前端按自己的 basis 选列渲染）"
    );
}

#[tokio::test]
async fn delivery_order_details_total_exceeds_items_when_truncated() {
    // 造 205 行（> DELIVERY_DETAIL_LIMIT = 200）：`total` 不受截断，`items` 被截。
    let pool = setup().await;
    let today = now_naive().date();
    let cust_id = insert_customer(&pool, "trunc_cust", "R").await;

    for _ in 0..(DELIVERY_DETAIL_LIMIT + 5) {
        insert_part(&pool, cust_id, "IN_PROCESS", Some(today), today).await;
    }

    let mut tx = pool.begin().await.unwrap();
    let out = DashboardService::new()
        .build_delivery_order_details(&mut *tx, today, vec!["IN_PROCESS".to_string()], None)
        .await
        .expect("details ok");
    drop(tx);

    assert_eq!(
        out.items.len(),
        DELIVERY_DETAIL_LIMIT as usize,
        "items 应被截断到 {DELIVERY_DETAIL_LIMIT}"
    );
    assert_eq!(
        out.total,
        DELIVERY_DETAIL_LIMIT + 5,
        "total 是匹配总数，不受 items 截断影响（前端据此显示「共 N 件」）"
    );
}

#[tokio::test]
async fn delivery_order_details_basis_switches_column() {
    // 同一行 planned / system 交期分处不同日：两口径必须打不同的列。
    let pool = setup().await;
    let today = now_naive().date();
    let planned_day = today;
    let system_day = today + chrono::Duration::days(4);
    let cust_id = insert_customer(&pool, "basis2_cust", "E").await;
    let part_id = insert_part(&pool, cust_id, "IN_PROCESS", Some(system_day), planned_day).await;

    let mut tx = pool.begin().await.unwrap();
    let by_planned = DashboardService::new()
        .build_delivery_order_details(
            &mut *tx,
            planned_day,
            vec!["IN_PROCESS".to_string()],
            Some(DeliveryBasis::Planned),
        )
        .await
        .expect("details ok");
    let by_system = DashboardService::new()
        .build_delivery_order_details(
            &mut *tx,
            system_day,
            vec!["IN_PROCESS".to_string()],
            Some(DeliveryBasis::System),
        )
        .await
        .expect("details ok");
    drop(tx);

    assert_eq!(by_planned.basis, "planned");
    assert_eq!(by_planned.total, 1);
    assert_eq!(by_planned.items[0].id, part_id.to_string());

    assert_eq!(by_system.basis, "system");
    assert_eq!(by_system.total, 1);
    assert_eq!(by_system.items[0].id, part_id.to_string());

    // 反向：另一天在本口径下不该命中任何行
    let mut tx = pool.begin().await.unwrap();
    let miss = DashboardService::new()
        .build_delivery_order_details(
            &mut *tx,
            system_day,
            vec!["IN_PROCESS".to_string()],
            Some(DeliveryBasis::Planned),
        )
        .await
        .expect("details ok");
    drop(tx);
    assert_eq!(miss.total, 0, "planned 口径下 system_day 不该命中");
}

#[tokio::test]
async fn http_delivery_orders_requires_date() {
    // 缺 date / 非法 date 都走 40001（AppError::validation），且 body 走 R<T> 信封。
    let pool = setup().await;
    let state = test_state(pool.clone()).await;
    let token = mint_test_token(&state, DashboardWsFixture::WS_USER_ID).await;

    let app = test_app(state.clone());
    let (status, envelope) = send(
        app,
        json_request(
            "GET",
            "/dashboard/delivery-orders?statuses=IN_PROCESS",
            None,
            Some(&token),
        ),
    )
    .await;
    assert!(status.is_client_error(), "缺 date 应是 4xx；got {status}");
    assert_eq!(envelope["code"], 40001, "缺 date → 40001");

    let app = test_app(state.clone());
    let (status, envelope) = send(
        app,
        json_request(
            "GET",
            "/dashboard/delivery-orders?date=2026-13-99&statuses=IN_PROCESS",
            None,
            Some(&token),
        ),
    )
    .await;
    assert!(status.is_client_error(), "非法 date 应是 4xx；got {status}");
    assert_eq!(envelope["code"], 40001, "非法 date → 40001");
}

#[tokio::test]
async fn http_delivery_orders_requires_statuses() {
    let pool = setup().await;
    let state = test_state(pool.clone()).await;
    let token = mint_test_token(&state, DashboardWsFixture::WS_USER_ID).await;
    let today = now_naive().date().format("%Y-%m-%d").to_string();

    // 缺 statuses
    let app = test_app(state.clone());
    let (status, envelope) = send(
        app,
        json_request(
            "GET",
            &format!("/dashboard/delivery-orders?date={today}"),
            None,
            Some(&token),
        ),
    )
    .await;
    assert!(
        status.is_client_error(),
        "缺 statuses 应是 4xx；got {status}"
    );
    assert_eq!(envelope["code"], 40001, "缺 statuses → 40001");

    // 空白 statuses（全是逗号 + 空格）
    let app = test_app(state.clone());
    let (status, envelope) = send(
        app,
        json_request(
            "GET",
            &format!("/dashboard/delivery-orders?date={today}&statuses=%20,%20,"),
            None,
            Some(&token),
        ),
    )
    .await;
    assert!(
        status.is_client_error(),
        "空 statuses 应是 4xx；got {status}"
    );
    assert_eq!(envelope["code"], 40001, "空 statuses → 40001");
}

#[tokio::test]
async fn http_delivery_orders_happy_path() {
    let pool = setup().await;
    let state = test_state(pool.clone()).await;
    let token = mint_test_token(&state, DashboardWsFixture::WS_USER_ID).await;
    let today = now_naive().date();
    let cust_id = insert_customer(&pool, "http_cust", "H").await;
    let part_id = insert_part(&pool, cust_id, "IN_PROCESS", Some(today), today).await;

    let app = test_app(state.clone());
    let (status, envelope) = send(
        app,
        json_request(
            "GET",
            &format!(
                "/dashboard/delivery-orders?date={}&statuses=IN_PROCESS,PENDING",
                today.format("%Y-%m-%d")
            ),
            None,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(envelope["code"], 0);
    let data = &envelope["data"];
    assert_eq!(data["date"], today.format("%Y-%m-%d").to_string().as_str());
    assert_eq!(data["basis"], "system");
    assert_eq!(data["total"], 1);
    assert_eq!(data["items"][0]["id"], part_id.to_string());
    assert!(
        data["items"][0]["id"].is_string(),
        "雪花 id 序列化为字符串（防 JS 精度截断）"
    );
    assert!(data["total"].is_number(), "total 是裸 number，不字符串化");
    assert!(!data["ts"].as_str().unwrap_or("").is_empty());
}

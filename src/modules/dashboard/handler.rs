//! dashboard WebSocket handler（2026-09-15 takeover-fill + followup-cleanup +
//! 2026-09-22 Group E 重构）
//!
//! 路径：`GET /ws/dashboard?token=<JWT>`（由 `dashboard::mod::router()` 桥接，
//! 再由 `modules::ws_router()` 在 `/ws` 前缀下挂）。
//!
//! ## 实现要点
//! - query token 鉴权：走 `auth::middleware::verify_session_token` 共享核验函数
//!   （与 HTTP middleware 同源：Bearer JWT 验签 + iss 校验 + Redis session 校验
//!   + 滑动 TTL；本 handler 不重复实现，2026-09-20 重构）
//! - WS 不走 axum middleware（query-token 而非 Bearer；WS upgrade 帧也无法被
//!   HTTP middleware 拦截），故单独调一次 `verify_session_token`
//! - 任意已登录（*）即可连接
//! - WS 升级：`axum::extract::ws::WebSocketUpgrade`
//! - 业务事件订阅：把 `state.ws_hub.broadcast` 上的 `WsEvent::DashboardEvent`
//!   透传给本连接
//! - 心跳：30s 周期发 `WsHeartbeatMsg` 作为 **text** 帧（浏览器 JS `onmessage` 可直接收到；
//!   原 `Message::Ping` 浏览器不会触发 `onmessage`，前端无法感知；2026-09-15 followup A6 改）。
//!   间隔由 `state.config.ws_heartbeat_interval_seconds` 控制，生产 30s，测试可调小。
//! - 推一次 `WsSnapshotMsg` 立即下发
//!
//! ## 2026-10-01 WS 健壮性加固（4 项）
//! 1. **B1 `RecvError::Lagged` 不再静默吞掉**：原先 `Err(_)` 同时匹配 `Lagged(n)` 与
//!    `Closed`，而 `Lagged` 意味 tokio **已永久丢弃**该连接的 n 条消息。丢的是
//!    `DashboardEvent` 帧，前端更新语义是「WS 事件 → invalidate → HTTP 重取」而非增量
//!    patch，故丢 1 个事件 = 对应区块永不刷新，且当时服务端零日志、客户端零感知。
//!    现拆成两分支：`Lagged` → `warn!` + 发 `4003` Close 帧 + 断开（强制前端重连 →
//!    一次全量 HTTP 重取，比补发 n 条后续事件更便宜也更正确）；`Closed` → `info!` + 断开。
//! 2. **B2 补 Close 帧**：此前全仓零 `sender.send(Message::Close(..))`，6 条退出路径全是
//!    裸 drop，浏览器只见 1006，无法区分「服务端主动踢 / 网络断 / session 失效」。
//!    见本文件底部 `close_with`。
//! 3. **B3 接真连接表**：`handle_socket` 入口 `ws_hub.register_conn`、退出前
//!    `unregister_conn`，并在两端 `info!` 当前连接数（`ws_hub` 侧 `conns` 已私有化）。
//! 4. **B4 存活检测**：新增协议层 `Message::Ping`（`ws_ping_interval_seconds`）+ 空闲
//!    超时（`ws_pong_timeout_seconds`，**绝对 deadline** 写法，见循环内注释）+ 周期性
//!    re-auth（每 `WS_REAUTH_EVERY_N_HEARTBEATS` 次 text 心跳验一次 session，失败发 `4001`）。
//!
//! ## 2026-09-22 Group E 重构：handler 三形态 ①（snapshot 单次只读聚合）
//! `build_snapshot_msg` 走 `state.pool.begin() → state.dashboard_service.build_snapshot_with_workers(&mut tx, None) → tx.commit()`
//! 路径，commit 即结束（WS 协议不依赖 tx，handler 内已完成全部 DB 读取）。后续 ws_hub.broadcast
//! 是订阅事件模式，不再走 service、不开 tx。
//!
//! 详见本文件 module-level doc + `service/mod.rs`。

use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::body::Bytes;
use axum::extract::ws::{CloseFrame, Message, Utf8Bytes, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::response::IntoResponse;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::sync::broadcast::error::RecvError;
use tracing::{info, warn};

use crate::auth::middleware::verify_session_token;
use crate::auth::rbac::CurrentUser;
use crate::infra::ws_hub::WsEvent;
use crate::modules::dashboard::vo::{DashboardSnapshot, WsEventMsg, WsHeartbeatMsg, WsSnapshotMsg};
use crate::shared::error::{AppError, code};
use crate::shared::response::R;
use crate::shared::types::deserialize_i64_opt;
use crate::state::AppState;

/// 2026-10-01 新增：每 N 次 text 心跳 tick 周期性 re-auth 一次 session（发 `4001` 的前置）。
///
/// 为什么不是每 tick 都验：`verify_session_token` 内部有 Redis 往返（黑名单 EXISTS +
/// session 查询 + `touch_session` 滑动 TTL 续期），30s 一次对 Redis 是白扔压力。
/// 生产节奏 = 30s × 10 = **5 分钟**一次；`REDIS_SESSION_TTL_SECONDS` 默认 900s（15min），
/// 即纯开大屏、无其它 HTTP 交互的会话也能被稳定续上（见 `handle_socket` 内 re-auth 段
/// 的「滑动 TTL 副作用」注释）。
const WS_REAUTH_EVERY_N_HEARTBEATS: u32 = 10;

#[derive(Debug, Deserialize)]
pub struct WsQuery {
    pub token: Option<String>,
}

/// `GET /api/v2/dashboard/snapshot` 的 query 入参（2026-09-30 新增）。
///
/// - `upcoming_days`：未来 N 天交付分桶的天数；None / 缺省 = 14（service 层兜底）；
///   业务取值范围 1..=60（service 层 `clamp` 防御恶意大数）。
/// - 字段解析走 `deserialize_i64_opt`：None 表示缺省，Some(str) parse 为 i64；
///   非数字字符串会返 4xx（axum Query 反序列化错误）——与仓内 part 域 DTO 一致。
#[derive(Debug, Default, Deserialize)]
pub struct SnapshotQuery {
    #[serde(default, deserialize_with = "deserialize_i64_opt")]
    pub upcoming_days: Option<i64>,
}

/// `GET /ws/dashboard?token=<JWT>`
///
/// WS-only 端点（2026-09-22 Group E 重构）：
/// - 路径：`/ws/dashboard`（`modules::ws_router()` 在 `/ws` 前缀下挂，无 `/api/v2`）
/// - 鉴权：`verify_session_token`（不走 HTTP middleware；WS upgrade 帧不能被拦截）
/// - snapshot 拉取走 handler 三形态 ①（`pool.begin() → service → commit`，开 tx 仅作
///   单次只读聚合边界，commit 即结束）
/// - 后续 ws_hub.broadcast 是订阅模式，不开 tx、不再走 service
pub async fn ws_dashboard(
    State(state): State<Arc<AppState>>,
    Query(q): Query<WsQuery>,
    ws: WebSocketUpgrade,
) -> Result<impl IntoResponse, AppError> {
    // 1. 鉴权：与 HTTP middleware 同源（详见 `auth::middleware::verify_session_token`）
    let token = q
        .token
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AppError::biz(code::UNAUTHORIZED, "缺少 token 查询参数"))?;
    // 2026-09-23 重构：返回值的第二个元素从 sha256 hash 改为 JWT jti (UUID v4)，
    // WS 端点不持久化 token，仅丢弃 jti（与 HTTP middleware 同源）。
    let (user, _jti) = verify_session_token(&state, token).await?;
    // 2026-09-20 修改：username 写日志，便于按用户名排查连接异常；当前端点任意已登录即可，
    // 故不调用 user.require_role(...)。未来若加「仅 MANAGER 可见」再启用 require_role 守卫。
    info!(user_id = user.id, username = %user.username, "ws dashboard: 鉴权通过");
    let user_id = user.id;
    // 2026-10-01 新增：`username` 进连接表（`ws_hub.register_conn` 元信息，日志可定位到人）；
    // `token` 字符串进 `handle_socket` 供周期性 re-auth 用（`verify_session_token` 需要原 token）。
    let username = user.username.clone();
    let token = token.to_string();

    // 2. 升级 + 把 state + user_id / username / token 移交给子任务
    let state_clone = state.clone();
    let resp =
        ws.on_upgrade(move |socket| handle_socket(socket, state_clone, user_id, username, token));
    Ok(resp)
}

/// `GET /api/v2/dashboard/snapshot`  （2026-09-28 新增，2026-09-30 扩 query）
///
/// HTTP 全量首取大屏快照（前端 dashboard 视图走「HTTP 首取 + WS 事件 invalidate」
/// 模式）。与 `GET /ws/dashboard` 共用同一 service（`DashboardService::build_snapshot_with_workers`），
/// 返回 `R<DashboardSnapshot>`（HTTP JSON 信封）；WS 端点仍推一次 `WsSnapshotMsg` envelope
/// 保留向后兼容。
///
/// 鉴权：
/// - HTTP 走 `v2_router` 末尾的 `authenticate_middleware`（Bearer JWT + Redis session）
/// - handler 用 `CurrentUser` extractor 占位（与 WS 端点权限对齐：任意已登录；不调
///   `require_role`，原因 2026-09-15 `ws_dashboard` 注释里有说明）
///
/// Query 入参（2026-09-30 新增）：
/// - `?upcoming_days=<i64>`：未来 N 天交付分桶的天数；缺省 / 非法 → 14
///   （service 层 `unwrap_or(14).clamp(1, 60)` 兜底）。前端 dashboard 视图可
///   按用户视图范围调整柱状图横轴宽度。
///
/// 实现要点（handler 三形态 ①：snapshot 单次只读聚合）：
/// - `state.pool.begin()` 借 tx 边界
/// - `state.dashboard_service.build_snapshot_with_workers(&mut *tx, None, q.upcoming_days)` —— 同
///   `build_snapshot_msg` 内部调用的 service 方法，零新 SQL；WS 路径 `None` 透传
///   走默认 14 天（前端 WS 信封 schema 不验长度，安全）
/// - `tx.commit()` 立即结束（service 层内部 SQL 全只读，开 tx 仅作聚合边界）
/// - 不引入新错误码：DB / SQL 失败走 `AppError::from(sqlx::Error)` 通透 `R<T>` 错误码
///   段（与现有 handler 一致）；非数字 `upcoming_days` 走 axum `Query` 反序列化
///   错误自动 4xx（与 part 域 DTO 行为对齐）
pub async fn get_snapshot(
    State(state): State<Arc<AppState>>,
    Query(q): Query<SnapshotQuery>,
    _current: CurrentUser,
) -> Result<Json<R<DashboardSnapshot>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let snap = state
        .dashboard_service
        .build_snapshot_with_workers(&mut *tx, None, q.upcoming_days)
        .await?;
    tx.commit().await?;
    Ok(Json(R::ok(snap)))
}

/// 单条 WS 连接的完整生命周期：登记连接表 → 主循环 → 注销连接表。
///
/// 2026-10-01 拆成薄壳 + `run_socket` 两层，只为让 register / unregister **各只有一处**
/// ——早期 `handle_socket` 有 2 条 pre-loop 早退路径，逐条补 unregister 极易漏。
async fn handle_socket(
    socket: WebSocket,
    state: Arc<AppState>,
    user_id: i64,
    username: String,
    token: String,
) {
    // 2026-10-01 B3：进连接表（conn_id 维度，同一用户可多条）。当前无统计端点，
    // 唯一消费方是这两行 info 日志（连接数打点），后续任务再加 metrics。
    let conn_id = state.ws_hub.register_conn(user_id, &username);
    info!(
        user_id = user_id,
        username = %username,
        conn_id = conn_id,
        conns = state.ws_hub.conn_count(),
        "ws dashboard: 连接建立"
    );
    run_socket(socket, state.clone(), user_id, conn_id, &token).await;
    state.ws_hub.unregister_conn(conn_id);
    info!(
        user_id = user_id,
        username = %username,
        conn_id = conn_id,
        conns = state.ws_hub.conn_count(),
        "ws dashboard: 连接清理完成"
    );
}

async fn run_socket(
    socket: WebSocket,
    state: Arc<AppState>,
    user_id: i64,
    conn_id: u64,
    token: &str,
) {
    // 2026-10-01 B2：先 split 出写端再做任何 await——这样 pre-loop 的两条早退路径
    // 也能发 Close 帧（不必把 `socket` move 掉后又抢不回来）。
    let (mut sender, mut receiver) = socket.split();

    // 1) 立即推一次快照（handler 走 state.pool.begin() 边界）
    let snapshot_msg = match build_snapshot_msg(&state).await {
        Ok(m) => m,
        Err(e) => {
            warn!(user_id = user_id, conn_id = conn_id, error = %e, "ws dashboard: 首次快照构建失败，连接关闭");
            // 裸 drop 前先发 1011，让前端能区分「服务端构建失败」与「网络断」
            // （后者只能看到 1006）。
            close_with(&mut sender, 1011, "snapshot build failed").await;
            return;
        }
    };

    // 2) 订阅 ws_hub.broadcast
    let mut rx = state.ws_hub.subscribe();

    // 写初始快照
    if let Err(e) = send_msg(&mut sender, &snapshot_msg).await {
        warn!(user_id = user_id, conn_id = conn_id, error = %e, "ws dashboard: 推初始快照失败，关闭连接");
        close_with(&mut sender, 1011, "send snapshot failed").await;
        return;
    }

    let heartbeat_interval = Duration::from_secs(state.config.ws_heartbeat_interval_seconds.max(1));
    // 2026-09-15 followup-cleanup A5：首次 tick 不立即 fire（`interval_at` 把首次
    // 触发时刻推迟到 now + period）；原 `interval(30s)` 在 select! 第一次轮询
    // 时立刻 ready，会浪费一帧 CPU / 误导 E2E 用例把首 tick 误当成 30s 后的真心跳。
    let mut heartbeat_timer = tokio::time::interval_at(
        tokio::time::Instant::now() + heartbeat_interval,
        heartbeat_interval,
    );
    heartbeat_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    // 2026-10-01 B4：协议层 Ping 定时器。**与上面的 text 心跳并存、职责分离**：
    // - text 心跳帧 → 浏览器 JS `onmessage` 收得到，给**前端**感知用；
    // - `Message::Ping` → 浏览器按 RFC 6455 §5.5.2 在**协议栈**自动回 `Pong`（JS 完全
    //   不可见），给**服务端**存活检测用。
    // 两个间隔独立配置，刻意不合并成一个 tick。
    let ping_interval = Duration::from_secs(state.config.ws_ping_interval_seconds.max(1));
    let mut ping_timer =
        tokio::time::interval_at(tokio::time::Instant::now() + ping_interval, ping_interval);
    ping_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    // 2026-10-01 B4：pong 超时**绝对 deadline** 的基准点。必须在循环**外**维护：
    // `select!` 每轮迭代都会重建 future，若改用相对时长 `sleep(Duration)`，30s 的 text
    // 心跳本身就会让循环每 30s 迭代一次、每次都把超时重置回满 → 超时分支**永远不会触发**。
    let mut last_seen = tokio::time::Instant::now();
    let pong_timeout = Duration::from_secs(state.config.ws_pong_timeout_seconds.max(1));

    // 2026-10-01 B4：周期性 re-auth 计数器（每 N 次 text 心跳验一次 session）。
    let mut heartbeat_ticks: u32 = 0;
    // 服务优雅退出信号：`state.shutdown` 被 cancel（Ctrl-C / 测试 `shutdown.cancel()`）
    // → 发 1012 告诉前端「服务端重启，请重连」而不是让它干等到 TCP 超时。
    // 刻意**每轮重建** `cancelled()` future 而不复用同一个：`WaitForCancellationFuture`
    // 是 `!Unpin`（pin_project），没法塞进 `&mut` 分支；而 `cancelled()` 本身无状态、
    // 幂等且 cancel-safe（token 已 cancel 时新 future 立即 ready），重建无副作用。
    let shutdown = state.shutdown.clone();

    loop {
        tokio::select! {
            // 客户端发来的消息（text/binary/ping/pong/close）
            ws_msg = receiver.next() => {
                // 2026-10-01 B4：**任何**入站帧都算存活证据（业务帧 / Pong / Close），
                // 一律刷新 deadline。`None`（流自然结束）不再刷新也无妨——下面直接 break。
                last_seen = tokio::time::Instant::now();
                match ws_msg {
                    Some(Ok(Message::Close(_))) | None => {
                        info!(user_id = user_id, conn_id = conn_id, "ws dashboard: 客户端关闭/断开");
                        break;
                    }
                    Some(Ok(Message::Ping(payload))) => {
                        if sender.send(Message::Pong(payload)).await.is_err() {
                            close_with(&mut sender, 1001, "write failed").await;
                            break;
                        }
                    }
                    Some(Ok(Message::Pong(_))) => {
                        // 静默续命（last_seen 已在上方刷新）
                    }
                    Some(Ok(Message::Text(_))) | Some(Ok(Message::Binary(_))) => {
                        // dashboard WS 暂不接收客户端业务指令（v1 的 subscribe 控制帧
                        // 由 send-on-connect 替代，简化协议）
                    }
                    Some(Err(e)) => {
                        warn!(user_id = user_id, conn_id = conn_id, error = %e, "ws dashboard: 接收错误，关闭连接");
                        break;
                    }
                }
            }
            // 广播来的业务事件
            broadcast = rx.recv() => {
                match broadcast {
                    Ok(WsEvent::DashboardSnapshot { data }) => {
                        // 由业务侧主动 broadcast 的快照：组装 envelope
                        let envelope = serde_json::json!({
                            "type": "snapshot",
                            "data": data,
                            "ts": chrono::Local::now().format("%Y-%m-%dT%H:%M:%S%.3f%:z").to_string(),
                        });
                        let text = Utf8Bytes::from(envelope.to_string());
                        if sender.send(Message::Text(text)).await.is_err() {
                            close_with(&mut sender, 1001, "write failed").await;
                            break;
                        }
                    }
                    Ok(WsEvent::DashboardEvent { kind, payload }) => {
                        let envelope = WsEventMsg::new(kind, payload);
                        let text = serde_json::to_string(&envelope).unwrap_or_default();
                        let text = Utf8Bytes::from(text);
                        if sender.send(Message::Text(text)).await.is_err() {
                            close_with(&mut sender, 1001, "write failed").await;
                            break;
                        }
                    }
                    // 2026-10-01 B1：`Err(_)` 拆成两个分支。`Lagged(n)` 表示 tokio 已
                    // **永久丢弃**本连接 n 条消息（队列 1024 溢出），此前被静默吞掉：
                    // 丢的是 DashboardEvent，而前端是「WS 事件 → invalidate → HTTP 重取」
                    // 语义（不是增量 patch），丢 1 条 = 对应区块永不刷新，且零日志零感知。
                    // 处理：warn 记 missed 条数 + 发 4003 踢出 → 前端重连时一次全量
                    // HTTP 重取，比补发 n 条后续事件更便宜也更正确。
                    Err(RecvError::Lagged(missed)) => {
                        warn!(
                            user_id = user_id,
                            conn_id = conn_id,
                            missed = missed,
                            "ws dashboard: 慢消费方，广播队列溢出丢事件，踢出连接（前端需重连 + 全量重取）"
                        );
                        close_with(&mut sender, 4003, "lagged").await;
                        break;
                    }
                    // 2026-10-01 B1：`Closed` = 所有 Sender 都已 drop（hub 随进程一起
                    // 走了），此时再等也不会有新事件，直接 info 收摊（无 Close 帧可发：
                    // 通道已死，发了也未必能 flush 出网）。
                    Err(RecvError::Closed) => {
                        info!(user_id = user_id, conn_id = conn_id, "ws dashboard: 广播通道已关闭");
                        break;
                    }
                }
            }
            // 30s 心跳：发 **text** 帧（浏览器 JS `onmessage` 可直接收到；
            // 原 `Message::Ping` 浏览器不会触发 `onmessage`，前端无法感知；
            // 2026-09-15 followup A6 改）。axum 协议层仍自动响应客户端的 Ping→Pong。
            _ = heartbeat_timer.tick() => {
                let ts = chrono::Utc::now().timestamp();
                let hb = WsHeartbeatMsg { msg_type: "heartbeat", ts };
                let text = match serde_json::to_string(&hb) {
                    Ok(t) => t,
                    Err(e) => {
                        warn!(error = %e, "ws dashboard: 心跳序列化失败，跳过本轮");
                        continue;
                    }
                };
                let frame = Message::Text(Utf8Bytes::from(text));
                if sender.send(frame).await.is_err() {
                    close_with(&mut sender, 1001, "heartbeat write failed").await;
                    break;
                }

                // 2026-10-01 B4：周期性 re-auth（`4001 auth expired` 的前置）。
                //
                // 为什么必须有：WS 握手的鉴权只发生在**连接建立那一刻**，之后 session 被
                // 吊销（登出 / 管理员踢 / refresh 轮转后 access jti 进黑名单）这条连接
                // 会一直收事件、一直显示大屏，直到自己心跳超时——安全上是洞。
                //
                // ⚠️ **滑动 TTL 副作用（务必知情）**：`verify_session_token` 内部会调
                // `touch_session` 给 session 续期 `REDIS_SESSION_TTL_SECONDS`。因此
                // 只要大屏页开着，该 session 就**永远不会因超时过期**（这与 HTTP 路径
                // 「每次请求滑动 TTL」的语义一致）。用户关掉页面后 WS 断开、不再续期，
                // session 才开始走向自然过期。若将来要「大屏开着但强制过期」，需给
                // session store 加「不续期」模式，不能靠 TTL 自然到期。
                heartbeat_ticks = heartbeat_ticks.wrapping_add(1);
                if heartbeat_ticks.is_multiple_of(WS_REAUTH_EVERY_N_HEARTBEATS) {
                    match verify_session_token(&state, token).await {
                        Ok(_) => {}
                        Err(e) => {
                            warn!(
                                user_id = user_id,
                                conn_id = conn_id,
                                error = %e,
                                "ws dashboard: 周期性 re-auth 失败，踢出连接"
                            );
                            close_with(&mut sender, 4001, "auth expired").await;
                            break;
                        }
                    }
                }
            }
            // 2026-10-01 B4：协议层存活探测 Ping。浏览器 / tungstenite 会在协议栈
            // 自动回 Pong，服务端靠上面「入站帧刷新 last_seen」续命。
            _ = ping_timer.tick() => {
                // 空 payload：Ping 只用「有没有回应」判定存活，不承载业务数据
                // （RFC 6455 要求 control 帧 payload ≤ 125 字节）。
                if sender.send(Message::Ping(Bytes::new())).await.is_err() {
                    close_with(&mut sender, 1001, "write failed").await;
                    break;
                }
            }
            // 2026-10-01 B4：pong 超时（对端已死 / 半开 TCP）。
            // ⚠️ 必须 `sleep_until(last_seen + pong_timeout)` 绝对 deadline，不能用
            // 相对 `sleep(Duration)`：select! 每轮重建 future，相对时长会被 30s 的
            // text 心跳不断重置 → 永不触发（详见 last_seen 声明处注释）。
            _ = tokio::time::sleep_until(last_seen + pong_timeout) => {
                warn!(
                    user_id = user_id,
                    conn_id = conn_id,
                    pong_timeout_secs = state.config.ws_pong_timeout_seconds,
                    "ws dashboard: 超过 pong_timeout 未收到任何入站帧，判定对端已死，关闭连接"
                );
                close_with(&mut sender, 1011, "pong timeout").await;
                break;
            }
            // 2026-10-01 B4：服务优雅退出 → 1012，让前端立刻重连而不是等 TCP 超时。
            _ = shutdown.cancelled() => {
                info!(user_id = user_id, conn_id = conn_id, "ws dashboard: 服务关闭中，发 1012 通知前端重连");
                close_with(&mut sender, 1012, "server restart").await;
                break;
            }
        }
    }
}

/// 拉一次快照并组装 envelope（handler 三形态 ①：开 tx → service → commit）。
///
/// 2026-09-22 Group E 重构：从 `state.dashboard_service`（不是业务层 `AppState`
/// 直接持有）取 service 实例；service 方法签名改 `<R: DashboardRepoTrait>(&self,
/// mut repo: R, ...)` by-value，handler 借 `&mut *tx` 喂给 trait（trait 已直接
/// `impl for &mut PgConnection`，2026-09-22 同 iam 范式）。
async fn build_snapshot_msg(state: &AppState) -> Result<String, AppError> {
    let mut tx = state.pool.begin().await?;
    // 2026-09-30 新增 days 形参：WS 握手 snapshot 与 HTTP `/snapshot` 共享 service，
    // WS 路径无 query，固定 `None` 走 service 默认 14 天（与 HTTP 缺省值对齐）；
    // 前端 WS 信封 schema 不验长度，透传对前端透明。
    let snap = state
        .dashboard_service
        .build_snapshot_with_workers(&mut *tx, None, None)
        .await?;
    tx.commit().await?;
    let envelope = WsSnapshotMsg::new(snap);
    Ok(serde_json::to_string(&envelope).unwrap_or_default())
}

async fn send_msg(
    sender: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    text: &str,
) -> Result<(), axum::Error> {
    sender
        .send(Message::Text(Utf8Bytes::from(text.to_string())))
        .await
}

/// 主动关闭连接并带上业务 Close code（2026-10-01 新增，B2）。
///
/// ## 为什么需要它
/// 2026-10-01 之前全仓**零** `sender.send(Message::Close(..))`：所有退出路径都是裸 drop，
/// 浏览器只会看到 1006（ Abnormal Closure），无法区分「服务端主动踢 / 网络断 /
/// session 失效」三种完全不同的情况，排查时只能猜。
///
/// ## code 段约定（RFC 6455 §7.4.2）
/// - `1000-2999` 协议保留段：`1000` normal / `1001` going away / `1011` internal error /
///   `1012` service restart。本文件实际用到 `1001` / `1011` / `1012`。
/// - `4000-4999` 应用私有段：`4001` auth expired（session 失效）/ `4003` lagged（慢消费方）。
///   与后端错误码段（`40100` 等 HTTP 信封码）刻意分开——WS Close code 是 2 字节 u16，
///   与 HTTP 信封是两套协议。
///
/// ## 为什么「`send(带 code 的 Close 帧)` + 再 `close()`」两步
/// `SplitSink::close()` 本身**不接受** code 参数（`SinkExt::close(&mut self)` 无参），
/// 单靠它发出去的 Close 帧没有业务 code，客户端只能拿到 1005/1006，等于没解决问题。
/// 因此顺序是：先用 `send` 把**带 code 的** Close 帧写出（tungstenite 内部
/// `Message::Close(c) → context.close(c)`：置 ClosedByUs + 写帧 + flush），
/// 再调 `close()` 走 `poll_close` 把关闭握手驱动到底（此时状态已是 ClosedByUs，
/// 不会重复写第二个 Close 帧，只做收尾 flush）。
async fn close_with(
    sender: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    code: u16,
    reason: &str,
) {
    let frame = CloseFrame {
        code,
        reason: Utf8Bytes::from(reason.to_string()),
    };
    if let Err(e) = sender.send(Message::Close(Some(frame))).await {
        warn!(error = %e, code = code, reason = reason, "ws dashboard: 发送 Close 帧失败（对端可能已断开）");
        return;
    }
    // 收尾：驱动关闭握手（Close 回声 / 底层 flush）。失败同样只记 warn——连接本就要断。
    if let Err(e) = sender.close().await {
        warn!(error = %e, code = code, reason = reason, "ws dashboard: 关闭握手收尾失败");
    }
}

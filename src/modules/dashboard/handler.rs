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
//! 2. **B2 补 Close 帧**：此前全仓零 `sender.send(Message::Close(..))`，全部退出路径都是
//!    裸 drop，浏览器只见 1006，无法区分「服务端主动踢 / 网络断 / session 失效」。
//!    见本文件底部 `close_with`（review 第 1 轮后：写侧已失败的那几条路径不再重复发，
//!    故 Close 帧调用点为 6 处、退出路径 14 条——逐条见本文件末尾的表）。
//! 3. **B3 接真连接表**：`handle_socket` 入口 `ws_hub.register_conn`、退出前
//!    `unregister_conn`（返回值用于打「连接存活时长」日志），并在两端 `info!` 当前连接数。
//! 4. **B4 存活检测**：新增协议层 `Message::Ping`（`ws_ping_interval_seconds`）+ 空闲
//!    超时（`ws_pong_timeout_seconds`，**绝对 deadline** 写法，见循环内注释）+ 周期性
//!    re-auth（每 `ws_reauth_every_n_heartbeats` 次 text 心跳验一次 session）。
//!
//! ### 2026-10-02 review 第 1 轮修复（3 Major + 9 Minor）
//! - **Major-1 re-auth 失败原因分流**：`verify_session_token` 的失败被压平成一个 code，
//!   其中「Redis 瞬时故障」（`AppError::Internal` → `50000`）与「session 真失效」
//!   （`40100/40102/40105`）走同一条 `4001`。而 `4001` 按契约文档要求前端「清 token 跳登录页」
//!   → 一次 Redis 抖动会把全站用户登出。现按 `e.code()` 分流：鉴权类 → `4001`，
//!   基础设施类 → `1011 re-auth unavailable`（前端走普通重连）。
//! - **Major-2 启动期把 `pong_timeout` 下限从 `> ping` 收紧为 `>= 2 × ping`**
//!   （`config.rs::validate_ws_liveness` + 3 个单测）。
//! - **Major-3 `close_with` 加 2s 收尾超时**：半开 TCP + 发送缓冲满时 `send`/`poll_close`
//!   会长时间 `Pending`，会把 `run_socket` → `handle_socket` 的 `unregister_conn` 一起卡住
//!   → 连接表条目泄漏（正是 B4 想回收的那类连接）。改动前这些路径是裸 `break`（立即 drop），
//!   故这是本次加固引入的新阻塞点。
//! - **Minor-1 `select!` 加 `biased;`**：pong deadline 与 socket 可读同时 ready 时随机选分支，
//!   有概率选中超时分支误杀健康连接。入站分支排第一（dashboard 入站只有 Pong，饿死不了别的分支）。
//! - **Minor-2 re-auth 套 5s `timeout`**：`verify_session_token` 有 Redis 往返，卡住会
//!   阻塞整个 `select!`（`shutdown.cancelled()` 无法响应）。超时按「本轮跳过」处理，不判死。
//! - **Minor-3 写侧已失败的 5 条路径不再重复 `close_with`**（回 Pong / 广播快照 / 广播事件
//!   / text 心跳 / 协议层 Ping 写失败；reviewer 数成 4 条，实为 5 条——漏算了广播快照那处）。
//!   见末尾「退出路径全景」表。
//! - **Minor-4/5** 修正 `close_with` 与 `Closed` 分支的注释（行为不变，reviewer 已核实
//!   `close()` 不等 Close 回声、只做收尾 flush；且 `is_allowed` 对本仓 5 个 code 全 true）。
//! - **Minor-7 re-auth 周期从 `const` 改为配置项** `WS_REAUTH_EVERY_N_HEARTBEATS`，
//!   使「re-auth 失败 → 4001」这条安全核心路径能被 E2E 覆盖。
//!
//! ### 退出路径全景（14 条，其中 6 条发 Close 帧）
//! | # | 触发 | 动作 | Close code |
//! |---|---|---|---|
//! | 1 | 首次快照构建失败 | close_with + return | `1011` |
//! | 2 | 推初始快照失败 | close_with + return | `1011` |
//! | 3 | 客户端 `Close` / 流结束 | 裸 break | —（客户端已发起关闭） |
//! | 4 | 入站帧解码错误 | 裸 break | — |
//! | 5 | 回 `Pong` 写失败 | 裸 break | —（写侧已死，见 Minor-3） |
//! | 6 | 广播 snapshot 写失败 | 裸 break | —（同上） |
//! | 7 | 广播 event 写失败 | 裸 break | —（同上） |
//! | 8 | text 心跳写失败 | 裸 break | —（同上） |
//! | 9 | 协议层 `Ping` 写失败 | 裸 break | —（同上） |
//! | 10 | 广播队列溢出 `Lagged(n)` | close_with + break | `4003` |
//! | 11 | 广播通道 `Closed` | 裸 break | — |
//! | 12 | 周期性 re-auth 失败 | close_with + break | `4001` / `1011` |
//! | 13 | 超过 `pong_timeout` 无入站帧 | close_with + break | `1011` |
//! | 14 | 服务优雅退出 | close_with + break | `1012` |
//!
//! （心跳序列化失败与 re-auth 超时是 `continue` 而非退出路径，不计入。）
//!
//! 实际发出的 Close code 共 **4 个**：`1011`（4 条路径：首次快照构建失败 / 推初始快照失败 /
//! re-auth 基础设施失败 / pong 超时）、`1012`（服务重启）、`4001`（re-auth 鉴权失败）、
//! `4003`（慢消费方）。`1001` 自 Minor-3 起**无路径发出**。详见
//! `docs/api/websocket.md`「连接关闭码」。
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

/// 2026-10-02 新增（review 第 1 轮 Minor 2）：单次周期性 re-auth 的调用超时上限。
///
/// `verify_session_token` 内部有 Redis 往返（黑名单 EXISTS + session 查询 +
/// `touch_session`），Redis 卡住（连接池耗尽 / 网络分区）会把整个 `select!` 一起冻住：
/// 期间既处理不了 `shutdown.cancelled()`（`1012` 发不出），返回后 `last_seen` 也早已过期
/// → 下一轮立即以 `1011 pong timeout` 踢掉**健康**连接。套 5s timeout 后超时按
/// 「本轮跳过」处理（不判死），下一轮心跳再验。
///
/// 为什么不刷新 `last_seen`：健康客户端的 Pong 此刻已躺在 socket 接收缓冲里，
/// 解冻后 `select!` 的 `biased` 入站分支会立刻取到它并刷新 deadline（见循环内注释）。
const WS_REAUTH_CALL_TIMEOUT: Duration = Duration::from_secs(5);

/// 2026-10-02 新增（review 第 1 轮 Major-3）：`close_with` 的收尾窗口上限。
///
/// 半开 TCP（对端掉电 / NAT 静默丢映射 / 发送缓冲被慢读方填满）时
/// `SplitSink::send` 会在 `poll_ready`/`poll_flush` 上长时间 `Pending`、`close()` 会在
/// `poll_close` 上同样挂起（直到内核重传超时，可达数分钟~十几分钟）。这会连带把
/// `run_socket` → `handle_socket` 的 `unregister_conn` 卡住 → 连接表条目泄漏，而
/// 回收这类连接正是 B4 的存在意义。2s 是「足够把 Close 帧推进内核缓冲区」的量级，
/// 超时就放弃 flush 直接断（丢掉一个 Close 帧好过泄漏一条连接表条目）。
const CLOSE_FLUSH_TIMEOUT: Duration = Duration::from_secs(2);

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
    // 2026-10-02（review 第 1 轮 Minor-6）：`unregister_conn` 的返回值原本被直接丢弃，
    // 改成用它打「连接存活时长」——这是 `WsConnMeta::connected_at` 的存在理由，也是
    // 半开连接被 pong timeout 回收时唯一能看出「活了多久」的地方。日志里的
    // user_id / username 一律取自连接表（记账的真相源），而不是本地形参。
    match state.ws_hub.unregister_conn(conn_id) {
        Some(meta) => info!(
            user_id = meta.user_id(),
            username = %meta.username(),
            conn_id = conn_id,
            conns = state.ws_hub.conn_count(),
            alive_secs = meta.connected_at().elapsed().as_secs(),
            "ws dashboard: 连接清理完成"
        ),
        None => warn!(
            user_id = user_id,
            username = %username,
            conn_id = conn_id,
            conns = state.ws_hub.conn_count(),
            "ws dashboard: 退出时连接表里找不到本 conn_id（异常，疑似重复注销）"
        ),
    }
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
    //
    // 2026-10-02（review 第 1 轮 Minor-9）：原为 `.max(1)`，会把非法的 `0` **静默改写**为 1，
    // 于是「`pong_timeout >= 2 × ping_interval`」这个不变式在非 `from_env` 构造路径
    // （测试 struct literal）上被掩盖成 `1 == 1`（一连接上就可能判死），配置写错时
    // 现场完全看不出来。启动期 `config.rs::validate_ws_liveness` 已对 `from_env` 路径
    // 强校验；此处改用 `debug_assert!` 兜住绕过 `from_env` 的路径——非法值在
    // `interval_at` 处 panic 报错，比静默改写成 1 早暴露、也好定位。
    let ping_interval = Duration::from_secs(state.config.ws_ping_interval_seconds);
    let pong_timeout = Duration::from_secs(state.config.ws_pong_timeout_seconds);
    debug_assert!(
        ping_interval >= Duration::from_secs(1) && pong_timeout >= ping_interval * 2,
        "WS 存活检测配置非法：ping_interval={ping_interval:?} pong_timeout={pong_timeout:?}，\
         应满足 pong_timeout >= 2 × ping_interval（生产由 config.rs::validate_ws_liveness 保证；\
         此处多为测试 struct literal 写错）"
    );
    let mut ping_timer =
        tokio::time::interval_at(tokio::time::Instant::now() + ping_interval, ping_interval);
    ping_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    // 2026-10-01 B4：pong 超时**绝对 deadline** 的基准点。必须在循环**外**维护：
    // `select!` 每轮迭代都会重建 future，若改用相对时长 `sleep(Duration)`，30s 的 text
    // 心跳本身就会让循环每 30s 迭代一次、每次都把超时重置回满 → 超时分支**永远不会触发**。
    let mut last_seen = tokio::time::Instant::now();

    // 2026-10-01 B4 / 2026-10-02 改为配置项（review 第 1 轮 Minor 7）：周期性 re-auth
    // 计数器（每 N 次 text 心跳验一次 session）。
    //
    // 原为模块级 `const WS_REAUTH_EVERY_N_HEARTBEATS = 10`：测试配置的 text 心跳是 1s
    // → 触发一次要跑 >10s，「re-auth 失败 → 4001」这条**安全核心路径**在 CI 上永远
    // 覆盖不到。改为 `WS_REAUTH_EVERY_N_HEARTBEATS`（缺省 10 = 生产 30s × 10 ≈ 5min；
    // 启动期校验 `>= 1`）后，E2E 用例传 `2` 即可两秒内验到。
    let reauth_every = state.config.ws_reauth_every_n_heartbeats;
    let mut heartbeat_ticks: u32 = 0;
    // 服务优雅退出信号：`state.shutdown` 被 cancel（Ctrl-C / 测试 `shutdown.cancel()`）
    // → 发 1012 告诉前端「服务端重启，请重连」而不是让它干等到 TCP 超时。
    // 刻意**每轮重建** `cancelled()` future 而不复用同一个：`WaitForCancellationFuture`
    // 是 `!Unpin`（pin_project），没法塞进 `&mut` 分支；而 `cancelled()` 本身无状态、
    // 幂等且 cancel-safe（token 已 cancel 时新 future 立即 ready），重建无副作用。
    let shutdown = state.shutdown.clone();

    loop {
        // 2026-10-02（review 第 1 轮 Minor-1）：`biased;` 让分支按**书写顺序**优先，
        // 而不是 tokio 随机挑。必须这么做的原因：pong deadline 与 socket 可读
        // **同时** ready（对端的 Pong 恰好在 deadline 那一刻到达）时，随机选会有一小
        // 概率先命中超时分支 → 把**健康**连接判死。入站分支排第一即解决。
        //
        // 会不会饿死其它分支：dashboard WS 的入站流量只有客户端自动回的 Pong
        // （1 个 / ping_interval，且随收随走），达不到「持续 ready」的程度；
        // 广播分支同理（每个事件消费一次就绪）。真被灌满时最坏结果是延迟另外几个
        // 定时器的处理，而 ping/heartbeat 定时器是 `MissedTickBehavior::Delay`，
        // 不会补发一堆积压 tick。
        tokio::select! {
            biased;
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
                        // 2026-10-02（review 第 1 轮 Minor-3）：写侧已失败时不再
                        // `close_with`——`send` 已 Err 说明写端/socket 已不可用，紧接着的
                        // `send(Close)` 必然再失败一次（每条死连接多打一条 `warn!` 噪音）。
                        // 只记本条错误即 break，语义上与「裸 drop 后浏览器看到 1006」一致。
                        if let Err(e) = sender.send(Message::Pong(payload)).await {
                            warn!(user_id = user_id, conn_id = conn_id, error = %e, "ws dashboard: 回 Pong 写失败，关闭连接（写侧已不可用）");
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
                        // 同 Minor-3：写失败即 break，不再重复发 Close 帧。
                        if let Err(e) = sender.send(Message::Text(text)).await {
                            warn!(user_id = user_id, conn_id = conn_id, error = %e, "ws dashboard: 广播快照写失败，关闭连接（写侧已不可用）");
                            break;
                        }
                    }
                    Ok(WsEvent::DashboardEvent { kind, payload }) => {
                        let envelope = WsEventMsg::new(kind, payload);
                        let text = serde_json::to_string(&envelope).unwrap_or_default();
                        let text = Utf8Bytes::from(text);
                        if let Err(e) = sender.send(Message::Text(text)).await {
                            warn!(user_id = user_id, conn_id = conn_id, error = %e, "ws dashboard: 广播事件写失败，关闭连接（写侧已不可用）");
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
                    // 走了 / 被整体替换），此时再等也不会有新事件，直接 info 收摊。
                    //
                    // 2026-10-02（review 第 1 轮 Minor-5）订正注释：原注释「通道已死，
                    // 发了也未必能 flush 出网」把广播通道与 TCP socket 混为一谈——两者
                    // 毫不相干，此时 socket 完全可以发 Close 帧。这里**保持不发**的理由
                    // 改为：`Closed` 是「hub 已经没了」这种全局信号，前端该做的是重连
                    // （新连接会重新建 hub 订阅），语义上等同 1006；真要区分「进程收尾」
                    // 由 `shutdown.cancelled()` 分支发 1012 承担，职责不重叠。
                    Err(RecvError::Closed) => {
                        info!(user_id = user_id, conn_id = conn_id, "ws dashboard: 广播通道已关闭（hub 已 drop），收摊等前端重连");
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
                if let Err(e) = sender.send(frame).await {
                    warn!(user_id = user_id, conn_id = conn_id, error = %e, "ws dashboard: 心跳写失败，关闭连接（写侧已不可用）");
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
                //
                // 2026-10-02（Minor-7）：周期取配置项 `WS_REAUTH_EVERY_N_HEARTBEATS`。
                // 2026-10-02（Minor-2）：整个调用套 `WS_REAUTH_CALL_TIMEOUT`。
                heartbeat_ticks = heartbeat_ticks.wrapping_add(1);
                // `reauth_every > 0` 由启动期 `ws_reauth_config` 保证；此处再挡一道是
                // 为「绕开 from_env 的 struct literal」兜底——`is_multiple_of(0)` 会 panic，
                // 写成显式比较而不是 `.max(1)` 静默改写，语义更清楚。
                if reauth_every > 0 && heartbeat_ticks.is_multiple_of(reauth_every) {
                    match tokio::time::timeout(
                        WS_REAUTH_CALL_TIMEOUT,
                        verify_session_token(&state, token),
                    )
                    .await
                    {
                        Ok(Ok(_)) => {}
                        // Redis 卡住超过 WS_REAUTH_CALL_TIMEOUT：判「本轮跳过」而不是判死。
                        // 判死会误伤健康连接（且真正的问题在服务端，不在客户端会话）。
                        Ok(Err(e)) => {
                            // 2026-10-02（review 第 1 轮 Major-1）：**按失败原因分流**，
                            // 决策全部收在纯函数 `reauth_close_code` 里（见其 doc）。
                            let (close_code, reason) = reauth_close_code(e.code());
                            warn!(
                                user_id = user_id,
                                conn_id = conn_id,
                                error = %e,
                                error_code = e.code(),
                                close_code = close_code,
                                "ws dashboard: 周期性 re-auth 失败，踢出连接"
                            );
                            close_with(&mut sender, close_code, reason).await;
                            break;
                        }
                        Err(_elapsed) => {
                            // 超时 ≠ 会话失效。记 warn 后继续下一轮循环：连接仍在，
                            // 下一轮心跳再验（期间 Pong 仍会刷新 `last_seen`）。
                            warn!(
                                user_id = user_id,
                                conn_id = conn_id,
                                timeout_secs = WS_REAUTH_CALL_TIMEOUT.as_secs(),
                                "ws dashboard: 周期性 re-auth 超时（疑似 Redis 不可用），本轮跳过，不判死"
                            );
                        }
                    }
                }
            }
            // 2026-10-01 B4：协议层存活探测 Ping。浏览器 / tungstenite 会在协议栈
            // 自动回 Pong，服务端靠上面「入站帧刷新 last_seen」续命。
            _ = ping_timer.tick() => {
                // 空 payload：Ping 只用「有没有回应」判定存活，不承载业务数据
                // （RFC 6455 要求 control 帧 payload ≤ 125 字节）。
                if let Err(e) = sender.send(Message::Ping(Bytes::new())).await {
                    warn!(user_id = user_id, conn_id = conn_id, error = %e, "ws dashboard: Ping 写失败，关闭连接（写侧已不可用）");
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

/// 周期性 re-auth 失败 → Close code 的**唯一决策点**（2026-10-02，review 第 1 轮
/// Major-1 抽成纯函数，便于单测钉死契约）。
///
/// `verify_session_token` 的失败原因**可区分**，只是原先被压平成一个 code：
///
/// | 失败原因 | `AppError` 变体 | `e.code()` | 本函数返回 |
/// |---|---|---|---|
/// | JWT 过期 / 验签失败 | `Jwt` / `Unauthorized` | `40102` / `40100` | `4001 auth expired` |
/// | session 不存在 / jti 进黑名单 | `Biz` | `40105` | `4001 auth expired` |
/// | Redis 挂 / 连接池耗尽 | `Internal` | `50000` | `1011 re-auth unavailable` |
///
/// 原实现对以上全部一律发 `4001`，而 `docs/api/websocket.md` 规定前端对 `4001` 的动作是
/// 「**清本地 token 并跳登录页**」→ 一次 Redis 抖动 = 全站大屏收 4001 = 按自家文档把
/// 全站用户登出。fail-closed 用错了位置：这里明明能判断（能区分鉴权失败与基础设施故障），
/// 不该 fail-closed。
///
/// `_ =>` 兜底到 `1011`（可重试）而非 `4001`（终止）：新出现的错误码默认「先当服务端问题
/// 处理」，宁可多几次重连，也不要因为一个没见过的状态把用户踢去登录页。
fn reauth_close_code(err_code: i32) -> (u16, &'static str) {
    match err_code {
        code::UNAUTHORIZED | code::TOKEN_EXPIRED | code::SESSION_REVOKED => (4001, "auth expired"),
        _ => (1011, "re-auth unavailable"),
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

/// 主动关闭连接并带上业务 Close code（2026-10-01 新增，B2；2026-10-02 加超时护栏）。
///
/// ## 为什么需要它
/// 2026-10-01 之前全仓**零** `sender.send(Message::Close(..))`：所有退出路径都是裸 drop，
/// 浏览器只会看到 1006（ Abnormal Closure），无法区分「服务端主动踢 / 网络断 /
/// session 失效」三种完全不同的情况，排查时只能猜。
///
/// ## code 段约定（RFC 6455 §7.4.2）
/// - `1000-2999` 协议保留段：`1000` normal / `1001` going away / `1011` internal error /
///   `1012` service restart。本文件实际用到 `1011` / `1012`（`1001` 曾在「写失败」路径
///   上用过，但那条路径的写侧已不可用，2026-10-02 Minor-3 起改为裸 break，见模块 doc）。
/// - `4000-4999` 应用私有段：`4001` auth expired（session 失效）/ `4003` lagged（慢消费方）。
///   与后端错误码段（`40100` 等 HTTP 信封码）刻意分开——WS Close code 是 2 字节 u16，
///   与 HTTP 信封是两套协议。
/// - tungstenite 的 `CloseCode::is_allowed` 对本仓这 5 个 code（`1001` / `1011` / `1012` /
///   `4001` / `4003`）均为 true，故服务端发的 code 不会被客户端改写（不会变成
///   1002 protocol error）。review 第 1 轮 Minor-4 已核实。
///
/// ## 为什么「`send(带 code 的 Close 帧)` + 再 `close()`」两步
/// `SplitSink::close()` 本身**不接受** code 参数（`SinkExt::close(&mut self)` 无参），
/// 单靠它发出去的 Close 帧没有业务 code，客户端只能拿到 1005/1006，等于没解决问题。
/// 因此顺序是：先用 `send` 把**带 code 的** Close 帧写出（tungstenite 内部
/// `Message::Close(c) → context.close(c)`：置 ClosedByUs + 写帧 + flush），
/// 再调 `close()` 做**收尾 flush**。
///
/// 2026-10-02 订正（review 第 1 轮 Minor-4，reviewer 已核 tungstenite 源码
/// `protocol/mod.rs:601`）：`close()` **不会**等对端的 Close 回声——`if let Active`
/// 在这里不成立（状态已是 ClosedByUs），它只把已缓冲的数据 push 出网。原文案
/// 「驱动关闭握手（Close 回声）」不准确，故改为「收尾 flush」。
///
/// ## 为什么套 `CLOSE_FLUSH_TIMEOUT`（2026-10-02，review 第 1 轮 Major-3）
/// `send` → `poll_ready`/`poll_flush`、`close()` → `poll_close`，两者在发送缓冲被填满
/// （半开 TCP：对端掉电 / NAT 静默丢映射 / 慢读方）时都会返回 `Pending` 并挂到 socket
/// 写就绪，可达数分钟~十几分钟（直到内核重传超时）。那会把 `run_socket` → `handle_socket`
/// 的 `unregister_conn` 一起卡住 → **连接表条目泄漏**，而回收这类连接正是 B4 的意义。
/// 改动前这些路径是裸 `break`（立即 drop），故这是本次加固引入的新阻塞点。
/// 超时后无条件放弃 flush 直接断：丢掉一个 Close 帧 ≫ 泄漏一条连接表条目。
///
/// 实现上用 `select!` + `biased;` 而非 `tokio::time::timeout`，理由见函数体末尾注释
/// （`timeout` 会在 runtime 被饿死时**一次都不 poll** 就判超时，导致 Close 帧丢失）。
async fn close_with(
    sender: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    code: u16,
    reason: &str,
) {
    let frame = CloseFrame {
        code,
        reason: Utf8Bytes::from(reason.to_string()),
    };
    let send_and_flush = async {
        if let Err(e) = sender.send(Message::Close(Some(frame))).await {
            warn!(error = %e, code = code, reason = reason, "ws dashboard: 发送 Close 帧失败（对端可能已断开）");
            return;
        }
        // 收尾 flush（不等 Close 回声，见上方订正）。失败同样只记 warn——连接本要断。
        if let Err(e) = sender.close().await {
            warn!(error = %e, code = code, reason = reason, "ws dashboard: Close 帧收尾 flush 失败");
        }
    };
    // ⚠️ 这里刻意用 `select!` + `biased;` 而**不是** `tokio::time::timeout`（2026-10-02 修复）：
    // `timeout` 在「task 被饿死 > 2s 后重新被 poll」时会**直接判超时**（deadline 已过），
    // inner future 可能**一次都没被 poll 过** → Close 帧根本没写出去，客户端只看到 1006。
    // 这在 CI 上真实发生过（冷编译 + 并行建库把 runtime 饿几秒 → `ws_e2e_pong_timeout_
    // closes_dead_peer` 拿不到 1011）。`biased` 保证**先 poll flush**：socket 可写时
    // 无论 sleep 是否已过期都先把帧发出去，只有「确实 Pending」才落到超时分支。
    tokio::select! {
        biased;
        _ = send_and_flush => {}
        _ = tokio::time::sleep(CLOSE_FLUSH_TIMEOUT) => {
            // 超时即放弃：socket 半开，继续等只会把 unregister_conn 一起卡死（Major-3）。
            warn!(
                code = code,
                reason = reason,
                timeout_secs = CLOSE_FLUSH_TIMEOUT.as_secs(),
                "ws dashboard: Close 帧收尾超时（对端半开 / 发送缓冲满），放弃 flush 直接断开"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // =======================================================================
    // 2026-10-02 新增（review 第 1 轮 Major-1）：`reauth_close_code` 的分流契约。
    //
    // 这是「Redis 瞬时故障 ≠ session 失效」的唯一决策点，也是上一轮 fail-closed
    // 用错位置（Redis 抖动 → 4001 → 按文档把全站用户登出）的回归闸。
    // 纯函数、零 IO，直接喂 `AppError::code()` 的字面量即可。
    // =======================================================================

    /// 鉴权类失败（JWT 无效 / 过期 / session 吊销）→ `4001 auth expired`
    /// （前端契约：清本地 token 跳登录页，**不要**重连——重连会被 40105 拒）。
    #[test]
    fn reauth_auth_failures_map_to_4001() {
        assert_eq!(
            reauth_close_code(code::UNAUTHORIZED),
            (4001, "auth expired")
        );
        assert_eq!(
            reauth_close_code(code::TOKEN_EXPIRED),
            (4001, "auth expired")
        );
        assert_eq!(
            reauth_close_code(code::SESSION_REVOKED),
            (4001, "auth expired")
        );
    }

    /// 基础设施类失败（Redis 挂 / 连接池耗尽 → `50000 INTERNAL`）→ `1011`
    /// （前端走**普通重连**，不登出）。这是 Major-1 的核心断言。
    #[test]
    fn reauth_infra_failure_maps_to_1011_not_4001() {
        assert_eq!(
            reauth_close_code(code::INTERNAL),
            (1011, "re-auth unavailable"),
            "Redis 故障必须走 1011（可重连），绝不能是 4001（会让前端清 token 跳登录页）"
        );
    }

    /// 未知码兜底也走 `1011`（可重试优先于终止），避免新错误码一上线就把用户踢去登录页。
    #[test]
    fn reauth_unknown_code_defaults_to_1011() {
        assert_eq!(reauth_close_code(12345), (1011, "re-auth unavailable"));
        assert_eq!(reauth_close_code(0), (1011, "re-auth unavailable"));
    }
}

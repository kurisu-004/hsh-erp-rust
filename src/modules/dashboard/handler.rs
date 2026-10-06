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
use crate::modules::dashboard::dto::DeliveryBasis;
use crate::modules::dashboard::vo::{
    DashboardSnapshot, DeliveryOrderDetailOut, UpcomingDeliveryBuckets, WsEventMsg, WsHeartbeatMsg,
    WsSnapshotMsg,
};
use crate::shared::error::{AppError, code};
use crate::shared::response::R;
use crate::shared::types::deserialize_i64_opt;
use crate::state::AppState;

const WS_REAUTH_CALL_TIMEOUT: Duration = Duration::from_secs(5);
const CLOSE_FLUSH_TIMEOUT: Duration = Duration::from_secs(2);

/// `statuses` 参数的元素数上限。抽屉按 status 过滤，真实入参就是前端 `LAYERS[]`
/// 里的几个字面量（≤ 8），16 留了两倍余量；超过即判为误传。
const STATUSES_MAX_ITEMS: usize = 16;

/// `statuses` 参数的原始串长度上限（字节）。16 个 12 字符的状态字面量 + 分隔符
/// 约 208 字节，256 够用。防的是「几百 KB 的逗号串」被整份绑进 `text[]`。
const STATUSES_MAX_RAW_LEN: usize = 256;

#[derive(Debug, Deserialize)]
pub struct WsQuery {
    pub token: Option<String>,
}

/// `GET /upcoming-delivery` 入参。`days` 走 string-or-number 容错解析。
#[derive(Debug, Default, Deserialize)]
pub struct UpcomingQuery {
    #[serde(default, deserialize_with = "deserialize_i64_opt")]
    pub days: Option<i64>,
    #[serde(default)]
    pub basis: Option<DeliveryBasis>,
}

/// `GET /delivery-orders` 入参。
///
/// `date` / `statuses` 声明成 `Option` 而非必填字段，是为了让「缺参数」也走
/// `AppError::validation`（40001，统一响应信封）：axum 提取器对缺字段直接返 400
/// **纯文本** body，不走 `R<T>` 信封，两类错误前端得分别处理。
#[derive(Debug, Default, Deserialize)]
pub struct DeliveryOrdersQuery {
    pub date: Option<String>,
    /// 逗号分隔的 OrderStatus 字面量列表
    pub statuses: Option<String>,
    #[serde(default)]
    pub basis: Option<DeliveryBasis>,
}

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
    let (user, _jti) = verify_session_token(&state, token).await?;
    // 2026-09-20 修改：username 写日志，便于按用户名排查连接异常；当前端点任意已登录即可，
    // 故不调用 user.require_role(...)。
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

/// 大屏首帧全量快照。任何已登录用户可读（无角色闸门）。
pub async fn get_snapshot(
    State(state): State<Arc<AppState>>,
    _current: CurrentUser,
) -> Result<Json<R<DashboardSnapshot>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let snap = state.dashboard_service.build_snapshot(&mut *tx).await?;
    tx.commit().await?;
    Ok(Json(R::ok(snap)))
}

/// 交期柱状图分桶（柱状图 + 「今日到期」「N 天到期」两个 KPI 的唯一数据源）。
pub async fn get_upcoming_delivery(
    State(state): State<Arc<AppState>>,
    Query(q): Query<UpcomingQuery>,
    _current: CurrentUser,
) -> Result<Json<R<UpcomingDeliveryBuckets>>, AppError> {
    let mut tx = state.pool.begin().await?;
    let out = state
        .dashboard_service
        .build_upcoming_buckets(&mut *tx, q.days, q.basis)
        .await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// 柱状图某一天的下钻抽屉明细。
pub async fn get_delivery_orders(
    State(state): State<Arc<AppState>>,
    Query(q): Query<DeliveryOrdersQuery>,
    _current: CurrentUser,
) -> Result<Json<R<DeliveryOrderDetailOut>>, AppError> {
    let date = q
        .date
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AppError::validation("date 必填（YYYY-MM-DD）"))
        .and_then(|s| {
            chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d")
                .map_err(|_| AppError::validation(format!("date 格式非法（期望 YYYY-MM-DD）：{s}")))
        })?;

    let raw = q
        .statuses
        .as_deref()
        .ok_or_else(|| AppError::validation("statuses 必填（逗号分隔的 OrderStatus 字面量）"))?;
    let statuses = parse_status_filter(raw)?;

    let mut tx = state.pool.begin().await?;
    let out = state
        .dashboard_service
        .build_delivery_order_details(&mut *tx, date, statuses, q.basis)
        .await?;
    tx.commit().await?;
    Ok(Json(R::ok(out)))
}

/// 解析并限长 `statuses` 查询串（逗号分隔的 OrderStatus 字面量列表）。
///
/// 只做**非空 + 限长**校验，**不校验元素是否属 `DELIVERY_STATUSES`**：抽屉按层
/// 传子集，白名单之外的字面量一律查 0 行（而不是 400），前端因此可以先于后端上线
/// 新的图层状态而不必等后端放行白名单。
///
/// 限长是防误传巨串——`statuses` 会整份绑进 `status = ANY($2::varchar[])`，
/// 几百 KB 的串就是几百 KB 的绑定参数。
fn parse_status_filter(raw: &str) -> Result<Vec<String>, AppError> {
    if raw.len() > STATUSES_MAX_RAW_LEN {
        return Err(AppError::validation(format!(
            "statuses 过长（{} 字节，上限 {STATUSES_MAX_RAW_LEN}）",
            raw.len()
        )));
    }
    let statuses: Vec<String> = raw
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    if statuses.is_empty() {
        return Err(AppError::validation("statuses 至少需要一个非空状态字面量"));
    }
    if statuses.len() > STATUSES_MAX_ITEMS {
        return Err(AppError::validation(format!(
            "statuses 元素过多（{} 个，上限 {STATUSES_MAX_ITEMS}）",
            statuses.len()
        )));
    }
    Ok(statuses)
}

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
    //
    // 2026-10-02（review 第 2 轮 Nit-2）：这里**不再** `close_with(1011, ...)`。
    // 上一轮把「写侧已失败」的 5 条路径改成裸 break，本条是**同类情形**（`send_msg` 已
    // 返回 Err），却仍发 Close 帧，自相矛盾：既然写端已不可用，紧接着的 `send(Close)`
    // 必然再失败一次（每条死连接多一条 `warn!` 噪音），且「6 条发 Close 帧」这个
    // 数字本身也站不住。
    //
    // 论证（与那 5 条同源）：tokio-tungstenite 的 `max_write_buffer_size` 缺省
    // `usize::MAX` ⇒ 写缓冲永不主动限流，`Sink::send` 唯一可能的失败就是 socket 级
    // 致命错（对端已关 / EPIPE）⇒ 此时 Close 帧**物理上**发不出去。
    if let Err(e) = send_msg(&mut sender, &snapshot_msg).await {
        warn!(user_id = user_id, conn_id = conn_id, error = %e, "ws dashboard: 推初始快照失败，关闭连接（写侧已不可用）");
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
        // 2026-10-02（review 第 1 轮 Minor-1 加、第 2 轮 Minor-1 订正顺序）：`biased;`
        // 让分支按**书写顺序**取第一个 Ready ⇒ **分支顺序本身是契约**，改顺序前先读这段。
        //
        // 1) `shutdown.cancelled()` 排**第一**。它只在真正关闭时才 Ready（平时是「一次
        //    waker 检查」级别的空转，不消耗 IO），放最前保证**入站洪流也饿不死优雅退出
        //    信号**——否则对端持续以 ≥ 处理速度灌帧时 `receiver.next()` 永远 Ready，
        //    1012 永远发不出去（前端只能干等 TCP 超时）。
        // 2) 入站分支第二，**必须仍在 pong 超时之前**：pong deadline 与 socket 可读
        //    **同时** ready（对端的 Pong 恰在 deadline 那一刻到达）时，取入站以刷新
        //    `last_seen`，否则会有一小概率先命中超时分支 → 把**健康**连接判死。
        // 3) 其余分支（广播 / text 心跳 / 协议层 Ping / pong 超时）顺序无要求。
        //
        // 代价（如实记录，不粉饰）：`biased` 下**持续 Ready 的高优先级分支会永久饿死
        // 低优先级分支**（tokio 语义，不是本文件的实现缺陷）。上面的排序之所以安全，
        // 靠的是「排前面的两个分支不会持续 Ready」：`shutdown` 见 1)；入站侧 dashboard
        // 协议只回自动 Pong（1 个 / ping_interval，随收随走），真正灌帧的客户端属于
        // 异常流量——那种情况下丢掉的恰好是下面这 3 类分支：心跳停发（前端无感知）、
        // `Lagged` 检测不到（前端少刷新，靠下次事件补）、`pong timeout` 判定不了
        // （但此时入站帧一直在来，本来也不该判死）。真正不可接受的是 1012 丢失，已由 1) 解决。
        tokio::select! {
            biased;
            // 服务优雅退出 → 1012，让前端立刻重连而不是等 TCP 超时。
            // 排第一的理由见上方 1)：入站洪流不能把「服务端要重启了」这个信号饿死。
            // 2026-10-01 B4。
            _ = shutdown.cancelled() => {
                info!(user_id = user_id, conn_id = conn_id, "ws dashboard: 服务关闭中，发 1012 通知前端重连");
                close_with(&mut sender, 1012, "server restart").await;
                break;
            }
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
                // 为「绕开 from_env 的 struct literal」兜底。
                //
                // 2026-10-02 订正（review 第 2 轮 Minor-2 同类问题）：上一版注释写
                // 「`is_multiple_of(0)` 会 panic」——**错的**。已核 rust std
                // `core/src/num/uint_macros.rs`：`is_multiple_of` 的实现是
                // `match rhs { 0 => self == 0, _ => self % rhs == 0 }`，**永不 panic**。
                // 真正的后果是：计数器非 0 时 `is_multiple_of(0)` 恒为 `false`
                // ⇒ re-auth **静默永不触发**，即「session 吊销后不再踢 4001」这条安全闸
                // 被无声关掉。启动期 bail（`ws_reauth_config`）拦的就是这个。
                //
                // 写成显式 `> 0` 比较而不是 `.max(1)` 静默改写：`0 → 1` 会把「配错」
                // 伪装成「每心跳都验」（对 Redis 是无谓压力），语义不如直说清楚。
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
                // 空 payload（RFC 6455：control 帧 payload ≤ 125 字节）：Ping 只用「有没有回应」判定存活。
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
        }
    }
}

fn reauth_close_code(err_code: i32) -> (u16, &'static str) {
    match err_code {
        code::UNAUTHORIZED | code::TOKEN_EXPIRED | code::SESSION_REVOKED => (4001, "auth expired"),
        _ => (1011, "re-auth unavailable"),
    }
}

async fn build_snapshot_msg(state: &AppState) -> Result<String, AppError> {
    let mut tx = state.pool.begin().await?;
    let snap = state.dashboard_service.build_snapshot(&mut *tx).await?;
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
    // `biased` 把「先推 Close 帧、再看 deadline」这个顺序写死在代码里（语义上与
    // `tokio::time::timeout` 等价，理由与订正见上方 doc）。
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

    #[test]
    fn statuses_filter_trims_blanks_and_dedups_nothing() {
        assert_eq!(
            parse_status_filter("PENDING, IN_PROCESS ,").unwrap(),
            vec!["PENDING".to_string(), "IN_PROCESS".to_string()],
            "空白元素应被丢弃、首尾空白应被 trim"
        );
    }

    #[test]
    fn statuses_filter_accepts_status_outside_the_whitelist() {
        // 刻意不校验白名单：抽屉按层传子集，白名单外的字面量查 0 行而非 400
        assert_eq!(
            parse_status_filter("PENDING,SOME_FUTURE_STATUS").unwrap(),
            vec!["PENDING".to_string(), "SOME_FUTURE_STATUS".to_string()]
        );
    }

    #[test]
    fn statuses_filter_rejects_empty_and_blank_only() {
        for raw in ["", "   ", ",,", " , , "] {
            let err = parse_status_filter(raw)
                .err()
                .unwrap_or_else(|| panic!("{raw:?} 应被判空"));
            assert_eq!(
                err.code(),
                code::VALIDATION_ERROR,
                "{raw:?} 应走 validation 码"
            );
        }
    }

    #[test]
    fn statuses_filter_rejects_oversized_raw_and_too_many_items() {
        // 巨串：1 个超长元素，长度闸门先拦
        let huge = "X".repeat(STATUSES_MAX_RAW_LEN + 1);
        assert!(
            parse_status_filter(&huge).is_err(),
            "超过 {STATUSES_MAX_RAW_LEN} 字节的串必须被拒"
        );

        // 元素数超限：每个元素 3 字节 + 分隔符，总长在闸门内、元素数超
        let many = std::iter::repeat_n("ABC", STATUSES_MAX_ITEMS + 1)
            .collect::<Vec<_>>()
            .join(",");
        assert!(
            many.len() <= STATUSES_MAX_RAW_LEN,
            "构造的元素数用例不应先被长度闸门拦下，否则测的不是元素数闸门"
        );
        assert!(
            parse_status_filter(&many).is_err(),
            "超过 {STATUSES_MAX_ITEMS} 个元素必须被拒"
        );

        // 恰好在上限内必须放行（边界不误伤）
        let at_limit = std::iter::repeat_n("ABC", STATUSES_MAX_ITEMS)
            .collect::<Vec<_>>()
            .join(",");
        assert_eq!(
            parse_status_filter(&at_limit).unwrap().len(),
            STATUSES_MAX_ITEMS
        );
    }
}

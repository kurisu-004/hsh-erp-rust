//! dashboard WebSocket handler（2026-09-15 takeover-fill + followup-cleanup +
//! 2026-09-22 Group E 重构）
//!
//! 路径：`GET /ws/dashboard?token=<JWT>`（由 `dashboard::mod::router()` 桥接，
//! 再由 `modules::ws_router()` 在 `/ws` 前缀下挂）。
//!
//! ## 实现要点
//! - query token 鉴权：走 `auth::middleware::verify_access_token` 共享核验函数
//!   （与 HTTP middleware 同源：Bearer JWT 验签 + iss 校验 + Redis session 校验
//!   + 滑动 TTL；本 handler 不重复实现，2026-09-20 重构）
//! - WS 不走 axum middleware（query-token 而非 Bearer；WS upgrade 帧也无法被
//!   HTTP middleware 拦截），故单独调一次 `verify_access_token`
//! - 任意已登录（*）即可连接
//! - WS 升级：`axum::extract::ws::WebSocketUpgrade`
//! - 业务事件订阅：把 `state.ws_hub.broadcast` 上的 `WsEvent::DashboardEvent`
//!   透传给本连接
//! - 心跳：30s 周期发 `WsHeartbeatMsg` 作为 **text** 帧（浏览器 JS `onmessage` 可直接收到；
//!   原 `Message::Ping` 浏览器不会触发 `onmessage`，前端无法感知；2026-09-15 followup A6 改）。
//!   间隔由 `state.config.ws_heartbeat_interval_seconds` 控制，生产 30s，测试可调小。
//! - 推一次 `WsSnapshotMsg` 立即下发
//!
//! ## 2026-09-22 Group E 重构：handler 三形态 ①（snapshot 单次只读聚合）
//! `build_snapshot_msg` 走 `state.pool.begin() → state.dashboard_service.build_snapshot_with_workers(&mut tx, None) → tx.commit()`
//! 路径，commit 即结束（WS 协议不依赖 tx，handler 内已完成全部 DB 读取）。后续 ws_hub.broadcast
//! 是订阅事件模式，不再走 service、不开 tx。
//!
//! 详见本文件 module-level doc + `service/mod.rs`。

use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::response::IntoResponse;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tracing::{info, warn};

use crate::auth::middleware::verify_session_token;
use crate::infra::ws_hub::WsEvent;
use crate::modules::dashboard::dto::{WsEventMsg, WsHeartbeatMsg, WsSnapshotMsg};
use crate::shared::error::{AppError, code};
use crate::state::AppState;

#[derive(Debug, Deserialize)]
pub struct WsQuery {
    pub token: Option<String>,
}

/// `GET /ws/dashboard?token=<JWT>`
///
/// WS-only 端点（2026-09-22 Group E 重构）：
/// - 路径：`/ws/dashboard`（`modules::ws_router()` 在 `/ws` 前缀下挂，无 `/api/v2`）
/// - 鉴权：`verify_access_token`（不走 HTTP middleware；WS upgrade 帧不能被拦截）
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
    let (user, _token_hash) = verify_session_token(&state, token).await?;
    // 2026-09-20 修改：username 写日志，便于按用户名排查连接异常；当前端点任意已登录即可，
    // 故不调用 user.require_role(...)。未来若加「仅 MANAGER 可见」再启用 require_role 守卫。
    info!(user_id = user.id, username = %user.username, "ws dashboard: 鉴权通过");
    let user_id = user.id;

    // 2. 升级 + 把 state + user_id 移交给子任务
    let state_clone = state.clone();
    let resp = ws.on_upgrade(move |socket| handle_socket(socket, state_clone, user_id));
    Ok(resp)
}

async fn handle_socket(socket: WebSocket, state: Arc<AppState>, user_id: i64) {
    info!(user_id = user_id, "ws dashboard: 连接建立");

    // 1) 立即推一次快照（handler 走 state.pool.begin() 边界）
    let snapshot_msg = match build_snapshot_msg(&state).await {
        Ok(m) => m,
        Err(e) => {
            warn!(error = %e, "ws dashboard: 首次快照构建失败，连接关闭");
            return;
        }
    };

    // 2) 订阅 ws_hub.broadcast
    let mut rx = state.ws_hub.subscribe();

    let (mut sender, mut receiver) = socket.split();

    // 写初始快照
    if let Err(e) = send_msg(&mut sender, &snapshot_msg).await {
        warn!(error = %e, "ws dashboard: 推初始快照失败，关闭连接");
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

    loop {
        tokio::select! {
            // 客户端发来的消息（text/binary/ping/pong/close）
            ws_msg = receiver.next() => {
                match ws_msg {
                    Some(Ok(Message::Close(_))) | None => {
                        info!(user_id = user_id, "ws dashboard: 客户端关闭/断开");
                        break;
                    }
                    Some(Ok(Message::Ping(payload))) => {
                        if sender.send(Message::Pong(payload)).await.is_err() {
                            break;
                        }
                    }
                    Some(Ok(Message::Pong(_))) => {
                        // 静默续命
                    }
                    Some(Ok(Message::Text(_))) | Some(Ok(Message::Binary(_))) => {
                        // dashboard WS 暂不接收客户端业务指令（v1 的 subscribe 控制帧
                        // 由 send-on-connect 替代，简化协议）
                    }
                    Some(Err(e)) => {
                        warn!(user_id = user_id, error = %e, "ws dashboard: 接收错误，关闭连接");
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
                        let text = axum::extract::ws::Utf8Bytes::from(envelope.to_string());
                        if sender.send(Message::Text(text)).await.is_err() {
                            break;
                        }
                    }
                    Ok(WsEvent::DashboardEvent { kind, payload }) => {
                        let envelope = WsEventMsg::new(kind, payload);
                        let text = serde_json::to_string(&envelope).unwrap_or_default();
                        let text = axum::extract::ws::Utf8Bytes::from(text);
                        if sender.send(Message::Text(text)).await.is_err() {
                            break;
                        }
                    }
                    Ok(WsEvent::Notification { .. }) | Ok(WsEvent::Heartbeat) | Err(_) => {
                        // dashboard 不透出 Notification / Heartbeat
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
                let frame = Message::Text(axum::extract::ws::Utf8Bytes::from(text));
                if sender.send(frame).await.is_err() {
                    break;
                }
            }
        }
    }

    info!(user_id = user_id, "ws dashboard: 连接清理完成");
}

/// 拉一次快照并组装 envelope（handler 三形态 ①：开 tx → service → commit）。
///
/// 2026-09-22 Group E 重构：从 `state.dashboard_service`（不是业务层 `AppState`
/// 直接持有）取 service 实例；service 方法签名改 `<R: DashboardRepoTrait>(&self,
/// mut repo: R, ...)` by-value，handler 借 `&mut *tx` 喂给 trait（trait 已直接
/// `impl for &mut PgConnection`，2026-09-22 同 iam 范式）。
async fn build_snapshot_msg(state: &AppState) -> Result<String, AppError> {
    let mut tx = state.pool.begin().await?;
    let snap = state
        .dashboard_service
        .build_snapshot_with_workers(&mut *tx, None)
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
        .send(Message::Text(axum::extract::ws::Utf8Bytes::from(
            text.to_string(),
        )))
        .await
}
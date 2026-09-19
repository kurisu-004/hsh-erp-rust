//! dashboard WebSocket handler（2026-09-15 takeover-fill + followup-cleanup）
//!
//! 路径：`GET /ws/dashboard?token=<JWT>`（由 `dashboard::mod::router()` 桥接，
//! 再由 `modules::ws_router()` 在 `/ws` 前缀下挂）。
//!
//! 实现要点：
//! - query token 鉴权：解 JWT + 验签 + 查 Redis session（如 `session_check_enabled=true`）
//! - 任意已登录（*）即可连接
//! - WS 升级：`axum::extract::ws::WebSocketUpgrade`
//! - 业务事件订阅：把 `state.ws_hub.broadcast` 上的 `WsEvent::DashboardEvent`
//!   透传给本连接
//! - 心跳：30s 周期发 `WsHeartbeatMsg` 作为 **text** 帧（浏览器 JS `onmessage` 可直接收到；
//!   原 `Message::Ping` 浏览器不会触发 `onmessage`，前端无法感知；2026-09-15 followup A6 改）。
//!   间隔由 `state.config.ws_heartbeat_interval_seconds` 控制，生产 30s，测试可调小。
//! - 推一次 `WsSnapshotMsg` 立即下发

use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::response::IntoResponse;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tracing::{info, warn};

use crate::auth::jwt::decode_access;
use crate::auth::rbac::{CurrentUser, parse_role_str_or_warn};
use crate::auth::session::hash_token;
use crate::infra::ws_hub::WsEvent;
use crate::modules::dashboard::dto::{WsEventMsg, WsHeartbeatMsg, WsSnapshotMsg};
use crate::modules::dashboard::service::DashboardService;
use crate::shared::error::{AppError, code};
use crate::state::AppState;

#[derive(Debug, Deserialize)]
pub struct WsQuery {
    pub token: Option<String>,
}

/// `GET /ws/dashboard?token=<JWT>`
pub async fn ws_dashboard(
    State(state): State<Arc<AppState>>,
    Query(q): Query<WsQuery>,
    ws: WebSocketUpgrade,
) -> Result<impl IntoResponse, AppError> {
    // 1. 鉴权
    let token = q
        .token
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AppError::biz(code::UNAUTHORIZED, "缺少 token 查询参数"))?;
    let claims = decode_access(token, &state.config.jwt.secret, &state.config.jwt.issuer)?;

    // 2. 服务端 session 校验（与 HTTP extractor 同语义）
    let cached_roles = if state.config.redis.session_check_enabled {
        let token_hash = hash_token(token);
        let cached = state
            .session
            .get_session(&token_hash)
            .await?
            .ok_or_else(|| AppError::biz(code::SESSION_REVOKED, "会话已被吊销，请重新登录"))?;
        if cached.user_id != claims.sub {
            return Err(AppError::biz(
                code::SESSION_REVOKED,
                "会话已被吊销，请重新登录",
            ));
        }
        // 滑动 TTL（best-effort）
        if let Err(e) = state
            .session
            .touch_session(&token_hash, state.config.redis.session_ttl_seconds)
            .await
        {
            warn!(error = %e, "ws dashboard: 刷新 session TTL 失败");
        }
        cached.cached.roles
    } else {
        // 关闭时直接用 JWT claims 的 roles
        claims
            .roles
            .iter()
            .map(|r| match r {
                crate::auth::rbac::Role::Manager => "MANAGER".to_string(),
                crate::auth::rbac::Role::Clerk => "CLERK".to_string(),
                crate::auth::rbac::Role::Inspector => "INSPECTOR".to_string(),
                crate::auth::rbac::Role::CncProgrammer => "CNC_PROGRAMMER".to_string(),
                crate::auth::rbac::Role::ShelfAccount => "SHELF_ACCOUNT".to_string(),
            })
            .collect()
    };

    // 3. 构造 CurrentUser（仅用于日志/后续权限扩展；本端点任意已登录）
    let mut roles = Vec::with_capacity(cached_roles.len());
    for r in &cached_roles {
        if let Some(role) = parse_role_str_or_warn(r) {
            roles.push(role);
        }
    }
    let _current = CurrentUser {
        id: claims.sub,
        username: claims.username.clone(),
        roles,
        shelf_ids: claims.shelf_ids.clone(),
        shelf_wildcard: claims.shelf_wildcard,
    };

    // 4. 升级 + 把 state + user_id 移交给子任务
    let state_clone = state.clone();
    let user_id = claims.sub;
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

/// 拉一次快照并组装 envelope。
async fn build_snapshot_msg(state: &AppState) -> Result<String, AppError> {
    let mut tx = state.pool.begin().await?;
    let snap = DashboardService::build_snapshot_with_workers(&mut tx, None).await?;
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

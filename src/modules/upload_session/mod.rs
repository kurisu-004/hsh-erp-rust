//! upload_session 域
//!
//! 2026-09-18 新增。
//!
//! 对应 Python myERP：
//! - 共享 STS 凭证机制（取代原 `part_file::upload-intents` 一次性签发 + 无状态 RPC）
//! - Redis 会话存储 `upload_session:{user_id}:{scope}`（24h 滑动 TTL）
//! - python 后端 STS 转发（替代 rust 直连 `cos_rust_sdk::sts`）
//!
//! 文件清单（项目标准六件套 + dto/vo）：
//! - `dto.rs`：7 端点入参 + 内部数据结构 `UploadSession` / `SessionFile` / `SessionCredentials`
//! - `vo/`：7 端点出参（Serialize-only，PR4 拆出）
//! - `repo.rs`：`UploadSessionRepo` trait + `RedisUploadSessionRepo` / `NoopUploadSessionRepo` /
//!   `InMemoryUploadSessionRepo`（测试用）
//! - `service.rs`：7 端点业务函数（get_or_create / allocate / complete / remove /
//!   renew / consume / discard），通过 `Arc<dyn UploadSessionRepo>` 操作 Redis，
//!   通过 `Arc<dyn PythonSts>` 转发 python 后端
//! - `handler.rs`：7 个 POST handler + axum 子路由
//! - `mod.rs`：re-export `handler::router`
//!
//! 不需要 model / statemachine（纯 Redis JSON，不依赖 DB）。

pub mod dto;
pub mod handler;
pub mod repo;
pub mod service;
pub mod vo;

use std::sync::Arc;

use axum::Router;

use crate::state::AppState;

pub fn router() -> Router<Arc<AppState>> {
    handler::router()
}

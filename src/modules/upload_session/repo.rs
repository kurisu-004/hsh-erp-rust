//! upload_session 域数据访问（Redis）
//!
//! 2026-09-18 新增。
//!
//! 设计动机：
//! - 浏览器直传 COS 需要 STS 凭证；原 part_file::upload-intents 每次调用都签一次 STS
//!   （无状态 RPC，前端需手动跟踪）。
//! - 新设计：服务端维护 `upload_session:{user_id}:{scope}` Redis 会话，TTL 24h 滑动；
//!   客户端 `get_or_create` 拿凭证后多次 `allocate` / `complete` / `remove` / `consume`
//!   共用同一会话；凭证 <600s 自动 renew。
//!
//! ## 键策略
//! - 单条：`upload_session:{user_id}:{scope}` （string，存 JSON `UploadSession`，TTL 24h 滑动）
//!
//! ## 竞态保证
//! 单 key 写者即单 user（用 `user_id` 隔离），同 user 多次并发写走 `GET → modify → SET EX`
//! 流程，竞态可接受（最坏情况是后续写覆盖前次结果；client 端按 tmp_key 跟踪，不依赖文件列表的强一致）。
//! 如需更强保证可后续切 WATCH/MULTI/EXEC 或 Lua，但当前 1:1 user/scope 单 key 实际无并发。
//!
//! ## 注入模式
//! 与 `auth/session.rs::SessionStore` 同形：`trait + Arc<dyn>` 注入；test 时提供
//! 内存实现，避免引入 Redis 测试容器。

use std::sync::Arc;

use async_trait::async_trait;
use deadpool_redis::{Connection, Pool};
use redis::AsyncCommands;

use super::dto::UploadSession;
use crate::shared::error::AppError;

/// Redis key 前缀。
const KEY_PREFIX: &str = "upload_session";

/// 拼 redis key：`upload_session:{user_id}:{scope}`。
pub fn key_for(user_id: i64, scope: &str) -> String {
    format!("{KEY_PREFIX}:{user_id}:{scope}")
}

/// session 存储抽象（trait + Arc<dyn> 注入模式）。
///
/// 2026-09-18 新增。
#[async_trait]
pub trait UploadSessionRepo: Send + Sync {
    /// 按 `(user_id, scope)` 读；不存在返回 `Ok(None)`。
    async fn get(&self, user_id: i64, scope: &str) -> Result<Option<UploadSession>, AppError>;

    /// 写整条 session（覆盖 + TTL 续期）。
    ///
    /// `ttl_seconds` 0 表示不显式设 TTL（Redis SET 默认永不过期，本模块不会传 0；保留以防误用）。
    async fn put(&self, session: &UploadSession, ttl_seconds: u64) -> Result<(), AppError>;

    /// 滑动 TTL：EXPIRE 续期。
    async fn touch(&self, user_id: i64, scope: &str, ttl_seconds: u64) -> Result<bool, AppError>;

    /// 删除整条 session。返回是否真的删除（false = key 不存在）。
    async fn delete(&self, user_id: i64, scope: &str) -> Result<bool, AppError>;
}

/// Redis 实现：JSON 编码 + deadpool-redis 连接池。
///
/// 2026-09-18 新增。
pub struct RedisUploadSessionRepo {
    pool: Pool,
}

impl RedisUploadSessionRepo {
    pub fn new(pool: Pool) -> Self {
        Self { pool }
    }

    async fn conn(&self) -> Result<Connection, AppError> {
        self.pool
            .get()
            .await
            .map_err(|e| AppError::internal(format!("redis pool: {e}")))
    }
}

fn map_redis(e: redis::RedisError) -> AppError {
    AppError::internal(format!("redis: {e}"))
}

#[async_trait]
impl UploadSessionRepo for RedisUploadSessionRepo {
    async fn get(&self, user_id: i64, scope: &str) -> Result<Option<UploadSession>, AppError> {
        let mut conn = self.conn().await?;
        let raw: Option<String> = conn.get(key_for(user_id, scope)).await.map_err(map_redis)?;
        match raw {
            None => Ok(None),
            Some(s) => serde_json::from_str(&s)
                .map(Some)
                .map_err(|e| AppError::internal(format!("redis: decode upload_session: {e}"))),
        }
    }

    async fn put(&self, session: &UploadSession, ttl_seconds: u64) -> Result<(), AppError> {
        let payload = serde_json::to_string(session)
            .map_err(|e| AppError::internal(format!("redis: serialize upload_session: {e}")))?;
        let mut conn = self.conn().await?;
        let key = key_for(session.user_id, &session.scope);
        // SET <key> <payload> EX <ttl>
        redis::cmd("SET")
            .arg(&key)
            .arg(&payload)
            .arg("EX")
            .arg(ttl_seconds)
            .query_async::<()>(&mut conn)
            .await
            .map_err(map_redis)?;
        Ok(())
    }

    async fn touch(&self, user_id: i64, scope: &str, ttl_seconds: u64) -> Result<bool, AppError> {
        let mut conn = self.conn().await?;
        let updated: bool = conn
            .expire(key_for(user_id, scope), ttl_seconds as i64)
            .await
            .map_err(map_redis)?;
        Ok(updated)
    }

    async fn delete(&self, user_id: i64, scope: &str) -> Result<bool, AppError> {
        let mut conn = self.conn().await?;
        let deleted: bool = conn.del(key_for(user_id, scope)).await.map_err(map_redis)?;
        Ok(deleted)
    }
}

/// No-op 实现：与 `auth::NoopSessionStore` 同模式。
///
/// 用于 `REDIS_SESSION_CHECK_ENABLED=false` 时（Rust 借 Python JWT 过渡期）；
/// 此时 `state.session` 已是 NoopSessionStore，`UploadSessionRepo` 也跟随失效。
///
/// 但 `upload_session` 域必须依赖 Redis 才能工作，所以 Noop 实现返回 `Ok(None)` /
/// `Ok(false)` 让所有写入静默成功，**配合上游禁用**（service 层应在 AppState 装配时
/// 根据 `redis.session_check_enabled` 选择 Redis 或 Noop；本类型仅供 trait 一致性）。
///
/// 2026-09-18 新增。
pub struct NoopUploadSessionRepo;

#[async_trait]
impl UploadSessionRepo for NoopUploadSessionRepo {
    async fn get(&self, _user_id: i64, _scope: &str) -> Result<Option<UploadSession>, AppError> {
        Ok(None)
    }

    async fn put(&self, _session: &UploadSession, _ttl_seconds: u64) -> Result<(), AppError> {
        tracing::warn!(
            user_id = _session.user_id,
            scope = %_session.scope,
            "NoopUploadSessionRepo::put 被调用（REDIS_SESSION_CHECK_ENABLED=false），session 未真实写入"
        );
        Ok(())
    }

    async fn touch(
        &self,
        _user_id: i64,
        _scope: &str,
        _ttl_seconds: u64,
    ) -> Result<bool, AppError> {
        Ok(false)
    }

    async fn delete(&self, _user_id: i64, _scope: &str) -> Result<bool, AppError> {
        Ok(false)
    }
}

// ============================================================
// 内存实现（单测用）
// ============================================================

/// 内存实现：用 `Arc<tokio::sync::Mutex<HashMap>>` 模拟 Redis 行为。
///
/// 仅用于 service 层单测（mock `UploadSessionRepo` trait），避免引入 Redis 测试容器。
/// 与 Redis 实现的语义差异：
/// - `put` 总是覆盖；TTL 忽略（测试场景不需要滑动窗口）
/// - `touch` 总是返回 true（不模拟过期）
/// - `delete` 返回 key 是否真实存在
///
/// 2026-09-18 新增。
pub struct InMemoryUploadSessionRepo {
    inner: Arc<tokio::sync::Mutex<std::collections::HashMap<String, UploadSession>>>,
}

impl InMemoryUploadSessionRepo {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
        }
    }
}

impl Default for InMemoryUploadSessionRepo {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl UploadSessionRepo for InMemoryUploadSessionRepo {
    async fn get(&self, user_id: i64, scope: &str) -> Result<Option<UploadSession>, AppError> {
        let g = self.inner.lock().await;
        Ok(g.get(&key_for(user_id, scope)).cloned())
    }

    async fn put(&self, session: &UploadSession, _ttl_seconds: u64) -> Result<(), AppError> {
        let mut g = self.inner.lock().await;
        g.insert(key_for(session.user_id, &session.scope), session.clone());
        Ok(())
    }

    async fn touch(&self, user_id: i64, scope: &str, _ttl_seconds: u64) -> Result<bool, AppError> {
        let g = self.inner.lock().await;
        Ok(g.contains_key(&key_for(user_id, scope)))
    }

    async fn delete(&self, user_id: i64, scope: &str) -> Result<bool, AppError> {
        let mut g = self.inner.lock().await;
        Ok(g.remove(&key_for(user_id, scope)).is_some())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modules::upload_session::dto::{SessionCredentials, SessionFile};

    fn sample_session(user_id: i64, scope: &str, sid: &str) -> UploadSession {
        UploadSession {
            session_id: sid.into(),
            user_id,
            scope: scope.into(),
            tmp_prefix: format!("tmp/sess/{sid}/"),
            bucket: "b".into(),
            region: "ap-shanghai".into(),
            credentials: SessionCredentials {
                tmp_secret_id: "i".into(),
                tmp_secret_key: "k".into(),
                session_token: "t".into(),
                start_time: 100,
                expired_time: 200,
            },
            expires_in: 100,
            files: vec![SessionFile {
                client_ref: "r1".into(),
                kind: "drawing".into(),
                original_filename: "a.pdf".into(),
                file_size: 1024,
                content_type: "application/pdf".into(),
                content_sha256: "a".repeat(64),
                tmp_key: format!("tmp/sess/{sid}/aaaa_a.pdf"),
                status: "pending".into(),
                etag: None,
                uploaded_at: None,
            }],
            created_at: 1,
            updated_at: 1,
        }
    }

    #[tokio::test]
    async fn in_memory_repo_put_get_delete() {
        let repo = InMemoryUploadSessionRepo::new();
        let s = sample_session(42, "parts_new", "sess-1");

        // get on empty
        assert!(repo.get(42, "parts_new").await.unwrap().is_none());

        // put
        repo.put(&s, 86400).await.unwrap();

        // get returns same
        let got = repo
            .get(42, "parts_new")
            .await
            .unwrap()
            .expect("must exist");
        assert_eq!(got.session_id, "sess-1");
        assert_eq!(got.user_id, 42);
        assert_eq!(got.files.len(), 1);

        // touch returns true
        assert!(repo.touch(42, "parts_new", 86400).await.unwrap());

        // scope 隔离：不同 scope 看不到
        assert!(repo.get(42, "assemblies_new").await.unwrap().is_none());

        // delete returns true once, false afterwards
        assert!(repo.delete(42, "parts_new").await.unwrap());
        assert!(!repo.delete(42, "parts_new").await.unwrap());
        assert!(repo.get(42, "parts_new").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn in_memory_repo_isolates_users() {
        let repo = InMemoryUploadSessionRepo::new();
        repo.put(&sample_session(42, "parts_new", "a"), 60)
            .await
            .unwrap();
        repo.put(&sample_session(99, "parts_new", "b"), 60)
            .await
            .unwrap();
        assert_eq!(
            repo.get(42, "parts_new").await.unwrap().unwrap().session_id,
            "a"
        );
        assert_eq!(
            repo.get(99, "parts_new").await.unwrap().unwrap().session_id,
            "b"
        );
    }

    #[test]
    fn key_for_format() {
        assert_eq!(key_for(42, "parts_new"), "upload_session:42:parts_new");
    }
}

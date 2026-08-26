//! 服务端 session 真相源（Redis）
//!
//! 对应方案 `redis-session-deadpool-redis-0-23-https-valiant-raven.md`。
//!
//! ## 设计动机
//! JWT 一旦签发，服务端无法强制吊销短期 access token。本模块在 Redis 中为每个
//! token 维护一条「session 条目」：登录/refresh 时写入；logout、改密、refresh 时删除；
//! `CurrentUser` extractor 每次都查 Redis——条目缺失即视为吊销。
//!
//! ## 键策略（双层）
//! - 每 token 一条主条目：`session:tok:<sha256_hex>`（string，存 JSON `CachedSession`，TTL 滑动）
//! - 每用户一个 Set 索引：`sessions:user:<user_id>`（每条 token 一个 sha256_hex）
//!
//! ## 兜底
//! `t_user.refresh_token_version` 的 DB 轮转保留——Redis 数据丢失或被 `FLUSHDB` 时，
//! refresh 仍会被版本校验挡住，access 则靠自然到期。

use async_trait::async_trait;
use chrono::Utc;
use deadpool_redis::redis::AsyncCommands;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::shared::error::AppError;

use deadpool_redis::{Connection, Pool};

/// 单条 token 的服务端 session 形态
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum TokenKind {
    Access,
    Refresh,
}

/// token 对应的会话缓存（含上下文一致性校验字段）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedSession {
    pub user_id: i64,
    pub token_kind: TokenKind,
    pub created_at: i64,
    pub expires_at: i64,
    pub cached: CachedCurrentUser,
}

/// 与 `CurrentUser` 同形——登录态直接从此构造，不查 DB（`/me` 仍然走 DB 取最新）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedCurrentUser {
    pub id: i64,
    pub username: String,
    /// 大写角色字符串列表（与 `auth::rbac::Role` serde rename 对齐："MANAGER"/"CLERK"/…）
    pub roles: Vec<String>,
    pub shelf_ids: Vec<i64>,
    pub shelf_wildcard: bool,
}

/// session 存储抽象（trait + Arc<dyn> 与现有 `CosClient` 同模式）
#[async_trait]
pub trait SessionStore: Send + Sync {
    /// 写入一条 session（同时建用户 Set 索引 + 双 TTL）
    async fn create_session(
        &self,
        token_hash: &str,
        user_id: i64,
        kind: TokenKind,
        ttl_seconds: u64,
        cached: &CachedCurrentUser,
    ) -> Result<(), AppError>;

    /// 读一条 session；不存在返回 `Ok(None)`，存在但解码失败走 `AppError::Internal`
    async fn get_session(&self, token_hash: &str) -> Result<Option<CachedSession>, AppError>;

    /// 删一条 session：GET user_id → DEL 主键 + SREM 用户 Set
    async fn delete_session(&self, token_hash: &str) -> Result<(), AppError>;

    /// 全清某用户的全部 session：SMEMBERS → 逐条 DEL → DEL Set
    async fn delete_all_user_sessions(&self, user_id: i64) -> Result<(), AppError>;

    /// 滑动 TTL；返回 true iff key 存在并 EXPIRE 成功
    async fn touch_session(&self, token_hash: &str, ttl_seconds: u64) -> Result<bool, AppError>;
}

/// Redis 实现的 SessionStore
pub struct RedisSessionStore {
    pool: Pool,
}

impl RedisSessionStore {
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

/// sha256(token) → hex；Redis key 的派生，避免直接用明文 token 当 key
pub fn hash_token(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(64);
    for b in digest {
        use std::fmt::Write as _;
        let _ = write!(&mut hex, "{b:02x}");
    }
    hex
}

fn key_token(token_hash: &str) -> String {
    format!("session:tok:{token_hash}")
}

fn key_user_set(user_id: i64) -> String {
    format!("sessions:user:{user_id}")
}

fn now_unix() -> i64 {
    Utc::now().timestamp()
}

fn map_redis(e: redis::RedisError) -> AppError {
    AppError::internal(format!("redis: {e}"))
}

#[async_trait]
impl SessionStore for RedisSessionStore {
    async fn create_session(
        &self,
        token_hash: &str,
        user_id: i64,
        kind: TokenKind,
        ttl_seconds: u64,
        cached: &CachedCurrentUser,
    ) -> Result<(), AppError> {
        let now = now_unix();
        let session = CachedSession {
            user_id,
            token_kind: kind,
            created_at: now,
            expires_at: now + ttl_seconds as i64,
            cached: cached.clone(),
        };
        let payload = serde_json::to_string(&session)
            .map_err(|e| AppError::internal(format!("redis: serialize session: {e}")))?;

        let mut conn = self.conn().await?;
        // pipe().atomic() 在 MULTI/EXEC 块中执行：
        //   1. SET <key> <payload> EX <ttl>
        //   2. SADD <user_set> <token_hash>
        //   3. EXPIRE <user_set> <ttl>  （与主条目 TTL 对齐，避免 Set 永久残留）
        redis::pipe()
            .atomic()
            .cmd("SET")
            .arg(key_token(token_hash))
            .arg(payload)
            .arg("EX")
            .arg(ttl_seconds)
            .ignore()
            .cmd("SADD")
            .arg(key_user_set(user_id))
            .arg(token_hash)
            .ignore()
            .cmd("EXPIRE")
            .arg(key_user_set(user_id))
            .arg(ttl_seconds)
            .ignore()
            .query_async::<()>(&mut conn)
            .await
            .map_err(map_redis)?;
        Ok(())
    }

    async fn get_session(&self, token_hash: &str) -> Result<Option<CachedSession>, AppError> {
        let mut conn = self.conn().await?;
        let raw: Option<String> = conn
            .get(key_token(token_hash))
            .await
            .map_err(map_redis)?;
        match raw {
            None => Ok(None),
            Some(s) => serde_json::from_str(&s)
                .map(Some)
                .map_err(|e| AppError::internal(format!("redis: decode session: {e}"))),
        }
    }

    async fn delete_session(&self, token_hash: &str) -> Result<(), AppError> {
        // GET → user_id（SREM 必需）；失败/不存在也允许继续 DEL（幂等）
        let mut conn = self.conn().await?;
        let raw: Option<String> = conn
            .get(key_token(token_hash))
            .await
            .map_err(map_redis)?;
        let user_id = raw
            .as_deref()
            .and_then(|s| serde_json::from_str::<CachedSession>(s).ok())
            .map(|s| s.user_id);

        redis::pipe()
            .atomic()
            .cmd("DEL")
            .arg(key_token(token_hash))
            .ignore()
            .query_async::<()>(&mut conn)
            .await
            .map_err(map_redis)?;

        if let Some(uid) = user_id {
            let _: () = conn
                .srem(key_user_set(uid), token_hash)
                .await
                .map_err(map_redis)?;
        }
        Ok(())
    }

    async fn delete_all_user_sessions(&self, user_id: i64) -> Result<(), AppError> {
        let mut conn = self.conn().await?;
        let set_key = key_user_set(user_id);
        // SMEMBERS 当前用户 Set 的全部 token_hash
        let hashes: Vec<String> = conn.smembers(&set_key).await.map_err(map_redis)?;
        if !hashes.is_empty() {
            // 用 DEL 批量删除所有 token 主条目（key 不存在会被 Redis 忽略，幂等）
            let mut pipe = redis::pipe();
            pipe.atomic();
            for h in &hashes {
                pipe.cmd("DEL").arg(key_token(h)).ignore();
            }
            pipe.query_async::<()>(&mut conn).await.map_err(map_redis)?;
            // SREM 把这些 hash 从 Set 里摘掉（最后一次 DEL 后 Set 也会被下面清空）
            let _: () = conn.srem(&set_key, &hashes).await.map_err(map_redis)?;
        }
        // DEL 用户 Set 本体
        let _: () = conn.del(&set_key).await.map_err(map_redis)?;
        Ok(())
    }

    async fn touch_session(&self, token_hash: &str, ttl_seconds: u64) -> Result<bool, AppError> {
        let mut conn = self.conn().await?;
        let updated: bool = conn
            .expire(key_token(token_hash), ttl_seconds as i64)
            .await
            .map_err(map_redis)?;
        Ok(updated)
    }
}

/// No-op 实现：用 Rust 自签 JWT 但借用 Python myERP 用户库的过渡期可关
/// 闭服务端的 session 真相源（迁移早期，access token 的吊销完全依赖 JWT
/// 短 TTL + refresh token）。所有写入和读取都是 no-op；extractor 中通过
/// `state.config.redis.session_check_enabled` gate，**完全不会调到**这些
/// 实现，但 trait 仍要求实现以保持 `Arc<dyn SessionStore>` 类型一致。
pub struct NoopSessionStore;

impl NoopSessionStore {
    /// 单元构造：保持与 `RedisSessionStore::new(pool)` 同形调用风格。
    pub fn new() -> Self {
        Self
    }
}

impl Default for NoopSessionStore {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl SessionStore for NoopSessionStore {
    async fn create_session(
        &self,
        _token_hash: &str,
        _user_id: i64,
        _kind: TokenKind,
        _ttl_seconds: u64,
        _cached: &CachedCurrentUser,
    ) -> Result<(), AppError> {
        // 借用 Python JWT 时不该有写入；打 warn 以便误用时可见
        tracing::warn!("NoopSessionStore::create_session 被调用（REDIS_SESSION_CHECK_ENABLED=false）");
        Ok(())
    }

    async fn get_session(
        &self,
        _token_hash: &str,
    ) -> Result<Option<CachedSession>, AppError> {
        Ok(None)
    }

    async fn delete_session(&self, _token_hash: &str) -> Result<(), AppError> {
        Ok(())
    }

    async fn delete_all_user_sessions(&self, _user_id: i64) -> Result<(), AppError> {
        Ok(())
    }

    async fn touch_session(
        &self,
        _token_hash: &str,
        _ttl_seconds: u64,
    ) -> Result<bool, AppError> {
        Ok(false)
    }
}

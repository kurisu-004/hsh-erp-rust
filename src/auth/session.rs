//! 服务端 session 真相源（Redis）
//!
//! ## 设计动机
//! JWT 一旦签发，服务端无法强制吊销短期 access token。本模块在 Redis 中为每个
//! token 维护一条「session 条目」：登录/refresh 时写入；logout、改密、refresh 时删除；
//! `CurrentUser` extractor 每次都查 Redis——条目缺失即视为吊销。
//!
//! ## 键策略（双层）
//! - 每 token 一条主条目：`session:tok:<jti UUID v4>`（string，存 JSON `CachedSession`，TTL 滑动）
//! - 每用户一个 Set 索引：`sessions:user:<user_id>`（每条 token 一个 jti (UUID v4)）
//!
//! ## 兜底
//! `t_user.refresh_token_version` 的 DB 轮转保留——Redis 数据丢失或被 `FLUSHDB` 时，
//! refresh 仍会被版本校验挡住，access 则靠自然到期。
//!
//! ## 2026-09-22 重构
//! - `CachedCurrentUser` → `CachedUserProfile`（删除 `id` 字段；user_id 由外层 `CachedSession`
//!   字段权威锚定，避免内外两个 id 漂移）。
//! - `CachedSession.cached` → `CachedSession.profile`，与字段语义对齐。
//!
//! ## 2026-09-23 重构
//! - Redis 主条目 key 从 `session:tok:<sha256(token)>` 改为 `session:tok:<jti>`，
//!   jti 直接复用 JWT 自带的 `claims.jwt_id`（UUID v4），不再调用 `hash_token`。
//! - 删除 `hash_token` 函数（`sha2` crate 因 part_file 仍保留），`SessionStore` trait
//!   入参从 `token_hash: &str` 改为 `jti: &str`；语义改名 `AuthenticatedTokenHash` → `SessionJti`。
//!
//! ## 2026-09-23 重构：refresh token rotation + reuse detection 黑名单
//! - `SessionStore` trait 新增两个方法：
//!   - `revoke_jti(jti, ttl_seconds)`：把 jti 写入 Redis 黑名单 `revoked:<jti>`（空值 + EX TTL），
//!     `SET ... NX` 语义避免覆盖已有条目；返回 `true` 表示本次写入、`false` 表示已存在跳过。
//!   - `is_jti_revoked(jti)`：`EXISTS revoked:<jti>` 检查 jti 是否被吊销。
//! - TTL 由调用方计算（业务层：`refresh_exp - now`，saturating 0；最多 7d，因 refresh TTL 默认 7d）。
//!   黑名单存活时间恰好覆盖原 refresh token 的剩余有效期，TTL 到期后 entry 被 Redis 自动回收，
//!   与 refresh 自身过期保持语义一致——refresh 失效了，黑名单也无需再保留。
//! - 复用检测触发逻辑（`auth/middleware.rs::verify_session_token` 与
//!   `modules/iam/service/session.rs::refresh` phase 1）：任一处看到 `is_jti_revoked=true`
//!   即视为会话/refresh 已失效，返回 40105 SESSION_REVOKED；refresh 路径额外触发
//!   `delete_all_user_sessions` 全清该用户的所有 session（强制下线）。

use async_trait::async_trait;
use chrono::Utc;
use deadpool_redis::redis::AsyncCommands;
use serde::{Deserialize, Serialize};

use crate::shared::error::AppError;

use deadpool_redis::{Connection, Pool};

/// 单条 token 的服务端 session 形态
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum TokenKind {
    Access,
    Refresh,
}

/// token 对应的会话缓存（含上下文一致性校验字段）
///
/// ⚠️ 2026-09-22 部署注意：`profile` 是从旧字段名 `cached` 重命名而来，
/// 线上已存在的 Redis session entry（JSON 含 `cached` 字段）反序列化会失败，
/// 上线前需清空 Redis session DB（`FLUSHDB` 或选择性删除 `session:tok:*`），
/// 否则已登录用户在 session TTL（默认 15min / 900s）内持续 5xx（50000 INTERNAL）。
/// 详见 `docs/api/index.md`「部署顺序」段。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedSession {
    pub user_id: i64,
    pub token_kind: TokenKind,
    pub created_at: i64,
    pub expires_at: i64,
    /// 2026-09-22 改名：原 `cached: CachedCurrentUser` → `profile: CachedUserProfile`。
    /// 字段语义与新类型名对齐（profile = 用户业务画像）。
    pub profile: CachedUserProfile,
}

/// 2026-09-22 重命名 + 删字段：原 `CachedCurrentUser` → `CachedUserProfile`。
///
/// 删除 `id` 字段：`user_id` 已在 `CachedSession` 外层作为权威锚点；profile
/// 不再冗余携带，避免内外两个 id 漂移。
///
/// 与 `CurrentUser` 同形（除 `id`）——登录态直接从此构造，不查 DB（`/me` 仍然走 DB 取最新）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedUserProfile {
    pub username: String,
    /// 大写角色字符串列表（与 `auth::rbac::Role` serde rename 对齐："MANAGER"/"CLERK"/…）
    pub roles: Vec<String>,
    pub shelf_ids: Vec<i64>,
    pub shelf_wildcard: bool,
}

/// 从 JWT claims.jwt_id 提取的 UUID v4，即 Redis session key `session:tok:<jti>`
/// 的后缀。
///
/// 2026-09-23 重构：原 `AuthenticatedTokenHash(String)` 改名而来；值类型不变
/// （仍是 String），但语义从 sha256 hex 改为 jti UUID v4 字符串。
#[derive(Debug, Clone)]
pub struct SessionJti(pub String);

/// session 存储抽象（trait + Arc<dyn> 与现有 `CosClient` 同模式）
///
/// 2026-09-23 重构：所有形参从 `token_hash: &str` 改为 `jti: &str`，
/// 业务层直接传 JWT 的 `claims.jwt_id`（UUID v4）。
#[cfg_attr(test, mockall::automock)]
#[async_trait]
pub trait SessionStore: Send + Sync {
    /// 写入一条 session（同时建用户 Set 索引 + 双 TTL）
    async fn create_session(
        &self,
        jti: &str,
        user_id: i64,
        kind: TokenKind,
        ttl_seconds: u64,
        profile: &CachedUserProfile,
    ) -> Result<(), AppError>;

    /// 读一条 session；不存在返回 `Ok(None)`，存在但解码失败走 `AppError::Internal`
    async fn get_session(&self, jti: &str) -> Result<Option<CachedSession>, AppError>;

    /// 删一条 session：GET user_id → DEL 主键 + SREM 用户 Set
    async fn delete_session(&self, jti: &str) -> Result<(), AppError>;

    /// 全清某用户的全部 session：SMEMBERS → 逐条 DEL → DEL Set
    async fn delete_all_user_sessions(&self, user_id: i64) -> Result<(), AppError>;

    /// 滑动 TTL；返回 true iff key 存在并 EXPIRE 成功
    async fn touch_session(&self, jti: &str, ttl_seconds: u64) -> Result<bool, AppError>;

    /// 2026-09-23 重构：把 jti 写入 refresh reuse detection 黑名单。
    ///
    /// Redis 实现：`SET revoked:<jti> "" EX <ttl_seconds>`（带 NX 语义避免覆盖）。
    /// 返回 `true` 表示本次新写入；`false` 表示黑名单已存在，跳过本次写入。
    ///
    /// 调用方负责计算 TTL（业务语义：refresh 剩余有效期）。
    async fn revoke_jti(&self, jti: &str, ttl_seconds: u64) -> Result<bool, AppError>;

    /// 2026-09-23 重构：检查 jti 是否在 reuse detection 黑名单中。
    ///
    /// Redis 实现：`EXISTS revoked:<jti>` 转 bool。
    /// 被 `auth::middleware::verify_session_token`（access jti 闸）和
    /// `iam::service::session::refresh`（refresh reuse 检测）调用。
    async fn is_jti_revoked(&self, jti: &str) -> Result<bool, AppError>;
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

fn key_session(jti: &str) -> String {
    format!("session:tok:{jti}")
}

fn key_user_set(user_id: i64) -> String {
    format!("sessions:user:{user_id}")
}

/// 2026-09-23 新增：reuse detection 黑名单 key。
///
/// 空值写入（payload 仅占位），TTL 由业务层根据 refresh 剩余有效期计算。
/// TTL 到期即由 Redis 自动回收——与 refresh token 自身的过期保持语义一致。
fn key_revoked(jti: &str) -> String {
    format!("revoked:{jti}")
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
        jti: &str,
        user_id: i64,
        kind: TokenKind,
        ttl_seconds: u64,
        profile: &CachedUserProfile,
    ) -> Result<(), AppError> {
        let now = now_unix();
        let session = CachedSession {
            user_id,
            token_kind: kind,
            created_at: now,
            expires_at: now + ttl_seconds as i64,
            profile: profile.clone(),
        };
        let payload = serde_json::to_string(&session)
            .map_err(|e| AppError::internal(format!("redis: serialize session: {e}")))?;

        let mut conn = self.conn().await?;
        // pipe().atomic() 在 MULTI/EXEC 块中执行：
        //   1. SET <key> <payload> EX <ttl>
        //   2. SADD <user_set> <jti>
        //   3. EXPIRE <user_set> <ttl>  （与主条目 TTL 对齐，避免 Set 永久残留）
        redis::pipe()
            .atomic()
            .cmd("SET")
            .arg(key_session(jti))
            .arg(payload)
            .arg("EX")
            .arg(ttl_seconds)
            .ignore()
            .cmd("SADD")
            .arg(key_user_set(user_id))
            .arg(jti)
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

    async fn get_session(&self, jti: &str) -> Result<Option<CachedSession>, AppError> {
        let mut conn = self.conn().await?;
        let raw: Option<String> = conn.get(key_session(jti)).await.map_err(map_redis)?;
        match raw {
            None => Ok(None),
            Some(s) => serde_json::from_str(&s)
                .map(Some)
                .map_err(|e| AppError::internal(format!("redis: decode session: {e}"))),
        }
    }

    async fn delete_session(&self, jti: &str) -> Result<(), AppError> {
        // GET → user_id（SREM 必需）；失败/不存在也允许继续 DEL（幂等）
        let mut conn = self.conn().await?;
        let raw: Option<String> = conn.get(key_session(jti)).await.map_err(map_redis)?;
        let user_id = raw
            .as_deref()
            .and_then(|s| serde_json::from_str::<CachedSession>(s).ok())
            .map(|s| s.user_id);

        redis::pipe()
            .atomic()
            .cmd("DEL")
            .arg(key_session(jti))
            .ignore()
            .query_async::<()>(&mut conn)
            .await
            .map_err(map_redis)?;

        if let Some(uid) = user_id {
            let _: () = conn
                .srem(key_user_set(uid), jti)
                .await
                .map_err(map_redis)?;
        }
        Ok(())
    }

    async fn delete_all_user_sessions(&self, user_id: i64) -> Result<(), AppError> {
        let mut conn = self.conn().await?;
        let set_key = key_user_set(user_id);
        // SMEMBERS 当前用户 Set 的全部 jti
        let jtis: Vec<String> = conn.smembers(&set_key).await.map_err(map_redis)?;
        if !jtis.is_empty() {
            // 用 DEL 批量删除所有 token 主条目（key 不存在会被 Redis 忽略，幂等）
            let mut pipe = redis::pipe();
            pipe.atomic();
            for jti in &jtis {
                pipe.cmd("DEL").arg(key_session(jti)).ignore();
            }
            pipe.query_async::<()>(&mut conn).await.map_err(map_redis)?;
            // SREM 把这些 jti 从 Set 里摘掉（最后一次 DEL 后 Set 也会被下面清空）
            let _: () = conn.srem(&set_key, &jtis).await.map_err(map_redis)?;
        }
        // DEL 用户 Set 本体
        let _: () = conn.del(&set_key).await.map_err(map_redis)?;
        Ok(())
    }

    async fn touch_session(&self, jti: &str, ttl_seconds: u64) -> Result<bool, AppError> {
        let mut conn = self.conn().await?;
        let updated: bool = conn
            .expire(key_session(jti), ttl_seconds as i64)
            .await
            .map_err(map_redis)?;
        Ok(updated)
    }

    async fn revoke_jti(&self, jti: &str, ttl_seconds: u64) -> Result<bool, AppError> {
        // 2026-09-23 重构：reuse detection 黑名单。
        // SET revoked:<jti> "" EX <ttl> NX —— NX 标志确保不会覆盖已有条目（race condition
        // 兜底：两条 refresh 几乎同时写同一个 jti，Redis 只接第一条）。
        // Redis 返回 nil 时（key 已存在）转 false；返回 "OK" 时转 true。
        let mut conn = self.conn().await?;
        let reply: Option<String> = redis::cmd("SET")
            .arg(key_revoked(jti))
            .arg("")
            .arg("EX")
            .arg(ttl_seconds)
            .arg("NX")
            .query_async(&mut conn)
            .await
            .map_err(map_redis)?;
        Ok(reply.is_some())
    }

    async fn is_jti_revoked(&self, jti: &str) -> Result<bool, AppError> {
        let mut conn = self.conn().await?;
        let exists: bool = conn.exists(key_revoked(jti)).await.map_err(map_redis)?;
        Ok(exists)
    }
}

/// No-op 实现：所有写入和读取都是 no-op。服务路径下 extractor 不会调到本实现
/// （生产已统一走 `RedisSessionStore`）；仅供 service 单元测试 fixture 用，
/// 但 trait 仍要求实现以保持 `Arc<dyn SessionStore>` 类型一致。
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
        _jti: &str,
        _user_id: i64,
        _kind: TokenKind,
        _ttl_seconds: u64,
        _profile: &CachedUserProfile,
    ) -> Result<(), AppError> {
        // 借用 JWT 时不该有写入；打 warn 以便误用时可见
        tracing::warn!(
            "NoopSessionStore::create_session 被调用（仅测试 fixture，生产不应到达）"
        );
        Ok(())
    }

    async fn get_session(&self, _jti: &str) -> Result<Option<CachedSession>, AppError> {
        Ok(None)
    }

    async fn delete_session(&self, _jti: &str) -> Result<(), AppError> {
        Ok(())
    }

    async fn delete_all_user_sessions(&self, _user_id: i64) -> Result<(), AppError> {
        Ok(())
    }

    async fn touch_session(&self, _jti: &str, _ttl_seconds: u64) -> Result<bool, AppError> {
        Ok(false)
    }

    async fn revoke_jti(&self, _jti: &str, _ttl_seconds: u64) -> Result<bool, AppError> {
        // Noop 实现：生产不该走到这里（已统一 RedisSessionStore）。打 warn 以便误用时可见。
        tracing::warn!(
            "NoopSessionStore::revoke_jti 被调用（仅测试 fixture，生产不应到达）"
        );
        Ok(false)
    }

    async fn is_jti_revoked(&self, _jti: &str) -> Result<bool, AppError> {
        // Noop 路径下永远视为未吊销 —— 测试 fixture 中复用检测分支将永远走 false
        // （即不会被黑名单拦截）；若测试需要走黑名单分支，必须用真实 RedisSessionStore。
        Ok(false)
    }
}

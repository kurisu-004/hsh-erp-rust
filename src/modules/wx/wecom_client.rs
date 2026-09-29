//! 企业微信小程序登录客户端（2026-09-29 新增）
//!
//! 身份源是**企业微信 userid**（不是微信 openid）。走企业微信的
//! `GET /cgi-bin/miniprogram/jscode2session`，**不是**微信的
//! `api.weixin.qq.com/sns/jscode2session`。因为小程序只在企业微信客户端内打开，
//! 不存在 openid / 微信端分支。
//!
//! ## 外部契约（官方文档）
//! ```text
//! GET /cgi-bin/gettoken?corpid={CORPID}&corpsecret={CORPSECRET}
//!   → { "errcode":0, "errmsg":"", "access_token":"...", "expires_in":7200 }
//!   → 失败 { "errcode":40091, "errmsg":"secret is invalid" }
//!
//! GET /cgi-bin/miniprogram/jscode2session
//!        ?access_token={AT}&js_code={CODE}&grant_type=authorization_code
//!   → { "corpid":"...", "userid":"...", "session_key":"...", "errcode":0, "errmsg":"ok" }
//!   → 失败 { "errcode":40029, "errmsg":"invalid code" }
//! ```
//!
//! ## access_token 必须缓存
//! `gettoken` 有严格频控（每企业每分钟上限），每次登录都换 token 会打爆配额。
//! 本实现把 token 写 Redis `wecom:access_token:<corpid>`，TTL = `expires_in - 300`
//! （留 5min 余量，避免边界取到已失效 token）。
//!
//! ## 安全硬约束
//! `corpsecret` / `session_key` / `access_token` **绝不可**出现在：
//! - 任何 `tracing` 日志（含 `Debug` / `Display` 格式化）
//! - `AppError` 的 message 字段（会原样回给 HTTP 客户端）
//! - `WeComSession` / 任何返回给调用方的结构体
//!
//! `session_key` 是小程序会话密钥，本方案只用 userid 做身份映射、不解密任何
//! 业务数据，因此**拿到即丢**（连内存里的中间字符串都不往上传），更不落库。
//!
//! ## 测试替身
//! - `NoopWeComClient`：`config.wecom.enabled == false`（未配置 corpid / corpsecret）
//!   时由 `main.rs` 装线的占位实现，直接返 `40109 BIZ_WX_NOT_CONFIGURED`。
//!   本地 `cargo run` / 单测 / 集成测试未配置企微凭据时均走这条路径。
//! - `MockWeComApiClient`（`#[automock]`）：集成测试注入 `AppState` 用。

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
// 2026-09-29 注：mockall `#[automock]` 默认无 cfg 门控（生成 `MockWeComApiClient`
// 始终可用）。与 `infra::py_backend::PyBackendClient` 同理——mock 在
// `tests/wecom_login.rs`（integration test，编译时 `cfg(test)` 不传给主 lib）使用，
// 故必须 always-on。
use mockall::automock;
use serde::Deserialize;

use crate::infra::config::WeComConfig;
use crate::shared::error::{AppError, code};

/// 企微 `gettoken` / `jscode2session` 错误码常量（官方文档取值）
mod errcode {
    /// invalid code —— js_code 非法 / 已使用 / 已过期（code 一次性，5 分钟过期）
    pub const INVALID_CODE: i32 = 40029;
    /// invalid credential —— corpsecret 错
    pub const INVALID_CREDENTIAL: i32 = 40001;
    /// invalid access_token —— token 非法
    pub const INVALID_ACCESS_TOKEN: i32 = 40014;
    /// access_token expired —— token 过期
    pub const ACCESS_TOKEN_EXPIRED: i32 = 42001;
}

// =============================================================================
// 对外数据契约
// =============================================================================

/// `jscode2session` 成功后的身份信息（**刻意不含 session_key**）。
///
/// 2026-09-29 变更：早期微信 openid 占位方案会把 `session_key` 一起返回给
/// 调用方。本方案只用 `userid` 做 `t_wx_identity` 预绑定表的反查键，
/// `session_key` 拿到即丢（见模块 doc 的安全硬约束段）——把它放进公开结构体
/// 等于把它扩散到 handler / service / 可能的日志，属于不必要的风险面。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WeComSession {
    /// 企业 ID（企微返回，用于与本地配置比对防跨企业串号）
    pub corp_id: String,
    /// 企业微信 userid（自建应用返回**明文**；第三方应用返回加密串，本方案不适用）
    pub user_id: String,
}

/// 企业微信小程序登录客户端抽象（trait + HttpWeComClient + NoopWeComClient）。
///
/// 与 `auth::session::SessionStore` / `infra::cos::CosClient` /
/// `infra::py_backend::PyBackendClient` 三段式同形：`#[automock]` + `async_trait`
/// + `Send + Sync` + `Result<T, AppError>` + Noop 占位。
#[automock]
#[async_trait]
pub trait WeComApiClient: Send + Sync {
    /// 用小程序 `wx.login()` 的 code 换企业微信身份。
    ///
    /// - `code`：一次性、5 分钟过期，**用后即废**。
    ///
    /// 失败路径见 [`map_wecom_errcode`]（errcode → 业务码映射的单一真相源）。
    async fn code_to_session(&self, code: &str) -> Result<WeComSession, AppError>;
}

// =============================================================================
// errcode 映射（抽成独立纯函数，便于单测）
// =============================================================================

/// 该 errcode 是否属于「access_token 失效」类（可删缓存重取后重试一次）。
///
/// 2026-09-29：`40001` 同时覆盖「corpsecret 配错」与「access_token 非法」两种
/// 情况（企微未细分）。这里按「可重试」处理：删缓存 → 重取一次；重取后仍失败
/// 才返 `40106`。若真实原因是 secret 配错，重取也必然失败，多一次无用调用，
/// 但保证「token 恰好在企微侧被提前失效」的场景能自愈。
fn is_access_token_class(errcode: i32) -> bool {
    errcode == errcode::INVALID_CREDENTIAL
        || errcode == errcode::INVALID_ACCESS_TOKEN
        || errcode == errcode::ACCESS_TOKEN_EXPIRED
}

/// 企微接口错误分类（`call_code2session` 的内部返回类型）。
///
/// ## 为什么不直接返回 `AppError`
/// `code_to_session` 需要区分「这类错误可以删 token 缓存重取后重试 1 次」与
/// 「重试无意义」。若从 `AppError` 反推（例如靠 message 字符串匹配）非常脆弱——
/// message 一改就静默失效。改为在 IO 层返回**类型化分类**，分类在产生错误的
/// 那一刻就确定，映射到 `AppError` 是最后一步。
#[derive(Debug)]
pub enum Code2SessionError {
    /// access_token 失效类（40001 / 40014 / 42001）——调用方可删缓存重取 1 次
    TokenInvalid,
    /// 终态错误：code 失效（40029）/ 未识别 errcode / 网络层 / 上游 5xx
    Fatal(AppError),
}

/// 企微 `errcode` → `AppError` 的**单一映射点**（纯函数，无 IO，可直接单测）。
///
/// 映射表：
/// | errcode | 行为 |
/// |---|---|
/// | `40029`（invalid code） | `BIZ_WX_LOGIN_FAILED`，**不重试**（code 一次性） |
/// | `40001` / `40014` / `42001` | `TokenInvalid` → 删缓存 → 重取 1 次；仍失败则 `BIZ_WX_LOGIN_FAILED` |
/// | 其他 `errcode != 0` | `INTERNAL` + `tracing::error!` |
///
/// ⚠️ 返回的 message **绝不包含** corpsecret / access_token / session_key；
/// 只带 errcode + 企微原 errmsg（`errmsg` 是企微返回的固定文案，不含凭据）。
pub fn classify_wecom_errcode(errcode: i32, errmsg: &str) -> Code2SessionError {
    debug_assert_ne!(errcode, 0, "errcode=0 不应进入错误分类");
    if errcode == errcode::INVALID_CODE {
        // code 一次性：重试无意义（企微侧已消费），且重试会打爆频控
        return Code2SessionError::Fatal(AppError::biz(
            code::BIZ_WX_LOGIN_FAILED,
            "企业微信登录失败：code 无效或已过期，请重新进入小程序",
        ));
    }
    if is_access_token_class(errcode) {
        return Code2SessionError::TokenInvalid;
    }
    tracing::error!(errcode, errmsg, "企业微信接口返回未识别错误");
    Code2SessionError::Fatal(AppError::internal(format!(
        "企业微信接口错误 errcode={errcode} errmsg={errmsg}"
    )))
}

// =============================================================================
// HTTP 实现
// =============================================================================

/// `gettoken` 响应体（字段名与企微 JSON 一致）
#[derive(Debug, Deserialize)]
struct GetTokenResp {
    #[serde(default)]
    errcode: i32,
    #[serde(default)]
    errmsg: String,
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    expires_in: Option<i64>,
}

/// `jscode2session` 响应体（字段名与企微 JSON 一致）
#[derive(Debug, Deserialize)]
struct Code2SessionResp {
    #[serde(default)]
    errcode: i32,
    #[serde(default)]
    errmsg: String,
    #[serde(default)]
    corpid: Option<String>,
    #[serde(default)]
    userid: Option<String>,
    // 2026-09-29：`session_key` 字段**刻意不反序列化**到任何结构体
    // （`#[allow(dead_code)]` 也不加）。serde 默认忽略未声明字段，因此企微
    // 返回的 session_key 在反序列化瞬间即被丢弃，不进入 Rust 堆上的任何
    // 长期存活对象——从类型层面杜绝「不小心 log 出去」的可能。
}

/// 真实 HTTP 客户端：调企业微信 `gettoken` + `jscode2session`。
pub struct HttpWeComClient {
    http: reqwest::Client,
    /// 与 session / idempotency 共用同一 Redis 池（`main.rs` 装线时传入）
    redis: deadpool_redis::Pool,
    config: Arc<WeComConfig>,
}

/// access_token 的 Redis 缓存 TTL 安全余量（秒）：企微给 `expires_in = 7200`，
/// 减去 300s 避免在边界取到刚好失效的 token（网络往返 + 换 token 都有延迟）。
const TOKEN_TTL_SAFETY_MARGIN: i64 = 300;

impl HttpWeComClient {
    /// 构造。`http` 由调用方（`main.rs` / 单测）按 `http_timeout_ms` 配好。
    pub fn new(
        http: reqwest::Client,
        redis: deadpool_redis::Pool,
        config: Arc<WeComConfig>,
    ) -> Self {
        Self { http, redis, config }
    }

    /// 构造一个按 `config.http_timeout_ms` 配好超时的 `reqwest::Client`。
    ///
    /// 抽成 helper 便于 `main.rs` 与单测共用同一条 timeout 语义
    /// （避免两处各写一个 `.timeout(...)` 后漂移）。
    pub fn build_http_client(config: &WeComConfig) -> anyhow::Result<reqwest::Client> {
        Ok(reqwest::Client::builder()
            .timeout(Duration::from_millis(config.http_timeout_ms))
            .build()?)
    }

    /// access_token 缓存 key（含 corpid，避免多企业部署共用一套 Redis 时串号）。
    fn token_cache_key(corpid: &str) -> String {
        format!("wecom:access_token:{corpid}")
    }

    /// 取 access_token：先读 Redis 缓存，未命中才调 `gettoken` 并 `SETEX`。
    ///
    /// `force_refresh = true` 时跳过缓存直连企微（`42001` 之类失效后的重取路径）。
    async fn get_token(&self, force_refresh: bool) -> Result<String, AppError> {
        if !force_refresh
            && let Some(tok) = self.read_token_cache().await?
        {
            return Ok(tok);
        }

        let url = format!("{}/cgi-bin/gettoken", self.config.api_base);
        // ⚠️ 日志里 **不打** url —— query string 含 corpsecret。改为只打 host 部分。
        tracing::info!(corpid = %self.config.corpid, "企业微信 gettoken 缓存未命中，实时换取 access_token");
        let resp = self
            .http
            .get(&url)
            .query(&[
                ("corpid", self.config.corpid.as_str()),
                ("corpsecret", self.config.corpsecret.as_str()),
            ])
            .send()
            .await
            .map_err(|e| {
                // reqwest::Error 的 Display 不含 query string（只在 `url` 字段里），
                // 因此这里只打 `%e` 不打 `e.url()`，避免 secret 进日志。
                tracing::error!(error = %e, corpid = %self.config.corpid, "企业微信 gettoken 请求失败（网络层）");
                AppError::internal(format!("企业微信 gettoken 网络错误: {e}"))
            })?;

        let status = resp.status();
        let body: GetTokenResp = resp
            .json()
            .await
            .map_err(|e| {
                tracing::error!(error = %e, %status, "企业微信 gettoken 响应解析失败");
                AppError::internal(format!("企业微信 gettoken 响应解析失败: {e}"))
            })?;

        if body.errcode != 0 {
            return Err(match classify_wecom_errcode(body.errcode, &body.errmsg) {
                Code2SessionError::TokenInvalid => AppError::biz(
                    code::BIZ_WX_LOGIN_FAILED,
                    "企业微信登录失败：access_token 失效，重取后仍失败",
                ),
                Code2SessionError::Fatal(e) => e,
            });
        }
        let token = body.access_token.ok_or_else(|| {
            tracing::error!(corpid = %self.config.corpid, "企业微信 gettoken 返回 errcode=0 但无 access_token");
            AppError::internal("企业微信 gettoken 响应缺 access_token")
        })?;
        // 幂余量：expires_in 缺省时退回 config.token_ttl_seconds，再减安全余量
        let ttl = match body.expires_in {
            Some(e) if e - TOKEN_TTL_SAFETY_MARGIN > 0 => (e - TOKEN_TTL_SAFETY_MARGIN) as u64,
            _ => self.config.token_ttl_seconds,
        };
        self.write_token_cache(&token, ttl).await?;
        Ok(token)
    }

    /// 读 access_token 缓存；读失败（Redis 抖动）仅 warn 并按「未命中」处理。
    async fn read_token_cache(&self) -> Result<Option<String>, AppError> {
        let mut conn = match self.redis.get().await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, "企业微信 access_token 缓存：取连接失败，降级为实时换取");
                return Ok(None);
            }
        };
        use redis::AsyncCommands;
        let res: redis::RedisResult<Option<String>> = conn
            .get(Self::token_cache_key(&self.config.corpid))
            .await;
        match res {
            Ok(v) => Ok(v),
            Err(e) => {
                tracing::warn!(error = %e, "企业微信 access_token 缓存：GET 失败，降级为实时换取");
                Ok(None)
            }
        }
    }

    /// 写 access_token 缓存（`SET key val EX ttl`）。失败仅 warn——缓存写不进去
    /// 只是退化为「每次实时换取」，不应因此让登录失败。
    async fn write_token_cache(&self, token: &str, ttl: u64) -> Result<(), AppError> {
        let mut conn = match self.redis.get().await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, "企业微信 access_token 缓存：取连接失败，跳过写入");
                return Ok(());
            }
        };
        use redis::AsyncCommands;
        let res: redis::RedisResult<()> = conn
            .set_ex(Self::token_cache_key(&self.config.corpid), token, ttl)
            .await;
        if let Err(e) = res {
            tracing::warn!(error = %e, ttl, "企业微信 access_token 缓存：SETEX 失败（登录不受影响）");
        }
        Ok(())
    }

    /// 删 access_token 缓存（`40001` / `40014` / `42001` 后重取前调用）。失败仅 warn。
    async fn invalidate_token_cache(&self) {
        let mut conn = match self.redis.get().await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, "企业微信 access_token 缓存：取连接失败，无法失效缓存");
                return;
            }
        };
        use redis::AsyncCommands;
        let res: redis::RedisResult<()> = conn
            .del(Self::token_cache_key(&self.config.corpid))
            .await;
        if let Err(e) = res {
            tracing::warn!(error = %e, "企业微信 access_token 缓存：DEL 失败");
        }
    }

    /// 调 `jscode2session`。`token` 已在调用方取好并（重试时）保证新鲜。
    ///
    /// 失败返回 [`Code2SessionError`]（`TokenInvalid` = 可重试；`Fatal` = 终态）。
    async fn call_code2session(
        &self,
        token: &str,
        code: &str,
    ) -> Result<WeComSession, Code2SessionError> {
        let url = format!(
            "{}/cgi-bin/miniprogram/jscode2session",
            self.config.api_base
        );
        // ⚠️ url 的 query 含 access_token，日志里只打路径不含 query。
        let resp = self
            .http
            .get(&url)
            .query(&[
                ("access_token", token),
                ("js_code", code),
                ("grant_type", "authorization_code"),
            ])
            .send()
            .await
            .map_err(|e| {
                tracing::error!(error = %e, "企业微信 jscode2session 请求失败（网络层）");
                Code2SessionError::Fatal(AppError::internal(format!(
                    "企业微信 jscode2session 网络错误: {e}"
                )))
            })?;

        let status = resp.status();
        // HTTP 5xx 视为系统错误（企微网关抖动），不映射成业务码
        if status.is_server_error() {
            tracing::error!(%status, "企业微信 jscode2session 返回 5xx");
            return Err(Code2SessionError::Fatal(AppError::internal(format!(
                "企业微信 jscode2session 上游 {} 错误",
                status.as_u16()
            ))));
        }
        let body: Code2SessionResp = resp.json().await.map_err(|e| {
            tracing::error!(error = %e, %status, "企业微信 jscode2session 响应解析失败");
            Code2SessionError::Fatal(AppError::internal(format!(
                "企业微信 jscode2session 响应解析失败: {e}"
            )))
        })?;

        if body.errcode != 0 {
            return Err(classify_wecom_errcode(body.errcode, &body.errmsg));
        }

        let corpid = body.corpid.unwrap_or_default();
        let userid = body.userid.unwrap_or_default();
        if corpid.is_empty() || userid.is_empty() {
            // 2026-09-29：errcode=0 但字段缺失属于上游契约异常（不应发生），
            // 归 internal 而非 40106——40106 语义是「登录失败」，这里会误导排查。
            tracing::error!(
                has_corpid = !corpid.is_empty(),
                has_userid = !userid.is_empty(),
                "企业微信 jscode2session 返回 errcode=0 但 corpid/userid 缺失"
            );
            return Err(Code2SessionError::Fatal(AppError::internal(
                "企业微信 jscode2session 响应缺少 corpid/userid",
            )));
        }
        Ok(WeComSession {
            corp_id: corpid,
            user_id: userid,
        })
    }
}

#[async_trait]
impl WeComApiClient for HttpWeComClient {
    /// 换身份；`40001` / `40014` / `42001` 时删 token 缓存 + 重取 **1 次**。
    ///
    /// 重试上限严格为 1 次：`code` 本身一次性，重试太多次既无收益也打爆企微频控。
    async fn code_to_session(&self, code: &str) -> Result<WeComSession, AppError> {
        let token = self.get_token(false).await?;
        match self.call_code2session(&token, code).await {
            Ok(sess) => Ok(sess),
            // 仅「access_token 失效类」重试；其余（40029 / 未识别 errcode /
            // 网络错误 / 上游 5xx）直接透传——重试无意义。
            Err(Code2SessionError::Fatal(e)) => Err(e),
            Err(Code2SessionError::TokenInvalid) => {
                tracing::info!("企业微信 access_token 疑似失效，删除缓存后重取 1 次");
                self.invalidate_token_cache().await;
                let token2 = self.get_token(true).await?;
                self.call_code2session(&token2, code).await.map_err(|e| match e {
                    Code2SessionError::TokenInvalid => AppError::biz(
                        code::BIZ_WX_LOGIN_FAILED,
                        "企业微信登录失败：access_token 失效，重取后仍失败",
                    ),
                    Code2SessionError::Fatal(e) => e,
                })
            }
        }
    }
}

// =============================================================================
// Noop 占位
// =============================================================================

/// `config.wecom.enabled == false`（未配置 corpid / corpsecret）时的静默占位。
///
/// ## 何时被用
/// - 本地 `cargo run` 未配 `WECOM_CORPID` / `WECOM_CORPSECRET`
/// - 集成测试（`TEST_DATABASE_BASE_URL` 快速路 + 无企微凭据）
/// - 任何「后端已部署但运维还没拿到 corpsecret」的窗口期
///
/// 一律返 `40109 BIZ_WX_NOT_CONFIGURED`（HTTP 503）——语义是「服务未就绪」，
/// 与「登录失败」40106 / 「未绑定」40107 明确区分，便于前端给出正确引导。
pub struct NoopWeComClient;

impl Default for NoopWeComClient {
    fn default() -> Self {
        Self
    }
}

#[async_trait]
impl WeComApiClient for NoopWeComClient {
    async fn code_to_session(&self, _code: &str) -> Result<WeComSession, AppError> {
        tracing::warn!("[NoopWeComClient] 企业微信登录未配置（WECOM_CORPID / WECOM_CORPSECRET 留空）");
        Err(AppError::biz(
            code::BIZ_WX_NOT_CONFIGURED,
            "企业微信登录未配置，请联系管理员设置 WECOM_CORPID / WECOM_CORPSECRET",
        ))
    }
}

/// 工厂函数：按 `enabled` 决定 `HttpWeComClient` 还是 `NoopWeComClient`。
///
/// 范式对齐 `infra::py_backend::build_py_backend`：
/// - `enabled=false` → `NoopWeComClient`（本地 / 测试不依赖企微网络）
/// - `enabled=true` → `HttpWeComClient`
/// - `enabled=true` 但 corpid 为空 → 强制 Noop（fail-safe，防御 `from_env` 被绕过）
pub fn build_wecom_client(
    cfg: &WeComConfig,
    redis: deadpool_redis::Pool,
) -> anyhow::Result<Arc<dyn WeComApiClient>> {
    if cfg.enabled && !cfg.corpid.trim().is_empty() {
        let http = HttpWeComClient::build_http_client(cfg)?;
        Ok(Arc::new(HttpWeComClient::new(http, redis, Arc::new(cfg.clone()))))
    } else {
        Ok(Arc::new(NoopWeComClient))
    }
}

// =============================================================================
// 单测
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::routing::get;
    use std::sync::{Arc, Mutex};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// 最小 mock 企微服务端：`WECOM_API_BASE` 指向它。
    ///
    /// 2026-09-29：本仓 dev-deps 没有 `wiremock`（也不打算为这轮新增 Cargo 依赖），
    /// 故用已在主依赖里的 `axum` 起一个监听 `127.0.0.1:0`（系统分配端口）的
    /// 最小 server，按路径返回可编程的 JSON。`reqwest` 侧用 `http://127.0.0.1:{port}`。
    struct MockWeComServer {
        base_url: String,
        /// gettoken 返回体
        gettoken_body: Arc<Mutex<serde_json::Value>>,
        /// jscode2session 返回体
        code2session_body: Arc<Mutex<serde_json::Value>>,
        /// gettoken 被调次数（验证缓存命中后不再调）
        gettoken_hits: Arc<AtomicUsize>,
        /// jscode2session 被调次数
        code2session_hits: Arc<AtomicUsize>,
    }

    impl MockWeComServer {
        async fn start() -> Self {
            let gettoken_body = Arc::new(Mutex::new(serde_json::json!({
                "errcode": 0, "errmsg": "ok", "access_token": "AT-1", "expires_in": 7200
            })));
            let code2session_body = Arc::new(Mutex::new(serde_json::json!({
                "errcode": 0, "errmsg": "ok", "corpid": "C1", "userid": "u1", "session_key": "SK-SECRET"
            })));
            let gettoken_hits = Arc::new(AtomicUsize::new(0));
            let code2session_hits = Arc::new(AtomicUsize::new(0));
            let tb = gettoken_body.clone();
            let cb = code2session_body.clone();
            let gh = gettoken_hits.clone();
            let ch = code2session_hits.clone();

            let app = Router::new()
                .route(
                    "/cgi-bin/gettoken",
                    get(move || {
                        let tb = tb.clone();
                        let gh = gh.clone();
                        async move {
                            gh.fetch_add(1, Ordering::SeqCst);
                            axum::Json(tb.lock().unwrap().clone())
                        }
                    }),
                )
                .route(
                    "/cgi-bin/miniprogram/jscode2session",
                    get(move || {
                        let cb = cb.clone();
                        let ch = ch.clone();
                        async move {
                            ch.fetch_add(1, Ordering::SeqCst);
                            axum::Json(cb.lock().unwrap().clone())
                        }
                    }),
                );

            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind mock wecom");
            let addr = listener.local_addr().expect("local_addr");
            tokio::spawn(async move {
                let _ = axum::serve(listener, app).await;
            });
            Self {
                base_url: format!("http://{addr}"),
                gettoken_body,
                code2session_body,
                gettoken_hits,
                code2session_hits,
            }
        }

        fn set_gettoken(&self, v: serde_json::Value) {
            *self.gettoken_body.lock().unwrap() = v;
        }
        fn set_code2session(&self, v: serde_json::Value) {
            *self.code2session_body.lock().unwrap() = v;
        }
        fn gettoken_calls(&self) -> usize {
            self.gettoken_hits.load(Ordering::SeqCst)
        }
        fn code2session_calls(&self) -> usize {
            self.code2session_hits.load(Ordering::SeqCst)
        }
    }

    /// 构造一个指向 mock server 的 `HttpWeComClient`。
    ///
    /// `force_token` 只为控制「每次都重取 token」，避免测试依赖 Redis 进程
    /// （本仓单测默认无 Redis；`code_to_session` 的缓存读路径在集成测试里覆盖）。
    fn client_for(base_url: &str) -> (HttpWeComClient, WeComConfig) {
        let cfg = WeComConfig {
            corpid: "C1".into(),
            corpsecret: "SECRET-DO-NOT-LOG".into(),
            api_base: base_url.to_string(),
            token_ttl_seconds: 6000,
            http_timeout_ms: 5000,
            enabled: true,
        };
        // 指向一个无人监听的 Redis 端口：本仓单测不依赖 Redis 进程，
        // 缓存读/写/删全部走「失败即降级」的 warn 分支，重试路径仍可跑。
        let redis_pool = deadpool_redis::Config::from_url("redis://127.0.0.1:1/0")
            .builder()
            .expect("deadpool builder")
            .max_size(1)
            .runtime(deadpool_redis::Runtime::Tokio1)
            .build()
            .expect("deadpool pool");
        let client = HttpWeComClient::new(
            HttpWeComClient::build_http_client(&cfg).expect("build client"),
            redis_pool,
            Arc::new(cfg.clone()),
        );
        (client, cfg)
    }

    // ---- 纯函数层（无需 IO）----

    #[test]
    fn classify_40029_is_terminal_login_failed() {
        match classify_wecom_errcode(40029, "invalid code") {
            Code2SessionError::Fatal(e) => {
                assert_eq!(e.code(), code::BIZ_WX_LOGIN_FAILED);
                // 40106 显式映射 401（不是 400 兜底）
                assert_eq!(e.http_status(), axum::http::StatusCode::UNAUTHORIZED);
            }
            Code2SessionError::TokenInvalid => panic!("40029 不应归入可重试类"),
        }
    }

    #[test]
    fn classify_access_token_class_is_retryable() {
        for ec in [40001, 40014, 42001] {
            assert!(is_access_token_class(ec), "{ec} 应归入 access_token 失效类");
            assert!(
                matches!(
                    classify_wecom_errcode(ec, "invalid credential"),
                    Code2SessionError::TokenInvalid
                ),
                "{ec} 应归入可重试类"
            );
        }
    }

    #[test]
    fn classify_unknown_errcode_is_internal() {
        match classify_wecom_errcode(40091, "secret is invalid") {
            Code2SessionError::Fatal(e) => {
                assert_eq!(e.code(), code::INTERNAL);
                assert_eq!(
                    e.http_status(),
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR
                );
            }
            Code2SessionError::TokenInvalid => panic!("40091 不应归入可重试类"),
        }
    }

    #[test]
    fn classify_never_prints_credentials() {
        // message 里只能出现 errcode + 企微 errmsg；凭据字面量绝不能出现
        match classify_wecom_errcode(99999, "boom") {
            Code2SessionError::Fatal(e) => {
                let s = e.to_string();
                assert!(!s.contains("SECRET"), "message 泄漏 secret: {s}");
                assert!(!s.contains("access_token="), "message 泄漏 token: {s}");
            }
            Code2SessionError::TokenInvalid => panic!("99999 不应归入可重试类"),
        }
    }

    // ---- IO 层（本地 mock axum server）----

    #[tokio::test]
    async fn code_to_session_success_drops_session_key() {
        let srv = MockWeComServer::start().await;
        let (client, _cfg) = client_for(&srv.base_url);
        let sess = client.code_to_session("code-1").await.expect("应成功");
        assert_eq!(sess.corp_id, "C1");
        assert_eq!(sess.user_id, "u1");
        // session_key 绝不出现在返回结构体的 Debug 里
        let dbg = format!("{sess:?}");
        assert!(!dbg.contains("SK-SECRET"), "session_key 泄漏: {dbg}");
    }

    #[tokio::test]
    async fn code_to_session_40029_returns_40106_without_retry() {
        let srv = MockWeComServer::start().await;
        srv.set_code2session(serde_json::json!({"errcode": 40029, "errmsg": "invalid code"}));
        let (client, _cfg) = client_for(&srv.base_url);
        let e = client.code_to_session("bad").await.expect_err("应失败");
        assert_eq!(e.code(), code::BIZ_WX_LOGIN_FAILED);
        // 不重试：jscode2session 只被调 1 次
        assert_eq!(srv.code2session_calls(), 1, "40029 不应重试");
    }

    #[tokio::test]
    async fn code_to_session_42001_retries_exactly_once() {
        let srv = MockWeComServer::start().await;
        // 第一次 jscode2session 报 42001（token 过期），后续返回成功
        srv.set_code2session(serde_json::json!({"errcode": 42001, "errmsg": "access_token expired"}));
        let (client, _cfg) = client_for(&srv.base_url);
        // Redis 不可用（端口 1）→ 缓存读写全降级 warn，重取路径仍可跑
        let res = client.code_to_session("code-1").await;
        assert_eq!(srv.code2session_calls(), 2, "42001 应重试恰好 1 次");
        // 首次取 token（缓存未命中）+ 重取（force_refresh）→ gettoken 共 2 次，
        // 不会因重试而反复打企微频控
        assert_eq!(srv.gettoken_calls(), 2, "gettoken 应恰好被调 2 次");
        // 重试后 mock 仍返 42001 → 最终 40106
        assert_eq!(res.expect_err("应失败").code(), code::BIZ_WX_LOGIN_FAILED);
    }

    #[tokio::test]
    async fn code_to_session_unknown_errcode_is_internal() {
        let srv = MockWeComServer::start().await;
        srv.set_code2session(serde_json::json!({"errcode": 40091, "errmsg": "secret is invalid"}));
        let (client, _cfg) = client_for(&srv.base_url);
        let e = client.code_to_session("code-1").await.expect_err("应失败");
        assert_eq!(e.code(), code::INTERNAL);
        // 未识别 errcode 不重试
        assert_eq!(srv.code2session_calls(), 1);
    }

    #[tokio::test]
    async fn gettoken_secret_invalid_is_internal() {
        let srv = MockWeComServer::start().await;
        srv.set_gettoken(serde_json::json!({"errcode": 40091, "errmsg": "secret is invalid"}));
        let (client, _cfg) = client_for(&srv.base_url);
        let e = client.code_to_session("code-1").await.expect_err("应失败");
        assert_eq!(e.code(), code::INTERNAL);
    }

    #[tokio::test]
    async fn errcode_zero_but_missing_userid_is_internal() {
        let srv = MockWeComServer::start().await;
        srv.set_code2session(serde_json::json!({"errcode": 0, "errmsg": "ok", "corpid": "C1"}));
        let (client, _cfg) = client_for(&srv.base_url);
        let e = client.code_to_session("code-1").await.expect_err("应失败");
        assert_eq!(e.code(), code::INTERNAL);
    }

    #[tokio::test]
    async fn network_error_is_internal() {
        // 指向一个无人监听的端口（拿到端口后立刻关闭）
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let (client, _cfg) = client_for(&format!("http://{addr}"));
        let e = client.code_to_session("code-1").await.expect_err("应失败");
        assert_eq!(e.code(), code::INTERNAL);
    }

    #[tokio::test]
    async fn noop_client_returns_40109() {
        let noop = NoopWeComClient;
        let e = noop.code_to_session("x").await.expect_err("应失败");
        assert_eq!(e.code(), code::BIZ_WX_NOT_CONFIGURED);
        assert_eq!(
            e.http_status(),
            axum::http::StatusCode::SERVICE_UNAVAILABLE
        );
    }
}

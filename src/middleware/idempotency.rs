// 2026-09-23 新增 Idempotency 中间件
//
// Header `Idempotency-Key: <key>` 启用；POST/PUT/PATCH 重复请求 24h 内直接
// 返回缓存响应。header 缺失 / GET / DELETE 全部 pass-through（零 Redis I/O）。
//
// 简化方案：key 不与 method/path 绑定，跨方法同 key 会撞车（这是 Stripe API
// 业界规范——client 自生成 UUID 撞 key 是 client bug）。
// 升级路径：把 key 改为 `idem:{method}:{path}:{key}` 即可（仅 T03 拼 key 行）。
//
// 2026-10-09：`idem:` 之外还会整体前置一层 `RedisIdempotencyStore::key_prefix`
// （见该 struct 的 doc）。本文件的 `format!("{KEY_PREFIX}{key}")` **保持不变**——
// 前缀是 Redis 专有设施，中间件层不该知道它。
//
// CachedResponse.headers 用 BTreeMap<String,String> 序列化会丢多值 header
// （如 set-cookie）—— 当前 API 形态（auth cookie + JSON 响应）不受影响。
//
// 2026-09-23 review #1 修复：内置 public path 闸门。即使 route_layer 顺序
// 正确，公开路径（login / refresh / health / _e2e）即便带 Idempotency-Key
// 也不该被缓存——否则 A POST /iam/login 带 K 的 200 OK 含 JWT 会被缓存，
// 后续任意请求带 K 即命中缓存拿到 A 的 JWT → session 劫持。这是与
// `auth::middleware::is_public_path` 的对偶防御：auth 闸门保证公开路径
// 不鉴权，本闸门保证公开路径不缓存。

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use async_trait::async_trait;
use axum::body::{Body, to_bytes};
use axum::extract::{FromRequestParts, Request, State};
use axum::http::header::CONTENT_TYPE;
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use deadpool_redis::Pool as RedisPool;
use deadpool_redis::redis::AsyncCommands;
// `automock` 仅在测试 build（`#[cfg_attr(test, automock)]`）时解析为 proc-macro
// 属性；非测试 build 编译器看到的是未识别符号，allow 静默之。
#[allow(unused_imports)]
use mockall::automock;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::state::AppState;

const HEADER_NAME: &str = "idempotency-key";
const KEY_PREFIX: &str = "idem:";
const KEY_MAX_LEN: usize = 255;

/// 缓存的响应（status + headers + body）。
///
/// header 序列化用 BTreeMap<String,String> 简化——多值 header（如 set-cookie）
/// 不被保留。2026-09-23 当前 API 形态（auth cookie 由 Set-Cookie 头分发，但
/// 走 response header 直接写而非 set-cookie 多值语义）不受影响。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedResponse {
    pub status: u16,
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
}

/// Idempotency 存储抽象（trait + Arc<dyn> 与现有 `CosClient` / `SessionStore` 同模式）
///
/// 2026-09-23 新增：
/// - `get` 返回 `Ok(None)` 表示未命中；
/// - `put` 把缓存写入底层存储 + 设置 TTL；
/// - key 由 caller 拼好（已含 `idem:` 前缀）——store 端不二次拼接 `idem:`。
///   2026-10-09 补一句：Redis 实现仍会在自己内部前置 `key_prefix`（进程级隔离用），
///   最终 key 是 `{key_prefix}idem:<client_key>`；该前缀不进 trait —— Noop / 内存
///   实现没有 Redis，不需要它。
#[cfg_attr(test, automock)]
#[async_trait]
pub trait IdempotencyStore: Send + Sync {
    async fn get(&self, key: &str) -> anyhow::Result<Option<CachedResponse>>;
    async fn put(&self, key: &str, value: &CachedResponse, ttl_seconds: u64) -> anyhow::Result<()>;
}

/// NoopIdempotencyStore —— 测试/本地占位实现
///
/// `get` 永远 `Ok(None)`（永远未命中）；`put` 永远 `Ok(())`（不真存）。
/// 让 service 在「没 Redis」或「测试不关心缓存命中」的场景下不报错。
#[derive(Debug, Default, Clone)]
pub struct NoopIdempotencyStore;

impl NoopIdempotencyStore {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl IdempotencyStore for NoopIdempotencyStore {
    async fn get(&self, _key: &str) -> anyhow::Result<Option<CachedResponse>> {
        Ok(None)
    }

    async fn put(
        &self,
        _key: &str,
        _value: &CachedResponse,
        _ttl_seconds: u64,
    ) -> anyhow::Result<()> {
        Ok(())
    }
}

/// 内存版 IdempotencyStore —— 单进程测试 fixture 用。
///
/// 数据结构：`Mutex<HashMap<String, InMemoryEntry>>`，每条带 `expires_at: Instant`。
/// `get` 路径上做 lazy 过期检查：过期 key 直接移除并返回 `None`，命中则在 TTL
/// 内返回 `CachedResponse`。`put` 时 caller 传的 `ttl_seconds` 显式落地到
/// `expires_at`——避免「参数被吞」的 silent footgun（M1 review）。
///
/// 进程重启数据丢失；多实例部署不能用——生产必走 `RedisIdempotencyStore`。
pub struct InMemoryIdempotencyStore {
    entries: Mutex<HashMap<String, InMemoryEntry>>,
}

/// 单条内存缓存：响应 + 过期时间点（Instant）。
///
/// 2026-09-23 review #1：把 `(CachedResponse, Instant)` 拆成具名结构体——
/// 旧的元组第二字段语义模糊（_inserted_at），TTL 实际未生效；改 `expires_at`
/// 后命名即文档，避免再误用。
struct InMemoryEntry {
    response: CachedResponse,
    expires_at: Instant,
}

impl InMemoryIdempotencyStore {
    pub fn new() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
        }
    }
}

impl Default for InMemoryIdempotencyStore {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl IdempotencyStore for InMemoryIdempotencyStore {
    async fn get(&self, key: &str) -> anyhow::Result<Option<CachedResponse>> {
        let mut guard = self.entries.lock().await;
        // lazy 过期：现取 `now()`，命中若过期则移除并返 None。
        let now = Instant::now();
        match guard.get(key) {
            Some(entry) if entry.expires_at > now => Ok(Some(entry.response.clone())),
            Some(_) => {
                // 过期——持有锁时移除避免后续 put 残留
                guard.remove(key);
                Ok(None)
            }
            None => Ok(None),
        }
    }

    async fn put(&self, key: &str, value: &CachedResponse, ttl_seconds: u64) -> anyhow::Result<()> {
        let mut guard = self.entries.lock().await;
        guard.insert(
            key.to_string(),
            InMemoryEntry {
                response: value.clone(),
                expires_at: Instant::now() + Duration::from_secs(ttl_seconds),
            },
        );
        Ok(())
    }
}

/// Redis 实现的 IdempotencyStore —— 生产路径。
///
/// key 形如 `idem:<client_key>`（caller 已拼好前缀）；value = `CachedResponse`。
/// `SET idem:<key> <json> EX <ttl_seconds>` 一次写入原子挂 TTL。
/// `GET idem:<key>` 反序列化失败 → 当作 `Ok(None)` 走 pass-through（不阻塞业务）。
///
/// 2026-10-09：`key` 前面还会前置 `key_prefix`（由 `new(pool, key_prefix)` 注入），
/// 最终落 Redis 的 key 是 `{key_prefix}idem:<client_key>`；`key_prefix` 为空串时与
/// 历史逐字节相同。caller（中间件主入口 `idempotency_middleware`）**不知道** Redis 的
/// 存在，故这一步在 store 内部完成——`IdempotencyStore` 还有 Noop / 内存实现，
/// 它们不需要前缀。
pub struct RedisIdempotencyStore {
    pool: RedisPool,
    /// 2026-10-09 新增：key 统一前缀（生产空串 / 测试 `t{pid}:`），语义见字段上方 doc。
    key_prefix: String,
}

impl RedisIdempotencyStore {
    pub fn new(pool: RedisPool, key_prefix: String) -> Self {
        Self { pool, key_prefix }
    }

    /// 把 caller 拼好的 `idem:<client_key>` 补上本 store 的前缀，得到真正落 Redis 的 key。
    fn redis_key(&self, key: &str) -> String {
        format!("{}{key}", self.key_prefix)
    }

    async fn conn(&self) -> anyhow::Result<deadpool_redis::Connection> {
        self.pool
            .get()
            .await
            .map_err(|e| anyhow::anyhow!("redis pool: {e}"))
    }
}

#[async_trait]
impl IdempotencyStore for RedisIdempotencyStore {
    async fn get(&self, key: &str) -> anyhow::Result<Option<CachedResponse>> {
        let redis_key = self.redis_key(key);
        let mut conn = self.conn().await?;
        let raw: Option<String> = conn
            .get(&redis_key)
            .await
            .with_context(|| format!("redis GET {redis_key}"))?;
        match raw {
            None => Ok(None),
            Some(s) => match serde_json::from_str::<CachedResponse>(&s) {
                Ok(v) => Ok(Some(v)),
                Err(e) => {
                    // 反序列化失败（Redis 里残留旧格式 / 被外部破坏）—— 当作未命中，
                    // 不阻塞业务。下次 put 会覆盖。
                    tracing::warn!(
                        key = %redis_key,
                        error = %e,
                        "idempotency 反序列化失败，按未命中处理"
                    );
                    Ok(None)
                }
            },
        }
    }

    async fn put(&self, key: &str, value: &CachedResponse, ttl_seconds: u64) -> anyhow::Result<()> {
        let redis_key = self.redis_key(key);
        let payload = serde_json::to_string(value)
            .with_context(|| format!("serialize CachedResponse for {redis_key}"))?;
        let mut conn = self.conn().await?;
        let _: () = conn
            .set_ex(&redis_key, payload, ttl_seconds)
            .await
            .with_context(|| format!("redis SET EX {redis_key}"))?;
        Ok(())
    }
}

// ===========================================================================
// 中间件主入口
// ===========================================================================

/// axum middleware：解析 `Idempotency-Key` header，命中缓存直接返回。
///
/// 决策矩阵（planner 2026-09-23 确认）：
/// - method 非 POST/PUT/PATCH → pass-through（不读 header、不读 Redis）
/// - header 缺失 → pass-through
/// - header 长度不在 1..=255 → pass-through（不传 4xx，避免 header 滥用阻断业务）
/// - header 命中缓存 → 重建 Response 返回
/// - header 未命中缓存 → 调 next.run，把响应 buffer 进内存缓存（spawn 写 Redis，
///   不阻塞主响应；写失败仅 warn）
///
/// 任何 Redis I/O 失败均 pass-through，不阻断业务（高可用降级）。
pub async fn idempotency_middleware(
    State(state): State<Arc<AppState>>,
    req: Request,
    next: Next,
) -> Response {
    // 1) method 过滤：只对 POST/PUT/PATCH 起作用
    if !matches!(
        req.method(),
        &axum::http::Method::POST | &axum::http::Method::PUT | &axum::http::Method::PATCH
    ) {
        return next.run(req).await;
    }

    // 1.5) 公开路径闸门：login / refresh / health / _e2e 的 200 OK 不该被缓存。
    // 防御目标：POST /iam/login 带 Idempotency-Key → 200 OK 含 access/refresh
    // token → 缓存条目 idem:<key> 含其它用户敏感数据；任意后续请求带同 key
    // 命中缓存即拿到原用户的 JWT → session 劫持。本闸门与 route_layer 顺序
    // （auth 在外层 = 先跑）是双保险：即便顺序错位也不缓存公开路径响应。
    if is_public_idempotency_path(req.uri().path()) {
        return next.run(req).await;
    }

    // 1.6) 2026-10-03 新增：打印路径闸门 —— 打印响应**直接 pass-through，不缓存**。
    // 防御目标：本中间件会把下游响应**整份 body 缓冲进内存并写进 Redis 24h**。
    // 前端打印请求（送货单 print / print-labels、零件 print-drawing /
    // print-drawing-batch）正常不带 Idempotency-Key，所以线上无感；但只要有人
    // 带上（网关/客户端统一注入、或未来某个打印批次的重试逻辑），多 MB 的
    // PDF / XLSX 就会被整份塞进 Redis —— 一次批量打印 20 件就是几十 MB，
    // 反复几次即可把实例 OOM。缓存打印二进制也毫无收益：打印是只读渲染，
    // 同一份 key 命中另一台机器的渲染结果反而可能是过期的。
    // 本闸门与 route_layer 顺序（auth 在外层 = 先跑）同样构成双保险：即便
    // 顺序错位也不缓存打印响应。
    if crate::middleware::timeout::is_print_path(req.uri().path()) {
        return next.run(req).await;
    }

    // 2) 提取 header 并校验长度
    let key = match extract_key(req.headers()) {
        Some(k) => k,
        None => return next.run(req).await,
    };

    // 3) buffer body 到内存（get 后重建 req 把同样的 bytes 喂给下游）
    let (parts, body) = req.into_parts();
    let max = state.config.max_request_body_size;
    let bytes = match to_bytes(body, max).await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(error = %e, "idempotency body 缓冲失败（body 超 max_request_body_size 或 client 断连），按未命中放行");
            let req = Request::from_parts(parts, Body::empty());
            return next.run(req).await;
        }
    };
    let req = Request::from_parts(parts, Body::from(bytes.clone()));

    // 4) 缓存命中检查
    let redis_key = format!("{KEY_PREFIX}{key}");
    match state.idempotency_store.get(&redis_key).await {
        Ok(Some(cached)) => {
            return build_cached_response(cached);
        }
        Ok(None) => {
            // 未命中，继续走下游
        }
        Err(e) => {
            // Redis 失败 — pass-through，不阻断业务
            tracing::warn!(
                key = %key,
                error = %e,
                "idempotency get 失败，按未命中放行"
            );
        }
    }

    // 5) 未命中 — 调下游 + buffer 响应
    let resp = next.run(req).await;

    // 6) 拆 parts + body 便于既 spawn 写缓存又重建 Response 返回
    //    （axum 0.8 Body 不实现 Clone，必须 into_parts 一次性消费）
    let (parts, body) = resp.into_parts();
    let body_bytes = match to_bytes(body, usize::MAX).await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(
                key = %redis_key,
                error = %e,
                "idempotency 响应 body buffer 失败，跳过缓存；直接返回原响应"
            );
            // 直接把空 body 拼回去（body 已消费），返回空响应 → 实际生产不应出现
            // 这一分支作为兜底。下游 handler 通常 body 较小，不太可能 buffer 失败。
            return Response::from_parts(parts, Body::empty());
        }
    };

    // 7) 构造缓存对象 + 异步 spawn 写 Redis（不影响主响应）
    let cached = CachedResponse {
        status: parts.status.as_u16(),
        headers: headers_to_map(&parts.headers),
        body: body_bytes.to_vec(),
    };
    let ttl = state.config.idempotency_ttl_seconds;
    let store = state.idempotency_store.clone();
    let redis_key_for_put = redis_key.clone();
    tokio::spawn(async move {
        if let Err(e) = store.put(&redis_key_for_put, &cached, ttl).await {
            tracing::warn!(
                key = %redis_key_for_put,
                error = %e,
                "idempotency cache put 失败（不影响主响应）"
            );
        }
    });

    // 8) 用同一 body_bytes 重建 Response 返回给客户端
    Response::from_parts(parts, Body::from(body_bytes))
}

/// 从 headers 抠 `Idempotency-Key: <key>`，长度 1..=255 返回 `Some(&str)`；
/// 缺失 / 长度非法 / 非 ASCII 字节一律 `None`（pass-through）。
fn extract_key(headers: &HeaderMap) -> Option<String> {
    let raw = headers.get(HEADER_NAME)?.to_str().ok()?;
    if raw.is_empty() || raw.len() > KEY_MAX_LEN {
        return None;
    }
    Some(raw.to_string())
}

/// 公开路径闸门（idempotency 对偶版）：login / refresh / health / _e2e 全子树
/// 不缓存响应（即便带 Idempotency-Key）。
///
/// 与 `crate::auth::middleware::is_public_path` 对偶：
/// - auth 闸门：公开路径不鉴权（health / login / refresh / _e2e 免 Bearer）；
/// - idem 闸门：公开路径不缓存（防止 POST /iam/login 的 200 OK 含 JWT 被
///   缓存后撞 key 劫持 session）。
///
/// 2026-09-23 review #1 实现：复制 auth 侧白名单逻辑，避免循环依赖（idem 是
/// infra 中间件层，auth 是 modules 之上层；调用方向反了）。两份白名单必须
/// 同步演化——新增公开路径时同时改这里与 auth 侧 `is_public_path`。
///
/// 2026-09-29 新增 `/wx/iam/wx-login`（企业微信小程序登录）。**这是最容易漏的
/// 一处**：wx-login 响应体含 `token` / `refresh_token`，若被本中间件缓存，
/// 攻击者只要复用同一个 `Idempotency-Key` 就能拿到别人的 access token。
/// 集成测试 `tests/wecom_login.rs::idem_key_does_not_cache_wx_login_response`
/// 就是为了钉死这条不变量。
///
/// 路径形式：生产 nest `/api/v2` + 模块子路径（`/api/v2/iam/login`）；测试
/// 直接挂 `v2_router` 时为 `/iam/login`。本函数先 strip `/api/v2` 前缀再匹配，
/// 两种调用模式都放行。
fn is_public_idempotency_path(path: &str) -> bool {
    let stripped = path.strip_prefix("/api/v2").unwrap_or(path);
    stripped == "/health"
        || stripped == "/iam/login"
        || stripped == "/iam/refresh"
        // 2026-09-29：企业微信小程序登录（响应含 JWT，**不可**缓存）
        || stripped == "/wx/iam/wx-login"
        || stripped == "/_e2e"
        || stripped.starts_with("/_e2e/")
}

/// 把 HeaderMap 扁平化为 `BTreeMap<String, String>`（供 CachedResponse 序列化）。
///
/// 多值 header（set-cookie 等）按第一值存（BTreeMap 单值约束）；当前 API 形态
/// 不依赖多值语义，简化方案足够。
fn headers_to_map(headers: &HeaderMap) -> BTreeMap<String, String> {
    let mut map: BTreeMap<String, String> = BTreeMap::new();
    for (name, value) in headers {
        if let Ok(s) = value.to_str() {
            map.insert(name.as_str().to_string(), s.to_string());
        }
    }
    map
}

/// 把 CachedResponse 还原成 axum Response。
fn build_cached_response(cached: CachedResponse) -> Response {
    let status = StatusCode::from_u16(cached.status).unwrap_or(StatusCode::OK);
    let mut builder = axum::response::Response::builder().status(status);
    for (k, v) in cached.headers.iter() {
        if let (Ok(name), Ok(value)) = (HeaderName::try_from(k), HeaderValue::from_str(v)) {
            builder = builder.header(name, value);
        }
    }
    // 兜底 content-type：没有显式 header 时补 application/json（业务响应都是 JSON）
    if !cached
        .headers
        .keys()
        .any(|k| k.eq_ignore_ascii_case("content-type"))
    {
        builder = builder.header(CONTENT_TYPE, "application/json");
    }
    builder
        .body(Body::from(cached.body))
        .unwrap_or_else(|_| (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response())
}

// ===========================================================================
// Extractor（可选；handler 调试用）
// ===========================================================================

/// `IdempotencyKey` extractor：从 request extensions 读 Idempotency-Key header。
///
/// `Some(String)` 表示带 header；`None` 表示未带。handler 调试 / 日志用。
/// 当前所有业务 handler 不引用，保留为可选 API（planner T03 决策 G）。
///
/// 读不到 / 解析失败 → `Ok(IdempotencyKey(None))`（不阻断业务）；type Rejection
/// 是 `Infallible`，extractor 永不失败。
pub struct IdempotencyKey(pub Option<String>);

impl<S> FromRequestParts<S> for IdempotencyKey
where
    S: Send + Sync,
{
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let raw = parts
            .headers
            .get(HEADER_NAME)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        Ok(IdempotencyKey(raw))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-10-03 新增：打印路径跳过闸门与**已注册路由**逐字对齐。
    ///
    /// 这条测试钉的是「改了一边忘了另一边」这个失效模式：若有人把
    /// `print-drawing` 改名而没同步 [`crate::middleware::timeout::is_print_path`]，
    /// 后果是双向的 —— 该端点既拿不到长档超时（批量打印被 30s 砍断），
    /// 打印响应又会重新落进 Redis 缓存。两者都不可接受。
    ///
    /// 断言方向：中间件只对 `is_print_path` 判定为真的路径跳过缓存，所以这里
    /// 反过来断言「全部已注册打印路由的两种路径形态（带 /api/v2 前缀的生产形态
    /// + 测试直挂 v2_router 的裸路径形态）都必须命中该判定」。
    ///
    /// 2026-10-08：送货单的两条打印路由随打印链路下线从 `is_print_path` 移除，
    /// 本表同步收窄到 parts 的 2 条（与 `middleware::timeout::is_print_path`
    /// 的判定表严格同源 —— 本测试守的就是「跳过缓存的闸门不被悄悄关掉」）。
    #[test]
    fn print_gate_covers_every_registered_print_route() {
        for p in [
            "/api/v2/parts/1234567890/print-drawing",
            "/api/v2/parts/print-drawing-batch",
            "/parts/1234567890/print-drawing",
            "/parts/print-drawing-batch",
        ] {
            assert!(
                crate::middleware::timeout::is_print_path(p),
                "打印路径应命中跳过闸门（否则多 MB 的 PDF/XLSX 会被整份塞进 Redis）：{p}"
            );
        }
    }

    /// 跳过闸门不能误伤普通端点：写路径（POST）仍必须正常进缓存。
    #[test]
    fn print_gate_does_not_swallow_regular_write_paths() {
        for p in [
            "/api/v2/com/delivery/note",
            "/api/v2/parts",
            "/api/v2/parts/1234567890/update",
            "/api/v2/com/delivery/note/1234567890/submit",
            // 名字相近但不是打印的路径：既不命中 is_print_path，也不该被跳过缓存
            "/api/v2/com/delivery/note/1234567890/print-preview",
            "/api/v2/parts/1234567890/print-drawing-batch",
        ] {
            assert!(
                !crate::middleware::timeout::is_print_path(p),
                "非打印路径不该命中跳过闸门（否则幂等能力被静默关掉）：{p}"
            );
        }
    }

    /// 公开路径闸门的既有语义保持不变（登录响应含 JWT，绝不可缓存）。
    #[test]
    fn public_path_gate_still_rejects_auth_endpoints() {
        assert!(is_public_idempotency_path("/api/v2/iam/login"));
        assert!(is_public_idempotency_path("/iam/login"));
        assert!(is_public_idempotency_path("/api/v2/iam/refresh"));
        assert!(is_public_idempotency_path("/api/v2/wx/iam/wx-login"));
        assert!(!is_public_idempotency_path(
            "/api/v2/delivery-notes/1/print"
        ));
    }

    /// `extract_key`：1..=255 长度才返回 `Some`，其余 pass-through。
    #[test]
    fn extract_key_enforces_length_bounds() {
        let mut h = HeaderMap::new();
        assert!(extract_key(&h).is_none(), "header 缺失 → None");

        h.insert(HEADER_NAME, HeaderValue::from_static("k"));
        assert_eq!(extract_key(&h).as_deref(), Some("k"));

        h.insert(HEADER_NAME, HeaderValue::from_static(""));
        assert!(extract_key(&h).is_none(), "空串 → None（pass-through）");

        let long = "x".repeat(KEY_MAX_LEN);
        let mut h2 = HeaderMap::new();
        h2.insert(HEADER_NAME, HeaderValue::from_str(&long).unwrap());
        assert_eq!(
            extract_key(&h2).as_deref(),
            Some(long.as_str()),
            "恰好 255 字符应放行"
        );

        let too_long = "x".repeat(KEY_MAX_LEN + 1);
        let mut h3 = HeaderMap::new();
        h3.insert(HEADER_NAME, HeaderValue::from_str(&too_long).unwrap());
        assert!(extract_key(&h3).is_none(), "超长 → None（不传 4xx）");
    }
}

/// 2026-10-09 新增（review 第 1 轮 Q10）：Redis 前缀拼接的**常驻**不变量单测。
///
/// 幂等 key 是**两层拼接**：中间件层 `format!("{KEY_PREFIX}{key}")`（`idem:<client_key>`，
/// 中间件不该知道 Redis 的存在）+ store 层 `RedisIdempotencyStore::redis_key` 的
/// `key_prefix`（进程级隔离）。生产 `key_prefix` 缺省空串 ⇒ 两层拼接结果必须与
/// 2026-10-09 之前**逐字节相同**，否则全部幂等缓存条目失配（同一请求被重复执行）。
#[cfg(test)]
mod key_prefix_tests {
    use super::{KEY_PREFIX, RedisIdempotencyStore};

    /// `deadpool_redis::Config::create_pool` 只构造 Manager、**不建连**，
    /// 故本测试零 IO、不需要 Redis 在场（与 `src/modules/wx/wecom_client.rs` 的
    /// `try_redis_pool` 注释同一条事实）。
    fn store(key_prefix: &str) -> RedisIdempotencyStore {
        let pool = deadpool_redis::Config::from_url("redis://127.0.0.1:1/0")
            .create_pool(Some(deadpool_redis::Runtime::Tokio1))
            .expect("构造连接池（不建连）");
        RedisIdempotencyStore::new(pool, key_prefix.to_string())
    }

    #[test]
    fn empty_prefix_reproduces_legacy_key_byte_for_byte() {
        let client_key = "req-abc-123";
        // 中间件层产出（历史逐字不变）
        let middleware_key = format!("{KEY_PREFIX}{client_key}");
        assert_eq!(middleware_key, "idem:req-abc-123");
        // store 层在空前缀下必须原样透传
        assert_eq!(store("").redis_key(&middleware_key), "idem:req-abc-123");
    }

    #[test]
    fn non_empty_prefix_is_prepended_whole() {
        let client_key = "req-abc-123";
        let middleware_key = format!("{KEY_PREFIX}{client_key}");
        let full = store("t12345:").redis_key(&middleware_key);
        assert_eq!(full, "t12345:idem:req-abc-123");
        assert_eq!(full.strip_prefix("t12345:"), Some(middleware_key.as_str()));
    }
}

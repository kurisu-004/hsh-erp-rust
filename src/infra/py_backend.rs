//! 转发到 python 后端的 HTTP 客户端抽象（trait + HttpPyBackend + NoopPyBackend）
//!
//! 2026-09-28 新增：修复 python STS 端口裸开认证漏洞。
//! 上轮"移除 STS session 设施"完成后，前端 `grantStsTmpKey` / `grantStsTmpKeyFiles`
//! 直接打到 python `POST /api/v1/files/sts-tmp-keys`（**未经过 backend-rust 鉴权**）。
//! 原因：删 `infra/python_sts.rs::HttpPythonSts`（rust → python STS 转发 HTTP 客户端）
//! 时，连带把"rust 鉴权后再转发"的薄壳端点也一起删了。
//!
//! 本模块提供新的薄壳端点 `POST /api/v2/files/sts-tmp-keys`：
//! - `PyBackendClient::forward_sts_tmp_keys`：handler 调用此方法透明转发请求到
//!   python `POST /api/v1/files/sts-tmp-keys`；
//! - 返回 `(status, headers, body)` 三元组（而非 `Response`），便于 mockall automock。
//!
//! 仿 `infra::cos.rs` 范式（trait + #[async_trait] + Send + Sync + Result<T, AppError>
//! + Noop 占位 + mockall automock）。

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::body::Bytes;
use axum::http::{HeaderMap, StatusCode};
// 2026-09-28 注：mockall `#[automock]` 默认无 cfg 门控（生成 `MockPyBackendClient`
// 类型始终可用）。`IdempotencyStore` / `SessionStore` 用 `#[cfg_attr(test, automock)]`
// 是因为它们的 mock 只在 lib 单元测试 (`src/**/tests/*.rs` 内的 `mod tests`) 使用；
// 本 `PyBackendClient` 的 mock 在 `tests/files_sts_tmp_keys.rs`（integration test，
// 编译时 `cfg(test)` 不传给主 lib）使用，故必须 always-on。代码膨胀 ≈ 200 行。
use mockall::automock;
use serde_json::Value;
use tracing::warn;

use crate::shared::error::{AppError, code};

/// rust → python 后端的转发客户端 trait
///
/// ## 2026-09-28 新增：薄壳鉴权转发
/// 修复 python STS 端点（`POST /api/v1/files/sts-tmp-keys`）裸开漏洞——
/// 此前删 `upload_session` 域时连带删了原 `infra::python_sts::HttpPythonSts`，
/// 但没有新增"rust 鉴权后再转发"的薄壳端点，导致 python 端 STS 签发绕过了
/// 整个 IAM 鉴权。本 trait 是新鉴权链路的 HTTP 客户端抽象。
///
/// ## 设计要点
/// - 返回 `(StatusCode, HeaderMap, Bytes)` 三元组而非 `axum::Response`：便于
///   mockall automock（`Response` 内部含 body stream + extensions，mockall
///   难合成；handler 拿到三元组后自己拼 Response）。
/// - trait 故意不暴露 `base_url` / `timeout`：HTTP 客户端对调用方透明，仅
///   「透传 body + headers」语义。Python 端切换、负载均衡等运维变更不影响 handler。
// 2026-09-28 新增：mockall automock always-on（生成 `MockPyBackendClient`）。
// 见上方 `use mockall::automock` 注释 —— integration test (`tests/*.rs`)
// 编译时 `cfg(test)` 不传给主 lib，故不能 `#[cfg_attr(test, automock)]`。
#[automock]
#[async_trait]
pub trait PyBackendClient: Send + Sync {
    /// 透传转发 STS 临时凭证签发请求到 python 后端 `POST /api/v1/files/sts-tmp-keys`。
    ///
    /// - `body`：前端发来的 JSON（已由 handler 用 `Json<Value>` 提取，转发时
    ///   序列化为字符串原样 POST 出去）。
    /// - `headers`：原始请求 headers（鉴权头**不**透传给 python——python 端
    ///   继续裸开 by design，不应依赖 rust 端的鉴权头）。
    ///
    /// 返回 `(status, headers, body)` 三元组：handler 拼装 `Response` 时保留
    /// 原 status + headers + body 字节流。
    async fn forward_sts_tmp_keys(
        &self,
        body: Value,
        headers: HeaderMap,
    ) -> Result<PyBackendResponse, AppError>;
}

/// python 后端响应（透传给前端）
#[derive(Debug, Clone)]
pub struct PyBackendResponse {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Bytes,
}

/// 真实 HTTP 客户端：往 `PYTHON_BACKEND_BASE_URL/api/v1/files/sts-tmp-keys`
/// 发起 POST，body = 调用方提供的 JSON 序列化字符串，response 原样透传。
///
/// ## 2026-09-28 实现要点
/// - 复用 Cargo.toml 里的 `reqwest = { rustls-tls, json }` 依赖；
/// - `Content-Type: application/json` 由 reqwest 的 `.json()` 设置；
/// - 鉴权头**故意不**透传（python 端裸开 by design，不能反向依赖 rust 端 JWT）；
/// - `Authorization` / `Cookie` / `X-Request-Id` 等敏感 header 过滤掉；
/// - timeout = `PythonBackendConfig::timeout_ms`（默认 10s，与前端 axios 30s timeout
///   错开，给 rust 足够时间）；
/// - 任意网络错误 / 超时 → 502 BAD_GATEWAY + `BIZ_STS_FORWARD_FAILED = 20406`。
pub struct HttpPyBackend {
    client: reqwest::Client,
    base_url: String,
    timeout: Duration,
}

impl HttpPyBackend {
    pub fn new(base_url: String, timeout: Duration) -> anyhow::Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(timeout)
            // 不跟随重定向（避免与 python 端重定向语义不一致）
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| anyhow::anyhow!("构造 reqwest client 失败: {e}"))?;
        Ok(Self {
            client,
            base_url,
            timeout,
        })
    }
}

/// 过滤后的 header 透传：去掉 hop-by-hop + 鉴权头，避免把 rust 端 JWT / Cookie
/// 反向暴露给 python 端。
///
/// ## 2026-09-28 过滤清单
/// - `Authorization` —— 不能让 python 端"借"rust 的 JWT 上下文
/// - `Cookie` —— 同理
/// - `Host` / `Content-Length` —— reqwest 会自己处理
/// - `Connection` / `Keep-Alive` / `Transfer-Encoding` / `Upgrade` —— hop-by-hop
/// - `X-Request-Id` —— 透传（trace id 透传，前端 Nginx → rust → python 一致）
fn filter_request_headers(src: &HeaderMap) -> reqwest::header::HeaderMap {
    let mut out = reqwest::header::HeaderMap::new();
    const SKIP: &[&str] = &[
        "authorization",
        "cookie",
        "host",
        "content-length",
        "connection",
        "keep-alive",
        "transfer-encoding",
        "upgrade",
        "te",
        "trailer",
    ];
    for (k, v) in src.iter() {
        let name = k.as_str().to_ascii_lowercase();
        if SKIP.contains(&name.as_str()) {
            continue;
        }
        if let (Ok(nk), Ok(nv)) = (
            reqwest::header::HeaderName::from_bytes(k.as_str().as_bytes()),
            reqwest::header::HeaderValue::from_bytes(v.as_bytes()),
        ) {
            out.append(nk, nv);
        }
    }
    out
}

/// 把 `axum::http::HeaderMap` 复制到 `axum::http::HeaderMap`（同类型，handler 透传）
fn copy_response_headers(src: &reqwest::header::HeaderMap) -> HeaderMap {
    let mut out = HeaderMap::new();
    for (k, v) in src.iter() {
        if let (Ok(nk), Ok(nv)) = (
            axum::http::HeaderName::from_bytes(k.as_str().as_bytes()),
            axum::http::HeaderValue::from_bytes(v.as_bytes()),
        ) {
            out.append(nk, nv);
        }
    }
    out
}

#[async_trait]
impl PyBackendClient for HttpPyBackend {
    async fn forward_sts_tmp_keys(
        &self,
        body: Value,
        headers: HeaderMap,
    ) -> Result<PyBackendResponse, AppError> {
        let url = format!("{}/api/v1/files/sts-tmp-keys", self.base_url);
        let filtered = filter_request_headers(&headers);
        tracing::debug!(
            url = %url,
            timeout_ms = self.timeout.as_millis() as u64,
            "转发 STS tmp-keys 请求到 python 后端"
        );
        let resp = self
            .client
            .post(&url)
            .headers(filtered)
            .json(&body)
            .send()
            .await
            .map_err(|e| {
                warn!(error = %e, url = %url, "转发 STS 请求失败（网络层）");
                AppError::biz(
                    code::BIZ_STS_FORWARD_FAILED,
                    format!("转发 STS 请求到 python 失败: {e}"),
                )
            })?;
        let status = StatusCode::from_u16(resp.status().as_u16())
            .unwrap_or(StatusCode::BAD_GATEWAY);
        let resp_headers = resp.headers().clone();
        let body_bytes = resp.bytes().await.map_err(|e| {
            warn!(error = %e, url = %url, "读取 python 响应 body 失败");
            AppError::biz(
                code::BIZ_STS_FORWARD_FAILED,
                format!("读取 python 响应 body 失败: {e}"),
            )
        })?;
        Ok(PyBackendResponse {
            status,
            headers: copy_response_headers(&resp_headers),
            body: body_bytes,
        })
    }
}

/// `PYTHON_BACKEND_ENABLED=false` 时的静默占位实现（与 `NoopCos` 同形）。
///
/// ## 2026-09-28 设计
/// - 与生产 `HttpPyBackend` 行为完全不一致（不会真发 HTTP 请求）；仅供
///   本地 `cargo run` 不依赖 python 后端时调试用。
/// - 返回 `501 Not Implemented` + 占位 body（与 `local://` URL 同语义），便于
///   前端联调时快速识别「未配 python base url」。
pub struct NoopPyBackend;

impl Default for NoopPyBackend {
    fn default() -> Self {
        Self
    }
}

#[async_trait]
impl PyBackendClient for NoopPyBackend {
    async fn forward_sts_tmp_keys(
        &self,
        _body: Value,
        _headers: HeaderMap,
    ) -> Result<PyBackendResponse, AppError> {
        warn!("[NoopPyBackend] 跳过真实转发（PYTHON_BACKEND_ENABLED=false，本地调试）");
        let body = serde_json::json!({
            "code": 50001,
            "message": "NoopPyBackend 未配置 PYTHON_BACKEND_BASE_URL（本地调试占位）",
            "data": null,
        });
        Ok(PyBackendResponse {
            status: StatusCode::NOT_IMPLEMENTED,
            headers: HeaderMap::new(),
            body: Bytes::from(serde_json::to_vec(&body).unwrap_or_default()),
        })
    }
}

/// 工厂函数：按 `enabled` 决定走 `HttpPyBackend` 还是 `NoopPyBackend`。
///
/// ## 2026-09-28 设计
/// - `enabled=false` → `NoopPyBackend`（本地 `cargo run` 不依赖 python）；
/// - `enabled=true` → `HttpPyBackend`（生产 / 测试，转发到 `base_url`）；
/// - `base_url` 为空 → 强制 Noop（fail-safe），避免 `reqwest` 收到 `http://` 空 host。
pub fn build_py_backend(
    cfg: &crate::infra::config::PythonBackendConfig,
) -> anyhow::Result<Arc<dyn PyBackendClient>> {
    if cfg.enabled && !cfg.base_url.trim().is_empty() {
        let timeout = Duration::from_millis(cfg.timeout_ms);
        Ok(Arc::new(HttpPyBackend::new(cfg.base_url.clone(), timeout)?))
    } else {
        Ok(Arc::new(NoopPyBackend))
    }
}
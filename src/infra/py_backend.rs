//! 转发到 python 后端的 HTTP 客户端抽象（trait + HttpPyBackend + NoopPyBackend）
//!
//! 全仓唯一的「rust → python」出口，共两条转发链路，都只做一件事：
//! **rust 侧强制鉴权 + RBAC 之后，把请求原样送到 python，把响应原样带回前端。**
//! python 端不反向依赖 rust 的 JWT（`Authorization` / `Cookie` 在转发前被剥掉）。
//!
//! ## 端点对照（v2 = rust 对外，v1 = python 现状）
//!
//! | v2（rust 对外） | v1（python） | 业务错误码 |
//! |---|---|---|
//! | `POST /api/v2/files/sts-tmp-keys` | `POST /api/v1/files/sts-tmp-keys` | `BIZ_STS_FORWARD_FAILED` = 20406 |
//! | `POST /api/v2/delivery-notes/{id}/print` | `POST /api/v1/delivery-notes/{id}/print` | `BIZ_PRINT_FORWARD_FAILED` = 20407 |
//! | `POST /api/v2/delivery-notes/{id}/print-labels` | `POST /api/v1/delivery-notes/{id}/print-labels` | 同上 |
//! | `GET /api/v2/parts/{id}/print-drawing` | `GET /api/v1/parts/{id}/print` | 同上 |
//! | `POST /api/v2/parts/print-drawing-batch` | `POST /api/v1/parts/print-batch` | 同上 |
//!
//! v2 → v1 的 URL 拼装**只存在于 [`HttpPyBackend`] 的 impl 里**（每方法 1 行
//! `format!`）；后两条零件端点与 python 端路径不同名（`print-drawing` → `print`、
//! `print-drawing-batch` → `print-batch`），映射差异集中在那两行，改名只动那里。
//!
//! ## 为什么打印不重新实现
//! 打印的 4 个真实渲染动作——PDF 光栅化、pikepdf 合并、ReportLab 条码背面、
//! openpyxl 填送货单模板——全在 python 侧。rust 端只提供「必须带 JWT 才能打印」
//! 这道闸门 + 一条能撑住分钟级耗时的通道，重写渲染等于把两套渲染实现长期并存。
//!
//! ## 两档超时
//! STS 档缺省 10s、打印档缺省 600s，相差 60 倍，client 级单一 timeout 表达不了：
//! [`HttpPyBackend`] 存两档，打印 4 个方法用 `RequestBuilder::timeout` 逐请求覆盖
//! client 默认值。打印档换算成秒后必须严格小于
//! [`AppConfig::print_request_timeout_seconds`](crate::infra::config::AppConfig::print_request_timeout_seconds)，
//! 否则先到点的是 rust，python 的真实错误被 408 掩盖。
//!
//! ## 错误码分家
//! 两条链路各自独立命名（20406 / 20407）：前端与日志能直接看出是哪条链路挂了，
//! 排查时不必先确认是 STS 还是打印。
//!
//! 仿 `infra::cos.rs` 范式（trait + #[async_trait] + Send + Sync + Result<T, AppError>
//! + Noop 占位 + mockall automock）。

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::body::Bytes;
use axum::http::header::CONTENT_LENGTH;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
// mockall `#[automock]` 默认无 cfg 门控（生成 `MockPyBackendClient` 类型始终可用）。
// `IdempotencyStore` / `SessionStore` 用 `#[cfg_attr(test, automock)]`
// 是因为它们的 mock 只在 lib 单元测试 (`src/**/tests/*.rs` 内的 `mod tests`) 使用；
// 本 `PyBackendClient` 的 mock 在 `tests/*.rs`（integration test，编译时 `cfg(test)`
// 不传给主 lib）使用，故必须 always-on。代码膨胀 ≈ 200 行。
use mockall::automock;
use serde_json::Value;
use tracing::warn;

use crate::shared::error::{AppError, code};

/// rust → python 后端的转发客户端 trait
///
/// ## 设计要点
/// - 返回 [`PyBackendResponse`]（status + headers + body 三元组的具名形态）而非
///   `axum::Response`：后者内部含 body stream + extensions，mockall 难合成；
///   handler 拿到三元组后自己拼 Response。
/// - trait 故意不暴露 `base_url` / `timeout`：HTTP 客户端对调用方透明，仅
///   「透传 body + headers」语义。Python 端切换、负载均衡等运维变更不影响 handler。
/// - 鉴权头（`Authorization` / `Cookie`）在 impl 内被剥掉，python 端继续裸开，
///   不反向依赖 rust 端 JWT；身份以 `X-Forwarded-User-Id` 单头透传。
#[automock]
#[async_trait]
pub trait PyBackendClient: Send + Sync {
    /// 透传转发 STS 临时凭证签发请求到 python 后端 `POST /api/v1/files/sts-tmp-keys`。
    ///
    /// - `body`：前端发来的 JSON（handler 用 `Json<Value>` 提取，转发时序列化为
    ///   字符串原样 POST 出去）。
    /// - `headers`：原始请求 headers（鉴权头由 impl 过滤，不透传给 python）。
    ///
    /// 失败（网络层超时 / 连接拒 / 读 body 失败）→ 502 + `BIZ_STS_FORWARD_FAILED`。
    async fn forward_sts_tmp_keys(
        &self,
        body: Value,
        headers: HeaderMap,
    ) -> Result<PyBackendResponse, AppError>;

    /// 2026-10-03 新增：送货单打印转发 → python `POST /api/v1/delivery-notes/{id}/print`。
    ///
    /// `body` 是 `Json<Value>` 原样透传：前端发的雪花 ID 是 string（> 2^53），
    /// rust 侧不定义强类型 DTO、不解析字段，由 python 侧 `parse_snowflake_id` 解析。
    async fn forward_delivery_note_print(
        &self,
        note_id: &str,
        body: Value,
        headers: HeaderMap,
    ) -> Result<PyBackendResponse, AppError>;

    /// 2026-10-03 新增：送货单标签打印转发
    /// → python `POST /api/v1/delivery-notes/{id}/print-labels`。
    async fn forward_delivery_note_labels(
        &self,
        note_id: &str,
        body: Value,
        headers: HeaderMap,
    ) -> Result<PyBackendResponse, AppError>;

    /// 2026-10-03 新增：单件零件图纸 PDF 转发
    /// → python `GET /api/v1/parts/{id}/print`（**与 v2 路径不同名**）。
    ///
    /// - `query`：原始 query 串（如 `vector=true`）**原样**透传，rust 侧不解析
    ///   `?vector=<bool>`（python 侧是 `Query(bool)`，语义由 python 负责）。
    async fn forward_part_print_pdf(
        &self,
        part_id: &str,
        query: Option<String>,
        headers: HeaderMap,
    ) -> Result<PyBackendResponse, AppError>;

    /// 2026-10-03 新增：批量零件图纸 PDF 转发
    /// → python `POST /api/v1/parts/print-batch`（**与 v2 路径不同名**）。
    async fn forward_part_print_pdf_batch(
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

/// 真实 HTTP 客户端：往 `PYTHON_BACKEND_BASE_URL/api/v1/...` 发起请求，
/// 响应经 [`filter_response_headers`] 清洗后透传。
///
/// ## 两档超时
/// - `timeout`：client 级默认值，走 STS 那条 10s 档（缺省
///   `PythonBackendConfig::timeout_ms` = 10s）。
/// - `print_timeout`：打印 4 个方法逐请求覆盖用（缺省
///   `PythonBackendConfig::print_timeout_ms` = 600s）。批量图纸打印 20 件/批、
///   前端并发 3 批，合法耗时数分钟，与 STS 的 10s 差 60 倍，client 级单一值
///   表达不了。
///
/// ## 其余实现要点
/// - 复用 Cargo.toml 里的 `reqwest = { rustls-tls, json }` 依赖；
/// - `Content-Type: application/json` 由 reqwest 的 `.json()` 设置；
/// - 不跟随重定向（避免与 python 端重定向语义不一致）；
/// - 任意网络错误 / 超时 → 502 BAD_GATEWAY + 调用方指定的业务错误码。
pub struct HttpPyBackend {
    client: reqwest::Client,
    base_url: String,
    timeout: Duration,
    print_timeout: Duration,
}

impl HttpPyBackend {
    /// `print_timeout` 缺省与 `timeout` 同档；打印链路请显式传
    /// `PythonBackendConfig::print_timeout_ms` 换算值（生产由
    /// [`build_py_backend`] 接上）。
    pub fn new(
        base_url: String,
        timeout: Duration,
        print_timeout: Duration,
    ) -> anyhow::Result<Self> {
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
            print_timeout,
        })
    }

    /// 5 个转发方法的唯一执行体：拼 URL → 过滤请求头 → 发请求 → 清洗响应头。
    ///
    /// - `query`：非空时以**原始 query 串**追加到 URL（`?{query}`）。刻意不走
    ///   `RequestBuilder::query()`——那会把 `&` / `=` 当作待转义的值再编码一次，
    ///   python 侧收到的是字面量而非参数。
    /// - `timeout`：逐请求覆盖 client 默认值（打印 4 方法传 `self.print_timeout`）。
    /// - `err_code`：失败时用的业务错误码（STS 20406 / 打印 20407）。由调用方
    ///   传而不是内部判路径——判定一旦散进 impl，URL 映射就不是唯一映射点了。
    #[allow(clippy::too_many_arguments)]
    async fn send(
        &self,
        method: reqwest::Method,
        url: String,
        json: Option<Value>,
        query: Option<String>,
        headers: HeaderMap,
        timeout: Duration,
        err_code: i32,
    ) -> Result<PyBackendResponse, AppError> {
        let url = match query.filter(|q| !q.is_empty()) {
            Some(q) => format!("{url}?{q}"),
            None => url,
        };
        let filtered = filter_request_headers(&headers);
        tracing::debug!(
            method = %method,
            url = %url,
            timeout_ms = timeout.as_millis() as u64,
            "转发请求到 python 后端"
        );
        let mut builder = self
            .client
            .request(method, &url)
            .headers(filtered)
            .timeout(timeout);
        if let Some(body) = json {
            builder = builder.json(&body);
        }
        let resp = builder.send().await.map_err(|e| {
            warn!(error = %e, url = %url, "转发请求到 python 失败（网络层）");
            AppError::biz(err_code, format!("转发请求到 python 失败: {e}"))
        })?;
        let status =
            StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
        let raw_headers = resp.headers().clone();
        let body_bytes = resp.bytes().await.map_err(|e| {
            warn!(error = %e, url = %url, "读取 python 响应 body 失败");
            AppError::biz(err_code, format!("读取 python 响应 body 失败: {e}"))
        })?;
        Ok(PyBackendResponse {
            status,
            headers: filter_response_headers(&raw_headers, body_bytes.len()),
            body: body_bytes,
        })
    }
}

/// 过滤敏感请求头：去掉鉴权头 + hop-by-hop，避免把 rust 端 JWT / Cookie 反向
/// 暴露给 python 端。
///
/// ## 过滤清单
/// - `Authorization` / `Cookie` —— 不能让 python 端"借"rust 的 JWT 上下文
/// - `Host` / `Content-Length` / `Content-Type` —— reqwest 自管：body 由
///   `RequestBuilder::json()` 序列化，它自己写 `content-type: application/json`；
///   而 `.headers(filtered)` 走 `HeaderMap::append` 是**追加**语义，前端带来的
///   `content-type` 若也放行，转发出去的请求会带两条 `content-type`。
///   与响应侧对 `content-length` 用 `insert` 消重是同一件事。
/// - `Connection` / `Keep-Alive` / `Transfer-Encoding` / `Upgrade` / `Te` /
///   `Trailer` —— hop-by-hop
/// - **不**含 `X-Forwarded-User-Id` —— handler 注入的 python 端身份依据，必须放行
/// - `X-Request-Id` —— 透传（trace id 在 前端 Nginx → rust → python 一致）
fn filter_request_headers(src: &HeaderMap) -> HeaderMap {
    let mut out = HeaderMap::new();
    const SKIP: &[&str] = &[
        "authorization",
        "cookie",
        "host",
        "content-length",
        "content-type",
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
        out.append(k.clone(), v.clone());
    }
    out
}

/// 2026-10-03 新增：清洗 python 响应的 header 后再交给 axum。
///
/// ## 为什么不能原样透传
/// python 端响应的 header 是给「它自己的下游」写的，直接搬进 rust 的 axum
/// response 会带出三类隐患：
/// - hop-by-hop 头（`connection` / `keep-alive` / `transfer-encoding` / `upgrade` /
///   `te` / `trailer`）—— 它们描述的是 python↔rust 这一跳的连接语义，对
///   rust↔前端那一跳无效甚至是错的；
/// - `content-encoding` —— 见下节；
/// - `content-length` —— 与实际 body 长度不一致时（gzip/chunked 解码后、
///   或上游自己写错），前端 `responseType: 'blob'` 的下载会被截断。打印响应是
///   多 MB 的 PDF / XLSX，截断表现为「下到一个坏文件」且无报错，极难排查。
///   故一律**用实际 body 长度重算**。
///
/// `date` / `server` 也一并剥掉：它们是上游 server 的自我标识，rust 自己会写。
///
/// ## `content-encoding` 为什么必须剥
/// `reqwest` 开了 `gzip` feature（`Cargo.toml`）：发请求时自动带
/// `accept-encoding: gzip`，收到 `content-encoding: gzip` 的响应时在**解码层**把 body
/// 还原成明文（tower-http `decompression-gzip`，并在同一层摘掉 `content-encoding` 与
/// `content-length`）。因此到达本函数时 body 是明文字节。
///
/// 若把 `content-encoding` 透传出去而 body 其实是明文，前端会拿「声明为 gzip 实为
/// 明文」的数据去解压 ⇒ 静默下到一个坏文件。反过来若**既不解码又剥头**，更糟：
/// body 是原始 gzip 字节、头被剥掉，浏览器按 `content-type` 当 PDF 解析 ⇒ 同样无报错。
/// 解码与剥头必须成对，缺一不可。
///
/// SKIP 清单里保留 `content-encoding` 是防御性兜底（当前 `reqwest` 已先摘过一道，
/// 走到这里通常已无此头）：万一将来换用未开 `gzip` feature 的 client，宁可剥掉一个
/// 冗余头，也不能让「编码声明」与「明文 body」错配流到前端。
///
/// ## 保留项
/// - `content-type` —— 前端靠它区分 PDF / xlsx
/// - `content-disposition` —— 前端 `parseFilename`（`src/api/deliveryNote.ts`）
///   靠它取下载文件名
/// - `cache-control` —— python 端对 PDF 给了 `private, max-age=600`，语义照搬
/// - 其余自定义头（`x-request-id` 等）—— 透传
fn filter_response_headers(src: &HeaderMap, body_len: usize) -> HeaderMap {
    /// 逐字清单：hop-by-hop 全套 + 与已解码 body 冲突 / 无意义的头。
    const SKIP: &[&str] = &[
        // hop-by-hop（RFC 9110 §7.6.1）
        "connection",
        "keep-alive",
        "transfer-encoding",
        "upgrade",
        "te",
        "trailer",
        // body 已被 reqwest 解码成明文，再声明编码会让前端误解
        "content-encoding",
        // 上游 server 的自我标识
        "date",
        "server",
    ];
    let mut out = HeaderMap::new();
    for (k, v) in src.iter() {
        let name = k.as_str().to_ascii_lowercase();
        if SKIP.contains(&name.as_str()) {
            continue;
        }
        out.append(k.clone(), v.clone());
    }
    if let Ok(v) = HeaderValue::from_str(&body_len.to_string()) {
        out.insert(CONTENT_LENGTH, v);
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
        // v2 `POST /files/sts-tmp-keys` → v1 `POST /api/v1/files/sts-tmp-keys`
        let url = format!("{}/api/v1/files/sts-tmp-keys", self.base_url);
        self.send(
            reqwest::Method::POST,
            url,
            Some(body),
            None,
            headers,
            self.timeout,
            code::BIZ_STS_FORWARD_FAILED,
        )
        .await
    }

    async fn forward_delivery_note_print(
        &self,
        note_id: &str,
        body: Value,
        headers: HeaderMap,
    ) -> Result<PyBackendResponse, AppError> {
        // 2026-10-03 新增。v2 `POST /delivery-notes/{id}/print` 与 python 端同名同路径段。
        let url = format!("{}/api/v1/delivery-notes/{note_id}/print", self.base_url);
        self.send(
            reqwest::Method::POST,
            url,
            Some(body),
            None,
            headers,
            self.print_timeout,
            code::BIZ_PRINT_FORWARD_FAILED,
        )
        .await
    }

    async fn forward_delivery_note_labels(
        &self,
        note_id: &str,
        body: Value,
        headers: HeaderMap,
    ) -> Result<PyBackendResponse, AppError> {
        // 2026-10-03 新增。v2 `POST /delivery-notes/{id}/print-labels` 与 python 端同名。
        let url = format!(
            "{}/api/v1/delivery-notes/{note_id}/print-labels",
            self.base_url
        );
        self.send(
            reqwest::Method::POST,
            url,
            Some(body),
            None,
            headers,
            self.print_timeout,
            code::BIZ_PRINT_FORWARD_FAILED,
        )
        .await
    }

    async fn forward_part_print_pdf(
        &self,
        part_id: &str,
        query: Option<String>,
        headers: HeaderMap,
    ) -> Result<PyBackendResponse, AppError> {
        // 2026-10-03 新增。v2 `GET /parts/{id}/print-drawing` → python 端叫
        // `GET /parts/{id}/print`：**与 v2 不同名**，映射只存在于这一行。
        let url = format!("{}/api/v1/parts/{part_id}/print", self.base_url);
        self.send(
            reqwest::Method::GET,
            url,
            None,
            query,
            headers,
            self.print_timeout,
            code::BIZ_PRINT_FORWARD_FAILED,
        )
        .await
    }

    async fn forward_part_print_pdf_batch(
        &self,
        body: Value,
        headers: HeaderMap,
    ) -> Result<PyBackendResponse, AppError> {
        // 2026-10-03 新增。v2 `POST /parts/print-drawing-batch` → python 端叫
        // `POST /parts/print-batch`：**与 v2 不同名**，映射只存在于这一行。
        let url = format!("{}/api/v1/parts/print-batch", self.base_url);
        self.send(
            reqwest::Method::POST,
            url,
            Some(body),
            None,
            headers,
            self.print_timeout,
            code::BIZ_PRINT_FORWARD_FAILED,
        )
        .await
    }
}

/// `PYTHON_BACKEND_ENABLED=false` 时的静默占位实现（与 `NoopCos` 同形）。
///
/// 与生产 `HttpPyBackend` 行为完全不一致（不会真发 HTTP 请求）；仅供
/// 本地 `cargo run` 不依赖 python 后端时调试用。5 个方法统一返
/// 「配置缺失」语义的业务错误（STS 20406 / 打印 20407），让 handler 走
/// `AppError::into_response()` 自动装信封；不手拼 `R { code, message, data }`
/// —— 与 `HttpPyBackend` 错误路径同源。
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
        Err(AppError::biz(
            code::BIZ_STS_FORWARD_FAILED,
            "PYTHON_BACKEND_BASE_URL 未配置（NoopPyBackend 占位）",
        ))
    }

    async fn forward_delivery_note_print(
        &self,
        _note_id: &str,
        _body: Value,
        _headers: HeaderMap,
    ) -> Result<PyBackendResponse, AppError> {
        warn!("[NoopPyBackend] 跳过送货单打印转发（PYTHON_BACKEND_ENABLED=false）");
        Err(AppError::biz(
            code::BIZ_PRINT_FORWARD_FAILED,
            "PYTHON_BACKEND_BASE_URL 未配置（NoopPyBackend 占位）",
        ))
    }

    async fn forward_delivery_note_labels(
        &self,
        _note_id: &str,
        _body: Value,
        _headers: HeaderMap,
    ) -> Result<PyBackendResponse, AppError> {
        warn!("[NoopPyBackend] 跳过送货单标签打印转发（PYTHON_BACKEND_ENABLED=false）");
        Err(AppError::biz(
            code::BIZ_PRINT_FORWARD_FAILED,
            "PYTHON_BACKEND_BASE_URL 未配置（NoopPyBackend 占位）",
        ))
    }

    async fn forward_part_print_pdf(
        &self,
        _part_id: &str,
        _query: Option<String>,
        _headers: HeaderMap,
    ) -> Result<PyBackendResponse, AppError> {
        warn!("[NoopPyBackend] 跳过零件图纸打印转发（PYTHON_BACKEND_ENABLED=false）");
        Err(AppError::biz(
            code::BIZ_PRINT_FORWARD_FAILED,
            "PYTHON_BACKEND_BASE_URL 未配置（NoopPyBackend 占位）",
        ))
    }

    async fn forward_part_print_pdf_batch(
        &self,
        _body: Value,
        _headers: HeaderMap,
    ) -> Result<PyBackendResponse, AppError> {
        warn!("[NoopPyBackend] 跳过零件图纸批量打印转发（PYTHON_BACKEND_ENABLED=false）");
        Err(AppError::biz(
            code::BIZ_PRINT_FORWARD_FAILED,
            "PYTHON_BACKEND_BASE_URL 未配置（NoopPyBackend 占位）",
        ))
    }
}

/// 工厂函数：按 `enabled` 决定走 `HttpPyBackend` 还是 `NoopPyBackend`。
///
/// - `enabled=false` → `NoopPyBackend`（本地 `cargo run` 不依赖 python）；
/// - `enabled=true` → `HttpPyBackend`（生产 / 测试，转发到 `base_url`）；
/// - `base_url` 为空 → 强制 Noop（fail-safe），避免 `reqwest` 收到 `http://` 空 host。
///
/// 2026-10-03 新增：`print_timeout_ms` 独立接成 [`HttpPyBackend::print_timeout`]，
/// 打印 4 个方法因此与 STS 档解耦。
pub fn build_py_backend(
    cfg: &crate::infra::config::PythonBackendConfig,
) -> anyhow::Result<Arc<dyn PyBackendClient>> {
    if cfg.enabled && !cfg.base_url.trim().is_empty() {
        let timeout = Duration::from_millis(cfg.timeout_ms);
        let print_timeout = Duration::from_millis(cfg.print_timeout_ms);
        Ok(Arc::new(HttpPyBackend::new(
            cfg.base_url.clone(),
            timeout,
            print_timeout,
        )?))
    } else {
        Ok(Arc::new(NoopPyBackend))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::header::{CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_TYPE};

    /// 2026-10-03 新增：响应头清洗的钉死用例。
    ///
    /// 断言三件事：hop-by-hop / `content-encoding` / `date` / `server` 被剥掉；
    /// `content-type` / `content-disposition` / `cache-control` 与自定义头保留；
    /// `content-length` 被**实际 body 长度**重算（构造一个与 body 长度不符的
    /// 上游 `content-length`，模拟 gzip/chunked 解码后的经典不一致）。
    #[test]
    fn filter_response_headers_strips_hop_by_hop_and_recomputes_content_length() {
        let mut src = HeaderMap::new();
        for (k, v) in [
            ("content-type", "application/pdf"),
            ("content-disposition", "attachment; filename=\"note.xlsx\""),
            ("cache-control", "private, max-age=600"),
            ("content-length", "999999"),
            ("content-encoding", "gzip"),
            ("transfer-encoding", "chunked"),
            ("connection", "keep-alive"),
            ("keep-alive", "timeout=5"),
            ("upgrade", "h2c"),
            ("te", "trailers"),
            ("trailer", "expires"),
            ("date", "Sat, 03 Oct 2026 00:00:00 GMT"),
            ("server", "uvicorn"),
            ("x-request-id", "req-123"),
        ] {
            src.insert(
                axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }

        let out = filter_response_headers(&src, 4096);

        // 保留项
        assert_eq!(out.get(CONTENT_TYPE).unwrap(), "application/pdf");
        assert_eq!(
            out.get(CONTENT_DISPOSITION).unwrap(),
            "attachment; filename=\"note.xlsx\"",
            "前端 parseFilename 依赖 content-disposition，必须保留"
        );
        assert_eq!(out.get("cache-control").unwrap(), "private, max-age=600");
        assert_eq!(out.get("x-request-id").unwrap(), "req-123");

        // 剥除项
        for gone in [
            "content-encoding",
            "transfer-encoding",
            "connection",
            "keep-alive",
            "upgrade",
            "te",
            "trailer",
            "date",
            "server",
        ] {
            assert!(
                out.get(gone).is_none(),
                "{gone} 是 hop-by-hop / 上游自标识，不该进 axum response"
            );
        }

        // content-length 用实际 body 长度重算（上游那个 999999 是错的）
        assert_eq!(out.get(CONTENT_LENGTH).unwrap(), "4096");
    }

    /// 请求侧过滤清单的钉死用例：鉴权头 / Cookie 必剥，`X-Forwarded-User-Id`
    /// 必留（它是 python 端唯一的身份依据，剥掉等于链路断身份）。
    #[test]
    fn filter_request_headers_drops_auth_keeps_forwarded_user_id() {
        let mut src = HeaderMap::new();
        src.insert("authorization", HeaderValue::from_static("Bearer jwt"));
        src.insert("cookie", HeaderValue::from_static("sid=1"));
        src.insert("x-forwarded-user-id", HeaderValue::from_static("42"));
        src.insert("x-request-id", HeaderValue::from_static("req-1"));
        src.insert("host", HeaderValue::from_static("example.com"));

        let out = filter_request_headers(&src);

        assert!(
            out.get("authorization").is_none(),
            "JWT 不得反向泄露给 python"
        );
        assert!(out.get("cookie").is_none(), "Cookie 不得透传给 python");
        assert!(out.get("host").is_none(), "host 由 reqwest 自行处理");
        assert_eq!(out.get("x-forwarded-user-id").unwrap(), "42");
        assert_eq!(out.get("x-request-id").unwrap(), "req-1");
    }
}

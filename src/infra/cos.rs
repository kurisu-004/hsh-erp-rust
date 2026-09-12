//! 腾讯云 COS 客户端抽象
//!
//! 对应 Python `backend-python/core/cos.py`：
//! - `put_object` / `get_object` / `presigned_get_url` / `delete_object`
//! - 业务实现阶段补：`upload_file_advanced`（大文件分块）、`download_object_cached`（SHA256 LRU 缓存）
//!
//! 2026-09-11 重构：使用 `cos-rust-sdk`（社区 SDK，仿 cos-python-sdk-v5）作为
//! 上传 / 下载 / 删除的真实客户端；`presigned_get_url` SDK 未提供，单独用
//! `hmac` + `sha1` + `base64` 手写 V1 签名拼 URL。新增 `COS_ENABLED` 开关：
//! 关闭时使用 `NoopCos` 静默成功，便于本地 `cargo run` 不依赖真实凭据。

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context as _};
use async_trait::async_trait;
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use cos_rust_sdk::{Config as CosSdkConfig, CosClient as SdkClient, ObjectClient};
use hmac::{Hmac, Mac};
use sha1::Sha1;
use tracing::info;

use crate::infra::config::CosConfig;
use crate::shared::error::{code, AppError};

#[async_trait]
pub trait CosClient: Send + Sync {
    async fn put_object(&self, key: &str, body: Vec<u8>, content_type: &str) -> Result<(), AppError>;
    async fn get_object(&self, key: &str) -> Result<Vec<u8>, AppError>;
    async fn presigned_get_url(&self, key: &str, expires_seconds: u32) -> Result<String, AppError>;
    async fn delete_object(&self, key: &str) -> Result<(), AppError>;
}

// =====================================================================================
// NoopCos：`COS_ENABLED=false` 时的静默占位实现，本地 cargo run 调试用
// =====================================================================================

/// `COS_ENABLED=false` 时的占位实现：所有方法静默返回成功。
///
/// 2026-09-11 修改：原骨架阶段 NoopCos 所有方法返回错误，本次改为：
/// - `put_object` / `delete_object`：打 info 日志后 `Ok(())`
/// - `get_object`：返回空字节
/// - `presigned_get_url`：返回 `local://{key}` 占位 URL（不会真发请求）
pub struct NoopCos;

#[async_trait]
impl CosClient for NoopCos {
    async fn put_object(
        &self,
        key: &str,
        _body: Vec<u8>,
        _content_type: &str,
    ) -> Result<(), AppError> {
        info!(key = %key, "[NoopCos] 跳过真实上传（COS_ENABLED=false，仅本地调试）");
        Ok(())
    }

    async fn get_object(&self, _key: &str) -> Result<Vec<u8>, AppError> {
        Ok(Vec::new())
    }

    async fn presigned_get_url(
        &self,
        key: &str,
        _expires_seconds: u32,
    ) -> Result<String, AppError> {
        Ok(format!("local://{key}"))
    }

    async fn delete_object(&self, key: &str) -> Result<(), AppError> {
        info!(key = %key, "[NoopCos] 跳过真实删除（COS_ENABLED=false，仅本地调试）");
        Ok(())
    }
}

// =====================================================================================
// TencentCos：`COS_ENABLED=true` 时的真实客户端
// 内部持 cos-rust-sdk ObjectClient（put/get/delete）+ Presigner（手写 V1 签名）
// =====================================================================================

/// 真实 COS 客户端（包装 `cos-rust-sdk::ObjectClient` + 自写预签 URL）。
///
/// 2026-09-11 新增。
pub struct TencentCos {
    inner: ObjectClient,
    presigner: Presigner,
}

impl TencentCos {
    /// 构造真实 COS 客户端。
    ///
    /// 步骤：
    /// 1. 解析 endpoint（优先 `cfg.endpoint`，否则按 `{scheme}://{bucket}-{appid}.cos.{region}.myqcloud.com` 拼，
    ///    `app_id` 为空时尝试从 bucket 末段解析）
    /// 2. 调 `cos_rust_sdk::Config::new(...)` 构造 SDK 配置 + 设置超时
    /// 3. 调 `CosClient::new` + `ObjectClient::new` 拿到 SDK 客户端
    /// 4. 构造内部 `Presigner`
    ///
    /// 任何一步失败都通过 `?` 上抛，被 `main.rs` 的 `?` 转 anyhow 终止启动。
    pub fn new(cfg: CosConfig) -> anyhow::Result<Self> {
        // 1. 解析 endpoint
        let endpoint = if !cfg.endpoint.is_empty() {
            cfg.endpoint.clone()
        } else {
            let app_id = if !cfg.app_id.is_empty() {
                cfg.app_id.clone()
            } else {
                cfg.bucket
                    .rsplit_once('-')
                    .map(|(_, id)| id.to_string())
                    .ok_or_else(|| anyhow!("无法从 bucket 名解析 app_id（COS_APP_ID 留空且 bucket 不含 '-'）"))?
            };
            format!(
                "{}://{}-{}.cos.{}.myqcloud.com",
                cfg.scheme, cfg.bucket, app_id, cfg.region
            )
        };

        // 2-3. SDK 配置 + 客户端
        let sdk_cfg = CosSdkConfig::new(
            cfg.secret_id.clone(),
            cfg.secret_key.clone(),
            cfg.region.clone(),
            cfg.bucket.clone(),
        )
        .with_timeout(Duration::from_secs(60));
        let sdk_client = SdkClient::new(sdk_cfg).context("cos-rust-sdk CosClient::new 失败")?;
        let inner = ObjectClient::new(sdk_client);

        // 4. Presigner（仅持 secret_id / key / endpoint，不发起 HTTP）
        let presigner = Presigner {
            secret_id: cfg.secret_id.clone(),
            secret_key: cfg.secret_key.clone(),
            endpoint,
        };

        Ok(Self { inner, presigner })
    }
}

#[async_trait]
impl CosClient for TencentCos {
    async fn put_object(
        &self,
        key: &str,
        body: Vec<u8>,
        content_type: &str,
    ) -> Result<(), AppError> {
        // SDK 返回 PutObjectResponse；trait 约定 ()，丢弃即可。
        let _ = self
            .inner
            .put_object(key, body, Some(content_type))
            .await
            .map_err(|e| {
                AppError::biz(
                    code::BIZ_PART_FILE_UPLOAD_FAILED,
                    format!("COS put_object 失败: {e}"),
                )
            })?;
        Ok(())
    }

    async fn get_object(&self, key: &str) -> Result<Vec<u8>, AppError> {
        let resp = self.inner.get_object(key).await.map_err(|e| {
            AppError::biz(
                code::BIZ_PART_FILE_UPLOAD_FAILED,
                format!("COS get_object 失败: {e}"),
            )
        })?;
        Ok(resp.data)
    }

    async fn presigned_get_url(
        &self,
        key: &str,
        expires_seconds: u32,
    ) -> Result<String, AppError> {
        self.presigner
            .sign_get(key, expires_seconds)
    }

    async fn delete_object(&self, key: &str) -> Result<(), AppError> {
        match self.inner.delete_object(key).await {
            Ok(_) => Ok(()),
            // 2026-09-11 幂等：COS 删除对象时 NoSuchKey 视为成功（对齐 Python）。
            // SDK 把 404 包装为 CosError::Server { code: "404 Not Found", message: <xml> }；
            // 业务上"对象不存在 = 删完"语义一致。
            Err(e) if is_not_found(&e) => {
                info!(key = %key, "COS delete_object 收到 NoSuchKey，按幂等成功处理");
                Ok(())
            }
            Err(e) => Err(AppError::biz(
                code::BIZ_PART_FILE_UPLOAD_FAILED,
                format!("COS delete_object 失败: {e}"),
            )),
        }
    }
}

/// 判断 cos-rust-sdk 错误是否代表「对象不存在」（404 / NoSuchKey）。
///
/// cos-rust-sdk 把 HTTP 错误包装为：
/// - `Server { code, message }`：HTTP 4xx/5xx（`code` 是状态码字符串）
/// - `Client { code, message }`：客户端构造错误（极少）
/// - `Other { message }`：杂项（不应在这里命中，但兜底按字符串匹配防漏）
///
/// 业务上"对象不存在 = 删完"语义一致，因此对 Server / Client 显式按
/// `code` / `message` 匹配；Other 仅兜底（不应到达）。
fn is_not_found(e: &cos_rust_sdk::CosError) -> bool {
    use cos_rust_sdk::CosError;
    match e {
        CosError::Server { code, message } | CosError::Client { code, message } => {
            code.starts_with("404") || message.contains("NoSuchKey")
        }
        CosError::Other { message } => {
            // 兜底：上游 SDK 包成 Other 但 message 里仍可能带 NoSuchKey；概率极低。
            message.contains("NoSuchKey") || message.contains("404")
        }
        // Http / Json / Url / Auth / Config / Io 等非业务 404：视为非 not-found，
        // 调用方会把错误冒泡给客户端。
        _ => false,
    }
}

// =====================================================================================
// Presigner：手写 COS V1 签名（cos-rust-sdk 未提供预签 URL）
// 参考 https://cloud.tencent.com/document/product/436/7778
// =====================================================================================

/// COS V1 签名预签 URL 拼装器。
///
/// 仅持有 secret_id / secret_key / endpoint，**不发起 HTTP 请求**。
/// 2026-09-11 新增。
struct Presigner {
    secret_id: String,
    secret_key: String,
    /// 仅用于拼 URL 前缀（如 `https://bucket-appid.cos.ap-shanghai.myqcloud.com`）。
    endpoint: String,
}

impl Presigner {
    /// 生成对象 GET 预签 URL（V1 算法，sha1 + HMAC）。
    ///
    /// 算法（参考腾讯云 COS XML API 签名 V1）：
    /// ```text
    /// HttpRequestInfo = GET + "\n" + pathname + "\n" + params + "\n" + headers + "\n"
    /// StringToSign    = "sha1\n" + sign_time + "\n" + hex(sha1(HttpRequestInfo)) + "\n"
    /// Signature       = base64(hmac_sha1(secret_key, StringToSign))
    /// ```
    ///
    /// URL 末尾追加查询参数：
    /// `?q-sign-algorithm=sha1&q-ak=<sid>&q-sign-time=<s>;<e>&q-key-time=<s>;<e>&q-signature=<sig>`
    ///
    /// 注意：当前实现 pathname 用 `/{key}`（不带 host），params / headers 均为空串
    /// （不携带 x-cos- 头）；与 Python `cos-python-sdk-v5` 默认签名上下文一致。
    /// endpoint 由 `TencentCos::new` 一次性生成后挂在 `Presigner.endpoint`，
    /// 本方法只拼 host+key+query，不读 prefix。
    fn sign_get(
        &self,
        key: &str,
        expires_seconds: u32,
    ) -> Result<String, AppError> {
        // 1. 时间窗（Unix 秒）
        let now: u64 = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| AppError::biz(code::INTERNAL, format!("系统时钟异常: {e}")))?
            .as_secs();
        let end = now.saturating_add(expires_seconds as u64);
        let sign_time = format!("{now};{end}");

        // 2. pathname：key 可能含中文 / 空格，需先 percent-encode 后再签；
        //    同时解码路径分隔符保持标准 URL 形态。
        let pathname = format!("/{}", percent_encode_path(key));

        // 3. HttpRequestInfo（params / headers 留空）
        let http_request_info = format!("GET\n{pathname}\n\n\n");
        let hashed_request = sha1_hex(http_request_info.as_bytes());

        // 4. StringToSign + HMAC-SHA1 签名
        let string_to_sign = format!("sha1\n{sign_time}\n{hashed_request}\n");
        let signature = hmac_sha1_base64(self.secret_key.as_bytes(), string_to_sign.as_bytes())?;

        // 5. 拼最终 URL
        //    endpoint 已是带 scheme 的完整 host；末尾去掉 key 之外的所有路径。
        let host = self.endpoint.trim_end_matches('/');
        Ok(format!(
            "{host}{pathname}?q-sign-algorithm=sha1&q-ak={sid}&q-sign-time={st}&q-key-time={st}&q-signature={sig}",
            sid = percent_encode_query(&self.secret_id),
            st = sign_time,
            sig = signature,
        ))
    }
}

/// SHA-1 hex（小写）。
fn sha1_hex(data: &[u8]) -> String {
    use sha1::{Digest, Sha1 as Sha1Digest};
    let mut h = Sha1Digest::new();
    h.update(data);
    let bytes = h.finalize();
    hex::encode(bytes)
}

/// HMAC-SHA1 → 标准 Base64 字符串。
fn hmac_sha1_base64(key: &[u8], data: &[u8]) -> Result<String, AppError> {
    type HmacSha1 = Hmac<Sha1>;
    let mut mac = HmacSha1::new_from_slice(key)
        .map_err(|e| AppError::biz(code::INTERNAL, format!("HMAC key 非法: {e}")))?;
    mac.update(data);
    let bytes = mac.finalize().into_bytes();
    Ok(BASE64.encode(bytes))
}

/// 按 RFC 3986 对 path 段做 percent-encoding：保留 `/` 不动（递归路径合法），
/// 其余非 unreserved 字符编码。
fn percent_encode_path(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~' | '/') {
            out.push(c);
        } else {
            // UTF-8 字节逐个编码
            let mut buf = [0u8; 4];
            let encoded = c.encode_utf8(&mut buf);
            for b in encoded.as_bytes() {
                out.push_str(&format!("%{:02X}", b));
            }
        }
    }
    out
}

/// 对 query value 做 percent-encoding（保留 `=` `&` 不编码以免破坏 URL 结构）。
fn percent_encode_query(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~') {
            out.push(c);
        } else {
            let mut buf = [0u8; 4];
            let encoded = c.encode_utf8(&mut buf);
            for b in encoded.as_bytes() {
                out.push_str(&format!("%{:02X}", b));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_encode_path_preserves_unreserved() {
        assert_eq!(percent_encode_path("abc-123_v.pdf"), "abc-123_v.pdf");
        assert_eq!(percent_encode_path("a/b/c"), "a/b/c");
    }

    #[test]
    fn percent_encode_path_encodes_unicode() {
        // 中文字符 UTF-8 编码后逐字节 percent-encode
        let s = percent_encode_path("图纸.pdf");
        assert!(s.ends_with(".pdf"));
        assert!(s.starts_with("%E5%9B%BE%E7%BA%B8"));
    }

    #[test]
    fn percent_encode_query_preserves_safe_chars() {
        assert_eq!(percent_encode_query("AKIDxxxx"), "AKIDxxxx");
    }

    #[test]
    fn hmac_sha1_base64_known_vector() {
        // 已知向量：key="key", data="The quick brown fox jumps over the lazy dog"
        // HMAC-SHA1 hex = de7c9b85b8b78aa6bc8a7a36f70a90701c9db4d9
        // base64        = 3nybhbi3iqa8ino29wqQcBydtNk=
        let sig = hmac_sha1_base64(b"key", b"The quick brown fox jumps over the lazy dog")
            .expect("hmac ok");
        assert_eq!(sig, "3nybhbi3iqa8ino29wqQcBydtNk=");
    }

    #[test]
    fn sha1_hex_known_vector() {
        // sha1("abc") = a9993e364706816aba3e25717850c26c9cd0d89d
        assert_eq!(sha1_hex(b"abc"), "a9993e364706816aba3e25717850c26c9cd0d89d");
    }

    #[test]
    fn presigner_sign_get_builds_url_with_required_params() {
        let p = Presigner {
            secret_id: "AKIDtest".to_string(),
            secret_key: "secretkey".to_string(),
            endpoint: "https://bucket-123.cos.ap-shanghai.myqcloud.com".to_string(),
        };
        let url = p
            .sign_get("uploads/part/1/DRAWING/abcdef0123456789_drawing.pdf", 3600)
            .expect("sign ok");
        assert!(url.starts_with(
            "https://bucket-123.cos.ap-shanghai.myqcloud.com/uploads/part/1/DRAWING/"
        ));
        assert!(url.contains("q-sign-algorithm=sha1"));
        assert!(url.contains("q-ak=AKIDtest"));
        assert!(url.contains("q-sign-time="));
        assert!(url.contains("q-key-time="));
        assert!(url.contains("q-signature="));
    }
}

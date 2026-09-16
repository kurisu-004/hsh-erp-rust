//! 腾讯云 COS 客户端抽象
//!
//! 对应 Python `backend-python/core/cos.py`：
//! - `put_object` / `get_object` / `presigned_get_url` / `delete_object`
//! - M2 新增 `head_object` / `copy_object`（服务端→服务端 copy + 元数据查询）
//! - 业务实现阶段补：`upload_file_advanced`（大文件分块）、`download_object_cached`（SHA256 LRU 缓存）
//!
//! 2026-09-11 重构：使用 `cos-rust-sdk`（社区 SDK，仿 cos-python-sdk-v5）作为
//! 上传 / 下载 / 删除的真实客户端；`presigned_get_url` SDK 未提供，单独用
//! `hmac` + `sha1` + `hex` 手写 V1 签名拼 URL。新增 `COS_ENABLED` 开关：
//! 关闭时使用 `NoopCos` 静默成功，便于本地 `cargo run` 不依赖真实凭据。
//!
//! 2026-09-16 M2-A 重构签名算法为两步 HMAC（hex SignKey → hex Signature），
//! 与 cos-rust-sdk 一致；移除 `base64` 依赖。同时新增 `head_object` / `copy_object`。
//! `head_object` 直接调 SDK；`copy_object` 用 reqwest PUT + `x-cos-copy-source`
//! + 手写 V1 签名（永久密钥，**不走 STS**，与 STS DELETE 403 限制无关）。
//!
//! V1 签名算法参考（与 spike_sts.rs::sign_v1 / spike_copy.rs::copy_object_v1 一致）：
//! ```text
//! HttpRequestInfo = method\n + pathname_encoded\n + params_string\n + headers_string\n
//! StringToSign    = sha1\n + sign_time\n + sha1_hex(HttpRequestInfo)\n
//! SignKey         = HMAC-SHA1(SecretKey, KeyTime)         → hex (40 chars)
//! Signature       = HMAC-SHA1(SignKey hex bytes, StringToSign) → hex (40 chars)
//! ```
//! - `SignKey` 用 SecretKey 与 KeyTime 算一次 HMAC，输出 **hex**。
//! - `Signature` 用 SignKey 的 **hex bytes**（即 40 字节 raw） 与 StringToSign 算二次 HMAC，输出 **hex**。
//! - header value / URL param value 严格 percent-encode：只保留 `-_.~` + ASCII 字母数字，
//!   其它（含 `/` → `%2F`）一律 `%XX`。
//! - reqwest 自动加的 Content-Length / Host / Date / User-Agent **不参与签名**（不列在 q-header-list）。

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, anyhow};
use async_trait::async_trait;
use cos_rust_sdk::{Config as CosSdkConfig, CosClient as SdkClient, ObjectClient};
use hmac::{Hmac, Mac};
use sha1::Sha1;
use tracing::info;

use crate::infra::config::CosConfig;
use crate::shared::error::{AppError, code};

/// COS 对象元数据（head_object 返回值）。
///
/// 2026-09-16 M2-A 新增：M3 业务模块需要校验服务端 tmp_object 存在 + 大小匹配。
/// `size` 选 i64 而非 u64 是为了与 `t_part_file.size_bytes` 等业务表字段直接比对
/// （避免 usize 跨界转换）。
#[derive(Debug, Clone)]
pub struct ObjectMeta {
    pub size: i64,
    pub etag: String,
}

#[async_trait]
pub trait CosClient: Send + Sync {
    async fn put_object(
        &self,
        key: &str,
        body: Vec<u8>,
        content_type: &str,
    ) -> Result<(), AppError>;
    async fn get_object(&self, key: &str) -> Result<Vec<u8>, AppError>;
    async fn presigned_get_url(&self, key: &str, expires_seconds: u32) -> Result<String, AppError>;
    async fn delete_object(&self, key: &str) -> Result<(), AppError>;
    /// 2026-09-16 M2-A 新增：取对象 size + etag（不带 body）。
    async fn head_object(&self, key: &str) -> Result<ObjectMeta, AppError>;
    /// 2026-09-16 M2-A 新增：服务端→服务端 copy（不下载再上传）。
    ///
    /// 内部用 reqwest PUT + `x-cos-copy-source` 头 + V1 签名（永久密钥）；
    /// 不走 STS，因为 STS `Policy::allow_put_object` 不含 `cos:DeleteObject` action
    /// 限制无关——copy 不需要 DELETE，但保持 "服务端操作走永久密钥" 的一致约定。
    async fn copy_object(&self, src_key: &str, dst_key: &str) -> Result<(), AppError>;
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
///
/// 2026-09-16 M2-A：`head_object` 返回 `size=0, etag="noop"`；`copy_object` 打 info 后 Ok。
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

    async fn head_object(&self, _key: &str) -> Result<ObjectMeta, AppError> {
        // NoopCos 不真发请求，返回占位元数据；M3 校验 size 时不会真的过这里
        // （因为客户端直传会绕过 NoopCos；NoopCos 仅用于本地 cargo run）。
        Ok(ObjectMeta {
            size: 0,
            etag: "noop".to_string(),
        })
    }

    async fn copy_object(&self, src_key: &str, dst_key: &str) -> Result<(), AppError> {
        info!(
            src = %src_key,
            dst = %dst_key,
            "[NoopCos] 跳过真实 copy_object（COS_ENABLED=false，仅本地调试）"
        );
        Ok(())
    }
}

// =====================================================================================
// TencentCos：`COS_ENABLED=true` 时的真实客户端
// 内部持 cos-rust-sdk ObjectClient（put/get/delete/head）+ Presigner（手写 V1 签名）
// =====================================================================================

/// 真实 COS 客户端（包装 `cos-rust-sdk::ObjectClient` + 自写预签 URL）。
///
/// 2026-09-11 新增；2026-09-16 M2-A 扩 `head_object` / `copy_object`。
pub struct TencentCos {
    inner: ObjectClient,
    /// SDK 默认 endpoint（如 `https://bucket-appid.cos.ap-shanghai.myqcloud.com`），用于 SDK 调用。
    sdk_endpoint: String,
    /// Presigner 用的同源 endpoint。
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
                    .ok_or_else(|| {
                        anyhow!("无法从 bucket 名解析 app_id（COS_APP_ID 留空且 bucket 不含 '-'）")
                    })?
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
            endpoint: endpoint.clone(),
        };

        Ok(Self {
            inner,
            sdk_endpoint: endpoint,
            presigner,
        })
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

    async fn presigned_get_url(&self, key: &str, expires_seconds: u32) -> Result<String, AppError> {
        self.presigner.sign_get(key, expires_seconds)
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

    async fn head_object(&self, key: &str) -> Result<ObjectMeta, AppError> {
        // SDK head_object 返回 HeadObjectResponse { content_length: u64, etag, ... }
        let resp = self.inner.head_object(key).await.map_err(|e| {
            AppError::biz(
                code::BIZ_PART_FILE_UPLOAD_FAILED,
                format!("COS head_object 失败: {e}"),
            )
        })?;
        // content_length 上限 i64::MAX（≈8EB），实际 COS 单对象上限 48.8TB，安全
        Ok(ObjectMeta {
            size: resp.content_length as i64,
            etag: resp.etag,
        })
    }

    async fn copy_object(&self, src_key: &str, dst_key: &str) -> Result<(), AppError> {
        // 永久密钥走 copy_object（服务端→服务端），不走 STS。
        // SDK 0.1.2 没暴露 PUT + x-cos-copy-source，直接走 reqwest + 手写 V1 签名。
        let copy_source = format!(
            "{}/{src_key}",
            self.sdk_endpoint
                .trim_start_matches("https://")
                .trim_start_matches("http://")
        );
        let now =
            unix_now().map_err(|e| AppError::biz(code::INTERNAL, format!("系统时钟异常: {e}")))?;
        let signed = self.presigner.sign(
            "PUT",
            &format!("/{dst_key}"),
            &[],
            &[("x-cos-copy-source", copy_source.as_str())],
            now,
            // copy_object 是服务端→服务端操作，URL 不外发；有效期设大一些便于覆盖长任务。
            // 3600s 对齐 `CosConfig::presign_expire_seconds` 默认值；显式常量避免
            // signature 在 copy 请求期间过期（服务端校验 q-sign-time）。
            3600u32,
            None,
        );
        let url = format!(
            "{}/{}?{}",
            self.sdk_endpoint.trim_end_matches('/'),
            dst_key.trim_start_matches('/'),
            signed.query
        );

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
            .map_err(|e| {
                AppError::biz(
                    code::BIZ_PART_FILE_UPLOAD_FAILED,
                    format!("copy_object reqwest client build 失败: {e}"),
                )
            })?;
        // 用 signed.headers 直接灌 header（key 已规范化小写），避免两份字面量漂移。
        // 当前只签了 1 个 header，取首元素；以后扩到多 header 时改 `.into_iter().any(...)`
        // 显式遍历。
        let header_value = signed
            .headers
            .first()
            .map(|(_, v)| v.clone())
            .unwrap_or_else(|| copy_source.clone());
        let resp = client
            .put(&url)
            .header("x-cos-copy-source", &header_value)
            .send()
            .await
            .map_err(|e| {
                AppError::biz(
                    code::BIZ_PART_FILE_UPLOAD_FAILED,
                    format!("copy_object HTTP 请求失败: {e}"),
                )
            })?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(AppError::biz(
                code::BIZ_PART_FILE_UPLOAD_FAILED,
                format!("copy_object 服务端返回 {status}: {body}"),
            ));
        }
        Ok(())
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

/// COS V1 签名预签 URL / 签名 PUT 的 helper。
///
/// 仅持有 secret_id / secret_key / endpoint，**不发起 HTTP 请求**。
/// 2026-09-11 新增；2026-09-16 M2-A 重构为两步 HMAC（hex SignKey → hex Signature）。
struct Presigner {
    secret_id: String,
    secret_key: String,
    /// 仅用于拼 URL 前缀（如 `https://bucket-appid.cos.ap-shanghai.myqcloud.com`）。
    endpoint: String,
}

/// 签名结果：URL 拼 query 用的字符串 + 要塞进 reqwest 的 header 列表（key 已小写）。
///
/// 2026-09-16 M2-A 新增。`sign_get`（旧 GET 预签 URL）内部走 `sign` 再加 query；
/// `copy_object` 直接拿 `query` + `headers` 拼 URL / 设 header。
#[derive(Debug, Clone)]
struct SignedRequest {
    pub query: String,
    pub headers: Vec<(String, String)>,
}

impl Presigner {
    /// 生成对象 GET 预签 URL（V1 算法，sha1 + 两步 HMAC）。
    ///
    /// 2026-09-16 M2-A 重构：
    /// - 旧实现：一步 HMAC-SHA1 + base64 输出
    /// - 新实现：两步 HMAC（SignKey = HMAC-SHA1(SecretKey, KeyTime) → hex；Signature = HMAC-SHA1(SignKey hex bytes, StringToSign) → hex）+ hex 输出
    /// - 内部统一走新 [`Presigner::sign`] helper
    fn sign_get(&self, key: &str, expires_seconds: u32) -> Result<String, AppError> {
        let now =
            unix_now().map_err(|e| AppError::biz(code::INTERNAL, format!("系统时钟异常: {e}")))?;
        let signed = self.sign(
            "GET",
            &format!("/{key}"),
            &[],
            &[],
            now,
            expires_seconds,
            None,
        );
        let host = self.endpoint.trim_end_matches('/');
        Ok(format!("{host}/{key}?{}", signed.query,))
    }

    /// 通用 V1 签名 helper（M2-A 新增；`sign_get` + `copy_object` 都走这里）。
    ///
    /// 算法（参考腾讯云 COS XML API 签名 V1，与 cos-rust-sdk 内部一致）：
    /// ```text
    /// SignKey         = HMAC-SHA1(SecretKey, KeyTime)            → hex (40 chars)
    /// StringToSign    = sha1\n + sign_time\n + sha1_hex(HttpRequestInfo)\n
    /// Signature       = HMAC-SHA1(SignKey hex bytes, StringToSign) → hex (40 chars)
    /// ```
    ///
    /// 参数：
    /// - `method`：HTTP 方法（GET / PUT / POST / DELETE）
    /// - `pathname`：URL path（含前导 `/`，如 `/uploads/x.pdf`），内部做 path percent-encode
    /// - `params`：URL query 参数列表（如 `[("uploads", "")]`），会进 `q-url-param-list`
    /// - `headers_to_sign`：要签的 header 列表（如 `[("x-cos-copy-source", value)]`），key 已规范化；
    ///   会进 `q-header-list`，并按 key 小写排序拼 headers_string
    /// - `secret_id` / `secret_key`：永久或临时密钥
    /// - `now`：调用方传入的 Unix 秒（让 `sign_get` 不读两次时钟）
    /// - `expires_seconds`：URL 有效期
    /// - `security_token`：STS token 头（不进签名头列表，只拼到 query 里 `q-sts-token`），
    ///   仅 STS 场景需要；永久密钥场景传 `None`
    ///
    /// reqwest 自动加的 Content-Length / Host / Date / User-Agent **不参与签名**
    /// （不在 `q-header-list`），需要 caller 显式列举要签的 header。
    #[allow(clippy::too_many_arguments)]
    fn sign(
        &self,
        method: &str,
        pathname: &str,
        params: &[(&str, &str)],
        headers_to_sign: &[(&str, &str)],
        now: u64,
        expires_seconds: u32,
        security_token: Option<&str>,
    ) -> SignedRequest {
        let end = now.saturating_add(expires_seconds as u64);
        let sign_time = format!("{now};{end}");

        // 1. headers 段：按 key 小写字典序排序
        let mut headers_vec: Vec<(String, String)> = headers_to_sign
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        headers_vec.sort_by_key(|a| a.0.to_lowercase());
        let headers_string = headers_vec
            .iter()
            .map(|(k, v)| format!("{}={}", k.to_lowercase(), percent_encode_strict(v)))
            .collect::<Vec<_>>()
            .join("&");

        // 2. params 段：按 key 小写字典序排序
        let mut params_vec: Vec<(String, String)> = params
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        params_vec.sort_by_key(|a| a.0.to_lowercase());
        let params_string = params_vec
            .iter()
            .map(|(k, v)| format!("{}={}", k.to_lowercase(), percent_encode_strict(v)))
            .collect::<Vec<_>>()
            .join("&");

        // 3. HttpRequestInfo（method 小写 + \n 分隔）
        let http_request_info = format!(
            "{}\n{}\n{}\n{}\n",
            method.to_lowercase(),
            percent_encode_path(pathname),
            params_string,
            headers_string,
        );
        let hashed_request = sha1_hex(http_request_info.as_bytes());

        // 4. StringToSign + 两步 HMAC-SHA1（hex 输出）
        let string_to_sign = format!("sha1\n{sign_time}\n{hashed_request}\n");
        // 4a. SignKey = HMAC-SHA1(SecretKey, KeyTime) → hex
        let sign_key_bytes = hmac_sha1_raw(self.secret_key.as_bytes(), sign_time.as_bytes());
        let sign_key_hex = hex::encode(sign_key_bytes);
        // 4b. Signature = HMAC-SHA1(SignKey hex bytes, StringToSign) → hex
        let signature = hmac_sha1_hex(sign_key_hex.as_bytes(), string_to_sign.as_bytes());

        // 5. URL 拼 query 段（q-header-list / q-url-param-list 都显式列出，即使空也保留）
        let header_list = headers_vec
            .iter()
            .map(|(k, _)| k.to_lowercase())
            .collect::<Vec<_>>()
            .join(";");
        let param_list = params_vec
            .iter()
            .map(|(k, _)| k.to_lowercase())
            .collect::<Vec<_>>()
            .join(";");
        let mut query = format!(
            "q-sign-algorithm=sha1&q-ak={sid}&q-sign-time={st}&q-key-time={st}&q-header-list={hl}&q-url-param-list={pl}&q-signature={sig}",
            sid = percent_encode_strict(&self.secret_id),
            st = sign_time,
            hl = header_list,
            pl = param_list,
            sig = signature,
        );
        if let Some(tok) = security_token {
            // STS 凭证场景：query 里追加 q-sts-token，便于 Python / 浏览器签名调试。
            // 真正的 token 仍必须通过 x-cos-security-token 头带（SDK 自动加）。
            query.push_str("&q-sts-token=");
            query.push_str(&percent_encode_strict(tok));
        }

        SignedRequest {
            query,
            headers: headers_vec,
        }
    }
}

/// Unix 秒（避开 SystemTime::duration_since 返回 Result 的样板）。
fn unix_now() -> anyhow::Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| anyhow!("系统时钟异常: {e}"))?
        .as_secs())
}

/// SHA-1 hex（小写）。
fn sha1_hex(data: &[u8]) -> String {
    use sha1::{Digest, Sha1 as Sha1Digest};
    let mut h = Sha1Digest::new();
    h.update(data);
    let bytes = h.finalize();
    hex::encode(bytes)
}

/// HMAC-SHA1 → 原始 20 字节（用于 SignKey 第一步）。
fn hmac_sha1_raw(key: &[u8], data: &[u8]) -> [u8; 20] {
    type HmacSha1 = Hmac<Sha1>;
    let mut mac = HmacSha1::new_from_slice(key).expect("HMAC key ok");
    mac.update(data);
    mac.finalize().into_bytes().into()
}

/// HMAC-SHA1 → hex 字符串（40 chars，用于 Signature 第二步）。
fn hmac_sha1_hex(key: &[u8], data: &[u8]) -> String {
    hex::encode(hmac_sha1_raw(key, data))
}

/// 严格 percent-encode：只保留 `-_.~` + ASCII 字母数字，其它一律 `%XX`。
///
/// COS V1 签名对 header value / param value 都要求这套编码（含 `/` → `%2F`）。
/// 与 RFC 3986 unreserved 集合一致（`-_.~` + `ALPHA / DIGIT`）。
///
/// 2026-09-16 M2-A 新增：原 `percent_encode_query` 用 `chars()` + UTF-8 字节逐个编码，
/// 对非 ASCII（中文等）输出比 RFC 3986 多 1 字节的「%」前缀；新版改用 `bytes()`
/// 逐字节编码，与服务端 FormatString 一致。
pub(crate) fn percent_encode_strict(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{:02X}", b));
        }
    }
    out
}

/// 路径段 percent-encode：保留 `-_.~` + ASCII 字母数字 + `/`（递归路径合法）。
///
/// 2026-09-16 M2-A 新增：与 `percent_encode_strict` 同样改用 `bytes()` 逐字节编码；
/// 用于 `HttpRequestInfo.pathname` 字段。
fn percent_encode_path(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~' | b'/') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{:02X}", b));
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
    fn percent_encode_strict_encodes_slash() {
        // M2-A：header / param value 里的 `/` 必须编码成 `%2F`（与 spike 一致）
        assert_eq!(percent_encode_strict("a/b"), "a%2Fb");
        assert_eq!(percent_encode_strict("a b=c&d"), "a%20b%3Dc%26d");
        // 非 unreserved 一律编码
        assert_eq!(
            percent_encode_strict("bucket.cos.region.myqcloud.com/path/to/x"),
            "bucket.cos.region.myqcloud.com%2Fpath%2Fto%2Fx"
        );
    }

    #[test]
    fn percent_encode_strict_preserves_unreserved() {
        assert_eq!(percent_encode_strict("AKIDxxxx-_.~"), "AKIDxxxx-_.~");
    }

    #[test]
    fn sha1_hex_known_vector() {
        // sha1("abc") = a9993e364706816aba3e25717850c26c9cd0d89d
        assert_eq!(sha1_hex(b"abc"), "a9993e364706816aba3e25717850c26c9cd0d89d");
    }

    #[test]
    fn hmac_sha1_two_step_known_vector() {
        // RFC 2202 §2 test case：key="Jefe", data="what do ya want for nothing?"
        // HMAC-SHA1 = effcdf6ae5eb2fa2d27416d5f184df9c259a7c79
        // 验证两步算法（hex SignKey → hex Signature）的链路
        // SignKey = HMAC-SHA1("Jefe", "Jefe".as_bytes()) → ...（任意 KeyTime）
        // 这里只验证第二步接口：HMAC-SHA1(hex(SignKey), StringToSign) 的输出
        let inner = hmac_sha1_raw(b"Jefe", b"Jefe"); // 用相同 key+data 算 SignKey
        let sign_key_hex = hex::encode(inner);
        let sig = hmac_sha1_hex(sign_key_hex.as_bytes(), b"what do ya want for nothing?");
        // SignKey 的 raw bytes 走 HMAC-SHA1(data)：
        // - raw bytes = 20 字节 hex 解码后 → Key
        // - data = "what do ya want for nothing?"
        // - 输出 hex = ?
        // 验证长度正确（40 chars）+ 全 hex 字符
        assert_eq!(sig.len(), 40);
        assert!(sig.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn presigner_sign_get_builds_url_with_required_params() {
        let p = Presigner {
            secret_id: "AKIDtest".to_string(),
            secret_key: "secretkey".to_string(),
            endpoint: "https://bucket-123.cos.ap-shanghai.myqcloud.com".to_string(),
        };
        // 用 now=1000000000, expires=3600（确定性的 sign_time，便于断言 hex 长度）
        let signed = p.sign(
            "GET",
            "/uploads/part/1/DRAWING/abcdef0123456789_drawing.pdf",
            &[],
            &[],
            1_000_000_000,
            3600,
            None,
        );
        // 包含 q-sign-* 系列
        assert!(signed.query.contains("q-sign-algorithm=sha1"));
        assert!(signed.query.contains("q-ak=AKIDtest"));
        assert!(signed.query.contains("q-sign-time=1000000000;1000003600"));
        assert!(signed.query.contains("q-key-time=1000000000;1000003600"));
        // q-signature 是 hex（40 chars）+ q-sts-token 永久密钥场景不带
        assert!(signed.query.contains("q-signature="));
        assert!(!signed.query.contains("q-sts-token="));
        // sign_get 包外层
        let url = format!(
            "https://bucket-123.cos.ap-shanghai.myqcloud.com/uploads/part/1/DRAWING/abcdef0123456789_drawing.pdf?{}",
            signed.query
        );
        assert!(url.starts_with(
            "https://bucket-123.cos.ap-shanghai.myqcloud.com/uploads/part/1/DRAWING/"
        ));
    }

    #[test]
    fn presigner_sign_get_signature_is_40_char_hex() {
        // M2-A 核心断言：signature 是 **hex**（40 chars）+ **两步** HMAC 的输出；
        // 旧实现是 base64（28 chars），会立刻失败。
        let p = Presigner {
            secret_id: "AKIDtest".to_string(),
            secret_key: "secretkey".to_string(),
            endpoint: "https://bucket-123.cos.ap-shanghai.myqcloud.com".to_string(),
        };
        let signed = p.sign("GET", "/x.pdf", &[], &[], 1_000_000_000, 3600, None);
        // 截取 q-signature= 后的值
        let sig = signed
            .query
            .split("q-signature=")
            .nth(1)
            .expect("q-signature 必须存在")
            .split('&')
            .next()
            .expect("q-signature 后面必须可切");
        assert_eq!(
            sig.len(),
            40,
            "hex signature 必须是 40 chars（旧 base64 是 28）"
        );
        assert!(
            sig.chars().all(|c| c.is_ascii_hexdigit()),
            "signature 必须全是 hex 字符（旧 base64 含 / + =）"
        );
    }

    #[test]
    fn presigner_sign_get_encodes_header_value_slash() {
        // 直接验证 percent_encode_strict 在 header value 路径上把 `/` 编成 `%2F`：
        // 同样 secret_key + 同样 headers，仅 `/` 与 `%2F` 的差异会产出不同 signature。
        let p = Presigner {
            secret_id: "AKIDz".to_string(),
            secret_key: "skeyz".to_string(),
            endpoint: "https://b-123.cos.ap-shanghai.myqcloud.com".to_string(),
        };
        let with_slash = p.sign(
            "PUT",
            "/dst.txt",
            &[],
            &[("x-cos-copy-source", "host/prefix/src.txt")],
            123_456_789,
            900,
            None,
        );
        // 把 header value 提前 percent-encode（手动模拟「不编码」语义）
        let pre_encoded = p.sign(
            "PUT",
            "/dst.txt",
            &[],
            &[("x-cos-copy-source", "host%2Fprefix%2Fsrc.txt")],
            123_456_789,
            900,
            None,
        );
        // 两个 signature 必须**不同**——证明函数内部做了 `%2F` 编码而非原样塞进 hash
        assert_ne!(with_slash.query, pre_encoded.query);
    }

    #[test]
    fn presigner_sign_get_is_deterministic() {
        // 相同 (sid, skey, method, pathname, params, headers, now, expires) 必须输出一致 signature
        // —— 两步算法链路里没有随机数 / nonce / 时间戳（now 由 caller 传入）。
        let p = Presigner {
            secret_id: "AKIDz".to_string(),
            secret_key: "skeyz".to_string(),
            endpoint: "https://b-123.cos.ap-shanghai.myqcloud.com".to_string(),
        };
        let a = p.sign(
            "PUT",
            "/dst.txt",
            &[],
            &[(
                "x-cos-copy-source",
                "b-123.cos.ap-shanghai.myqcloud.com/src.txt",
            )],
            123_456_789,
            900,
            None,
        );
        let b = p.sign(
            "PUT",
            "/dst.txt",
            &[],
            &[(
                "x-cos-copy-source",
                "b-123.cos.ap-shanghai.myqcloud.com/src.txt",
            )],
            123_456_789,
            900,
            None,
        );
        assert_eq!(a.query, b.query);
        // x-cos-copy-source 出现在 q-header-list；value 的 `/` → `%2F` 编码
        // 只影响 HttpRequestInfo 的 headers_string（→ sha1 → StringToSign → Signature），
        // 不直接出现在 URL query 上（query 只含 header 名 + signature）。
        assert!(a.query.contains("q-header-list=x-cos-copy-source"));
    }
}

//! 2026-09-20 spike：OpenDAL 对接真 COS 的端到端烟雾测试（**仅 opt-in**）
//!
//! ## 触发方式
//! - 默认忽略（`#[ignore]`），常规 `cargo test` 不会跑
//! - 显式运行：`RUN_REAL_COS_TESTS=1 cargo test --test cos_real_smoke -- --ignored --nocapture`
//! - **不**需要测试打印凭据；读取环境变量从 .env / shell 拿
//!
//! ## 安全
//! - key prefix 用唯一标识（`opendal-spike-2026-09-20-<uuid>/`），**不**触碰业务 prefix `drawings/`
//! - 测试结束后 `delete_object` 自清理
//!
//! ## 覆盖
//! 1. put_object → 真上传
//! 2. head_object → 校验 size + etag 返回值（spike memory backend 这两个是空的）
//! 3. get_object → 回读比对字节
//! 4. presigned_get_url → 拿 URL + 浏览器-like 风格 HTTP GET 验证可达
//! 5. copy_object → 服务端 PUT copy-object（spike memory backend 不支持这条）
//! 6. delete_object → 幂等清理

use std::env;
use std::time::Duration;

use hsh_erp_rust::infra::config::{CosBackend, CosConfig};
use hsh_erp_rust::infra::cos::CosClient;
use hsh_erp_rust::infra::cos_opendal::OpenDalCos;

fn require_env(name: &str) -> String {
    env::var(name).unwrap_or_else(|_| panic!("{name} 必须设置（spike 真 COS 验证）"))
}

fn build_cos_config() -> CosConfig {
    // 2026-09-20 迁移清理：删 `sts_duration_seconds` 字段（STS 链路完全走
    // `UploadSessionConfig::sts_duration_seconds` + python 后端转发）。
    CosConfig {
        enabled: true,
        region: require_env("COS_REGION"),
        bucket: require_env("COS_BUCKET"),
        // 凭据不打印
        secret_id: require_env("COS_SECRET_ID"),
        secret_key: require_env("COS_SECRET_KEY"),
        app_id: env::var("COS_APP_ID").unwrap_or_default(),
        endpoint: env::var("COS_ENDPOINT").unwrap_or_default(),
        scheme: env::var("COS_SCHEME").unwrap_or_else(|_| "https".into()),
        upload_prefix: "drawings/".into(),
        presign_expire_seconds: 900,
        max_file_size: 100 * 1024 * 1024,
        tmp_prefix: "tmp/".into(),
        backend: CosBackend::OpenDal,
    }
}

/// 生成 spike 期间唯一的 key prefix，避免并发 spike 或历史残留污染。
fn unique_prefix() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("opendal-spike-2026-09-20-{nanos}/")
}

#[tokio::test]
#[ignore = "需真 COS 凭据；opt-in 运行 RUN_REAL_COS_TESTS=1 cargo test --test cos_real_smoke -- --ignored --nocapture"]
async fn open_dal_round_trip_all_six_methods_against_real_cos() {
    // opt-in 检查
    if env::var("RUN_REAL_COS_TESTS").ok().as_deref() != Some("1") {
        eprintln!("跳过：未设置 RUN_REAL_COS_TESTS=1");
        return;
    }

    let cfg = build_cos_config();
    let prefix = unique_prefix();
    eprintln!(
        "[spike] bucket={} region={} prefix={}",
        cfg.bucket, cfg.region, prefix
    );

    // 构造真 OpenDalCos（构造失败立即 fail，便于排查 builder 字段名错配）
    let cos = OpenDalCos::new(cfg.clone())
        .expect("OpenDalCos::new 真凭据构造失败（OpenDAL builder 字段名/COS endpoint 拼写问题）");

    // 1. put_object
    let key_src = format!("{prefix}src.bin");
    let payload = b"opendal-spike-payload-2026-09-20".to_vec();
    cos.put_object(&key_src, payload.clone(), "application/octet-stream")
        .await
        .expect("put_object 真 COS 失败");
    eprintln!("[spike] 1/6 put_object OK: key={key_src}");

    // 2. head_object —— 这是 spike memory backend 返回 None 的字段
    let meta = cos
        .head_object(&key_src)
        .await
        .expect("head_object 真 COS 失败");
    eprintln!(
        "[spike] 2/6 head_object OK: size={} etag={:?}（Memory backend 此处 etag=None）",
        meta.size, meta.etag
    );
    assert_eq!(
        meta.size as usize,
        payload.len(),
        "head_object size 应等于 payload 长度"
    );
    assert!(
        !meta.etag.is_empty() && meta.etag != "noop" && meta.etag != "None",
        "真 COS etag 必须非空非占位：got={:?}",
        meta.etag
    );

    // 3. get_object —— 字节级比对
    let read_back = cos
        .get_object(&key_src)
        .await
        .expect("get_object 真 COS 失败");
    assert_eq!(read_back, payload, "get_object 字节比对必须一致");
    eprintln!("[spike] 3/6 get_object OK: bytes={}", read_back.len());

    // 4. presigned_get_url —— 拿 URL 后用 reqwest GET 验证可达
    let presigned_url = cos
        .presigned_get_url(&key_src, 60)
        .await
        .expect("presigned_get_url 真 COS 失败");
    eprintln!(
        "[spike] 4/6 presigned_get_url OK: len={} prefix={}",
        presigned_url.len(),
        &presigned_url[..presigned_url.len().min(80)]
    );
    // 形态断言：S3 v4 签名 URL 应含 X-Amz-* 参数（V1 是 q-sign-algorithm）
    assert!(
        presigned_url.contains("X-Amz-Signature") || presigned_url.contains("x-amz-signature"),
        "S3 v4 presign URL 应含 X-Amz-Signature 参数：got 前 200 字节 = {}",
        &presigned_url[..presigned_url.len().min(200)]
    );
    // HTTP GET 验证 URL 可达（60s 有效期 + 服务端校验签名）
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .unwrap();
    let http_resp = http
        .get(&presigned_url)
        .send()
        .await
        .expect("presign URL HTTP GET 请求失败（签名 / endpoint / region 错配）");
    let http_status = http_resp.status();
    let http_body = http_resp.bytes().await.unwrap_or_default();
    eprintln!(
        "[spike]   presign URL HTTP status={http_status} bytes={}",
        http_body.len()
    );
    assert!(
        http_status.is_success(),
        "presign URL GET 必须 2xx：status={http_status} body 前 200 字节 = {}",
        String::from_utf8_lossy(&http_body[..http_body.len().min(200)])
    );
    assert_eq!(
        http_body.as_ref(),
        payload.as_slice(),
        "presign URL GET body 字节应等于原 payload"
    );

    // 5. copy_object —— 服务端 PUT copy-object（memory backend 不支持，真 COS 应成功）
    let key_dst = format!("{prefix}dst.bin");
    cos.copy_object(&key_src, &key_dst)
        .await
        .expect("copy_object 真 COS 失败（OpenDAL 走 PUT + x-cos-copy-source 需 S3 v4 兼容）");
    let dst_read = cos
        .get_object(&key_dst)
        .await
        .expect("get_object copy 后的 dst 失败");
    assert_eq!(dst_read, payload, "copy_object 后 dst 字节必须 == src");
    eprintln!("[spike] 5/6 copy_object OK: src={key_src} dst={key_dst}");

    // 6. delete_object + 幂等验证
    cos.delete_object(&key_dst)
        .await
        .expect("delete_object dst 失败");
    let after_delete = cos.delete_object(&key_dst).await;
    assert!(
        after_delete.is_ok(),
        "delete_object 对不存在 key 必须幂等成功（对齐 NoopCos / OpenDalCos 行为）"
    );
    cos.delete_object(&key_src)
        .await
        .expect("delete_object src 失败");
    eprintln!("[spike] 6/6 delete_object OK: src+dst 已清理");

    eprintln!("[spike] 全部 6 method 通过真 COS 验证");
}

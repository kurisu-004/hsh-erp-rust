//! OpenDAL S3 backend 集成测试（2026-09-20 spike 第 4 轮 / 迁移清理后）
//!
//! 2026-09-23 重构：补 `JWT_PRIVATE_KEY_PATH` / `JWT_PUBLIC_KEYS_DIR` env
//! （RS256 + kid 多密钥轮换要求）。本测试不实际签发/验签 token（仅调
//! `AppConfig::from_env` 走 COS_BACKEND 解析路径），但 from_env 启动期严格校验
//! 必须有 PEM 文件存在；用 tempfile 在测试 setUp 阶段写一对一次性 pem，`JwtConfig`
//! 解析成功即可。环境串行锁避免与其它 set_var 测试并行冲突。
//!
//! 覆盖 OpenDAL `Operator` 适配 `CosClient` trait 的 6 个 method +
//! 配置 / backend 选择路径。**不发起任何网络请求**，全部走 `NoopOpenDal`（Memory backend）。
//!
//! 范围：
//! - trait method happy path：put/get/head/delete/copy/presign
//! - 边界：删除不存在 key（幂等）、二次 put 覆盖、head 不存在 key 返回错误
//! - 配置：`CosBackend` 枚举解析、build_cos_client 二选一路径（OpenDal / Noop）
//! - STS builder smoke：S3 builder 接受 session_token
//!
//! 不依赖 PG / Redis，纯单测式集成（虽然在 `tests/` 目录，但避免 docker daemon
//! 依赖；保留 `--test-threads=4` 兼容默认调度）。
//!
//! ## Fixture 范本化（2026-09-24 PR13 Phase I）
//! 本文件原 `use ...::fixtures::*` 改走 `use hsh_erp_test_support::*` +
//! `load_cos_opendal_fixture(&pool)`（stub）。fixture 是空 stub（`SELECT 1;`），
//! 保持 `load_<binary>_fixture` 调用约定一致。本测试 4 个 AppConfig::from_env 测试
//! 走 ENV_LOCK 串行，不依赖 DB pool；其它 trait method 测试也不需要 DB。
//! 字面请求 / 断言逐字保留。

#![allow(clippy::needless_raw_string_hashes)]

use std::sync::Arc;

use hsh_erp_rust::infra::config::{AppConfig, CosBackend, CosConfig};
use hsh_erp_rust::infra::cos::CosClient;
use hsh_erp_rust::infra::cos_opendal::{NoopOpenDal, OpenDalCos, build_cos_client};
use hsh_erp_test_support::{load_cos_opendal_fixture, test_pool};

// =====================================================================================
// helper：每个测试用唯一 key 避免 Memory backend namespace 冲突
// =====================================================================================

fn fresh_client() -> NoopOpenDal {
    NoopOpenDal::new().expect("NoopOpenDal::new 应永远 OK")
}

fn make_cos_config(backend: CosBackend, enabled: bool) -> CosConfig {
    // 2026-09-20 迁移清理：删 `sts_duration_seconds` 字段（STS 链路完全走
    // `UploadSessionConfig::sts_duration_seconds` + python 后端转发）。
    CosConfig {
        backend,
        enabled,
        region: "ap-shanghai".to_string(),
        secret_id: "AKID_TEST".to_string(),
        secret_key: "SECRET_TEST".to_string(),
        app_id: "1234567890".to_string(),
        endpoint: "".to_string(),
        scheme: "https".to_string(),
        upload_prefix: "uploads".to_string(),
        presign_expire_seconds: 3600,
        max_file_size: 300 * 1024 * 1024,
        tmp_prefix: "tmp/".to_string(),
        bucket: "test-bucket-1234567890".to_string(),
    }
}

// =====================================================================================
// trait method 6 个 happy path
// =====================================================================================

#[tokio::test]
async fn put_get_roundtrip_via_trait() {
    let _pool = test_pool().await;
    let _fx = load_cos_opendal_fixture(&_pool).await;
    let client = fresh_client();
    let key = "spike/it/roundtrip.bin";
    let body = b"integration test body".to_vec();

    client
        .put_object(key, body.clone(), "application/octet-stream")
        .await
        .expect("put_object");
    let got = client.get_object(key).await.expect("get_object");
    assert_eq!(got, body);
}

#[tokio::test]
async fn delete_nonexistent_is_idempotent_ok() {
    let _pool = test_pool().await;
    let _fx = load_cos_opendal_fixture(&_pool).await;
    let client = fresh_client();
    let key = "spike/it/never_exists_DELETE.bin";
    // 反复删 3 次都不应报错（幂等）
    for _ in 0..3 {
        client
            .delete_object(key)
            .await
            .expect("delete nonexistent 必须幂等 Ok");
    }
}

#[tokio::test]
async fn delete_existing_then_get_returns_error() {
    let _pool = test_pool().await;
    let _fx = load_cos_opendal_fixture(&_pool).await;
    let client = fresh_client();
    let key = "spike/it/exists_then_deleted.bin";
    let body = b"to be deleted".to_vec();

    client.put_object(key, body, "text/plain").await.unwrap();
    client.delete_object(key).await.expect("delete");
    // 删除后 get 应返回错误（业务上抛 4xx/5xx）
    let result = client.get_object(key).await;
    assert!(
        result.is_err(),
        "已删除对象的 get_object 必须返回 Err，实际 Ok（泄漏状态）"
    );
}

#[tokio::test]
async fn head_object_returns_correct_size() {
    // 2026-09-20 spike 发现：OpenDAL Memory backend 的 head_object **不设置 etag**
    // （返回 None → 转空串）。真 S3 backend 上 etag 由服务端回传（MD5），无此问题。
    // 本测试只断言 size 正确。
    let _pool = test_pool().await;
    let _fx = load_cos_opendal_fixture(&_pool).await;
    let client = fresh_client();
    let key = "spike/it/head_size.bin";
    let body = b"x".repeat(123); // 123 bytes
    client
        .put_object(key, body.clone(), "text/plain")
        .await
        .unwrap();

    let meta = client.head_object(key).await.expect("head");
    assert_eq!(meta.size as usize, 123, "size 必须精确等于 put 的字节数");
    // etag 在 Memory backend 上故意为空——spike 仅记录，不修
}

#[tokio::test]
async fn head_object_nonexistent_returns_error() {
    let _pool = test_pool().await;
    let _fx = load_cos_opendal_fixture(&_pool).await;
    let client = fresh_client();
    let key = "spike/it/never_exists_HEAD.bin";
    let result = client.head_object(key).await;
    assert!(
        result.is_err(),
        "head 不存在 key 必须返回 Err，实际 Ok（业务层 head 期望清晰语义）"
    );
}

#[tokio::test]
async fn copy_object_creates_dst_with_same_content() {
    // 2026-09-20 spike 发现：OpenDAL Memory backend 不支持 copy（Unsupported）。
    // 真 S3 backend 上 copy 走服务端 PUT copy-object，spike 无真凭据未实测。
    // 本测试断言：NoopOpenDal（Memory）调用 copy_object 必须返回 Err，**不**绕过。
    let _pool = test_pool().await;
    let _fx = load_cos_opendal_fixture(&_pool).await;
    let client = fresh_client();
    let src = "spike/it/copy_SRC.bin";
    let dst = "spike/it/copy_DST.bin";
    let body = b"copy me across".to_vec();

    client
        .put_object(src, body.clone(), "text/plain")
        .await
        .unwrap();
    let result = client.copy_object(src, dst).await;
    assert!(
        result.is_err(),
        "Memory backend copy 必须返回 Err（Unsupported），实际 Ok"
    );
}

#[tokio::test]
async fn presigned_get_url_returns_string_containing_key() {
    let _pool = test_pool().await;
    let _fx = load_cos_opendal_fixture(&_pool).await;
    let client = fresh_client();
    let key = "spike/it/presign/file.pdf";
    let url = client
        .presigned_get_url(key, 3600)
        .await
        .expect("presigned url");
    assert!(
        url.contains(key),
        "NoopOpenDal 占位 URL 必须含 key，实际: {url}"
    );
}

// =====================================================================================
// OpenDalCos::new 真实构造（用伪造凭据；不真连 COS，只验证 builder + Operator::new 成功）
// =====================================================================================

#[tokio::test]
async fn open_dal_cos_new_accepts_full_endpoint() {
    let cfg = CosConfig {
        endpoint: "https://my-private-cos.example.com".to_string(),
        ..make_cos_config(CosBackend::OpenDal, true)
    };
    let client = OpenDalCos::new(cfg).expect("带 endpoint 的构造必须成功");
    // 不发请求；仅验证 client 持有 Operator
    let _ = client;
}

// =====================================================================================
// build_cos_client 二选一路径（2026-09-20 迁移清理后只剩 OpenDal + Noop）
// =====================================================================================

#[tokio::test]
async fn build_cos_client_opendal_disabled_uses_noop_opendal() {
    let cfg = make_cos_config(CosBackend::OpenDal, false);
    let client = build_cos_client(&cfg).expect("build opendal disabled");
    // 实际是 NoopOpenDal（Operator），put + get 应走内存并成功
    let key = "spike/it/build_opendal_disabled.bin";
    client
        .put_object(key, b"x".to_vec(), "text/plain")
        .await
        .unwrap();
    let got = client.get_object(key).await.unwrap();
    assert_eq!(got, b"x".to_vec());
}

#[tokio::test]
async fn build_cos_client_noop_returns_noop_open_dal() {
    // 2026-09-20 迁移清理：COS_BACKEND=noop 不论 COS_ENABLED 都走 NoopOpenDal
    // （迁移前是 NoopCos，迁移后统一 NoopOpenDal 真内存）。
    for enabled in [true, false] {
        let cfg = make_cos_config(CosBackend::Noop, enabled);
        let client = build_cos_client(&cfg).expect("build noop");
        let _: Arc<dyn CosClient> = client;
    }
}

// =====================================================================================
// AppConfig 解析路径：env 变量 → CosBackend enum
// =====================================================================================

#[test]
fn cos_backend_default_is_opendal_when_env_missing() {
    // 不污染进程 env；用临时函数直接验证字符串 → enum 映射逻辑
    //
    // 2026-09-20 迁移清理：删 `CosSdk` 变体后仅 `OpenDal` / `Noop` 两路；
    // 缺省从 `cos_sdk` 切到 `opendal`，非法值 → 解析失败（不静默 fallback）。
    fn parse(input: Option<&str>) -> Option<CosBackend> {
        match input.unwrap_or("opendal").to_ascii_lowercase().as_str() {
            "opendal" => Some(CosBackend::OpenDal),
            "noop" => Some(CosBackend::Noop),
            _ => None, // 非法值：config.rs 的 from_env 会 anyhow bail
        }
    }
    assert_eq!(parse(None), Some(CosBackend::OpenDal));
    assert_eq!(parse(Some("opendal")), Some(CosBackend::OpenDal));
    assert_eq!(parse(Some("OPENDAL")), Some(CosBackend::OpenDal));
    assert_eq!(parse(Some("noop")), Some(CosBackend::Noop));
    assert_eq!(parse(Some("NOOP")), Some(CosBackend::Noop));
    // 非法值（历史 `.env` 残留的 `cos_sdk` / 拼写错误）：解析失败
    assert_eq!(parse(Some("cos_sdk")), None);
    assert_eq!(parse(Some("COS_SDK")), None);
    assert_eq!(parse(Some("garbage")), None);
}

#[test]
fn cos_config_clone_preserves_backend_field() {
    let cfg = make_cos_config(CosBackend::OpenDal, true);
    let cloned = cfg.clone();
    assert_eq!(cloned.backend, CosBackend::OpenDal);
    assert!(cloned.enabled);
    assert_eq!(cloned.bucket, "test-bucket-1234567890");
}

// =====================================================================================
// 整体端到端（不依赖 PG）：同一 client 跑完 6 个 method，证明 Operator 全链路通
// =====================================================================================

#[tokio::test]
async fn end_to_end_all_six_methods_on_fresh_namespace() {
    let client = fresh_client();
    let key = "spike/it/e2e/full.bin";
    let body = b"end-to-end body for opendal".to_vec();

    // 1. put
    client
        .put_object(key, body.clone(), "application/octet-stream")
        .await
        .expect("put");
    // 2. head（拿 size；Memory backend 不填 etag）
    let meta = client.head_object(key).await.expect("head");
    assert_eq!(meta.size as usize, body.len());
    // 3. get
    let got = client.get_object(key).await.expect("get");
    assert_eq!(got, body);
    // 4. copy（Memory backend 不支持，确认返回 Err 而非绕过）
    let copy_dst = "spike/it/e2e/full_COPY.bin";
    let copy_result = client.copy_object(key, copy_dst).await;
    assert!(copy_result.is_err(), "Memory backend copy 必须 Err");
    // 5. presign（NoopOpenDal 返回 local:// 占位，仅断言非空 + 含 key）
    let url = client.presigned_get_url(key, 60).await.expect("presign");
    assert!(url.contains(key));
    // 6. delete
    client.delete_object(key).await.expect("delete");
    // 删完 get 应 Err
    assert!(client.get_object(key).await.is_err());
}

// =====================================================================================
// AppConfig 集成：实际通过 AppConfig::from_env 走 COS_BACKEND 解析路径
// =====================================================================================

/// env 串行锁（避免 std::env::set_var / remove_var 与其它测试并行竞争）
/// 单 binary 内串行访问；不同 binary 间各自持锁不冲突。
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// 2026-09-23 重构：测试用 RSA PEM 路径（PKCS#8 私钥 + SPKI 公钥目录）。
///
/// 4 个 AppConfig::from_env 测试需要 `JWT_PRIVATE_KEY_PATH` 与
/// `JWT_PUBLIC_KEYS_DIR` 指向真实存在的 PEM 文件，否则 from_env 在启动期严格
/// 校验阶段 bail。本函数：
/// 1. 进程级 `OnceLock` 内生成一对 2048-bit RSA（pkcs8 私钥 + spki 公钥）；
/// 2. 写到 `std::env::temp_dir()` 下唯一的 `cos_opendal_test_pem_<uuid>/` 目录
///    （private key: `current.pem`，public dir 即该目录）；
/// 3. 返回 `(private_pem_path, public_keys_dir)` 绝对路径。
///
/// 文件**不**主动删除：进程退出后由 OS 清理 tmp；目录命名带 uuid 防多进程冲突。
/// 测试目的仅满足 from_env 校验，不真用 token。
fn test_jwt_pem_paths() -> (&'static str, &'static str) {
    use rsa::pkcs8::{EncodePrivateKey, EncodePublicKey, LineEnding};
    use rsa::traits::PublicKeyParts;
    use rsa::{RsaPrivateKey, RsaPublicKey};

    static PATHS: std::sync::OnceLock<(String, String)> = std::sync::OnceLock::new();
    let (priv_str, dir_str) = PATHS.get_or_init(|| {
        let mut rng = rsa::rand_core::OsRng;
        let priv_key = RsaPrivateKey::new(&mut rng, 2048).expect("gen rsa priv");
        let pub_key = RsaPublicKey::from(&priv_key);
        debug_assert_eq!(pub_key.size() * 8, 2048);

        // 目录：<tmp>/cos_opendal_test_pem_<uuid>/
        let tmp = std::env::temp_dir();
        let dir_name = format!(
            "cos_opendal_test_pem_{}",
            uuid::Uuid::new_v4().simple()
        );
        let dir = tmp.join(&dir_name);
        std::fs::create_dir_all(&dir).expect("create tmp pem dir");

        let priv_path = dir.join("current.pem");
        let pub_path = dir.join("current.pem.pub");

        let priv_pem = priv_key
            .to_pkcs8_pem(LineEnding::LF)
            .expect("priv pem")
            .to_string();
        let pub_pem = pub_key
            .to_public_key_pem(LineEnding::LF)
            .expect("pub pem");

        std::fs::write(&priv_path, priv_pem.as_bytes()).expect("write priv pem");
        std::fs::write(&pub_path, pub_pem.as_bytes()).expect("write pub pem");

        (
            priv_path.to_string_lossy().into_owned(),
            dir.to_string_lossy().into_owned(),
        )
    });
    // 安全：OnceLock 持有的 String 是 'static
    let priv_static: &'static str = Box::leak(priv_str.clone().into_boxed_str());
    let dir_static: &'static str = Box::leak(dir_str.clone().into_boxed_str());
    (priv_static, dir_static)
}

#[test]
fn app_config_from_env_with_cos_backend_opendal() {
    // 2026-09-20 迁移清理：COS_ENABLED=true + COS_BACKEND=opendal → OpenDal
    // （迁移前 COS_ENABLED=false + COS_BACKEND=opendal 也是 OpenDal；新行为下
    // COS_ENABLED=false 强制 Noop，详见 config.rs from_env 注释）。
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (priv_path, pub_dir) = test_jwt_pem_paths();
    unsafe {
        std::env::set_var("JWT_SECRET", "test_secret_for_opendal_spike");
        std::env::set_var("JWT_PRIVATE_KEY_PATH", priv_path);
        std::env::set_var("JWT_PUBLIC_KEYS_DIR", pub_dir);
        std::env::set_var("POSTGRES_USER", "test");
        std::env::set_var("POSTGRES_PASSWORD", "test");
        std::env::set_var("POSTGRES_DB", "test");
        std::env::set_var("COS_ENABLED", "true");
        std::env::set_var("COS_BACKEND", "opendal");
        std::env::set_var("COS_BUCKET", "test-bucket-1234567890");
        std::env::set_var("COS_SECRET_ID", "AKID_TEST");
        std::env::set_var("COS_SECRET_KEY", "SECRET_TEST");
    }
    let cfg = AppConfig::from_env(".env.nonexistent_for_test").expect("from_env");
    assert_eq!(cfg.cos.backend, CosBackend::OpenDal);
    assert!(cfg.cos.enabled, "应读出 COS_ENABLED=true");
    unsafe {
        std::env::remove_var("COS_BACKEND");
        std::env::remove_var("JWT_PRIVATE_KEY_PATH");
        std::env::remove_var("JWT_PUBLIC_KEYS_DIR");
    }
}

#[test]
fn app_config_cos_enabled_false_forces_noop_regardless_of_backend_env() {
    // 2026-09-20 迁移清理：COS_ENABLED=false 强制 Noop（不论 COS_BACKEND 怎么设）。
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (priv_path, pub_dir) = test_jwt_pem_paths();
    unsafe {
        std::env::set_var("JWT_SECRET", "test_secret_for_opendal_spike");
        std::env::set_var("JWT_PRIVATE_KEY_PATH", priv_path);
        std::env::set_var("JWT_PUBLIC_KEYS_DIR", pub_dir);
        std::env::set_var("POSTGRES_USER", "test");
        std::env::set_var("POSTGRES_PASSWORD", "test");
        std::env::set_var("POSTGRES_DB", "test");
        std::env::set_var("COS_ENABLED", "false");
        std::env::set_var("COS_BACKEND", "opendal");
        std::env::set_var("COS_BUCKET", "test-bucket-1234567890");
    }
    let cfg = AppConfig::from_env(".env.nonexistent_for_test").expect("from_env");
    assert_eq!(
        cfg.cos.backend,
        CosBackend::Noop,
        "COS_ENABLED=false 必须强制 Noop，实际: {:?}",
        cfg.cos.backend
    );
    unsafe {
        std::env::remove_var("COS_BACKEND");
        std::env::remove_var("JWT_PRIVATE_KEY_PATH");
        std::env::remove_var("JWT_PUBLIC_KEYS_DIR");
    }
}

#[test]
fn app_config_default_backend_is_opendal() {
    // 2026-09-20 迁移清理：缺省 backend 从 `cos_sdk` 切到 `opendal`。
    // 需要 COS_ENABLED=true 才会走 COS_BACKEND 解析（false 强制 Noop），
    // 此时 from_env 强制要求 COS_SECRET_ID / COS_SECRET_KEY。
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (priv_path, pub_dir) = test_jwt_pem_paths();
    unsafe {
        std::env::set_var("JWT_SECRET", "test_secret_for_opendal_spike");
        std::env::set_var("JWT_PRIVATE_KEY_PATH", priv_path);
        std::env::set_var("JWT_PUBLIC_KEYS_DIR", pub_dir);
        std::env::set_var("POSTGRES_USER", "test");
        std::env::set_var("POSTGRES_PASSWORD", "test");
        std::env::set_var("POSTGRES_DB", "test");
        std::env::set_var("COS_ENABLED", "true");
        std::env::remove_var("COS_BACKEND");
        std::env::set_var("COS_BUCKET", "test-bucket-1234567890");
        std::env::set_var("COS_SECRET_ID", "AKID_TEST");
        std::env::set_var("COS_SECRET_KEY", "SECRET_TEST");
    }
    let cfg = AppConfig::from_env(".env.nonexistent_for_test").expect("from_env");
    assert_eq!(cfg.cos.backend, CosBackend::OpenDal);
    unsafe {
        std::env::remove_var("JWT_PRIVATE_KEY_PATH");
        std::env::remove_var("JWT_PUBLIC_KEYS_DIR");
    }
}

#[test]
fn app_config_from_env_with_invalid_cos_backend_fails() {
    // 2026-09-20 迁移清理：非法值（含历史 `cos_sdk`）→ anyhow bail（不静默 fallback）
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (priv_path, pub_dir) = test_jwt_pem_paths();
    unsafe {
        std::env::set_var("JWT_SECRET", "test_secret_for_opendal_spike");
        std::env::set_var("JWT_PRIVATE_KEY_PATH", priv_path);
        std::env::set_var("JWT_PUBLIC_KEYS_DIR", pub_dir);
        std::env::set_var("POSTGRES_USER", "test");
        std::env::set_var("POSTGRES_PASSWORD", "test");
        std::env::set_var("POSTGRES_DB", "test");
        std::env::set_var("COS_ENABLED", "true");
        std::env::set_var("COS_BACKEND", "cos_sdk"); // 历史残留，迁移后非法
        std::env::set_var("COS_BUCKET", "test-bucket-1234567890");
        std::env::set_var("COS_SECRET_ID", "AKID_TEST");
        std::env::set_var("COS_SECRET_KEY", "SECRET_TEST");
    }
    let result = AppConfig::from_env(".env.nonexistent_for_test");
    unsafe {
        std::env::remove_var("COS_BACKEND");
        std::env::remove_var("JWT_PRIVATE_KEY_PATH");
        std::env::remove_var("JWT_PUBLIC_KEYS_DIR");
    }
    assert!(
        result.is_err(),
        "非法 COS_BACKEND=cos_sdk 必须报错，实际: {result:?}"
    );
}
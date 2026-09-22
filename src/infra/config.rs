//! 应用配置：dotenvy 加载 .env 后从 std::env 读取
//! 对应 Python myERP/core/config.py

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use jsonwebtoken::{DecodingKey, EncodingKey};

#[derive(Clone, Debug)]
pub struct AppConfig {
    pub database_url: String,
    pub listen_addr: String,
    pub jwt: JwtConfig,
    pub cos: CosConfig,
    pub snowflake: SnowflakeConfig,
    pub max_request_body_size: usize,
    pub auto_complete: AutoCompleteConfig,
    /// Redis 会话存储（服务端 session 真相源；access token 吊销依赖）
    pub redis: RedisConfig,
    /// 送货单 Excel 模板目录（P4 打印）。环境变量 `DELIVERY_NOTE_TEMPLATE_DIR`
    /// 优先；缺省回退到编译期绝对路径 `<CARGO_MANIFEST_DIR>/template`，
    /// 因此本地 `cargo run` 不依赖 cwd。
    pub delivery_note_template_dir: PathBuf,
    /// 2026-09-14 新增：是否启用 /api/v2/_e2e/* hook。
    /// 启用后 e2e 测试可通过匿名 POST 直接灌入 seed 数据 + revoke session。
    /// 仅 dev / test 环境开启；prod 通过环境变量显式 `E2E_HOOKS_ENABLED=false`（ops 责任）。
    /// 2026-09-14 修复 Bug #1：移除 main.rs 原 release profile 二次硬关。
    /// 环境变量 `E2E_HOOKS_ENABLED`，缺省 `true`（dev/test 容器场景）。
    pub enable_e2e_hooks: bool,
    /// 2026-09-15 followup-cleanup A5/A6：dashboard WS 心跳间隔（秒）。
    /// 生产 30s；测试可调小到 1s 以便在 CI 内验证 heartbeat text 帧。
    /// 环境变量 `WS_HEARTBEAT_INTERVAL_SECONDS`，缺省 `30`。
    pub ws_heartbeat_interval_seconds: u64,
    /// 2026-09-20 新增：HTTP `/api/v2/*` nest 请求超时（秒）。仅挂在 nest 内层
    /// （不影响 WS 长连接，也不影响根 Router 的 CORS/Body limit）。环境变量
    /// `REQUEST_TIMEOUT_SECONDS`，缺省 `30`。
    pub request_timeout_seconds: u64,
    /// 2026-09-18 新增：上传会话域配置（Redis 会话机制 + python STS 转发）。
    pub upload_session: UploadSessionConfig,
}

#[derive(Clone, Debug)]
pub struct RedisConfig {
    /// 完整 Redis URL（优先 `REDIS_URL`，否则从 `REDIS_HOST/PORT/DB/PASSWORD` 拼接）
    pub url: String,
    /// session 条目 TTL（秒），对应 JWT `access_ttl_seconds` 的预期寿命；滑动窗口
    pub session_ttl_seconds: u64,
    /// 连接池上限
    pub pool_max_size: usize,
}

/// JWT 配置（2026-09-22 增 audience 字段 + 2026-09-23 重构 RS256 + kid）
///
/// ## 2026-09-23 重构要点（HS256 → RS256 + kid）
/// - `signing_kid` / `private_key` / `public_keys` / `allow_hs256_fallback` 4 字段
///   本轮新增，详见字段 doc。
/// - `secret` 字段保留：HS256 fallback 过渡期 decode 端仍按 `allow_hs256_fallback=true`
///   走 secret 验签；签发端永不产出 HS256 token。下轮 cleanup PR 删除 secret + fallback 路径。
/// - 启动期严格校验：`JWT_PRIVATE_KEY_PATH` / `JWT_PUBLIC_KEYS_DIR` / `signing_kid ∈ public_keys`
///   任何一项缺失或格式错误即 bail（fail-fast，避免运行时才发现签不出/发不出对应 kid）。
#[derive(Clone, Debug)]
pub struct JwtConfig {
    /// 2026-09-23 重构：HS256 fallback 过渡期仍占用。**签发端不再使用**（encode 强制
    /// RS256），仅作为 `decode_access` / `decode_refresh` 在 `allow_hs256_fallback=true`
    /// 时对历史 HS256 token 的验签 secret。环境变量 `JWT_SECRET`，仅在
    /// `allow_hs256_fallback=true` 时必填；下轮 cleanup PR 删除。
    pub secret: String,
    pub issuer: String,
    /// JWT `aud` 校验目标（2026-09-22 新增：删 Python v1 兼容后 Rust 自签 token 强绑定 audience）。
    /// 环境变量 `JWT_AUDIENCE`，缺省 `hsh-erp-rust`。
    pub audience: String,
    pub access_ttl_seconds: i64,
    pub refresh_ttl_days: i64,
    /// 2026-09-23 重构：RS256 + kid 多密钥轮换。
    ///
    /// - `signing_kid`：签发端写入 header.kid 的值；环境变量 `JWT_SIGNING_KID`，缺省 `current`。
    ///   必须出现在 `public_keys` 字典里（启动期校验：避免签发端 kid 找不到对应公钥）。
    /// - `private_key`：从 `JWT_PRIVATE_KEY_PATH`（必填）读 PEM 后构造的 `EncodingKey`。
    ///   生产构建中这是 RS256 私钥；签发端不再走 HS256 secret。
    /// - `public_keys`：从 `JWT_PUBLIC_KEYS_DIR`（必填）目录扫描 `*.pem`，kid = 文件名
    ///   去后缀（同一目录 kid 必须唯一）。`BTreeMap` 保证按 kid 字典序遍历，便于审计。
    /// - `allow_hs256_fallback`：环境变量 `JWT_ALLOW_HS256_FALLBACK`，缺省 `true`；
    ///   `true` 时 `secret` 必填且 `decode_access` / `decode_refresh` 接受 HS256 token
    ///   走 `secret` 验签；`false` 时仅 RS256。HS256 fallback 段写明过渡期保留，
    ///   下轮 cleanup PR 删除。
    pub signing_kid: String,
    pub private_key: EncodingKey,
    pub public_keys: BTreeMap<String, DecodingKey>,
    pub allow_hs256_fallback: bool,
}

/// COS 客户端 backend 选择（2026-09-20 spike 新增，2026-09-20 迁移清理后只保留两路）。
///
/// 迁移清理前曾保留三路（`cos_sdk` / `opendal` / `noop`）；迁完只保留
/// `OpenDal` + `Noop`，删掉 `cos_sdk`（及其依赖的 `cos-rust-sdk` + 手写 V1 签名
/// Presigner，详见 `OPENDAL_SPIKE.md` §12）。
///
/// 通过 `COS_BACKEND` 环境变量切换：
/// - `opendal`（默认）：走 `OpenDalCos`（Apache OpenDAL S3 backend）
/// - `noop`：强制走 `NoopCos`，与 `COS_ENABLED=false` 效果相同（本地 cargo run / 集成测）
///
/// 选 `opendal` 时仍受 `COS_ENABLED` 控制：enabled=true → `OpenDalCos`，
/// enabled=false → `NoopOpenDal`（与 spike 业务回归测试路径对齐）。
///
/// 非法值（如历史 `.env` 残留的 `cos_sdk`）→ 解析失败并报错，不静默 fallback。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CosBackend {
    OpenDal,
    Noop,
}

#[derive(Clone, Debug)]
pub struct CosConfig {
    /// 2026-09-20 spike 新增；2026-09-20 迁移清理后保留 2 路（`OpenDal` / `Noop`）。
    /// env `COS_BACKEND` 解析，缺省 `OpenDal`（迁移默认从 `cos_sdk` 切到 `opendal`）。
    pub backend: CosBackend,
    /// 是否启用真实 COS 上传。
    ///
    /// - true：使用 OpenDalCos（真实上传到腾讯云，S3 v4 兼容）
    /// - false：使用 NoopOpenDal（OpenDAL Memory backend 本地内存占位，**不走网络**）
    ///
    /// 环境变量 `COS_ENABLED`，缺省 `true`。
    ///
    /// 2026-09-11 新增；2026-09-20 迁移：NoopCos 仅保留给 `COS_BACKEND=noop` 显式场景，
    /// `enabled=false` 默认走 OpenDalCos 路径下的 NoopOpenDal（保持 OpenDAL 全链路可测）。
    pub enabled: bool,
    pub region: String,
    pub bucket: String,
    pub secret_id: String,
    pub secret_key: String,
    /// COS AppId（腾讯云账户 ID）。若 bucket 命名为 `<name>-<appid>` 格式
    /// （如 `erp-drawing-1410882329`），填 appid 用于拼 endpoint；空则尝试
    /// 从 bucket 解析。环境变量 `COS_APP_ID`，缺省空串。
    ///
    /// 2026-09-11 新增
    pub app_id: String,
    /// 可选 endpoint 覆盖（私有化部署 / 加速域名）。
    /// 留空走标准 endpoint：`{scheme}://cos.{region}.myqcloud.com`。
    /// 环境变量 `COS_ENDPOINT`，缺省空串。
    ///
    /// 2026-09-11 新增；2026-09-20 spike 修正：endpoint 拼装去掉 `-{app_id}` 后缀，
    /// 由 OpenDAL 配合 `enable_virtual_host_style()` 把 bucket 名（已含 appid 后缀）
    /// 整体作为 virtual-host 第一段。
    pub endpoint: String,
    pub scheme: String,
    pub upload_prefix: String,
    pub presign_expire_seconds: u32,
    pub max_file_size: usize,
    /// COS 临时对象 prefix 模板前缀（默认 `tmp/`，含尾斜杠）。可用于多种场景：
    /// - confirm handler 校验 `tmp_key` 必须以此前缀开头
    /// - upload_session 域 `tmp/sess/<uuid>/` 派生时也以此前缀为锚
    ///
    /// 可通过 `COS_TMP_PREFIX` env 覆盖，缺省 `tmp/`。
    pub tmp_prefix: String,
}

/// 雪花 ID 配置（位布局对齐 myERP Python `snowflake-id` 包）：
/// `ts << 22 | instance << 12 | seq`。instance 占 10 位（0..=1023）。
#[derive(Copy, Clone, Debug)]
pub struct SnowflakeConfig {
    /// 节点实例号（环境变量 `SNOWFLAKE_INSTANCE`，0..=1023）。
    pub instance: u16,
    /// 自定义纪元（毫秒），环境变量 `SNOWFLAKE_EPOCH`。
    pub epoch_ms: u64,
}

#[derive(Copy, Clone, Debug)]
pub struct AutoCompleteConfig {
    pub threshold_days: u32,
    pub interval_hours: u64,
}

/// 上传会话域配置（2026-09-18 新增）
///
/// 集中管理 upload_session 域的所有可调参数：
/// - `python_backend_base_url`：rust → python STS 转发目标地址
/// - `ttl_seconds`：Redis key TTL（24h 滑动）
/// - `sts_duration_seconds`：请求 python 签发时的 expire_seconds（python 端可能按
///   自身配置上下限收敛；此值仅作调用方期望值）
/// - `renew_threshold_seconds`：get_or_create hit 路径下，凭证 < 此阈值自动 renew
#[derive(Clone, Debug)]
pub struct UploadSessionConfig {
    /// 完整 base URL（含 scheme / host / port），如 `http://backend:8000`。
    /// 留空 → NoopPythonSts（本地调试用）。
    /// 环境变量 `PYTHON_BACKEND_BASE_URL`，缺省 `http://backend:8000`。
    pub python_backend_base_url: String,
    /// Redis 会话条目 TTL（秒）；每次写都 SET EX 续期。
    /// 环境变量 `UPLOAD_SESSION_TTL_SECONDS`，缺省 `86400`（24h）。
    pub ttl_seconds: u64,
    /// 请求 python 端签发 STS 时的期望有效期（秒）。
    /// 环境变量 `UPLOAD_SESSION_STS_DURATION_SECONDS`，缺省 `7200`（2h，比 STS
    /// 默认 900s 长以减少 renew 频率）。
    pub sts_duration_seconds: u32,
    /// get_or_create hit 路径下，凭证剩余有效期 < 此阈值 → 自动 renew。
    /// 环境变量 `UPLOAD_SESSION_RENEW_THRESHOLD_SECONDS`，缺省 `600`（10min）。
    pub renew_threshold_seconds: i64,
}

impl AppConfig {
    pub fn from_env(env_file: &str) -> Result<Self> {
        // 加载 .env（不存在不报错）
        let _ = dotenvy::from_filename(env_file);

        Ok(Self {
            database_url: build_database_url()?,
            listen_addr: env_or("LISTEN_ADDR", "0.0.0.0:3000"),
            max_request_body_size: env_parse("MAX_REQUEST_BODY_SIZE", 300 * 1024 * 1024)?,

            jwt: {
                // 2026-09-23 重构：RS256 + kid 多密钥轮换。
                //
                // 加载规则：
                // 1. JWT_PRIVATE_KEY_PATH（必填）→ fs::read → EncodingKey::from_rsa_pem
                //    失败即 bail（缺私钥签不出 token）
                // 2. JWT_PUBLIC_KEYS_DIR（必填）→ fs::read_dir 扫描 *.pem，kid = 文件名去
                //    后缀；kid 唯一性校验；目录不存在 bail
                // 3. JWT_SIGNING_KID 必须在 public_keys 字典内（启动期断言：签发端
                //    kid 必须有对应公钥），缺失 bail
                // 4. JWT_ALLOW_HS256_FALLBACK=true（默认）：保留 HS256 fallback 能力，
                //    此时 JWT_SECRET 仍必填（decode 走 secret 验签）；false：仅 RS256，
                //    JWT_SECRET 可省略（HS256 token 一律 40100）
                //
                // HS256 fallback 段：过渡期保留——decode_access / decode_refresh 按
                // header.alg 分支，HS256 仅在 allow_hs256_fallback=true 且
                // hs256_fallback_secret.is_some() 时走 secret 验签；签发端永不产出
                // HS256 token。下轮 cleanup PR（next iteration）删除 secret 字段 +
                // fallback 路径。
                let private_key_path = env_required("JWT_PRIVATE_KEY_PATH")?;
                let private_key = load_private_key(&private_key_path)
                    .with_context(|| format!("加载 JWT 私钥失败 ({private_key_path})"))?;
                let public_keys_dir = env_required("JWT_PUBLIC_KEYS_DIR")?;
                let public_keys = load_public_keys_dir(&public_keys_dir)
                    .with_context(|| format!("扫描 JWT 公钥目录失败 ({public_keys_dir})"))?;
                let signing_kid = env_or("JWT_SIGNING_KID", "current");
                if !public_keys.contains_key(&signing_kid) {
                    return Err(anyhow!(
                        "JWT_SIGNING_KID={signing_kid:?} 不在 JWT_PUBLIC_KEYS_DIR={public_keys_dir:?} \
                         扫描出的公钥字典中（kid = PEM 文件名去后缀）。请检查 env 配置或 \
                         把 {signing_kid}.pem 放进公钥目录"
                    ));
                }
                let allow_hs256_fallback = env_bool("JWT_ALLOW_HS256_FALLBACK", true)?;
                // secret 在 HS256 fallback=true 时仍必填；false 时允许省略。
                let secret = if allow_hs256_fallback {
                    env_required("JWT_SECRET").context(
                        "JWT_ALLOW_HS256_FALLBACK=true 时 JWT_SECRET 必填（HS256 fallback 用）",
                    )?
                } else {
                    env::var("JWT_SECRET").unwrap_or_default()
                };
                JwtConfig {
                    secret,
                    issuer: env_or("JWT_ISSUER", "myerp"),
                    audience: env_or("JWT_AUDIENCE", "hsh-erp-rust"),
                    access_ttl_seconds: env_parse("JWT_ACCESS_TOKEN_EXPIRE_SECONDS", 900)?,
                    refresh_ttl_days: env_parse("JWT_REFRESH_TOKEN_EXPIRE_DAYS", 7)?,
                    signing_kid,
                    private_key,
                    public_keys,
                    allow_hs256_fallback,
                }
            },

            cos: {
                // 2026-09-11 修改：先读 COS_ENABLED；disabled 时 COS_SECRET_* 不强制要求，
                // 占位空串即可（NoopCos / NoopOpenDal 不读凭据）。
                let cos_enabled = env_bool("COS_ENABLED", true)?;
                // 2026-09-20 迁移清理：env `COS_BACKEND` 仅支持 `opendal` / `noop`，
                // 缺省 `opendal`（迁移默认从 `cos_sdk` 切到 `opendal`，与 spike 验证
                // 结论一致）。非法值（如历史 `.env` 残留的 `cos_sdk`）→ 解析失败并
                // 报错（不静默 fallback 到 `opendal`），便于 ops 显式确认旧配置已迁。
                //
                // 强制规则（2026-09-20 迁移新增）：
                //   - COS_ENABLED=false → backend 强制 Noop（不论 env 怎么设），便于
                //     「关掉 COS」的本地 cargo run / 集成测场景走最简路径
                //   - COS_ENABLED=true + COS_BACKEND 未设 → 默认 OpenDal（生产推荐）
                let backend_raw = std::env::var("COS_BACKEND").ok();
                let backend = if !cos_enabled {
                    CosBackend::Noop
                } else {
                    match backend_raw
                        .unwrap_or_else(|| "opendal".to_string())
                        .to_ascii_lowercase()
                        .as_str()
                    {
                        "opendal" => CosBackend::OpenDal,
                        "noop" => CosBackend::Noop,
                        other => {
                            return Err(anyhow!(
                                "环境变量 COS_BACKEND 无法解析为合法 backend: {other:?} \
                                 （仅支持 `opendal` / `noop`；2026-09-20 迁移清理后 \
                                 删除 `cos_sdk` 选项）"
                            ));
                        }
                    }
                };
                let (secret_id, secret_key) = if cos_enabled && backend != CosBackend::Noop {
                    (
                        env_required("COS_SECRET_ID")?,
                        env_required("COS_SECRET_KEY")?,
                    )
                } else {
                    (String::new(), String::new())
                };
                CosConfig {
                    backend,
                    enabled: cos_enabled,
                    region: env_or("COS_REGION", "ap-shanghai"),
                    bucket: env_required("COS_BUCKET")?,
                    secret_id,
                    secret_key,
                    app_id: env_or("COS_APP_ID", ""),
                    endpoint: env_or("COS_ENDPOINT", ""),
                    scheme: env_or("COS_SCHEME", "https"),
                    upload_prefix: env_or("COS_UPLOAD_PREFIX", "uploads"),
                    presign_expire_seconds: env_parse("COS_PRESIGN_EXPIRE", 3600)?,
                    max_file_size: env_parse("COS_MAX_FILE_SIZE", 300 * 1024 * 1024)?,
                    // 2026-09-20 迁移：删 `sts_duration_seconds` 字段（spike 已记
                    // 「未来清理」）；STS 链路完全走 `UploadSessionConfig::sts_duration_seconds`
                    // + python 后端转发，与 COS 对象存储解耦。
                    tmp_prefix: env_or("COS_TMP_PREFIX", "tmp/"),
                }
            },

            snowflake: SnowflakeConfig {
                instance: env_parse("SNOWFLAKE_INSTANCE", 0)?,
                epoch_ms: env_parse("SNOWFLAKE_EPOCH", 1_735_689_600_000u64)?, // 2025-01-01 UTC（与 Python 配置默认一致）
            },

            auto_complete: AutoCompleteConfig {
                threshold_days: env_parse("AUTO_COMPLETE_THRESHOLD_DAYS", 7)?,
                interval_hours: env_parse("AUTO_COMPLETE_INTERVAL_HOURS", 24)?,
            },

            redis: RedisConfig {
                url: build_redis_url(),
                // 默认 15min（900s），可通过 JWT_ACCESS_TOKEN_EXPIRE_SECONDS 覆盖
                // Redis 滑动 TTL 在 extractor 中 EXPIRE 续期
                session_ttl_seconds: env_parse("REDIS_SESSION_TTL_SECONDS", 900u64)?,
                pool_max_size: env_parse("REDIS_POOL_MAX_SIZE", 10usize)?,
            },
            delivery_note_template_dir: PathBuf::from(env_or(
                "DELIVERY_NOTE_TEMPLATE_DIR",
                concat!(env!("CARGO_MANIFEST_DIR"), "/template"),
            )),
            // 2026-09-14 新增：_e2e 路由门控。
            // 单一控制点 = env `E2E_HOOKS_ENABLED`（缺省 true）。docker compose / dev `cargo run`
            // 走默认（启用）；prod / staging 必须显式 `E2E_HOOKS_ENABLED=false`（ops 责任）。
            // 2026-09-14 修复 Bug #1：移除 main.rs 原 release profile 二次硬关。
            enable_e2e_hooks: env_bool("E2E_HOOKS_ENABLED", true)?,
            // 2026-09-15 followup-cleanup A5/A6：dashboard WS 心跳间隔（秒）；生产 30，测试可调小。
            ws_heartbeat_interval_seconds: env_parse("WS_HEARTBEAT_INTERVAL_SECONDS", 30u64)?,
            // 2026-09-20 新增：HTTP nest 请求超时；与 WS 隔离（挂在内层）。
            request_timeout_seconds: env_parse("REQUEST_TIMEOUT_SECONDS", 30u64)?,
            // 2026-09-18 新增：upload_session 域配置
            upload_session: UploadSessionConfig {
                python_backend_base_url: env_or("PYTHON_BACKEND_BASE_URL", "http://backend:8000"),
                ttl_seconds: env_parse("UPLOAD_SESSION_TTL_SECONDS", 86_400u64)?,
                sts_duration_seconds: env_parse("UPLOAD_SESSION_STS_DURATION_SECONDS", 7_200u32)?,
                renew_threshold_seconds: env_parse(
                    "UPLOAD_SESSION_RENEW_THRESHOLD_SECONDS",
                    600i64,
                )?,
            },
        })
    }
}

/// 从环境变量构建 PostgreSQL 连接 URL（开发库）
fn build_database_url() -> Result<String> {
    // 优先使用完整的 DATABASE_URL（兼容旧方式）
    if let Ok(url) = env::var("DATABASE_URL") {
        return Ok(url);
    }

    // 否则从拆分变量构建
    let host = env_or("POSTGRES_HOST", "localhost");
    let port = env_or("POSTGRES_PORT", "5432");
    let user = env_required("POSTGRES_USER")?;
    let password = env_required("POSTGRES_PASSWORD")?;
    let db = env_required("POSTGRES_DB")?;

    Ok(format!("postgresql://{user}:{password}@{host}:{port}/{db}"))
}

/// 从环境变量构建 PostgreSQL 连接 URL（测试库）
pub fn build_test_database_url() -> Result<String> {
    // 优先使用完整的 DATABASE_TEST_URL（兼容完整 URL 方式）
    if let Ok(url) = env::var("DATABASE_TEST_URL") {
        return Ok(url);
    }

    // 否则从测试库拆分变量构建
    let host = env_or("POSTGRES_TEST_HOST", "localhost");
    let port = env_or("POSTGRES_TEST_PORT", "5429");
    let user = env_required("POSTGRES_TEST_USER")?;
    let password = env_required("POSTGRES_TEST_PASSWORD")?;
    let db = env_required("POSTGRES_TEST_DB")?;

    Ok(format!("postgresql://{user}:{password}@{host}:{port}/{db}"))
}

/// 从环境变量构建 Redis 连接 URL（session store）
///
/// 两层回退：优先 `REDIS_URL`（含密码 / db index），否则按 `REDIS_HOST/PORT/DB/PASSWORD`
/// 拼接（dev/test 默认即可）。注意测试容器走 `redis://localhost:6380/15`（与
/// dev 的 db 0 隔离）。
pub fn build_redis_url() -> String {
    if let Ok(url) = env::var("REDIS_URL") {
        return url;
    }

    let host = env_or("REDIS_HOST", "localhost");
    let port = env_or("REDIS_PORT", "6379");
    let db = env_or("REDIS_DB", "0");
    let password = env::var("REDIS_PASSWORD").ok();

    match password.as_deref() {
        Some(pw) if !pw.is_empty() => format!("redis://:{pw}@{host}:{port}/{db}"),
        _ => format!("redis://{host}:{port}/{db}"),
    }
}

fn env_or(key: &str, default: &str) -> String {
    env::var(key).unwrap_or_else(|_| default.to_string())
}

/// 从 PEM 文件加载 RS256 私钥 → `jsonwebtoken::EncodingKey`。
///
/// 2026-09-23 重构：JWT_PRIVATE_KEY_PATH 指向单个 PEM 文件（PKCS#8 / PKCS#1
/// 都可，jsonwebtoken 内部自动识别）。
fn load_private_key(path: &str) -> Result<EncodingKey> {
    let pem_bytes = fs::read(path)
        .with_context(|| format!("读取文件失败 {path}（确认 JWT_PRIVATE_KEY_PATH 路径正确）"))?;
    EncodingKey::from_rsa_pem(&pem_bytes)
        .with_context(|| format!("PEM 解析失败 {path}（确认是 RS256 私钥 PKCS#8 / PKCS#1 格式）"))
}

/// 扫描 `JWT_PUBLIC_KEYS_DIR` 目录所有 `*.pem` 文件，构建 `BTreeMap<kid, DecodingKey>`。
///
/// kid = 文件名去后缀（例：`/keys/public/current.pem` → kid = `"current"`）。
/// 同目录 kid 必须唯一（重复 → bail）。
/// 目录不存在或非目录 → bail；空目录 → 启动失败（必须有公钥才能验签）。
fn load_public_keys_dir(dir: &str) -> Result<BTreeMap<String, DecodingKey>> {
    let dir_path = Path::new(dir);
    if !dir_path.is_dir() {
        return Err(anyhow!(
            "JWT_PUBLIC_KEYS_DIR 指向的路径不是目录或不存在: {dir}"
        ));
    }
    let mut map: BTreeMap<String, DecodingKey> = BTreeMap::new();
    for entry in fs::read_dir(dir_path)
        .with_context(|| format!("读取目录失败 {dir}（确认 JWT_PUBLIC_KEYS_DIR 可访问）"))?
    {
        let entry = entry.with_context(|| format!("读取目录项失败 {dir}"))?;
        let path = entry.path();
        // 只处理 *.pem（大小写不敏感；linux fs 默认大小写敏感，这里只匹配 .pem / .PEM）
        let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
            continue;
        };
        if !ext.eq_ignore_ascii_case("pem") {
            continue;
        }
        // 文件名去后缀 = kid
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if stem.is_empty() {
            continue;
        }
        if map.contains_key(stem) {
            return Err(anyhow!(
                "JWT_PUBLIC_KEYS_DIR={dir} 含重复 kid={stem:?}（文件名去后缀必须唯一）"
            ));
        }
        let pem_bytes = fs::read(&path)
            .with_context(|| format!("读取 PEM 文件失败 {}", path.display()))?;
        let key = DecodingKey::from_rsa_pem(&pem_bytes).with_context(|| {
            format!(
                "PEM 解析失败 {}（确认是 RS256 公钥 SPKI 格式）",
                path.display()
            )
        })?;
        map.insert(stem.to_string(), key);
    }
    if map.is_empty() {
        return Err(anyhow!(
            "JWT_PUBLIC_KEYS_DIR={dir} 目录无 *.pem 公钥文件（至少 1 枚才能验签）"
        ));
    }
    Ok(map)
}

fn env_required(key: &str) -> Result<String> {
    env::var(key).with_context(|| format!("缺少环境变量 {key}"))
}

fn env_parse<T: std::str::FromStr>(key: &str, default: T) -> Result<T> {
    match env::var(key) {
        Ok(s) => s
            .parse()
            .map_err(|_| anyhow!("环境变量 {key} 解析失败：{s}")),
        Err(_) => Ok(default),
    }
}

/// 从环境变量读 bool。接受 "true"/"false"/"1"/"0"（大小写不敏感）；
/// 缺省返回 `default`；其余值 → anyhow 错误。
fn env_bool(key: &str, default: bool) -> Result<bool> {
    match env::var(key) {
        Ok(s) => match s.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" | "on" => Ok(true),
            "false" | "0" | "no" | "off" => Ok(false),
            other => Err(anyhow!("环境变量 {key} 无法解析为 bool: {other:?}")),
        },
        Err(_) => Ok(default),
    }
}

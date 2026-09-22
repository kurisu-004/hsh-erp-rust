//! 应用配置：dotenvy 加载 .env 后从 std::env 读取
//! 对应 Python myERP/core/config.py

use std::env;
use std::path::PathBuf;

use anyhow::{Context, Result, anyhow};

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
    /// session 条目 TTL（秒），对应 JWT `access_ttl_hours` 的预期寿命；滑动窗口
    pub session_ttl_seconds: u64,
    /// 连接池上限
    pub pool_max_size: usize,
    /// 是否在 extractor 中校验 Redis 服务端 session。
    /// 关掉后，main.rs 不建连接池；所有 session 写入走 no-op store；
    /// 适用于 Rust 借 Python JWT 的迁移过渡期。
    /// 环境变量 `REDIS_SESSION_CHECK_ENABLED`，缺省 `true`。
    pub session_check_enabled: bool,
}

#[derive(Clone, Debug)]
pub struct JwtConfig {
    pub secret: String,
    pub issuer: String,
    /// JWT `aud` 校验目标（2026-09-22 新增：删 Python v1 兼容后 Rust 自签 token 强绑定 audience）。
    /// 环境变量 `JWT_AUDIENCE`，缺省 `hsh-erp-rust`。
    pub audience: String,
    pub access_ttl_hours: i64,
    pub refresh_ttl_days: i64,
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

            jwt: JwtConfig {
                secret: env_required("JWT_SECRET")?,
                issuer: env_or("JWT_ISSUER", "myerp"),
                // 2026-09-22 新增：audience 强校验（删 Python v1 兼容后改回硬绑定）。
                audience: env_or("JWT_AUDIENCE", "hsh-erp-rust"),
                access_ttl_hours: env_parse("JWT_ACCESS_TOKEN_EXPIRE_HOURS", 12)?,
                refresh_ttl_days: env_parse("JWT_REFRESH_TOKEN_EXPIRE_DAYS", 7)?,
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
                // 默认 12h，对齐 JWT_ACCESS_TOKEN_EXPIRE_HOURS=24 的常见一半；
                // Redis 滑动 TTL 在 extractor 中 EXPIRE 续期
                session_ttl_seconds: env_parse("REDIS_SESSION_TTL_SECONDS", 43_200u64)?,
                pool_max_size: env_parse("REDIS_POOL_MAX_SIZE", 10usize)?,
                session_check_enabled: env_bool("REDIS_SESSION_CHECK_ENABLED", true)?,
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

//! 应用配置：dotenvy 加载 .env 后从 std::env 读取
//! 对应 Python myERP/core/config.py

use std::env;
use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};

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
    pub access_ttl_hours: i64,
    pub refresh_ttl_days: i64,
}

#[derive(Clone, Debug)]
pub struct CosConfig {
    pub region: String,
    pub bucket: String,
    pub secret_id: String,
    pub secret_key: String,
    pub scheme: String,
    pub upload_prefix: String,
    pub presign_expire_seconds: u32,
    pub max_file_size: usize,
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
                access_ttl_hours: env_parse("JWT_ACCESS_TOKEN_EXPIRE_HOURS", 12)?,
                refresh_ttl_days: env_parse("JWT_REFRESH_TOKEN_EXPIRE_DAYS", 7)?,
            },

            cos: CosConfig {
                region: env_or("COS_REGION", "ap-shanghai"),
                bucket: env_required("COS_BUCKET")?,
                secret_id: env_required("COS_SECRET_ID")?,
                secret_key: env_required("COS_SECRET_KEY")?,
                scheme: env_or("COS_SCHEME", "https"),
                upload_prefix: env_or("COS_UPLOAD_PREFIX", "uploads"),
                presign_expire_seconds: env_parse("COS_PRESIGN_EXPIRE", 3600)?,
                max_file_size: env_parse("COS_MAX_FILE_SIZE", 300 * 1024 * 1024)?,
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
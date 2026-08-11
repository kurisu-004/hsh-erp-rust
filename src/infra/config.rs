//! 应用配置：dotenvy 加载 .env 后从 std::env 读取
//! 对应 Python myERP/core/config.py

use std::env;

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

#[derive(Copy, Clone, Debug)]
pub struct SnowflakeConfig {
    /// 数据中心 ID（原 SNOWFLAKE_INSTANCE）
    pub instance: u16,
    /// 工作机器 ID（原 SNOWFLAKE_SEQ）
    pub seq: u16,
    /// 自定义纪元（毫秒），原 SNOWFLAKE_EPOCH
    pub epoch_ms: u64,
}

#[derive(Copy, Clone, Debug)]
pub struct AutoCompleteConfig {
    pub threshold_days: u32,
    pub interval_hours: u64,
}

impl AppConfig {
    pub fn from_env() -> Result<Self> {
        // 加载 .env（不存在不报错）
        let _ = dotenvy::dotenv();

        Ok(Self {
            database_url: env_required("DATABASE_URL")?,
            listen_addr: env_or("LISTEN_ADDR", "0.0.0.0:3000"),
            max_request_body_size: env_parse("MAX_REQUEST_BODY_SIZE", 300 * 1024 * 1024)?,

            jwt: JwtConfig {
                secret: env_required("JWT_SECRET")?,
                issuer: env_or("JWT_ISSUER", "hsh-erp"),
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
                instance: env_parse("SNOWFLAKE_INSTANCE", 1)?,
                seq: env_parse("SNOWFLAKE_SEQ", 1)?,
                epoch_ms: env_parse("SNOWFLAKE_EPOCH", 1_577_836_800_000u64)?, // 2020-01-01 UTC
            },

            auto_complete: AutoCompleteConfig {
                threshold_days: env_parse("AUTO_COMPLETE_THRESHOLD_DAYS", 7)?,
                interval_hours: env_parse("AUTO_COMPLETE_INTERVAL_HOURS", 24)?,
            },
        })
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
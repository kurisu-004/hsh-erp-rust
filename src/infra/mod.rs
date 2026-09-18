//! 外部资源与横切基建封装
//!
//! 对应 Python myERP/core/ 下除 security/permission 之外的 infra 模块：
//! - `config`：应用配置（dotenvy + env 读取）
//! - `db`：sqlx PgPool 构建
//! - `cos`：腾讯云 COS 抽象（trait + NoopCos 占位）
//! - `snowflake`：分布式雪花 ID 生成器
//! - `clock`：Asia/Shanghai 时区工具
//! - `serial`：业务单号/序列号计数（占位）
//! - `ws_hub`：WebSocket 广播中枢
//! - `redis`：Redis 连接池构建（deadpool-redis 0.23，session store 用）
//! - `sts`：STS 临时凭证签发（M2-A 旧 `TencentSts` 直连腾讯云；2026-09-18 已删除，
//!   改为 `python_sts` 通过 HTTP 转发到 python 后端）

pub mod clock;
pub mod config;
pub mod cos;
pub mod db;
pub mod python_sts; // 2026-09-18 新增：转发 python 后端签发 STS
pub mod redis;
pub mod serial;
pub mod snowflake;
pub mod ws_hub;

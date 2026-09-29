//! 外部资源与横切基建封装
//!
//! 对应 Python myERP/core/ 下除 security/permission 之外的 infra 模块：
//! - `config`：应用配置（dotenvy + env 读取）
//! - `db`：sqlx PgPool 构建
//! - `seed`：seeds/ 目录声明式种子加载（2026-09-25 sqlx 接管新增）
//! - `cos`：腾讯云 COS 抽象（trait + NoopCos 占位）
//! - `cos_opendal`：Apache OpenDAL S3 backend 实现的 COS 客户端（2026-09-20 spike）
//! - `snowflake`：分布式雪花 ID 生成器
//! - `clock`：Asia/Shanghai 时区工具
//! - `serial`：业务单号/序列号计数（占位）
//! - `ws_hub`：WebSocket 广播中枢
//! - `redis`：Redis 连接池构建（deadpool-redis 0.23，session store 用）
//!
//! 2026-09-28 删除 STS 转发相关模块（转发器与 sts NoopSts 占位）：
//! 相关上传会话域已下线，前端改为单 uploader 触发时单 HTTP 调用 python `sts-tmp-keys`
//! 数组入参直签，rust 后端不再代为转发 STS 凭证。

pub mod clock;
pub mod config;
pub mod cos;
pub mod cos_opendal; // 2026-09-20 spike：OpenDAL S3 backend 替代 cos-rust-sdk 可行性验证
pub mod db;
// 2026-09-28 新增：rust → python 后端转发 HTTP 客户端抽象（薄壳鉴权转发到 python STS 端点）。
pub mod py_backend;
pub mod redis;
pub mod seed; // 2026-09-25 新增：菜单等配置数据声明式种子
pub mod serial;
pub mod snowflake;
pub mod ws_hub;

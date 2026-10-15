//! 外部资源与横切基建封装
//!
//! 对应 Python myERP/core/ 下除 security/permission 之外的 infra 模块：
//! - `config`：应用配置（dotenvy + env 读取）
//! - `crypto`：AES-256-GCM 对称加解密（2026-10-16 新增，邮箱授权码加密入库用）
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
//! 2026-10-15：`py_backend` 的 STS 转发方法（`forward_sts_tmp_keys`）随
//! `POST /api/v2/files/sts-tmp-keys` 端点一并删除 —— 预签 PUT 由
//! `POST /files/objects/upload-intents` 在本仓内直接签发（OpenDAL SigV4），
//! 不再向 python 取临时凭证。该模块剩余的出口是零件图纸打印与 CI 预览转发。

pub mod clock;
pub mod config;
pub mod cos;
pub mod cos_opendal; // 2026-09-20 spike：OpenDAL S3 backend 替代 cos-rust-sdk 可行性验证
pub mod crypto; // 2026-10-16 新增：邮箱授权码加密（AES-256-GCM）
pub mod db;
// 2026-09-28 新增：rust → python 后端转发 HTTP 客户端抽象（薄壳鉴权转发到 python STS 端点）。
pub mod py_backend;
pub mod redis;
pub mod seed; // 2026-09-25 新增：菜单等配置数据声明式种子
pub mod serial;
pub mod snowflake;
pub mod ws_hub;

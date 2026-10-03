//! 应用配置：dotenvy 加载 .env 后从 std::env 读取
//! 对应 Python myERP/core/config.py

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::Path;

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
    /// 2026-10-01 新增：服务端 protocol-level `Ping` 间隔（秒）。与上面的 text 心跳
    /// **职责分离、两者并存**（勿合并）：
    /// - text 心跳帧 `{"type":"heartbeat",ts}` → 浏览器 JS `onmessage` 收得到，给**前端**感知用；
    /// - `Message::Ping` → 浏览器按 RFC 6455 §5.5.2 在**协议栈**自动回 `Pong`（JS 完全不可见），
    ///   给**服务端**存活检测用（`ws_pong_timeout_seconds` 靠它续命）。
    ///
    /// 缺这个的原因：`sender.send()` 一个几十字节的心跳帧**不会报错**（TCP 写只要内核
    /// 发送缓冲收下就返回 Ok），写成功不能证明对端活着——可能要等缓冲写满触发 EPIPE
    /// 才现形，可能是几小时；而 `TimeoutLayer` 只挂在 `/api/v2` nest 内，碰不到 `/ws`。
    ///
    /// 环境变量 `WS_PING_INTERVAL_SECONDS`，缺省 `20`。
    pub ws_ping_interval_seconds: u64,
    /// 2026-10-01 新增：`Pong` 超时阈值（秒）。超过此时长**没收到任何入站帧**即判定对端
    /// 已死 → 发 `1011 pong timeout` Close 帧并断开（清理半开 TCP 连接 / 死标签页）。
    /// 环境变量 `WS_PONG_TIMEOUT_SECONDS`，缺省 `60`（= 3× ping 间隔，容忍连续丢 2 次）。
    ///
    /// 2026-10-02 修复（review 第 1 轮 Major-2）：启动期强制校验由 `> ping_interval`
    /// 收紧为 **`>= 2 × ping_interval`**（见 `validate_ws_liveness`）。原校验只挡
    /// `pong_timeout <= ping_interval`，于是 `ping=20 / pong=21` 这种只有 **1s 余量**
    /// 的配置能通过启动校验，而 1s 余量在 RTT + tokio 调度抖动下极易被击穿 → 健康连接
    /// 被误判为已死。2× 是「连续丢 1 次 Pong 仍不判死」的下限；缺省 3×（60/20）。
    pub ws_pong_timeout_seconds: u64,
    /// 2026-10-02 新增：每 N 次 text 心跳做一次周期性 re-auth（发 `4001` 的前置闸）。
    ///
    /// 为什么做成配置项而不是 `const`：原 `WS_REAUTH_EVERY_N_HEARTBEATS` 是硬编码
    /// `const 10`，而测试配置的 text 心跳是 1s → 触发一次要跑 >10s，CI 上根本没法
    /// 验证「re-auth 失败 → 4001」这条**安全核心路径**（review 第 1 轮 Minor 7）。
    /// 可注入后 E2E 用例传 `2`，两秒内即可验到。
    ///
    /// 环境变量 `WS_REAUTH_EVERY_N_HEARTBEATS`，缺省 `10`（生产 30s × 10 ≈ 5min）。
    /// 启动期强制校验 `>= 1`（0 ⇒ re-auth 静默永不触发，见 `ws_reauth_config`）。
    pub ws_reauth_every_n_heartbeats: u32,
    /// 2026-09-20 新增：HTTP `/api/v2/*` nest 请求超时（秒）。仅挂在 nest 内层
    /// （不影响 WS 长连接，也不影响根 Router 的 CORS/Body limit）。环境变量
    /// `REQUEST_TIMEOUT_SECONDS`，缺省 `30`。
    pub request_timeout_seconds: u64,
    /// 2026-10-03 新增：打印路径 HTTP 请求超时（秒），缺省 `660`。与上面的通用档
    /// 分档并存——`middleware::timeout` 按 [`crate::middleware::timeout::is_print_path`]
    /// 判定：打印路径走本档，其余路径仍走 `request_timeout_seconds`（30s 不变）。
    ///
    /// ## 为什么缺省 660 而不是 600（多出的 60s 是有意的边际）
    /// 批量图纸打印（20 件/批）由 Python 端执行，Python 侧超时是 600s
    /// （`python_backend.print_timeout_ms`）。Rust 自己的超时必须**严格大于**
    /// Python 的超时：两者同时到点时，先被杀的是 Rust，Python 侧真实的超时 / 上游
    /// 错误就再也浮不上来，用户只会看到一句「请求超时」而看不到真正的原因。留
    /// 60s（10%）边际后，Python 侧的 502 会先于 Rust 的 408 抵达前端，定位到的是
    /// 准确原因而不是超时表象。
    ///
    /// 环境变量 `PRINT_REQUEST_TIMEOUT_SECONDS`，缺省 660。
    pub print_request_timeout_seconds: u64,
    /// 2026-09-23 新增 Idempotency 中间件 TTL（秒）：POST/PUT/PATCH 带
    /// `Idempotency-Key` header 的请求，缓存响应在 Redis 中的过期时间。
    /// 环境变量 `IDEMPOTENCY_TTL_SECONDS`，缺省 `86400`（24h）。
    pub idempotency_ttl_seconds: u64,
    /// 2026-09-26 新增：是否在启动钩子里应用 `seeds/admin.sql`（初始管理员账号）。
    /// 环境变量 `BOOTSTRAP_ADMIN_ENABLED`，缺省 `false`（生产安全默认）。
    /// 启用后必须立刻登录 admin/changeme、改密、设回 `false`、重启；详见
    /// `src/infra/seed.rs` 模块 doc + `seeds/README.md`。
    pub bootstrap_admin_enabled: bool,
    /// 2026-09-28 新增：rust → python 后端转发配置（薄壳鉴权转发到 python STS 端点）。
    /// 环境变量 `PYTHON_BACKEND_BASE_URL`（设了就 enabled=true）/ `PYTHON_STS_TIMEOUT_MS`。
    pub python_backend: PythonBackendConfig,
    /// 2026-09-29 新增：企业微信小程序登录配置（自建应用 `jscode2session`）。
    /// 环境变量 `WECOM_CORPID` / `WECOM_CORPSECRET`（两者都非空才 enabled）。
    pub wecom: WeComConfig,
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
    /// COS 桶内上传前缀（保留兼容 .env 旧值）。
    ///
    /// 2026-09-29 扁平化：CAS key 模板已从五段简化为两段
    /// `{prefix}{sha16}_{safe_filename}`，新模板的 prefix 仅在新 key 写入路径
    /// (`util::cos_key::build_cas_key`) 使用一次。后续会切到 bin 迁移历史 DB
    /// 行的 object_key 后彻底删除（迁移 bin 见 §cos_key_migrate.rs）。
    ///
    /// 当前阶段：保留 `upload_prefix` 字段以兼容 .env / tests 的 `uploads` 或
    /// `uploads/` 旧值不报错，标记 `dead_code` 允许编译器跳过 unused 警告。
    #[allow(dead_code)]
    pub upload_prefix: String,
    pub presign_expire_seconds: u32,
    pub max_file_size: usize,
    /// COS 临时对象 prefix 模板前缀（默认 `tmp/`，含尾斜杠）。可用于多种场景：
    /// - confirm handler 校验 `tmp_key` 必须以此前缀开头
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

// 2026-09-28 删除：相关上传会话域配置结构体（域整体下线）。

/// 2026-10-03 新增：Python 端**打印**执行超时缺省值（毫秒）= 10 分钟。
///
/// 抽成 const 是为了让「Rust HTTP 档必须比它多 60s 边际」这条不变量能被单测直接
/// 断言（见 `tests::print_timeout_defaults_leave_headroom_over_python`），而不必去
/// 读进程级 env（本仓 `env_parse` 读全局 env，同进程并行单测会互相覆盖）。
const PYTHON_PRINT_TIMEOUT_MS_DEFAULT: u64 = 600_000;

/// 2026-10-03 新增：Rust 端**打印路径** HTTP 超时缺省值（秒）= 600s + 60s 边际。
///
/// 为什么是 660 而不是 600：打印真正在 Python 端执行，Python 侧超时 600s。Rust
/// 自己的超时若与它同时到点，先被杀的是 Rust，Python 真实的超时 / 上游错误再也
/// 浮不上来。多留 60s 让 Python 侧的 502 先于 Rust 的 408 抵达前端，暴露准确原因。
const PRINT_REQUEST_TIMEOUT_SECONDS_DEFAULT: u64 = 660;

/// 2026-09-28 新增：rust → python 后端转发配置（薄壳鉴权转发专用）。
///
/// ## 触发场景
/// `POST /api/v2/files/sts-tmp-keys` 强制 JWT 鉴权后，透明转发到 python 后端
/// `POST /api/v1/files/sts-tmp-keys`（python 端**继续裸开** by design +
/// 部署层隔离）；rust 端是新的强制鉴权点。
///
/// ## env
/// - `PYTHON_BACKEND_BASE_URL`：python 后端 base URL（如 `http://backend:8000`）；
///   设置即 `enabled=true`，未设置走 `NoopPyBackend`（本地 `cargo run` 不依赖 python）。
/// - `PYTHON_STS_TIMEOUT_MS`：单次转发请求超时，缺省 `10_000`（10s）。
/// - `PYTHON_PRINT_TIMEOUT_MS`（2026-10-03 新增）：打印转发专用超时，缺省 `600_000`
///   （10 分钟）——打印在 Python 端执行，不能与 STS 的 10s 通道同档。
///
/// ## 与原 `infra::python_sts::PythonStsConfig` 的区别
/// 原 STS 配置随 `upload_session` 域下线已删除；本配置是新的「rust 鉴权后
/// 转发到 python」场景，含义更窄、timeout 也对齐 `HttpPyBackend::timeout`。
#[derive(Clone, Debug)]
pub struct PythonBackendConfig {
    /// python 后端 base URL（如 `http://backend:8000`）。空字符串或未设 → 走 `NoopPyBackend`。
    pub base_url: String,
    /// 单次转发请求超时（毫秒），传给 `reqwest::Client::timeout`。
    pub timeout_ms: u64,
    /// 2026-10-03 新增：**打印**转发专用超时（毫秒），缺省 `600_000`（10 分钟）。
    /// 打印请求由 Python 端真正执行（批量图纸打印 20 件/批，合法耗时数分钟），
    /// 不能与 STS 那条 10s 通道共用同一档；且换算成秒后必须严格小于
    /// [`AppConfig::print_request_timeout_seconds`]（600s < 660s），否则先到点的是
    /// Rust，Python 的真实错误被 408 掩盖。
    /// 环境变量 `PYTHON_PRINT_TIMEOUT_MS`。
    pub print_timeout_ms: u64,
    /// 是否启用真实转发。`false` → `NoopPyBackend`（本地 cargo run 不依赖 python）。
    pub enabled: bool,
}

impl Default for PythonBackendConfig {
    fn default() -> Self {
        // 与旧 `infra::python_sts::PythonStsConfig::default()` 区分：base_url 默认
        // 是 dev 本机端口而非 compose 内 backend:8000，便于 `cargo run` 在无 compose
        // 环境也能跑（仍走 NoopPyBackend；只是参数一致）。
        Self {
            base_url: "http://localhost:8000".to_string(),
            timeout_ms: 10_000,
            print_timeout_ms: PYTHON_PRINT_TIMEOUT_MS_DEFAULT,
            enabled: false,
        }
    }
}

/// 2026-09-29 新增：企业微信小程序登录（自建应用 `jscode2session`）配置。
///
/// ## 触发场景
/// `POST /api/v2/wx/iam/wx-login` 用小程序 `wx.login()` 拿到的**一次性 code**
/// 换企业微信 `userid`，再按 `t_wx_identity` 预绑定表反查系统账号。
/// 走企业微信的 `/cgi-bin/miniprogram/jscode2session`（**不是**微信的
/// `api.weixin.qq.com/sns/jscode2session`），因此 `access_token` 必须用
/// **该小程序关联的企业微信自建应用**的 Secret 换取，用**企业的 corpid**
/// （不是小程序 appid）。
///
/// ## env
/// - `WECOM_CORPID`：企业微信管理后台「我的企业 → 企业信息 → 企业ID」。
/// - `WECOM_CORPSECRET`：「应用管理 → 小程序」下已关联的**自建应用**的 Secret。
///   第三方应用会返回加密 userid（需 `suite_access_token` + `auth/getuserinfo3rd`），
///   本方案不适用。
/// - `WECOM_API_BASE`：企微 API 根地址，缺省 `https://qyapi.weixin.qq.com`
///   （单测指向本地 mock server）。
/// - `WECOM_TOKEN_TTL`：access_token 的 Redis 缓存 TTL（秒）**兜底值**，缺省 `6000`。
///   正常路径用企微 `gettoken` 返回的 `expires_in - 300`（= 6900），只有响应里
///   **没有** `expires_in` 字段时才回落到这个值（见 `wecom_client::get_token`）。
/// - `WECOM_HTTP_TIMEOUT_MS`：单次企微 HTTP 请求超时（毫秒），缺省 `5000`。
///
/// ## 为什么用 `env_or(.., "")` 而**不是** `env_required`
/// 降级范式对齐既有 [`PythonBackendConfig`]：未配置时 `enabled=false`，
/// 后端**照常启动**，只有 `wx-login` 端点返回 `40109 BIZ_WX_NOT_CONFIGURED`。
/// 若用 `env_required`，运维在还没拿到 corpsecret 之前就无法启动服务（fail-fast
/// 变成 fail-all）。更关键的反例：若为了「跑得起来」而在占位位置填一个**假
/// secret**，`enabled` 判定只看「非空」就会被判为 true，此后每次登录都会
/// 真打企微接口并拿到 `errcode=40001 invalid credential`，最终抛出一个
/// 40106/40109 都无法直接指向根因的 40106，排查成本极高。**留空**才是正确
/// 降级：未配置时 40109 一眼看出是配置问题。
///
/// ## 安全
/// `corpsecret` 绝不出现在任何 `tracing` 日志 / `Debug` 输出里。
///
/// 2026-09-29 加固（review 第 1 轮 B4）：原先 `#[derive(Debug)]` 让 `{:?}` 会把
/// `corpsecret` 一起打出去，安全只靠「调用方记得别打印整个 struct」这条**约定**。
/// 改为手写 `Debug`（见下方 impl）把 `corpsecret` 固定输出成 `***`——不靠约定、
/// 靠类型。下游 `info!(?cfg)` 之类的排障用法仍可看到 corpid / enabled / api_base。
#[derive(Clone)]
pub struct WeComConfig {
    /// 企业 ID。空串 = 未配置。
    pub corpid: String,
    /// 自建应用 Secret。空串 = 未配置。**禁止**进日志。
    pub corpsecret: String,
    /// 企微 API 根地址（无尾斜杠）。
    pub api_base: String,
    /// access_token 缓存 TTL（秒）。
    pub token_ttl_seconds: u64,
    /// 单次 HTTP 超时（毫秒）。
    pub http_timeout_ms: u64,
    /// 是否启用真实企微调用。`corpid` 与 `corpsecret` **均非空**才 true。
    pub enabled: bool,
}

/// 手写 `Debug`：屏蔽 `corpsecret`（2026-09-29，review 第 1 轮 B4）。
///
/// 与 `serde` 无关——这里只管 `{:?}` / `{cfg:?}`。字段顺序与声明顺序一致，
/// 便于和 derive 版本对照阅读。
impl std::fmt::Debug for WeComConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WeComConfig")
            .field("corpid", &self.corpid)
            // ⚠️ 永远输出 `***`，即使 corpsecret 为空串也不回显原值
            .field("corpsecret", &"***")
            .field("api_base", &self.api_base)
            .field("token_ttl_seconds", &self.token_ttl_seconds)
            .field("http_timeout_ms", &self.http_timeout_ms)
            .field("enabled", &self.enabled)
            .finish()
    }
}

impl Default for WeComConfig {
    fn default() -> Self {
        Self {
            corpid: String::new(),
            corpsecret: String::new(),
            api_base: "https://qyapi.weixin.qq.com".to_string(),
            token_ttl_seconds: 6000,
            http_timeout_ms: 5000,
            enabled: false,
        }
    }
}

impl AppConfig {
    pub fn from_env(env_file: &str) -> Result<Self> {
        // 加载 .env（不存在不报错）
        let _ = dotenvy::from_filename(env_file);

        // 2026-10-01 新增：WS 存活检测两个参数**成对解析 + 成对校验**。提前于 struct
        // literal 是因为 `pong_timeout >= 2 × ping_interval` 是跨字段约束，struct literal
        // 的字段之间无法互相引用。
        let (ws_ping_interval_seconds, ws_pong_timeout_seconds) = ws_liveness_config()?;
        // 2026-10-02 新增：re-auth 周期同样在启动期校验（0 会 panic）。
        let ws_reauth_every_n_heartbeats = ws_reauth_config()?;

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
                    // 「未来清理」）；STS 链路与 COS 对象存储解耦。
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
            // 2026-09-14 新增：_e2e 路由门控。
            // 单一控制点 = env `E2E_HOOKS_ENABLED`（缺省 true）。docker compose / dev `cargo run`
            // 走默认（启用）；prod / staging 必须显式 `E2E_HOOKS_ENABLED=false`（ops 责任）。
            // 2026-09-14 修复 Bug #1：移除 main.rs 原 release profile 二次硬关。
            enable_e2e_hooks: env_bool("E2E_HOOKS_ENABLED", true)?,
            // 2026-09-15 followup-cleanup A5/A6：dashboard WS 心跳间隔（秒）；生产 30，测试可调小。
            ws_heartbeat_interval_seconds: env_parse("WS_HEARTBEAT_INTERVAL_SECONDS", 30u64)?,
            // 2026-10-01 新增：WS 存活检测（协议层 Ping + Pong 超时），见字段 doc 与
            // `ws_liveness_config` 的交叉校验。text 心跳与协议层 Ping 职责分离，两者并存。
            ws_ping_interval_seconds,
            ws_pong_timeout_seconds,
            // 2026-10-02 新增：周期性 re-auth 的心跳周期（可注入，便于 CI 覆盖 4001 路径）。
            ws_reauth_every_n_heartbeats,
            // 2026-09-20 新增：HTTP nest 请求超时；与 WS 隔离（挂在内层）。
            request_timeout_seconds: env_parse("REQUEST_TIMEOUT_SECONDS", 30u64)?,
            // 2026-10-03 新增：打印路径专用 HTTP 超时档（`middleware::timeout` 按
            // `is_print_path` 分档）。缺省 660s = Python 打印档 600s + 60s 边际，
            // 理由见 `AppConfig::print_request_timeout_seconds` 的 doc。
            // 用 `env_parse(.., 缺省)` 而非 `env_required`：本仓约定是「未配置则降级」
            // （与 `python_backend` / `wecom` 的 enabled 闸同一范式），不是「未配置则拒启」。
            print_request_timeout_seconds: env_parse(
                "PRINT_REQUEST_TIMEOUT_SECONDS",
                PRINT_REQUEST_TIMEOUT_SECONDS_DEFAULT,
            )?,
            // 2026-09-23 新增 Idempotency 中间件 TTL（秒）。
            idempotency_ttl_seconds: env_parse("IDEMPOTENCY_TTL_SECONDS", 86_400u64)?,
            // 2026-09-26 新增：可选初始管理员账号种子开关（生产默认关闭）。
            bootstrap_admin_enabled: env_bool("BOOTSTRAP_ADMIN_ENABLED", false)?,
            // 2026-09-28 新增：rust → python 后端转发配置（薄壳鉴权转发到 python STS）。
            // env 沿用既有命名（3 个 compose 文件已带 `PYTHON_BACKEND_BASE_URL=${...:-http://backend:8000}`），
            // 不重命名。设置 `PYTHON_BACKEND_BASE_URL` 即 `enabled=true`，未设 → Noop。
            python_backend: {
                let base_url = env_or("PYTHON_BACKEND_BASE_URL", "");
                let enabled =
                    !base_url.trim().is_empty() && std::env::var("PYTHON_BACKEND_BASE_URL").is_ok();
                PythonBackendConfig {
                    base_url,
                    timeout_ms: env_parse("PYTHON_STS_TIMEOUT_MS", 10_000u64)?,
                    // 2026-10-03 新增：打印转发走独立长档（10 分钟），与 STS 的 10s 通道
                    // 分开；见 `PythonBackendConfig::print_timeout_ms` 的 doc。
                    print_timeout_ms: env_parse(
                        "PYTHON_PRINT_TIMEOUT_MS",
                        PYTHON_PRINT_TIMEOUT_MS_DEFAULT,
                    )?,
                    enabled,
                }
            },
            // 2026-09-29 新增：企业微信小程序登录（自建应用 jscode2session）。
            // 刻意用 `env_or(.., "")` 而非 `env_required`：未配置 → enabled=false，
            // 后端照常启动，wx-login 干净返 40109（详见 WeComConfig doc 的理由段）。
            wecom: {
                let corpid = env_or("WECOM_CORPID", "");
                let corpsecret = env_or("WECOM_CORPSECRET", "");
                // `enabled` 只看「两者都非空」；不 trim 后再判，避免
                // " " 这种纯空格占位被判为已配置（打企微接口必失败）。
                WeComConfig {
                    enabled: !corpid.trim().is_empty() && !corpsecret.trim().is_empty(),
                    corpid,
                    corpsecret,
                    api_base: env_or("WECOM_API_BASE", "https://qyapi.weixin.qq.com"),
                    // 仅兜底：正常路径用企微返回的 expires_in - 300（安全余量），
                    // 只有企微没回 expires_in 时才用这个值
                    token_ttl_seconds: env_parse("WECOM_TOKEN_TTL", 6000u64)?,
                    http_timeout_ms: env_parse("WECOM_HTTP_TIMEOUT_MS", 5000u64)?,
                }
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

/// 2026-10-01 新增：解析 WS 存活检测的两个环境变量
/// （`WS_PING_INTERVAL_SECONDS` / `WS_PONG_TIMEOUT_SECONDS`）后交给纯函数交叉校验。
///
/// 拆成「读 env」+「纯校验」两层的理由（2026-10-02 review 第 1 轮 Major-2）：`env_parse`
/// 读的是**进程级全局 env**，同进程内并行单测互相覆盖会 flaky；而交叉校验本身零 IO，
/// 拆开后单测直接喂字面量即可确定性地覆盖边界。
fn ws_liveness_config() -> Result<(u64, u64)> {
    let ping_interval = env_parse("WS_PING_INTERVAL_SECONDS", 20u64)?;
    let pong_timeout = env_parse("WS_PONG_TIMEOUT_SECONDS", 60u64)?;
    validate_ws_liveness(ping_interval, pong_timeout)
}

/// WS 存活检测交叉校验（**纯函数、零 IO**，便于单测直接喂字面量）。
///
/// 校验规则（任一不满足即 bail，**fail-fast**：宁可不启动，也不要带病上线误杀连接）：
/// 1. `ping_interval >= 1`（0 会让 Ping 定时器每轮 select 立刻 ready，空转打满 CPU）；
/// 2. `pong_timeout >= 1`（0 意味着一连接上就判死）；
/// 3. `pong_timeout >= 2 × ping_interval`（**最关键**）。
///
/// ## 为什么第 3 条是 2× 而不是 1×（2026-10-02 修复 review 第 1 轮 Major-2）
/// 判定用的 deadline 是「最后一次入站帧时刻 + pong_timeout」，而入站帧的**唯一常规来源**
/// 是对 `Message::Ping` 的 `Pong` 回声（周期 = ping_interval）。所以
/// `pong_timeout - ping_interval` 就是留给「RTT + 客户端处理 + 回程 + tokio 调度抖动 +
/// 服务端 30s text 心跳分支里的 `select!` 排队」的**总余量**。
///
/// 原校验只挡 `pong_timeout <= ping_interval`，于是 `ping=20 / pong=21`（余量 **1s**）
/// 能过启动校验——1s 在跨网 RTT 或 CI/生产调度抖动下极易被击穿，结果是**健康连接被判死**
/// （而这条判死恰好会踢掉真正在用的大屏）。2× 是「连续丢 1 次 Pong 仍不判死」的下限；
/// 缺省 60/20 = 3×（容忍连续丢 2 次）。
fn validate_ws_liveness(ping_interval: u64, pong_timeout: u64) -> Result<(u64, u64)> {
    if ping_interval == 0 || pong_timeout == 0 {
        return Err(anyhow!(
            "WS_PING_INTERVAL_SECONDS / WS_PONG_TIMEOUT_SECONDS 必须 ≥ 1 \
             （当前 ping_interval={ping_interval} pong_timeout={pong_timeout}）"
        ));
    }
    if pong_timeout < ping_interval.saturating_mul(2) {
        return Err(anyhow!(
            "WS_PONG_TIMEOUT_SECONDS({pong_timeout}) 必须 ≥ 2 × WS_PING_INTERVAL_SECONDS({ping_interval})：\
             1~2 倍余量扛不住 RTT + 调度抖动，会误杀健康连接（缺省 60/20 = 3×）"
        ));
    }
    Ok((ping_interval, pong_timeout))
}

/// 2026-10-02 新增：解析周期性 re-auth 的心跳周期 `WS_REAUTH_EVERY_N_HEARTBEATS`。
///
/// 独立于 `ws_liveness_config`：它不属于「存活检测」而是「鉴权刷新」，但同样需要
/// 启动期校验。
///
/// ## 为什么 0 必须 bail（2026-10-02 订正，review 第 2 轮 Minor-2 同类问题）
/// 上一版注释写「0 会让 handler 里的 `is_multiple_of(0)` panic」——**错的**，已核 rust std
/// `core/src/num/uint_macros.rs`：`is_multiple_of` 的实现是
/// `match rhs { 0 => self == 0, _ => self % rhs == 0 }`，文档明确「never panic」。
/// 真实后果更阴险：handler 的计数器 `heartbeat_ticks` 非 0，故 `is_multiple_of(0)`
/// **恒为 `false`** ⇒ re-auth **静默永不触发** ⇒ 「session 在连接期间被吊销 → 发 4001」
/// 这条**安全闸被无声关掉**，而服务看起来完全正常。这类「不崩、只是把安全检查关掉」的
/// 配置错误正是启动期 fail-fast 该拦的。
fn ws_reauth_config() -> Result<u32> {
    let every_n = env_parse("WS_REAUTH_EVERY_N_HEARTBEATS", 10u32)?;
    if every_n == 0 {
        return Err(anyhow!(
            "WS_REAUTH_EVERY_N_HEARTBEATS 必须 ≥ 1（当前 0：is_multiple_of(0) 恒为 false \
             ——不会 panic，但会让周期性 re-auth 静默永不触发，等于关掉「session 吊销 → \
             4001」这条安全闸）"
        ));
    }
    Ok(every_n)
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
        let pem_bytes =
            fs::read(&path).with_context(|| format!("读取 PEM 文件失败 {}", path.display()))?;
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 手写 `Debug` 必须屏蔽 `corpsecret`（2026-09-29，review 第 1 轮 B4）。
    ///
    /// 这是「不靠约定靠类型」的回归闸：只要有人把 `impl Debug` 删掉改回
    /// `#[derive(Debug)]`，本测试立刻红。
    #[test]
    fn wecom_config_debug_masks_corpsecret() {
        let cfg = WeComConfig {
            corpid: "wwCORPID1234".into(),
            corpsecret: "SUPER-SECRET-VALUE".into(),
            ..WeComConfig::default()
        };
        let dbg = format!("{cfg:?}");
        assert!(
            !dbg.contains("SUPER-SECRET-VALUE"),
            "WeComConfig Debug 泄漏 corpsecret: {dbg}"
        );
        // 排障需要的非敏感字段仍应可见
        assert!(dbg.contains("wwCORPID1234"), "corpid 应可见: {dbg}");
        assert!(dbg.contains("***"), "corpsecret 应显示为 ***: {dbg}");
    }

    /// corpsecret 为空（未配置）时也不能因为「反正没值」而改回回显逻辑。
    #[test]
    fn wecom_config_debug_masks_corpsecret_even_when_blank() {
        let dbg = format!("{:?}", WeComConfig::default());
        assert!(dbg.contains("corpsecret: \"***\""), "{dbg}");
    }

    // =======================================================================
    // 2026-10-02 新增（review 第 1 轮 Major-2）：`validate_ws_liveness` 的边界闸。
    //
    // 原实现只挡 `pong_timeout <= ping_interval`，`ping=20 / pong=21`（1s 余量）
    // 能过启动校验，而 1s 余量扛不住 RTT + 调度抖动 → 误杀健康连接。
    // 改为 `pong_timeout >= 2 × ping_interval` 后这两个用例分别是
    // 「1s 余量必须被拒」与「缺省 3× 必须放行」两侧的钉子。
    //
    // 直接调纯函数（不设 env）：`env_parse` 读进程级 env，同进程并行单测会互相覆盖。
    // =======================================================================

    /// 回归闸：`ping=20 / pong=21`（余量 1s）必须 **bail**，不能放行。
    #[test]
    fn ws_liveness_rejects_one_second_headroom() {
        let err = validate_ws_liveness(20, 21).expect_err("余量仅 1s，应被拒绝");
        let msg = err.to_string();
        assert!(
            msg.contains("必须 ≥ 2 × WS_PING_INTERVAL_SECONDS(20)"),
            "报错文案应说明 2× 约束，实际：{msg}"
        );
    }

    /// 缺省 20 / 60（3×）必须放行。
    #[test]
    fn ws_liveness_accepts_three_times_default() {
        assert_eq!(
            validate_ws_liveness(20, 60).expect("3× 缺省应放行"),
            (20, 60)
        );
    }

    /// 2× 边界本身必须放行（`>=` 而非 `>`），且 0 值仍被拒
    /// （`interval_at` 的 period 为 0 会 panic，见 tokio `time/interval.rs`）。
    #[test]
    fn ws_liveness_boundary_two_times_is_ok_and_zero_rejected() {
        assert_eq!(validate_ws_liveness(20, 40).expect("2× 应放行"), (20, 40));
        assert!(
            validate_ws_liveness(0, 60).is_err(),
            "ping=0 必须 bail（interval 周期为 0 会 panic）"
        );
        assert!(
            validate_ws_liveness(20, 0).is_err(),
            "pong=0 必须 bail（一连接上就判死）"
        );
    }

    // =======================================================================
    // 2026-10-03 新增：打印链路两档超时的缺省值钉子。
    //
    // 两个新配置都是纯缺省值（无跨字段校验），因此不拆「读 env / 纯校验」两段——
    // 缺省值本身被提成 const，直接对 const 断言即可，不需要读进程级 env
    // （`env_parse` 读全局 env，同进程并行单测会互相覆盖，见 `validate_ws_liveness`
    // 上方的说明）。
    // =======================================================================

    /// Python 打印档缺省 10 分钟，且 `PythonBackendConfig::default()` 带上它
    /// （`Default` 与 `from_env` 的缺省必须一致，否则本地 `cargo run` 与生产行为分叉）。
    #[test]
    fn python_print_timeout_default_is_ten_minutes() {
        assert_eq!(PYTHON_PRINT_TIMEOUT_MS_DEFAULT, 600_000);
        assert_eq!(PythonBackendConfig::default().print_timeout_ms, 600_000);
        // 打印档与 STS 档是两个独立档位，别被合并成一个
        assert!(PythonBackendConfig::default().timeout_ms < PYTHON_PRINT_TIMEOUT_MS_DEFAULT);
    }

    /// Rust 打印 HTTP 档缺省必须**严格大于** Python 打印执行档，且边际是 60s。
    ///
    /// 这条不变量是 660 这个数字的全部理由：两者同时到点时先被杀的是 Rust，
    /// Python 侧真实的超时 / 上游错误（502）就被 Rust 的 408 掩盖了。留 60s 边际后
    /// 502 先抵达前端，暴露的是准确原因。
    #[test]
    fn print_timeout_defaults_leave_headroom_over_python() {
        assert_eq!(PRINT_REQUEST_TIMEOUT_SECONDS_DEFAULT, 660);
        let python_seconds = PYTHON_PRINT_TIMEOUT_MS_DEFAULT / 1000;
        assert_eq!(python_seconds, 600);
        assert!(
            PRINT_REQUEST_TIMEOUT_SECONDS_DEFAULT > python_seconds,
            "Rust 档（{}s）必须严格大于 Python 档（{python_seconds}s），否则同时到点时是 Rust 先杀",
            PRINT_REQUEST_TIMEOUT_SECONDS_DEFAULT
        );
        assert_eq!(
            PRINT_REQUEST_TIMEOUT_SECONDS_DEFAULT - python_seconds,
            60,
            "边际固定 60s（10%），够覆盖 Python 回 502 的往返与响应收尾"
        );
    }
}

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
    /// 2026-10-13 新增：CI 预览轮询节流间隔（秒）。
    ///
    /// 环境变量 `PREVIEW_POLL_BACKOFF_SECONDS`，缺省 2
    /// （[`PREVIEW_POLL_BACKOFF_DEFAULT_SECONDS`]），合法区间 `1..=5`
    /// （上界 [`PREVIEW_POLL_BACKOFF_MAX_SECONDS`]），启动期由
    /// `validate_preview_poll_backoff` 严格校验（fail-fast）。
    ///
    /// 语义：`preview::service` 在「首次登记 / 提交 CI job 后」把 `next_poll_at`
    /// 推到的间隔秒数 —— 到期前的请求直接返 PENDING（零外部 IO），到期后才
    /// describe 打 python。生产设 1 可让首次等待更快：前端 `usePreviewPolling`
    /// 的第 1 次等待 = `clamp(后端建议值, 1s, 8s)`，后端配 1 ⇒ 前端第一次等 1s；
    /// 前端的 1.6× 递增退避与本字段无关（不随它变）。
    pub preview_poll_backoff_seconds: i64,
    /// 2026-09-20 新增：HTTP `/api/v2/*` nest 请求超时（秒）。仅挂在 nest 内层
    /// （不影响 WS 长连接，也不影响根 Router 的 CORS/Body limit）。环境变量
    /// `REQUEST_TIMEOUT_SECONDS`，缺省 `30`。
    pub request_timeout_seconds: u64,
    /// 2026-10-03 新增：**长耗时路径** HTTP 请求超时（秒），缺省 `660`。与上面的通用档
    /// 分档并存——`middleware::timeout` 按 [`crate::middleware::timeout::is_long_timeout_path`]
    /// 判定：白名单路径走本档，其余路径仍走 `request_timeout_seconds`（30s 不变）。
    /// 白名单 = 2 条批量打印路由 + `POST /cron/wecom-daily/send`（2026-10-19 加；
    /// 共用本档而非新开一档，理由见 `middleware::timeout` 的模块 doc）。
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
    /// 2026-10-16 新增：邮箱账号**授权码的加密主密钥**（域 `scheduled_task`）。
    ///
    /// env `MAIL_ACCOUNT_SECRET`，**必填、缺失即启动 bail**（见 [`MailAccountSecret`]）。
    /// 授权码本身加密进 `t_mail_account.encrypted_secret`（AES-256-GCM），
    /// 密文**不可回显**、出参里连密文片段都没有。
    ///
    /// ⚠️ 与 `wecom` 那一套的差别：企业微应用凭据缺失是「降级 + 照常启动」
    /// （`enabled=false`，小程序登录返 40109）；主密钥缺失**不是**可降级项 ——
    /// 带着一把猜出来的密钥启动，只会让「管理员保存邮箱成功、但永远发不出信」
    /// 变成现场事故。理由详见 [`MailAccountSecret`] 的模块段。
    pub mail_account_secret: MailAccountSecret,
}

#[derive(Clone, Debug)]
pub struct RedisConfig {
    /// 完整 Redis URL（优先 `REDIS_URL`，否则从 `REDIS_HOST/PORT/DB/PASSWORD` 拼接）
    pub url: String,
    /// session 条目 TTL（秒），对应 JWT `access_ttl_seconds` 的预期寿命；滑动窗口
    pub session_ttl_seconds: u64,
    /// 连接池上限
    pub pool_max_size: usize,
    /// 2026-10-09 新增：Redis key 统一前缀。生产默认空串（key 格式与历史逐字节一致）；
    /// 集成测试按进程填 `t{pid}:` 实现并行隔离（Redis 只有 16 个 db，无法按 binary
    /// 分桶，21 个测试 binary 必然撞）。改动本字段会作废所有已签发 session 的缓存条目。
    ///
    /// ⚠️ 多副本部署时**所有实例必须配置相同前缀**：前缀不一致 ⇒ 各副本写的 key 物理
    /// 不相交 ⇒ LB 后 session 不共享 ⇒ 已登录用户随机被 40105 拦下。
    ///
    /// 环境变量 `REDIS_KEY_PREFIX`，缺省 `""`。前缀只能**整体前置**到既有 key
    /// 之前（`{prefix}session:tok:{jti}`），不得插进命名结构中间。
    ///
    /// ⚠️ 已知偏差：`src/modules/wx/wecom_client.rs::token_cache_key` 的
    /// `wecom:access_token:<corpid>` **不吃本前缀**（该处有偏差登记）。
    pub key_prefix: String,
}

/// JWT 配置（2026-09-22 增 audience 字段 + 2026-09-23 重构 RS256 + kid）
///
/// ## 2026-09-23 重构要点（HS256 → RS256 + kid）
/// - `signing_kid` / `private_key` / `public_keys` / `allow_hs256_fallback` 4 字段
///   本轮新增，详见字段 doc。
/// - `secret` 字段保留：`allow_hs256_fallback=true` 时 decode 端仍走 secret 验签；
///   签发端永不产出 HS256 token。2026-10-04 起该开关缺省改为 false（见字段 doc）。
/// - 启动期严格校验：`JWT_PRIVATE_KEY_PATH` / `JWT_PUBLIC_KEYS_DIR` / `signing_kid ∈ public_keys`
///   任何一项缺失或格式错误即 bail（fail-fast，避免运行时才发现签不出/发不出对应 kid）。
#[derive(Clone, Debug)]
pub struct JwtConfig {
    /// HS256 fallback 的验签 secret。**签发端不使用**（encode 强制 RS256）。
    /// 仅作为 `decode_access` / `decode_refresh` 在 `allow_hs256_fallback=true`
    /// 时对历史 HS256 token 的验签 secret。环境变量 `JWT_SECRET`，仅在
    /// `allow_hs256_fallback=true` 时必填。
    /// 2026-10-04 起 `allow_hs256_fallback` 缺省为 false，本字段在默认配置下
    /// 恒为空串且无人读取（生产/staging/local 三份 compose 均已删除 JWT_SECRET）。
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
    /// - `allow_hs256_fallback`：环境变量 `JWT_ALLOW_HS256_FALLBACK`，缺省 `false`
    ///   （2026-10-04 由 true 收紧）。`false` 时仅 RS256，HS256 token 一律 40100；
    ///   `true` 时 `secret` 变必填且 `decode_access` / `decode_refresh` 接受 HS256 token
    ///   走 `secret` 验签 —— 仅供紧急回滚，全链路已无 HS256 token 的生产者。
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
    /// COS 桶内上传前缀（env `COS_UPLOAD_PREFIX`，代码缺省 `obj/`）。
    ///
    /// 2026-09-29 扁平化：CAS key 模板已从五段简化为两段
    /// `{prefix}{sha16}_{safe_filename}` —— prefix 只在新 key 的写入路径
    /// (`util::cos_key::build_cas_key`) 与读端点的 legacy fallback
    /// (`PartFileService::resolve_effective_key`) 各用一次。
    ///
    /// 2026-10-13：字段**恢复为活字段**（去掉 `#[allow(dead_code)]`）。此前它是死字段
    /// 的原因是 `upload_file_for_owner` 把 prefix 硬编码成 `""`（新对象落桶根），
    /// 而 part 域那条上传路径在读配置 —— 两套并存导致同一张表的对象一半在 `drawings/`
    /// 一半在桶根。本轮把那处硬编码改成读本字段，两个写入点就此统一。
    ///
    /// ⚠️ **改本字段不需要数据迁移**：`t_part_file.object_key` 存的是**完整 key**，
    /// 读端点先 `head_object(db_object_key)` 优先命中 ⇒ 历史对象原地不动，
    /// 只是新上传改落新前缀。别为此跑 `src/bin/cos_key_migrate.rs`（那是 2026-09-29
    /// 五段→两段的迁移，与前缀无关）。
    ///
    /// ### ⚠️ 2026-10-13 review 第 1 轮 MAJOR-2：容器部署下前缀**仍未**统一
    ///
    /// 代码侧（`config.rs` 缺省 / `.env.example` / 代码回退三处）已是 `obj/`，但
    /// **`docker-compose{,-local,-staging}.yml` 的 `${COS_UPLOAD_PREFIX:-drawings/}`
    /// 共 6 处仍在根仓**（backend-rust 仓外，本 PR 结构上无法触及）⇒
    /// **容器部署下前缀仍落 `drawings/`**，代码回退值 `obj/` 只在**不经 compose** 的
    /// `cargo run` 场景生效。根仓那 6 处待单独提交改掉，本仓不要在文档里声称
    /// 「前缀已统一」——那会让后人以为闭环已合上。
    pub upload_prefix: String,
    /// COS 预签 URL 有效期（秒，env `COS_PRESIGN_EXPIRE`，代码缺省 3600）。
    ///
    /// 读侧（`GET /objects/{id}/url` 回的下载直链）用它；rust 侧此前把 `3600`
    /// 写死在 service 里、这个 env 解析了从没被读过（2026-10-15 修正为真读）。
    pub presign_expire_seconds: u32,
    pub max_file_size: usize,
    /// 写侧**预签 PUT URL** 的有效期（秒，env `COS_UPLOAD_URL_EXPIRE`，缺省 900）。
    ///
    /// 2026-10-15 新增：统一上传链路后前端拿 URL 直传 COS，本字段决定那张 URL
    /// 活多久。缺省 900s 远大于一次上传所需（大文件 + 弱网），又不至于让一张能写
    /// 桶的 URL 挂太久。与 [`Self::presign_expire_seconds`]（读侧下载直链）分属两
    /// 条链路、各自独立配置。
    pub upload_url_expire_seconds: u32,
    /// COS 临时对象 prefix 模板前缀（默认 `tmp/`，含尾斜杠）。
    ///
    /// ⚠️ **2026-10-15 起进入下线流程**：统一上传链路不再有 tmp 区，
    /// `POST /files/objects/confirm` 的防呆改为「服务端按 sha+filename 重新派生
    /// CAS key 比对」，`POST /parts/batch` 的 bindings 分支整条删除。随那两处改完
    /// 本字段（与 `COS_TMP_PREFIX` env）一并删除。
    pub tmp_prefix: String,
    /// COS 桶内 **CI 预览图** 前缀（env `COS_PREVIEW_PREFIX`，代码缺省 `previews`）。
    ///
    /// 2026-10-13 PR2 新增。只被 `files::part_file::preview` 的 URL 拼接读一次
    /// （`PreviewService::preview_urls`），是**读端**配置 —— rust 侧不往这个前缀写
    /// 任何东西，JPG 由 python 侧的 CI 任务产出。
    ///
    /// ⚠️ **该前缀必须是 public-read**，否则返回给前端的是「永久不过期的公开读
    /// 直链」这个安全取舍就落不了地（取取舍的理由与补偿见
    /// `src/modules/files/part_file/preview/mod.rs` 的模块 doc）。
    ///
    /// ⚠️ **绝不能在这里拼文件名**：路径成分只有 `<preview_prefix>/<preview_seed>/p{n}.jpg`，
    /// 其中 `preview_seed` 是纯哈希。`t_part_file.object_key` 的 `{sha16}_{原文件名}`
    /// 里那半截文件名（图号）**可枚举**，public-read 前缀下拼进去等于公开图号清单。
    pub preview_prefix: String,
    /// CI 预览图的对外站点根（env `COS_PREVIEW_BASE_URL`）。
    ///
    /// 2026-10-13 PR2 新增。**缺省回退到 COS 源站域名**
    /// `"{scheme}://{bucket}.cos.{region}.myqcloud.com"`（照本结构体已有的
    /// `scheme` / `bucket` / `region` 三字段拼），所以不配也能跑 —— 配它只是为了
    /// 允许前面挂 CDN 或换加速域名。
    ///
    /// ⚠️ 回退值**不读** `endpoint` 字段：那个字段是 OpenDAL 的 S3 endpoint
    /// （可能带私有化路径），与浏览器直链的站点根不是一回事。
    pub preview_base_url: String,
    /// 预览图 `view` 档的 EdgeOne 图片即时处理 URL 参数
    /// （env `COS_PREVIEW_IMAGE_VIEW_PARAMS`，缺省 `eo-img.resize=l/2560`）。
    ///
    /// 2026-10-12 新增。这是**部署级固定 query**：由
    /// `GET /objects/{id}/preview?size=view|thumb|full` 的 `size` 档位选中后，
    /// 拼在每个 `pages[]` 直链的 query 尾部（`?` / `&` 由 service 按 URL 现状选）。
    ///
    /// ⚠️ **本值不做语法校验**：内容来自部署配置（EdgeOne 规则），rust 侧只当
    /// 不透明字符串拼接；写错的症状是「图片 404 / 镜像不按预期缩放」，不是启动失败。
    ///
    /// ⚠️ **`full` 档恒无参数**（不读本字段，也不读 thumb 字段）—— 原始全尺寸
    /// 走无 query 的直链。⚠️ **只允许单轴**（如 `w/480` 或 `l/2560`）：写成
    /// `w/480&h/480` 会让非等比图**失真**，图片即时处理不替调用方保比例。
    pub preview_image_view_params: String,
    /// 预览图 `thumb` 档的 EdgeOne 图片即时处理 URL 参数
    /// （env `COS_PREVIEW_IMAGE_THUMB_PARAMS`，缺省 `eo-img.resize=w/480`）。
    ///
    /// 与 [`Self::preview_image_view_params`] 同款契约（不校验语法 / 只允许单轴 /
    /// `full` 档不读）。列表页缩略图用本档，单张查看默认走 `view` 档。
    pub preview_image_thumb_params: String,
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
/// `POST /api/v2/wx/login/wecom`（2026-10-11 硬切，旧 `/api/v2/wx/iam/wx-login`
/// 已 404）用小程序 `wx.login()` 拿到的**一次性 code**
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

/// 邮箱授权码加密主密钥的 env 变量名（2026-10-16 新增）。
///
/// 同时是「生成命令」的文档锚点与 `crypto.rs` 的读取键名（两者共用本常量，
/// 避免文档与实现各写一份字面量后悄悄漂移）。
pub const MAIL_ACCOUNT_SECRET_ENV: &str = "MAIL_ACCOUNT_SECRET";

/// 生成 [`MAIL_ACCOUNT_SECRET_ENV`] 的命令（写进 `.env.example` 与错误信息）。
pub const MAIL_ACCOUNT_SECRET_HINT: &str = "openssl rand -hex 32";

/// 2026-10-16 新增：邮箱账号**授权码的 AES-256-GCM 主密钥**（32 字节）。
///
/// ## env 格式
///
/// `MAIL_ACCOUNT_SECRET`，两种写法都接受（自动判别）：
/// - **64 位十六进制**（缺省推荐）：`openssl rand -hex 32` 的输出；
/// - **44 位标准 base64**（32 字节的常规编码，末尾可能有一个 `=`）。
///
/// ⚠️ **不接受** 其它长度 / 其它进制 —— 长度不对就报错，不截断、不补零、不取前 32 字节。
/// 一个被悄悄截断的密钥仍然是「一把能用的密钥」，于是配置看着没问题、发信时全失败。
///
/// ## 为什么**必须** fail-fast（与本仓其它 env 的降级范式相反）
///
/// `wecom` / `python_backend` / COS 那几处都是「未配置 ⇒ enabled=false ⇒ 照常启动」。
/// 主密钥不能这样：它不是「某个功能可不可用」的开关，而是「已存进库的密文能不能解开」
/// 的前提。带着一把猜出来的密钥启动，后果是**管理员保存邮箱成功、页面显示一切正常、
/// 直到定时任务真发信才失败**——那时现场只会看到一条 SMTP 错误，而真因在几周前的
/// 一次部署里。故照 `MAIL_SMTP_TLS_MODE` 的同款范式：解析失败直接 bail 并说明怎么生成。
///
/// ## 换密钥 = 存量授权码全部作废
///
/// GCM 没有密钥轮换机制，`MAIL_ACCOUNT_SECRET` 一换，`t_mail_account` 里所有
/// `encrypted_secret` 都解不开（解密失败复用 `21701 BIZ_MAIL_NOT_CONFIGURED`）。
/// `docs/api/mail.md` §8.3 已把「逐行重新加密的迁移流程」登记为未实现缺口。
///
/// ## 安全
///
/// 手写 `Debug`（见下方 impl）固定输出 `***`，**不靠「调用方记得别打印」这条约定**
/// ——与既有 `WeComConfig` 同一手法。
#[derive(Clone)]
pub struct MailAccountSecret {
    key: [u8; 32],
}

impl MailAccountSecret {
    /// 解析 env 值（hex 或 base64），失败时错误信息带格式说明与生成命令。
    pub fn parse(raw: &str) -> Result<Self> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(mail_account_secret_err("为空"));
        }
        let bytes = decode_hex32(trimmed)
            .or_else(|| decode_base64_32(trimmed))
            .ok_or_else(|| {
                mail_account_secret_err(&format!(
                    "长度/格式不合法（收到 {} 个字符）",
                    trimmed.chars().count()
                ))
            })?;
        let mut key = [0u8; 32];
        key.copy_from_slice(&bytes);
        Ok(Self { key })
    }

    /// 读 env `MAIL_ACCOUNT_SECRET` 并解析 —— **生产唯一入口**（`AppConfig::from_env` 只在
    /// 这一处拿主密钥，缺失或格式不对即 `Err` ⇒ **进程启动 bail**）。
    ///
    /// ⚠️ **不是**「只在 `Default` 构造路径用」：`Default`（`[0x5A; 32]`）才是那个只供测试
    /// / 非 `from_env` 装配路径用的，它与本函数恰好互为反面（见 [`Default`] impl 的 doc）。
    /// 另注 `crypto::seal` / `open` 每次调用也**重读同一 env**（同一个 [`parse`] 函数）
    /// ⇒ 「启动通过」与「运行期可解密」不可能分叉；只有集成测试是例外（它不走
    /// `from_env`，自己在进程里写 env）。
    pub fn from_env_value() -> Result<Self> {
        let raw = std::env::var(MAIL_ACCOUNT_SECRET_ENV)
            .with_context(|| format!("环境变量 {MAIL_ACCOUNT_SECRET_ENV} 未设置"))?;
        Self::parse(&raw)
    }

    /// 32 字节主密钥本体（`crypto.rs` 的 `seal_with` / `open_with` 形参）。
    pub fn key(&self) -> [u8; 32] {
        self.key
    }
}

/// 64 位 hex（大小写都接受）。
fn decode_hex32(s: &str) -> Option<Vec<u8>> {
    if s.len() != 64 {
        return None;
    }
    (0..32)
        .map(|i| u8::from_str_radix(s.get(i * 2..i * 2 + 2)?, 16).ok())
        .collect()
}

/// 32 字节的标准 base64（`base64` 已是既有依赖；44 字符含末尾 `=`）。
fn decode_base64_32(s: &str) -> Option<Vec<u8>> {
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(s.as_bytes())
        .ok()?;
    (bytes.len() == 32).then_some(bytes)
}

fn mail_account_secret_err(why: &str) -> anyhow::Error {
    anyhow!(
        "{MAIL_ACCOUNT_SECRET_ENV} {why}：必须是 32 字节，写作 64 位 hex（{MAIL_ACCOUNT_SECRET_HINT}）\
         或 44 位标准 base64。该密钥用于加密 t_mail_account.encrypted_secret，\
         缺失/错误会让所有已存授权码解不开"
    )
}

/// 手写 `Debug`：屏蔽密钥本体（2026-10-16，与 `WeComConfig` 同一手法）。
impl std::fmt::Debug for MailAccountSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MailAccountSecret")
            // ⚠️ 永远输出 `***`，即使只是长度也不能给（长度对攻击者没意义，但
            // 让人误以为「长度是可以打日志的」是个更坏的先例）。
            .field("key", &"***")
            .finish()
    }
}

impl Default for MailAccountSecret {
    /// ⚠️ 这个 `Default` **只**供「不走 `AppConfig::from_env` 的装配路径」用
    /// （集成测试的 `AppConfig { .. }` 字面量）。生产路径由 `from_env` 强制
    /// env 存在且合法，**永不**落到这里。
    ///
    /// 取值是 32 个 `0x5A`（可辨识的占位，不是真密钥）。若某天真的有生产路径落到了
    /// 这里，`crypto.rs` 解不开任何存量密文（会报 21701），属 fail-loud 而非静默错。
    fn default() -> Self {
        Self { key: [0x5Au8; 32] }
    }
}

/// 2026-10-16 保留：SMTP 连接的 TLS 模式三态。
///
/// 原先是 `MAIL_SMTP_TLS_MODE` 的 env 解析结果；该 env 随「邮箱改为页面内配置」
/// 整轮删除（见 [`MailAccountSecret`]）。**枚举类型本身保留** —— `scheduled_task`
/// 域的 SMTP 预设表（`dto::MailAccountPreset`）要映射到它，lettre 侧的三条握手路径
/// 也仍按它分派。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MailTlsModeKind {
    /// 隐式 TLS：连接建立即 TLS（典型端口 465）。**预设表里的唯一取值**。
    Wrapped,
    /// 先明文连上、再 `STARTTLS` 升级（典型端口 587）。
    ///
    /// ⚠️ **当前无生产方**：预设只有网易 163 与 QQ，两者都是 465 / `Wrapped`。
    /// 保留变体是因为 lettre 侧的映射是穷举 `match`，删掉它会让「将来加一家
    /// 587 端口的服务商」变成一次改动 `match` 的破坏性变更 —— 而那种变更最难在
    /// review 时被看见（一个删掉的 `match` 分支看起来总是对的）。
    StartTls,
    /// 明文（仅本地调试用；会把凭据裸传）。
    ///
    /// ⚠️ 同上，**当前无生产方**。
    None,
}

impl std::fmt::Display for MailTlsModeKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Wrapped => "WRAPPED",
            Self::StartTls => "STARTTLS",
            Self::None => "NONE",
        })
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
                // 4. JWT_ALLOW_HS256_FALLBACK（缺省 false）：false 时仅 RS256，
                //    JWT_SECRET 可省略（HS256 token 一律 40100）；true 才恢复
                //    历史 HS256 token 的验签能力，此时 JWT_SECRET 变回必填。
                //
                // HS256 fallback 段：2026-10-04 起默认关闭。签发端自 2026-09-23 起
                // 只产 RS256，v1 python 端 IAM 已下线也不再签发 token，全链路已无
                // HS256 token 的生产者，fallback 只是一条无人走的历史兼容分支。
                // 缺省值取 false 而非 true 是刻意的：此前缺省 true 时，任何漏配
                // JWT_ALLOW_HS256_FALLBACK 的环境（staging / prod 的 compose 就漏配过）
                // 都会默默开着 HS256 验签口。代码缺省与部署配置必须同向收紧。
                // decode_access / decode_refresh 的 header.alg 分支与 secret 字段
                // 仍保留（显式置 true 可回滚），彻底删除留待后续 cleanup。
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
                let allow_hs256_fallback = env_bool("JWT_ALLOW_HS256_FALLBACK", false)?;
                // secret 在 HS256 fallback=true 时仍必填；false（缺省）时允许省略，
                // 省略时为空串 —— 但空串绝不会被拿去验签：middleware.rs / session.rs
                // 按 allow_hs256_fallback 决定传 Some(&secret) 还是 None，
                // 传 None 时 decode_* 直接 40100（防「空 secret HMAC bypass」）。
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
                // 2026-10-13 PR2：预览站点根的**代码回退值**。先算出来再进结构体
                // 字面量，让 `preview_base_url` 永远是可直接拼 URL 的站点根
                // （不含尾斜杠），下游 `PreviewService::preview_urls` 无需再判空。
                let region = env_or("COS_REGION", "ap-shanghai");
                let bucket = env_required("COS_BUCKET")?;
                let scheme = env_or("COS_SCHEME", "https");
                let preview_base_url = env_or_non_empty(
                    "COS_PREVIEW_BASE_URL",
                    // ⚠️ 刻意**不**用 `COS_ENDPOINT`：那是 OpenDAL 的 S3 endpoint
                    // （可能带私有化路径），浏览器直链的站点根与之不是一回事。
                    &format!("{scheme}://{bucket}.cos.{region}.myqcloud.com"),
                );
                // 2026-10-12 新增：预览图 `size` 档位的 EdgeOne 图片即时处理参数。
                // 用 `env_or_non_empty`（空值回落缺省）：`COS_PREVIEW_IMAGE_VIEW_PARAMS=`
                // 这种「变量在、值为空」的写法在 `.env` 里极常见，语义应是「用缺省」
                // 而不是「把空串拼进 URL」（与 `preview_base_url` 同款理由）。
                let preview_image_view_params =
                    env_or_non_empty("COS_PREVIEW_IMAGE_VIEW_PARAMS", "eo-img.resize=l/2560");
                let preview_image_thumb_params =
                    env_or_non_empty("COS_PREVIEW_IMAGE_THUMB_PARAMS", "eo-img.resize=w/480");
                CosConfig {
                    backend,
                    enabled: cos_enabled,
                    region,
                    bucket,
                    secret_id,
                    secret_key,
                    app_id: env_or("COS_APP_ID", ""),
                    endpoint: env_or("COS_ENDPOINT", ""),
                    scheme,
                    // 2026-10-13：缺省值与 `.env.example` / compose 的
                    // `COS_UPLOAD_PREFIX` 同步为 `obj/`（此前是 `uploads`，与
                    // `.env` 的 `drawings/` 又不同 —— 前缀三套并存正是本轮要收敛的）。
                    upload_prefix: env_or("COS_UPLOAD_PREFIX", "obj/"),
                    presign_expire_seconds: env_parse("COS_PRESIGN_EXPIRE", 3600)?,
                    max_file_size: env_parse("COS_MAX_FILE_SIZE", 300 * 1024 * 1024)?,
                    // 2026-10-15 新增：写侧预签 PUT URL 有效期（前端直传上传用）。
                    upload_url_expire_seconds: env_parse("COS_UPLOAD_URL_EXPIRE", 900u32)?,
                    // 2026-10-15：进入下线流程（见字段 doc），随 confirm / batch
                    // 改造一并删除。
                    tmp_prefix: env_or("COS_TMP_PREFIX", "tmp/"),
                    // 2026-10-13 PR2：`files::part_file::preview` 的 URL 拼接两件套
                    preview_prefix: env_or("COS_PREVIEW_PREFIX", "previews"),
                    preview_base_url,
                    // 2026-10-12 新增：`size` 档位的图片即时处理参数（见字段 doc）
                    preview_image_view_params,
                    preview_image_thumb_params,
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
                // 2026-10-09 新增：缺省空串 = 生产 key 格式与历史逐字节一致。
                key_prefix: env_or("REDIS_KEY_PREFIX", ""),
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
            // 2026-10-13 新增：CI 预览轮询节流间隔（`preview::service` 的
            // `next_poll_at` 推后量 + 前端建议重试间隔的缺省值）。缺省 2s 与
            // 既有行为逐字一致；生产可设 1 让首次等待更快（前端 clamp(.., 1s, 8s)）。
            preview_poll_backoff_seconds: preview_poll_backoff_config()?,
            // 2026-09-20 新增：HTTP nest 请求超时；与 WS 隔离（挂在内层）。
            request_timeout_seconds: env_parse("REQUEST_TIMEOUT_SECONDS", 30u64)?,
            // 2026-10-03 新增：长耗时路径专用 HTTP 超时档（`middleware::timeout` 按
            // `is_long_timeout_path` 分档）。缺省 660s = Python 打印档 600s + 60s 边际，
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
            // 2026-10-16 新增：邮箱账号授权码的加密主密钥。⚠️ 本仓**唯一**在
            // mail 相关配置上 fail-fast 的一处（`MailAccountSecret` 的 doc 写了
            // 为什么它与 wecom / COS 那套「未配置就降级」的范式相反）。
            mail_account_secret: MailAccountSecret::from_env_value()
                .with_context(|| format!("加载 {MAIL_ACCOUNT_SECRET_ENV} 失败"))?,
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
/// 拼接（dev/test 默认即可）。注意测试容器走 `redis://localhost:6380/0`，但那是
/// **另一个实例**（`redis-test` vs dev 的 `redis-dev`）；同实例内的测试进程隔离
/// 由 `RedisConfig::key_prefix` 承担（2026-10-09，原先靠 db 分桶，已作废）。
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

/// 2026-10-13 review 第 1 轮 MAJOR-2 新增：与 [`env_or`] 同款，但**空值也回落缺省**。
///
/// ## 为什么需要它（`env_or` 的坑）
///
/// `env::var` 对「变量**存在但为空**」返回 `Ok("")`，**不触发** `unwrap_or_else`
/// 的缺省分支。而部署方把某行**留在文件里、只清空值**（`COS_PREVIEW_BASE_URL=`）
/// 是极常见的做法 —— 那正是 `.env.example` 模板里「可选项留空 = 用缺省」这一惯例
/// 诱导出来的。
///
/// 于是 `preview_base_url = ""` ⇒ `preview_urls` 产出
/// `/previews/<seed>/p1.jpg` —— **协议相对 URL，指向前端自己的源站**，不是 COS。
/// 生产表现是「预览全是裂图」且**日志里看不出任何异常**（一切都成功，只是 URL 指错
/// 了地方）。任何照抄 `.env.example` 起服务的部署都会中招。
///
/// ## ⚠️ 返回值也要 `trim`（2026-10-13 review 第 2 轮 N-9）
///
/// 判空用 `trim()`、返回用原值是**不一致**的：`COS_PREVIEW_BASE_URL=https://cdn.example.com `
///（行尾一个空格，`.env` 手改 / 从文档复制粘贴时极常见）会原样带进 URL ⇒
/// 每个预览 URL 末尾多一个空格 ⇒ 前端 `<img src>` 加载失败。
/// ⇒ **判空与返回用同一种口径**。
fn env_or_non_empty(key: &str, default: &str) -> String {
    match env::var(key) {
        Ok(v) if !v.trim().is_empty() => v.trim().to_string(),
        _ => default.to_string(),
    }
}

/// 2026-10-13 新增：CI 预览轮询节流的**缺省**间隔（秒）。
///
/// 语义：`preview::service` 首次登记后 / 提交 CI job 后把 `next_poll_at` 推到的
/// 间隔。取 **2s** 的理由（原 service 侧常量的 doc 迁入此处的取舍记录）：
/// CI 光栅化一页 PDF 的典型耗时是几百毫秒到数秒，2s 足以让「点了预览立刻再看」
/// 的用户在第二次请求就拿到结果；同时又不至于把 python 打成高频探针
/// （前端轮询间隔通常 ≥1s）。
///
/// 可经 env `PREVIEW_POLL_BACKOFF_SECONDS` 覆盖（合法区间
/// `1..=PREVIEW_POLL_BACKOFF_MAX_SECONDS`）；缺省 2 与改动前行为逐字一致，
/// 生产想更快可设 1。
pub const PREVIEW_POLL_BACKOFF_DEFAULT_SECONDS: i64 = 2;

/// 2026-10-13 新增：预览轮询节流的**校验上限**（秒）。
///
/// 两处消费：① 启动期 `validate_preview_poll_backoff` 的合法区间上界；
/// ② `preview::service` 侧对已落库值的**防御性 clamp**（启动校验才是真闸）。
///
/// ⚠️ 当前实现只写「登记 / 提交后推一个固定间隔」（**无递增退避**）：
/// `t_file_preview` 没有「已轮询次数」列，加递增要改 schema，而 CI 通常几秒内
/// 收敛。这个上限常量是给「将来真要加递增退避」预留的接缝 —— 那时退避序列写成
/// `min(缺省值 × 2^n, 本上限)`。
///
/// 之所以现在就把它钉成一个常量而不是等需要时再加：让它**先在代码里存在**，
/// 将来加递增退避的人能一眼看到上限在哪，而不必自己拍一个（拍高了会拖慢收敛）。
pub const PREVIEW_POLL_BACKOFF_MAX_SECONDS: i64 = 5;

/// 2026-10-13 新增：解析并校验 `PREVIEW_POLL_BACKOFF_SECONDS`（缺省 2）。
///
/// 拆成「读 env」+「纯校验」两层的理由与 `ws_liveness_config` 同款：
/// `env_parse` 读**进程级全局 env**，同进程并行单测互相覆盖会 flaky；校验本身
/// 零 IO，拆开后单测直接喂字面量即可确定性地覆盖边界。
///
/// ⚠️ `env_parse` 对「变量存在但为空串」会解析失败（启动报错）—— 这是有意的、
/// 与其它数值变量同款；本变量由根仓 compose 透传并给缺省 2（同批编排改动，根仓
/// 单列提交），不经 compose 裸跑时由代码缺省 2 兜底。`.env` / `.env.example` 里**不可留空**。
fn preview_poll_backoff_config() -> Result<i64> {
    let v = env_parse(
        "PREVIEW_POLL_BACKOFF_SECONDS",
        PREVIEW_POLL_BACKOFF_DEFAULT_SECONDS,
    )?;
    validate_preview_poll_backoff(v)
}

/// 预览轮询节流校验（**纯函数、零 IO**，便于单测直接喂字面量）。
///
/// 合法区间 `1..=PREVIEW_POLL_BACKOFF_MAX_SECONDS`，区间外 fail-fast
/// （宁可不启动，也不要带病上线）：
/// - `<= 0` 是**忙轮询**：`next_poll_at` 被推到当前或过去 ⇒ 前端拿到的
///   `retry_after_ms` 恒 0 ⇒ 每个请求都真打一次 python / CI；
/// - `> 上限` 无意义且有害：前端 `usePreviewPolling` 的单次等待 8s 封顶会把
///   建议值截断，配大了不会「等更久」，只会让两端语义分叉。
fn validate_preview_poll_backoff(v: i64) -> Result<i64> {
    if !(1..=PREVIEW_POLL_BACKOFF_MAX_SECONDS).contains(&v) {
        return Err(anyhow!(
            "环境变量 PREVIEW_POLL_BACKOFF_SECONDS 必须在 \
             1..={PREVIEW_POLL_BACKOFF_MAX_SECONDS}（秒）之间，实际 {v}：\
             0 / 负数 = 忙轮询（retry_after_ms 恒 0，每个请求都真打 python）；\
             超上限前端 8s 封顶会截断建议值。缺省 2，生产想更快可设 1"
        ));
    }
    Ok(v)
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
    // 2026-10-16：`MailAccountSecret` 的解析与 fail-fast 边界。
    // 直接调纯函数 `parse`（**不设 env**）：env 是进程级的，同进程并行单测会
    // 互相覆盖，本仓一律用纯函数覆盖配置解析（与 `validate_ws_liveness` 同款）。
    // =======================================================================

    /// hex 与 base64 两种写法都要能用（`.env` 里人写的多半是 hex，但 base64
    /// 从别处粘贴过来也很常见）。
    #[test]
    fn mail_account_secret_accepts_hex_and_base64() {
        use base64::Engine;
        let hex = "5a".repeat(32);
        let k1 = MailAccountSecret::parse(&hex).expect("64 位 hex 必须可解析");
        let base64 = base64::engine::general_purpose::STANDARD.encode([0x5Au8; 32]);
        let k2 = MailAccountSecret::parse(&base64).expect("44 位 base64 必须可解析");
        assert_eq!(k1.key(), [0x5Au8; 32]);
        assert_eq!(k2.key(), [0x5Au8; 32], "两种写法必须解出同一把密钥");
        // 大写 hex 同样接受
        let k3 = MailAccountSecret::parse(&"AB".repeat(32)).unwrap();
        assert_eq!(k3.key(), [0xABu8; 32]);
        // 前后空白容忍（`.env` 手写常带缩进 / 复制粘贴常带尾空格）
        assert!(MailAccountSecret::parse(&format!("  {hex}  ")).is_ok());
    }

    /// 长度 / 格式不对必须 **bail**，且错误信息要写清怎么生成 —— 这是 fail-fast
    /// 的全部价值所在（运维照着报错就能改好，不用翻文档）。
    #[test]
    fn mail_account_secret_rejects_bad_length() {
        for bad in ["", "   ", "5a", &"5a".repeat(31), &"5a".repeat(33)] {
            let err =
                MailAccountSecret::parse(bad).expect_err(&format!("{bad:?} 长度不合法，必须被拒"));
            let msg = err.to_string();
            assert!(
                msg.contains("openssl rand -hex 32"),
                "报错必须给出生成命令：{msg}"
            );
        }
        // 64 个合法 hex 字符之外的字符（非 hex、非 base64）
        assert!(MailAccountSecret::parse(&"z".repeat(64)).is_err());
        // 十六进制只认 hex / base64，不接受 base64url 的 `-` / `_`
        assert!(MailAccountSecret::parse(&"-".repeat(43)).is_err());
    }

    /// 手写 `Debug` 必须屏蔽密钥本体（与 `WeComConfig` 同款「不靠约定靠类型」）。
    #[test]
    fn mail_account_secret_debug_masks_key() {
        let hex = "5a".repeat(32);
        let cfg = MailAccountSecret::parse(&hex).unwrap();
        let dbg = format!("{cfg:?}");
        assert!(!dbg.contains(&hex), "泄漏密钥: {dbg}");
        assert!(dbg.contains("***"), "密钥应显示为 ***: {dbg}");
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
    // 2026-10-13 新增：预览轮询节流校验（`validate_preview_poll_backoff`）。
    //
    // 表驱动直接喂字面量（不设 env）：`env_parse` 读进程级全局 env，
    // 同进程并行单测会互相覆盖，与上面 `validate_ws_liveness` 同款理由。
    // =======================================================================

    /// 表驱动：`1 / 2 / 5` 放行且**原值返回**；`-1 / 0 / 6 / 100` 一律 bail。
    ///
    /// 两侧的钉子分别是「生产要配的 1」「缺省 2」「区间上界 5」与
    /// 「忙轮询（0 / 负数）」「超上限（前端 8s 封顶会截断）」，缺任一条都会让
    /// 区间的一端失去回归保护。
    #[test]
    fn validate_preview_poll_backoff_rejects_out_of_range() {
        for v in [1, 2, 5] {
            assert_eq!(
                validate_preview_poll_backoff(v).expect("1..=5 应放行"),
                v,
                "合法值必须原样返回（缺省 2 / 生产 1 / 上界 5 都在这段里）"
            );
        }
        for v in [-1, 0, 6, 100] {
            let err = validate_preview_poll_backoff(v)
                .expect_err("区间外必须 bail（0 / 负数 = 忙轮询；超上限前端 8s 封顶会截断建议值）");
            let msg = err.to_string();
            assert!(
                msg.contains("PREVIEW_POLL_BACKOFF_SECONDS") && msg.contains(&v.to_string()),
                "报错必须带 env 名与实际值，便于运维定位：{msg}"
            );
        }
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

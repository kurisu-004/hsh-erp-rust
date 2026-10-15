//! mail_digest 域 —— 交期到期汇总邮件（配置 + 预览 + 手动发送 + 定时发送）
//!
//! 需求（用户原话）：「用 tokio-cron-scheduler + lettre 构建一个每天发送未来 14 天
//! 系统交期到期的工单的汇总邮件的定时任务」，并配一个管理界面可指定
//! 收件人 / 发送内容 / 触发时间与频率。
//!
//! ## 端点表（挂在 `/api/v2/cron/mail-digest`，**全部 Manager 独占**）
//!
//! | 方法 | 路径 | 说明 |
//! |---|---|---|
//! | `GET`  | `/config`  | 读单例配置；表里无行时返回全部默认值 + `version=0` + `id="0"` |
//! | `POST` | `/config`  | 全量覆写；**body 必含 `version`**（OCC）；成功后热重载 cron |
//! | `GET`  | `/meta`    | 内容与调度侧的表单选项源：频率枚举 / 列白名单 / 取值范围（**2026-10-16 起不再含 `smtp` 段** —— 发信通道改由 `scheduled_task` 域的 `GET /api/v2/cron/mail/meta` + `GET /api/v2/cron/mail/accounts` 提供，见 `dto::MetaOut`） |
//! | `POST` | `/preview` | 按「当前配置 + 临时覆盖」渲染 HTML；**不发送、不落流水** |
//! | `POST` | `/send`    | 手动触发：渲染 + 真发 + 落流水（成败都落） |
//! | `GET`  | `/logs`    | 发送历史分页（`limit∈[1,200]` + `offset`） |
//!
//! 权威路由清单是 [`handler::ROUTES`]（`handler::tests` 断言它与 [`router`] 源码逐条一致）。
//!
//! ## 硬切记录（2026-10-18）
//!
//! - `/api/v2/mail-digest/*` → **`/api/v2/cron/mail-digest/*`**（**旧路径 404，无 alias**）。
//!   归入「定时任务」域组前缀 `/cron`（其下四个子域：`mail` 通道 / `mail-digest` 内容
//!   / `wecom` 通道 / `wecom-daily` 内容）。
//! - 前缀由「顶层 nest、单段」变为「`/cron` 父前缀下的两段」；域内路由表本身
//!   （[`handler::ROUTES`]）**一行未动**，变的只是 nest 路径。
//! - 守卫：`tests/mail_digest/config.rs::old_mail_digest_prefix_returns_404`
//!   （实跑旧前缀，断言 404；同用例先钉新前缀 200 作正向对照）。
//!
//! ## 硬约束记录
//! - **只用 GET / POST**：本仓 HTTP 路由**零** PUT/PATCH/DELETE（全仓 `.put(` /
//!   `.patch(` / `.delete(` 的命中只有 opendal / redis 客户端调用，没有一处是路由方法
//!   ——2026-10-13 review 第 1 轮 m10 实测核实）。配置写入走 `POST /config` 而非
//!   `PUT /config/{id}`。
//!   （本条原先还写着 `206 × .get(` + `18 × .post(` ——reviewer 实测 `.get(` 是 300 处，
//!   且这种「全仓路由方法计数」无法手工稳定复现、随别人的改动立刻过期，故删除；
//!   **零 PUT/PATCH/DELETE** 这半句是结论性事实，留着。）
//! - **SMTP 凭据走 `scheduled_task` 域的邮箱表，不在本域**（2026-10-16 整段替换）：
//!   授权码 AES-256-GCM 加密进 `t_mail_account.encrypted_secret`（主密钥 env
//!   `MAIL_ACCOUNT_SECRET`，见 [`crate::infra::crypto`]），本域只存「选哪个邮箱」
//!   （`t_mail_digest_config.sender_account_id`）+ 收件人 / 内容口径 / 调度。
//!
//!   ⚠️ **为什么是加密而不是「明文入库」或「散列」**：明文入库则备份与任何有只读库
//!   权限的人都能拿到授权码；散列（bcrypt）取不回明文，而 SMTP 发信必须把它交给
//!   lettre ⇒ 只能是**可逆**的对称加密。
//!
//!   ⚠️ 本条**整段替换**掉了原先的「凭据只走 env、不进 DB」：`MAIL_SMTP_HOST` /
//!   `_PORT` / `_TLS_MODE` / `_USERNAME` / `_PASSWORD` / `MAIL_FROM_ADDRESS` /
//!   `_NAME` / `MAIL_DIGEST_ENABLED` **已全部删除、运行时不读**，代码里没有任何
//!   env 兜底路径，也没有启动自检 / 一次性引导导入。
//! - **域外依赖 = 1 个模块**：[`crate::modules::scheduled_task`]（读 `t_mail_account`
//!   一行 + [`crate::infra::crypto`] 解密，见 [`service::resolve_sender`]）。方向是
//!   **mail_digest → scheduled_task**（内容域依赖通道域），反向不存在 —— 通道域
//!   完全不知道自己会被谁用。
//! - **定时任务的注册判据**（2026-10-16 改）：从「`MailConfig::is_enabled()`（env）」
//!   换成「`t_mail_digest_config.enabled` **且**存在可用发信通道」
//!   （= `sender_account_id` 指向一个未软删邮箱）。语义逐条对齐，只是判据从 env
//!   换成了 DB —— 判据在 [`crate::task::mail_digest::reschedule_inner`] 内。
//! - **频率只有 4 个预设**（`DAILY` / `WEEKLY` / `MONTHLY` / `MANUAL_ONLY`），
//!   **没有**「每隔 N 小时」。
//! - **多实例部署不防重**：与既有 `task::auto_complete` 裸循环同一现状 ——
//!   部署 N 个副本就可能在同一时刻发 N 封信。要防重得引入分布式锁/领导者选举，
//!   登记为后续独立重构，**不要**在某个端点里顺手加个 DB 唯一索引当半个防重
//!   （那会让「第二个副本发信失败」被记成流水里的 FAILED，污染审计）。
//!
//! ## 口径分叉登记（与 dashboard 域）
//!
//! 「未交付」的**判据**与 dashboard 逐字同源，行单位同为**工单级**（`t_part` 侧带
//! `assembly_id IS NULL`，装配件行替换其子件行），已交量守卫同为
//! `status IN ('DELIVERED','COMPLETED')`。这三处是既定契约。
//!
//! 「未交付」6 态白名单取自共享常量
//! [`crate::shared::delivery_status::DELIVERY_STATUSES`]：`shared::` 不属于任何业务
//! 域，各域的域隔离判定不受影响；本域的统计 SQL 仍然**自己写全**（见上节那张对照表），
//! 共享的只是那份**状态字面量**。⚠️ 状态白名单留成多份副本时，「加一个状态」漏改一处
//! **不会编译失败**，只会在邮件与大屏之间产生互相矛盾的逾期数，而且没有单测会红。
//!
//! **刻意不同的逐项对照见 `docs/api/mail_digest.md` §4.2**（六项，⚠️ 不止「取哪些行」：
//! 还有**取哪些列**、客户名的取法、两侧窗口边界、行单位 —— 2026-10-16 复核更正，本句
//! 原先写成「仍只有『取哪些行』」，与 §4.2 那张表对不上）。其中影响最大的两条：`LIMIT 30`
//! vs 无 LIMIT（大屏一屏放不下 vs 邮件必须给全量 —— 漏一条交期就是漏一个客户），以及未来节
//! 的 `[today, today+N)` 上界（用户要的就是「未来 14 天」）。
//!
//! ## 域隔离护栏：**不适用**（2026-10-16 复核过）
//!
//! 本域有两类对别域的依赖，**没有一类**适合 `assert_no_foreign_domain`：
//!
//! 1. **只读聚合别域的表**（`t_part` / `t_assembly` / `t_part_batch` / `t_customer`
//!    四张，另加批次位置解析用的 `t_shelf` / `t_worker` / `t_outsource_company`）：
//!    那是本仓既定 pattern（同 `dashboard` / `statistics` / `admin`），统计 SQL 完全在
//!    本域内写全（[`repo`]），不 import 他域的 service / repo。⚠️ **张数刻意不写死**
//!    （加表即过期），完整清单以 `docs/api/mail_digest.md` §8.1 的表格为准。
//! 2. **读 `scheduled_task` 域的 `t_mail_account`**（2026-10-16 新增）：这条**确实**
//!    是 `use crate::modules::scheduled_task::{dto, repo}` 形式的跨域引用 ——
//!    护栏若装上会当场红。
//!
//! 为什么不装：`scheduled_task` 是**通道域**、本域是**内容域**，「发信要拿通道的
//! 凭据」是这个切分的**必然结果**，不是「图省事 import 了别人的 service」。
//! 与第 1 类一样，正确做法在本域内已经做到了（自己写 SQL 查那一行、自己解密），
//! 护栏的规则（「不 import 别域的 service / repo」）在这里恰好把**正确**的写法也拦了。
//!
//! ⚠️ 反向的护栏**装在 `scheduled_task` 域**（`assert_no_foreign_domain`
//! 扫 `src/modules/scheduled_task/**`）：通道域对内容域**零**代码引用，那条
//! 「被引用禁删」的检查是它自己写的 SQL 查 `t_mail_digest_config`。
//!
//! ⚠️ 「统计用的状态白名单」来自 `shared::delivery_status`，**不构成跨域引用** ——
//! `shared::` 不属于任何业务域，护栏判的是「域根段 + 他域路径」的前缀失配。

pub mod dto;
pub mod handler;
pub mod mailer;
pub mod model;
pub mod repo;
pub mod schedule;
pub mod service;
pub mod template;

#[cfg(test)]
mod tests;

use std::sync::Arc;

use axum::Router;

use crate::state::AppState;

/// `/api/v2/cron/mail-digest/*` 端点（6 条，全部 Manager 独占）。
///
/// 挂载点登记在 [`crate::modules::v2_router`] 的 `.nest("/cron/mail-digest", …)`
/// （`/cron` 父前缀下的两段，**不是**顶层单段 —— 2026-10-18 硬切，见模块 doc 的
/// 「硬切记录」）。
/// （2026-10-13 review 第 2 轮 #11：原先写成 [`crate::modules::mod::v2_router`]，
/// `mod` 是关键字，rustdoc 解析不了；仓内其它域一律写不带 `mod` 的形式，见 `wx/mod.rs`。）
pub fn router() -> Router<Arc<AppState>> {
    handler::router()
}

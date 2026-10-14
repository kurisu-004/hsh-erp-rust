//! wecom 域 —— 「定时任务」菜单下的**企业微信推送通道**（群机器人 webhook）
//!
//! 需求（用户原话）：「在定时任务一级菜单下新增 企业微信 子菜单，并新增企业微信子模块。
//! 与邮箱类似，**第一个 tab 页面为 webhook 配置**」。
//!
//! ## ⚠️ 与 `src/modules/wx/wecom_client.rs` **同名不同物**
//!
//! 读本域任何一行之前先分清这两个东西：
//!
//! | | 本域 `modules::wecom` | `modules::wx::wecom_client` |
//! |---|---|---|
//! | 干什么 | 定时任务菜单下的**群机器人 webhook**（把消息推进企业微信群） | **小程序登录**（`wx.login` 的 code 换企业微信 userid） |
//! | 需要凭据 | **不需要** —— 地址里的 `?key=<uuid>` 就是全部鉴权 | 需要 `WECOM_CORPID` / `WECOM_CORPSECRET` |
//! | 有状态吗 | 无（每次一条裸 HTTP POST） | 有（`access_token` 写 Redis 缓存，TTL `expires_in-300`） |
//! | 端点 | `/api/v2/cron/wecom/*`（Manager 独占） | `/api/v2/wx/login/wecom`（公开端点） |
//!
//! 两者除「都打企业微信的域名」外**零交集**：不要复用那边的 client，也不要把本域的
//! webhook URL 当成那边的 `access_token` 看待。
//!
//! ## 域范围：只管**通道**，不管内容与调度
//!
//! | 归本域 | 归 `wecom_daily` 域 |
//! |---|
//! | `t_wecom_webhook`（群机器人实体） | `t_wecom_daily_config`（收哪些节 / 统计口径 / 频率 / **选哪个 webhook**） |
//! | webhook 地址的加密与出入库 | `t_wecom_daily_log`（推送流水） |
//! | 地址掩码 | 每日总结的 Markdown 渲染与分片 |
//! | 「测试连通」探针的发送通道（[`sender`]） | 定时任务的注册与触发 |
//!
//! 切分线的理由：**「有哪些推送通道」与「用通道推什么」是两个独立演进的职责**（与邮件两域
//! 同款）。加第二种推送类型（周报、异常告警）时前者不动、后者各自加自己的内容配置表；
//! 而 webhook 实体、地址加密、掩码、`reqwest` 出网这一整套只需要建一次。
//!
//! ⚠️ **方向是单向的**：`wecom_daily` import 本域的 [`service::resolve_target`] /
//! [`service::target_is_available`] / [`service::send_probe`] 与 [`sender::send_markdown`]；
//! 本域**对 `wecom_daily` 零代码依赖**（唯一触及它的地方是 `repo.rs` 里那条「查有没有
//! 配置在用我」的 COUNT SQL，见「域隔离护栏」）。
//!
//! ## 端点表（挂在 `/api/v2/cron/wecom`，**全部 Manager 独占**）
//!
//! | 方法 | 路径 | 说明 |
//! |---|---|---|
//! | `GET`  | `/webhooks`                | 列表（`is_default` 排最前）。**绝不返回密文**，只返 `url_masked` |
//! | `POST` | `/webhooks`                | 新建。body `{ name, webhook_url }`。**不设默认** |
//! | `POST` | `/webhooks/{id}`           | **全量覆写**。body `{ version, name, webhook_url }`。`webhook_url` **空串或缺省 ⇒ 沿用旧密文** |
//! | `POST` | `/webhooks/{id}/delete`    | 软删。body `{ version }`。被 `t_wecom_daily_config.webhook_id` 引用 ⇒ `21808` |
//! | `POST` | `/webhooks/{id}/set-default` | 置默认。body `{ version }`（目标行的）。同事务**先降后升** |
//! | `POST` | `/webhooks/{id}/test`      | **body 为空对象 `{}`**（⚠️ **不含 `version`**）。解密 URL 后真发一条 `text` 探针。⚠️ **不写任何日志表** |
//!
//! 权威路由清单是 [`handler::ROUTES`]（`handler::tests` 断言它与 [`router`] 源码逐条一致）。
//! 挂载点登记在 [`crate::modules::v2_router`] 的 `.nest("/cron/wecom", …)`。
//!
//! ## 硬切记录
//!
//! **全新域，无任何旧路径、无 alias**（2026-10-18）。本域的 6 条端点是随本模块一起新增的，
//! 历史上从未存在过 `/api/v2/wecom/*` 之类的公开路径。
//!
//! ⚠️ **与 `/api/v2/cron/wecom-daily` 的区分**：那是内容域的前缀（下一批落地），
//! 第 2 段 `wecom` 与 `wecom-daily` 互不为对方的前缀 ⇒ matchit 在第 2 段就分道，
//! 不存在「静态段被吞成 catch-all」的窗口（核对见 `crate::modules::v2_router` 内
//! `/cron` 前缀族的四条理由）。
//!
//! **只用 GET / POST**：本仓 HTTP 路由**零** PUT/PATCH/DELETE，故「改」走
//! `POST /webhooks/{id}`、「删」走 `POST /webhooks/{id}/delete`（与既有
//! `POST /prod/batches/{id}/receive-from-outsource` 同款约定）。回归用例
//! `tests/wecom/accounts.rs::put_patch_delete_verbs_are_not_routed` 钉住。
//!
//! ## 硬约束记录
//!
//! - **地址加密入库**：`encrypted_url` 存 AES-256-GCM 密文
//!   （`base64(nonce12 ‖ ct ‖ tag16)`，AAD = [`dto::WEBHOOK_URL_AAD`] =
//!   `t_wecom_webhook.url.v1`），主密钥走 env `MAIL_ACCOUNT_SECRET`（**复用，不新增
//!   env**；[`crate::infra::crypto::seal_with_aad`] / `open_with_aad`）。
//!   ⚠️ **AAD 与 mail 的 `t_mail_account.secret.v1` 刻意不同**：同一把主密钥、不同 AAD ⇒
//!   两类密文**互不串扰**（AEAD 把 AAD 纳入认证，拿 webhook 的密文冒充 SMTP 授权码会直接
//!   解不开）。
//!   ⚠️ **不是散列**：群机器人推送必须取回明文 URL 才能发请求，bcrypt 那条路
//!   （[`crate::auth::password`]）是单向的，用它等于配一个永远发不出消息的通道。
//! - **掩码不回显**：出参只给 `url_masked`（地址前 4 + `***` + 后 4），明文 URL 一个字都
//!   不出现在任何出参、错误 `message`、`tracing` 日志里。⚠️ **刻意不设 `url_configured`
//!   布尔**（照 mail 的理由：建 webhook 时地址必填 ⇒ 该布尔恒 true，是会误导出「以后再补」
//!   流程的死字段）。
//! - **被引用禁删**：`t_wecom_daily_config.webhook_id` 指向本行 ⇒ `21808` + 409。
//! - **★ 必须解析响应体的 `errcode`**：[`sender`] 是本域**唯一**调 `reqwest` 打 webhook
//!   的文件。企业微信的失败**绝大多数是 HTTP 200 + `errcode != 0`**，不解析就是
//!   「HTTP 成功 = 推送成功」的静默失败。内容域调它的 `send_markdown`，**不自己再写一份**。
//! - 完整端到端契约见 [`docs/api/wecom.md`](../../../docs/api/wecom.md)。
//!
//! ## 域隔离护栏：**适用**，且已装上
//!
//! [`assert_no_foreign_domain`] 装在 `src/modules/wecom/tests/mod.rs`
//! （`wecom_domain_depends_on_no_other_domain`）。
//!
//! 本域唯一的跨域表引用是 `repo.rs::count_daily_configs_referencing`（查
//! `t_wecom_daily_config` 有几份未软删配置指向本 webhook，`21808` 用）。**SQL 写在本域**、
//! 只查一个标量，**不 import** `wecom_daily` 的任何符号 —— 这正是 `scheduled_task` 查
//! `t_mail_digest_config` 时用的既定 pattern。
//!
//! ⚠️ **护栏扫的是「代码路径」引用（`crate::modules::<他域>`），不是 SQL 表名**。
//!
//! 将来若本域需要 import `wecom_daily`（例如让「删除 webhook」顺便清空引用），护栏会当场
//! 红 —— 那时的正确做法是**反过来**：把引用检查留在本域 SQL 里，或把清理动作交给
//! `wecom_daily` 自己（`POST /api/v2/cron/wecom-daily/config` 存 NULL）。

pub mod dto;
pub mod handler;
pub mod model;
pub mod repo;
pub mod sender;
pub mod service;

#[cfg(test)]
mod tests;

use std::sync::Arc;

use axum::Router;

use crate::state::AppState;

/// `/api/v2/cron/wecom/*` 端点（6 条，全部 Manager 独占）。
///
/// 挂载点登记在 [`crate::modules::v2_router`] 的 `.nest("/cron/wecom", …)`
/// （`/cron` 父前缀下的两段；第 2 段刻意是 `wecom` 单段而非 kebab-case 的
/// `wecom-webhooks`，与菜单 seed 的 `path='/wecom'` 对齐成同一个词，
/// `docs/api/wecom.md` §1 与 `seeds/menu.sql` 同款登记）。
pub fn router() -> Router<Arc<AppState>> {
    handler::router()
}

//! scheduled_task 域 —— 「定时任务」菜单下的**邮件通道**（邮箱实体 + SMTP 预设）
//!
//! 需求（用户原话）：「定时任务菜单的名称结构重新设计。一级菜单：定时任务，
//! 二级菜单：邮件。页面上方使用 tab 区分不同的邮件类型，当前只有一种：交期汇总。
//! 邮箱账号和授权码可以在页面中配置」。
//!
//! ## 域范围：只管**通道**，不管内容与调度
//!
//! | 归本域 | 归 `mail_digest` 域 |
//! |---|---|
//! | `t_mail_account`（邮箱实体） | `t_mail_digest_config`（收件人 / 内容口径 / 调度 / **选哪个邮箱**） |
//! | SMTP 预设元数据（host / port / tls_mode） | `t_mail_digest_log`（发送流水） |
//! | 授权码的加密与出入库 | 交期汇总的渲染与发信编排 |
//!
//! 切分线的理由：**「有哪些发信通道」与「用通道发什么」是两个独立演进的职责**。
//! 加第二个邮件类型（订单变更通知、月度报表）时，前者不动、后者各自加自己的
//! 内容配置表；而邮箱、预设、授权码加密这一整套只需要建一次。
//!
//! ## 端点表（挂在 `/api/v2/cron/mail`，**全部 Manager 独占**）
//!
//! | 方法 | 路径 | 说明 |
//! |---|---|---|
//! | `GET`  | `/meta`                 | `{presets:[{value,label,host,port,tls_mode}]}`（纯函数、不查库） |
//! | `GET`  | `/accounts`             | 邮箱列表；**绝不返回** `encrypted_secret` |
//! | `POST` | `/accounts`             | 新建（`name` / `preset` / `account` / `secret`）；**不设默认** |
//! | `POST` | `/accounts/{id}`        | 全量覆写；body **必含 `version`**（OCC）；`secret` 空串或缺省 ⇒ 沿用旧密文 |
//! | `POST` | `/accounts/{id}/delete` | 软删；被 mail_digest 配置引用 ⇒ `21708` |
//! | `POST` | `/accounts/{id}/set-default` | 置默认；同事务内**先降旧、再升新** |
//!
//! 权威路由清单是 [`handler::ROUTES`]（`handler::tests` 断言它与 [`router`] 源码
//! 逐条一致）。挂载点登记在 [`crate::modules::v2_router`] 的
//! `.nest("/cron/mail", …)`。
//!
//! ## 硬切记录（2026-10-18）
//!
//! - `/api/v2/mail/*` → **`/api/v2/cron/mail/*`**（**旧路径 404，无 alias**）。
//!   归入「定时任务」域组前缀 `/cron`（其下四个子域：`mail` 通道 / `mail-digest` 内容
//!   / `wecom` 通道 / `wecom-daily` 内容），与菜单 seed 的 `path='/mail'` 仍是同一个词 ——
//!   **菜单 path 不跟着 API 前缀变**，变的只是 URL。
//! - 前缀由「顶层 nest、单段」变为「`/cron` 父前缀下的两段」；域内路由表本身
//!   （[`handler::ROUTES`]）**一行未动**，变的只是 nest 路径。
//! - 守卫：`tests/scheduled_task/accounts.rs::old_mail_prefix_returns_404`
//!   （实跑旧前缀，断言 404；同文件先钉新前缀 200 作正向对照）。
//!
//! **只用 GET / POST**：本仓 HTTP 路由**零** PUT/PATCH/DELETE，故「改」走
//! `POST /accounts/{id}`、「删」走 `POST /accounts/{id}/delete`（与既有
//! `POST /prod/batches/{id}/receive-from-outsource` 同款约定）。
//!
//! ## 硬约束记录
//!
//! - **凭据加密入库**：`encrypted_secret` 存 AES-256-GCM 密文
//!   （`base64(nonce12 ‖ ct ‖ tag16)`，AAD = `t_mail_account.secret.v1`），
//!   主密钥走 env `MAIL_ACCOUNT_SECRET`（[`crate::infra::crypto`] +
//!   [`crate::infra::config::MailAccountSecret`]）。
//!   ⚠️ **不是散列**：SMTP 发信必须取回明文，bcrypt 那条路（[`crate::auth::password`]）
//!   是单向的，用它等于配一个永远发不出信的邮箱。
//! - **掩码不可回显，出参里连掩码都没有**：本仓的 `mail_digest` 域曾用
//!   `configured: bool` 表达「是否配了凭据」，在本域**刻意不做** —— 建邮箱时授权码
//!   必填 ⇒ 该布尔恒 true，是死字段，而死字段会让人误以为「授权码可以以后再补」。
//!   同样的理由不设 `secret_fingerprint` 列：前端不需要用密文比对什么。
//! - **只 GET / POST + 掩码不回显 + 被引用禁删** 三条是本域对外的全部约束；
//!   完整端到端契约见 [`docs/api/mail.md`](../../../docs/api/mail.md)。
//! - **预设是 SMTP 参数的唯一来源**：`host` / `port` / `tls_mode` 由
//!   [`dto::MailAccountPreset`] 推导，**不存 DB**。两家都是 465 隐式 TLS 且
//!   都用**授权码**而非登录密码（管理员十有八九会填成密码 —— 每个变体的 doc 里
//!   都点明了这一点）。
//!
//! ## 域隔离护栏：**适用**，且已装上
//!
//! [`assert_no_foreign_domain`] 装在 `src/modules/scheduled_task/tests/mod.rs`
//! （`scheduled_task_domain_depends_on_no_other_domain`）。
//!
//! 与 `dashboard` / `statistics` 的差别：那两个域是「只读聚合别域的表」而**不 import**
//! 别域代码；本域同样做到这一点 —— `repo.rs` 里那条「查 mail_digest 是否引用本邮箱」
//! 的 SQL **写在本域**、只查一个标量，不 import `mail_digest` 的任何符号。
//!
//! ⚠️ **护栏扫的是「代码路径」引用（`crate::modules::<他域>`），不是 SQL 表名**。
//! 这正是「本域读别域的表」能装上护栏的原因，与 CLAUDE.md 登记的既定 pattern
//! （`dashboard` / `statistics` / `admin` 各自在本域 SQL 里只读聚合）一致。
//!
//! 将来若本域需要 import `mail_digest`（例如让「删除邮箱」顺便清空引用），
//! 护栏会当场红 —— 那时的正确做法是**反过来**：把「引用检查」留在本域 SQL 里，
//! 或把清理动作交给 mail_digest 自己（`POST /api/v2/cron/mail-digest/config` 存 NULL）。

pub mod dto;
pub mod handler;
pub mod model;
pub mod repo;
pub mod service;

#[cfg(test)]
mod tests;

use std::sync::Arc;

use axum::Router;

use crate::state::AppState;

/// `/api/v2/cron/mail/*` 端点（6 条，全部 Manager 独占）。
///
/// 挂载点登记在 [`crate::modules::v2_router`] 的 `.nest("/cron/mail", …)`
/// （`/cron` 父前缀下的两段，**不是**顶层单段 —— 2026-10-18 硬切，见模块 doc 的
/// 「硬切记录」；第 2 段 `mail` 刻意不是 kebab-case，菜单结构改版时与前端约定成
/// `/mail` 一词，`docs/api/mail.md` §1 与 `seeds/menu.sql` 同款登记）。
pub fn router() -> Router<Arc<AppState>> {
    handler::router()
}

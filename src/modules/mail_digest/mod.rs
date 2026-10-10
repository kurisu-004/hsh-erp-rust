//! mail_digest 域 —— 交期到期汇总邮件（配置 + 预览 + 手动发送 + 定时发送）
//!
//! 需求（用户原话）：「用 tokio-cron-scheduler + lettre 构建一个每天发送未来 14 天
//! 系统交期到期的工单的汇总邮件的定时任务」，并配一个管理界面可指定
//! 收件人 / 发送内容 / 触发时间与频率。
//!
//! ## 端点表（挂在 `/api/v2/mail-digest`，**全部 Manager 独占**）
//!
//! | 方法 | 路径 | 说明 |
//! |---|---|---|
//! | `GET`  | `/config`  | 读单例配置；表里无行时返回全部默认值 + `version=0` + `id="0"` |
//! | `POST` | `/config`  | 全量覆写；**body 必含 `version`**（OCC）；成功后热重载 cron |
//! | `GET`  | `/meta`    | 表单选项源：频率枚举 / 列白名单 / 取值范围 / SMTP 公开信息 |
//! | `POST` | `/preview` | 按「当前配置 + 临时覆盖」渲染 HTML；**不发送、不落流水** |
//! | `POST` | `/send`    | 手动触发：渲染 + 真发 + 落流水（成败都落） |
//! | `GET`  | `/logs`    | 发送历史分页（`limit∈[1,200]` + `offset`） |
//!
//! 权威路由清单是 [`handler::ROUTES`]（`handler::tests` 断言它与 [`router`] 源码逐条一致）。
//!
//! ## 硬约束记录
//! - **只用 GET / POST**：本仓 HTTP 路由**零** PUT/PATCH/DELETE（全仓 `.put(` /
//!   `.patch(` / `.delete(` 的命中只有 opendal / redis 客户端调用，没有一处是路由方法
//!   ——2026-10-13 review 第 1 轮 m10 实测核实）。配置写入走 `POST /config` 而非
//!   `PUT /config/{id}`。
//!   （本条原先还写着 `206 × .get(` + `18 × .post(` ——reviewer 实测 `.get(` 是 300 处，
//!   且这种「全仓路由方法计数」无法手工稳定复现、随别人的改动立刻过期，故删除；
//!   **零 PUT/PATCH/DELETE** 这半句是结论性事实，留着。）
//! - **SMTP 凭据只走 env，不进 DB**：`t_mail_digest_config` 只存收件人 / 内容口径 /
//!   调度；密码进 DB 就跟着备份走、且任何有只读库权限的人都能捞走。
//!   env 侧见 [`crate::infra::config::MailConfig`]。
//! - **频率只有 4 个预设**（`DAILY` / `WEEKLY` / `MONTHLY` / `MANUAL_ONLY`），
//!   **没有**「每隔 N 小时」。
//! - **多实例部署不防重**：与既有 `task::auto_complete` 裸循环同一现状 ——
//!   部署 N 个副本就可能在同一时刻发 N 封信。要防重得引入分布式锁/领导者选举，
//!   登记为后续独立重构，**不要**在某个端点里顺手加个 DB 唯一索引当半个防重
//!   （那会让「第二个副本发信失败」被记成流水里的 FAILED，污染审计）。
//!
//! ## 口径分叉登记（与 dashboard 域）
//!
//! 「未交付」的判据与 dashboard **共用同一份常量**
//! [`crate::modules::dashboard::repo::DELIVERY_STATUSES`]，行单位同为**工单级**
//! （`t_part` 侧带 `assembly_id IS NULL`，装配件行替换其子件行），已交量守卫同为
//! `status IN ('DELIVERED','COMPLETED')`。这三处与 dashboard 逐字同源，是既定契约。
//!
//! **刻意不同的只有「取哪些行」**：dashboard 每桶 `LIMIT 30`（大屏一屏放不下），
//! 邮件必须给全量 —— 漏一条交期就是漏一个客户；且本域未来节有 `[today, today+N)`
//! 的时间窗口（用户要的就是「未来 14 天」）。理由详见 [`repo`] 模块 doc 的对照表。
//!
//! ## 域隔离护栏：**不适用**
//!
//! 本域读 `t_part` / `t_assembly` / `t_part_batch` / `t_customer` 四张别域的表，
//! 但那是**只读聚合**（本仓既定 pattern，同 `dashboard` / `statistics` / `admin`），
//! 且统计 SQL 完全在本域内写全（[`repo`]），**不 import 他域的 service / repo**。
//! 唯一的「引用他域符号」是共用常量 [`DELIVERY_STATUSES`] —— 它是一个
//! `&'static [&str; 6]`，正是 CLAUDE.md 要求「四处共用」的那一份。
//!
//! 故 `src/modules/mail_digest/tests/` 里**不装** `assert_no_foreign_domain`：
//! 那条护栏扫的是 `crate::modules::<别的域>` 形式的路径引用，本域引用 dashboard 的
//! **常量**（不是 service / repo）正是设计意图，硬要绕开反而会逼出一份副本常量
//! —— 那才是真正的漂移源。

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

/// `/api/v2/mail-digest/*` 端点（6 条，全部 Manager 独占）。
///
/// 挂载点登记在 [`crate::modules::v2_router`] 的 `.nest("/mail-digest", …)`。
/// （2026-10-13 review 第 2 轮 #11：原先写成 [`crate::modules::mod::v2_router`]，
/// `mod` 是关键字，rustdoc 解析不了；仓内其它域一律写不带 `mod` 的形式，见 `wx/mod.rs`。）
pub fn router() -> Router<Arc<AppState>> {
    handler::router()
}

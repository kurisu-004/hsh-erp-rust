//! wecom_daily 域 —— 「定时任务」菜单下的**企业微信每日总结推送**（内容域）
//!
//! 需求（用户原话）：「我需要一个每日总结定时推送，内容为今日生产情况：
//! 1.全厂报工总览 2.今日出货单数 3.逾期未交工单，3日内交期工单 4.所有工人今日报工统计
//! （用条形统计图展示）」「消息不要截断，如果发现字符超上限则分成两条消息发送」
//! 「按照1的设计，参数应该在配置页可调」。
//!
//! ## 与 `src/modules/wecom/` 的切分线：**通道 vs 内容**
//!
//! | 归 `wecom`（通道域） | 归本域（内容域） |
//! |---|---|
//! | `t_wecom_webhook`（群机器人实体） | `t_wecom_daily_config`（收哪些节 / 统计口径 / 频率 / **选哪个 webhook**） |
//! | webhook 地址的加密与出入库 | `t_wecom_daily_log`（推送流水） |
//! | 地址掩码 | 每日总结的 markdown 渲染与**分片** |
//! | 「测试连通」探针的发送通道 | 定时任务的注册与触发 |
//!
//! 切分线的理由：**「有哪些推送通道」与「用通道推什么」是两个独立演进的职责**（与邮件
//! 两域同款）。加第二种推送类型（周报、异常告警）时前者不动、后者各自加自己的内容
//! 配置表。
//!
//! ⚠️ **方向是单向的**：本域 import `wecom` 的
//! [`service::resolve_target`] / [`service::target_is_available`] /
//! [`sender::send_markdown`]；`wecom` 域**对本域零代码依赖**（唯一触及它的地方是
//! `wecom::repo` 里那条「查有没有配置在用我」的 COUNT SQL）。⇒ 本域**不适用**
//! `assert_no_foreign_domain`（与 `mail_digest → scheduled_task` 同款取舍，见
//! `docs/api/wecom-daily.md` §8.1 的复核登记）。
//!
//! ## 端点表（挂在 `/api/v2/cron/wecom-daily`，**5 个端点 / 6 条路由，全部 Manager 独占**）
//!
//! | 方法 | 路径 | 说明 |
//! |---|---|---|
//! | `GET`  | `/config`  | 读配置**单例**。表空时返**全默认 + `version=0` + `id="0"`**（**不是 404**） |
//! | `POST` | `/config`  | **全量覆写**，body 必含 `version`。成功后热重载 cron（**失败不回滚配置**，只 warn） |
//! | `GET`  | `/meta`    | 表单选项源：频率（含 3 个 `needs_*`）/ 工人榜指标 / 三段取值范围 / 缺省值 |
//! | `POST` | `/preview` | 渲染分片。**不发送、不落流水**。空体 `{}` = 全用当前配置。⚠️ **超 `max_chunks` ⇒ `21804`，绝不截断** |
//! | `POST` | `/send`    | 真发 + 落流水（**成败都落**）。**顺序**逐片发送，不重发已成功的片 |
//! | `GET`  | `/logs`    | 分页，`sent_at DESC, id DESC`，`limit` clamp `[1,200]` 缺省 20，`offset` clamp `>=0`，`total` 不受 limit 截断 |
//!
//! 权威路由清单是 [`handler::ROUTES`]（`handler::tests` 断言它与 [`router`] 源码逐条
//! 一致）。挂载点登记在 [`crate::modules::v2_router`] 的
//! `.nest("/cron/wecom-daily", …)`（⚠️ **独立 `.nest()`**，不合并成 `/cron` 的一个
//! router；理由见 `modules/mod.rs` 里那段 `/cron` 前缀族的 matchit 核对）。
//!
//! ## 硬切记录
//!
//! **全新域，无任何旧路径、无 alias**（2026-10-18）。这 5 个端点是随本模块一起新增的，
//! 历史上从未存在过 `/api/v2/wecom-daily/*` 之类的公开路径。
//!
//! ## 硬约束记录
//! - **只用 GET / POST**：本仓 HTTP 路由**零** PUT/PATCH/DELETE，写端点一律
//!   `POST /config` 而非 `PUT /config/{id}`。
//! - **i64 ⇒ JSON string**：`ConfigOut.id` / `.webhook_id` / `SendResultOut.log_id`
//!   一律经 `shared::types::serialize_i64` / `serialize_i64_opt`（雪花 19 位 ID 在 JS
//!   Number 下丢精度）。
//! - **写端点 `version` 必填、无 `#[serde(default)]`**（CLAUDE.md §8）：缺失 ⇒ axum
//!   `Json` 提取器返 **HTTP 422 纯文本**，不是业务信封。
//! - **`40901` 只表达 OCC**：表带 `version` 列 ⇒ 全量覆写走
//!   `UPDATE … WHERE id=$1 AND version=$2`；0 行命中即 `40901`。首次初始化的并发
//!   撞 23505 也**转成 `40901`**（语义是「别人刚建好了，刷新再改」）。
//! - **无物理外键 / 无 DB ENUM**：`webhook_id` 是裸 bigint（指向已软删 ⇒ `21806`）；
//!   `frequency` / `worker_metric` / `trigger_source` / `status` 都是 varchar，取值域
//!   由 Rust 侧枚举校验，未知值在**写侧**就被 `21804` 拒、不静默取默认值。
//!
//! ## 渲染与分片（核心设计，见 [`template`]）
//!
//! - 选企业微信 **`markdown` v1** 而不是 `markdown_v2`：v2 支持表格但**不支持字体
//!   颜色**、且客户端需 4.1.36+（安卓 4.1.38+）**低版本退化为纯文本** ⇒ 整份日报在老
//!   客户端上变成一坨没格式的文本。v1 牺牲表格换来颜色（逾期数橙红是日报的核心信号）
//!   + 无版本门槛。
//! - **分片不是截断**：渲染器产出**块**列表，两趟贪心装填成 ≤4096 **字节**的 N 条；
//!   `WorkerRow` 不可再拆 ⇒ **永远不会把一行劈成两半**。超 `max_chunks` 一律
//!   `21804`，**没有任何「另有 N 人省略」的字节截断 footer**。
//! - 取数**恒 ≤ 6 条语句**，关掉的节一条 SQL 都不发；`service::fetch_rows` 的循环体内
//!   只许做内存操作（源码级护栏：`tests/wecom_daily/n_plus_one_guard.rs`）。
//!
//! ## 已知限制
//!
//! - **多副本部署不防重**（与 `mail_digest` / `auto_complete` 同一现状，无 leader
//!   election）；**`/send` 无频控**（唯一护栏是 MANAGER 角色 + `enabled` 闸）。
//! - **`task::wecom_daily` 与 `task::mail_digest` 各持一个调度器单例**：本轮刻意
//!   **不抽公共注册表**（通道/内容两域各一个 job 是既有形态），将来出现**第三个**推送
//!   类型时再抽。两者各自用自己的 `OnceLock` + 单 job 槽，互不干扰。
//! - ⚠️ **`worker_id` 在 2026-10-12 之前从未被写入过** ⇒ 该日期之前的日报「工人报工」
//!   恒为 0。**这不是 bug**，是数据事实；登记见 `docs/api/wecom-daily.md` §4。

pub mod dto;
pub mod handler;
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

/// `/api/v2/cron/wecom-daily/*` 端点（6 条路由 = 5 个端点，全部 Manager 独占）。
///
/// 挂载点登记在 [`crate::modules::v2_router`] 的 `.nest("/cron/wecom-daily", …)`
/// （`/cron` 父前缀下的两段，与 `wecom` 通道域各自独立 nest —— 理由见 `modules/mod.rs`
/// 的「`/cron` 前缀族的 matchit 注册顺序核对」）。
pub fn router() -> Router<Arc<AppState>> {
    handler::router()
}

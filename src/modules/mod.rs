//! 业务域聚合：按 REST/mcp/ws 三种入口组装 Router
//!
//! 重构版业务 REST 接口统一挂在 `/api/v2`；AI 只读入口 `/api/mcp` 保持非版本化；
//! WebSocket 大屏挂在 `/ws/dashboard`。

use std::sync::Arc;

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;

use crate::state::AppState;

pub mod _e2e;
pub mod assembly;
// 2026-09-19 新增 com 模块聚合：customer + applicant 平移至 `com::customer` / `com::applicant`，
// URL 迁移到 `/api/v2/com/*`（见本文件 `v2_router` 与 `com/mod.rs`）。
pub mod com;
pub mod dashboard;
// 2026-09-28 新增 files 域：薄壳鉴权转发端点（`POST /api/v2/files/sts-tmp-keys` → python `/api/v1/files/sts-tmp-keys`）。
// 修复 python STS 端口裸开鉴权漏洞：rust 端强制 JWT + Role 鉴权后再转发。
// 2026-10-13 升格为**文件资源容器域**：原独立 `part_file` 域与 `cnc_program` 域收编进来，
// 顶层 `pub mod part_file;` / `pub mod cnc_program;` 两条声明随之删除，
// 下方 `/part-files` 与 `/cnc-programs` 两个 nest 也一并删除（**旧路径 404，无 alias**）。
pub mod files;
// 2026-09-19 IAM 域合并（PR-1）：合并 `auth` + `user` 为单一 `iam` 业务域；
// handler 内 17 端点（session 5 + users 12）+ 1 个 router 工厂函数 `router()`。
// auth / user 目录已删除。
// 2026-09-19 IAM 域收尾（PR-4）：旧 alias `/auth` + `/users` nest 已下线，
// `/api/v2/iam/*` 成为 IAM 域唯一对外接口。
pub mod iam;
// 2026-10-13 新增 mail_digest 域：交期到期汇总邮件的配置 + 预览 + 手动触发
// （6 端点，全部 Manager 独占）+ 后台定时任务（`task::mail_digest`）。
// ⚠️ 2026-10-16 改造：SMTP 凭据**不再走 env**（`MAIL_SMTP_*` 已整轮删除），
// 改由 `scheduled_task` 域的邮箱表提供；本域只管收件人 / 内容口径 / 调度配置。
pub mod mail_digest;
pub mod outsource;
pub mod part;
// 2026-10-16 新增 scheduled_task 域：「定时任务」菜单下的**邮件通道**（邮箱实体 +
// SMTP 预设元数据），6 端点全部 Manager 独占，URL 挂 `/api/v2/mail`。
// 与 mail_digest 域的切分线见该域 `mod.rs` 的模块 doc（通道 vs 内容与调度）。
pub mod scheduled_task;
// 2026-10-02 域归属定案：原 `part_batch` 域（1866 行 helper，无独立 URL）整体
// 归入 prod 域的 `prod::batch` 子模块（src/modules/prod/batch/），承载 `t_part_batch`
// 的 repo / model / shared::batch::status / 全部批次用例与批次路由（逐条见
// `prod/batch/mod.rs` 的路由表）。
// 这里不再 `pub mod part_batch;`，所有引用走 `crate::modules::prod::batch::*`。
// 2026-10-13：原独立 `pub mod part_file;` 已删除 —— 该域收编进 files 域，
// 源码现为 `crate::modules::files::part_file::*`，URL 为 `/api/v2/files/objects/*`。
// 2026-09-19 新增 prod 模块聚合：worker + work_type + process + process_chain +
// queue 平移至 `prod::*`，URL 硬切换到 `/api/v2/prod/*`（无 alias，前端锁步）。
// part / assembly 是核心实体未移入；报工端点保留在 part 域。
// 2026-10-11 补记：assembly 的「域本体不进 prod」决策不变（下方
// `.nest("/assemblies", assembly::router())` 承载其 8 条 CRUD / 状态机端点 ——
// 2026-10-13 订正，原写 10 条；该 nest 的路径与挂载位置不变）；**唯一例外**是装配件级强制完成逃生端点
// `POST /api/v2/prod/assemblies/{assembly_id}/force-complete` 挂在 `prod::router()`
// 下 —— 它操作的是「子件 + 批次的生产执行状态」，与批次 / 队列动作同属生产链路。
// 这是端点级归属裁决，不是域归属变更，两处注释不要读成互相矛盾。
pub mod prod;
pub mod statistics;
// 2026-09-28 删除：相关上传会话域（Redis 共享 STS 凭证会话机制）。
// 前端改为单 uploader 触发时单 HTTP 调用 python `sts-tmp-keys` 数组入参直签，
// 不再走 rust 转发。
// 2026-09-28 新增：微信小程序 BFF 域（`/api/v2/wx/*` 聚合端点）
pub mod wx;

#[derive(Serialize)]
struct HealthResp {
    status: &'static str,
    service: &'static str,
    version: &'static str,
}

async fn health(State(_state): State<Arc<AppState>>) -> Json<HealthResp> {
    Json(HealthResp {
        status: "ok",
        service: "hsh-erp-api",
        version: "v2",
    })
}

/// `/api/v2/*` 业务路由聚合（重构版统一版本前缀）
///
/// 2026-09-19 IAM 域收尾（PR-4）：旧 alias `/auth` + `/users` nest 已删除，
/// `/api/v2/iam/*` 成为 IAM 域唯一对外接口。
///
/// 2026-09-19 com 模块聚合：customer + applicant 已平移至 `com` 子模块，
/// nest 路径由 `/customers` + `/applicants` 迁至 `/com/customers` + `/com/applicants`。
///
/// 2026-09-19 prod 模块聚合：worker + work_type + process + process_chain +
/// queue 5 支撑域平移至 `prod`，URL 硬切换到 `/api/v2/prod/*`。
/// 旧 `/workers` + `/work-types` + `/processes` + `/process-chains` + `/worker-pool` +
/// `/admin/worker-pool` 6 个 nest 同步下线，无 alias（前端配套 PR 锁步）。
///
/// 2026-09-20 新增：签名收 `Arc<AppState>`，在 `route_layer` 上挂 `authenticate_middleware`
/// —— Bearer JWT 验签 + Redis session 校验 + 滑动 TTL 集中处理；公开路径
/// （health / login / refresh / `_e2e`）在 middleware 内部白名单放行。
/// `route_layer` 仅作用于已匹配路由，404 不会被强制鉴权（与现状一致）；
/// 边界由 `tests/auth_middleware.rs::nonexistent_route_returns_404_not_40100`
/// 守住不变量——**绝对不能**换成 `.layer()`，否则 404 路径会先过 middleware
/// 拿 40100，掩盖真实路由错误。
/// 需要 state：axum 0.8 的 `from_fn` 不支持 `State` 提取，必须用
/// `from_fn_with_state(state.clone(), ...)`，因此 v2_router 收 state。
pub fn v2_router(state: Arc<AppState>) -> Router<Arc<AppState>> {
    Router::new()
        .route("/health", get(health))
        // 2026-09-19 IAM 域：新路径 `/iam` 17 端点（session 5 + users 12，PR-1 起开放，PR-4 收尾后唯一）
        .nest("/iam", iam::router())
        // 2026-09-19 com 聚合：customer + applicant 统一挂在 `/com/*` 下
        .nest("/com", com::router())
        // 2026-09-19 prod 聚合：工人 / 工种 / 工序 / 工艺链 / 工人池 5 支撑域统一挂在 `/prod/*` 下
        .nest("/prod", prod::router())
        // 2026-10-09：批次拆分升为全模块共用端点（生产队列看板 / 外协看板 / 零件详情三处调用）。
        // ⚠️ `prod::batch` 因此有两处挂载：本前缀（域内）与 `/api/v2/batches`（本条）。
        .nest("/batches", prod::split_router())
        // 2026-09-28 新增：dashboard 域 HTTP 全量首取端点（`GET /snapshot`）。
        // 路径：挂在 `/api/v2/dashboard/*`，与 `/ws/dashboard`（WS-only，不带
        // `/api/v2` 前缀）并存；前者给前端走「HTTP 首取 + WS 事件 invalidate」
        // 模式，后者保留供 WS-only 老客户端握手 + snapshot。
        .nest("/dashboard", dashboard::http_router())
        // 2026-09-28 新增：files 域（当时是薄壳鉴权转发 STS 端点到 python）。
        // 端点：`POST /api/v2/files/sts-tmp-keys`（详见 `modules/files/mod.rs`）。
        // 2026-10-13：files 域升格为文件资源容器域，**这一行是 files 域唯一的顶层挂载**，
        // 域内两个嵌套子模块的路由由 `files::router()` 内部 nest 声明：
        //   `/files/sts-tmp-keys`  → STS 凭证转发（本 nest 自身）
        //   `/files/objects/*`      → `files::part_file`      （t_part_file 的对象语义）
        //   `/files/cnc/*`          → `files::part_file::cnc` = CNC 配对
        // 硬切无 alias：旧顶层前缀 `/api/v2/part-files/*`（12 条里 4 条改名 + 8 条删除）
        // 与 `/api/v2/cnc-programs/*`（1 条改名 + 4 条删除）一律 404。
        .nest("/files", files::router())
        // 2026-10-13：part 域的强制完成逃生端点**硬切到 `/api/v2/prod/parts/*`**
        // （`prod::router()` 内的 nest，见 `part::force_complete_router`）—— 与装配件侧
        // 的 `/api/v2/prod/assemblies/*` 对称。⇒ **part 域有两处挂载**：本行的
        // `/api/v2/parts/*`（域本体端点一行未动）+ `prod::router()` 里的
        // `/api/v2/prod/parts/*`（1 条）。⚠️ `POST /parts/{id}/cancel` 判据不同
        // （主写入是 `t_part.status`、OCC 锚在 `part.version`），**刻意留在本 nest**。
        // 移除记录见 `src/modules/part/mod.rs` 的模块 doc。
        .nest("/parts", part::router())
        .nest("/assemblies", assembly::router())
        // 2026-10-13 删除：`.nest("/part-files", part_file::router())` 与
        //   `.nest("/cnc-programs", cnc_program::router())`
        //   —— 两域收编进 files 域，旧前缀 404 无 alias（端点清单见 modules/files/mod.rs）。
        // 2026-09-28 删除：/api/v2/upload-sessions nest（域整体下线）
        .nest("/outsource-companies", outsource::company_router())
        .nest("/outsource-quotes", outsource::quote_router())
        .nest("/outsource-shipments", outsource::shipment_router())
        // 外协看板（2 条只读聚合 + 1 条三合一移动写端点）。
        // 旧挂载点 `/outsource-pool`（三条旧读）与 `/outsource-sendable`（可发送一览）
        // 均于 2026-10-09 硬切下线，无 alias。改名理由与 `prod::worker_pool →
        // prod::queue` 同源 —— `pool` 只覆盖了「候选池」一块，而本 nest 返回的是
        // 「候选 + 公司列 + 内联在途批次」的整块看板与它的移动写端点。
        .nest("/outsource-queue", outsource::queue_router())
        .nest("/statistics", statistics::router())
        // 2026-10-13 新增：mail_digest 域（交期汇总邮件）。6 端点全部 Manager 独占，
        // 角色守卫写在各 handler 第一行（`require_role(Role::Manager)`），此处不挂 role layer。
        // 前缀 `/mail-digest`：本仓顶层 nest 惯例是 kebab-case 单段（`/part-files` /
        // `/cnc-programs` / `/outsource-queue`），域名 `mail_digest` 直接连字符化。
        .nest("/mail-digest", mail_digest::router())
        // 2026-10-16 新增：scheduled_task 域（邮件通道：邮箱 CRUD + SMTP 预设表）。
        // 6 端点全部 Manager 独占，角色守卫写在各 handler 第一行
        // （`require_role(Role::Manager)`），此处不挂 role layer。
        // 前缀 `/mail`：**刻意不是** kebab-case —— 菜单结构改版（一级「定时任务」
        // / 二级「邮件」）时与前端路由、菜单 seed 的 `path='/mail'` 对齐成同一个词。
        // 完整契约见 `docs/api/mail.md`。
        .nest("/mail", scheduled_task::router())
        // 2026-09-28 新增：微信小程序 BFF 域（聚合端点 + 复用 IAM 鉴权）
        // 2026-10-11 重构：按小程序页面切子模块（`/login` + `/part-list`），
        //   URL 跟页面名走，旧 `/wx/iam/*` / `/wx/parts*` / `/wx/dashboard*`
        //   **硬切无 alias**（一律 404），端点清单见 `modules/wx/mod.rs`。
        .nest("/wx", wx::router())
        // 2026-09-14 新增：e2e 测试 seed hook（dev/test 默认启用，release profile 硬关）
        .nest("/_e2e", _e2e::router())
        // 2026-09-20 新增：JWT 验证统一走中间件（详见 auth::middleware）
        // 2026-09-22 重构：`auth_middleware` → `authenticate_middleware`（全词化）。
        //
        // 2026-09-23 review #1 修复：route_layer 调用顺序语义是「后调 = 外层 =
        // 请求先经过」。想要 auth 先跑 → auth 必须「后调 = 最后写」。现顺序：
        // 1. idempotency_middleware 先调 = 内层 = handler 之前最后跑（命中即返）
        // 2. authenticate_middleware 后调 = 外层 = handler 之前最先跑（先鉴权）
        // 请求流：auth → idempotency → handler；鉴权失败的 401 不会被 idem 缓存，
        // 公开路径（login / refresh / health / _e2e）也在 auth 白名单直接放行，
        // idem 内置 public path 闸门是双保险（即使顺序错也不缓存登录 JWT）。
        // 顺序不可换：若 auth 在内层，idem 在外层，则 A 带 key K POST 的响应会
        // 被 idem 缓存，B 用同 key POST 命中缓存直接拿到 A 的响应 → 跨用户数据
        // 泄漏 + 公开路径 JWT 缓存劫持 session。
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            crate::middleware::idempotency::idempotency_middleware,
        ))
        .route_layer(axum::middleware::from_fn_with_state(
            state,
            crate::auth::middleware::authenticate_middleware,
        ))
}

/// `/ws/*` WebSocket 入口（当前仅 dashboard 大屏）
pub fn ws_router() -> Router<Arc<AppState>> {
    dashboard::router()
}

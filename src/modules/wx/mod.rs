//! 微信小程序 BFF 模块聚合（2026-10-11 重写）
//!
//! 路径：`/api/v2/wx/*`，挂在 [`crate::modules::v2_router`] 的 `/wx` nest 下。
//!
//! # 职责
//!
//! **单一消费方**是 `wx-app` 微信小程序（另一消费方是 Vue Web 前端 `frontend/`，
//! 它走各业务域的 `/api/v2/part/*` `/api/v2/prod/*` …端点，**不经本域**）。本域存在的
//! 理由是「小程序只想要瘦卡片，不要全字段响应」——一条响应压到 ~1.5KB / 10 张卡片，
//! 首屏秒开。
//!
//! # 切分原则（2026-10-11 重构）
//! **按小程序页面一一对应切子模块**，而不是按数据表切：
//!
//! - 每个页面 = **1 个「首屏聚合端点」**（角标 + 第 1 页数据）+ **1 个「上拉增量端点」**
//!   （只返增量，不重算角标）
//! - **VO 不复用任何他域结构**（消灭了跨域复用的 `iam::vo::CurrentUserOut` /
//!   `iam::vo::LoginResponse` / `part::statemachine::PartStatus`）
//! - **URL 跟页面名走**（`/login` / `/part-list` / `/production`），不再把别域的
//!   路径前缀（`/iam`）嫁接过来
//!
//! # 端点清单（2026-10-11 硬切，无 alias）
//!
//! | 旧路径 | 现状（2026-10-11） |
//! |---|---|
//! | `POST /api/v2/wx/iam/wx-login` | **删除** → `POST /api/v2/wx/login/wecom`（见 [`login`]） |
//! | `GET /api/v2/wx/parts/counts` | **删除** → 与列表合并进 `GET /api/v2/wx/part-list`（见 [`part_list`]） |
//! | `GET /api/v2/wx/parts/?status=&page=&size=` | **删除** → `GET /api/v2/wx/part-list/page?status=&page=&size=` |
//! | `GET /api/v2/wx/parts/by-serial/{serial_no}` | **删除**（前端 `fetchPartBySerial` 零消费者） |
//! | `GET /api/v2/wx/dashboard/home` | **整域删除**（前端 `fetchHomeDashboard` 零消费者，且无 dashboard 页；随该域一起消失的还有跨域复用的 `CurrentUserOut`） |
//! | `GET /api/v2/wx/batches/counts?period=YYYY-MM` | **删除** → 并入 `GET /api/v2/wx/production`（见 [`production`]） |
//! | `GET /api/v2/wx/batches/?tab=&period=&page=&size=` | **删除** → `GET /api/v2/wx/production/page?tab=&period=&page=&size=` |
//! | `GET /api/v2/wx/worker/stats?period=YYYY-MM` | **删除** → 并入 `GET /api/v2/wx/production` |
//!
//! **硬切 = 旧路径一律 404，无 alias**。小程序侧必须同步切 URL。
//!
//! ## ⚠️ 两条路由事实（踩过的坑，勿忘）
//!
//! 1. **`GET /wx/parts/?…`（带尾斜杠）实际是 404**，无尾斜杠 `/wx/parts` 才命中
//!    handler。本仓 axum 版本下 `nest("/parts") + route("/")` 只匹配**无**尾斜杠的
//!    路径。小程序侧曾按**相反**的假设发请求并踩过 404。新契约全部**无尾斜杠**，
//!    并由 `tests/wx/part_list.rs::trailing_slash_form_is_pinned` 与
//!    `tests/wx/production.rs::trailing_slash_form_is_pinned` 双双钉死。
//! 2. 旧 `GET /wx/dashboard/home` 依赖的 `CurrentUserOut`（跨域复用 iam 的）随该域
//!    一起消失。
//!
//! # 模块布局
//!
//! ```text
//! wx/
//! ├── login/            小程序登录页：POST /login/wecom（公开端点）
//! │   ├── mod.rs        模块 doc + 路由转发
//! │   ├── dto.rs        WxLoginRequest（仅 Deserialize）
//! │   ├── vo.rs         WxLoginOut / WxLoginUserOut（仅 Serialize，不复用 iam）
//! │   ├── service.rs    外部 HTTP + corpid 比对 + iam→wx VO 投影
//! │   └── handler.rs    路由 + 三阶段事务边界
//! ├── part_list/        零件一览页：GET /part-list + /part-list/page
//! │   ├── mod.rs        模块 doc + 路由转发
//! │   ├── dto.rs        PartListQuery（两端点共用）
//! │   ├── vo.rs         PartCardOut（camelCase）+ 2 个分页外壳
//! │   ├── model.rs      FromRow 行结构（不 Serialize）
//! │   ├── repo.rs       SQL 真源（WHERE 只写一份）
//! │   ├── service.rs    tab↔DB 映射表 + 归桶 + 投影 + hasMore
//! │   └── handler.rs    路由（只提取参数 + R::ok）
//! ├── production/       生产页：GET /production + /production/page
//! │   ├── mod.rs        模块 doc + 路由转发 + 域隔离护栏
//! │   ├── dto.rs        ProductionQuery（两端点共用）
//! │   ├── vo.rs         WorkerOut / WorkerStatsOut / BatchCountsOut /
//! │   │                 ProductionBatchCardOut（camelCase）+ 2 个分页外壳
//! │   ├── model.rs      FromRow 行结构（不 Serialize）
//! │   ├── repo.rs       SQL 真源（WHERE 只写一份；count 方法已删）
//! │   ├── service.rs    resolve_period + tab↔DB 映射 + 工人解链 + 投影 + hasMore
//! │   └── handler.rs    路由（只提取参数 + R::ok）
//! └── wecom_client.rs  企业微信 API 客户端（trait + Http + Noop + mock）
//! ```
//!
//! # 跨域依赖方向（单向：wx → 他域）
//!
//! wx 域是**消费方**，只允许 import 他域的 service / VO-as-中间值，**不**写他域的表：
//!
//! - `login` → `iam::service::{AccountService, SessionService}`（查 `t_wx_identity`、
//!   签 token、写 Redis session）。`t_wx_identity` 的 **SQL 真源属 iam 域**
//!   （`modules::iam::repo::sql::wx_identity`），本域对该表**零 SQL**。
//! - `part_list` / `production` → **纯跨域只读聚合**：直接读 `t_part` /
//!   `t_part_batch` / `t_customer`（`part_list`）与 `t_user` / `t_worker` /
//!   `t_work_type` / `t_part_event`（`production`）—— 与 `dashboard` /
//!   `statistics` 同形：只 SELECT，不写，**零**他域 import。
//! - ⚠️ **禁止**反向：任何域都不许 import `modules::wx::*`。唯一的跨域例外是
//!   `state.wecom`（`Arc<dyn WeComApiClient>`）——它由 `AppState` 持有，不是 wx 域
//!   的私有类型。
//!
//! # 已知偏差登记
//!
//! 完整清单见 [`docs/api/wx.md`](../../../docs/api/wx.md)（§8 已知偏差登记）。四条最
//! 要紧的：
//!
//! 1. **4 类 `status` 折叠有静默兜底**（`part_list`）：DB 的 `PROGRAMMING` /
//!    `OUTSOURCE` / `COMPLETED` / `CANCELLED` 不映射到任何 tab 值。它们只计入
//!    `counts.all`，在 `list[].status` 里一律填 `"pendingProduction"`（与旧前端
//!    `mapStatus` 的 catch-all 逐字对齐）。⇒ **`counts` 与 `list` 的归属不完全
//!    对齐**。`production` 域**没有**这个偏差（2 类 tab 与过滤集一一对应）。
//! 2. **`drawingUrl` 有意缺字段**：`t_part` 无图纸列，后端不出该字段（也不加恒
//!    `null` 的占位），小程序侧自己在映射时用 `/asset/drawing/{code}.png` 兜底。
//!    等 COS 文件服务接入后单独 PR 补。
//! 3. **旧路径 404 无 alias** + `by-serial` / `dashboard/home` /
//!    `batches/*` / `worker/stats` 已删除。
//! 4. **`production` 的 `batchQty` 与 `part_list` 的同名字段不同义**（前者是本批次
//!    件数、后者是工单总件数），且 `workHours` 无值时是 `null` 而非 `0`。见
//!    `docs/api/wx.md` §8.6 / §8.8。
//!
//! # 事务分层
//!
//! - [`part_list`] / [`production`] 全部 read-only：`pool.acquire()` 不开事务。
//! - [`login`] 的 `POST /login/wecom` 是**全域唯一**有事务的端点，且外部 HTTP
//!   **必须在事务外**（详见 `login/mod.rs` 的「事务分层」段）。

use std::sync::Arc;

use axum::Router;

use crate::state::AppState;

pub mod login;
pub mod part_list;
pub mod production;
pub mod wecom_client;

/// `/api/v2/wx/*` 入口 router 工厂。
///
/// 一个前缀一个 router 工厂：路由表全部在各子模块的 `handler.rs` 里，本函数只做
/// 转发（仓库硬约束：新域与重构域一律走转发式，`mod.rs` 内联路由的少数派
/// —— `part` / `prod::queue` / `admin` —— 不适用于本次重构）。
///
/// 注册顺序：
/// 1. `login::router()`      —— `/login/wecom`（**公开**端点）
/// 2. `part_list::router()`  —— `/part-list` + `/part-list/page`
/// 3. `production::router()` —— `/production` + `/production/page`
///
/// 三个前缀的段名互不相同（`login` / `part-list` / `production`），**不存在
/// catch-all**，故注册顺序无硬约束（各子模块内部的 `/page` 与 `/` 顺序才是硬
/// 约束，见各 `handler.rs`）。
///
/// ⚠️ 2026-10-11（B3）：旧 `/batches/*` + `/worker/stats` 三个端点已**合并**进
/// `production`（首屏聚合 + 上拉增量），**硬切无 alias**，旧路径一律 404。
pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .nest("/login", login::router())
        .nest("/part-list", part_list::router())
        .nest("/production", production::router())
}

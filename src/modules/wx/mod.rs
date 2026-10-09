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
//! - **URL 跟页面名走**（`/login` / `/part-list` / 生产页），不再把别域的路径前缀
//!   （`/iam`）嫁接过来
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
//! | — | `GET /api/v2/wx/batches/counts?period=YYYY-MM` **原样保留**（B3 换 `/production/*`） |
//! | — | `GET /api/v2/wx/batches?tab=&period=&page=&size=` **原样保留**（B3 换 `/production/*`） |
//! | — | `GET /api/v2/wx/worker/stats?period=YYYY-MM` **原样保留**（B3 换 `/production/*`） |
//!
//! **硬切 = 旧路径一律 404，无 alias**。小程序侧必须同步切 URL。
//!
//! ## ⚠️ 两条路由事实（踩过的坑，勿忘）
//!
//! 1. **`GET /wx/parts/?…`（带尾斜杠）实际是 404**，无尾斜杠 `/wx/parts` 才命中
//!    handler。本仓 axum 版本下 `nest("/parts") + route("/")` 只匹配**无**尾斜杠的
//!    路径。小程序侧曾按**相反**的假设发请求并踩过 404。新契约全部**无尾斜杠**，
//!    并由 `tests/wx/part_list.rs::trailing_slash_form_is_pinned` 钉死。
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
//! ├── batches.rs       批次页（B3 搬进 production/，本步原样保留）
//! ├── worker.rs        工人工作量（B3 搬进 production/，本步原样保留）
//! ├── repo.rs          **B3 过渡期**：只剩 batch / worker 的 SQL（B3 会搬走）
//! ├── vo.rs            **B3 过渡期**：只剩 batch / worker 的 VO（B3 会搬走）
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
//! - `part_list` → 直接读 `t_part` / `t_part_batch` / `t_customer`（**跨域只读
//!   聚合**，与 `dashboard` / `statistics` 同形：只 SELECT，不写）。
//! - ⚠️ **禁止**反向：任何域都不许 import `modules::wx::*`。唯一的跨域例外是
//!   `state.wecom`（`Arc<dyn WeComApiClient>`）——它由 `AppState` 持有，不是 wx 域
//!   的私有类型。
//!
//! # 已知偏差登记
//!
//! 完整清单见 [`docs/api/wx.md`](../../../docs/api/wx.md)（§8 已知偏差登记）。三条最
//! 要紧的：
//!
//! 1. **4 类 `status` 折叠有静默兜底**：DB 的 `PROGRAMMING` / `OUTSOURCE` /
//!    `COMPLETED` / `CANCELLED` 不映射到任何 tab 值。它们只计入 `counts.all`，在
//!    `list[].status` 里一律填 `"pendingProduction"`（与旧前端 `mapStatus` 的
//!    catch-all 逐字对齐）。⇒ **`counts` 与 `list` 的归属不完全对齐**：这类工单只
//!    在「全部」列表可见，点进任一具体 tab 都看不到。
//! 2. **`drawingUrl` 有意缺字段**：`t_part` 无图纸列，后端不出该字段（也不加恒
//!    `null` 的占位），小程序侧自己在映射时用 `/asset/drawing/{code}.png` 兜底。
//!    等 COS 文件服务接入后单独 PR 补。
//! 3. **旧路径 404 无 alias** + `by-serial` / `dashboard/home` 已删除（零消费者）。
//!
//! # 事务分层
//!
//! - [`part_list`] 全部 read-only：`pool.acquire()` 不开事务。
//! - [`login`] 的 `POST /login/wecom` 是**全域唯一**有事务的端点，且外部 HTTP
//!   **必须在事务外**（详见 `login/mod.rs` 的「事务分层」段）。

use std::sync::Arc;

use axum::Router;

use crate::shared::error::AppError;
use crate::state::AppState;

pub mod batches;
pub mod login;
pub mod part_list;
pub mod repo;
pub mod vo;
pub mod wecom_client;
pub mod worker;

/// 把可选 `period`（YYYY-MM）归一化：`None` → 当前月；`Some(s)` → 严格校验。
///
/// 设计：服务端 fallback 到当前月是为了让 mini-program 端不必每次拼 query 字符
/// 串；同时支持前端显式传 period（历史月份视图）。
///
/// 校验规则：
/// - 长度必须 7（`YYYY-MM`）
/// - 第 5 字节必须是 `-`
/// - 月份 ∈ `01..=12`
///
/// 2026-09-28 review #1 修复：从 batches / worker 抽到本模块共享，原地两副本
/// 删除。测试也一并合并到本模块（`#[cfg(test)] mod tests`），避免分散。
pub(crate) fn resolve_period(raw: Option<&str>) -> Result<String, AppError> {
    match raw {
        None => Ok(chrono::Local::now().format("%Y-%m").to_string()),
        Some(s) => {
            if s.len() != 7 || s.as_bytes()[4] != b'-' {
                return Err(AppError::validation(format!(
                    "period {s:?} 格式非法（要求 YYYY-MM）"
                )));
            }
            let month: u32 = s[5..7]
                .parse()
                .map_err(|_| AppError::validation(format!("period {s:?} 月份非法")))?;
            if !(1..=12).contains(&month) {
                return Err(AppError::validation(format!("period {s:?} 月份非法")));
            }
            Ok(s.to_string())
        }
    }
}

/// `/api/v2/wx/*` 入口 router 工厂。
///
/// 一个前缀一个 router 工厂：路由表全部在各子模块的 `handler.rs` 里，本函数只做
/// 转发（仓库硬约束：新域与重构域一律走转发式，`mod.rs` 内联路由的少数派
/// —— `part` / `prod::queue` / `admin` —— 不适用于本次重构）。
///
/// 注册顺序：
/// 1. `login::router()`    —— `/login/wecom`（**公开**端点）
/// 2. `part_list::router()` —— `/part-list` + `/part-list/page`
/// 3. `batches::router()`  —— `/batches/*`（⚠️ 2026-10-11 原样保留；B3 会把
///    3+4 一起换成 `.nest("/production", production::router())`）
/// 4. `worker::router()`   —— `/worker/stats`（同上，B3 范围）
pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .nest("/login", login::router())
        .nest("/part-list", part_list::router())
        // ↓ B3（2026-10-11 之后接手）把下面两个换成
        //   `.nest("/production", production::router())`；本步**刻意原样保留**，
        //   保证 `/wx/batches/*` 与 `/wx/worker/stats` 线上不断。
        .nest("/batches", batches::router())
        .nest("/worker", worker::router())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_period_defaults_to_current_month() {
        let p = resolve_period(None).unwrap();
        assert_eq!(p.len(), 7);
        assert_eq!(p.as_bytes()[4], b'-');
    }

    #[test]
    fn resolve_period_accepts_valid() {
        assert_eq!(resolve_period(Some("2026-09")).unwrap(), "2026-09");
        assert_eq!(resolve_period(Some("2025-12")).unwrap(), "2025-12");
    }

    #[test]
    fn resolve_period_rejects_invalid() {
        assert!(resolve_period(Some("2026-9")).is_err()); // 月份 1 位
        assert!(resolve_period(Some("2026/09")).is_err()); // 分隔符错
        assert!(resolve_period(Some("2026-13")).is_err()); // 月份 13
        assert!(resolve_period(Some("2026-00")).is_err()); // 月份 0
        assert!(resolve_period(Some("26-09")).is_err()); // 年份 2 位
    }
}

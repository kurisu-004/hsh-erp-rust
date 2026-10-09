//! wx::production 子模块 —— 小程序「生产」页（2026-10-11 新增）
//!
//! 本域是**旧 `src/modules/wx/batches.rs` + `worker.rs` + `repo.rs` + `vo.rs` 的
//! 替代品**，三个端点合并成两个、URL 与语义都重新切过：
//!
//! | 旧 | 新 |
//! |---|---|
//! | `GET /api/v2/wx/worker/stats?period=YYYY-MM` | 并入 `GET /api/v2/wx/production` |
//! | `GET /api/v2/wx/batches/counts?period=YYYY-MM` | 并入 `GET /api/v2/wx/production` |
//! | `GET /api/v2/wx/batches/?tab=&period=&page=&size=` | `GET /api/v2/wx/production/page?…` |
//!
//! **硬切无 alias**：旧路径一律 404。
//!
//! ## 两个端点 = 一个页面的两种加载方式
//! - `GET /api/v2/wx/production` —— **首屏聚合**：工人卡（`worker`）+ 月度统计
//!   （`stats`）+ 2 个 tab 角标（`counts`）+ 第 1 页卡片（`list`）。小程序首屏只
//!   需要这一次请求（旧实现要打 3 次：`/worker/stats` + `/batches/counts` +
//!   `/batches/`）。
//! - `GET /api/v2/wx/production/page` —— **上拉增量**：只返 `list` + `hasMore`，
//!   **不重算**角标 / 统计 / 工人。两个端点**共用同一个列表查询**
//!   （`service::list_cards`），故 `?page=2` 在两端点返回的 `list` 逐字相同
//!   （回归：`tests/wx/production.rs::page_endpoint_matches_home_endpoint_at_same_page`）。
//!
//! ## ⚠️ 无尾斜杠（2026-10-11 实测钉死）
//! 本仓 axum 版本下，`nest("/production")` + 内层 `route("/")` **只匹配无尾斜杠**
//! 的 `GET /api/v2/wx/production`；带尾斜杠的 `GET /api/v2/wx/production/` **不命中**
//! （实测 404 **空 body**、不走 `R<T>` 信封）。小程序侧旧代码发的恰恰是带尾斜杠的
//! `/wx/batches/`（见 `wx-app/miniprogram/services/production.ts` 的 2026-09-28
//! 注释），改 URL 时**顺手把尾斜杠去掉**。实测形态登记在 `docs/api/wx.md` §5.1，
//! 回归 `tests/wx/production.rs::trailing_slash_form_is_pinned`。
//!
//! ## ★ 本次修掉的既有 bug：`stats` / `worker` 恒为 0
//! 旧 `GET /api/v2/wx/worker/stats` 把 `CurrentUser.id`（**`t_user.id`**）直接当
//! `t_part_event.worker_id`（语义是 **`t_worker.id`**）查，而**两表之间没有任何
//! 映射** —— 实测 dev 库 5750 条 `t_part_event` 的 13 个 `worker_id` 全部只命中
//! `t_worker`、零命中 `t_user`，故该端点对任何真实用户**恒返
//! `batch_count: 0`**。
//!
//! 修复链路（B1 加列 + 本域解链）：
//!
//! ```text
//! CurrentUser.id (t_user.id)
//!   → t_user.worker_id        ← B1 migration 20261011120000_001 新增（无物理 FK）
//!   → t_worker.id
//!   → t_work_type.name        ← workType 的来源
//! ```
//!
//! ⚠️ **B1 没有回填数据**：回填脚本 `scripts/sql/20261011_backfill_t_user_worker_id.sql`
//! 需人工确认后手工执行，**上线前必须先跑完它** —— `t_user.worker_id` 至今**没有任何
//! app 写端点**（建账号 / 改账号的 DTO 都还没有 `worker_id` 入口），不跑脚本就没有
//! 任何其它途径能把工人绑上。在那之前绝大多数账号（admin / 系统管理员 / `hmi-*` 等
//! 非工人账号）都会命中「未绑定」分支 —— 表现为 `worker: null` +
//! `stats: { batchCount: 0, workHours: 0 }`，**HTTP 仍 200**（不报错，见下文）。
//!
//! ## 「未绑定工人」是**已知状态**，不是错误
//! `worker: null` + 零值 `stats` + HTTP 200。做成 401/403 会让非工人账号**连批次
//! 列表都看不了**，而列表本身与工人身份无关。口径登记在 `docs/api/wx.md` §8.7。
//!
//! ## `?tab=` 语义
//!
//! | 前端传 | SQL 状态集 | `counts` 归桶 |
//! |---|---|---|
//! | `in_progress` | `['IN_PROCESS']` | `status='IN_PROCESS'` + `updated_at` 落当月 |
//! | `done` | `['DELIVERED','COMPLETED']` | `status IN (...)` + 当月存在 `DELIVERED` 事件 |
//! | **其它一切值** | **40001 / HTTP 422** | — |
//!
//! - ⚠️ **必填**：缺 `?tab=` 是 serde 缺字段 ⇒ axum 提取器层 **HTTP 400 纯文本**
//!   （**不走 `R<T>`**，旧端点同款行为，本轮不改）；传了但非法才走 40001 / 422。
//! - ⚠️ **刻意不用** `part::statemachine::PartStatus` 做校验 —— 那正是本次要消灭的
//!   跨域复用。
//!
//! ## ⚠️ `counts` 的键保持 snake_case（`in_progress` / `done`）
//! 它是**前端 tab 名**（前端 `mock/production.ts` 声明为
//! `Record<BatchStatus, number>`），不是卡片字段。卡片字段（`workHours` /
//! `hasMore` / `batchCount` …）才是 camelCase。**别自作主张把 counts 转 camel。**
//!
//! ## 模块结构（照 `prod::inspection` / `wx::part_list` 范式）
//! - `dto.rs` —— 入参（`ProductionQuery`，两个端点共用）
//! - `vo.rs` —— 出参（`WorkerOut` / `WorkerStatsOut` / `BatchCountsOut` /
//!   `ProductionBatchCardOut` + 2 个分页外壳，**camelCase**）
//! - `model.rs` —— `FromRow` 行结构（**不** `Serialize`，不进 JSON）
//! - `repo.rs` —— SQL 真源（ZST `ProductionRepo`；★ **WHERE 只写一份**，`count`
//!   整个方法已删，`hasMore` 改「取 `size + 1` 条」）
//! - `service.rs` —— 业务逻辑（`resolve_period` + tab↔DB 映射表 + 工人解链 +
//!   投影 + `hasMore`）
//! - `handler.rs` —— HTTP 路由（只做参数提取 + `pool.acquire()` + `R::ok`）
//!
//! ## 事务边界
//! 纯读端点：handler `pool.acquire()` 不开事务，**不发** WS 广播（无业务流转）。

use std::sync::Arc;

use axum::Router;

use crate::state::AppState;

pub mod dto;
pub mod handler;
pub mod model;
pub mod repo;
pub mod service;
pub mod vo;

/// `/api/v2/wx/production/*` 入口 router 工厂（转发式：`mod.rs` 只放 `pub fn`，
/// 路由表在 [`handler`]）。
pub fn router() -> Router<Arc<AppState>> {
    handler::router()
}

#[cfg(test)]
mod tests {
    //! 域隔离护栏：把「生产页 BFF 不依赖其它域」从口头约定变成 CI 强制。
    //!
    //! ⚠️ 本域**读** `t_user` / `t_worker` / `t_work_type`（iam / prod 域的数据），
    //! 但按本仓既定 pattern（`statistics` / `admin` / `dashboard`）**只在本域 SQL
    //! 里只读聚合**，**不** import 他域的 service / repo —— 护栏钉死的就是这一点。
    //!
    //! 探测器实现（剥注释、根段 + 域路径前缀匹配、漏报盲区登记、元测试）见
    //! [`crate::shared::domain_guard`]，本域只负责传参 + 域专属指引。

    use std::path::Path;

    use crate::shared::domain_guard::assert_no_foreign_domain;

    #[test]
    fn production_domain_depends_on_no_other_domain() {
        assert_no_foreign_domain(
            "wx::production",
            &Path::new(env!("CARGO_MANIFEST_DIR")).join("src/modules/wx/production"),
            "需要别的域的数据时，正确做法是像 statistics / admin / dashboard 那样在本域 SQL 里\
             只读聚合（t_user / t_worker / t_work_type / t_part / t_part_batch / t_part_event \
             等表，SQL 真源见 repo.rs），而不是 import 别人的 service / repo。",
        );
    }
}

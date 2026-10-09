//! wx::part_list 子模块 —— 小程序「零件一览」页（2026-10-11 新增）
//!
//! 本域是**旧 `src/modules/wx/parts.rs` 的替代品**，URL 与语义都重新切过：
//!
//! | 旧 | 新 |
//! |---|---|
//! | `GET /api/v2/wx/parts/counts` | 与列表合并进 `GET /api/v2/wx/part-list` |
//! | `GET /api/v2/wx/parts/?status=&page=&size=` | `GET /api/v2/wx/part-list/page?…` |
//! | `GET /api/v2/wx/parts/by-serial/{serial_no}` | **删除**（前端零消费者） |
//!
//! **硬切无 alias**：旧路径一律 404。
//!
//! ## 两个端点 = 一个页面的两种加载方式
//! - `GET /api/v2/wx/part-list` —— **首屏聚合**：4 个 tab 角标（`counts`）+ 第 1 页
//!   卡片（`list`）。小程序首屏只需要这一次请求。
//! - `GET /api/v2/wx/part-list/page` —— **上拉增量**：只返 `list` + `hasMore`，
//!   **不重算角标**。两个端点**共用同一个列表查询**（`service::list_cards`），
//!   故 `?page=2` 在两端点返回的 `list` 逐字相同（回归：
//!   `tests/wx/part_list.rs::page_endpoint_matches_home_endpoint_at_same_page`）。
//!
//! ## ⚠️ 无尾斜杠（2026-10-11 实测钉死）
//! 本仓 axum 版本下，`nest("/part-list")` + 内层 `route("/")` **只匹配无尾斜杠**的
//! `GET /api/v2/wx/part-list`；带尾斜杠的 `GET /api/v2/wx/part-list/` **不命中**
//! （实测形态登记在 `docs/api/wx.md` §5 与
//! `tests/wx/part_list.rs::trailing_slash_form_is_pinned`）。
//!
//! 小程序侧曾按**相反**的假设发请求并踩过 404（旧 `/wx/parts/` 同因）。
//!
//! ## `?status=` 语义（2026-10-11 顺带修掉两个既有 bug）
//! 前端传的 `status` 是**前端的 tab 值**，不再是 DB 原值：
//!
//! | 前端传 | 后端过滤的 DB 状态 |
//! |---|---|
//! | 缺省 / `all` | 不过滤 |
//! | `pendingProduction` | `['PENDING']` |
//! | `inProduction` | `['IN_PROCESS']` |
//! | `pendingInspection` | `['INSPECTION']` |
//! | `delivered` | `['READY_TO_SHIP', 'DELIVERED']`（**两个**） |
//!
//! 白名单外的值（如 DB 原值 `PENDING`）→ `AppError::validation`（40001 / HTTP 422）。
//! ⚠️ **刻意不用** `part::statemachine::PartStatus` 做校验 —— 那正是本次要消灭的
//! 跨域复用。
//!
//! ### 修掉的两个 bug
//! 1. **口径不一致**：旧实现角标按 `READY_TO_SHIP + DELIVERED` 算（本地库实测
//!    72 + 126 = 198），列表却只收单值 `DELIVERED`（最多 126）⇒ 角标 198、列表翻到
//!    底也只有 126。本次由 service 层**统一拥有**映射表，两者必然一致。
//! 2. **422 口径错位**：旧实现把前端 tab 值当 DB 状态白名单校验，前端传
//!    `status=inProduction` 会拿到 40001。
//!
//! ## 模块结构（照 `prod::process_design` / `prod::inspection` 范式）
//! - `dto.rs` —— 入参（`PartListQuery`，Query string，两个端点共用）
//! - `vo.rs` —— 出参（`PartCardOut` 判别联合 + 2 个分页外壳，**camelCase**）
//! - `model.rs` —— `FromRow` 行结构（**不** `Serialize`，不进 JSON）
//! - `repo.rs` —— SQL 真源（ZST `PartListRepo` + `counts_by_status` / `list_parts`）
//! - `service.rs` —— 业务逻辑（tab↔DB 映射表 + 归桶 + row→vo 投影 + `hasMore`）
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

/// `/api/v2/wx/part-list/*` 入口 router 工厂（转发式：`mod.rs` 只放 `pub fn`，
/// 路由表在 [`handler`]）。
pub fn router() -> Router<Arc<AppState>> {
    handler::router()
}

#[cfg(test)]
mod tests {
    //! 域隔离护栏：把「零件一览页 BFF 不依赖其它域」从口头约定变成 CI 强制。
    //!
    //! 2026-10-11 review 第 1 轮新增（原计划只要求 `wx::production` 挂）。实测
    //! 本域零他域 import，所以挂上是**零成本 CI 加固**：把「今后有人图省事直接
    //! `use crate::part::…` 复用零件域的类型」当场拦住，而不是等 review 才发现。
    //!
    //! 本域**读** `t_part` / `t_part_batch` / `t_customer` / `t_part_event`
    //! （`part` / `batch` / `com` 域的数据），但按本仓既定 pattern（`statistics` /
    //! `admin` / `dashboard`）**只在本域 SQL 里只读聚合**，**不** import 他域的
    //! service / repo —— 护栏钉死的就是这一点。
    //!
    //! 探测器实现（剥注释、根段 + 域路径前缀匹配、漏报盲区登记、元测试）见
    //! [`crate::shared::domain_guard`]，本域只负责传参 + 域专属指引。

    use std::path::Path;

    use crate::shared::domain_guard::assert_no_foreign_domain;

    #[test]
    fn part_list_domain_depends_on_no_other_domain() {
        assert_no_foreign_domain(
            "wx::part_list",
            &Path::new(env!("CARGO_MANIFEST_DIR")).join("src/modules/wx/part_list"),
            "需要别的域的数据时，正确做法是像 statistics / admin / dashboard 那样在本域 SQL 里\
             只读聚合（t_part / t_part_batch / t_customer / t_part_event 等表，SQL 真源见 \
             repo.rs），而不是 import 别人的 service / repo。",
        );
    }
}

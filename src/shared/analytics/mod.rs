//! 跨域共享聚合函数（2026-09-22 PR3 重构）
//!
//! ## 定位
//! `analytics` 是**共享库**而非**业务域**——不挂 URL、不进 modules/mod.rs、
//! 没有 handler/service/repo/dto/vo 五段式，只承担"纯聚合函数"复用。
//!
//! ## 出处
//! - statistics/service.rs（工人贡献度 + 零填充日计数）
//! - 2026-10-13：`delivered.rs`（已交数量 / 已交套数两条批量聚合），自 part 域
//!   `service::list_enrichment` 与 dashboard 域 `repo::delivery` 的两份逐字副本提取
//!
//! ## 4 条硬边界
//! 1. 不挂 URL：`src/shared/analytics/mod.rs` **不**出现在 `src/modules/mod.rs`
//! 2. 没有五段式：不创建 handler/、service/、repo/、dto/、vo/ 任一段
//! 3. **零 SQL 真源**：所有 SQL 仍在 statistics/repo.rs。本库**不**是任何一张表的
//!    repo、也**不调任何域的 repo**。**唯一例外**是 `delivered` 的两条 SQL（2026-10-13
//!    新增）：它们原先要跨 part / dashboard 两个域复用，故直接落在这里（属**共享口径**
//!    而非某域表结构的私有查询，见 `delivered.rs` 的模块 doc）。
//!    ⚠️ 本库仍 import 既有他域**类型**（`statistics::repo::WorkerPickupRow`、
//!    `statistics::vo::DayCount`）—— 那是 row / vo 的形状，不是 SQL 真源、也不是
//!    调他的 repo；这类依赖是既有事实，见下一节。
//! 4. 零鉴权：analytics 函数不调 `current.require_role()`
//!
//! ## 边界收敛
//! 本库的函数**各有若干消费方**（`statistics` 2 个；`delivered` 共 **3 个消费方**：
//! `part::service::crud` / `dashboard::repo::delivery` / `com::union_list::service::crud`
//! —— 后者经 `part::service::list_enrichment` 的 `pub(crate)` 重导出取用）。
//! `cargo test --lib` 的 dashboard 域隔离护栏
//! （`modules::dashboard::tests::dashboard_domain_depends_on_no_other_domain`）
//! 挡的是「引其它**业务域**」这一类依赖——它只扫 `src/modules/dashboard/**/*.rs`
//! 里的「域根段 + 他域路径」前缀失配，**不拦** `shared::analytics`。
//! `delivered`（2026-10-13 自 part 域与 dashboard 域的两份副本提取）正是靠这一点
//! 消掉副本：**护栏强度未降**，dashboard 域仍然零跨域依赖。

pub mod daily_buckets;
pub mod delivered;
pub mod worker_contribution;

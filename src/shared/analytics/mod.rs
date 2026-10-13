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
//! 3. **SQL 只在本库内自持**：本库**不**是任何一张表的 repo、也不调任何域的 repo；
//!    `daily_buckets` / `worker_contribution` 是纯函数（SQL 在 statistics/repo.rs），
//!    `delivered` 的两条 SQL 因原先要跨两个域复用而直接落在这里（它们是**共享口径**
//!    而非某域表结构的私有查询，见 `delivered.rs` 的模块 doc）
//! 4. 零鉴权：analytics 函数不调 `current.require_role()`
//!
//! ## 边界收敛
//! 本库的函数**各有若干消费方**（`statistics` 2 个、`delivered` 是 part 与 dashboard
//! 两域共用）。`cargo test --lib` 的 dashboard 域隔离护栏
//! （`modules::dashboard::tests::dashboard_domain_depends_on_no_other_domain`）
//! 挡的是「引其它**业务域**」这一类依赖——它只扫 `src/modules/dashboard/**/*.rs`
//! 里的「域根段 + 他域路径」前缀失配，**不拦** `shared::analytics`。
//! `delivered`（2026-10-13 自 part 域与 dashboard 域的两份副本提取）正是靠这一点
//! 消掉副本：**护栏强度未降**，dashboard 域仍然零跨域依赖。

pub mod daily_buckets;
pub mod delivered;
pub mod worker_contribution;

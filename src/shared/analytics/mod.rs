//! 跨域共享聚合函数（2026-09-22 PR3 重构）
//!
//! ## 定位
//! `analytics` 是**共享库**而非**业务域**——不挂 URL、不进 modules/mod.rs、
//! 没有 handler/service/repo/dto/vo 五段式，只承担"纯聚合函数"复用。
//!
//! ## 出处
//! - dashboard/service/snapshot.rs（货架分组）
//! - statistics/service.rs（工人贡献度 + 零填充日计数）
//!
//! ## 4 条硬边界
//! 1. 不挂 URL：`src/shared/analytics/mod.rs` **不**出现在 `src/modules/mod.rs`
//! 2. 没有五段式：不创建 handler/、service/、repo/、dto/、vo/ 任一段
//! 3. 零 SQL 真源：所有 SQL 仍在原 dashboard/repo/sql.rs、statistics/repo.rs
//! 4. 零鉴权：analytics 函数不调 `current.require_role()`
//!
//! ## 未抽离的候选
//! `dashboard/repo/sql.rs::snapshot_counters` 中的"未来 N 天交付分桶 + 零填充"
//! 纯聚合段本可作为第 4 个候选（`bucket_upcoming_delivery`），但按 plan 约束
//! 不得修改 sql.rs（SQL 零变化），故保留原状；待后续 PR 单独处理。
//! 详见 receipt「未完成项」段。

pub mod daily_buckets;
pub mod shelf_grouping;
pub mod worker_contribution;

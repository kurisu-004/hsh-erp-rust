//! 跨域共享聚合函数（2026-09-22 PR3 重构）
//!
//! ## 定位
//! `analytics` 是**共享库**而非**业务域**——不挂 URL、不进 modules/mod.rs、
//! 没有 handler/service/repo/dto/vo 五段式，只承担"纯聚合函数"复用。
//!
//! ## 出处
//! - statistics/service.rs（工人贡献度 + 零填充日计数）
//!
//! ## 4 条硬边界
//! 1. 不挂 URL：`src/shared/analytics/mod.rs` **不**出现在 `src/modules/mod.rs`
//! 2. 没有五段式：不创建 handler/、service/、repo/、dto/、vo/ 任一段
//! 3. 零 SQL 真源：所有 SQL 仍在 statistics/repo.rs
//! 4. 零鉴权：analytics 函数不调 `current.require_role()`
//!
//! ## 边界收敛
//! 本库**只有 statistics 一个消费方**。`cargo test --lib` 的 dashboard 域隔离护栏
//! （`modules::dashboard::tests::dashboard_domain_depends_on_no_other_domain`）把
//! 「dashboard 不引域外库、也不引本库」钉死，防止第二个消费方再次出现（第二个消费方
//! 会让任何一方改动都要跨文件确认，纯属负担）。

pub mod daily_buckets;
pub mod worker_contribution;

//! 跨模块共享类型与工具

// 2026-09-22 PR3：抽离 dashboard/statistics 共享聚合函数到独立 analytics 库。
// analytics 不挂 URL、不进 modules/mod.rs（仅在 shared::analytics 命名空间下），
// 没有 handler/service/repo/dto/vo 五段式，详见 analytics/mod.rs 顶部注释。
pub mod analytics;
pub mod error;
pub mod pagination;
pub mod response;
pub mod serial; // 2026-09-14 Phase 3：跨域序列号派发（assembly + 后续 part 域统一入口）
pub mod types;

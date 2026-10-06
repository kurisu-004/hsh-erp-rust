//! 跨模块共享类型与工具

// 2026-09-22 PR3：抽离 dashboard/statistics 共享聚合函数到独立 analytics 库。
// analytics 不挂 URL、不进 modules/mod.rs（仅在 shared::analytics 命名空间下），
// 没有 handler/service/repo/dto/vo 五段式，详见 analytics/mod.rs 顶部注释。
pub mod analytics;
// 2026-10-08 新增 batch：批次公共设施层（TPartBatch / get_batch_by_id /
// status 写入口 / 7 个守卫）。**边界记档**：`shared::batch` 是本仓**唯一**
// 经域 repo 写库的 shared 模块（`PartRepo::update_part_rollup` /
// `AssemblyService::sync_assembly_status`），因为它是 CLAUDE.md「状态派生契约」
// 三层派生图（t_part_batch.status → t_part.status/next_process_id →
// t_assembly.status）的实现本身，该契约天然跨域、无域可归属；留在 prod::batch
// 只会让 part / assembly 反向依赖 prod::batch，与逐域剥离方向相反。
// 详见 `shared/batch/mod.rs` 顶部注释。
pub mod batch;
pub mod customer; // 2026-10-07：客户 L1/L2 id 展开（part / com::union_list / prod::batch 三域共用）
// 2026-10-07 新增 domain_guard：跨域只读聚合域（dashboard / prod::programming …）的
// 域隔离护栏，把「本域不 import 其它域的 service / repo」从口头约定变成 CI 强制。
// 只在单测里用（调用方全在各域 `#[cfg(test)] mod tests`），故不进生产 API 面。
#[cfg(test)]
pub mod domain_guard;
pub mod error;
pub mod pagination;
pub mod response;
pub mod serial; // 2026-09-14 Phase 3：跨域序列号派发（assembly + 后续 part 域统一入口）
pub mod types;

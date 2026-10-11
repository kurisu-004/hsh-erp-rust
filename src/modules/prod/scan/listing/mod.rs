//! prod::scan 报工台**只读聚合**子模块（3 端点）
//!
//! 三个只读端点的实现：
//! - `GET /api/v2/prod/scan/pickable` —— 该工种在生产架上可领取的批次
//! - `GET /api/v2/prod/scan/held` —— 某工人当前持有的批次（含工序链位置 + 容量）
//! - `GET /api/v2/prod/scan/locate` —— 扫码找件 · 诊断（**2026-10-15 新增**：
//!   扫序列号 → 该 part 全部批次的位置事实 + 针对指定工人的 action / 原因）
//!
//! 2026-10-10 自 `part::service::phase1::work_type` 搬来（硬切无 alias）；
//! `locate` 是 2026-10-15 新立的第三条，不涉及迁移。
//!
//! ## 为什么要独立成子模块而不是塞进 `service/`
//!
//! 域隔离护栏 `assert_no_foreign_domain` 收的是**目录**路径。要让「listing 聚合
//! SQL 零跨域依赖」这条规则可被 CI 执行，被扫描的代码必须自成一个目录。
//! `prod::scan` 域整体**不适用**该护栏 —— `worker_scan` 是转发型用例，必然 import
//! `part` / `assembly` / `prod::queue` / `prod::worker` / `shared::shelf` 五处
//! （与 `prod::queue` 的写端点同款 pattern）。但 `listing` 这三块纯只读聚合
//! **完全可以零跨域** —— 圈出来单独守，比整域不守要强。
//!
//! ## SQL 条数（与工人数 / 批次数无关）
//! - `pickable`：**2 条**（取行 / COUNT）
//! - `held`：**3 条**（取行 / COUNT / 工种上限）
//! - `locate`：**≤ 4 条**（工单命中 / 工人 + 工种上限 / 持有数 / 批次取行；
//!   未传 `worker_id` 时只有 2 条。持有数复用 `/held` 的 `count_held` 同一条 SQL）
//!
//! 三条端点都不按行数重复查（`pickable` 一次 SQL 把「工种→工序映射 → 架 → 批次」
//! 全链 JOIN 完；`held` / `locate` 一次 SQL 把链位置派生挂在 `LEFT JOIN LATERAL`
//! 上，`locate` 另把 PENDING 链首 / 工种工序映射 / 外协在途 shipment 一并 JOIN 完）。
//!
//! 2026-10-14：`held` 的第 3 条是 `fetch_max_held_batches`（`t_worker LEFT JOIN
//! t_work_type` 取 `max_held_batches`，供 `HeldListOut` 的容量展示）。它是
//! 「工人维度」的量，与取行 / COUNT 的「批次维度」量互不覆盖、无法合并 —— 合并
//! 就得把 `t_worker` 拖进那条已经 JOIN 了 `t_part` / `t_part_batch` / 链 step 的
//! 大 SQL 里，让分页 COUNT 为一个非分页的常量付出代价。
//!
//! ⚠️ `list_held` 的 `total` 与 `current_held` **复用同一次 COUNT 的同一个值**
//! （不是两条 SQL 两次查询）—— 见 `service.rs::list_held`。
//!
//! ## 表依赖
//! `t_part` / `t_part_batch` / `t_work_type_process` / `t_shelf` / `t_process` /
//! `t_process_chain` / `t_process_chain_step`，外加 2026-10-14 起 `/held` 容量字段
//! 所需的 `t_worker` / `t_work_type`（`fetch_max_held_batches` 的一条
//! `LEFT JOIN`），以及 2026-10-15 起 `/locate` 追加的 `t_worker` / `t_work_type`
//! （工人身份 + 工种容量）+ `t_outsource_company` / `t_outsource_shipment`
//! （外协在途公司与发出时间）。全部经本目录的 SQL 直读，一处他域的 service /
//! repo 都不 import。

pub mod repo;
pub mod service;

pub use repo::ScanListingRepo;
pub use service::ScanListingService;

#[cfg(test)]
mod tests {
    //! 域隔离护栏：listing 聚合代码不依赖其它域。
    //!
    //! 探测器实现（剥注释、根段 + 域路径前缀匹配、元测试）见
    //! [`crate::shared::domain_guard`]，本模块只负责传参 + 域专属指引。

    use std::path::Path;

    use crate::shared::domain_guard::assert_no_foreign_domain;

    /// listing 聚合 SQL 需要的数据（工单 / 批次 / 工种↔工序映射 / 货架 / 工序 /
    /// 工艺链 step / 工人 + 工种容量上限 / 外协公司与在途 shipment）全部在
    /// `listing/repo.rs` 的 SQL 里聚合，代码区不应出现任何其它域路径（含 `part` /
    /// `prod::batch` / `prod::worker` 等同父兄弟域）。
    ///
    /// 这条护栏是 `prod::scan` **唯一的**域隔离约束 —— 整域其余部分（写路径）
    /// 是转发型，本来就不适用。
    #[test]
    fn listing_aggregation_depends_on_no_other_domain() {
        assert_no_foreign_domain(
            "prod::scan",
            &Path::new(env!("CARGO_MANIFEST_DIR")).join("src/modules/prod/scan/listing"),
            "报工台的 listing 只需要读 t_part / t_part_batch / t_work_type_process / \
             t_shelf / t_process / t_process_chain / t_process_chain_step / \
             t_worker / t_work_type / t_outsource_company / t_outsource_shipment，\
             直接在本目录的 SQL 里聚合即可。不要 import 别人的 service / repo —— \
             跨域设施走 `crate::shared::…`（如 `shared::batch::chain` 的链位置片段、\
             `shared::shelf` 的负载聚合），那些不是域。",
        );
    }
}

//! prod::queue 队列板子模块（只读聚合，2026-10-08 新增）
//!
//! 两个只读端点的实现：
//! - `GET /api/v2/prod/queue/snapshot` —— 工序序列板
//! - `GET /api/v2/prod/queue/processes/{process_id}` —— 单工序板
//!
//! ## 为什么要独立成子模块而不是塞进 `repo/`
//!
//! 域隔离护栏 `assert_no_foreign_domain` 收的是**目录**路径。要让「board 聚合
//! SQL 零跨域依赖」这条规则可被 CI 执行，被扫描的代码必须自成一个目录。
//! queue 域整体**不适用**该护栏（它继承 worker_pool 的「经本域 trait 转发其它域
//! 单表查询」pattern，见 `repo/mod.rs` 顶部记档），但 board 这部分聚合 SQL 是
//! 纯只读的、完全可以零跨域 —— 把它圈出来单独守，比整域不守要强。
//!
//! ## SQL 条数（与工人数 / 批次数无关）
//! - `board_snapshot`：**3 条**（计数 / 工序元数据 / 待下发计数）
//! - `board_process_detail`：**6 条**（工序元数据 / 工人+工种 max_held /
//!   全部工人持有批次一次 ANY / 候选池 / 待下发计数）
//!
//! 旧路径（`GET /pool/{process_id}` + 逐 worker `GET /pool/state`）在 10 个工人
//! 时要发 1 + 1 + 10 = 12 个 HTTP 请求；新路径恒定 1 个。

pub mod repo;
pub mod service;

pub use repo::QueueBoardRepo;
pub use service::QueueBoardService;

#[cfg(test)]
mod tests {
    //! 域隔离护栏：board 聚合代码不依赖其它域。
    //!
    //! 探测器实现（剥注释、根段 + 域路径前缀匹配、元测试）见
    //! [`crate::shared::domain_guard`]，本模块只负责传参 + 域专属指引。

    use std::path::Path;

    use crate::shared::domain_guard::assert_no_foreign_domain;

    /// board 聚合 SQL 需要的数据（工序 / 工人 / 工种 / 批次 / 工单 / 客户 /
    /// 申请人 / 货架）全部在 `board/repo.rs` 的 SQL 里聚合，代码区不应出现任何
    /// 其它域路径（含 `prod::batch`、`prod::process`、`prod::worker` 等
    /// 同父兄弟域）。
    #[test]
    fn board_aggregation_depends_on_no_other_domain() {
        assert_no_foreign_domain(
            "prod::queue",
            &Path::new(env!("CARGO_MANIFEST_DIR")).join("src/modules/prod/queue/board"),
            "需要别的域的数据时，正确做法是像 dashboard 域那样在本模块 SQL 里只读聚合\
             （读 t_process / t_worker / t_work_type / t_work_type_process / \
             t_part_batch / t_part / t_customer / t_applicant / t_shelf），\
             而不是 import 别人的 service / repo。\
             queue 域的**写端点**（refill / move / dispatch）走 `QueueRepoTrait`\
             转发其它域单表查询是既有 pattern，但那是写路径，与本护栏无关 —— \
             护栏只扫本 `board/` 目录。",
        );
    }
}

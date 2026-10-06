//! 跨域批次设施层（2026-10-08 新增）
//!
//! 抽取动机：逐域剥离（batch → queue / scan / inspection / delivery / repair /
//! outsource / cnc）之前，**所有**模块都要用到的批次设施住在 `prod::batch` 域内，
//! 于是任何新域一旦碰批次就得反向依赖 batch 域（queue 域的 `get_by_id`、recall
//! 都属此类）。这层设施的本质是「批次这一张表的公共语义」，与「批次有哪些业务
//! 用例」无关，故整体上移到 shared。
//!
//! | 文件 | 内容 | 依赖 |
//! |---|---|---|
//! | [`model`] | `TPartBatch` 全列行结构 | 仅 chrono |
//! | [`read`] | `get_batch_by_id` / `list_active_batches_by_part_id` | `model` + sqlx |
//! | [`status`] | `apply_batch_status_change` / `apply_bulk_batch_status_change_for_part` + 派生链 | assembly / part / 本表（见下） |
//! | [`guards`] | 7 个自由函数：状态机守卫 / OCC / 货架校验 / status 薄包装 | part / shelf / `status` |
//!
//! ## 边界登记：shared/batch 是本仓唯一经域 repo 写库的 shared 模块
//!
//! [`status`] 经 `PartRepo::update_part_rollup`（part 域）与
//! `AssemblyService::sync_assembly_status`（assembly 域）写库，即 shared 层
//! 反过来调域的 repo。**这是有意为之的例外**，理由：它是 `CLAUDE.md`
//! 「状态派生契约」三层派生图
//!
//! ```text
//! t_part_batch.status            ← 唯一真源
//!    │  rollup_part_derived（min-progress）
//!    ▼
//! t_part.status / next_process_id   ← 派生缓存
//!    │  compute_assembly_target
//!    ▼
//! t_assembly.status               ← 派生缓存
//! ```
//!
//! 的实现本身。该契约的定义域就是「跨三张表的三层单向派生」，天然无域归属可循；
//! 把它留在 batch 域只会让 part / assembly 域反向依赖 batch 域，与本轮剥离方向
//! 相反。上移到 shared 后，**派生方向与域依赖方向一致**（上层域 → shared）。
//!
//! 换言之：`shared::customer`（读 `t_customer`，无反向写）代表 shared 的常态，
//! `shared::batch` 是唯一例外，且例外的成因是「跨域契约」而非「图省事」。
//!
//! ## 不放什么
//!
//! - 批次**列表**查询（`list_by_part_id` / `list_by_delivery_note` / 窄投影
//!   行）—— 各域列表端点的列集与筛选语义各不相同，逐域自持；
//! - 批次**流转**用例（送检 / 发货 / 返修 / 外协）—— 属 batch 域或已剥离的新域。

pub mod guards;
pub mod model;
pub mod read;
pub mod status;

pub use guards::{
    assert_shelf_maps_process, ensure_transition, mark_batch_status_only,
    mark_batch_with_status_and_meta, optional_process_chain, optional_step_id,
    status_guard_for_target, validate_batch_version, validate_shelf_zone, InspectionRepairRow,
};
pub use model::TPartBatch;

pub use read::{get_batch_by_id, list_active_batches_by_part_id};
pub use status::{
    apply_batch_status_change, apply_bulk_batch_status_change_for_part, rollup_part_derived,
    PartDerivation, RollupOutcome, StatusChange,
};

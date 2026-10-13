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
//! | [`guards`] | 9 个 `pub fn`：状态机守卫 / OCC / 货架校验 / status 薄包装 | `status_machine` / iam / `status` |
//! | [`status_machine`] | `PartStatus` 枚举 + `can_transition_to` + rollup 纯函数（`part_status_progress` / `compute_part_target` / `BatchForRollup` / `PartRollupTarget`）—— **2026-10-13 自 `part::statemachine` 上提**，零外部依赖 | 无（纯函数） |
//! | [`chain`] | `CHAIN_POSITION_LATERAL_SQL` + `HAS_PROCESS_CHAIN_EXPR` + `resolve_chain_position`（批次在工序链上的位置派生，读写共用） | `model` + 本表 SQL 内聚合（见下） |
//!
//! ## 边界登记：shared/batch 依赖 4 个域，是本仓依赖面最宽的 shared 模块
//!
//! > ⚠️ **反向依赖（`shared → modules`）已在 2026-10-13 消除**：本表 4 行全是
//! > 「shared 经**域 repo** 读写别域的表」，方向仍是 `shared → modules`，但那是
//! > **既有的、已登记的**例外（派生链的实现本身）。本轮消除的是另一类问题：
//! > `shared/batch/{status,guards}.rs` 直接 `use crate::modules::part::statemachine::…`
//! > 这类「shared 借用某个域的纯逻辑」—— `PartStatus` / `compute_part_target` 已上提
//! > 到本模块的 [`status_machine`]，part 域的 `statemachine.rs` 随之整文件删除。
//!
//! | 域 | 经 shared 的哪个文件 | 读 / 写 | 入口 |
//! |---|---|---|---|
//! | `part` | `status` | **写** | `PartRepo::update_part_rollup`（`part` 派生列回填） |
//! | `assembly` | `status` | **写** | `AssemblyService::sync_assembly_status`（父件派生级联） |
//! | `iam` | `guards` | 读 | `ShelfRepo::get_by_id`（`validate_shelf_zone` 的存在 / 停用 / zone 三谓词在 Rust 层逐条判） |
//! | `prod::process_chain` | `guards` | 读 | `ProcessChainRepo::resolve_step_id_by_process`（`optional_step_id`） |
//!
//! ## `chain` 的表依赖（不经域 repo，与上表 4 行不同类）
//!
//! | 表 | 读 / 写 | 为什么不经域 repo |
//! |---|---|---|
//! | `t_part`（`process_chain_id`） | 只读 | 锚链的第一来源。`ProcessChainRepo` 的入口全部以 `chain_id` 为入参（「某条链如何」），而本层问的是「这个 part 当前锚在哪条链」—— 与 `optional_process_chain`（`guards`）读的是同一个列，两处各写一份 SQL 只会让锚链口径漂移 |
//! | `t_part_process_chain` | 只读 | `CHAIN_POSITION_LATERAL_SQL` 的锚链 JOIN 查的就是它，而 `pc.deleted_at IS NULL` 这一条决定「锚链能否解析」：链被软删时位置解析无行、落 `NONE`。写侧口径**只对齐了一半**：`ProcessChainRepo::first_step_in_chain` 补上了同一道软删闸门（dispatch 侧），而 `resolve_step_id_by_process` 只查 `t_process_chain_step`、**无这道闸门** ⇒ worker-scan 显式分支仍能在已软删链里解析出活跃 step（分叉登记见 `docs/api/queue.md` §8.4） |
//! | `t_process_chain_step` | 只读 | 「链内定位 + 下一道」必须与读侧 `LEFT JOIN LATERAL` 在**同一条 SQL** 里完成（拆成两条往返会让「定位到的位置」与「算出的下一道」之间出现写窗口）。同理 `NEXT_PROCESS_LATERAL_SQL`（`outsource` 域自己的同款片段）也是片段内自聚合 |
//!
//! 两条纪律（锚链两步定位、按 `current_process_id` 重新定位）与理由见
//! [`chain`] 的模块 doc；纪律的文本锚点由该文件的单测钉住。
//!
//! 前两行是**写**库，故 `shared::batch` 是本仓**唯一一个「经域 repo 写别域的表」的
//! shared 模块**（2026-10-12 起 `t_part_event` 的写入也已上提到 `shared::part_event`，
//! 但那一侧是**自己的表、不经域 repo**，故本句的「唯一」口径不变）。这是有意为之的
//! 例外，理由：它是 `CLAUDE.md`「状态派生契约」三层派生图
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
//! 后两行是**只读**单表查询：守卫的判序与文案是全仓一份的公共语义，留域内会让
//! 每个新域反向依赖 batch（见 `guards` 模块 doc 的 `current_holder_id` 三写点清单）。
//!
//! 换言之：`shared::customer`（读 `t_customer`，无反向写）代表 shared 的常态，
//! `shared::batch` 是唯一经域写库（且写的不是自己的表）、也是唯一依赖 4 个域的模块，
//! `shared::part_event` 则是唯一「写自己的表」的 shared 模块；三者的成因
//! 都是「跨域契约」而非「图省事」。
//!
//! ## 不放什么
//!
//! - 批次**列表**查询（`list_by_part_id` / `list_by_delivery_note` / 窄投影
//!   行）—— 各域列表端点的列集与筛选语义各不相同，逐域自持；
//! - 批次**流转**用例（送检 / 发货 / 返修 / 外协）—— 属 batch 域或已剥离的新域。

pub mod chain;
pub mod guards;
pub mod model;
pub mod read;
pub mod status;
pub mod status_machine;

pub use guards::{
    InspectionRepairRow, ensure_transition, mark_batch_status_only,
    mark_batch_with_status_and_meta, optional_process_chain, optional_step_id,
    status_guard_for_target, validate_batch_version, validate_shelf_zone,
};
pub use model::TPartBatch;

pub use read::{get_batch_by_id, list_active_batches_by_part_id};
pub use status::{
    PartDerivation, RollupOutcome, StatusChange, apply_batch_status_change,
    apply_bulk_batch_status_change_for_part, rollup_part_derived,
};
// ⚠️ 2026-10-13：`status_machine`（自 `part::statemachine` 上提的 PartStatus /
// rollup 纯函数）**刻意不做平铺 `pub use`** —— 域侧一律走显式路径
// `shared::batch::status_machine::X`。理由：这三类符号（状态枚举、rollup 输入
// 投影、rollup 纯函数）在全仓到处用，平铺到 `shared::batch::*` 面上会与
// `guards` / `status` 的导出混在一起、看不出它们的归属子模块。

//! `t_part_event` 写侧设施层（2026-10-12 自 `modules::part` 上提到 `shared`）
//!
//! | 文件 | 内容 | 依赖 |
//! |---|---|---|
//! | [`model`] | `NewPartEvent<'a>` builder（15 列取 13 列） | 无（零 import） |
//! | [`sql`] | `insert`（唯一写点，`sqlx::query!` 宏） | `model` + sqlx |
//!
//! ## 为什么无域归属
//!
//! `t_part_event` 是工单级事件流水，**7 个模块都在写**，且没有一个是 owner：
//!
//! | 写方 | 写什么事件 |
//! |---|---|
//! | `part` | `CANCELLED` / `FORCE_COMPLETED` / `SOFT_DELETED` |
//! | `prod::batch` | `INSPECTED` / `BATCH_PASSED` / `INSPECTION_FAILED` / `DELIVERED` / `COMPLETED` / `REPAIR_STARTED` / `REPAIR_COMPLETED` / `SPLIT` / `BATCH_CANCELLED` / `PLACED_ON_SHELF` / `CNC_RELEASED` / `STATUS_CHANGED` |
//! | `prod::scan` | `PICKED_UP` / `SPLIT` / `RETURNED_TO_SHELF` / `SENT_TO_INSPECTION` |
//! | `prod::queue` | `TAKEN_FROM_POOL` / `MOVED` / `RECALLED` / `PLACED_ON_SHELF` |
//! | `outsource` | `OUTSOURCE_*` 三条流转字面量（`write_part_event`） |
//! | `assembly` | `FORCE_COMPLETED`（子件强制完成） |
//! | `shared::batch` | `SERIAL_RELEASED`（终态序列号归档） |
//!
//! 留在 `part` 域的后果就是其余 6 个写方全部反向依赖 `modules::part` —— 与逐域剥离
//! 方向相反；留在任何单一「用例域」（比如 `prod::batch`）后果相同。故与
//! `shared::batch`（批次公共语义）同批上提到 `shared`，依赖方向变成
//! **域 → shared**。
//!
//! ## 为什么本表没有 `T*` 投影
//!
//! 仓库惯例：`T*` = 某域 **owned** 的表的全列 `#[derive(sqlx::FromRow)]` 投影。
//! 全仓 `pub struct T*` 实测 19 个（`customer` / `applicant` / `assembly` / `part_file` /
//! `part` ×3 / `process` / `outsource` ×5 / `worker` / `work_type` / `process_chain` ×2 /
//! `shelf` / `shared::batch::TPartBatch`），**全部 derive FromRow、零例外**。
//!
//! `t_part_event` 不满足「owned」，因而也不该有 `T*` 投影：本表读侧一律走**端点私有
//! row struct + VO** —— 零件时间线用 `modules::part::service::phase1::events` 内的
//! `EventListRow`，statistics 域用自己的 row struct。给一张读侧人人自定义列集的表造一个
//! 全列的公共投影，只会让「哪一列是端点要的」这件事被投影结构掩盖。
//!
//! ## 为什么 builder 保留 `New` 前缀（而不是改名 `PartEvent`）
//!
//! 命名规则：`T*` = 全列投影，`New*` = **INSERT 输入 builder**。`NewPartEvent` 是
//! **15 列的 13 列子集**，两列不写：
//!
//! - `created_at` —— 走 DB 默认 `now()`（INSERT 里是字面量而非 bind，builder 里没有这个字段）；
//! - `outsource_company_id` —— Rust 写侧**零写方**（外协流转事件只写 `note` 里的字面量）。
//!
//! 与 [`crate::modules::prod::process_chain::model::NewProcessChainStep`]（12 列取
//! 4 列）**同性质**：都是「表有几列、INSERT 只落其中一部分」的 builder，不是投影。
//! 按规则保留 `New` 前缀。
//!
//! ## 2026-10-12：两个新字段的写入点清单
//!
//! 两列此前**从未被 Rust 写侧写过**（老 INSERT 的列清单里根本没有它们），本次逐个
//! 构造点决定填不填。
//!
//! ### `worker_id`（`t_worker.id`，**不是** `t_user.id`）
//!
//! | 有值 | 位置 | 取值依据 |
//! |---|---|---|
//! | ✅ 2 处 | `prod::scan::service::worker_scan`（`RETURNED_TO_SHELF` / `SENT_TO_INSPECTION`） | 形参 `worker: &TWorker` ⇒ `Some(worker.id)` |
//! | ✅ 2 处 | `prod::scan::service::pickup`（`SPLIT` / `PICKED_UP`） | `Some(req.worker_id)` —— 该 worker 已在函数前半段查库校验存在且 active |
//! | ✅ 2 个调用点 | `prod::queue::service::queue`（2 处 `TAKEN_FROM_POOL`） | 形参 `worker_id`（已被 worker 取料 SQL 验过 `t_worker.id`） |
//! | ❌ 其余全部写点 | 见下 | 见下 |
//!
//! `None` 的理由（**不是**「取不到」而是「语境里没有作业人」）：这批事件是 part 级 /
//! 批次级的状态流转（`DELIVERED` / `INSPECTED` / `STATUS_CHANGED` / `REPAIR_*` /
//! `BATCH_CANCELLED` / `RECALLED` / `SERIAL_RELEASED` / `FORCE_COMPLETED` /
//! `CANCELLED` / `SOFT_DELETED` / `PLACED_ON_SHELF` / `CNC_RELEASED` / `SPLIT` /
//! `MOVED`），发起者是**管理员账号或系统调度**而非工人。上下文里拿得到的是操作者
//! `t_user.id`（`created_by` 那一列），与 `t_worker.id` 是**两回事**——两者共用雪花
//! ID 空间但无映射表，statistics 侧 `JOIN t_worker w ON w.id = e.worker_id` 命中不了
//! 就是 0 行。混填 `created_by` 会让「按工人聚合」统计到从未做过活的账号上。
//!
//! ### `process_id`（事件时刻的工序快照，migration 009）
//!
//! | 有值 | 位置 | 取值依据 |
//! |---|---|---|
//! | ✅ 4 处 | `prod::scan::service::worker_scan`（2 处）、`prod::scan::service::pickup`（2 处） | `Some(batch.current_process_id)`，见下 |
//! | ❌ 3 个调用点 | `prod::queue::service::queue` 的 3 处 `MOVED` | `MOVED` 有三个方向（POOL→WORKER / WORKER→POOL / WORKER→WORKER），只有第一个是「开始加工」，后两个分别是「放回」与「转手」；而有效工时口径只用 `RETURNED_TO_SHELF` + `SENT_TO_INSPECTION` ⇒ 填不填都不进任何聚合，一律 `None` 少一处需要推理的地方 |
//! | ❌ 2 个调用点 | `prod::queue::service::queue` 的 2 处 `TAKEN_FROM_POOL` | 该路径手上只有 `TakenItem`，**取不到批次工序**（`prod::queue::repo::sql::TakenRow` / 最终 SELECT 都不含 `pb.current_process_id`，`worker_id` 能拿到而 `process_id` 拿不到）。补它要连带改 `TakenRow` + CTE + RETURNING + `TakenItem` 四个落点并改动 `/prod/queue/refill` 响应契约，超出本轮范围 |
//! | ❌ 其余全部写点 | 见下 | 这些事件发生时批次确实带着 `current_process_id`，但它们既不是「开始加工」也不是「加工完毕」⇒ 工序归属对统计无贡献；一并 `None` |
//!
//! 4 处「有值」为什么取 `batch.current_process_id` 就够：
//!
//! - `RETURNED_TO_SHELF`：`mark_batch_returned` 在**同一事务内**已把
//!   `t_part_batch.current_process_id` 推进到 `next_pid`，但手上看的是**推进前**读进
//!   内存的 `batch` 快照 ⇒ `batch.current_process_id` 正是「这位工人刚做完哪道工序」。
//!   这正是 migration 009 要快照的原因。
//! - `SENT_TO_INSPECTION`：`mark_batch_inspected` 不推进工序，值不变。
//! - `pickup` 的 2 处：部分领取走 `split_batch_for_partial_pass`，该 SQL
//!   `INSERT ... SELECT` **继承**源批次的 `current_process_id`（另见
//!   `prod::scan::service::pickup` 处 `pick_up` 的「部分领取语义」段），且随后的
//!   `mark_batch_with_status_and_meta(..., batch.current_process_step_id,
//!   batch.current_process_id, ...)` 又显式把它透传给新批次 ⇒ 拆批前后同值，
//!   `SPLIT` / `PICKED_UP` 两条事件取源批次的 `batch.current_process_id` 即可。

pub mod model;
pub mod sql;

pub use model::NewPartEvent;
pub use sql::insert;

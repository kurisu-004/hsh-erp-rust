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
//! | ✅ 3 处 | `prod::scan::service::worker_scan`（`RETURNED_TO_SHELF` / `SENT_TO_INSPECTION` / 部分数量拆批的 `SPLIT`） | 形参 `worker: &TWorker`（`resolve_operated_batch` 也按此补了形参）⇒ `Some(worker.id)` |
//! | ✅ 2 处 | `prod::scan::service::pickup`（`SPLIT` / `PICKED_UP`） | `Some(req.worker_id)` —— 该 worker 已在函数前半段查库校验存在且 active |
//! | ✅ 2 个调用点 | `prod::queue::service::queue`（2 处 `TAKEN_FROM_POOL`） | 形参 `worker_id`（已被 worker 取料 SQL 验过 `t_worker.id`） |
//! | ❌ 其余全部写点 | 见下 | 见下 |
//!
//! `None` 的理由（**不是**「取不到」而是「语境里没有作业人」）：这批事件是 part 级 /
//! 批次级的状态流转（`DELIVERED` / `INSPECTED` / `STATUS_CHANGED` / `REPAIR_*` /
//! `BATCH_CANCELLED` / `RECALLED` / `SERIAL_RELEASED` / `FORCE_COMPLETED` /
//! `CANCELLED` / `SOFT_DELETED` / `PLACED_ON_SHELF` / `CNC_RELEASED` / `MOVED`，
//! 以及 `prod::batch` 那条**手工**拆批 `SPLIT`（`POST /batches/split`，管理员发起），
//! 发起者是**管理员账号或系统调度**而非工人。上下文里拿得到的是操作者 `t_user.id`
//! （`created_by` 那一列），与 `t_worker.id` 是**两回事**——两者共用雪花 ID 空间但无
//! 映射表，statistics 侧 `JOIN t_worker w ON w.id = e.worker_id` 命中不了就是 0 行。
//! 混填 `created_by` 会让「按工人聚合」统计到从未做过活的账号上。
//!
//! ### `process_id`（事件时刻的工序快照，migration 009）
//!
//! | 有值 | 位置 | 取值依据 |
//! |---|---|---|
//! | ✅ 5 处 | `worker_scan` 3 处、`pickup` 2 处 | 取值来源分两档，见下 |
//! | ❌ 3 个调用点 | `prod::queue::service::queue` 的 3 处 `MOVED` | `MOVED` 有三个方向（POOL→WORKER / WORKER→POOL / WORKER→WORKER），只有第一个是「开始加工」，后两个分别是「放回」与「转手」；而有效工时口径只用 `RETURNED_TO_SHELF` + `SENT_TO_INSPECTION` ⇒ 填不填都不进任何聚合，一律 `None` 少一处需要推理的地方 |
//! | ❌ 2 个调用点 | `prod::queue::service::queue` 的 2 处 `TAKEN_FROM_POOL` | 该路径手上只有 `TakenItem`，**取不到批次工序**（`prod::queue::repo::sql::TakenRow` / 最终 SELECT 都不含 `pb.current_process_id`，`worker_id` 能拿到而 `process_id` 拿不到）。补它要连带改 `TakenRow` + CTE + RETURNING + `TakenItem` 四个落点并改动 `/prod/queue/refill` 响应契约，超出本轮范围 |
//! | ❌ 其余全部写点 | 见下 | 这些事件发生时批次确实带着 `current_process_id`，但它们既不是「开始加工」也不是「加工完毕」⇒ 工序归属对统计无贡献；一并 `None` |
//!
//! 5 处「有值」分两档取值，判据是**「手上有没有事件锚（`batch_id` 指向）那一批的行」**：
//!
//! **一档 · 有事件锚那一批的行，取它（`worker_scan` 的 `RETURNED_TO_SHELF` /
//! `SENT_TO_INSPECTION`）**：
//!
//! - `RETURNED_TO_SHELF`：取 `operated.current_process_id` —— `operated` 就是事件
//!   `batch_id` 指向的那一批、也是 `mark_batch_returned` 实际推进的那一批（部分数量
//!   拆批路径下它是拆完 `find_batch_by_id` 重读的真行）。该调用在同一事务内已把这行
//!   的 `current_process_id` 推进到 `next_pid`，而 `operated` 是**推进前**读进内存的
//!   快照 ⇒ 该值正是「这位工人刚做完哪道工序」，这正是 migration 009 要快照的原因；
//!   事后回查批次只会拿到「下一道」。
//! - `SENT_TO_INSPECTION`：取形参 `batch.current_process_id` —— 形参本身就是事件锚
//!   （`batch_id` / OCC / 状态机守卫都用它），且 `mark_batch_inspected` 不推进工序，
//!   值不变。
//!
//! **二档 · 手上只有源批次（拆出来的新批次只有 id、没有行），取源批次的
//! `current_process_id`（`worker_scan` 的拆批 `SPLIT`、`pickup` 的 2 处）**：
//!
//! `split_batch_for_partial_pass` 的 `INSERT ... SELECT` **继承**源批次的
//! `current_process_id`，源批次的 UPDATE 又只动 `quantity` + `version`
//! （`pickup` 侧还经 `mark_batch_with_status_and_meta(..., batch.
//! current_process_step_id, batch.current_process_id, ...)` 显式透传）⇒ 新旧两批
//! 同工序，故取源快照与取事件锚等价。`worker_scan` 的拆批 `SPLIT` 尤其如此：它写事件
//! 时新批次行还没被读回来（`find_batch_by_id` 在事件之后），源快照是唯一可取值 ——
//! 与 `pickup` 那条 `SPLIT` **逐字同口径**。
//!
//! **为什么分两档而不是统一取源批次**：两档的**运行结果**一致（拆批只继承、不改工序），
//! 区别在抗漂移。一档那两处的源批次仍在作用域内，一旦有人给源批次的 UPDATE 加上工序
//! 推进，「取源批次」会静默记成「下一道工序」；取事件锚那一批的推进前快照天然免疫。

pub mod model;
pub mod sql;

pub use model::NewPartEvent;
pub use sql::insert;

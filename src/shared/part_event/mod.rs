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
//! | `part` | `CANCELLED`（取消工单）/ `FORCE_COMPLETED`（零件级强推）/ `SOFT_DELETED` |
//! | `prod::batch` | `INSPECTED` / `FORCED_SENT_TO_INSPECTION`（★ 2026-10-17 新增）/ `BATCH_PASSED` / `INSPECTION_FAILED`（`scan_inspect` 的 PASS/FAIL 结果对）/ `STATUS_CHANGED`（to-ship）/ `DELIVERED` / `COMPLETED`（lifecycle）/ `REPAIR_STARTED` / `REPAIR_COMPLETED` / `SPLIT`（手工 `POST /batches/split`）/ `BATCH_CANCELLED`（批量取消）/ `PLACED_ON_SHELF`（`place-on-shelf`）/ `CNC_RELEASED` |
//! | `prod::scan` | `PICKED_UP` / `SPLIT`（取件与 worker-scan 的部分数量拆批）/ `RETURNED_TO_SHELF` / `SENT_TO_INSPECTION`（`worker_scan`）/ `TRANSFERRED`（扫码接管，2026-10-15） |
//! | `prod::queue` | `TAKEN_FROM_POOL`（2 处）/ `MOVED`（3 处）—— 这 5 处经 `repo::part_insert_part_event` 这个**转发器**写入，块内只有字段简写、字面量在实参；`RECALLED`（`service/recall.rs`，**裸字面量** `NewPartEvent` 构造）/ `PLACED_ON_SHELF`（`service/dispatch.rs`，**裸字面量** `NewPartEvent` 构造）
//! | `outsource` | `SENT_TO_OUTSOURCE` / `RECEIVED_FROM_OUTSOURCE`（经 `write_part_event` **转发器**，字面量在调用点） |
//! | `assembly` | `FORCE_COMPLETED`（装配件级强推其子件） |
//! | `shared::batch` | `SERIAL_RELEASED`（终态序列号归档） |
//!
//! ⚠️ **2026-10-17 订正**：本表此前是**建表时**的旧快照 —— 漏了 `TRANSFERRED`
//! （2026-10-15 新增），把外协两条写成了**不存在的** `OUTSOURCE_*` 三条，
//! `prod::batch` 行也漏了 `DELIVERED` / `COMPLETED` 两条 lifecycle 事件
//! （它们是 `service/lifecycle.rs` 的两个独立写点）。上表已按**当前代码**逐构造点
//! 复核重建（`NewPartEvent {` 的全部字面量构造点 + 2 个转发器的调用点；
//! ⚠️ **刻意不写处数** —— 那类计数必然过期，`sql.rs` 的模块 doc 已经因此过期过一次）。
//! ⚠️ **2026-10-17 review 第 1 轮再订正**：`prod::queue` 行此前把四个事件一律说成
//! 「都经转发器写入」，实测转发器只有 **5 个**调用点（`TAKEN_FROM_POOL` ×2 /
//! `MOVED` ×3，全在 `service/queue.rs`），`RECALLED` / `PLACED_ON_SHELF` 是**直接**
//! 的 `NewPartEvent` 字面量构造。该行已按形态拆开 —— **本表里「谁写」与「怎么写」
//! 是两列事实，混写一处就会把转发器的覆盖面说大**。
//! ⚠️ **事件类型的全集以 [`ALL_EVENT_TYPES`] 为准**，本表只答「哪个模块写哪些」
//! —— 两份的改一同步二义务见 [`ALL_EVENT_TYPES`] 的 doc。
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
//! | ✅ 1 处 | `prod::scan::service::transfer`（`TRANSFERRED`，2026-10-15 扫码接管新增） | 形参 `worker`（工牌反查出的**接管人**）⇒ `Some(worker.id)`；旧持有人只进 `note` 文本，不填本列 |
//! | ✅ 2 个调用点 | `prod::queue::service::queue`（2 处 `TAKEN_FROM_POOL`） | 形参 `worker_id`（已被 worker 取料 SQL 验过 `t_worker.id`） |
//! | ❌ 其余全部写点 | 见下 | 见下 |
//!
//! `None` 的理由（**不是**「取不到」而是「语境里没有作业人」）：这批事件是 part 级 /
//! 批次级的状态流转（`DELIVERED` / `INSPECTED` / `FORCED_SENT_TO_INSPECTION` /
//! `STATUS_CHANGED` / `REPAIR_*` / `BATCH_CANCELLED` / `RECALLED` / `SERIAL_RELEASED` /
//! `FORCE_COMPLETED` / `CANCELLED` / `SOFT_DELETED` / `PLACED_ON_SHELF` /
//! `CNC_RELEASED` / `MOVED`，以及 `prod::batch` 那条**手工**拆批 `SPLIT`
//! （`POST /batches/split`，管理员发起），发起者是**管理员账号或系统调度**而非工人。
//! 上下文里拿得到的是操作者 `t_user.id`
//! （`created_by` 那一列），与 `t_worker.id` 是**两回事**——两者共用雪花 ID 空间但无
//! 映射表，statistics 侧 `JOIN t_worker w ON w.id = e.worker_id` 命中不了就是 0 行。
//! 混填 `created_by` 会让「按工人聚合」统计到从未做过活的账号上。
//!
//! ⚠️ **`FORCED_SENT_TO_INSPECTION` 尤其不能填原工人**（2026-10-17）：它是运营代工人
//! 做的一次越权送检，**业务上就不该**算进该工人的报工统计 —— 填了就等于让运营的一次
//! 兜底操作变成工人的有效工时。
//!
//! ### `process_id`（事件时刻的工序快照，migration 009）
//!
//! | 有值 | 位置 | 取值依据 |
//! |---|---|---|
//! | ✅ 6 处 | `worker_scan` 3 处、`pickup` 2 处、`transfer` 1 处 | 取值来源分三档，见下 |
//! | ❌ 3 个调用点 | `prod::queue::service::queue` 的 3 处 `MOVED` | `MOVED` 有三个方向（POOL→WORKER / WORKER→POOL / WORKER→WORKER），只有第一个是「开始加工」，后两个分别是「放回」与「转手」；而有效工时口径只用 `RETURNED_TO_SHELF` + `SENT_TO_INSPECTION` ⇒ 填不填都不进任何聚合，一律 `None` 少一处需要推理的地方 |
//! | ❌ 2 个调用点 | `prod::queue::service::queue` 的 2 处 `TAKEN_FROM_POOL` | 该路径手上只有 `TakenItem`，**取不到批次工序**（`prod::queue::repo::sql::TakenRow` / 最终 SELECT 都不含 `pb.current_process_id`，`worker_id` 能拿到而 `process_id` 拿不到）。补它要连带改 `TakenRow` + CTE + RETURNING + `TakenItem` 四个落点并改动 `/prod/queue/refill` 响应契约，超出本轮范围 |
//! | ❌ 其余全部写点 | 见下 | 这些事件发生时批次确实带着 `current_process_id`，但它们既不是「开始加工」也不是「加工完毕」⇒ 工序归属对统计无贡献；一并 `None` |
//!
//! 6 处「有值」分三档取值，判据是**「手上有没有事件锚（`batch_id` 指向）那一批的行」**：
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
//! `current_process_id`，源批次的 UPDATE 又只动 `quantity` + `version` ⇒ 新旧两批
//! 同工序，故取源快照与取事件锚等价。`worker_scan` 的拆批 `SPLIT` 尤其如此：它写事件
//! 时新批次行还没被读回来（`find_batch_by_id` 在事件之后），源快照是唯一可取值 ——
//! 与 `pickup` 那条 `SPLIT` **逐字同口径**。
//!
//! ⚠️ **2026-10-15 S2-B 起 `pickup` 的 PENDING 分支是一个刻意例外**：链首落位解析出的
//! `placed_process_id` 既不是源批次现值（PENDING 行两列恒 NULL）、也不是拆批继承 ——
//! 它是这批货领取后的**目标工序**（链首）。写侧直接取 `placed_process_id`，故
//! 「新旧两批同工序」这个前提在「PENDING + 有链」时不成立（源批次保持 NULL、
//! 新批次落链首）。IN_PROCESS 起点与无链 PENDING 仍与改动前逐字一致。
//!
//! **三档 · 目标工序（扫码接管，`transfer` 1 处）**：`TRANSFERRED` 取接管后的目标工序
//! `next_pid`（链上推导或前端显式指定）—— 接管动作的本质是「这一批从现在起在新工序上
//! 开工」，与 RETURNED 的「刚做完哪道」方向相反。它不进有效工时聚合（口径只认
//! `RETURNED_TO_SHELF` / `SENT_TO_INSPECTION`），值只服务 breakdown / 时间线显示。
//!
//! **为什么一、二档不统一取源批次**：两档的**运行结果**一致（拆批只继承、不改工序），
//! 区别在抗漂移。一档那两处的源批次仍在作用域内，一旦有人给源批次的 UPDATE 加上工序
//! 推进，「取源批次」会静默记成「下一道工序」；取事件锚那一批的推进前快照天然免疫。

pub mod model;
pub mod sql;

pub use model::NewPartEvent;
pub use sql::insert;

/// `INSPECTED` 事件类型字面量（`t_part_event.event_type`，列是 `varchar(30)`）。
///
/// 2026-10-17 新增。此前写点把它硬写成裸字面量，且**写点分散在两处**
/// （`prod::batch::service::scan::scan_inspect` 与
/// `prod::batch::service::transition_core::to_inspection_core`）—— 打错字没有任何
/// 拦截（该列无 CHECK 约束、无 DB ENUM、读侧也无 Rust enum），故提到这里做具名常量。
/// 与 `shared::batch::status::EVENT_SERIAL_RELEASED` 同一做法，但 `part_event` 是
/// 更合适的家（那张表没有域归属，本模块就是它的写侧设施层）。
pub const EVENT_INSPECTED: &str = "INSPECTED";

/// `FORCED_SENT_TO_INSPECTION` 事件类型字面量（`varchar(30)`，本值 26 字符）。
///
/// 2026-10-17 新增：待品检页对「**压在工人手上**的批次」提供强制送检入口
/// （`POST /prod/batches/{batch_id}/to-inspection` 带 `force=true`）。该动作是
/// **运营代工人做的一次越权送检**，不是工人自己完成的工序，因此：
///
/// - 事件类型与工人自主送检的 `SENT_TO_INSPECTION` 区分开（后者**计入**有效工时）；
/// - 写侧 `worker_id` / `process_id` 一律 `None` —— 两道独立保险里的一道，
///   另一道是 statistics 侧 `effective_minutes::WHERE_SQL` 的
///   `event_type IN ('RETURNED_TO_SHELF','SENT_TO_INSPECTION')` 白名单天然排除本值。
pub const EVENT_FORCED_SENT_TO_INSPECTION: &str = "FORCED_SENT_TO_INSPECTION";

/// 全仓 `t_part_event.event_type` 的**写侧真源**：每一个会被写进该列的字面量。
///
/// 2026-10-17 新增。此前该列是 `varchar(30)`、**无 CHECK 约束、无 DB ENUM**、读侧
/// 也无 Rust enum（`model.rs::NewPartEvent::event_type` 就是 `&'a str`），于是
/// **打错字没有任何拦截** —— 拼错的类型能正常入库，只是前端时间线上按它分组时永远
/// 匹配不到任何一档文案，静默变成一行裸字面量。
///
/// ## 收录标准
///
/// 「全仓会写的每一个 `event_type` 字面量」，含：
/// - `NewPartEvent { event_type: … }` 各构造点（`rg 'NewPartEvent \{'` 逐个打开）；
/// - **间接**形态：`prod::queue` 经 `repo::part_insert_part_event` 的
///   `event_type: &'a str` 转发形参、`outsource` 经 `write_part_event` 的同款转发
///   —— 它们的字面量在**调用点**，不在 `NewPartEvent {` 块内；
/// - **具名常量**形态：`const EVENT_X: &str = "…"`（本模块两个 +
///   `shared::batch::status::EVENT_SERIAL_RELEASED` + `outsource` 两个）；
/// - **迁移回填**：`migrations/20261001000200_007_serial_release.sql` 里的
///   `'SERIAL_RELEASED'`（历史行的类型，字面量与写侧常量同值）。
///
/// ⚠️ **不收录**别的事件表：`t_outsource_quote_event.event_type`
/// （`CREATED` / `SUBMITTED` / `APPROVED` / `REJECTED` / `SENT`）、已删除的
/// `t_delivery_note_event.event_type`、以及 `prod::scan` 出参
/// `WorkerScanEvent`（`WORKER_SCAN_RETURNED` / `WORKER_SCAN_INSPECTED`）与
/// dashboard WS payload —— 它们**同名不同表 / 不同语义**，混进来会让本表既不
/// 「单一真源」也不可核对。
///
/// ## 与前端的同步纪律（**人工同步，无编译期保障**）
///
/// 前端 `frontend/src/types/parts.ts::PART_EVENT_LABEL` 是**UI 文案侧**的真源，
/// 本常量是**写侧**的真源。改一侧**必须**改另一侧（新增类型 = 后端能写出来 + 后端
/// 有中文文案；少一侧的症状都是「某一类事件在时间线上没有文案」或「前端有一档
/// 后端永远写不出来」）。
///
/// ❌ **刻意不用 `include_str!` 跨仓到 frontend 做编译期校验**：`Dockerfile` 的
/// 构建上下文只有 backend-rust 仓（只 `COPY src ./src` 等），跨仓路径会**直接
/// 打断 Docker 构建**。这条禁令是架构事实，不是可以绕过的实现细节。
///
/// ## CI 强制：新增字面量必须登记
///
/// lib 单测 [`event_type_guard_tests::every_new_part_event_literal_is_registered`]
/// 扫 `src/**/*.rs`，把所有 `NewPartEvent { … }` 块内的 `event_type` 字面量与所有
/// `EVENT_*` 常量的值逐个对表（**断言的是集合包含关系，不是基数** —— 加类型不该
/// 要求改测试）。漏报形态登记在该子模块的 doc 顶部。
pub const ALL_EVENT_TYPES: &[&str] = &[
    // part 级（工单整体生命周期，发起者是账号 / 系统）
    "CANCELLED",
    "FORCE_COMPLETED",
    "SOFT_DELETED",
    // 批次级（`prod::batch`）
    "BATCH_CANCELLED",
    "BATCH_PASSED",
    "CNC_RELEASED",
    "COMPLETED",
    "DELIVERED",
    "INSPECTED",
    "INSPECTION_FAILED",
    "PLACED_ON_SHELF",
    "REPAIR_COMPLETED",
    "REPAIR_STARTED",
    "SPLIT",
    "STATUS_CHANGED",
    // 工人作业级（`prod::scan` / `prod::queue`）—— 前两类进有效工时口径
    "MOVED",
    "PICKED_UP",
    "RETURNED_TO_SHELF",
    "SENT_TO_INSPECTION",
    "TAKEN_FROM_POOL",
    "TRANSFERRED",
    // 运营越权代做（**不**计工人工时，见 statistics::repo::sql::effective_minutes）
    "FORCED_SENT_TO_INSPECTION",
    // 外协流转（`outsource`）
    "RECEIVED_FROM_OUTSOURCE",
    "SENT_TO_OUTSOURCE",
    // 队列召回 + 终态清算（`prod::queue` / `shared::batch`）
    "RECALLED",
    "SERIAL_RELEASED",
];

#[cfg(test)]
mod event_type_guard_tests {
    //! 把「新增 `t_part_event` 事件类型必须登记进
    //! [`super::ALL_EVENT_TYPES`]」钉成 CI 强制（2026-10-17）。
    //!
    //! 本仓已有三个同形态的源码扫描型护栏（`shared::snowflake_guard`、
    //! `shared::batch::status::write_guard_tests`、`shared::domain_guard`），
    //! 本子模块逐字沿用它们的那条核心纪律：**判定只认「代码字符」** —— 注释与
    //! 字符串 / raw string / 字符字面量的正文在匹配前已被排除，于是本仓 doc 里
    //! 大量「`event_type='REPAIR_*'` 注释里贴旧写法」的历史溯源文字天然不误报。
    //!
    //! ## 判定口径（两条规则）
    //!
    //! 1. **`NewPartEvent { … }` 块内的 `event_type` 赋值**：值是字符串字面量 ⇒
    //!    必须在注册表里；值是标识符（具名常量）⇒ 解析成该常量的值再查表。
    //! 2. **任何 `const EVENT_…: &str = "…"` 的值** ⇒ 必须在注册表里。这条覆盖
    //!    规则 1够不着的间接形态：`outsource::write_part_event` 与
    //!    `prod::queue::part_insert_part_event` 是转发器，字面量在**调用点**的
    //!    实参里，块内只有 `event_type,`（字段简写）。
    //!
    //! ### 为什么**限定在 `NewPartEvent { … }` 块内**而不是全仓扫 `event_type:`
    //!
    //! 全仓同名 `event_type` 字段**至少还有四处、语义完全不同**：
    //! `t_outsource_quote_event`（`CREATED` / `APPROVED` …）、已删除的
    //! `t_delivery_note_event`、`prod::scan` 出参 `WorkerScanEvent`
    //! （`WORKER_SCAN_RETURNED` / `WORKER_SCAN_INSPECTED`）、dashboard 的 WS
    //! payload。不限定作用域则它们**必然误报** —— 护栏一旦误报，人就会去加白名单，
    //! 加着加着就真漏了。限定到「`NewPartEvent` 字面量」这个唯一写入 builder 之后，
    //! 扫描结果与 [`super::ALL_EVENT_TYPES`] 的收录标准**逐条对齐**。
    //!
    //! ## ⚠️ 已知漏报盲区（**全部**登记在此 —— 未登记的漏报形态一律视为护栏缺陷）
    //!
    //! 1. **字段简写 / 局部变量**：`event_type,`（无值表达式）不判。现状两处：
    //!    `prod::queue::repo::part_insert_part_event` 与 `outsource::
    //!    service::move::write_part_event` 两个转发器；另有 `prod::batch::
    //!    service::scan::scan_inspect` 用 `let (to_status, event_type) = if …
    //!    { ("READY_TO_SHIP", "BATCH_PASSED") } else { … }` 解构后简写 ——
    //!    ⇒ `BATCH_PASSED` 这类值**改错了抓不到**（它进注册表靠的是人工，不是本
    //!    护栏）。要覆盖得做跨语句的数据流分析，成本远超本仓暴露面。
    //! 2. **转发器的实参**（`part_insert_part_event(…, "TAKEN_FROM_POOL", …)`）
    //!    同样不在 `NewPartEvent` 块内 ⇒ 同上，抓不到。规则 2 只兜住其中
    //!    **具名常量**那一档（`SENT_TO_OUTSOURCE` / `RECEIVED_FROM_OUTSOURCE`）。
    //! 3. **多分支表达式**（`event_type: if cond { A } else { B }`）：两个分支里
    //!    的值一律不判。现状**只有一处**这种形态 ——
    //!    `prod::batch::service::transition_core::to_inspection_core` 按 `force`
    //!    二选一。⇒ 该处实际上仍被守住：两个分支都是 `EVENT_*` 具名常量，改名是
    //!    编译错误、改值被**规则 2** 抓到。
    //! 4. **`use … as …` 重命名的常量**：源码里不出现常量名，查不到。
    //! 5. **未闭合块注释**（rustc 已先拦下的畸形输入）会让文件剩余内容被当注释跳过。
    //! 6. **值位置是「非 `EVENT_*` 前缀的表达式 / 标识符」**：`event_type: self.kind`
    //!    / `event_type: some_var` / `event_type: format!(…)` 一律**跳过、不判**
    //!    （2026-10-17 review 第 1 轮 NIT-1 收窄：此前它们会被当成常量名去 `consts`
    //!    查、查不到就当值判违规，报出 `event_type = "self"` / `"format"` 这种**读者
    //!    完全看不懂的违规** —— 误报的护栏比没有护栏更糟，人会去加白名单，加着加着
    //!    就真漏了）。⇒ 本护栏只对「裸字面量」与「`EVENT_*` 具名常量」负责；
    //!    任何**计算出来的** `event_type` 都需要人工登记。
    //!    连带代价：写了个**不带 `EVENT_` 前缀**的常量（命名约定之外）会漏报，规则 2
    //!    按同一前缀收常量、同样查不到它。
    //!
    //! ## ⚠️ 刻意**不做**的事
    //!
    //! - **不硬编码「当前有 N 条」**：这类计数必然过期，错的那个数比没有更坏。
    //!   断言的是**集合包含关系**（每个被扫到的值 ∈ 注册表）+ 两条与基数无关的
    //!   不变式（无重复项、每项 ≤ `varchar(30)` 的 30 字符）。
    //! - **不断言「注册表里的每一项都还在被写」**：历史回填（`SERIAL_RELEASED`）
    //!   与已下线方向（`RECEIVED_TO_INSPECTION`）本就不该再被写，而把退役类型清出
    //!   注册表会让历史行的读侧文案失配。

    use std::collections::{BTreeMap, BTreeSet};
    use std::path::{Path, PathBuf};

    use super::ALL_EVENT_TYPES;

    /// 扫描根（相对 `CARGO_MANIFEST_DIR`）—— 相对 crate 根的显式清单。
    ///
    /// **只扫 `src/`**：本护栏的对象是**生产写侧**，而 `tests/**` 里出现的
    /// `event_type` 全是断言 / fixture 里的字面量（造历史行用），不受「登记」
    /// 约束 —— 否则每加一条断言就要改注册表。
    const SCAN_ROOT: &str = "src";

    /// `t_part_event.event_type` 的列宽（`varchar(30)`）—— 超长会让 PG 返 `22001`
    /// 并把**整个事务**回滚（同一处理由见 `outsource::service::move` 的常量 doc）。
    const EVENT_TYPE_MAX_LEN: usize = 30;

    const KIND_OTHER: u8 = 0; // 空白 / 注释（注释已被等量空格化）
    const KIND_CODE: u8 = 1; // 代码字符
    const KIND_STR: u8 = 2; // 字符串 / raw string / 字符字面量（含引号本身）

    const NEEDLE_NEW_PART_EVENT: &str = "NewPartEvent";
    const NEEDLE_FIELD: &str = "event_type";
    const NEEDLE_CONST_PREFIX: &str = "EVENT_";

    /// 值位置上**不是**「字面量 / 常量」的那些 Rust 关键字 —— 它们开启的是一个
    /// **表达式**（`if cond { A } else { B }` / `match x { … }`），不是标识符。
    ///
    /// ⚠️ 漏掉这张表的后果是实测出来的：`event_type: if force { A } else { B }`
    /// 会被当成「值是标识符 `if`」，于是注册表被塞进一条 `event_type = "if"` 的
    /// 违规 —— 护栏当场误报，而误报的护栏比没有护栏更糟（人会去加白名单）。
    const VALUE_KEYWORDS: &[&str] = &["if", "match", "return", "move", "unsafe", "let"];

    /// 把 Rust 源码切成「注释已空格化」的文本 + 每字节分类。
    ///
    /// 注释**空格化**而不是整段删除，是为了让**行号保持不变**，失败信息里才能给出
    /// `file:line`。与 `src/shared/snowflake_guard.rs::scan_rust` 逐字同款的状态机
    /// （行注释 / 嵌套块注释 / raw string 与 byte string 前缀 / 普通字符串含转义 /
    /// 字符字面量 vs 生命周期），本文件是它的**第二份拷贝**（那一份是私有函数、
    /// 要复用就得改它的可见性，超出本文件范围；两份的关系如实记档于
    /// `snowflake_guard.rs` 的模块 doc「为什么不直接复用」一节）。
    fn scan_rust(src: &str) -> (Vec<u8>, Vec<u8>) {
        let b = src.as_bytes();
        let mut out = b.to_vec();
        let mut kind = vec![KIND_OTHER; b.len()];
        let mut i = 0usize;
        while i < b.len() {
            let c = b[i];
            if c == b'/' && b.get(i + 1) == Some(&b'/') {
                while i < b.len() && b[i] != b'\n' {
                    out[i] = b' ';
                    i += 1;
                }
                continue;
            }
            if c == b'/' && b.get(i + 1) == Some(&b'*') {
                // `depth` 从 0 起步，让开界本身也由内层循环统一计数（配平的 `*/`
                // 恰好把它降回 0）—— 初版从 1 起步会重复计一次，导致单层块注释之后
                // 的全部内容都被当注释跳过、整条规则对该文件静默失效。
                let mut depth = 0usize;
                while i < b.len() {
                    if b[i] == b'\n' {
                        i += 1;
                        continue;
                    }
                    if b[i] == b'/' && b.get(i + 1) == Some(&b'*') {
                        depth += 1;
                        out[i] = b' ';
                        out[i + 1] = b' ';
                        i += 2;
                        continue;
                    }
                    if b[i] == b'*' && b.get(i + 1) == Some(&b'/') {
                        depth -= 1;
                        out[i] = b' ';
                        out[i + 1] = b' ';
                        i += 2;
                        if depth == 0 {
                            break;
                        }
                        continue;
                    }
                    out[i] = b' ';
                    i += 1;
                }
                continue;
            }
            if let Some((lit_start, content_start, hashes)) = raw_string_start(b, i) {
                let Some(close) = raw_string_end(b, content_start, hashes) else {
                    // 理论不可达（源码一定编译得过）；保守当普通代码处理，避免死循环
                    kind[i] = KIND_CODE;
                    i += 1;
                    continue;
                };
                for slot in kind.iter_mut().take(close + 1).skip(lit_start) {
                    *slot = KIND_STR;
                }
                i = close + 1;
                continue;
            }
            if c == b'"' {
                let end = string_end(b, i + 1).unwrap_or(b.len().saturating_sub(1));
                for slot in kind.iter_mut().take(end + 1).skip(i) {
                    *slot = KIND_STR;
                }
                i = end + 1;
                continue;
            }
            if c == b'\'' {
                let is_char = match b.get(i + 1) {
                    Some(b'\\') => true,
                    Some(_) if b.get(i + 2) == Some(&b'\'') => true,
                    _ => false,
                };
                if is_char {
                    let end = string_end(b, i + 1).unwrap_or(b.len().saturating_sub(1));
                    for slot in kind.iter_mut().take(end + 1).skip(i) {
                        *slot = KIND_STR;
                    }
                    i = end + 1;
                    continue;
                }
                kind[i] = KIND_CODE;
                i += 1;
                continue;
            }
            if !c.is_ascii_whitespace() {
                kind[i] = KIND_CODE;
            }
            i += 1;
        }
        (out, kind)
    }

    fn string_end(b: &[u8], from: usize) -> Option<usize> {
        let mut j = from;
        while j < b.len() {
            match b[j] {
                b'\\' => j += 2,
                c if c == b'"' || c == b'\'' => return Some(j),
                b'\n' => return None,
                _ => j += 1,
            }
        }
        None
    }

    /// 返回 `(字面量起始, 内容起始, 开引号的 # 个数)`；`r` / `b` / `c` 前缀本身算
    /// 字面量。两条分支的契约统一成「一律给**内容**起点」（否则 `r#"…"#` 的 `#`
    /// 计数恒为 0，一路扫到 EOF 判为未闭合）。
    fn raw_string_start(b: &[u8], i: usize) -> Option<(usize, usize, usize)> {
        let mut p = i;
        while matches!(b.get(p), Some(b'r') | Some(b'b') | Some(b'c')) {
            p += 1;
        }
        let is_raw = b.get(p) == Some(&b'r') && p > i;
        let q = if is_raw { p + 1 } else { p };
        match b.get(q) {
            Some(b'"') => Some((i, q + 1, 0)),
            Some(b'#') => {
                let mut h = q;
                while b.get(h) == Some(&b'#') {
                    h += 1;
                }
                if b.get(h) == Some(&b'"') {
                    Some((i, h + 1, h - q))
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    fn raw_string_end(b: &[u8], content_start: usize, hashes: usize) -> Option<usize> {
        let mut p = content_start;
        while p < b.len() {
            if b[p] == b'"' {
                let mut h = 0usize;
                while b.get(p + 1 + h) == Some(&b'#') {
                    h += 1;
                }
                if h == hashes {
                    return Some(p + h);
                }
                p += 1 + h;
                continue;
            }
            p += 1;
        }
        None
    }

    fn skip_ws(b: &[u8], mut i: usize) -> usize {
        while b.get(i).is_some_and(|c| c.is_ascii_whitespace()) {
            i += 1;
        }
        i
    }

    fn is_ident(c: u8) -> bool {
        c.is_ascii_alphanumeric() || c == b'_' || c == b'$'
    }

    /// 从 `from` 起找「前一个字符不是标识符字符」的子串，返回**绝对**下标。
    fn find_word_at(hay: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
        let mut at = from;
        while let Some(rel) = find_sub(hay, needle, at) {
            if rel == 0 || !is_ident(hay[rel - 1]) {
                return Some(rel);
            }
            at = rel + 1;
        }
        None
    }

    fn find_sub(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
        if needle.is_empty() || hay.len() < needle.len() {
            return None;
        }
        (from..=hay.len() - needle.len()).find(|&s| &hay[s..s + needle.len()] == needle)
    }

    /// 从 `at` 起读一个标识符（代码区），返回 `(标识符, 其后下标)`；`at` 处不是标识符
    /// 起始则返回 `None`。
    fn read_ident(out: &[u8], kind: &[u8], at: usize) -> Option<(String, usize)> {
        if kind.get(at) != Some(&KIND_CODE) || !is_ident(out[at]) || out[at].is_ascii_digit() {
            return None;
        }
        let mut end = at;
        while end < out.len() && kind.get(end) == Some(&KIND_CODE) && is_ident(out[end]) {
            end += 1;
        }
        Some((String::from_utf8_lossy(&out[at..end]).into_owned(), end))
    }

    /// 读一个紧跟 `"` 的普通字符串字面量的**内容**（不含引号、不解转义）。
    fn read_str_literal(out: &[u8], kind: &[u8], at: usize) -> Option<(String, usize)> {
        if out.get(at) != Some(&b'"') {
            return None;
        }
        let mut end = at + 1;
        while end < out.len() && out[end] != b'"' {
            if out[end] == b'\\' {
                end += 1;
            }
            end += 1;
        }
        let _ = kind;
        Some((
            String::from_utf8_lossy(&out[at + 1..end.min(out.len())]).into_owned(),
            end + 1,
        ))
    }

    fn line_of(out: &[u8], at: usize) -> usize {
        1 + out[..at].iter().filter(|c| **c == b'\n').count()
    }

    /// 扫一个文件，返回
    /// `(const 名 → 值, NewPartEvent 块内 event_type 的取值及其行号)`。
    fn harvest(out: &[u8], kind: &[u8]) -> (BTreeMap<String, String>, Vec<(String, usize)>) {
        let mut consts = BTreeMap::new();
        let mut fields = Vec::new();
        let mut at = 0usize;
        while let Some(pos) = find_word_at(out, at, b"const") {
            at = pos + 1;
            if kind.get(pos) != Some(&KIND_CODE) {
                continue;
            }
            let (name, after_name) = match read_ident(out, kind, skip_ws(out, pos + 5)) {
                Some(v) => v,
                None => continue,
            };
            let p = skip_ws(out, after_name);
            if !out[p..].starts_with(b":") {
                continue;
            }
            let q = skip_ws(out, p + 1);
            if !out[q..].starts_with(b"&str") {
                continue;
            }
            let r = skip_ws(out, q + 4);
            if !out[r..].starts_with(b"=") {
                continue;
            }
            let s = skip_ws(out, r + 1);
            if let Some((value, _)) = read_str_literal(out, kind, s) {
                consts.insert(name, value);
            }
        }

        let mut at2 = 0usize;
        while let Some(pos) = find_word_at(out, at2, NEEDLE_NEW_PART_EVENT.as_bytes()) {
            at2 = pos + 1;
            if kind.get(pos) != Some(&KIND_CODE) {
                continue;
            }
            let open = skip_ws(out, pos + NEEDLE_NEW_PART_EVENT.len());
            if out.get(open) != Some(&b'{') {
                continue;
            }
            // 逐字节走完整块（`{` / `}` 只认代码字符 ⇒ 字符串与注释里的括号天然无害），
            // 在 depth ≥ 1 的区间里找 `event_type` 字段。
            let mut depth = 0usize;
            let mut i = open;
            while i < out.len() {
                let c = out[i];
                if kind.get(i) == Some(&KIND_CODE) {
                    if c == b'{' {
                        depth += 1;
                        i += 1;
                        continue;
                    }
                    if c == b'}' {
                        depth -= 1;
                        i += 1;
                        if depth == 0 {
                            break;
                        }
                        continue;
                    }
                }
                if depth >= 1 && find_word_at(out, i, NEEDLE_FIELD.as_bytes()) == Some(i) {
                    let p = skip_ws(out, i + NEEDLE_FIELD.len());
                    if out.get(p) == Some(&b':') {
                        let s = skip_ws(out, p + 1);
                        let value = match read_str_literal(out, kind, s) {
                            Some((v, _)) => Some(v),
                            // 标识符（具名常量）交给第二轮解析；字段简写 / 表达式跳过
                            //
                            // ⚠️ 2026-10-17 review 第 1 轮 NIT-1 收窄：**只**接受
                            // `EVENT_*` 前缀的名字。此前这里是「任何非关键字标识符都当
                            // 常量名去 `consts` 查、查不到就当值判违规」，于是
                            // `event_type: self.kind` / `event_type: some_var` /
                            // `event_type: format!(…)` 三种形态都会被报成
                            // `event_type = "self"` / `"format"` 这种**看不懂的违规**
                            // （误报比没有护栏更糟：人会去加白名单，加着加着就真漏了）。
                            // 收窄后失败信息里出现的每一条都是**真的字面量或真的常量**。
                            // 代价 = 「有人写了个不带 `EVENT_` 前缀的事件类型常量」会
                            // 漏报（规则 2 同样按前缀收，查不到它）—— 那是命名约定之外
                            // 的写法；要改约定**两处前缀一起改**，别只改一处。登记见
                            // 本子模块 doc 的盲区 6。
                            None => read_ident(out, kind, s)
                                .filter(|(name, _)| {
                                    name.starts_with(NEEDLE_CONST_PREFIX)
                                        && !VALUE_KEYWORDS.contains(&name.as_str())
                                })
                                .map(|(name, _)| name),
                        };
                        if let Some(v) = value {
                            fields.push((v, line_of(out, i)));
                        }
                        i = s;
                        continue;
                    }
                }
                i += 1;
            }
        }
        (consts, fields)
    }

    /// 把一轮 harvest 的字段取值解析成「常量值 → 出现位置」。
    ///
    /// `fields` 里存的可能是**值本身**（裸字面量）也可能是**常量名**
    /// （`event_type: EVENT_X`）—— 一律先在 `consts` 里查一次；查不到就当值用
    /// （跨文件常量在第一轮已并进 `consts`，故「查不到」只在「无该常量」时发生）。
    fn values_of(
        consts: &BTreeMap<String, String>,
        fields: Vec<(String, usize)>,
    ) -> Vec<(String, String)> {
        fields
            .into_iter()
            .map(|(v, line)| {
                let resolved = consts.get(&v).cloned().unwrap_or(v);
                (resolved, format!("line {line}"))
            })
            .collect()
    }

    /// 一轮完整 harvest（供元测试直接喂假源码）。
    fn literals_of(src: &str) -> Vec<(String, String)> {
        let (out, kind) = scan_rust(src);
        let (consts, fields) = harvest(&out, &kind);
        values_of(&consts, fields)
    }

    fn collect_rs(dir: &Path, acc: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        let mut entries: Vec<_> = entries.flatten().map(|e| e.path()).collect();
        entries.sort();
        for path in entries {
            if path.is_dir() {
                collect_rs(&path, acc);
            } else if path.extension().is_some_and(|e| e == "rs") {
                acc.push(path);
            }
        }
    }

    fn root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
    }

    fn rel_of(path: &Path, root: &Path) -> String {
        path.strip_prefix(root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/")
    }

    fn registered() -> BTreeSet<&'static str> {
        ALL_EVENT_TYPES.iter().copied().collect()
    }

    // ── 主规则 ──────────────────────────────────────────────────────────────

    /// ★ 规则 1：`NewPartEvent { … }` 块内每一个 `event_type` 值都在注册表里。
    #[test]
    fn every_new_part_event_literal_is_registered() {
        let root = root();
        let mut files = Vec::new();
        collect_rs(&root.join(SCAN_ROOT), &mut files);
        assert!(
            !files.is_empty(),
            "扫不到 `{SCAN_ROOT}/**/*.rs`（CARGO_MANIFEST_DIR={}），规则 1 本身失效",
            root.display()
        );

        // 第一轮：先收齐全仓的 `const NAME: &str = "…"`（规则 2 的判定也要用）
        let mut consts: BTreeMap<String, String> = BTreeMap::new();
        let mut sources: Vec<(String, String)> = Vec::new();
        for path in &files {
            let rel = rel_of(path, &root);
            let Ok(src) = std::fs::read_to_string(path) else {
                continue;
            };
            let (out, kind) = scan_rust(&src);
            let (file_consts, _) = harvest(&out, &kind);
            for (k, v) in file_consts {
                // 同名常量以「先到先得」为准（现状全仓无重名；重名时报错由下面的
                // 一致性检查之外的手段兜 —— 不为此单开断言，避免过期规则）
                consts.entry(k).or_insert(v);
            }
            sources.push((rel, src));
        }

        let registry = registered();
        let mut violations: Vec<String> = Vec::new();
        for (rel, src) in &sources {
            let (out, kind) = scan_rust(src);
            let (_, fields) = harvest(&out, &kind);
            for (value, at) in values_of(&consts, fields) {
                if !registry.contains(value.as_str()) {
                    violations.push(format!("  {rel}:{at}  event_type = {value:?}"));
                }
            }
        }
        assert!(
            violations.is_empty(),
            "{msg}",
            msg = failure_message(&violations)
        );
    }

    /// 规则 2：任何 `const EVENT_…: &str = "…"` 的值都在注册表里。
    ///
    /// 与规则 1 互补：它覆盖 `outsource::write_part_event` /
    /// `prod::queue::part_insert_part_event` 两个**转发器**的字面量 —— 那些值在
    /// 块内只以「具名常量实参 / 字段简写」形态出现，规则 1 够不着。
    #[test]
    fn every_event_const_value_is_registered() {
        let root = root();
        let mut files = Vec::new();
        collect_rs(&root.join(SCAN_ROOT), &mut files);
        assert!(
            !files.is_empty(),
            "扫不到 `{SCAN_ROOT}/**/*.rs`，规则 2 本身失效"
        );

        let registry = registered();
        let mut violations: Vec<String> = Vec::new();
        for path in &files {
            let rel = rel_of(path, &root);
            let Ok(src) = std::fs::read_to_string(path) else {
                continue;
            };
            let (out, kind) = scan_rust(&src);
            let (consts, _) = harvest(&out, &kind);
            for (name, value) in consts {
                if !name.starts_with(NEEDLE_CONST_PREFIX) {
                    continue;
                }
                if !registry.contains(value.as_str()) {
                    violations.push(format!("  {rel}  const {name} = {value:?}"));
                }
            }
        }
        assert!(
            violations.is_empty(),
            "{msg}",
            msg = failure_message(&violations)
        );
    }

    // ── 与基数无关的注册表不变式 ────────────────────────────────────────────

    /// 注册表不得有重复项。
    ///
    /// 为什么值得断言：重复项不会让功能出错，但会让「注册表 = 事件类型集合」这个
    /// 不变式在肉眼层面失效（review 时数不出「到底几种」），而它正是本护栏的地基。
    #[test]
    fn registry_has_no_duplicates() {
        let mut seen = BTreeSet::new();
        let dups: Vec<&str> = ALL_EVENT_TYPES
            .iter()
            .copied()
            .filter(|v| !seen.insert(*v))
            .collect();
        assert!(
            dups.is_empty(),
            "ALL_EVENT_TYPES 里有重复项：{dups:?}（去重要手工改常量表，不是改本测试）"
        );
    }

    /// 注册表每项都不超 `varchar(30)`。
    ///
    /// 超长会让 PG 返 `22001` 并把**整个事务**回滚（事件 INSERT 紧跟在状态翻转
    /// 之后 ⇒ 一次超长直接让整个送检 / 交付动作失败）。
    #[test]
    fn registry_entries_fit_the_column_width() {
        for v in ALL_EVENT_TYPES {
            assert!(
                v.len() <= EVENT_TYPE_MAX_LEN,
                "ALL_EVENT_TYPES 里的 {v:?} 长 {} 字符，超出 t_part_event.event_type \
                 的 varchar({EVENT_TYPE_MAX_LEN}) ⇒ 写入会 22001 并回滚整个事务",
                v.len()
            );
        }
    }

    /// 注册表非空（防「探测器因为某个原因啥都没扫到」时的静默绿）。
    #[test]
    fn registry_is_not_empty() {
        assert!(
            !ALL_EVENT_TYPES.is_empty(),
            "ALL_EVENT_TYPES 为空：要么探测器失效，要么有人把整张表删了"
        );
    }

    // ── 元测试：探测器本身必须能报警，否则「全绿」只是因为探测器瞎了 ──────────

    /// 正例：未登记的字面量一律抓到；`event_type: 具名常量` 经解析后同样能判。
    #[test]
    fn detector_flags_unregistered_literals_in_new_part_event_blocks() {
        // 1) 最典型的真实形态：裸字面量
        let bare = "fn f() {\n    let _e = NewPartEvent {\n        event_type: \"TYPO_EVENT\",\n    };\n}\n";
        assert_eq!(
            literals_of(bare)
                .into_iter()
                .map(|(v, _)| v)
                .collect::<Vec<_>>(),
            vec!["TYPO_EVENT".to_string()],
            "裸字面量应被抓到"
        );

        // 2) 具名常量：解析成值再判
        let named = "const EVENT_TYPO: &str = \"TYPO_EVENT\";\n\
                     fn f() {\n    let _e = NewPartEvent {\n        event_type: EVENT_TYPO,\n    };\n}\n";
        assert_eq!(
            literals_of(named)
                .into_iter()
                .map(|(v, _)| v)
                .collect::<Vec<_>>(),
            vec!["TYPO_EVENT".to_string()],
            "event_type: CONST 应被解析后判"
        );

        // 3) 跨文件的常量（生产代码里常量定义与写点常常不同文件）
        let cross = "fn f() {\n    let _e = NewPartEvent {\n        event_type: EVENT_ELSEWHERE,\n    };\n}\n";
        let mut consts = BTreeMap::new();
        consts.insert("EVENT_ELSEWHERE".to_string(), "TYPO_EVENT".to_string());
        let (out, kind) = scan_rust(cross);
        let (_, fields) = harvest(&out, &kind);
        assert_eq!(
            values_of(&consts, fields)
                .into_iter()
                .map(|(v, _)| v)
                .collect::<Vec<_>>(),
            vec!["TYPO_EVENT".to_string()],
            "跨文件常量也应被解析"
        );

        // 4) `::` / `:` 前后有空白（Rust 允许），不能给后人留后门
        let spaced = "fn f() {\n    let _e = NewPartEvent\n    {\n        event_type\n            :\n            \"TYPO_EVENT\",\n    };\n}\n";
        assert_eq!(
            literals_of(spaced)
                .into_iter()
                .map(|(v, _)| v)
                .collect::<Vec<_>>(),
            vec!["TYPO_EVENT".to_string()],
            "空白不应成为绕过口"
        );

        // 5) 同一文件多处逐个报，不只报第一处
        let two = "fn f() {\n    let _a = NewPartEvent {\n        event_type: \"TYPO_A\",\n    };\n}\n\
                   fn g() {\n    let _b = NewPartEvent {\n        event_type: \"TYPO_B\",\n    };\n}\n";
        let got: Vec<String> = literals_of(two).into_iter().map(|(v, _)| v).collect();
        assert_eq!(got, vec!["TYPO_A".to_string(), "TYPO_B".to_string()]);

        // 6) 已登记的字面量**能被采到**（否则「护栏全绿」可能只是因为探测器啥都没
        //    看见 —— 采集与判违规是两件事，本函数是采集侧）
        let ok = format!(
            "fn f() {{\n    let _e = NewPartEvent {{\n        event_type: {v:?},\n    }};\n}}\n",
            v = ALL_EVENT_TYPES[0]
        );
        assert_eq!(
            literals_of(&ok)
                .into_iter()
                .map(|(v, _)| v)
                .collect::<Vec<_>>(),
            vec![ALL_EVENT_TYPES[0].to_string()],
            "已登记值也应被采到（判违规在调用方）"
        );
    }

    /// 反例：注释 / 字符串 / 别的事件表 / 字段简写，都不该报警。
    #[test]
    fn detector_ignores_comments_strings_and_other_tables() {
        // 1) 行注释（本仓 doc 里大量「历史溯源」贴旧字面量）
        let line_comment = "fn f() {\n    // 旧写法：event_type: \"TYPO_EVENT\"\n}\n";
        assert!(literals_of(line_comment).is_empty());

        // 2) 文档注释
        let doc_comment = "//! event_type: \"TYPO_EVENT\"\nfn f() {}\n";
        assert!(literals_of(doc_comment).is_empty());

        // 3) 块注释（含嵌套）
        let block_comment = "/* event_type: \"TYPO_EVENT\" /* 嵌套 TYPO_EVENT2 */ */\nfn f() {}\n";
        assert!(literals_of(block_comment).is_empty());

        // 4) raw string（本仓用 `r#"…"#` 贴旧写法代码样例）
        let raw = "fn f() {\n    let doc = r#\"event_type: \"TYPO_EVENT\"\"#;\n}\n";
        assert!(literals_of(raw).is_empty());

        // 5) 字符串里的 `NewPartEvent {` 不算真实构造点
        let in_str =
            "fn f() {\n    let s = \"NewPartEvent { event_type: \\\"TYPO_EVENT\\\" }\";\n}\n";
        assert!(literals_of(in_str).is_empty());

        // 6) **别的事件表**：字段同名但结构体不同 ⇒ 一律不判
        for other in [
            "let _e = NewQuoteEvent { event_type: \"TYPO_EVENT\".to_string() };\n",
            "let _e = NewOutsourceEvent { event_type };\n",
            "pub event_type: WorkerScanEvent,\n",
        ] {
            assert!(
                literals_of(other).is_empty(),
                "非 NewPartEvent 的 `event_type` 不该被判：{other}"
            );
        }

        // 7) 字段简写 `event_type,`（转发器形态）不判 —— 已知漏报，已在模块 doc 登记
        let shorthand = "fn f() {\n    let _e = NewPartEvent {\n        event_type,\n    };\n}\n";
        assert!(
            literals_of(shorthand).is_empty(),
            "字段简写不在本护栏能力范围内（已知漏报）"
        );

        // 7b) ⚠️ 值位置是**表达式**（`if cond { A } else { B }`）时也不判，且
        //     **绝不能**把关键字 `if` 当成常量名（那样会凭空多出一条
        //     `event_type = "if"` 违规）。该形态由规则 2 兜底（见模块 doc 盲区 3）。
        let expr = "fn f(c: bool) {\n    let _e = NewPartEvent {\n        \
                   event_type: if c { \"TYPO_A\" } else { \"TYPO_B\" },\n    };\n}\n";
        assert!(
            literals_of(expr).is_empty(),
            "表达式形态不在采集范围（且不应被误判成常量名）"
        );

        // 7c) ⚠️ 2026-10-17 review 第 1 轮 NIT-1：`event_type:` 后跟**非
        //     `EVENT_*` 前缀**的标识符 / 宏调用时一律跳过。修之前它们会被报成
        //     `event_type = "self"` / `"format"` 这种读者看不懂的违规（误报比没有
        //     护栏更糟）。见模块 doc 盲区 6。
        for expr_form in [
            "fn f() {\n    let _e = NewPartEvent {\n        event_type: self.kind,\n    };\n}\n",
            "fn f() {\n    let _e = NewPartEvent {\n        event_type: some_var,\n    };\n}\n",
            "fn f() {\n    let _e = NewPartEvent {\n        event_type: format!(\"X_{}\", 1),\n    };\n}\n",
            "fn f() {\n    let _e = NewPartEvent {\n        event_type: kind.clone(),\n    };\n}\n",
        ] {
            assert!(
                literals_of(expr_form).is_empty(),
                "非 EVENT_* 前缀的标识符 / 表达式不该被判（否则报出看不懂的违规）: \
                 {expr_form}"
            );
        }

        // 8) 只 `use` 结构体（不带字面量）不判
        let only_use = "use crate::shared::part_event::NewPartEvent;\n";
        assert!(literals_of(only_use).is_empty());
    }

    /// 失败信息正文：给出「怎么改」而不是只列违规。
    #[test]
    fn failure_message_carries_guidance() {
        let msg = failure_message(&["  src/demo.rs:42  event_type = \"TYPO\"".to_string()]);
        assert!(msg.contains("src/demo.rs:42"), "缺 file:line：{msg}");
        assert!(msg.contains("ALL_EVENT_TYPES"), "缺该改哪张表的指引：{msg}");
        assert!(msg.contains("PART_EVENT_LABEL"), "缺跨仓同步义务：{msg}");
    }

    fn failure_message(violations: &[String]) -> String {
        let lines = vec![
            format!(
                "以下 {} 处 `t_part_event` 事件类型**没有**登记进 \
                 `src/shared/part_event/mod.rs::ALL_EVENT_TYPES`：",
                violations.len()
            ),
            violations.join("\n"),
            String::new(),
            "为什么违规 —— `t_part_event.event_type` 是 varchar(30)、无 CHECK 约束、".to_string(),
            "无 DB ENUM、读侧也无 Rust enum ⇒ 打错字没有任何拦截，能正常入库，只是".to_string(),
            "前端时间线上永远匹配不到一档文案，静默退化成一行裸字面量。".to_string(),
            String::new(),
            "怎么修：".to_string(),
            "  1. 把字面量按语义归类加进 `ALL_EVENT_TYPES`（带一句中文注释说明它记的是".to_string(),
            "     什么业务事实）；".to_string(),
            "  2. 同时同步前端 `frontend/src/types/parts.ts::PART_EVENT_LABEL` 的中文".to_string(),
            "     文案（人工同步，**无编译期保障**；少一侧的症状是「某一类事件在时间线".to_string(),
            "     上没有文案」）。⚠️ 禁止用 `include_str!` 跨仓到 frontend 做编译期校验 ——"
                .to_string(),
            "     Dockerfile 的构建上下文只有 backend-rust 仓，跨仓路径会直接打断构建。"
                .to_string(),
            String::new(),
            "本条报错里的每一条都是**真的字面量或真的 `EVENT_*` 常量** —— 本护栏判定".to_string(),
            "范围只到这两档（`event_type: some_var` / `format!(…)` 等表达式形态一律跳过，"
                .to_string(),
            "不会出现在这里）。**如果你刚写的 `event_type` 没被抓到**，它八成是表达式形态："
                .to_string(),
            "拼错字同样不会被拦，请人工对照 `ALL_EVENT_TYPES`。漏报形态清单见本子模块".to_string(),
            "模块 doc 的「已知漏报盲区」一节。".to_string(),
        ];
        lines.join("\n")
    }
}

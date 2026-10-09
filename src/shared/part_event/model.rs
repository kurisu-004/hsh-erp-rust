//! `t_part_event` 的写侧 builder（2026-10-12 自 `modules::part::model` 迁入）。
//!
//! 本文件**零 `#[derive(sqlx::FromRow)]`、零 `T*` 结构体** —— 理由见
//! `shared/part_event/mod.rs` 的「为什么本表没有 `T*` 投影」一节。
//!
//! 11 个字段（`id` / `part_id` / `event_type` / `from_status` / `to_status` /
//! `batch_id` / `quantity` / `drawing_code` / `badge_code` / `note` / `created_by`）
//! 自 `modules::part::model::NewPartEvent` 逐字搬来，2026-10-12 只在
//! `badge_code` 之后插入 `worker_id` / `process_id` 两个新字段。

/// 新事件 builder —— 由 service 层在事务内构造，repo 负责 INSERT。
///
/// `id` 由调用方用 `SnowflakeIdGenerator::next_id()` 预生成；
/// `created_at` 走 DB 默认 `now()`（保持与其它写入路径一致，故不在本结构体内）。
/// 其余可选字段在不需要时传 `None`。
///
/// 字段顺序与 [`crate::shared::part_event::sql::insert`] 的 INSERT 列清单**逐项对齐**
/// （新增的两个字段夹在 `badge_code` 与 `note` 之间，INSERT 侧同位置），读代码时
/// 结构体从上往下扫一遍就能对上占位符 `$n`，不必来回翻 sql.rs。
pub struct NewPartEvent<'a> {
    pub id: i64,
    pub part_id: i64,
    /// 事件类型，如 `"STATUS_CHANGED"` / `"BATCH_PASSED"` 等。
    pub event_type: &'a str,
    pub from_status: Option<&'a str>,
    pub to_status: Option<&'a str>,
    pub batch_id: Option<i64>,
    pub quantity: Option<i32>,
    pub drawing_code: Option<&'a str>,
    pub badge_code: Option<&'a str>,
    /// 2026-10-12 新增：**动作发起人 / 作业人**（`t_worker.id`，**不是** `t_user.id`
    /// —— 依据见 `migrations/20261011120000_001_add_user_worker_id.sql` 文件头）。
    ///
    /// 此前 Rust 写侧**恒不写**本列（老的 part 域事件 INSERT 的列清单里根本没有
    /// 它），statistics 域的「工人取件 / 放回」聚合因此按 `worker_id` 过滤时
    /// 只能命中零星历史行。
    pub worker_id: Option<i64>,
    /// 2026-10-12 新增：**事件发生时该批次所处的工序快照**（`t_process.id`），
    /// 由 migration `20261011130000_009` 加列。
    ///
    /// 与 `t_part_batch.current_process_id`（批次**当前**工序，会随流转推进）是两回事：
    /// 本列是写事件那一刻的快照，读历史行时不会被后续流转改写。`RETURNED_TO_SHELF`
    /// 事件写在 `mark_batch_returned` **之后**，而该函数同一事务内已把
    /// `current_process_id` 推进到下一道工序 ⇒ 不快照就永远拿不到「这位工人刚做完
    /// 哪道工序」。写入点清单见 `shared/part_event/mod.rs`。
    pub process_id: Option<i64>,
    pub note: Option<&'a str>,
    pub created_by: Option<i64>,
}

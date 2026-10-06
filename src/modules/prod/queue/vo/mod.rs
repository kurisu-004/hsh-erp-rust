//! prod::queue VO —— 出参（handler 响应序列化层）
//!
//! 仅 `Serialize` 不 `Deserialize`（禁止出现在 axum extractor 反序列化侧）。
//! 入参见 [`super::dto`]。
//!
//! i64 一律序列化为 JSON **string**（雪花 ID > 2^53，JS `Number` 会丢精度，
//! 见 `shared::types`）。本域两种写法并存：入参侧走 `serialize_i64` 派生属性，
//! 聚合端点的出参走装配处 `.to_string()`（照 dashboard 域 VO 收口做法 ——
//! 那批字段是 service 手工装配的内存结构，derive 序列化助手反而绕）。
//!
//! ## 分片
//! - [`worker`] —— 队列写端点（refill / move / auto-allocate）的出参
//! - [`queue`] —— 下发流（pending / dispatch / auto-dispatch / recall）的出参
//! - [`board`] —— 只读聚合端点（snapshot / processes/{id}）的出参
//!
//! 2026-10-08 变更：原 `model.rs`（`TakenItem` / `HeldBatchItem` / `RefillResult` /
//! `ProcessPoolCount` / `WorkerPoolState`）整体并入本目录 —— 那 5 个 struct 全是纯
//! 出参，没有一列对应独立的表行模型，留在 `model.rs` 会让人误以为存在「行模型层」。
//! 本仓对「只读聚合域」已是这个形态（dashboard 域无 `model.rs`，出参全在 `vo/`）。
//! 同批删掉的字段见 `docs/api/production/queue.md` §5「移除记录」。

pub mod board;
pub mod queue;
pub mod worker;

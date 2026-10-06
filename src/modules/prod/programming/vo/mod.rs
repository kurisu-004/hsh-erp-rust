//! prod::programming 域响应 VO（HTTP 返回值隔离层）
//!
//! 仅含 handler 返回的 output 类型；入参 DTO 见 [`super::dto`]（全部收在
//! `dto.rs`：本域只有 `ProgrammingListQuery` 一个入参结构体，handler 直接用
//! `Query<ProgrammingListQuery>` 提取，不需要 dashboard 那类「入参结构体定义在
//! handler.rs」的例外——那 3 个结构体是 WS 与下钻端点各自的 query，域内无处安放
//! 才落在 handler；本域无此情形）。
//!
//! ## 与 `super::dto` 的边界
//! VO **禁止**出现在 axum extractor 反序列化侧——`serde::Deserialize` 不实现；
//! 只用于 service 组装 + handler `Json(R::ok(...))` 返回值序列化。
//!
//! i64 一律走 `shared::types::serialize_i64` → JSON string（雪花 ID > 2^53，JS
//! `Number` 会丢精度，参见 `shared::types` 模块 doc）。唯一的例外是分页计数
//! （`total` / `limit` / `offset`），理由见 [`ProgrammingListOut`] 的文档。
//!
//! ## 与前端 schema 的对应
//! `ProgrammingItemOut` ↔ 前端待编程一览页的行类型，`ProgrammingListOut` ↔ 该页
//! 的列表响应（`items` + 分页三元组）。字段集**刻意收窄到 15 个**（工单标识 +
//! 展示 + 交期 + 客户 + CNC 程序标记 + 批次锚点），不加 `match_reason` 之类诊断
//! 字段——本端点只做「筛选 + 列表」，命中原因由规则语义（链含 CNC / 批次在 CNC
//! 工序）表达，前端不需要逐行归因。前端新增消费字段时，先改本文件再改
//! `repo.rs::ProgrammingRow`（两者逐字段一一对应）。
//!
//! ## 子文件划分
//! 本域出参只有 `GET /pending` 一组，行项与顶层响应是同一份契约的两层（`items`
//! 里装的就是行项），按「端点」切不开有信息的界，故**只放一个职责文件**
//! [`pending`]：拆成「行项 / 信封」两半只会让读一个字段的人多跳一次文件。
//! 若将来本域长出第二个端点（如「待下发一览」），届时按端点分文件，
//! 本文件即成 [`pending`] 的样板。
//!
//! 对外只 re-export [`ProgrammingItemOut`] 与 [`ProgrammingListOut`] 两个结构体
//! （精确 re-export，不做 glob）：本域类型不进任何 axum extractor，调用方只有
//! service 组装与 handler 序列化两处，都已显式列出名字。

pub mod pending;

pub use pending::{ProgrammingItemOut, ProgrammingListOut};

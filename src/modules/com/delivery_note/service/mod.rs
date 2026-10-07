//! com::delivery_note service 层入口
//!
//! 按职责拆为下列子模块：
//! - `group` —— `DeliveryGroupService`：送货分组 CRUD
//! - `crud` —— `DeliveryNoteService`：列表 / 详情 / 批量详情 / 编辑 / 移除批次
//! - `lifecycle` —— `DeliveryNoteService`：状态流转与读视图（提交 / 撤回 / 领取 /
//!   软删）
//! - `scan_tree` —— `DeliveryNoteService::scan_tree`：扫码三层树（**纯读**，不建单）
//! - `scan_entry` —— `DeliveryNoteService::scan_entry`：`POST /scan` 扫码入单
//!   （**唯一**入单入口；DP 分配 + 拆批 + 挂单在同一事务内）
//! - `batch_allocation` —— DP 批次分配纯函数（`allocate`；零 IO，10 个单测）
//! - `find_or_create` —— `scan_find_or_create_draft`：按单键
//!   `(customer_id, status='DRAFT')` 找或建草稿（含 23505 并发重查兜底）
//! - `shippable_sets` —— 装配件「可出货套数」纯函数（分子只计 `READY_TO_SHIP`；
//!   详情 VO 与扫码树共用同一公式）
//! - `inner` —— 跨子模块共享的私有 helper（`build_note_outs` / `get_with_parts` /
//!   `validate_*` / 错误构造器）
//!
//! ## service 形参 by-value trait（iam / shelf / customer 严格范本）
//! service 方法一律 `<R: DeliveryNoteRepoTrait>(&self, mut repo: R, ...)`，生产
//! `R = &mut PgConnection`（借 `&mut *tx` / `&mut *conn` 喂入）。跨域 ZST 静态调用
//! （`CustomerRepo::xxx` / `PartRepo::xxx` / `PartBatchRepo::xxx` /
//! `AssemblyRepo::xxx`）走 `&mut *repo.conn_mut()` 借位。
//!
//! ## service 装线 AppState
//! `DeliveryNoteService` / `DeliveryGroupService` 字段仅
//! `Arc<SnowflakeIdGenerator>`（事务已移交 handler；WS 广播也移交 handler）；
//! `Arc<DeliveryNoteService>` / `Arc<DeliveryGroupService>` 注入 `AppState`，
//! handler 调 `state.delivery_note_service.method(&mut *tx, ...)`。
//!
//! ## handler 三形态严格区分
//! ① 纯写端点 `pool.begin() → service → commit`；
//! ② 写 + post-commit 副作用（`scan_entry` / `submit` / `pickup` 等写后广播）
//! `pool.begin() → service → commit → state.ws_hub.broadcast(...)`；
//! ③ 读端点（`list_*` / `get_*` / `scan_tree`）`pool.acquire() → service`，不开事务。
//!
//! ## 本域 SQL 真源与胖 trait
//! SQL 全在 `repo/sql.rs` 与 `repo/scan_tree.rs`；胖 trait
//! `DeliveryNoteRepoTrait`（21 方法 = 11 group + 10 note）直接
//! `impl for &mut PgConnection`，service 内部调用走 `conn.note_xxx()` /
//! `conn.group_xxx()`，trait 另提供 `conn_mut()` 访问器供跨域 ZST 调用。

mod batch_allocation;
mod crud;
mod find_or_create;
mod group;
mod inner;
mod lifecycle;
mod scan_entry;
mod scan_tree;
mod shippable_sets;

/// 2026-10-04 新增：本单口径的装配件「可出货套数」纯内存计算。
///
/// 模块保持私有（与其余 service 子模块一致），只把函数放开到 crate 内 ——
/// 打印 handler（`handler/print.rs`）也要用它把套数注入转发 body，与
/// `inner.rs::get_with_parts` / `crud.rs::get_many_with_parts` 共用同一公式，
/// 避免三处各写一遍。范式 `outsource/service/mod.rs` 的 `pub(crate) fn` 导出。
pub(crate) use shippable_sets::note_shippable_sets;

use std::sync::Arc;

use crate::infra::snowflake::SnowflakeIdGenerator;

/// P1 送货分组 service（2026-09-22 D-5 + review 第 1 轮修正）。
///
/// 字段仅 `snowflake`；handler 借 `&mut *tx` / `&mut *conn` 喂给
/// `DeliveryNoteRepoTrait`（trait 已直接 `impl for &mut PgConnection`）。
pub struct DeliveryGroupService {
    snowflake: Arc<SnowflakeIdGenerator>,
}

/// delivery_note 主 service（2026-09-22 D-5 + review 第 1 轮修正）。
///
/// 字段仅 `snowflake`；handler 借 `&mut *tx` / `&mut *conn` 喂给
/// `DeliveryNoteRepoTrait`（trait 已直接 `impl for &mut PgConnection`）。
pub struct DeliveryNoteService {
    snowflake: Arc<SnowflakeIdGenerator>,
}

impl DeliveryGroupService {
    pub fn new(snowflake: Arc<SnowflakeIdGenerator>) -> Self {
        Self { snowflake }
    }
}

impl DeliveryNoteService {
    pub fn new(snowflake: Arc<SnowflakeIdGenerator>) -> Self {
        Self { snowflake }
    }
}

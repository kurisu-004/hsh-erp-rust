//! statistics 域（生产统计，MANAGER-only）
//!
//! 对应 Python myERP：
//! - api/v1/statistics.py
//! - service/statistics_service.py
//! - repository/statistics_repository.py
//! - schema/statistics.py
//!
//! 2026-09-15 takeover-fill：完整化 5 端点（overview / workers / workers/{id} /
//! pickup-skips / pickup-skips/{id}），权限统一 MANAGER-only。
//! 2026-10-12：pickup-skips 两端点随 `t_pickup_skip_event` 删除下线（**无 alias**，
//! 旧路径 404），剩 **3** 端点：`overview` / `workers` / `workers/{worker_id}`。
//!
//! 2026-10-12：`workers` / `workers/{worker_id}` 两个响应新增**有效工时**维度
//! （分钟整数），**端点集合与入参逐字未变**。口径细则集中在
//! `repo::sql::effective_minutes` 的骨架常量 doc 与 `vo::worker` 的顶部注释，
//! 整域契约见 `docs/api/statistics.md`。
//!
//! **域隔离护栏不适用**：本域是只读跨域聚合域（读 `t_part` / `t_part_event` /
//! `t_worker` / `t_work_type` / `t_process` / `t_process_chain_step` 六表），按既定
//! pattern 在本域 SQL 内直接 JOIN，不 import 别的域的 service / repo；唯一的跨域
//! 模块调用是经 `repo.conn_mut()` 借位的 `WorkerRepo` / `WorkTypeRepo` 两个单表查询
//! （与 `DeliveryNoteRepoTrait::conn_mut` 同形）。

pub mod dto;
pub mod handler;
pub mod repo;
pub mod service;
pub mod vo;

use crate::state::AppState;
use axum::Router;
use std::sync::Arc;

pub fn router() -> Router<Arc<AppState>> {
    handler::router()
}

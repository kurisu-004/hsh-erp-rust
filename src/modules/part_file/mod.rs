//! part_file 域
//!
//! 对应 Python myERP：
//! - api/v1/part_file.py
//! - service/part_file_service.py
//! - repository/part_file_repository.py
//! - model/part_file.py
//! - schema/part_file.py
//!
//! 2026-09-14 Phase 3：补齐 service / handler / DTO；polymorphic owner（PART / ASSEMBLY）；
//! SHA-256 CAS 去重 + COS 预签下载 URL。
//!
//! 2026-09-22 对齐 iam 范式：
//! - `repo.rs` → `repo/{mod, sql}.rs`：ZST `PartFileRepo` + 胖 trait `PartFileRepoTrait`。
//!   `PartFileRepo` 静态方法签名零 diff（跨模块调用方零修改）；`PartFileRepoTrait` 含
//!   12 方法（sql 6 + 跨域 owner 校验 2 + 软删 2 + cnc_pair 2），对 `&mut PgConnection`
//!   直接实现。
//! - `service.rs`：`PartFileService` 成为带字段结构（`Arc<SnowflakeIdGenerator>` +
//!   `Arc<dyn CosClient>`），方法签名 `<R: PartFileRepoTrait>(&self, mut repo: R, ...)`
//!   by-value；handler 借 `&mut *tx` / `&mut *conn` 喂给 trait。
pub mod dto;
pub mod handler;
pub mod model;
pub mod policy;
pub mod repo;
pub mod service;

use crate::state::AppState;
use axum::Router;
use std::sync::Arc;

pub fn router() -> Router<Arc<AppState>> {
    handler::router()
}
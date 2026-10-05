//! prod::inspection 子模块 handler 层 —— HTTP 路由
//!
//! 2026-10-05 新增：单只读端点。
//!
//! ## 端点
//! - `GET /api/v2/prod/inspection/scan/{serial_no}` —— Manager + Inspector
//!
//! ## 事务边界
//! 读端点：`pool.acquire()` 不开事务，**不发** WS 广播（纯查询，无业务流转）。
//!
//! ## 角色守卫
//! 在 service 第一行下沉（沿 `prod::batch` 范本），handler 不重复校验。
//!
//! ## 路径参数
//! `serial_no` 是 `Path<String>`（不是 `ni64!` 那类数值提取器）：序列号是
//! `varchar(15)` 的**字符串**，且可能含 `-`（子件 `{asm}-{i:02d}`），不能用数值
//! 提取器。空串 / 尾随空白由 service 统一 trim + 兜底（空串按未命中 → 20101）。

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};

use crate::auth::rbac::CurrentUser;
use crate::shared::error::AppError;
use crate::shared::response::R;
use crate::state::AppState;

use super::service::InspectionScanService;
use super::vo::ScanTreeOut;

/// `GET /api/v2/prod/inspection/scan/{serial_no}`
///
/// 扫码查询：返回「装配件（可空）→ 全部子件 → 全部批次」三层树。
///
/// 角色：Manager + Inspector（service 内守卫）。
/// 读端点：`pool.acquire()` 不开事务，不发 WS 广播。
pub async fn scan(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Path(serial_no): Path<String>,
) -> Result<Json<R<ScanTreeOut>>, AppError> {
    let mut conn = state.pool.acquire().await?;
    let out = InspectionScanService::scan(&mut conn, &current, &serial_no).await?;
    Ok(Json(R::ok(out)))
}

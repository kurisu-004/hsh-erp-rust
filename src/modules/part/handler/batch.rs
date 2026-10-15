//! part 域批量创建 handler
//!
//! `POST /api/v2/parts/batch` —— 批量创建零件（共享 `customer_id`）；per-item 失败
//! 不中断整体。
//!
//! ## 2026-10-15：**本端点不再绑定文件**
//!
//! 原先的 `items[].drawing_file` / `items[].model3d_file`（bindings 分支）与
//! `POST /files/objects/upload-intents` 签发的预签 PUT URL 配套：服务端先 head/copy
//! tmp 对象、再在事务里 INSERT `t_part_file`。统一上传链路后，文件一律走
//! `upload-intents` → 前端直传 → `confirm`（owner = 建单后拿到的 part_id），
//! 本端点回归**纯建单**，不再持有任何 `t_part_file` 写入路径。
//!
//! ## 错误码
//! - 20102 / 20308 / 20104：客户不存在 / 未配序列号 prefix / 非法客户层级
//! - 20108 / 20105：序列号派发失败
//! - 40901 之类的并发冲突：savepoint 内的 per-item 失败

use std::sync::Arc;

use axum::Json;
use axum::extract::State;

use crate::auth::rbac::CurrentUser;
use crate::modules::part::dto_crud::PartBatchCreateRequest;
use crate::modules::part::service::PartService;
use crate::modules::part::vo::PartBatchCreateOut;
use crate::shared::error::AppError;
use crate::shared::response::R;
use crate::state::AppState;

/// `POST /api/v2/parts/batch`
///
/// 批量创建（共享 `customer_id`）；per-item 失败不中断整体（每条 item 一个
/// savepoint，失败进 `failed[]`）。
///
/// 权限：Manager / Clerk（service 内复核一次）。
pub async fn batch_create_parts(
    State(state): State<Arc<AppState>>,
    current: CurrentUser,
    Json(req): Json<PartBatchCreateRequest>,
) -> Result<Json<R<PartBatchCreateOut>>, AppError> {
    use crate::auth::rbac::Role;
    current.require_any_role(&[Role::Manager, Role::Clerk])?;
    let mut tx = state.pool.begin().await?;
    let out_result =
        PartService::batch_create_parts(&mut *tx, &state.snowflake, &req, &current).await;
    // **登记（2026-10-05）**：Err 分支也 `commit()` 而不是 `rollback()`。
    // service 的 Err 出口分两类：循环之前（0 行落库 ⇒ 与 rollback 等价）与循环之内
    // （per-item 已 commit 的行 + 后续语句级失败）。何时会被引爆：往循环里加
    // **业务级**的 per-item `?` 时，前 k-1 件会被 commit 掉而客户端收到错误信封 ——
    // 届时应改成 `Err ⇒ tx.rollback()`。
    let commit_result = tx.commit().await;
    // commit 错误优先于 service Err 返回（让客户端看到真正的失败原因）
    commit_result.map_err(AppError::from)?;
    out_result.map(|out| Json(R::ok(out)))
}

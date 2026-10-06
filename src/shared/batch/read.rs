//! 跨域批次读取（2026-10-08 自 `prod::batch::repo::queries` 抽出）
//!
//! 只有一个函数 [`get_batch_by_id`] —— 「按主键取一条批次」是所有域都要的
//! 最小读取单元：progress 计算、OCC 锚确认、召回 / 拆分 / 扫码各流的起点。
//! 它原先挂在 `prod::batch::repo::queries` 这个 SQL 真源上，跨域调用方要么
//! import prod::batch 的 repo（让 queue 域反向依赖 batch 域），要么各自重写
//! 一份投影（列漂移无人发现）。放 shared 让「全列投影只有一份」成为结构事实。
//!
//! [`list_active_batches_by_part_id`] —— 「某工单的全部未删批次」是派生链
//! `rollup_part_derived` 的输入，与 [`get_batch_by_id`] 同属「批次这张表的公共
//! 语义」；它原先也挂在 `prod::batch::repo::queries`，搬进来是为了让 shared 层
//! **不反向 import prod::batch**（shared 唯一的反向依赖已登记在 `shared::batch`
//! 模块 doc 的边界小节，且仅限写派生）。
//!
//! 其余 `t_part_batch` 查询仍留在 `prod::batch::repo::queries`（只服务 prod 域
//! 自己的列表与流转用例）。

use sqlx::PgExecutor;

use crate::shared::batch::model::TPartBatch;

/// 按主键取一条批次行。
///
/// `include_deleted = true` 时**不**加软删闸门（仅供 rollup 派生 / 终态序列号
/// 归档等必须看到已软删行的场景使用；业务读端点应传 `false`）。
pub async fn get_batch_by_id<'e, E: PgExecutor<'e>>(
    executor: E,
    id: i64,
    include_deleted: bool,
) -> Result<Option<TPartBatch>, sqlx::Error> {
    sqlx::query_as!(
        TPartBatch,
        r#"
        SELECT id, part_id, batch_no, quantity, status, location,
               current_holder_id, current_process_id, current_process_step_id,
               delivery_note_id, parent_batch_id,
               is_repairing,
               version, created_at, created_by, updated_at, updated_by, deleted_at
        FROM t_part_batch
        WHERE id = $1
          AND ($2::bool OR deleted_at IS NULL)
        "#,
        id,
        include_deleted,
    )
    .fetch_optional(executor)
    .await
}

/// 按工单取全部未删批次（`batch_no ASC`）。
///
/// 派生链 `rollup_part_derived` 的输入：min-progress 派生需要该工单**全部**
/// 活跃批次的 status，故不过滤状态（终态批次参与 progress 计算）。「活跃」在本
/// 函数里的含义是「未软删」，与状态无关。
pub async fn list_active_batches_by_part_id<'e, E: PgExecutor<'e>>(
    executor: E,
    part_id: i64,
) -> Result<Vec<TPartBatch>, sqlx::Error> {
    sqlx::query_as!(
        TPartBatch,
        r#"
        SELECT id, part_id, batch_no, quantity, status, location,
               current_holder_id, current_process_id, current_process_step_id,
               delivery_note_id, parent_batch_id,
               is_repairing,
               version, created_at, created_by, updated_at, updated_by, deleted_at
        FROM t_part_batch
        WHERE part_id = $1 AND deleted_at IS NULL
        ORDER BY batch_no ASC
        "#,
        part_id,
    )
    .fetch_all(executor)
    .await
}

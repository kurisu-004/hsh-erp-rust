//! work_type 域数据访问
//!
//! 对应 Python myERP/repository/work_type_repository.py。函数签名接收 `impl PgExecutor<'_>`，
//! 兼容 `&PgPool` / `&mut PgConnection` / `&mut Transaction`。
//!
//! Phase P1（送货分组）只暴露 delivery_note 后续会用到的只读点：
//! - `get_by_id` —— 工种存在性 + name 校验（送货单 driver_worker.work_type_id 校验）
//!
//! worker-pool 域新增：
//! - `list_process_ids` —— 工种可用工序列表（白名单）

use sqlx::PgExecutor;

use super::model::TWorkType;

pub struct WorkTypeRepo;

impl WorkTypeRepo {
    pub async fn get_by_id<'e, E: PgExecutor<'e>>(
        executor: E,
        id: i64,
    ) -> Result<Option<TWorkType>, sqlx::Error> {
        sqlx::query_as!(
            TWorkType,
            r#"
            SELECT id, code, name, version,
                   created_at, created_by, updated_at, updated_by, deleted_at,
                   max_held_batches
            FROM t_work_type
            WHERE id = $1 AND deleted_at IS NULL
            "#,
            id,
        )
        .fetch_optional(executor)
        .await
    }

    /// 工种可执行的工序 id 列表（worker-pool 校验 worker 操作的 process 是否属于其工种）。
    /// `t_work_type_process` 无业务软删（mapping 表通常保留历史），不筛 `deleted_at`。
    pub async fn list_process_ids<'e, E: PgExecutor<'e>>(
        executor: E,
        work_type_id: i64,
    ) -> Result<Vec<i64>, sqlx::Error> {
        let rows: Vec<i64> = sqlx::query_scalar!(
            r#"
            SELECT process_id AS "process_id!"
            FROM t_work_type_process
            WHERE work_type_id = $1
            "#,
            work_type_id,
        )
        .fetch_all(executor)
        .await?;
        Ok(rows)
    }
}

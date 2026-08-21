//! worker 域数据访问
//!
//! 对应 Python myERP/repository/worker_repository.py。函数签名接收 `impl PgExecutor<'_>`，
//! 兼容 `&PgPool` / `&mut PgConnection` / `&mut Transaction`。
//!
//! Phase P1（送货分组）只暴露 delivery_note 后续会用到的只读点：
//! - `get_by_id` —— 司机存在性 + is_active + work_type 校验

use sqlx::PgExecutor;

use super::model::TWorker;

pub struct WorkerRepo;

impl WorkerRepo {
    pub async fn get_by_id<'e, E: PgExecutor<'e>>(
        executor: E,
        id: i64,
        include_deleted: bool,
    ) -> Result<Option<TWorker>, sqlx::Error> {
        sqlx::query_as!(
            TWorker,
            r#"
            SELECT id, badge_code, name, is_active,
                   work_type_id, version,
                   created_at, created_by, updated_at, updated_by, deleted_at
            FROM t_worker
            WHERE id = $1
              AND ($2::bool OR deleted_at IS NULL)
            "#,
            id,
            include_deleted,
        )
        .fetch_optional(executor)
        .await
    }
}
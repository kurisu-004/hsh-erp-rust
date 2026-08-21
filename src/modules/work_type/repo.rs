//! work_type 域数据访问
//!
//! 对应 Python myERP/repository/work_type_repository.py。函数签名接收 `impl PgExecutor<'_>`，
//! 兼容 `&PgPool` / `&mut PgConnection` / `&mut Transaction`。
//!
//! Phase P1（送货分组）只暴露 delivery_note 后续会用到的只读点：
//! - `get_by_id` —— 校验司机 work_type.code='送货司机'（Phase P2）

use sqlx::PgExecutor;

use super::model::TWorkType;

pub struct WorkTypeRepo;

impl WorkTypeRepo {
    pub async fn get_by_id<'e, E: PgExecutor<'e>>(
        executor: E,
        id: i64,
        include_deleted: bool,
    ) -> Result<Option<TWorkType>, sqlx::Error> {
        sqlx::query_as!(
            TWorkType,
            r#"
            SELECT id, code, name, max_held_batches,
                   version, created_at, created_by, updated_at, updated_by, deleted_at
            FROM t_work_type
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
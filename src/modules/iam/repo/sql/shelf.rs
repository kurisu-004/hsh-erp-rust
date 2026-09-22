//! iam 域 `t_shelf` SQL 真源（1 方法，本域只读）

use sqlx::PgExecutor;

use crate::modules::iam::repo::model::Shelf;

pub async fn get_shelf_by_id<'e, E: PgExecutor<'e>>(
    executor: E,
    id: i64,
) -> Result<Option<Shelf>, sqlx::Error> {
    sqlx::query_as!(
        Shelf,
        r#"
        SELECT id, code, name, zone, location, is_active, display_order, version,
               created_at, created_by, updated_at, updated_by, deleted_at
        FROM t_shelf
        WHERE id = $1 AND deleted_at IS NULL
        "#,
        id
    )
    .fetch_optional(executor)
    .await
}
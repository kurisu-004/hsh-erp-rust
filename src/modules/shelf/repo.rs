//! shelf 域数据访问
//!
//! 当前仅暴露 scan-inspect / fail-inspection 所需的只读点（zone 校验 + active 校验）。
//! 完整 shelf CRUD（handler / service / dto）留待后续 PR。

use sqlx::PgConnection;

use super::model::TShelf;

pub struct ShelfRepo;

impl ShelfRepo {
    /// 按 id 查 active 货架（is_active=true, deleted_at IS NULL）。用于 INSPECTION/PRODUCTION 区校验。
    pub async fn get_active_by_id(
        conn: &mut PgConnection,
        id: i64,
    ) -> Result<Option<TShelf>, sqlx::Error> {
        sqlx::query_as!(
            TShelf,
            r#"
            SELECT id, code, name, zone, is_active, display_order,
                   version, created_at, created_by, updated_at, updated_by, deleted_at
            FROM t_shelf
            WHERE id = $1 AND is_active = true AND deleted_at IS NULL
            "#,
            id,
        )
        .fetch_optional(&mut *conn)
        .await
    }

    /// 按 id 查（不强制 is_active；用于 service 层区分 20501 NOT_FOUND vs 20512 INACTIVE）。
    pub async fn get_by_id(
        conn: &mut PgConnection,
        id: i64,
    ) -> Result<Option<TShelf>, sqlx::Error> {
        sqlx::query_as!(
            TShelf,
            r#"
            SELECT id, code, name, zone, is_active, display_order,
                   version, created_at, created_by, updated_at, updated_by, deleted_at
            FROM t_shelf
            WHERE id = $1 AND deleted_at IS NULL
            "#,
            id,
        )
        .fetch_optional(&mut *conn)
        .await
    }
}
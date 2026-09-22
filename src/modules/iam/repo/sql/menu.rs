//! iam 域 `t_menu` SQL 真源（1 方法）

use sqlx::PgExecutor;

use crate::modules::iam::repo::model::Menu;

/// 取给定角色集合可见的全部启用菜单（去重）。角色间菜单可重叠，故 `DISTINCT`。
pub async fn list_active_menus_by_roles<'e, E: PgExecutor<'e>>(
    executor: E,
    roles: &[String],
) -> Result<Vec<Menu>, sqlx::Error> {
    sqlx::query_as!(
        Menu,
        r#"
        SELECT DISTINCT
               m.id, m.parent_id, m.code, m.title, m.path, m.icon,
               m.sort_order, m.is_active, m.version,
               m.created_at, m.created_by, m.updated_at, m.updated_by, m.deleted_at
        FROM t_menu m
        JOIN t_role_menu rm ON rm.menu_id = m.id
        WHERE rm.role = ANY($1)
          AND m.is_active = TRUE
          AND m.deleted_at IS NULL
          AND rm.deleted_at IS NULL
        ORDER BY m.sort_order, m.code
        "#,
        roles
    )
    .fetch_all(executor)
    .await
}
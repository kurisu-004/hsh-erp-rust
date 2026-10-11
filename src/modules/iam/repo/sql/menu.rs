//! iam 域 `t_menu` SQL 真源（2 方法）

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

/// 全部启用中的菜单，**不经角色过滤**（2026-10-14 新增，供
/// `iam::rbac` 的 `GET /iam/menus` 给 ADMIN 渲染完整勾选树）。
///
/// 与 `list_active_menus_by_roles` 的差别**只有**少了那三行角色过滤，排序键
/// 与列投影逐字相同 —— 两条 SQL 都要喂同一个 `build_menu_tree`，列集或排序
/// 不一致会让 ADMIN 与普通管理员看到形状不同的树。
pub async fn list_all_active_menus<'e, E: PgExecutor<'e>>(
    executor: E,
) -> Result<Vec<Menu>, sqlx::Error> {
    sqlx::query_as!(
        Menu,
        r#"
        SELECT m.id, m.parent_id, m.code, m.title, m.path, m.icon,
               m.sort_order, m.is_active, m.version,
               m.created_at, m.created_by, m.updated_at, m.updated_by, m.deleted_at
        FROM t_menu m
        WHERE m.is_active = TRUE
          AND m.deleted_at IS NULL
        ORDER BY m.sort_order, m.code
        "#,
    )
    .fetch_all(executor)
    .await
}

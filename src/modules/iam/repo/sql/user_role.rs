//! iam 域 `t_user_role` SQL 真源（5 方法）

use chrono::NaiveDateTime;
use sqlx::PgExecutor;

use crate::modules::iam::repo::model::UserRole;

// ===========================================================================
// 读模型 + 入参 DTO
// ===========================================================================

/// `t_user_role` LEFT JOIN `t_shelf` 后的读模型（附带货架编号/名称）
#[derive(Debug, Clone)]
pub struct UserRoleRow {
    pub id: i64,
    pub user_id: i64,
    pub role: String,
    pub scope_type: Option<String>,
    pub scope_id: Option<i64>,
    pub version: i32,
    pub created_at: NaiveDateTime,
    pub created_by: Option<i64>,
    pub updated_at: NaiveDateTime,
    pub updated_by: Option<i64>,
    pub deleted_at: Option<NaiveDateTime>,
    pub shelf_code: Option<String>,
    pub shelf_name: Option<String>,
}

/// `t_user_role` INSERT 入参
pub struct UserRoleInsert {
    pub id: i64,
    pub user_id: i64,
    pub role: String,
    pub scope_type: Option<String>,
    pub scope_id: Option<i64>,
    pub created_at: NaiveDateTime,
    pub created_by: Option<i64>,
}

// ===========================================================================
// SQL 真源（free fn）
// ===========================================================================

/// 列出用户的全部有效角色，并 LEFT JOIN `t_shelf` 带出 SHELF_ACCOUNT 的货架编号/名称。
pub async fn list_user_roles_by_user_id<'e, E: PgExecutor<'e>>(
    executor: E,
    user_id: i64,
) -> Result<Vec<UserRoleRow>, sqlx::Error> {
    sqlx::query_as!(
        UserRoleRow,
        r#"
        SELECT ur.id            AS "id!",
               ur.user_id       AS "user_id!",
               ur.role          AS "role!",
               ur.scope_type    AS "scope_type?",
               ur.scope_id      AS "scope_id?",
               ur.version       AS "version!",
               ur.created_at    AS "created_at!",
               ur.created_by    AS "created_by?",
               ur.updated_at    AS "updated_at!",
               ur.updated_by    AS "updated_by?",
               ur.deleted_at    AS "deleted_at?",
               s.code           AS "shelf_code?",
               s.name           AS "shelf_name?"
        FROM t_user_role ur
        LEFT JOIN t_shelf s
               ON ur.scope_type = 'shelf'
              AND ur.scope_id = s.id
              AND s.deleted_at IS NULL
        WHERE ur.user_id = $1 AND ur.deleted_at IS NULL
        ORDER BY ur.created_at, ur.id
        "#,
        user_id
    )
    .fetch_all(executor)
    .await
}

pub async fn get_user_role_by_id<'e, E: PgExecutor<'e>>(
    executor: E,
    id: i64,
) -> Result<Option<UserRole>, sqlx::Error> {
    sqlx::query_as!(
        UserRole,
        r#"
        SELECT id, user_id, role, scope_type, scope_id, version,
               created_at, created_by, updated_at, updated_by, deleted_at
        FROM t_user_role
        WHERE id = $1 AND deleted_at IS NULL
        "#,
        id
    )
    .fetch_optional(executor)
    .await
}

/// 查重：`(user_id, role, scope_type, scope_id)` 在未软删记录中是否已存在。
///
/// `scope_type` / `scope_id` 可为 NULL，故用 `IS NOT DISTINCT FROM` 而非 `=`
/// （SQL 里 `NULL = NULL` 为 NULL 而非 true，会漏判重复）。
pub async fn has_user_role_with_scope<'e, E: PgExecutor<'e>>(
    executor: E,
    user_id: i64,
    role: &str,
    scope_type: Option<&str>,
    scope_id: Option<i64>,
) -> Result<bool, sqlx::Error> {
    let row = sqlx::query!(
        r#"
        SELECT EXISTS (
            SELECT 1 FROM t_user_role
            WHERE user_id = $1
              AND role = $2
              AND scope_type IS NOT DISTINCT FROM $3::varchar
              AND scope_id IS NOT DISTINCT FROM $4::bigint
              AND deleted_at IS NULL
        ) AS "exists!"
        "#,
        user_id,
        role,
        scope_type,
        scope_id
    )
    .fetch_one(executor)
    .await?;
    Ok(row.exists)
}

pub async fn create_user_role<'e, E: PgExecutor<'e>>(
    executor: E,
    role_row: &UserRoleInsert,
) -> Result<(), sqlx::Error> {
    sqlx::query!(
        r#"
        INSERT INTO t_user_role (
            id, user_id, role, scope_type, scope_id, version,
            created_at, created_by, updated_at, updated_by
        )
        VALUES ($1, $2, $3, $4, $5, 0, $6, $7, $6, $7)
        "#,
        role_row.id,
        role_row.user_id,
        role_row.role,
        role_row.scope_type,
        role_row.scope_id,
        role_row.created_at,
        role_row.created_by,
    )
    .execute(executor)
    .await?;
    Ok(())
}

pub async fn soft_delete_user_role<'e, E: PgExecutor<'e>>(
    executor: E,
    id: i64,
    version: i32,
    when: NaiveDateTime,
    updated_by: Option<i64>,
) -> Result<u64, sqlx::Error> {
    let res = sqlx::query!(
        r#"
        UPDATE t_user_role
        SET deleted_at = $3,
            version    = version + 1,
            updated_at = $3,
            updated_by = $4
        WHERE id = $1 AND version = $2 AND deleted_at IS NULL
        "#,
        id,
        version,
        when,
        updated_by,
    )
    .execute(executor)
    .await?;
    Ok(res.rows_affected())
}
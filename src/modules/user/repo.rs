//! user 域数据访问
//!
//! 对应 Python myERP/repository/user_repository.py。函数签名接收 `impl PgExecutor<'_>`，
//! 兼容 `&PgPool` / `&mut PgConnection` / `&mut Transaction`。
//!
//! 约定：
//! - 全部使用 `sqlx::query!` / `query_as!` 编译期宏（需 `DATABASE_URL` 或 `.sqlx/` 离线元数据）
//! - 读查询一律带 `deleted_at IS NULL`（软删）
//! - 写查询带 `WHERE id = $1 AND version = $2` 乐观锁，返回 `rows_affected`，0 行由 service 转 409
//! - 时间戳由调用方传入 `crate::infra::clock::now_naive()`（Asia/Shanghai），
//!   **不使用 DB 的 `now()`**——容器时区为 UTC，会与应用侧写入的 naive 时间不一致。

use chrono::NaiveDateTime;
use sqlx::PgExecutor;

use super::model::{Menu, Shelf, User, UserRole};

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

/// `t_user` INSERT 入参（id 与审计字段由 service 用雪花/时钟填好）
pub struct UserInsert {
    pub id: i64,
    pub username: String,
    pub password_hash: String,
    pub full_name: String,
    pub phone: Option<String>,
    pub is_active: bool,
    pub created_at: NaiveDateTime,
    pub created_by: Option<i64>,
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

// ---------------------------------------------------------------------------
// UserRepo
// ---------------------------------------------------------------------------

pub struct UserRepo;

impl UserRepo {
    pub async fn get_by_id<'e, E: PgExecutor<'e>>(
        executor: E,
        id: i64,
    ) -> Result<Option<User>, sqlx::Error> {
        sqlx::query_as!(
            User,
            r#"
            SELECT id, username, password_hash, full_name, phone, is_active,
                   last_login_at, refresh_token_version, version,
                   created_at, created_by, updated_at, updated_by, deleted_at
            FROM t_user
            WHERE id = $1 AND deleted_at IS NULL
            "#,
            id
        )
        .fetch_optional(executor)
        .await
    }

    /// 按用户名精确查（调用方需先 `.trim().to_lowercase()`）
    pub async fn get_by_username<'e, E: PgExecutor<'e>>(
        executor: E,
        username_lower: &str,
    ) -> Result<Option<User>, sqlx::Error> {
        sqlx::query_as!(
            User,
            r#"
            SELECT id, username, password_hash, full_name, phone, is_active,
                   last_login_at, refresh_token_version, version,
                   created_at, created_by, updated_at, updated_by, deleted_at
            FROM t_user
            WHERE username = $1 AND deleted_at IS NULL
            "#,
            username_lower
        )
        .fetch_optional(executor)
        .await
    }

    /// 条件列表。过滤条件用 `$n IS NULL OR ...` 在 SQL 内做可选分支，
    /// 以便继续使用编译期校验的 `query_as!`（而非 QueryBuilder 动态拼接）。
    pub async fn list_with_filters<'e, E: PgExecutor<'e>>(
        executor: E,
        username_like: Option<&str>,
        is_active: Option<bool>,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<User>, sqlx::Error> {
        sqlx::query_as!(
            User,
            r#"
            SELECT id, username, password_hash, full_name, phone, is_active,
                   last_login_at, refresh_token_version, version,
                   created_at, created_by, updated_at, updated_by, deleted_at
            FROM t_user
            WHERE deleted_at IS NULL
              AND ($1::text IS NULL OR username ILIKE '%' || $1 || '%')
              AND ($2::bool IS NULL OR is_active = $2)
            ORDER BY created_at DESC, id DESC
            LIMIT $3 OFFSET $4
            "#,
            username_like,
            is_active,
            limit,
            offset
        )
        .fetch_all(executor)
        .await
    }

    pub async fn count_with_filters<'e, E: PgExecutor<'e>>(
        executor: E,
        username_like: Option<&str>,
        is_active: Option<bool>,
    ) -> Result<i64, sqlx::Error> {
        let row = sqlx::query!(
            r#"
            SELECT COUNT(*) AS "count!"
            FROM t_user
            WHERE deleted_at IS NULL
              AND ($1::text IS NULL OR username ILIKE '%' || $1 || '%')
              AND ($2::bool IS NULL OR is_active = $2)
            "#,
            username_like,
            is_active
        )
        .fetch_one(executor)
        .await?;
        Ok(row.count)
    }

    pub async fn create<'e, E: PgExecutor<'e>>(
        executor: E,
        user: &UserInsert,
    ) -> Result<(), sqlx::Error> {
        sqlx::query!(
            r#"
            INSERT INTO t_user (
                id, username, password_hash, full_name, phone, is_active,
                refresh_token_version, version,
                created_at, created_by, updated_at, updated_by
            )
            VALUES ($1, $2, $3, $4, $5, $6, 0, 0, $7, $8, $7, $8)
            "#,
            user.id,
            user.username,
            user.password_hash,
            user.full_name,
            user.phone,
            user.is_active,
            user.created_at,
            user.created_by,
        )
        .execute(executor)
        .await?;
        Ok(())
    }

    /// 部分更新：未提供的字段保持原值。带乐观锁，返回影响行数。
    ///
    /// `phone` 需要区分「不修改」与「显式清空」两种语义，故用 `set_phone` 旗标 +
    /// `CASE WHEN` 而非 `COALESCE`：Python 侧 `data.phone.strip() or None` 允许传空串
    /// 把手机号置 NULL，单靠 `COALESCE` 表达不了（`COALESCE(NULL, phone)` 会保留原值）。
    /// 其余字段「`None` = 不修改」，`COALESCE` 即可。
    ///
    /// 注意：本函数**不**轮转 `refresh_token_version`——即使传入了新的 `password_hash`。
    /// 与 Python `update_user` 一致（管理员在此改密不踢下线；自助改密与管理员重置
    /// 走 `update_password_and_rotate`，才会踢下线）。
    #[allow(clippy::too_many_arguments)]
    pub async fn update_partial<'e, E: PgExecutor<'e>>(
        executor: E,
        id: i64,
        version: i32,
        full_name: Option<&str>,
        set_phone: bool,
        phone: Option<&str>,
        password_hash: Option<&str>,
        is_active: Option<bool>,
        when: NaiveDateTime,
        updated_by: Option<i64>,
    ) -> Result<u64, sqlx::Error> {
        let res = sqlx::query!(
            r#"
            UPDATE t_user
            SET full_name     = COALESCE($3::varchar, full_name),
                phone         = CASE WHEN $4::bool THEN $5::varchar ELSE phone END,
                password_hash = COALESCE($6::varchar, password_hash),
                is_active     = COALESCE($7::bool, is_active),
                version       = version + 1,
                updated_at    = $8,
                updated_by    = $9
            WHERE id = $1 AND version = $2 AND deleted_at IS NULL
            "#,
            id,
            version,
            full_name,
            set_phone,
            phone,
            password_hash,
            is_active,
            when,
            updated_by,
        )
        .execute(executor)
        .await?;
        Ok(res.rows_affected())
    }

    /// 软删除：置 `deleted_at` + `is_active = false`，带乐观锁。
    pub async fn soft_delete<'e, E: PgExecutor<'e>>(
        executor: E,
        id: i64,
        version: i32,
        when: NaiveDateTime,
        updated_by: Option<i64>,
    ) -> Result<u64, sqlx::Error> {
        let res = sqlx::query!(
            r#"
            UPDATE t_user
            SET deleted_at = $3,
                is_active  = FALSE,
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

    /// 登录成功后刷新 `last_login_at`（不动 version，避免与并发业务更新冲突）
    pub async fn touch_login<'e, E: PgExecutor<'e>>(
        executor: E,
        id: i64,
        when: NaiveDateTime,
    ) -> Result<(), sqlx::Error> {
        sqlx::query!(
            r#"UPDATE t_user SET last_login_at = $2 WHERE id = $1 AND deleted_at IS NULL"#,
            id,
            when
        )
        .execute(executor)
        .await?;
        Ok(())
    }

    /// 轮转 refresh token 版本（登出/自助改密/refresh 轮换），使旧 refresh token 立即作废。
    pub async fn increment_refresh_token_version<'e, E: PgExecutor<'e>>(
        executor: E,
        id: i64,
        version: i32,
        when: NaiveDateTime,
        updated_by: Option<i64>,
    ) -> Result<u64, sqlx::Error> {
        let res = sqlx::query!(
            r#"
            UPDATE t_user
            SET refresh_token_version = refresh_token_version + 1,
                version               = version + 1,
                updated_at            = $3,
                updated_by            = $4
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

    /// 自助改密：在**同一条 UPDATE** 内写 `password_hash` 并轮转 `refresh_token_version`，
    /// 保证「改密即踢下线」是原子的（对齐 Python `change_own_password`）。
    pub async fn update_password_and_rotate<'e, E: PgExecutor<'e>>(
        executor: E,
        id: i64,
        version: i32,
        password_hash: &str,
        when: NaiveDateTime,
        updated_by: Option<i64>,
    ) -> Result<u64, sqlx::Error> {
        let res = sqlx::query!(
            r#"
            UPDATE t_user
            SET password_hash         = $3,
                refresh_token_version = refresh_token_version + 1,
                version               = version + 1,
                updated_at            = $4,
                updated_by            = $5
            WHERE id = $1 AND version = $2 AND deleted_at IS NULL
            "#,
            id,
            version,
            password_hash,
            when,
            updated_by,
        )
        .execute(executor)
        .await?;
        Ok(res.rows_affected())
    }
}

// ---------------------------------------------------------------------------
// UserRoleRepo
// ---------------------------------------------------------------------------

pub struct UserRoleRepo;

impl UserRoleRepo {
    /// 列出用户的全部有效角色，并 LEFT JOIN `t_shelf` 带出 SHELF_ACCOUNT 的货架编号/名称。
    pub async fn list_by_user<'e, E: PgExecutor<'e>>(
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

    pub async fn get_by_id<'e, E: PgExecutor<'e>>(
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
    pub async fn exists_same_scope<'e, E: PgExecutor<'e>>(
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

    pub async fn create<'e, E: PgExecutor<'e>>(
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

    pub async fn soft_delete<'e, E: PgExecutor<'e>>(
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
}

// ---------------------------------------------------------------------------
// MenuRepo
// ---------------------------------------------------------------------------

pub struct MenuRepo;

impl MenuRepo {
    /// 取给定角色集合可见的全部启用菜单（去重）。角色间菜单可重叠，故 `DISTINCT`。
    pub async fn list_active_for_roles<'e, E: PgExecutor<'e>>(
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
}

// ---------------------------------------------------------------------------
// ShelfRepo
// ---------------------------------------------------------------------------

pub struct ShelfRepo;

impl ShelfRepo {
    pub async fn get_by_id<'e, E: PgExecutor<'e>>(
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
}

//! iam 域 `t_user` SQL 真源（8 读 + 写）
//!
//! 与 SQL 紧耦合的入参 DTO（`UserInsert` / `UserPartialUpdate`）也在本文件定义。
//! 2026-09-22 重构 #2：原 `sql.rs::UserRepo` struct 改为 free fn。
//!
//! impl 块统一收在 `super::mod.rs`（Rust coherence 规则：同 crate 内同一 trait 对同一类型
//! 至多一个 impl 块，跨文件分散 4 个会 E0119）。本文件仅放 SQL 真源。

use chrono::NaiveDateTime;
use sqlx::PgExecutor;

use crate::modules::iam::repo::model::User;

// ===========================================================================
// 入参 DTO
// ===========================================================================

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

/// `t_user` 部分更新字段集（取代 9 参数 update_partial）
///
/// 把可变参数封装在一处，便于 service / 测试 / mockall 共同引用；与 Python
/// `update_user` `exclude_unset` 语义对齐：`None` = 不修改。
///
/// `phone` 用 `set_phone` 旗标区分「不修改」与「显式清空」两种语义——
/// Python 侧 `data.phone.strip() or None` 允许传空串把手机号置 NULL，
/// 单靠 `Option::None` 表达不了。
pub struct UserPartialUpdate<'a> {
    pub full_name: Option<&'a str>,
    pub set_phone: bool,
    pub phone: Option<&'a str>,
    pub password_hash: Option<&'a str>,
    pub is_active: Option<bool>,
    pub when: NaiveDateTime,
    pub updated_by: Option<i64>,
}

// ===========================================================================
// SQL 真源（free fn）
// ===========================================================================

pub async fn get_user_by_id<'e, E: PgExecutor<'e>>(
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
pub async fn get_user_by_username<'e, E: PgExecutor<'e>>(
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
pub async fn list_users_with_filters<'e, E: PgExecutor<'e>>(
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

pub async fn count_users_with_filters<'e, E: PgExecutor<'e>>(
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

pub async fn create_user<'e, E: PgExecutor<'e>>(
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
/// 走 `update_user_password_and_rotate`，才会踢下线）。
pub async fn update_user_partial<'e, E: PgExecutor<'e>>(
    executor: E,
    id: i64,
    version: i32,
    args: &UserPartialUpdate<'_>,
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
        args.full_name,
        args.set_phone,
        args.phone,
        args.password_hash,
        args.is_active,
        args.when,
        args.updated_by,
    )
    .execute(executor)
    .await?;
    Ok(res.rows_affected())
}

/// 软删除：置 `deleted_at` + `is_active = false`，带乐观锁。
pub async fn soft_delete_user<'e, E: PgExecutor<'e>>(
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
pub async fn touch_user_last_login_at<'e, E: PgExecutor<'e>>(
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
pub async fn increment_user_refresh_token_version<'e, E: PgExecutor<'e>>(
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
pub async fn update_user_password_and_rotate<'e, E: PgExecutor<'e>>(
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
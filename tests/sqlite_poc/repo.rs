//! 2026-09-22 新增：SQLite POC 简化版 t_user repo。
//!
//! **这是 POC 简化版，绕开 PG ILIKE / ANY 数组 / 类型 cast，与
//! `src/modules/iam/repo/sql.rs` 的 PG 版并存、不共享 SQL。**
//!
//! 设计要点：
//!   - 仅 `get_by_id` / `create` / `touch_login` 三个最简单方法的 SQLite 版本，对齐 PG 版同名方法语义。
//!   - 不实现 `IamRepo` trait（绕开 service / handler / AppState）——纯静态 async fn + `&mut SqliteConnection`。
//!   - **不用 `sqlx::query!` 宏**：SQLite 无编译期离线元数据（.sqlx/ 是 PG 元数据，混用会 cache miss）；
//!     用 `sqlx::query` / `query_as` + `FromRow` derive。
//!   - 时间戳由调用方传入 ISO8601 字符串（`chrono::NaiveDateTime::to_string()`），不走 SQLite `datetime('now')`：
//!     与 PG 范式对齐（应用侧统一时钟，避免容器时区漂移）。
//!
//! 局限（不准备在 POC 里修复）：
//!   - `is_active` SQLite 存 INTEGER 0/1，PG 存 boolean —— POC 用 `bool` FromRow 靠 sqlx-sqlite 的自动转换。
//!   - 不实现软删查询（WHERE deleted_at IS NULL）——本表刻意不引入软删路径，保持最小切面。

use chrono::NaiveDateTime;
use serde::Serialize;
use sqlx::{FromRow, SqliteConnection};

/// `t_user` 行模型（SQLite 版，POC 简化版：不含 PG 特有的 refresh_token_version 等列）。
#[derive(Debug, Clone, FromRow, Serialize)]
pub struct UserRow {
    pub id: i64,
    pub username: String,
    pub password_hash: String,
    pub full_name: String,
    pub phone: Option<String>,
    pub is_active: bool,
    pub last_login_at: Option<NaiveDateTime>,
    pub version: i32,
    pub created_at: NaiveDateTime,
    pub created_by: Option<i64>,
    pub updated_at: NaiveDateTime,
    pub updated_by: Option<i64>,
    pub deleted_at: Option<NaiveDateTime>,
}

/// INSERT 入参（id 与审计字段由 service 用雪花/时钟填好；POC 测试用例直接在 caller 构造）。
pub struct UserInsert<'a> {
    pub id: i64,
    pub username: &'a str,
    pub password_hash: &'a str,
    pub full_name: &'a str,
    pub phone: Option<&'a str>,
    pub is_active: bool,
    pub created_at: NaiveDateTime,
    pub created_by: Option<i64>,
}

/// 按 ID 查（POI POC 不带 `deleted_at IS NULL`，因为本表不演示软删语义）。
pub async fn get_by_id(
    conn: &mut SqliteConnection,
    id: i64,
) -> Result<Option<UserRow>, sqlx::Error> {
    sqlx::query_as::<_, UserRow>(
        "SELECT id, username, password_hash, full_name, phone, is_active,
                last_login_at, version,
                created_at, created_by, updated_at, updated_by, deleted_at
         FROM t_user
         WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(conn)
    .await
}

/// 插入（对齐 PG 版 `create`：version=0、updated_at=created_at、updated_by=created_by）。
pub async fn create(
    conn: &mut SqliteConnection,
    user: &UserInsert<'_>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO t_user (
            id, username, password_hash, full_name, phone, is_active,
            version, created_at, created_by, updated_at, updated_by
         ) VALUES (?, ?, ?, ?, ?, ?, 0, ?, ?, ?, ?)",
    )
    .bind(user.id)
    .bind(user.username)
    .bind(user.password_hash)
    .bind(user.full_name)
    .bind(user.phone)
    .bind(user.is_active)
    .bind(user.created_at)
    .bind(user.created_by)
    .bind(user.created_at)
    .bind(user.created_by)
    .execute(conn)
    .await?;
    Ok(())
}

/// 登录成功后刷新 `last_login_at`（不动 version，对齐 PG 版 `touch_login`）。
pub async fn touch_login(
    conn: &mut SqliteConnection,
    id: i64,
    when: NaiveDateTime,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE t_user SET last_login_at = ? WHERE id = ?")
        .bind(when)
        .bind(id)
        .execute(conn)
        .await?;
    Ok(())
}

//! assembly 域数据访问
//!
//! 对应 Python myERP/repository/assembly_repository.py。函数签名接收 `impl PgExecutor<'_>`，
//! 兼容 `&PgPool` / `&mut PgConnection` / `&mut Transaction`。
//!
//! Phase P1（送货分组）只暴露 delivery_note + delivery_group 后续会用到的只读点：
//! - `get_by_id` / `list_by_ids`
//! - `get_by_serial` —— 扫码入单解析（Phase P3）需要 exact match

use sqlx::PgExecutor;

use super::model::TAssembly;

pub struct AssemblyRepo;

impl AssemblyRepo {
    pub async fn get_by_id<'e, E: PgExecutor<'e>>(
        executor: E,
        id: i64,
        include_deleted: bool,
    ) -> Result<Option<TAssembly>, sqlx::Error> {
        sqlx::query_as!(
            TAssembly,
            r#"
            SELECT id, drawing_no, name, customer_id, status, serial_no,
                   version, created_at, created_by, updated_at, updated_by, deleted_at,
                   order_no
            FROM t_assembly
            WHERE id = $1
              AND ($2::bool OR deleted_at IS NULL)
            "#,
            id,
            include_deleted,
        )
        .fetch_optional(executor)
        .await
    }

    pub async fn list_by_ids<'e, E: PgExecutor<'e>>(
        executor: E,
        ids: &[i64],
        include_deleted: bool,
    ) -> Result<Vec<TAssembly>, sqlx::Error> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        sqlx::query_as!(
            TAssembly,
            r#"
            SELECT id, drawing_no, name, customer_id, status, serial_no,
                   version, created_at, created_by, updated_at, updated_by, deleted_at,
                   order_no
            FROM t_assembly
            WHERE id = ANY($1)
              AND ($2::bool OR deleted_at IS NULL)
            ORDER BY id ASC
            "#,
            ids,
            include_deleted,
        )
        .fetch_all(executor)
        .await
    }

    /// 按 `serial_no` exact match 查（扫码定位用）。
    /// `t_assembly` 上有 partial unique（`uk_t_assembly_serial_no`），活跃行只可能一条。
    pub async fn get_by_serial<'e, E: PgExecutor<'e>>(
        executor: E,
        serial_no: &str,
        include_deleted: bool,
    ) -> Result<Option<TAssembly>, sqlx::Error> {
        sqlx::query_as!(
            TAssembly,
            r#"
            SELECT id, drawing_no, name, customer_id, status, serial_no,
                   version, created_at, created_by, updated_at, updated_by, deleted_at,
                   order_no
            FROM t_assembly
            WHERE serial_no = $1
              AND ($2::bool OR deleted_at IS NULL)
            "#,
            serial_no,
            include_deleted,
        )
        .fetch_optional(executor)
        .await
    }
}
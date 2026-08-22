//! part 域数据访问
//!
//! 对应 Python myERP/repository/part_repository.py。函数签名接收 `impl PgExecutor<'_>`，
//! 兼容 `&PgPool` / `&mut PgConnection` / `&mut Transaction`。
//!
//! Phase P1（送货分组）只暴露 delivery_note + delivery_group 后续会用到的只读点：
//! - `get_by_id` / `list_by_ids`
//! - `get_by_serial` —— 扫码入单解析（Phase P3）需要 exact match
//! - `list_children` —— 装配件子件列表（扫码整套入单需要，Phase P3）

use sqlx::PgExecutor;

use super::model::TPart;

pub struct PartRepo;

impl PartRepo {
    pub async fn get_by_id<'e, E: PgExecutor<'e>>(
        executor: E,
        id: i64,
        include_deleted: bool,
    ) -> Result<Option<TPart>, sqlx::Error> {
        sqlx::query_as!(
            TPart,
            r#"
            SELECT id, serial_no, name, drawing_no, customer_id, assembly_id, status,
                   version, created_at, created_by, updated_at, updated_by, deleted_at,
                   delivery_note_id
            FROM t_part
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
    ) -> Result<Vec<TPart>, sqlx::Error> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        sqlx::query_as!(
            TPart,
            r#"
            SELECT id, serial_no, name, drawing_no, customer_id, assembly_id, status,
                   version, created_at, created_by, updated_at, updated_by, deleted_at,
                   delivery_note_id
            FROM t_part
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
    /// `serial_no` 在 DB 层有 partial unique（`uk_t_part_serial_no`），
    /// 活跃行只可能一条；include_deleted=false 时过滤掉软删件（扫码不应该命中软删）。
    pub async fn get_by_serial<'e, E: PgExecutor<'e>>(
        executor: E,
        serial_no: &str,
        include_deleted: bool,
    ) -> Result<Option<TPart>, sqlx::Error> {
        sqlx::query_as!(
            TPart,
            r#"
            SELECT id, serial_no, name, drawing_no, customer_id, assembly_id, status,
                   version, created_at, created_by, updated_at, updated_by, deleted_at,
                   delivery_note_id
            FROM t_part
            WHERE serial_no = $1
              AND ($2::bool OR deleted_at IS NULL)
            "#,
            serial_no,
            include_deleted,
        )
        .fetch_optional(executor)
        .await
    }

    /// 装配件的子件列表，按 id 升序（Phase P3 扫码整套入单需要）。
    pub async fn list_children<'e, E: PgExecutor<'e>>(
        executor: E,
        assembly_id: i64,
        include_deleted: bool,
    ) -> Result<Vec<TPart>, sqlx::Error> {
        sqlx::query_as!(
            TPart,
            r#"
            SELECT id, serial_no, name, drawing_no, customer_id, assembly_id, status,
                   version, created_at, created_by, updated_at, updated_by, deleted_at,
                   delivery_note_id
            FROM t_part
            WHERE assembly_id = $1
              AND ($2::bool OR deleted_at IS NULL)
            ORDER BY id ASC
            "#,
            assembly_id,
            include_deleted,
        )
        .fetch_all(executor)
        .await
    }
}
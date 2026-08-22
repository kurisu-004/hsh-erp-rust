//! customer 域数据访问
//!
//! 对应 Python myERP/repository/customer.py。函数签名接收 `impl PgExecutor<'_>`，
//! 兼容 `&PgPool` / `&mut PgConnection` / `&mut Transaction`。
//!
//! 约定：
//! - 全部使用 `sqlx::query!` / `query_as!` 编译期宏（需 `DATABASE_URL` 或 `.sqlx/` 离线元数据）
//! - 读查询一律带 `deleted_at IS NULL`（软删）；需要时通过 `include_deleted` 旗标放开
//! - 写查询带 `WHERE id = $1 AND version = $2` 乐观锁，返回 `rows_affected`，0 行由 service 转 409
//!
//! Phase P1（送货分组）只暴露 delivery_note + delivery_group 所需的只读点：
//! - `get_by_id` —— 解析 L1 / L2 客户存在性、parent_id
//! - `list_by_ids` —— 批量解析成员客户名（构造 `DeliveryGroupMemberOut` 用）
//! - `list_children` —— 取 L1 下全部 L2（用于计算 `ungrouped_customers`）

use sqlx::PgExecutor;

use super::model::TCustomer;

pub struct CustomerRepo;

impl CustomerRepo {
    pub async fn get_by_id<'e, E: PgExecutor<'e>>(
        executor: E,
        id: i64,
        include_deleted: bool,
    ) -> Result<Option<TCustomer>, sqlx::Error> {
        sqlx::query_as!(
            TCustomer,
            r#"
            SELECT id, name, parent_id, version,
                   created_at, created_by, updated_at, updated_by, deleted_at,
                   serial_prefix
            FROM t_customer
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
    ) -> Result<Vec<TCustomer>, sqlx::Error> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        sqlx::query_as!(
            TCustomer,
            r#"
            SELECT id, name, parent_id, version,
                   created_at, created_by, updated_at, updated_by, deleted_at,
                   serial_prefix
            FROM t_customer
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

    /// 取 `parent_id = $1` 的全部 L2 子客户（用于 L1 分组列表计算组外 L2）。
    /// 空 parent_id 时返回空 Vec，由 caller 自行短路。
    pub async fn list_children<'e, E: PgExecutor<'e>>(
        executor: E,
        parent_id: i64,
        include_deleted: bool,
    ) -> Result<Vec<TCustomer>, sqlx::Error> {
        sqlx::query_as!(
            TCustomer,
            r#"
            SELECT id, name, parent_id, version,
                   created_at, created_by, updated_at, updated_by, deleted_at,
                   serial_prefix
            FROM t_customer
            WHERE parent_id = $1
              AND ($2::bool OR deleted_at IS NULL)
            ORDER BY id ASC
            "#,
            parent_id,
            include_deleted,
        )
        .fetch_all(executor)
        .await
    }
}
//! process 域数据访问
//!
//! 对应 Python myERP/repository/process_repository.py。函数签名接收 `impl PgExecutor<'_>`，
//! 兼容 `&PgPool` / `&mut PgConnection` / `&mut Transaction`。
//!
//! worker-pool 域新增：
//! - `list_by_ids` —— 工种可用工序白名单展开（输入是 ids 切片，返回完整 TProcess）

use sqlx::PgExecutor;

use super::model::TProcess;

pub struct ProcessRepo;

impl ProcessRepo {
    pub async fn list_by_ids<'e, E: PgExecutor<'e>>(
        executor: E,
        ids: &[i64],
    ) -> Result<Vec<TProcess>, sqlx::Error> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        sqlx::query_as!(
            TProcess,
            r#"
            SELECT id, code, name, version,
                   created_at, created_by, updated_at, updated_by, deleted_at
            FROM t_process
            WHERE id = ANY($1) AND deleted_at IS NULL
            "#,
            ids,
        )
        .fetch_all(executor)
        .await
    }
}

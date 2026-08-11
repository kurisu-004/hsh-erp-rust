//! worker 域数据访问占位
//!
// 对应 Python myERP/repository/worker_repository.py。函数签名接收 `impl PgExecutor<'_>`，
// 兼容 `&PgPool` / `&mut PgConnection` / `&mut Transaction`。

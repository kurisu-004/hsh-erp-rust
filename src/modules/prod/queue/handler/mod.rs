//! prod::queue HTTP handler 汇总 + 路由注册。
//!
//! ## 子文件
//! - `dispatch.rs` —— 下发流 3 端点（`pending` / `dispatch` / `auto-dispatch`）
//! - `recall.rs` —— 召回 1 端点（`recall`）
//! - `pool.rs` —— 队列写端点 3 条（`refill` / `move` / `auto-allocate`）
//! - `board.rs` —— 队列板只读聚合 2 端点（`snapshot` / `processes/{id}`）
//!
//! ## 事务边界
//! 统一在 handler：`state.pool.begin()` → 传 `&mut tx` 给 service → 显式
//! `tx.commit()`；提前 return（`?`）时 `Transaction` 的 Drop 自动回滚。
//! 纯读端点（`pending` / `auto-dispatch` / `snapshot` / `processes/{id}`）走
//! `pool.acquire()` 不开事务。
//! 统一响应信封：`Result<Json<R<T>>, AppError>`。角色守卫在 service 入口
//! （handler 只做权限分发 + 事务 + 广播）。

pub mod board;
pub mod dispatch;
pub mod pool;
pub mod recall;

//! 跨域客户树 helper（2026-10-07 新增）
//!
//! 本模块只放「客户 L1/L2 两层树的 id 展开」这一件事——多个域的列表端点都要把
//! Query 里的单个 `customer_id` 展开成一组 ids 才能进 SQL 的 `= ANY($n)`，
//! 口径必须**全仓一份**，否则同一页面的不同入口会给出不同的行集。
//!
//! [`expand_customer_id`] 只读 `t_customer`，不带任何权限 / 分页 / 软删之外的
//! 口径，故可放在 shared 而非某个域的 service：调用方各自用自己的连接句柄传入，
//! 本函数自身不开事务（**事务边界在 handler**，见 `CLAUDE.md` 领域结构一节）。
//!
//! ## 为什么不做成 repo
//! 各域的客户表访问面差别很大（customer 域是全量 CRUD，`part` 域只在筛选时
//! 读一行 id/parent_id），为「读两列」单独造一个跨域 trait 只会把域隔离变成
//! 一层空壳。共享的粒度取在**函数**而不是**仓储**上：谁需要谁拿连接句柄来调。

use sqlx::PgConnection;

use crate::shared::error::{AppError, code};

/// 展开 `customer_id` 为 `[id]`（含自身 + 子节点）。
///
/// 语义：
/// - L1 客户（无 parent_id）→ 自身 + 全部 L2 子节点 ids
/// - L2 客户（有 parent_id）→ 自身 + 同 L1 下所有兄弟 L2 ids
///
/// 客户不存在（含软删）→ `20102 BIZ_CUSTOMER_NOT_FOUND`：筛选条件指向一个已经
/// 消失的客户时，让调用方拿到空集去查表会把「客户没录进来」伪装成「该客户没有
/// 工单」，前端只能显示一个空列表页，无法提示用户。
///
/// 返回的 ids **不去重、不排序**，由调用方按自己的 SQL 语义消费（`= ANY($n)`
/// 对重复项与顺序均不敏感）。
pub async fn expand_customer_id(conn: &mut PgConnection, cid: i64) -> Result<Vec<i64>, AppError> {
    let row: Option<(i64, Option<i64>)> =
        sqlx::query_as("SELECT id, parent_id FROM t_customer WHERE id = $1 AND deleted_at IS NULL")
            .bind(cid)
            .fetch_optional(&mut *conn)
            .await?;
    let (_id, parent_id) = row.ok_or_else(|| {
        AppError::biz(
            code::BIZ_CUSTOMER_NOT_FOUND,
            format!("customer {cid} 不存在"),
        )
    })?;
    if let Some(p) = parent_id {
        let mut rows: Vec<i64> = sqlx::query_scalar(
            "SELECT id FROM t_customer WHERE parent_id = $1 AND deleted_at IS NULL",
        )
        .bind(p)
        .fetch_all(&mut *conn)
        .await?;
        if !rows.contains(&cid) {
            rows.push(cid);
        }
        Ok(rows)
    } else {
        sqlx::query_scalar(
            "SELECT id FROM t_customer WHERE (parent_id = $1 OR id = $1) AND deleted_at IS NULL",
        )
        .bind(cid)
        .fetch_all(&mut *conn)
        .await
        .map_err(Into::into)
    }
}

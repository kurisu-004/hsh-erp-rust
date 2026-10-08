//! iam 域 `t_wx_identity` SQL 真源（5 方法）
//!
//! 2026-10-10 迁移：本文件从 `modules/wx/repo.rs::WxIdentityRepo` 搬来。搬家的理由是
//! 表归属 —— `t_wx_identity` 存的是「企业微信 userid ↔ 本系统 `t_user.id`」的账号
//! 映射，属 iam 域的数据；wx 域只是它的一个消费方（登录时反查绑定），不该持有该表
//! 的 SQL 真源。搬入后 wx 域对 `t_wx_identity` 零 SQL，只能经
//! `AccountService::resolve_wx_login_user` 开口。
//!
//! 命名按 `repo/mod.rs` 的域内约定（`get_xxx_by_yyy` / `count_xxx_by_*` /
//! `create_xxx` / `soft_delete_xxx`）重写，SQL 与唯一索引语义逐字未改。

use chrono::NaiveDateTime;
use sqlx::PgExecutor;

use crate::modules::iam::repo::model::{WxIdentity, WxIdentityInsert};

// ===========================================================================
// SQL 真源（free fn）
// ===========================================================================

/// 按 `(corp_id, wx_user_id)` 查活跃绑定（wx-login 主路径）。
/// 0 行 → `Ok(None)`，service 层转 `40107 BIZ_WX_NOT_BOUND`。
pub async fn get_wx_identity_by_corp_and_user<'e, E: PgExecutor<'e>>(
    executor: E,
    corp_id: &str,
    wx_user_id: &str,
) -> Result<Option<WxIdentity>, sqlx::Error> {
    sqlx::query_as!(
        WxIdentity,
        r#"
        SELECT id, corp_id, wx_user_id, user_id, version,
               created_at, created_by, updated_at, updated_by, deleted_at
        FROM t_wx_identity
        WHERE corp_id = $1 AND wx_user_id = $2 AND deleted_at IS NULL
        "#,
        corp_id,
        wx_user_id,
    )
    .fetch_optional(executor)
    .await
}

/// 查某系统账号的全部活跃绑定，按 `created_at ASC, id ASC` 排序。
///
/// 排序对「一个账号历史上绑过多个 userid」的存量数据有意义：读端点
/// （`GET /iam/users/{id}/wx-bind`）只取第一行作为当前绑定，解绑端点则全清。
pub async fn get_wx_identity_by_user_id<'e, E: PgExecutor<'e>>(
    executor: E,
    user_id: i64,
) -> Result<Vec<WxIdentity>, sqlx::Error> {
    sqlx::query_as!(
        WxIdentity,
        r#"
        SELECT id, corp_id, wx_user_id, user_id, version,
               created_at, created_by, updated_at, updated_by, deleted_at
        FROM t_wx_identity
        WHERE user_id = $1 AND deleted_at IS NULL
        ORDER BY created_at ASC, id ASC
        "#,
        user_id,
    )
    .fetch_all(executor)
    .await
}

/// 数某系统账号当前有几行活跃绑定（system → wx 方向的一对一检查）。
///
/// ⚠️ 本仓**没有** `uk_wx_identity_user_id` 这样的 partial unique 索引（`t_wx_identity`
/// 只有 `uk_wx_identity_corp_user` 那个 wx → system 方向的索引），故
/// `count` 与随后的 `INSERT` 之间存在 TOCTOU 窗口，靠管理端低并发兜住。详见
/// `docs/api/iam.md` 的「已知偏差登记」。
pub async fn count_active_wx_identities_by_user_id<'e, E: PgExecutor<'e>>(
    executor: E,
    user_id: i64,
) -> Result<i64, sqlx::Error> {
    let row = sqlx::query!(
        r#"
        SELECT count(*) AS "n!"
        FROM t_wx_identity
        WHERE user_id = $1 AND deleted_at IS NULL
        "#,
        user_id,
    )
    .fetch_one(executor)
    .await?;
    Ok(row.n)
}

/// 新增一条绑定。
///
/// 唯一索引 `uk_wx_identity_corp_user`（partial unique，soft-deleted 行不参与）
/// 是并发下的最终防线：应用层的「先查后插」存在 TOCTOU 窗口，撞唯一索引时
/// 由 service 层把 `sqlx::Error::Database(unique_violation)` 翻译成
/// `40108 BIZ_WX_BINDING_DUPLICATE`。
pub async fn create_wx_identity<'e, E: PgExecutor<'e>>(
    executor: E,
    insert: &WxIdentityInsert,
) -> Result<(), sqlx::Error> {
    sqlx::query!(
        r#"
        INSERT INTO t_wx_identity
            (id, corp_id, wx_user_id, user_id, version, created_at, created_by, updated_at, updated_by)
        VALUES ($1, $2, $3, $4, 0, $5, $6, $5, $6)
        "#,
        insert.id,
        insert.corp_id,
        insert.wx_user_id,
        insert.user_id,
        insert.created_at,
        insert.created_by,
    )
    .execute(executor)
    .await?;
    Ok(())
}

/// 软删一条绑定（解绑）。带乐观锁：影响 0 行 = 并发已被改 / 已解绑。
///
/// 返回受影响行数供 service 层判 409（`code::VERSION_CONFLICT`）。
pub async fn soft_delete_wx_identity<'e, E: PgExecutor<'e>>(
    executor: E,
    id: i64,
    version: i32,
    when: NaiveDateTime,
    updated_by: Option<i64>,
) -> Result<u64, sqlx::Error> {
    let res = sqlx::query!(
        r#"
        UPDATE t_wx_identity
        SET deleted_at = $3, updated_at = $3, updated_by = $4, version = version + 1
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

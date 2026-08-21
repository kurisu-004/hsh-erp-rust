//! delivery_note 域数据访问
//!
//! 对应 Python myERP/repository/delivery_note_repository.py。函数签名接收
//! `impl PgExecutor<'_>`，兼容 `&PgPool` / `&mut PgConnection` / `&mut Transaction`。
//!
//! ## Phase P1 范围
//! 本期只暴露「送货分组」相关查询 + 写：
//! - `DeliveryGroupRepo::{list_by_customer, list_members_by_group_ids, get_by_id,
//!    get_by_name, insert, update, soft_delete, insert_member,
//!    soft_delete_members_by_group, list_active_member_by_customer}`
//!
//! 送货单 CRUD（list / find-or-create / submit / recall / pickup 等）留到 P2。
//   TODO(P2): DeliveryNoteRepo + DeliveryNoteEventRepo + DeliveryNoteCounterRepo
//   + part/batch/assembly 跨域只读点。

use chrono::NaiveDateTime;
use sqlx::PgExecutor;

use super::model::{DeliveryGroup, DeliveryGroupMember};

// ---------------------------------------------------------------------------
//  DeliveryGroupRepo
// ---------------------------------------------------------------------------

pub struct DeliveryGroupRepo;

impl DeliveryGroupRepo {
    /// L1 客户的全部活跃分组（按 id ASC）
    pub async fn list_by_customer<'e, E: PgExecutor<'e>>(
        executor: E,
        l1_id: i64,
        include_deleted: bool,
    ) -> Result<Vec<DeliveryGroup>, sqlx::Error> {
        sqlx::query_as!(
            DeliveryGroup,
            r#"
            SELECT id, customer_id, name, version,
                   created_at, created_by, updated_at, updated_by, deleted_at
            FROM t_delivery_group
            WHERE customer_id = $1
              AND ($2::bool OR deleted_at IS NULL)
            ORDER BY id ASC
            "#,
            l1_id,
            include_deleted,
        )
        .fetch_all(executor)
        .await
    }

    /// 批量取一组 group 的全部成员（用于 `list_for_l1` 组装 members 字段）。
    /// 不传 `include_deleted` 旗标：成员随分组走，分组被软删时连带处理。
    pub async fn list_members_by_group_ids<'e, E: PgExecutor<'e>>(
        executor: E,
        group_ids: &[i64],
        include_deleted: bool,
    ) -> Result<Vec<DeliveryGroupMember>, sqlx::Error> {
        if group_ids.is_empty() {
            return Ok(Vec::new());
        }
        sqlx::query_as!(
            DeliveryGroupMember,
            r#"
            SELECT id, group_id, customer_id,
                   created_at, created_by, deleted_at
            FROM t_delivery_group_member
            WHERE group_id = ANY($1)
              AND ($2::bool OR deleted_at IS NULL)
            ORDER BY id ASC
            "#,
            group_ids,
            include_deleted,
        )
        .fetch_all(executor)
        .await
    }

    pub async fn get_by_id<'e, E: PgExecutor<'e>>(
        executor: E,
        id: i64,
        include_deleted: bool,
    ) -> Result<Option<DeliveryGroup>, sqlx::Error> {
        sqlx::query_as!(
            DeliveryGroup,
            r#"
            SELECT id, customer_id, name, version,
                   created_at, created_by, updated_at, updated_by, deleted_at
            FROM t_delivery_group
            WHERE id = $1
              AND ($2::bool OR deleted_at IS NULL)
            "#,
            id,
            include_deleted,
        )
        .fetch_optional(executor)
        .await
    }

    /// 同 L1 下查同名的活跃分组（重名检测 21414）
    pub async fn get_by_name<'e, E: PgExecutor<'e>>(
        executor: E,
        l1_id: i64,
        name: &str,
        include_deleted: bool,
    ) -> Result<Option<DeliveryGroup>, sqlx::Error> {
        sqlx::query_as!(
            DeliveryGroup,
            r#"
            SELECT id, customer_id, name, version,
                   created_at, created_by, updated_at, updated_by, deleted_at
            FROM t_delivery_group
            WHERE customer_id = $1
              AND name = $2
              AND ($3::bool OR deleted_at IS NULL)
            "#,
            l1_id,
            name,
            include_deleted,
        )
        .fetch_optional(executor)
        .await
    }

    /// INSERT。id / 审计字段由 service 用雪花 / `now_naive()` 填好。
    pub async fn insert<'e, E: PgExecutor<'e>>(
        executor: E,
        g: &DeliveryGroup,
    ) -> Result<(), sqlx::Error> {
        sqlx::query!(
            r#"
            INSERT INTO t_delivery_group
                (id, customer_id, name, version,
                 created_at, created_by, updated_at, updated_by)
            VALUES ($1, $2, $3, 0, $4, $5, $4, $5)
            "#,
            g.id,
            g.customer_id,
            g.name,
            g.created_at,
            g.created_by,
        )
        .execute(executor)
        .await?;
        Ok(())
    }

    /// version-checked UPDATE（name + 审计字段）
    pub async fn update<'e, E: PgExecutor<'e>>(
        executor: E,
        id: i64,
        version: i32,
        name: &str,
        when: NaiveDateTime,
        updated_by: Option<i64>,
    ) -> Result<u64, sqlx::Error> {
        let res = sqlx::query!(
            r#"
            UPDATE t_delivery_group
            SET name       = $3,
                version    = version + 1,
                updated_at = $4,
                updated_by = $5
            WHERE id = $1 AND version = $2 AND deleted_at IS NULL
            "#,
            id,
            version,
            name,
            when,
            updated_by,
        )
        .execute(executor)
        .await?;
        Ok(res.rows_affected())
    }

    /// version-checked 软删除
    pub async fn soft_delete<'e, E: PgExecutor<'e>>(
        executor: E,
        id: i64,
        version: i32,
        when: NaiveDateTime,
        deleted_by: Option<i64>,
    ) -> Result<u64, sqlx::Error> {
        let res = sqlx::query!(
            r#"
            UPDATE t_delivery_group
            SET deleted_at = $3,
                version    = version + 1,
                updated_at = $3,
                updated_by = $4
            WHERE id = $1 AND version = $2 AND deleted_at IS NULL
            "#,
            id,
            version,
            when,
            deleted_by,
        )
        .execute(executor)
        .await?;
        Ok(res.rows_affected())
    }

    /// INSERT 成员行。`t_delivery_group_member` 无 version，service 直接 batch insert。
    pub async fn insert_member<'e, E: PgExecutor<'e>>(
        executor: E,
        m: &DeliveryGroupMember,
    ) -> Result<(), sqlx::Error> {
        sqlx::query!(
            r#"
            INSERT INTO t_delivery_group_member
                (id, group_id, customer_id, created_at, created_by)
            VALUES ($1, $2, $3, $4, $5)
            "#,
            m.id,
            m.group_id,
            m.customer_id,
            m.created_at,
            m.created_by,
        )
        .execute(executor)
        .await?;
        Ok(())
    }

    /// 全量替换成员用：软删一个分组下的**所有活跃成员**。返回影响行数。
    pub async fn soft_delete_members_by_group<'e, E: PgExecutor<'e>>(
        executor: E,
        group_id: i64,
        when: NaiveDateTime,
    ) -> Result<u64, sqlx::Error> {
        let res = sqlx::query!(
            r#"
            UPDATE t_delivery_group_member
            SET deleted_at = $2
            WHERE group_id = $1 AND deleted_at IS NULL
            "#,
            group_id,
            when,
        )
        .execute(executor)
        .await?;
        Ok(res.rows_affected())
    }

    /// L2 是否已属于某活跃分组（创建 / 更新时 21415 冲突检测）。
    /// 返回 Option 行（含 group_id），None 表示不在任何活跃分组中。
    pub async fn list_active_member_by_customer<'e, E: PgExecutor<'e>>(
        executor: E,
        l2_customer_id: i64,
    ) -> Result<Option<DeliveryGroupMember>, sqlx::Error> {
        sqlx::query_as!(
            DeliveryGroupMember,
            r#"
            SELECT id, group_id, customer_id,
                   created_at, created_by, deleted_at
            FROM t_delivery_group_member
            WHERE customer_id = $1 AND deleted_at IS NULL
            "#,
            l2_customer_id,
        )
        .fetch_optional(executor)
        .await
    }
}
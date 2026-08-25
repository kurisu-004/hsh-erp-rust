//! 写查询（INSERT / UPDATE / DELETE）。

use chrono::NaiveDateTime;
use sqlx::PgExecutor;

use super::super::model::{DeliveryGroup, DeliveryGroupMember, DeliveryNote, DeliveryNoteEvent};

use super::{DeliveryGroupRepo, DeliveryNoteEventRepo, DeliveryNoteRepo};

// ---------------------------------------------------------------------------
//  DeliveryGroupRepo
// ---------------------------------------------------------------------------

impl DeliveryGroupRepo {
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
}

// ---------------------------------------------------------------------------
//  DeliveryNoteRepo  (P2)
// ---------------------------------------------------------------------------

impl DeliveryNoteRepo {
    /// INSERT。id / 审计字段由 service 用雪花 / `now_naive()` 填好。
    pub async fn create<'e, E: PgExecutor<'e>>(
        executor: E,
        n: &DeliveryNote,
    ) -> Result<(), sqlx::Error> {
        sqlx::query!(
            r#"
            INSERT INTO t_delivery_note
                (id, delivery_note_no, customer_id, status,
                 submitted_at, picked_up_at, submitted_by, picked_up_by,
                 driver_worker_id, note, delivery_date,
                 delivery_group_id, leaf_customer_id,
                 version, created_at, created_by, updated_at, updated_by)
            VALUES ($1, $2, $3, $4,
                    $5, $6, $7, $8,
                    $9, $10, $11,
                    $12, $13,
                    $14, $15, $16, $15, $17)
            "#,
            n.id,
            n.delivery_note_no,
            n.customer_id,
            n.status,
            n.submitted_at,
            n.picked_up_at,
            n.submitted_by,
            n.picked_up_by,
            n.driver_worker_id,
            n.note,
            n.delivery_date,
            n.delivery_group_id,
            n.leaf_customer_id,
            n.version,
            n.created_at,
            n.created_by,
            n.updated_by,
        )
        .execute(executor)
        .await?;
        Ok(())
    }

    /// version-checked UPDATE（caller 修改完字段后整体写回）。
    pub async fn update<'e, E: PgExecutor<'e>>(
        executor: E,
        n: &DeliveryNote,
    ) -> Result<u64, sqlx::Error> {
        let res = sqlx::query!(
            r#"
            UPDATE t_delivery_note
            SET status           = $2,
                submitted_at     = $3,
                picked_up_at     = $4,
                submitted_by     = $5,
                picked_up_by     = $6,
                driver_worker_id = $7,
                note             = $8,
                delivery_date    = $9,
                version          = $10,
                updated_at       = $11,
                updated_by       = $12
            WHERE id = $1 AND version = $13 AND deleted_at IS NULL
            "#,
            n.id,
            n.status,
            n.submitted_at,
            n.picked_up_at,
            n.submitted_by,
            n.picked_up_by,
            n.driver_worker_id,
            n.note,
            n.delivery_date,
            n.version,
            n.updated_at,
            n.updated_by,
            n.version - 1, // previous version for OCC
        )
        .execute(executor)
        .await?;
        Ok(res.rows_affected())
    }

    /// version-checked 软删除。
    pub async fn soft_delete<'e, E: PgExecutor<'e>>(
        executor: E,
        id: i64,
        version: i32,
        when: NaiveDateTime,
        deleted_by: Option<i64>,
    ) -> Result<u64, sqlx::Error> {
        let res = sqlx::query!(
            r#"
            UPDATE t_delivery_note
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
}

// ---------------------------------------------------------------------------
//  DeliveryNoteEventRepo  (P2)
// ---------------------------------------------------------------------------

impl DeliveryNoteEventRepo {
    /// 同步 add（state machine callback 用）；走 `execute` flush inline。
    pub async fn add_event<'e, E: PgExecutor<'e>>(
        executor: E,
        ev: &DeliveryNoteEvent,
    ) -> Result<(), sqlx::Error> {
        sqlx::query!(
            r#"
            INSERT INTO t_delivery_note_event
                (id, delivery_note_id, event_type,
                 from_status, to_status, note, created_by, created_at)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
            "#,
            ev.id,
            ev.delivery_note_id,
            ev.event_type,
            ev.from_status,
            ev.to_status,
            ev.note,
            ev.created_by,
            ev.created_at,
        )
        .execute(executor)
        .await?;
        Ok(())
    }
}
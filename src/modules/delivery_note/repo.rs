//! delivery_note 域数据访问
//!
//! 对应 Python myERP/repository/delivery_note_repository.py。函数签名接收
//! `impl PgExecutor<'_>`，兼容 `&PgPool` / `&mut PgConnection` / `&mut Transaction`。
//!
//! ## Phase 范围
//! - **P1**：送货分组 CRUD（DeliveryGroupRepo）
//! - **P2**：送货单 CRUD + 事件 + 草稿查找（DeliveryNoteRepo / DeliveryNoteEventRepo）

use chrono::NaiveDateTime;
use sqlx::PgExecutor;

use super::model::{DeliveryGroup, DeliveryGroupMember, DeliveryNote, DeliveryNoteEvent};
use crate::modules::delivery_note::model::{DeliveryNoteSortKey, NoteScope};

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

// ---------------------------------------------------------------------------
//  DeliveryNoteRepo  (P2)
// ---------------------------------------------------------------------------

pub struct DeliveryNoteRepo;

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

    /// 按 id 查；`include_deleted=false` 时过滤软删。
    pub async fn get_by_id<'e, E: PgExecutor<'e>>(
        executor: E,
        id: i64,
        include_deleted: bool,
    ) -> Result<Option<DeliveryNote>, sqlx::Error> {
        sqlx::query_as!(
            DeliveryNote,
            r#"
            SELECT id, delivery_note_no, customer_id, status,
                   submitted_at, picked_up_at, submitted_by, picked_up_by,
                   driver_worker_id, note, delivery_date,
                   delivery_group_id, leaf_customer_id,
                   version, created_at, created_by, updated_at, updated_by, deleted_at
            FROM t_delivery_note
            WHERE id = $1
              AND ($2::bool OR deleted_at IS NULL)
            "#,
            id,
            include_deleted,
        )
        .fetch_optional(executor)
        .await
    }

    /// 批量按 id 查；`include_deleted=false` 时过滤软删。
    pub async fn list_by_ids<'e, E: PgExecutor<'e>>(
        executor: E,
        ids: &[i64],
        include_deleted: bool,
    ) -> Result<Vec<DeliveryNote>, sqlx::Error> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        sqlx::query_as!(
            DeliveryNote,
            r#"
            SELECT id, delivery_note_no, customer_id, status,
                   submitted_at, picked_up_at, submitted_by, picked_up_by,
                   driver_worker_id, note, delivery_date,
                   delivery_group_id, leaf_customer_id,
                   version, created_at, created_by, updated_at, updated_by, deleted_at
            FROM t_delivery_note
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

    /// 过滤 + 分页 + 排序（list_with_filters）。keyword 仅在 delivery_note_no 上 ILIKE。
    #[allow(clippy::too_many_arguments)]
    pub async fn list_with_filters<'e, E: PgExecutor<'e>>(
        executor: E,
        statuses: &[&str],
        customer_id: Option<i64>,
        keyword: Option<&str>,
        sort_by: DeliveryNoteSortKey,
        sort_dir: SortDir,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<DeliveryNote>, sqlx::Error> {
        let (status_clause, status_params) = build_status_clause(statuses);
        let kw_pat = keyword.map(|k| format!("%{}%", k.trim()));
        let order_col = match sort_by {
            DeliveryNoteSortKey::CreatedAt => "created_at",
            DeliveryNoteSortKey::SubmittedAt => "submitted_at",
            DeliveryNoteSortKey::PickedUpAt => "picked_up_at",
            DeliveryNoteSortKey::DeliveryNoteNo => "delivery_note_no",
        };
        let order_dir = match sort_dir {
            SortDir::Asc => "ASC",
            SortDir::Desc => "DESC",
        };
        let id_dir = match sort_dir {
            SortDir::Asc => "ASC",
            SortDir::Desc => "DESC",
        };

        // 动态计算 limit / offset 的 placeholder 编号，避免 status 数量变化时的 gap
        let n_status = status_params.len();
        let limit_ph = 3 + n_status;
        let offset_ph = limit_ph + 1;

        let sql = format!(
            r#"
            SELECT id, delivery_note_no, customer_id, status,
                   submitted_at, picked_up_at, submitted_by, picked_up_by,
                   driver_worker_id, note, delivery_date,
                   delivery_group_id, leaf_customer_id,
                   version, created_at, created_by, updated_at, updated_by, deleted_at
            FROM t_delivery_note
            WHERE deleted_at IS NULL
              AND ($1::bigint IS NULL OR customer_id = $1)
              AND ($2::text IS NULL OR delivery_note_no ILIKE $2)
              {status_clause}
            ORDER BY {order_col} {order_dir} NULLS LAST, id {id_dir}
            LIMIT ${limit_ph}::bigint OFFSET ${offset_ph}::bigint
            "#,
        );

        // 绑定顺序：$1=customer_id, $2=kw_pat, $3..$3+N=status, {limit}, {offset}
        let mut q = sqlx::query_as::<_, DeliveryNote>(&sql)
            .bind(customer_id)
            .bind(kw_pat);
        for p in status_params {
            q = q.bind(p);
        }
        q = q.bind(limit).bind(offset);
        q.fetch_all(executor).await
    }

    /// 同 list_with_filters 的 WHERE 子句，但只 SELECT COUNT(*)。
    pub async fn count_with_filters<'e, E: PgExecutor<'e>>(
        executor: E,
        statuses: &[&str],
        customer_id: Option<i64>,
        keyword: Option<&str>,
    ) -> Result<i64, sqlx::Error> {
        let (status_clause, status_params) = build_status_clause(statuses);
        let kw_pat = keyword.map(|k| format!("%{}%", k.trim()));
        let sql = format!(
            r#"
            SELECT COUNT(*)::bigint AS "count!"
            FROM t_delivery_note
            WHERE deleted_at IS NULL
              AND ($1::bigint IS NULL OR customer_id = $1)
              AND ($2::text IS NULL OR delivery_note_no ILIKE $2)
              {status_clause}
            "#,
        );
        let mut q = sqlx::query_scalar::<_, i64>(&sql)
            .bind(customer_id)
            .bind(kw_pat);
        for p in status_params {
            q = q.bind(p);
        }
        q.fetch_one(executor).await
    }

    /// 待司机领取一览：SUBMITTED、非软删，按 `submitted_at DESC` 排。
    pub async fn list_for_pickup<'e, E: PgExecutor<'e>>(
        executor: E,
        customer_id: Option<i64>,
    ) -> Result<Vec<DeliveryNote>, sqlx::Error> {
        sqlx::query_as!(
            DeliveryNote,
            r#"
            SELECT id, delivery_note_no, customer_id, status,
                   submitted_at, picked_up_at, submitted_by, picked_up_by,
                   driver_worker_id, note, delivery_date,
                   delivery_group_id, leaf_customer_id,
                   version, created_at, created_by, updated_at, updated_by, deleted_at
            FROM t_delivery_note
            WHERE deleted_at IS NULL
              AND status = 'SUBMITTED'
              AND ($1::bigint IS NULL OR customer_id = $1)
            ORDER BY submitted_at DESC
            "#,
            customer_id,
        )
        .fetch_all(executor)
        .await
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

    /// 按 (customer_id, scope) 查一张活跃 DRAFT 草稿：
    /// - `NoteScope::Group(gid)`：customer_id + delivery_group_id = gid
    /// - `NoteScope::Leaf(cid)`：customer_id + leaf_customer_id = cid
    /// - `NoteScope::L1Wide`：customer_id + 两列均 NULL → 取最早 (`ORDER BY id ASC LIMIT 1`)
    /// `other_than` 排除指定 id（recall 时排除自己）。
    pub async fn find_open_draft_by_scope<'e, E: PgExecutor<'e>>(
        executor: E,
        l1_id: i64,
        scope: NoteScope,
        other_than: Option<i64>,
    ) -> Result<Option<DeliveryNote>, sqlx::Error> {
        match scope {
            NoteScope::Group(gid) => sqlx::query_as!(
                DeliveryNote,
                r#"
                SELECT id, delivery_note_no, customer_id, status,
                       submitted_at, picked_up_at, submitted_by, picked_up_by,
                       driver_worker_id, note, delivery_date,
                       delivery_group_id, leaf_customer_id,
                       version, created_at, created_by, updated_at, updated_by, deleted_at
                FROM t_delivery_note
                WHERE customer_id        = $1
                  AND delivery_group_id  = $2
                  AND status             = 'DRAFT'
                  AND deleted_at IS NULL
                  AND ($3::bigint IS NULL OR id <> $3)
                ORDER BY id ASC
                LIMIT 1
                "#,
                l1_id,
                gid,
                other_than,
            )
            .fetch_optional(executor)
            .await,
            NoteScope::Leaf(cid) => sqlx::query_as!(
                DeliveryNote,
                r#"
                SELECT id, delivery_note_no, customer_id, status,
                       submitted_at, picked_up_at, submitted_by, picked_up_by,
                       driver_worker_id, note, delivery_date,
                       delivery_group_id, leaf_customer_id,
                       version, created_at, created_by, updated_at, updated_by, deleted_at
                FROM t_delivery_note
                WHERE customer_id       = $1
                  AND leaf_customer_id   = $2
                  AND status            = 'DRAFT'
                  AND deleted_at IS NULL
                  AND ($3::bigint IS NULL OR id <> $3)
                ORDER BY id ASC
                LIMIT 1
                "#,
                l1_id,
                cid,
                other_than,
            )
            .fetch_optional(executor)
            .await,
            NoteScope::L1Wide => sqlx::query_as!(
                DeliveryNote,
                r#"
                SELECT id, delivery_note_no, customer_id, status,
                       submitted_at, picked_up_at, submitted_by, picked_up_by,
                       driver_worker_id, note, delivery_date,
                       delivery_group_id, leaf_customer_id,
                       version, created_at, created_by, updated_at, updated_by, deleted_at
                FROM t_delivery_note
                WHERE customer_id          = $1
                  AND delivery_group_id IS NULL
                  AND leaf_customer_id  IS NULL
                  AND status               = 'DRAFT'
                  AND deleted_at IS NULL
                  AND ($2::bigint IS NULL OR id <> $2)
                ORDER BY id ASC
                LIMIT 1
                "#,
                l1_id,
                other_than,
            )
            .fetch_optional(executor)
            .await,
        }
    }
}

/// 排序方向（与 Python `model.enums::SortDir` 对齐）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortDir {
    Asc,
    Desc,
}

/// 按 `statuses` 切片生成 SQL 过滤子句（含 placeholder + 实际绑定值）。
///
/// 返回 (sql_clause, status_values_for_in_clause).
/// 设计：placeholder **始终** 用 `$3`（list 4、count 用 3）；空状态时
/// 子句为 `AND TRUE`，绑定值 vec 为空——caller 在最后一次性把 status 跳过。
///
/// - 空切片：`AND TRUE`，无 status 绑定
/// - 单元素：`AND status = $3::text`，绑定 1 个 status 值
/// - 多元素：`AND status = ANY($3::text[])`，绑定 N 个 status 值
///
/// caller 必须按 `$1 / $2 / $3..$3+N / $4 / $5`（list）或
/// `$1 / $2 / $3..$3+N`（count）顺序 bind。
fn build_status_clause(statuses: &[&str]) -> (String, Vec<String>) {
    if statuses.is_empty() {
        return ("AND TRUE".to_string(), Vec::new());
    }
    if statuses.len() == 1 {
        return ("AND status = $3::text".to_string(), vec![statuses[0].to_string()]);
    }
    (
        "AND status = ANY($3::text[])".to_string(),
        statuses.iter().map(|s| s.to_string()).collect(),
    )
}

// ---------------------------------------------------------------------------
//  DeliveryNoteEventRepo  (P2)
// ---------------------------------------------------------------------------

pub struct DeliveryNoteEventRepo;

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

    pub async fn list_by_note<'e, E: PgExecutor<'e>>(
        executor: E,
        note_id: i64,
    ) -> Result<Vec<DeliveryNoteEvent>, sqlx::Error> {
        sqlx::query_as!(
            DeliveryNoteEvent,
            r#"
            SELECT id, delivery_note_id, event_type,
                   from_status, to_status, note, created_by, created_at
            FROM t_delivery_note_event
            WHERE delivery_note_id = $1
            ORDER BY created_at ASC, id ASC
            "#,
            note_id,
        )
        .fetch_all(executor)
        .await
    }
}
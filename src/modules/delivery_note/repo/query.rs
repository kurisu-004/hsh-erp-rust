//! 读查询（SELECT）。

use std::collections::HashMap;

use sqlx::PgExecutor;

use super::super::model::{DeliveryGroup, DeliveryGroupMember, DeliveryNote, DeliveryNoteEvent};
use crate::modules::delivery_note::model::{DeliveryNoteSortKey, NoteScope};

use super::{DeliveryGroupRepo, DeliveryNoteEventRepo, DeliveryNoteRepo, SortDir};

// ---------------------------------------------------------------------------
//  DeliveryGroupRepo
// ---------------------------------------------------------------------------

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

    /// 取 L1 客户的所有活跃分组及其成员 id（Phase P3 扫码入单 find-or-create 草稿前用）。
    ///
    /// 返回每组 `(group, member_ids)`；groups 端已按 id ASC；成员按 customer_id ASC（隐含
    /// `GROUP BY` aggregate 选择）。空组（含 0 个成员）依然返回（member_ids 为空 Vec）。
    ///
    /// 该 helper 是设计 §3.2 `classify()` 的数据预加载步骤，**只在 `scan_add` 流程内**用；
    /// `list_for_l1` 走自己的 list_by_customer + list_members_by_group_ids 以支撑
    /// `DeliveryGroupListOut` 输出（后者还需成员姓名）。
    ///
    /// 该方法需要多次复用 executor，签名收 `&mut PgConnection`（与 `split_batch` 同形）。
    pub async fn list_active_groups_with_members_for_l1(
        conn: &mut sqlx::PgConnection,
        l1_id: i64,
    ) -> Result<Vec<(DeliveryGroup, Vec<i64>)>, sqlx::Error> {
        let groups: Vec<DeliveryGroup> = sqlx::query_as!(
            DeliveryGroup,
            r#"
            SELECT id, customer_id, name, version,
                   created_at, created_by, updated_at, updated_by, deleted_at
            FROM t_delivery_group
            WHERE customer_id = $1
              AND deleted_at IS NULL
            ORDER BY id ASC
            "#,
            l1_id,
        )
        .fetch_all(&mut *conn)
        .await?;
        if groups.is_empty() {
            return Ok(Vec::new());
        }
        let group_ids: Vec<i64> = groups.iter().map(|g| g.id).collect();
        let members: Vec<DeliveryGroupMember> = sqlx::query_as!(
            DeliveryGroupMember,
            r#"
            SELECT id, group_id, customer_id,
                   created_at, created_by, deleted_at
            FROM t_delivery_group_member
            WHERE group_id = ANY($1)
              AND deleted_at IS NULL
            ORDER BY group_id ASC, customer_id ASC
            "#,
            &group_ids,
        )
        .fetch_all(&mut *conn)
        .await?;
        let mut by_group: HashMap<i64, Vec<i64>> = HashMap::new();
        for m in members {
            by_group.entry(m.group_id).or_default().push(m.customer_id);
        }
        Ok(groups
            .into_iter()
            .map(|g| {
                let mut ids = by_group.remove(&g.id).unwrap_or_default();
                ids.sort_unstable();
                ids.dedup();
                (g, ids)
            })
            .collect())
    }
}

// ---------------------------------------------------------------------------
//  DeliveryNoteRepo  (P2)
// ---------------------------------------------------------------------------

impl DeliveryNoteRepo {
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
    ///
    /// sqlx 0.9 起 `query_as(&String)` 不再自动通过 `SqlSafeStr` 检查（要求
    /// 字面量 SQL），改用 `QueryBuilder` 把所有动态部分（status 过滤 + ORDER BY）
    /// 安全地 push 进去。所有用户/外部数据走 `push_bind`（自动 bind）。
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

        let mut qb: sqlx::QueryBuilder<sqlx::Postgres> = sqlx::QueryBuilder::new(
            "SELECT id, delivery_note_no, customer_id, status, \
             submitted_at, picked_up_at, submitted_by, picked_up_by, \
             driver_worker_id, note, delivery_date, \
             delivery_group_id, leaf_customer_id, \
             version, created_at, created_by, updated_at, updated_by, deleted_at \
             FROM t_delivery_note \
             WHERE deleted_at IS NULL",
        );
        if let Some(c) = customer_id {
            qb.push(" AND customer_id = ").push_bind(c);
        }
        if let Some(kw) = keyword {
            let pat = format!("%{}%", kw.trim());
            qb.push(" AND delivery_note_no ILIKE ").push_bind(pat);
        }
        push_status_filter(&mut qb, statuses);
        qb.push(format!(
            " ORDER BY {} {} NULLS LAST, id {}",
            order_col, order_dir, order_dir
        ));
        qb.push(" LIMIT ").push_bind(limit);
        qb.push(" OFFSET ").push_bind(offset);

        qb.build_query_as::<DeliveryNote>()
            .fetch_all(executor)
            .await
    }

    /// 同 list_with_filters 的 WHERE 子句，但只 SELECT COUNT(*)。
    pub async fn count_with_filters<'e, E: PgExecutor<'e>>(
        executor: E,
        statuses: &[&str],
        customer_id: Option<i64>,
        keyword: Option<&str>,
    ) -> Result<i64, sqlx::Error> {
        let mut qb: sqlx::QueryBuilder<sqlx::Postgres> = sqlx::QueryBuilder::new(
            "SELECT COUNT(*)::bigint FROM t_delivery_note WHERE deleted_at IS NULL",
        );
        if let Some(c) = customer_id {
            qb.push(" AND customer_id = ").push_bind(c);
        }
        if let Some(kw) = keyword {
            let pat = format!("%{}%", kw.trim());
            qb.push(" AND delivery_note_no ILIKE ").push_bind(pat);
        }
        push_status_filter(&mut qb, statuses);

        qb.build_query_scalar::<i64>().fetch_one(executor).await
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

    /// 按 (customer_id, scope) 查一张活跃 DRAFT 草稿：
    /// - `NoteScope::Group(gid)`：customer_id + delivery_group_id = gid
    /// - `NoteScope::Leaf(cid)`：customer_id + leaf_customer_id = cid
    /// - `NoteScope::L1Wide`：customer_id + 两列均 NULL → 取最早 (`ORDER BY id ASC LIMIT 1`)
    ///
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

// ---------------------------------------------------------------------------
//  DeliveryNoteEventRepo  (P2)
// ---------------------------------------------------------------------------

impl DeliveryNoteEventRepo {
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

/// 向 QueryBuilder 追加 `statuses` 过滤子句。
///
/// - 空切片：什么都不追加
/// - 单元素：`AND status = $N`，绑定该值
/// - 多元素：`AND status = ANY($N::text[])`，绑定 Vec<String>
///
/// 所有值通过 `push_bind` 进入，调用方无需关心 placeholder 编号。
pub(super) fn push_status_filter(qb: &mut sqlx::QueryBuilder<sqlx::Postgres>, statuses: &[&str]) {
    if statuses.is_empty() {
        return;
    }
    if statuses.len() == 1 {
        qb.push(" AND status = ").push_bind(statuses[0].to_string());
    } else {
        let arr: Vec<String> = statuses.iter().map(|s| s.to_string()).collect();
        qb.push(" AND status = ANY(").push_bind(arr).push(")");
    }
}
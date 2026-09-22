//! delivery_note 域 repo 层（SQL 真源 + 胖 trait + PG 实现）
//!
//! ## 结构（2026-09-22 D-5 重构对齐 iam / shelf / customer / part_batch 范本）
//! - `sql.rs`：原 `repo/query.rs` + `repo/mutate.rs` 两文件 SQL 全文搬迁合并，
//!   23 个 pub 固有静态方法 + sqlx `query!` 宏，**内容零 diff**
//!   （`.sqlx/query-*.json` 哈希不变）。ZST struct `DeliveryGroupRepo` /
//!   `DeliveryNoteRepo` / `DeliveryNoteEventRepo` 收 `impl PgExecutor<'_>` 形参。
//! - `mod.rs`（本文件）：对外暴露胖 trait `DeliveryNoteRepoTrait`（23 方法合并单 trait；
//!   `DeliveryGroupRepo` 11 + `DeliveryNoteRepo` 10 + `DeliveryNoteEventRepo` 2），
//!   并直接 `impl DeliveryNoteRepoTrait for &mut PgConnection`——handler/service
//!   借 `&mut *tx` / `&mut *conn` 即可，零中间壳。
//!
//! ## 为什么是胖 trait 而不是按实体拆 3 trait
//! `&mut PgConnection` 同一作用域只能借给一个 repo 实例；service 同时需要
//! `group_repo` + `note_repo` + `event_repo` 时无法表达「同连接三次借用」。胖
//! trait `DeliveryNoteRepoTrait` 是单借位，service 签名 `<R: DeliveryNoteRepoTrait>`
//! 一次收下（by-value；生产 `R = &mut PgConnection`，单测 `R = MockDeliveryNoteRepo`）。
//!
//! ## 为什么 trait 可以直接对 `&mut PgConnection` 实现
//! `Transaction<'_, Postgres>` 与 `PoolConnection<Postgres>` 都 `DerefMut<Target = PgConnection>`，
//! 故 `&mut *tx` / `&mut *conn` 即 `&mut PgConnection`，可直接喂给
//! `sql::DeliveryGroupRepo::yyy` 等。无任何 `PgDeliveryNoteRepo<'a>` 转发壳（与 iam
//! 2026-09-22 删 `PgIamRepo` / part 2026-09-22 D-6 同步）。
//!
//! ## automock
//! `#[cfg_attr(test, mockall::automock)]` 在 trait 上声明，生成
//! `MockDeliveryNoteRepoTrait` 供 `service_tests` 注入。方法签名里的 `<'a>` 显式
//! 生命周期是 mockall 0.15 + async_trait 的硬性要求。
//!
//! ## 错误类型
//! repo trait 方法 → `sqlx::Error`（与 sql.rs 签名 1:1，零翻译）。
//!
//! ## 跨域 helper（暂无）
//! delivery_note service 跨域调用（t_customer / t_part / t_part_batch / t_assembly /
//! t_worker / t_work_type）由 service 直接调对应 ZST 静态方法（`CustomerRepo::xxx` /
//! `PartBatchRepo::xxx` 等），通过 `repo.conn_mut()` 借位即可——本任务**不**下沉跨域
//! SQL 到 trait（与 part 域 D-6 阶段过渡 conn_mut 模式同步）。
//!
//! ## `query.rs` / `mutate.rs` 重导出壳
//! 历史：原 `repo/query.rs` + `repo/mutate.rs` 拆分。本任务把 SQL 全搬到 `sql.rs`，
//! 但保留 `query.rs` / `mutate.rs` 作为重导出壳（注释文件），让 `use ...repo::query::xxx`
//! 或 `...repo::mutate::xxx` 路径仍可解析（避免打破潜在 caller；2026-09-22 shelf /
//! customer / part_batch 范本同形做法）。

use async_trait::async_trait;
use sqlx::PgConnection;

pub mod mutate;
pub mod query;
pub mod sql;

// 重导出 sql.rs 中的 ZST struct 与 model 表行类型，让上层继续用
// `super::repo::{DeliveryGroup, DeliveryNote, DeliveryNoteEvent, DeliveryGroupMember,
//  DeliveryGroupRepo, DeliveryNoteRepo, DeliveryNoteEventRepo}` 这种路径不破。
pub use super::model::{DeliveryGroup, DeliveryGroupMember, DeliveryNote, DeliveryNoteEvent};
pub use sql::{DeliveryGroupRepo, DeliveryNoteEventRepo, DeliveryNoteRepo};

/// 排序方向（与 Python `model.enums::SortDir` 对齐）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortDir {
    Asc,
    Desc,
}

/// delivery_note 域数据访问 trait（23 方法 = t_delivery_group 11 + t_delivery_note 10
/// + t_delivery_note_event 2）。
///
/// 单 trait 而非每实体一个：`&mut PgConnection` 同一作用域只能借给一个 repo 实例，
/// 拆分会让 service 无法同时持有多个 repo（2026-09-22 D-5 重构定案；与 iam / shelf /
/// customer / part_batch / part / assembly 同形）。
///
/// 方法签名 = `sql.rs` 固有静态方法去 executor 形参。`<'a>` 显式生命周期是 mockall
/// 0.15 automock 在 `async_trait` 上下文的硬性要求。
#[cfg_attr(test, mockall::automock)]
#[async_trait]
pub trait DeliveryNoteRepoTrait: Send {
    // ── t_delivery_group 查询（6）──
    async fn group_list_by_customer(
        &mut self,
        l1_id: i64,
        include_deleted: bool,
    ) -> Result<Vec<DeliveryGroup>, sqlx::Error>;
    async fn group_list_members_by_group_ids<'a>(
        &mut self,
        group_ids: &'a [i64],
        include_deleted: bool,
    ) -> Result<Vec<DeliveryGroupMember>, sqlx::Error>;
    async fn group_get_by_id(
        &mut self,
        id: i64,
        include_deleted: bool,
    ) -> Result<Option<DeliveryGroup>, sqlx::Error>;
    async fn group_get_by_name<'a>(
        &mut self,
        l1_id: i64,
        name: &'a str,
        include_deleted: bool,
    ) -> Result<Option<DeliveryGroup>, sqlx::Error>;
    async fn group_list_active_member_by_customer(
        &mut self,
        l2_customer_id: i64,
    ) -> Result<Option<DeliveryGroupMember>, sqlx::Error>;
    /// 一次查询取 L1 全部活跃分组 + 各组成员 id 列表（需要 `&mut PgConnection`）。
    async fn group_list_active_groups_with_members_for_l1(
        &mut self,
        l1_id: i64,
    ) -> Result<Vec<(DeliveryGroup, Vec<i64>)>, sqlx::Error>;

    // ── t_delivery_group 写（5）──
    async fn group_insert(&mut self, g: &DeliveryGroup) -> Result<(), sqlx::Error>;
    async fn group_update(
        &mut self,
        id: i64,
        version: i32,
        name: &str,
        when: chrono::NaiveDateTime,
        updated_by: Option<i64>,
    ) -> Result<u64, sqlx::Error>;
    async fn group_soft_delete(
        &mut self,
        id: i64,
        version: i32,
        when: chrono::NaiveDateTime,
        deleted_by: Option<i64>,
    ) -> Result<u64, sqlx::Error>;
    async fn group_insert_member(
        &mut self,
        m: &DeliveryGroupMember,
    ) -> Result<(), sqlx::Error>;
    async fn group_soft_delete_members_by_group(
        &mut self,
        group_id: i64,
        when: chrono::NaiveDateTime,
    ) -> Result<u64, sqlx::Error>;

    // ── t_delivery_note 查询（5）──
    async fn note_get_by_id(
        &mut self,
        id: i64,
        include_deleted: bool,
    ) -> Result<Option<DeliveryNote>, sqlx::Error>;
    async fn note_list_by_ids<'a>(
        &mut self,
        ids: &'a [i64],
        include_deleted: bool,
    ) -> Result<Vec<DeliveryNote>, sqlx::Error>;
    #[allow(clippy::too_many_arguments)]
    async fn note_list_with_filters<'a>(
        &mut self,
        statuses: &'a [&'a str],
        customer_id: Option<i64>,
        keyword: Option<&'a str>,
        sort_by: super::super::model::DeliveryNoteSortKey,
        sort_dir: SortDir,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<DeliveryNote>, sqlx::Error>;
    async fn note_count_with_filters<'a>(
        &mut self,
        statuses: &'a [&'a str],
        customer_id: Option<i64>,
        keyword: Option<&'a str>,
    ) -> Result<i64, sqlx::Error>;
    async fn note_list_for_pickup(
        &mut self,
        customer_id: Option<i64>,
    ) -> Result<Vec<DeliveryNote>, sqlx::Error>;
    async fn note_find_open_draft_by_scope(
        &mut self,
        l1_id: i64,
        scope: super::super::model::NoteScope,
        other_than: Option<i64>,
    ) -> Result<Option<DeliveryNote>, sqlx::Error>;

    // ── t_delivery_note 写（3）──
    async fn note_create(&mut self, n: &DeliveryNote) -> Result<(), sqlx::Error>;
    async fn note_update(&mut self, n: &DeliveryNote) -> Result<u64, sqlx::Error>;
    async fn note_soft_delete(
        &mut self,
        id: i64,
        version: i32,
        when: chrono::NaiveDateTime,
        deleted_by: Option<i64>,
    ) -> Result<u64, sqlx::Error>;

    // ── t_delivery_note_event（2）──
    async fn event_list_by_note(
        &mut self,
        note_id: i64,
    ) -> Result<Vec<DeliveryNoteEvent>, sqlx::Error>;
    async fn event_add(&mut self, ev: &DeliveryNoteEvent) -> Result<(), sqlx::Error>;
}

/// 把 `DeliveryNoteRepoTrait` 直接对 `&mut PgConnection` 实现——handler/service
/// 借 `&mut *tx` 或 `&mut *conn` 即可调用 `sql::XxxRepo::yyy`，零转发壳（与 iam 2026-09-22
/// 删 `PgIamRepo` 同步）。
///
/// trait 方法收 `&mut self`，impl 在 `&mut PgConnection` 上时
/// `self: &mut &mut PgConnection`，两次 deref 才得到 `PgConnection`：
/// `*self: &mut PgConnection`，`**self: PgConnection`，故喂给 `sql::XxxRepo::yyy`
/// 须写 `&mut **self`（reborrow，避免 move 引用本身）。
#[async_trait]
impl DeliveryNoteRepoTrait for &mut PgConnection {
    // ── t_delivery_group 查询（6）── 一行委托 sql::DeliveryGroupRepo ───────
    async fn group_list_by_customer(
        &mut self,
        l1_id: i64,
        include_deleted: bool,
    ) -> Result<Vec<DeliveryGroup>, sqlx::Error> {
        DeliveryGroupRepo::list_by_customer(&mut **self, l1_id, include_deleted).await
    }

    async fn group_list_members_by_group_ids<'b>(
        &mut self,
        group_ids: &'b [i64],
        include_deleted: bool,
    ) -> Result<Vec<DeliveryGroupMember>, sqlx::Error> {
        DeliveryGroupRepo::list_members_by_group_ids(&mut **self, group_ids, include_deleted).await
    }

    async fn group_get_by_id(
        &mut self,
        id: i64,
        include_deleted: bool,
    ) -> Result<Option<DeliveryGroup>, sqlx::Error> {
        DeliveryGroupRepo::get_by_id(&mut **self, id, include_deleted).await
    }

    async fn group_get_by_name<'b>(
        &mut self,
        l1_id: i64,
        name: &'b str,
        include_deleted: bool,
    ) -> Result<Option<DeliveryGroup>, sqlx::Error> {
        DeliveryGroupRepo::get_by_name(&mut **self, l1_id, name, include_deleted).await
    }

    async fn group_list_active_member_by_customer(
        &mut self,
        l2_customer_id: i64,
    ) -> Result<Option<DeliveryGroupMember>, sqlx::Error> {
        DeliveryGroupRepo::list_active_member_by_customer(&mut **self, l2_customer_id).await
    }

    async fn group_list_active_groups_with_members_for_l1(
        &mut self,
        l1_id: i64,
    ) -> Result<Vec<(DeliveryGroup, Vec<i64>)>, sqlx::Error> {
        DeliveryGroupRepo::list_active_groups_with_members_for_l1(&mut **self, l1_id).await
    }

    // ── t_delivery_group 写（5）── 一行委托 sql::DeliveryGroupRepo ─────────
    async fn group_insert(&mut self, g: &DeliveryGroup) -> Result<(), sqlx::Error> {
        DeliveryGroupRepo::insert(&mut **self, g).await
    }

    async fn group_update(
        &mut self,
        id: i64,
        version: i32,
        name: &str,
        when: chrono::NaiveDateTime,
        updated_by: Option<i64>,
    ) -> Result<u64, sqlx::Error> {
        DeliveryGroupRepo::update(&mut **self, id, version, name, when, updated_by).await
    }

    async fn group_soft_delete(
        &mut self,
        id: i64,
        version: i32,
        when: chrono::NaiveDateTime,
        deleted_by: Option<i64>,
    ) -> Result<u64, sqlx::Error> {
        DeliveryGroupRepo::soft_delete(&mut **self, id, version, when, deleted_by).await
    }

    async fn group_insert_member(
        &mut self,
        m: &DeliveryGroupMember,
    ) -> Result<(), sqlx::Error> {
        DeliveryGroupRepo::insert_member(&mut **self, m).await
    }

    async fn group_soft_delete_members_by_group(
        &mut self,
        group_id: i64,
        when: chrono::NaiveDateTime,
    ) -> Result<u64, sqlx::Error> {
        DeliveryGroupRepo::soft_delete_members_by_group(&mut **self, group_id, when).await
    }

    // ── t_delivery_note 查询（5+1）── 一行委托 sql::DeliveryNoteRepo ─────────
    async fn note_get_by_id(
        &mut self,
        id: i64,
        include_deleted: bool,
    ) -> Result<Option<DeliveryNote>, sqlx::Error> {
        DeliveryNoteRepo::get_by_id(&mut **self, id, include_deleted).await
    }

    async fn note_list_by_ids<'b>(
        &mut self,
        ids: &'b [i64],
        include_deleted: bool,
    ) -> Result<Vec<DeliveryNote>, sqlx::Error> {
        DeliveryNoteRepo::list_by_ids(&mut **self, ids, include_deleted).await
    }

    #[allow(clippy::too_many_arguments)]
    async fn note_list_with_filters<'b>(
        &mut self,
        statuses: &'b [&'b str],
        customer_id: Option<i64>,
        keyword: Option<&'b str>,
        sort_by: super::super::model::DeliveryNoteSortKey,
        sort_dir: SortDir,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<DeliveryNote>, sqlx::Error> {
        DeliveryNoteRepo::list_with_filters(
            &mut **self,
            statuses,
            customer_id,
            keyword,
            sort_by,
            sort_dir,
            limit,
            offset,
        )
        .await
    }

    async fn note_count_with_filters<'b>(
        &mut self,
        statuses: &'b [&'b str],
        customer_id: Option<i64>,
        keyword: Option<&'b str>,
    ) -> Result<i64, sqlx::Error> {
        DeliveryNoteRepo::count_with_filters(&mut **self, statuses, customer_id, keyword).await
    }

    async fn note_list_for_pickup(
        &mut self,
        customer_id: Option<i64>,
    ) -> Result<Vec<DeliveryNote>, sqlx::Error> {
        DeliveryNoteRepo::list_for_pickup(&mut **self, customer_id).await
    }

    async fn note_find_open_draft_by_scope(
        &mut self,
        l1_id: i64,
        scope: super::super::model::NoteScope,
        other_than: Option<i64>,
    ) -> Result<Option<DeliveryNote>, sqlx::Error> {
        DeliveryNoteRepo::find_open_draft_by_scope(&mut **self, l1_id, scope, other_than).await
    }

    // ── t_delivery_note 写（3）── 一行委托 sql::DeliveryNoteRepo ─────────────
    async fn note_create(&mut self, n: &DeliveryNote) -> Result<(), sqlx::Error> {
        DeliveryNoteRepo::create(&mut **self, n).await
    }

    async fn note_update(&mut self, n: &DeliveryNote) -> Result<u64, sqlx::Error> {
        DeliveryNoteRepo::update(&mut **self, n).await
    }

    async fn note_soft_delete(
        &mut self,
        id: i64,
        version: i32,
        when: chrono::NaiveDateTime,
        deleted_by: Option<i64>,
    ) -> Result<u64, sqlx::Error> {
        DeliveryNoteRepo::soft_delete(&mut **self, id, version, when, deleted_by).await
    }

    // ── t_delivery_note_event（2）── 一行委托 sql::DeliveryNoteEventRepo ─────
    async fn event_list_by_note(
        &mut self,
        note_id: i64,
    ) -> Result<Vec<DeliveryNoteEvent>, sqlx::Error> {
        DeliveryNoteEventRepo::list_by_note(&mut **self, note_id).await
    }

    async fn event_add(&mut self, ev: &DeliveryNoteEvent) -> Result<(), sqlx::Error> {
        DeliveryNoteEventRepo::add_event(&mut **self, ev).await
    }
}
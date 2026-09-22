//! part 域 repo 层（SQL 真源 + 胖 trait + PG 实现）
//!
//! ## 结构（2026-09-22 D-6 重构对齐 iam / shelf / customer / part_batch / worker_pool 范本）
//! - `sql.rs`：原 `repo/part.rs` + `repo/batch.rs` + `repo/event.rs` 三文件 SQL 全文
//!   搬迁合并，37 个 pub 固有静态方法 + sqlx `query!` 宏，**内容零 diff**
//!   （`.sqlx/query-*.json` 哈希不变）。ZST struct `PartRepo` 收 `impl PgExecutor<'_>` 形参。
//! - `mod.rs`（本文件）：对外暴露胖 trait `PartRepoTrait`（37 方法合并单 trait；
//!   t_part 16 + t_part_batch 17 + t_part_event 1 + 跨域 helper 3），并直接
//!   `impl PartRepoTrait for &mut PgConnection`——handler/service 借 `&mut *tx` /
//!   `&mut *conn` 即可，零中间壳。
//!
//! ## 为什么 trait 命名为 `PartRepoTrait`（带 `Trait` 后缀）
//! 跨模块静态调用方（delivery_note / assembly / outsource / part_file / statistics /
// shelf 6 域，prod::worker_pool 1 域）继续走 `PartRepo::xxx(&mut *conn, ...)`
//! ZST 静态方法——保持 12 处静态调用零修改（本任务**不能**破坏 `part::repo::PartRepo`
//! 作为 ZST 的对外身份），故 trait 改名 `PartRepoTrait`（与 shelf / customer / part_batch
//! 范本同形）：
//!
//! - `part::repo::PartRepo` —— ZST struct（在 `sql.rs` 内，通过 `pub use sql::PartRepo;`
//!   重新导出至本模块），保留 37 个静态方法签名不变（cross-module 调用方零修改）。
//! - `part::repo::PartRepoTrait` —— 本文件新加的胖 trait（37 方法合并单 trait），part 域
//!   内部 service 用 `<R: PartRepoTrait>` 收。trait 方法数 = SQL 静态方法数（1:1 对应）。
//!
//! ## 为什么是胖 trait 而非按表拆 3 trait
//! `&mut PgConnection` 同一作用域只能借给一个 repo 实例；service 同时需要
//! `get_part_detail`（t_part）+ `find_batch_by_id`（t_part_batch）+ `insert_part_event`
//! （t_part_event）时无法表达「同连接三次借用」。胖 trait 是单借位，service 签名
//! `<R: PartRepoTrait>(&self, mut repo: R, ...)` 一次收下（by-value；生产 `R = &mut
//! PgConnection`，单测 `R = MockPartRepo`）。
//!
//! ## 跨域 helper（3）—— 下沉到 PartRepoTrait
//! service 跨域调用（CustomerRepo::lookup_names / ProcessChainRepo::xxx /
//! PartBatchRepo::xxx / PartFileRepo::xxx / WorkerPoolService::refill_*）下沉到
//! `PartRepoTrait` helper 方法，trait impl 一行委托到对应域的 ZST 静态方法。这样
//! service 仍只需一个 `repo: R: PartRepoTrait` 参数，避免多 trait 借连接的限制。
//!
//! - `customer_lookup_names(cid)` —— 委托 `CustomerRepo::lookup_names`
//! - `part_batch_has_active_on_delivery_note(part_id)` —— 委托 `PartBatchRepo::has_active_batch_on_delivery_note`
//! - `part_batch_list_active_by_part_id(part_id)` —— 委托 `PartBatchRepo::list_active_by_part_id`
//!
//! 注：原 `enrich_part_list_with_location_and_holder` 跨域 helper（t_shelf / t_worker /
//! t_outsource_company 三表解析 holder 名）暂保留 service 内调用形态——下沉到 trait 会
//! 让 trait 膨胀且与 D-6 范围不符，留待后续 D-7/D-8 处理。
//!
//! ## 为什么 trait 可以直接对 `&mut PgConnection` 实现
//! `Transaction<'_, Postgres>` 与 `PoolConnection<Postgres>` 都 `DerefMut<Target = PgConnection>`，
//! 故 `&mut *tx` / `&mut *conn` 即 `&mut PgConnection`，可直接喂给 `sql::PartRepo::yyy`。
//! 无需任何 `PgPartRepo<'a>` 转发壳（与 iam 2026-09-22 删 `PgIamRepo` 同步）。
//!
//! ## automock
//! `#[cfg_attr(test, mockall::automock)]` 在 trait 上声明，生成 `MockPartRepoTrait` 供
//! service 单测注入。part 域当前无内联 mod tests（service 全部走 `tests/part_*_api.rs`
//! + `tests/worker_scan_api.rs` 等集成测试守护），故未建 `part/service_tests/` 目录——
//!   按 conventions.md §4.1 含 IO 不强求 100%。
//!
//! ## 错误类型
//! repo trait 方法 → `sqlx::Error`（与 `sql.rs` 签名 1:1，零翻译）。
//!
//! ## 已知架构债（D-6 阶段过渡）
//!
//! `conn_mut()` 暴露 `&mut PgConnection` 让 service 拿连接做 inline SQL / 跨域 repo
//! 静态调用；这是 D-6 阶段过渡 API，因 part 域 50+ 端点 + 跨 5 域 inline SQL 太多，
//! 统一 trait 形参成本过高。
//!
//! 147 处散点 `repo.conn_mut()` 调用是技术债；D-7/D-8 计划逐方法下沉到 trait helper：
//! - 首批候选：`enrich_part_list_with_location_and_holder`（M1 已下沉到
//!   `service/list_enrichment.rs`）+ `sync_from_batch_change`
//! - Phase1 inline query（inspection/repair 跨域 join）后续逐项下沉
//!
//! forward-compat 目标：最终 `conn_mut()` 调用 < 10 处（仅保留必要的极复杂 inline SQL）。

use async_trait::async_trait;
use sqlx::PgConnection;

pub mod batch;
pub mod event;
pub mod part;
pub mod sql;

// 重导出 sql.rs 中的 ZST struct / builder / row 与 model 表行类型，让上层继续用
// `super::repo::{TPart, TPartInspected, TPartEvent, NewPartEvent, PartUpdate, PartListFilters,
//  NewPartCreate, ChildInheritFields, PartRepo}` 这种路径不破（cross-module 调用方都依赖这条路径）。
pub use crate::modules::part::model::{
    NewPartEvent, TPart, TPartEvent, TPartInspected, TPartRollupState,
};
pub use sql::{
    scale_qty, ChildInheritFields, NewPartCreate, PartListFilters, PartRepo, PartUpdate,
};

/// part 域数据访问 trait（37 方法 = t_part 16 + t_part_batch 17 + t_part_event 1 + 跨域 helper 3）。
///
/// 单 trait 而非按表拆 3 trait：`&mut PgConnection` 同一作用域只能借给一个 repo 实例，
/// 拆分会让 service 无法同时持有三个 repo（2026-09-22 D-6 重构定案；与 iam / shelf /
/// customer / part_batch / worker_pool 同形）。
///
/// 方法签名 = `sql.rs` 固有静态方法去 executor 形参。`<'a` 显式生命周期是 mockall
/// 0.15 automock 在 `async_trait` 上下文的硬性要求。
#[cfg_attr(test, mockall::automock)]
#[async_trait]
pub trait PartRepoTrait: Send {
    /// 取底层 `&mut PgConnection`：service 内 inline sqlx / 跨域 repo 静态调用 / 内部
    /// 工具方法（如 `_validate_inspection_shelf`、`sync_from_batch_change`）需
    /// 要再次借用连接。
    ///
    /// 2026-09-22 D-7：service 签名简化为 `<R: PartRepoTrait>(mut repo: R, ...)`，
    /// 移除冗余的 `conn: &mut PgConnection` 形参（handler 无法同时给出两个
    /// `&mut *tx` 借位）；内层需要连接时统一走 `repo.conn_mut()`。
    fn conn_mut(&mut self) -> &mut PgConnection;

    // ── t_part 查询（5）──
    async fn get_by_id<'a>(
        &mut self,
        id: i64,
        include_deleted: bool,
    ) -> Result<Option<TPart>, sqlx::Error>;
    async fn list_by_ids<'a>(
        &mut self,
        ids: &'a [i64],
        include_deleted: bool,
    ) -> Result<Vec<TPart>, sqlx::Error>;
    async fn get_by_serial<'a>(
        &mut self,
        serial_no: &'a str,
        include_deleted: bool,
    ) -> Result<Option<TPart>, sqlx::Error>;
    async fn list_children<'a>(
        &mut self,
        assembly_id: i64,
        include_deleted: bool,
    ) -> Result<Vec<TPart>, sqlx::Error>;
    async fn get_part_inspected(
        &mut self,
        part_id: i64,
    ) -> Result<Option<TPartInspected>, sqlx::Error>;

    // ── t_part CRUD（6）──
    async fn get_part_detail(
        &mut self,
        part_id: i64,
    ) -> Result<Option<TPart>, sqlx::Error>;
    async fn create_part<'a>(&mut self, new: NewPartCreate<'a>) -> Result<i64, sqlx::Error>;
    async fn update_part<'a>(
        &mut self,
        part_id: i64,
        expected_version: i32,
        upd: PartUpdate<'a>,
    ) -> Result<u64, sqlx::Error>;
    async fn soft_delete_part(
        &mut self,
        part_id: i64,
        expected_version: i32,
        current_user_id: i64,
    ) -> Result<u64, sqlx::Error>;
    async fn list_with_filters<'a>(
        &mut self,
        f: &PartListFilters<'a>,
    ) -> Result<Vec<TPart>, sqlx::Error>;
    async fn count_with_filters<'a>(
        &mut self,
        f: &PartListFilters<'a>,
    ) -> Result<i64, sqlx::Error>;

    // ── t_part assembly 子件（4）──
    async fn list_by_assembly_id(
        &mut self,
        assembly_id: i64,
        include_deleted: bool,
    ) -> Result<Vec<TPart>, sqlx::Error>;
    #[allow(clippy::too_many_arguments)]
    async fn insert_child_for_assembly<'a, 'b, 'c, 'd, 'e, 'f>(
        &mut self,
        id: i64,
        customer_id: i64,
        assembly_id: i64,
        serial_no: &'a str,
        name: &'b str,
        drawing_no: Option<&'c str>,
        quantity: i32,
        planned_delivery_date: Option<chrono::NaiveDate>,
        inherit: ChildInheritFields<'f>,
        current_user_id: i64,
        initial_batch_id: i64,
    ) -> Result<(), sqlx::Error>;
    #[allow(clippy::too_many_arguments)]
    async fn cascade_sync_from_assembly<'a, 'b, 'c, 'd, 'e, 'f, 'g>(
        &mut self,
        assembly_id: i64,
        request_date: chrono::NaiveDate,
        applicant_name: &'a str,
        order_no: Option<&'b str>,
        system_delivery_date: Option<chrono::NaiveDate>,
        planned_delivery_date: chrono::NaiveDate,
        is_urgent: bool,
        note: Option<&'c str>,
        customer_id: i64,
        updated_by: i64,
    ) -> Result<u64, sqlx::Error>;
    async fn scale_children_quantity(
        &mut self,
        assembly_id: i64,
        old_qty: i32,
        new_qty: i32,
        updated_by: i64,
    ) -> Result<u64, sqlx::Error>;

    // ── t_part rollup（3）──
    async fn get_part_rollup_state(
        &mut self,
        part_id: i64,
    ) -> Result<Option<TPartRollupState>, sqlx::Error>;
    async fn update_part_rollup<'a>(
        &mut self,
        part_id: i64,
        status: &'a str,
        next_process_id: Option<i64>,
        updated_by: i64,
    ) -> Result<u64, sqlx::Error>;
    async fn clear_part_serial_no_when_completed(
        &mut self,
        part_id: i64,
        updated_by: i64,
    ) -> Result<u64, sqlx::Error>;

    // ── t_part_batch 查询（4）──
    async fn find_inprocess_batch_for_part(
        &mut self,
        part_id: i64,
        expected_batch_id: Option<i64>,
    ) -> Result<Option<crate::modules::part_batch::model::TPartBatch>, sqlx::Error>;
    async fn find_scan_target_batch(
        &mut self,
        part_id: i64,
        expected_batch_id: Option<i64>,
    ) -> Result<Option<crate::modules::part_batch::model::TPartBatch>, sqlx::Error>;
    async fn find_inspection_batch_for_fail(
        &mut self,
        part_id: i64,
        expected_batch_id: Option<i64>,
    ) -> Result<Option<crate::modules::part_batch::model::TPartBatch>, sqlx::Error>;
    async fn find_current_inspection_batch_id(
        &mut self,
        part_id: i64,
    ) -> Result<Option<i64>, sqlx::Error>;
    async fn find_batch_by_id(
        &mut self,
        batch_id: i64,
    ) -> Result<Option<crate::modules::part_batch::model::TPartBatch>, sqlx::Error>;
    async fn find_inprocess_batch_by_id_and_holder(
        &mut self,
        batch_id: i64,
        holder_id: i64,
    ) -> Result<Option<crate::modules::part_batch::model::TPartBatch>, sqlx::Error>;
    async fn find_worker_held_batch_for_part(
        &mut self,
        part_id: i64,
        worker_id: i64,
        expected_batch_id: Option<i64>,
    ) -> Result<Option<crate::modules::part_batch::model::TPartBatch>, sqlx::Error>;

    // ── t_part_batch mark_*（6）──
    async fn mark_batch_passed_inspection(
        &mut self,
        batch_id: i64,
        expected_version: i32,
        current_user_id: Option<i64>,
    ) -> Result<u64, sqlx::Error>;
    async fn mark_batch_inspected(
        &mut self,
        batch_id: i64,
        expected_version: i32,
        shelf_id: i64,
        current_user_id: Option<i64>,
    ) -> Result<u64, sqlx::Error>;
    async fn mark_batch_failed_inspection(
        &mut self,
        batch_id: i64,
        expected_version: i32,
        shelf_id: i64,
        current_process_step_id: Option<i64>,
        current_user_id: Option<i64>,
    ) -> Result<u64, sqlx::Error>;
    async fn mark_batch_returned(
        &mut self,
        batch_id: i64,
        expected_version: i32,
        shelf_id: i64,
        current_process_step_id: Option<i64>,
        current_user_id: Option<i64>,
    ) -> Result<u64, sqlx::Error>;

    // ── t_part_batch lifecycle（5）──
    async fn mark_batch_delivered(
        &mut self,
        batch_id: i64,
        expected_version: i32,
        current_user_id: i64,
    ) -> Result<u64, sqlx::Error>;
    async fn mark_batch_completed(
        &mut self,
        batch_id: i64,
        expected_version: i32,
        current_user_id: i64,
    ) -> Result<u64, sqlx::Error>;
    async fn mark_batch_cancelled(
        &mut self,
        batch_id: i64,
        expected_version: i32,
        current_user_id: i64,
    ) -> Result<u64, sqlx::Error>;
    async fn mark_part_cancelled(
        &mut self,
        part_id: i64,
        expected_version: i32,
        current_user_id: i64,
    ) -> Result<u64, sqlx::Error>;
    async fn mark_batch_repairing(
        &mut self,
        batch_id: i64,
        expected_version: i32,
        current_user_id: i64,
    ) -> Result<u64, sqlx::Error>;
    async fn cancel_all_active_batches_for_part(
        &mut self,
        part_id: i64,
        current_user_id: i64,
    ) -> Result<u64, sqlx::Error>;

    // ── t_part_batch split（1）──
    #[allow(clippy::too_many_arguments)]
    async fn split_batch_for_partial_pass<'a>(
        &mut self,
        new_batch_id: i64,
        src_batch_id: i64,
        src_version: i32,
        part_id: i64,
        split_quantity: i32,
        new_batch_status: &'a str,
        current_user_id: Option<i64>,
    ) -> Result<i64, sqlx::Error>;

    // ── t_part_event 事件日志（1）──
    async fn insert_part_event<'a>(
        &mut self,
        e: NewPartEvent<'a>,
    ) -> Result<(), sqlx::Error>;

    // ── 跨域 helper（2）── 委托 part_batch 静态方法，避免 service 收第二个 conn
    /// part 任一活跃批次是否已挂送货单（委托 `PartBatchRepo::has_active_batch_on_delivery_note`）。
    async fn part_batch_has_active_on_delivery_note(
        &mut self,
        part_id: i64,
    ) -> Result<bool, sqlx::Error>;
    /// 列 part 全部活跃批次（委托 `PartBatchRepo::list_active_by_part_id`，用于 rollup）。
    async fn part_batch_list_active_by_part_id(
        &mut self,
        part_id: i64,
    ) -> Result<Vec<crate::modules::part_batch::model::TPartBatch>, sqlx::Error>;
}

/// 把 `PartRepoTrait` 直接对 `&mut PgConnection` 实现——handler/service 借 `&mut *tx` 或
/// `&mut *conn` 即可调用 `sql::PartRepo::yyy`，零转发壳（与 iam 2026-09-22 删 `PgIamRepo`
/// 同步）。
///
/// trait 方法收 `&mut self`，impl 在 `&mut PgConnection` 上时 `self: &mut &mut PgConnection`，
/// 两次 deref 才得到 `PgConnection`：`*self: &mut PgConnection`，`**self: PgConnection`，
/// 故喂给 `sql::PartRepo::yyy` 须写 `&mut **self`（reborrow，避免 move 引用本身）。
#[async_trait]
impl PartRepoTrait for &mut PgConnection {
    // ── 连接获取（2026-09-22 D-7 新增）──
    fn conn_mut(&mut self) -> &mut PgConnection {
        // self: &mut &mut PgConnection，函数形参已 reborrow 一次，故直接 `self`
        // 即可借到内层 `&mut PgConnection`（auto-deref 处理第二层）
        self
    }

    // ── t_part 查询（5）── 一行委托 sql::PartRepo ────────────────────
    async fn get_by_id<'a>(
        &mut self,
        id: i64,
        include_deleted: bool,
    ) -> Result<Option<TPart>, sqlx::Error> {
        PartRepo::get_by_id(&mut **self, id, include_deleted).await
    }

    async fn list_by_ids<'a>(
        &mut self,
        ids: &'a [i64],
        include_deleted: bool,
    ) -> Result<Vec<TPart>, sqlx::Error> {
        PartRepo::list_by_ids(&mut **self, ids, include_deleted).await
    }

    async fn get_by_serial<'a>(
        &mut self,
        serial_no: &'a str,
        include_deleted: bool,
    ) -> Result<Option<TPart>, sqlx::Error> {
        PartRepo::get_by_serial(&mut **self, serial_no, include_deleted).await
    }

    async fn list_children<'a>(
        &mut self,
        assembly_id: i64,
        include_deleted: bool,
    ) -> Result<Vec<TPart>, sqlx::Error> {
        PartRepo::list_children(&mut **self, assembly_id, include_deleted).await
    }

    async fn get_part_inspected(
        &mut self,
        part_id: i64,
    ) -> Result<Option<TPartInspected>, sqlx::Error> {
        PartRepo::get_part_inspected(&mut **self, part_id).await
    }

    // ── t_part CRUD（6）── 一行委托 sql::PartRepo ────────────────────
    async fn get_part_detail(
        &mut self,
        part_id: i64,
    ) -> Result<Option<TPart>, sqlx::Error> {
        PartRepo::get_part_detail(&mut **self, part_id).await
    }

    async fn create_part<'b>(
        &mut self,
        new: NewPartCreate<'b>,
    ) -> Result<i64, sqlx::Error> {
        PartRepo::create_part(&mut **self, new).await
    }

    async fn update_part<'b>(
        &mut self,
        part_id: i64,
        expected_version: i32,
        upd: PartUpdate<'b>,
    ) -> Result<u64, sqlx::Error> {
        PartRepo::update_part(&mut **self, part_id, expected_version, upd).await
    }

    async fn soft_delete_part(
        &mut self,
        part_id: i64,
        expected_version: i32,
        current_user_id: i64,
    ) -> Result<u64, sqlx::Error> {
        PartRepo::soft_delete_part(&mut **self, part_id, expected_version, current_user_id).await
    }

    async fn list_with_filters<'b>(
        &mut self,
        f: &PartListFilters<'b>,
    ) -> Result<Vec<TPart>, sqlx::Error> {
        PartRepo::list_with_filters(&mut **self, f).await
    }

    async fn count_with_filters<'b>(
        &mut self,
        f: &PartListFilters<'b>,
    ) -> Result<i64, sqlx::Error> {
        PartRepo::count_with_filters(&mut **self, f).await
    }

    // ── t_part assembly 子件（4）──
    async fn list_by_assembly_id(
        &mut self,
        assembly_id: i64,
        include_deleted: bool,
    ) -> Result<Vec<TPart>, sqlx::Error> {
        PartRepo::list_by_assembly_id(&mut **self, assembly_id, include_deleted).await
    }

    #[allow(clippy::too_many_arguments)]
    async fn insert_child_for_assembly<'a, 'b, 'c, 'd, 'e, 'f>(
        &mut self,
        id: i64,
        customer_id: i64,
        assembly_id: i64,
        serial_no: &'a str,
        name: &'b str,
        drawing_no: Option<&'c str>,
        quantity: i32,
        planned_delivery_date: Option<chrono::NaiveDate>,
        inherit: ChildInheritFields<'f>,
        current_user_id: i64,
        initial_batch_id: i64,
    ) -> Result<(), sqlx::Error> {
        PartRepo::insert_child_for_assembly(
            &mut **self,
            id,
            customer_id,
            assembly_id,
            serial_no,
            name,
            drawing_no,
            quantity,
            planned_delivery_date,
            inherit,
            current_user_id,
            initial_batch_id,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn cascade_sync_from_assembly<'a, 'b, 'c, 'd, 'e, 'f, 'g>(
        &mut self,
        assembly_id: i64,
        request_date: chrono::NaiveDate,
        applicant_name: &'a str,
        order_no: Option<&'b str>,
        system_delivery_date: Option<chrono::NaiveDate>,
        planned_delivery_date: chrono::NaiveDate,
        is_urgent: bool,
        note: Option<&'c str>,
        customer_id: i64,
        updated_by: i64,
    ) -> Result<u64, sqlx::Error> {
        PartRepo::cascade_sync_from_assembly(
            &mut **self,
            assembly_id,
            request_date,
            applicant_name,
            order_no,
            system_delivery_date,
            planned_delivery_date,
            is_urgent,
            note,
            customer_id,
            updated_by,
        )
        .await
    }

    async fn scale_children_quantity(
        &mut self,
        assembly_id: i64,
        old_qty: i32,
        new_qty: i32,
        updated_by: i64,
    ) -> Result<u64, sqlx::Error> {
        PartRepo::scale_children_quantity(&mut **self, assembly_id, old_qty, new_qty, updated_by)
            .await
    }

    // ── t_part rollup（3）──
    async fn get_part_rollup_state(
        &mut self,
        part_id: i64,
    ) -> Result<Option<TPartRollupState>, sqlx::Error> {
        PartRepo::get_part_rollup_state(&mut **self, part_id).await
    }

    async fn update_part_rollup<'a>(
        &mut self,
        part_id: i64,
        status: &'a str,
        next_process_id: Option<i64>,
        updated_by: i64,
    ) -> Result<u64, sqlx::Error> {
        PartRepo::update_part_rollup(&mut **self, part_id, status, next_process_id, updated_by)
            .await
    }

    async fn clear_part_serial_no_when_completed(
        &mut self,
        part_id: i64,
        updated_by: i64,
    ) -> Result<u64, sqlx::Error> {
        PartRepo::clear_part_serial_no_when_completed(&mut **self, part_id, updated_by).await
    }

    // ── t_part_batch 查询（7）──
    async fn find_inprocess_batch_for_part(
        &mut self,
        part_id: i64,
        expected_batch_id: Option<i64>,
    ) -> Result<Option<crate::modules::part_batch::model::TPartBatch>, sqlx::Error> {
        PartRepo::find_inprocess_batch_for_part(&mut **self, part_id, expected_batch_id).await
    }

    async fn find_scan_target_batch(
        &mut self,
        part_id: i64,
        expected_batch_id: Option<i64>,
    ) -> Result<Option<crate::modules::part_batch::model::TPartBatch>, sqlx::Error> {
        PartRepo::find_scan_target_batch(&mut **self, part_id, expected_batch_id).await
    }

    async fn find_inspection_batch_for_fail(
        &mut self,
        part_id: i64,
        expected_batch_id: Option<i64>,
    ) -> Result<Option<crate::modules::part_batch::model::TPartBatch>, sqlx::Error> {
        PartRepo::find_inspection_batch_for_fail(&mut **self, part_id, expected_batch_id).await
    }

    async fn find_current_inspection_batch_id(
        &mut self,
        part_id: i64,
    ) -> Result<Option<i64>, sqlx::Error> {
        PartRepo::find_current_inspection_batch_id(&mut **self, part_id).await
    }

    async fn find_batch_by_id(
        &mut self,
        batch_id: i64,
    ) -> Result<Option<crate::modules::part_batch::model::TPartBatch>, sqlx::Error> {
        PartRepo::find_batch_by_id(&mut **self, batch_id).await
    }

    async fn find_inprocess_batch_by_id_and_holder(
        &mut self,
        batch_id: i64,
        holder_id: i64,
    ) -> Result<Option<crate::modules::part_batch::model::TPartBatch>, sqlx::Error> {
        PartRepo::find_inprocess_batch_by_id_and_holder(&mut **self, batch_id, holder_id).await
    }

    async fn find_worker_held_batch_for_part(
        &mut self,
        part_id: i64,
        worker_id: i64,
        expected_batch_id: Option<i64>,
    ) -> Result<Option<crate::modules::part_batch::model::TPartBatch>, sqlx::Error> {
        PartRepo::find_worker_held_batch_for_part(&mut **self, part_id, worker_id, expected_batch_id)
            .await
    }

    // ── t_part_batch mark_*（4）──
    async fn mark_batch_passed_inspection(
        &mut self,
        batch_id: i64,
        expected_version: i32,
        current_user_id: Option<i64>,
    ) -> Result<u64, sqlx::Error> {
        PartRepo::mark_batch_passed_inspection(&mut **self, batch_id, expected_version, current_user_id)
            .await
    }

    async fn mark_batch_inspected(
        &mut self,
        batch_id: i64,
        expected_version: i32,
        shelf_id: i64,
        current_user_id: Option<i64>,
    ) -> Result<u64, sqlx::Error> {
        PartRepo::mark_batch_inspected(&mut **self, batch_id, expected_version, shelf_id, current_user_id)
            .await
    }

    async fn mark_batch_failed_inspection(
        &mut self,
        batch_id: i64,
        expected_version: i32,
        shelf_id: i64,
        current_process_step_id: Option<i64>,
        current_user_id: Option<i64>,
    ) -> Result<u64, sqlx::Error> {
        PartRepo::mark_batch_failed_inspection(
            &mut **self,
            batch_id,
            expected_version,
            shelf_id,
            current_process_step_id,
            current_user_id,
        )
        .await
    }

    async fn mark_batch_returned(
        &mut self,
        batch_id: i64,
        expected_version: i32,
        shelf_id: i64,
        current_process_step_id: Option<i64>,
        current_user_id: Option<i64>,
    ) -> Result<u64, sqlx::Error> {
        PartRepo::mark_batch_returned(
            &mut **self,
            batch_id,
            expected_version,
            shelf_id,
            current_process_step_id,
            current_user_id,
        )
        .await
    }

    // ── t_part_batch lifecycle（6）──
    async fn mark_batch_delivered(
        &mut self,
        batch_id: i64,
        expected_version: i32,
        current_user_id: i64,
    ) -> Result<u64, sqlx::Error> {
        PartRepo::mark_batch_delivered(&mut **self, batch_id, expected_version, current_user_id)
            .await
    }

    async fn mark_batch_completed(
        &mut self,
        batch_id: i64,
        expected_version: i32,
        current_user_id: i64,
    ) -> Result<u64, sqlx::Error> {
        PartRepo::mark_batch_completed(&mut **self, batch_id, expected_version, current_user_id)
            .await
    }

    async fn mark_batch_cancelled(
        &mut self,
        batch_id: i64,
        expected_version: i32,
        current_user_id: i64,
    ) -> Result<u64, sqlx::Error> {
        PartRepo::mark_batch_cancelled(&mut **self, batch_id, expected_version, current_user_id)
            .await
    }

    async fn mark_part_cancelled(
        &mut self,
        part_id: i64,
        expected_version: i32,
        current_user_id: i64,
    ) -> Result<u64, sqlx::Error> {
        PartRepo::mark_part_cancelled(&mut **self, part_id, expected_version, current_user_id)
            .await
    }

    async fn mark_batch_repairing(
        &mut self,
        batch_id: i64,
        expected_version: i32,
        current_user_id: i64,
    ) -> Result<u64, sqlx::Error> {
        PartRepo::mark_batch_repairing(&mut **self, batch_id, expected_version, current_user_id)
            .await
    }

    async fn cancel_all_active_batches_for_part(
        &mut self,
        part_id: i64,
        current_user_id: i64,
    ) -> Result<u64, sqlx::Error> {
        PartRepo::cancel_all_active_batches_for_part(&mut **self, part_id, current_user_id).await
    }

    // ── t_part_batch split（1）──
    #[allow(clippy::too_many_arguments)]
    async fn split_batch_for_partial_pass<'a>(
        &mut self,
        new_batch_id: i64,
        src_batch_id: i64,
        src_version: i32,
        part_id: i64,
        split_quantity: i32,
        new_batch_status: &'a str,
        current_user_id: Option<i64>,
    ) -> Result<i64, sqlx::Error> {
        PartRepo::split_batch_for_partial_pass(
            &mut **self,
            new_batch_id,
            src_batch_id,
            src_version,
            part_id,
            split_quantity,
            new_batch_status,
            current_user_id,
        )
        .await
    }

    // ── t_part_event 事件日志（1）──
    async fn insert_part_event<'b>(
        &mut self,
        e: NewPartEvent<'b>,
    ) -> Result<(), sqlx::Error> {
        PartRepo::insert_part_event(&mut **self, e).await
    }

    // ── 跨域 helper（2）── 委托 part_batch 静态方法 ──────────
    async fn part_batch_has_active_on_delivery_note(
        &mut self,
        part_id: i64,
    ) -> Result<bool, sqlx::Error> {
        use crate::modules::part_batch::repo::PartBatchRepo;
        PartBatchRepo::has_active_batch_on_delivery_note(&mut **self, part_id).await
    }

    async fn part_batch_list_active_by_part_id(
        &mut self,
        part_id: i64,
    ) -> Result<Vec<crate::modules::part_batch::model::TPartBatch>, sqlx::Error> {
        use crate::modules::part_batch::repo::PartBatchRepo;
        PartBatchRepo::list_active_by_part_id(&mut **self, part_id).await
    }
}
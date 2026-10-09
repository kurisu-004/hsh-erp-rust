//! part 域 repo 层（SQL 真源 + 胖 trait + PG 实现）
//!
//! ## 结构（2026-09-22 D-6 重构对齐 iam / shelf / customer / part_batch / queue 范本）
//! - `sql/`（原 `sql.rs`，已按表拆 `part_sql.rs` / `event_sql.rs` /
//!   `helper_sql.rs` + `mod.rs`）：SQL 全文，
//!   25 个 pub 固有静态方法（t_part 24 + t_part_event 1）
//!   + sqlx `query!` 宏。ZST struct `PartRepo` 收 `impl PgExecutor<'_>` 形参。
//! - `mod.rs`（本文件）：对外暴露胖 trait `PartRepoTrait`（44 方法合并单 trait，
//!   口径见下文「方法计数口径」），并直接 `impl PartRepoTrait for &mut PgConnection`
//!   ——handler/service 借 `&mut *tx` / `&mut *conn` 即可，零中间壳。
//!
//! ## 方法计数口径
//! 「trait 方法数」= `pub trait PartRepoTrait` 花括号内声明的 `fn` 签名条数；
//! `conn_mut` 这类工具方法计入，`#[allow(...)]` / doc 注释不计；`#[async_trait]`
//! 展开出的生命周期形参不算独立方法。44 = `conn_mut` 1 + t_part 18 +
//! t_part_batch 17 + t_part_event 1 + 跨域 helper 2 +
//! 采购订单 Excel 匹配 + 订单信息回填 5：
//!
//! - `conn_mut` 1 —— 工具方法，暴露 `&mut PgConnection`
//! - t_part 18 —— 查询 5 + CRUD 6 + assembly 子件 4 + rollup 3（一行委托 `sql::PartRepo`）
//! - t_part_batch 17 —— 查询 6 + mark_* 品检 4 + lifecycle 6 + split 1（一行委托
//!   `prod::batch::repo::PartBatchRepo` / `shared::batch::status`）
//! - t_part_event 1 —— `insert_part_event`（委托 `sql::PartRepo`）
//! - 跨域 helper 2 —— `part_batch_has_active_on_delivery_note`（委托 `PartBatchRepo`）
//!   + `serial_prefix_for_customer`（2026-10-05 新增，查 `t_customer`，委托
//!     `sql::PartRepo`；建单派发序列号的前置）
//! - 采购订单 Excel 匹配 + 订单信息回填 5（2026-10-06 新增）—— 委托 `sql::PartRepo`：
//!   `list_match_parts_by_keys` / `list_match_assemblies_by_keys` /
//!   `list_assembly_names_by_ids` / `list_children_by_assemblies` /
//!   `update_order_info`。上 trait 的**唯一动机是可 mock**：旧实现走
//!   `conn_mut()` 内联 sqlx，匹配分档只能靠集成测试守护。
//!
//! ⚠️ trait 方法数 **不等于** `sql/` 静态方法数（25）：`t_part_batch` 段（17）与跨域 helper
//! 中的一项转发到 `prod::batch` 域的 ZST 静态方法，`conn_mut` 则无对应 SQL 方法。
//! 2026-10-06 起 `sql::PartRepo` 的**全部**静态方法都已上 trait（此前
//! `list_children_by_assemblies` 是唯一漏项，由匹配链路补上）。
//!
//! ## 为什么 trait 命名为 `PartRepoTrait`（带 `Trait` 后缀）
//! 跨模块静态调用方（2026-10-03 实测 7 域 10 文件：assembly 6 / delivery_note 5 /
//! prod::batch 5 / com::union_list 3 / admin 2 / prod::queue 2 /
//! prod::process_chain 1）继续走 `PartRepo::xxx(&mut *conn, ...)` ZST 静态方法——保持
//! 24 处静态调用零修改（口径：`src/` 下剔除注释行后的 `PartRepo::` 出现次数，排除
//! part 域自身；本任务**不能**破坏 `part::repo::PartRepo` 作为 ZST 的对外身份），
//! 故 trait 改名 `PartRepoTrait`（与 shelf / customer / part_batch 范本同形）：
//!
//! - `part::repo::PartRepo` —— ZST struct（在 `sql/mod.rs` 内，通过 `pub use sql::PartRepo;`
//!   重新导出至本模块），保留 25 个静态方法签名不变（cross-module 调用方零修改）。
//! - `part::repo::PartRepoTrait` —— 本文件的胖 trait（44 方法合并单 trait），part 域
//!   内部 service 用 `<R: PartRepoTrait>` 收。
//!
//! ## 为什么是胖 trait 而非按表拆 3 trait
//! `&mut PgConnection` 同一作用域只能借给一个 repo 实例；service 同时需要
//! `get_part_detail`（t_part）+ `find_batch_by_id`（t_part_batch）+ `insert_part_event`
//! （t_part_event）时无法表达「同连接三次借用」。胖 trait 是单借位，service 签名
//! `<R: PartRepoTrait>(&self, mut repo: R, ...)` 一次收下（by-value；生产 `R = &mut
//! PgConnection`，单测 `R = MockPartRepoTrait`）。
//!
//! ## 跨域 helper（2）—— 下沉到 PartRepoTrait
//! 设计意图：service 跨域读别的域时，除 `repo: R: PartRepoTrait` 外还得再借一次连接，
//! 而 `&mut PgConnection` 同一作用域只能借给一个 repo 实例。故 D-6 起计划把这类调用
//! （历史上候选涉及 Customer / ProcessChain / PartBatch / PartFile / WorkerPool 等域）
//! 下沉成 `PartRepoTrait` helper、impl 一行委托到对应域 ZST 静态方法，service 就只需
//! 一个 `repo` 参数。**实际落地 2 个**：
//!
//! - `part_batch_has_active_on_delivery_note(part_id)` —— 委托 `PartBatchRepo::has_active_batch_on_delivery_note`
//! - `serial_prefix_for_customer(customer_id)` —— 查 `t_customer`（2026-10-05 新增，
//!   建单派发序列号的前置；SQL 在 `sql::PartRepo`）
//!
//! 注：2026-10-03 订正——原文把上述历史候选与实际 trait 方法并列为「跨域 helper 清单」，
//! 读起来像都已落地。其中 `customer_lookup_names` 从未落地；
//! `part_batch_list_active_by_part_id` 曾落地但零调用方，已与
//! `find_inprocess_batch_by_id_and_holder`、`mark_batch_cancelled` 一并删除。
//! 原 `enrich_part_list_with_location_and_holder`（t_shelf / t_worker /
//! t_outsource_company 三表解析 holder 名）保留 service 内调用形态——下沉到 trait 会让
//! trait 膨胀。
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
//! 44 个方法里 34 个返回 `sqlx::Error`、9 个返回 `AppError`
//! （`t_part_batch.status` 写点 8 个 —— 契约是「没写成 = `VERSION_CONFLICT`」，
//! 转 `sqlx::Error` 会把 409 降级成 500；`serial_prefix_for_customer` 1 个 ——
//! 20308 / 20104 / 20102 三个业务码要原样透出）、`conn_mut` 无返回值。
//! 按委托目标分三处——**都是零翻译**，但 1:1 的对象不同：
//! - 25 个 1:1 委托 `sql::PartRepo`（t_part 18 + t_part_event 1 + 跨域 helper 1
//!   + 采购订单 Excel 匹配 5 = 恰好等于 `sql/` 静态方法数 25；其中 24 个返回
//!     `sqlx::Error`，只有 `serial_prefix_for_customer` 返回 `AppError`）
//! - 18 个 1:1 委托 `prod::batch::PartBatchRepo`（t_part_batch 段 17 + 跨域 helper 1
//!   = 10 个返回 `sqlx::Error` + 8 个 status 写点返回 `AppError`）
//! - `conn_mut` 1 个不委托任何域，只交出连接
//!
//! ## 已知架构债（D-6 阶段过渡）
//!
//! `conn_mut()` 暴露 `&mut PgConnection` 让 service 拿连接做 inline SQL / 跨域 repo
//! 静态调用；这是 D-6 阶段过渡 API，因 part 域 50+ 端点 + 跨 5 域 inline SQL 太多，
//! 统一 trait 形参成本过高。
//!
//! 2026-10-03 实测 `.conn_mut()` 真实调用行 198 处（口径：`src/` 下剔除注释行后含
//! `.conn_mut()` 的代码行，分布 25 个文件；其中 part 域 61 处，`tests/` 0 处），是技术债；
//! D-7/D-8 计划逐方法下沉到 trait helper：
//! - 首批候选：`enrich_part_list_with_location_and_holder`（M1 已下沉到
//!   `service/list_enrichment.rs`）+ `sync_from_batch_change`
//! - Phase1 inline query（inspection/repair 跨域 join）后续逐项下沉
//!
//! forward-compat 目标：最终 `conn_mut()` 调用 < 10 处（仅保留必要的极复杂 inline SQL）。

use async_trait::async_trait;
use sqlx::PgConnection;

use crate::modules::prod::batch::repo::PartBatchRepo;
use crate::shared::batch::status as batch_status;
use crate::shared::error::AppError;

pub mod batch;
pub mod event;
pub mod part;
pub mod sql;
// 2026-10-01 review 第 1 轮 M8：`shared::batch::status` 已从 `repo/` 移到
// `src/shared/batch/status.rs` —— 它承载的是「写批次 + 派生
// part/assembly + 终态序列号释放」这套**业务策略**（终态判定、归档/释放、
// 跨域调用 `AssemblyService`），按 CLAUDE.md 六件套分层属 service 层职责；
// repo 层只留纯 SQL。

// 重导出 sql.rs 中的 ZST struct / builder / row 与 model 表行类型，让上层继续用
// `super::repo::{TPart, TPartInspected, TPartEvent, NewPartEvent, PartUpdate, PartListFilters,
//  NewPartCreate, ChildInheritFields, PartRepo}` 这种路径不破（cross-module 调用方都依赖这条路径）。
pub use crate::modules::part::model::{
    NewPartEvent, TPart, TPartEvent, TPartInspected, TPartRollupState,
};
pub use sql::{
    AssemblyMatchRow, ChildInheritFields, NewPartCreate, PartListFilters, PartRepo, PartUpdate,
    scale_qty,
};

/// part 域数据访问 trait（44 方法 = `conn_mut` 1 + t_part 18 + t_part_batch 17 +
/// t_part_event 1 + 跨域 helper 2 + 采购订单 Excel 匹配 5；
/// 计数口径见模块头「方法计数口径」小节）。
///
/// 单 trait 而非按表拆 3 trait：`&mut PgConnection` 同一作用域只能借给一个 repo 实例，
/// 拆分会让 service 无法同时持有三个 repo（2026-09-22 D-6 重构定案；与 iam / shelf /
/// customer / part_batch / queue 同形）。
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
    async fn get_part_detail(&mut self, part_id: i64) -> Result<Option<TPart>, sqlx::Error>;
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
    async fn count_with_filters<'a>(&mut self, f: &PartListFilters<'a>)
    -> Result<i64, sqlx::Error>;

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
        unit_price: Option<rust_decimal::Decimal>,
        total_price: Option<rust_decimal::Decimal>,
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

    // ── t_part_batch 查询（6）──
    async fn find_inprocess_batch_for_part(
        &mut self,
        part_id: i64,
        expected_batch_id: Option<i64>,
    ) -> Result<Option<crate::shared::batch::TPartBatch>, sqlx::Error>;
    async fn find_scan_target_batch(
        &mut self,
        part_id: i64,
        expected_batch_id: Option<i64>,
    ) -> Result<Option<crate::shared::batch::TPartBatch>, sqlx::Error>;
    /// 2026-10-02：原 `find_inspection_batch_for_fail(part_id, Option<batch_id>)`
    /// 改为 `find_inspection_batch_by_id(batch_id)` —— `batch_id` 是 URL 路径
    /// 参数（必填），`part_id` 由 service 从批次行反查。
    async fn find_inspection_batch_by_id(
        &mut self,
        batch_id: i64,
    ) -> Result<Option<crate::shared::batch::TPartBatch>, sqlx::Error>;
    async fn find_current_inspection_batch_id(
        &mut self,
        part_id: i64,
    ) -> Result<Option<i64>, sqlx::Error>;
    async fn find_batch_by_id(
        &mut self,
        batch_id: i64,
    ) -> Result<Option<crate::shared::batch::TPartBatch>, sqlx::Error>;
    async fn find_worker_held_batch_for_part(
        &mut self,
        part_id: i64,
        worker_id: i64,
        expected_batch_id: Option<i64>,
    ) -> Result<Option<crate::shared::batch::TPartBatch>, sqlx::Error>;

    // ── t_part_batch mark_*（4）──
    //
    // 2026-10-01：除 `mark_batch_returned`（不改 status）外，全部
    // `t_part_batch.status` 写点已收口到 `shared::batch::status`。
    // 返回类型由 `sqlx::Error` 改 `AppError`：shared::batch::status 的契约是
    // 「没写成 = `VERSION_CONFLICT`」，转 `sqlx::Error` 会把 409 降级成 500。
    //
    // 2026-10-01 追加：下面 3 个方法返回 `batch_status::RollupOutcome` 而非
    // `u64`。shared::batch::status 在同一个函数里已经做完 part 派生 + assembly 反向
    // 同步，而这三个方法的调用点要拿 `SyncOutcome` 填响应里的
    // `synced_assembly_id`（并据此发 `ASSEMBLY_UPDATED` 广播）。若让 service
    // 再补调一次 `PartService::sync_from_batch_change`，第二次派生必然
    // `NoChange`，那个字段就会被**恒为 null** 吞掉（2026-10-01 修掉的真实回归）。
    async fn mark_batch_passed_inspection(
        &mut self,
        batch_id: i64,
        expected_version: i32,
        current_user_id: Option<i64>,
    ) -> Result<batch_status::RollupOutcome, AppError>;
    async fn mark_batch_inspected(
        &mut self,
        batch_id: i64,
        expected_version: i32,
        shelf_id: i64,
        current_user_id: Option<i64>,
    ) -> Result<batch_status::RollupOutcome, AppError>;
    async fn mark_batch_failed_inspection(
        &mut self,
        batch_id: i64,
        expected_version: i32,
        shelf_id: i64,
        current_process_step_id: Option<i64>,
        current_process_id: Option<i64>,
        current_user_id: Option<i64>,
    ) -> Result<batch_status::RollupOutcome, AppError>;
    async fn mark_batch_returned(
        &mut self,
        batch_id: i64,
        expected_version: i32,
        shelf_id: i64,
        current_process_step_id: Option<i64>,
        advance_to_process_id: Option<i64>,
        current_user_id: Option<i64>,
    ) -> Result<u64, sqlx::Error>;

    // ── t_part_batch lifecycle（6）──
    async fn mark_batch_delivered(
        &mut self,
        batch_id: i64,
        expected_version: i32,
        current_user_id: i64,
    ) -> Result<u64, AppError>;
    /// `event_id`（2026-10-01 review 第 1 轮 M4）：本方法能让 part 新进
    /// COMPLETED（最后一条批次完成时），故 caller 必须传真实雪花 id 供终态序列号
    /// 归档事件（`SERIAL_RELEASED`）使用。
    async fn mark_batch_completed(
        &mut self,
        batch_id: i64,
        expected_version: i32,
        current_user_id: i64,
        event_id: Option<i64>,
    ) -> Result<u64, AppError>;
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
    ) -> Result<u64, AppError>;
    async fn cancel_all_active_batches_for_part(
        &mut self,
        part_id: i64,
        current_user_id: i64,
    ) -> Result<u64, AppError>;
    // 2026-09-30 新增：force-complete 端点 — 单 SQL 强推 part 下所有非
    // CANCELLED 活跃批次到 COMPLETED（绕状态机 + 不走 OCC）。
    //
    // `event_id`（2026-10-01 review 第 1 轮 M4）：本方法**必定**让 part 派生进
    // COMPLETED，故 caller 必须传真实雪花 id 供终态序列号归档事件使用。
    async fn force_complete_all_batches_for_part(
        &mut self,
        part_id: i64,
        current_user_id: i64,
        event_id: Option<i64>,
    ) -> Result<u64, AppError>;

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
    async fn insert_part_event<'a>(&mut self, e: NewPartEvent<'a>) -> Result<(), sqlx::Error>;

    // ── 跨域 helper（2）── 委托其它域 / 其它表，避免 service 收第二个 conn
    /// part 任一活跃批次是否已挂送货单（委托 `PartBatchRepo::has_active_batch_on_delivery_note`）。
    async fn part_batch_has_active_on_delivery_note(
        &mut self,
        part_id: i64,
    ) -> Result<bool, sqlx::Error>;
    /// 2026-10-05 新增：取 L1 客户的 `serial_prefix` 首字符（派发序列号用）。
    /// 入参 L1 / L2 均可（内部折回 L1）；失败语义见 `sql::PartRepo` 同名方法。
    async fn serial_prefix_for_customer(&mut self, customer_id: i64) -> Result<char, AppError>;

    // ── 采购订单 Excel 匹配 + 订单信息回填（2026-10-06 新增，5）──
    //
    // 这 5 个方法存在的理由是**可 mock**：`POST /parts/match-by-excel-items` 的
    // 旧实现走 `repo.conn_mut()` 内联 sqlx，service 单测无法注入假数据，只能退化成
    // 集成测试。分档决策抽成纯函数后，service 只依赖下面这几个方法 ⇒ 分档判定
    // 可以用 `MockPartRepoTrait` 覆盖。
    //
    // **查询数硬约束**：整条匹配链路最多 4 条查询（见 service 侧
    // `phase1::excel_match::collect_match_index`），与请求行数无关。
    /// 按「图号命中 ∪ 名称命中」捞回 `t_part` 候选（软删闸门在 SQL 内）。
    async fn list_match_parts_by_keys<'a>(
        &mut self,
        drawing_nos: &'a [&'a str],
        names: &'a [&'a str],
    ) -> Result<Vec<TPart>, sqlx::Error>;
    /// 同上，查 `t_assembly`（装配件命中后取其子件作为候选）。
    async fn list_match_assemblies_by_keys<'a>(
        &mut self,
        drawing_nos: &'a [&'a str],
        names: &'a [&'a str],
    ) -> Result<Vec<AssemblyMatchRow>, sqlx::Error>;
    /// 候选零件「所属装配件」名称映射（`assembly_id` → `name`）。
    async fn list_assembly_names_by_ids<'a>(
        &mut self,
        assembly_ids: &'a [i64],
    ) -> Result<Vec<AssemblyMatchRow>, sqlx::Error>;
    /// 一批装配件的全部有效子件（2026-10-06 由 `sql::PartRepo` 的同名静态方法
    /// 上 trait，供匹配链路取装配件候选；其余调用方仍走静态方法）。
    async fn list_children_by_assemblies<'a>(
        &mut self,
        assembly_ids: &'a [i64],
        include_deleted: bool,
    ) -> Result<Vec<TPart>, sqlx::Error>;
    /// 订单信息三态窄写（`order_no` / `system_delivery_date` / `note`），只服务
    /// `POST /parts/batch-update-order-info`；**不复用** `update_part` +
    /// `PartUpdate`：本方法三列**恒三态**（batch 端要区分「缺省 / 清空 / 设值」，
    /// 日期列还要逐行解析、把非法文本降级为行级失败），而 `update_part` 是「通用
    /// 表单全量提交」语义 —— 且 2026-10-10 起 `PartUpdate` 的同名列也已改成三态，
    /// 两条路径的入参形态与失败语义不再重合。逐列语义与完整理由见
    /// `sql::PartRepo::update_order_info`。
    async fn update_order_info<'a>(
        &mut self,
        part_id: i64,
        expected_version: i32,
        order_no: Option<Option<&'a str>>,
        system_delivery_date: Option<Option<chrono::NaiveDate>>,
        note: Option<Option<&'a str>>,
        updated_by: i64,
    ) -> Result<u64, sqlx::Error>;
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
    async fn get_part_detail(&mut self, part_id: i64) -> Result<Option<TPart>, sqlx::Error> {
        PartRepo::get_part_detail(&mut **self, part_id).await
    }

    async fn create_part<'b>(&mut self, new: NewPartCreate<'b>) -> Result<i64, sqlx::Error> {
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
        unit_price: Option<rust_decimal::Decimal>,
        total_price: Option<rust_decimal::Decimal>,
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
            unit_price,
            total_price,
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

    // ── t_part_batch 查询（6）──
    async fn find_inprocess_batch_for_part(
        &mut self,
        part_id: i64,
        expected_batch_id: Option<i64>,
    ) -> Result<Option<crate::shared::batch::TPartBatch>, sqlx::Error> {
        PartBatchRepo::find_inprocess_batch_for_part(&mut **self, part_id, expected_batch_id).await
    }

    async fn find_scan_target_batch(
        &mut self,
        part_id: i64,
        expected_batch_id: Option<i64>,
    ) -> Result<Option<crate::shared::batch::TPartBatch>, sqlx::Error> {
        PartBatchRepo::find_scan_target_batch(&mut **self, part_id, expected_batch_id).await
    }

    async fn find_inspection_batch_by_id(
        &mut self,
        batch_id: i64,
    ) -> Result<Option<crate::shared::batch::TPartBatch>, sqlx::Error> {
        PartBatchRepo::find_inspection_batch_by_id(&mut **self, batch_id).await
    }

    async fn find_current_inspection_batch_id(
        &mut self,
        part_id: i64,
    ) -> Result<Option<i64>, sqlx::Error> {
        PartBatchRepo::find_current_inspection_batch_id(&mut **self, part_id).await
    }

    async fn find_batch_by_id(
        &mut self,
        batch_id: i64,
    ) -> Result<Option<crate::shared::batch::TPartBatch>, sqlx::Error> {
        PartBatchRepo::find_batch_by_id(&mut **self, batch_id).await
    }

    async fn find_worker_held_batch_for_part(
        &mut self,
        part_id: i64,
        worker_id: i64,
        expected_batch_id: Option<i64>,
    ) -> Result<Option<crate::shared::batch::TPartBatch>, sqlx::Error> {
        PartBatchRepo::find_worker_held_batch_for_part(
            &mut **self,
            part_id,
            worker_id,
            expected_batch_id,
        )
        .await
    }

    // ── t_part_batch mark_*（4）──
    async fn mark_batch_passed_inspection(
        &mut self,
        batch_id: i64,
        expected_version: i32,
        current_user_id: Option<i64>,
    ) -> Result<batch_status::RollupOutcome, AppError> {
        PartBatchRepo::mark_batch_passed_inspection(
            &mut **self,
            batch_id,
            expected_version,
            current_user_id,
        )
        .await
    }

    async fn mark_batch_inspected(
        &mut self,
        batch_id: i64,
        expected_version: i32,
        shelf_id: i64,
        current_user_id: Option<i64>,
    ) -> Result<batch_status::RollupOutcome, AppError> {
        PartBatchRepo::mark_batch_inspected(
            &mut **self,
            batch_id,
            expected_version,
            shelf_id,
            current_user_id,
        )
        .await
    }

    async fn mark_batch_failed_inspection(
        &mut self,
        batch_id: i64,
        expected_version: i32,
        shelf_id: i64,
        current_process_step_id: Option<i64>,
        current_process_id: Option<i64>,
        current_user_id: Option<i64>,
    ) -> Result<batch_status::RollupOutcome, AppError> {
        PartBatchRepo::mark_batch_failed_inspection(
            &mut **self,
            batch_id,
            expected_version,
            shelf_id,
            current_process_step_id,
            current_process_id,
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
        advance_to_process_id: Option<i64>,
        current_user_id: Option<i64>,
    ) -> Result<u64, sqlx::Error> {
        PartBatchRepo::mark_batch_returned(
            &mut **self,
            batch_id,
            expected_version,
            shelf_id,
            current_process_step_id,
            advance_to_process_id,
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
    ) -> Result<u64, AppError> {
        PartBatchRepo::mark_batch_delivered(
            &mut **self,
            batch_id,
            expected_version,
            current_user_id,
        )
        .await
    }

    async fn mark_batch_completed(
        &mut self,
        batch_id: i64,
        expected_version: i32,
        current_user_id: i64,
        event_id: Option<i64>,
    ) -> Result<u64, AppError> {
        PartBatchRepo::mark_batch_completed(
            &mut **self,
            batch_id,
            expected_version,
            current_user_id,
            event_id,
        )
        .await
    }

    async fn mark_part_cancelled(
        &mut self,
        part_id: i64,
        expected_version: i32,
        current_user_id: i64,
    ) -> Result<u64, sqlx::Error> {
        PartBatchRepo::mark_part_cancelled(&mut **self, part_id, expected_version, current_user_id)
            .await
    }

    async fn mark_batch_repairing(
        &mut self,
        batch_id: i64,
        expected_version: i32,
        current_user_id: i64,
    ) -> Result<u64, AppError> {
        PartBatchRepo::mark_batch_repairing(
            &mut **self,
            batch_id,
            expected_version,
            current_user_id,
        )
        .await
    }

    async fn cancel_all_active_batches_for_part(
        &mut self,
        part_id: i64,
        current_user_id: i64,
    ) -> Result<u64, AppError> {
        PartBatchRepo::cancel_all_active_batches_for_part(&mut **self, part_id, current_user_id)
            .await
    }

    // 2026-09-30 新增：force-complete 端点（MANAGER 单角色强推 part + 批次）。
    async fn force_complete_all_batches_for_part(
        &mut self,
        part_id: i64,
        current_user_id: i64,
        event_id: Option<i64>,
    ) -> Result<u64, AppError> {
        PartBatchRepo::force_complete_all_batches_for_part(
            &mut **self,
            part_id,
            current_user_id,
            event_id,
        )
        .await
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
        PartBatchRepo::split_batch_for_partial_pass(
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
    async fn insert_part_event<'b>(&mut self, e: NewPartEvent<'b>) -> Result<(), sqlx::Error> {
        PartRepo::insert_part_event(&mut **self, e).await
    }

    // ── 跨域 helper（2）── 委托其它域 / 其它表 ──────────
    async fn part_batch_has_active_on_delivery_note(
        &mut self,
        part_id: i64,
    ) -> Result<bool, sqlx::Error> {
        use crate::modules::prod::batch::repo::PartBatchRepo;
        PartBatchRepo::has_active_batch_on_delivery_note(&mut **self, part_id).await
    }

    /// 2026-10-05 新增：建单派发序列号前取 L1 客户的 `serial_prefix`。
    /// 返回 `AppError`（而非 `sqlx::Error`）：20308 / 20104 / 20102 三个业务码要
    /// 原样透到响应信封，转 `sqlx::Error` 会被降级成 500。
    async fn serial_prefix_for_customer(&mut self, customer_id: i64) -> Result<char, AppError> {
        PartRepo::serial_prefix_for_customer(&mut **self, customer_id).await
    }

    // ── 采购订单 Excel 匹配 + 订单信息回填（2026-10-06 新增，5）──
    async fn list_match_parts_by_keys<'a>(
        &mut self,
        drawing_nos: &'a [&'a str],
        names: &'a [&'a str],
    ) -> Result<Vec<TPart>, sqlx::Error> {
        PartRepo::list_match_parts_by_keys(&mut **self, drawing_nos, names).await
    }

    async fn list_match_assemblies_by_keys<'a>(
        &mut self,
        drawing_nos: &'a [&'a str],
        names: &'a [&'a str],
    ) -> Result<Vec<AssemblyMatchRow>, sqlx::Error> {
        PartRepo::list_match_assemblies_by_keys(&mut **self, drawing_nos, names).await
    }

    async fn list_assembly_names_by_ids<'a>(
        &mut self,
        assembly_ids: &'a [i64],
    ) -> Result<Vec<AssemblyMatchRow>, sqlx::Error> {
        PartRepo::list_assembly_names_by_ids(&mut **self, assembly_ids).await
    }

    async fn list_children_by_assemblies<'a>(
        &mut self,
        assembly_ids: &'a [i64],
        include_deleted: bool,
    ) -> Result<Vec<TPart>, sqlx::Error> {
        PartRepo::list_children_by_assemblies(&mut **self, assembly_ids, include_deleted).await
    }

    async fn update_order_info<'a>(
        &mut self,
        part_id: i64,
        expected_version: i32,
        order_no: Option<Option<&'a str>>,
        system_delivery_date: Option<Option<chrono::NaiveDate>>,
        note: Option<Option<&'a str>>,
        updated_by: i64,
    ) -> Result<u64, sqlx::Error> {
        PartRepo::update_order_info(
            &mut **self,
            part_id,
            expected_version,
            order_no,
            system_delivery_date,
            note,
            updated_by,
        )
        .await
    }
}

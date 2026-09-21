//! outsource 域 repo 层（SQL 真源 + 胖 trait + PG 实现）
//!
//! ## 结构（2026-09-22 refactor 对齐 iam 事务分层范式）
//! - `sql.rs`：原 `repo.rs` 全文搬迁，3 个 ZST struct `OutsourceCompanyRepo` /
//!   `OutsourceQuoteRepo` / `OutsourceShipmentRepo` + 全部固有静态方法；**SQL 字符串零 diff**。
//! - `mod.rs`（本文件）：对外暴露胖 trait `OutsourceRepoTrait`，并直接
//!   `impl OutsourceRepoTrait for &mut PgConnection` ——handler/service 借
//!   `&mut *tx` / `&mut *conn` 即可，零中间壳。
//!
//! ## 为什么是胖 trait 而不是按实体拆 4 trait
//! `&mut PgConnection` 同一作用域只能借给一个 repo 实例；如果按实体拆 4 trait，service
//! 同时使用 company_repo + quote_repo 时无法表达「同连接两次借用」。胖 trait
//! `OutsourceRepoTrait` 是单借位，service 签名
//! `<R: OutsourceRepoTrait>(&self, mut repo: R, ...)` 一次收下（by-value；生产路径
//! `R = &mut PgConnection`，单测 `R = MockOutsourceRepo`），方法体内全部 `repo.xxx()`
//! 都走同一连接。
//!
//! ## 为什么 trait 命名为 `OutsourceRepoTrait`（带 `Trait` 后缀）
//! 与 shelf / customer 同形：原本 `repo.rs` 已经有 3 个 ZST struct
//! `OutsourceCompanyRepo` / `OutsourceQuoteRepo` / `OutsourceShipmentRepo` 分别承载
//! 各子表方法，trait 是这三者的合并 —— 用 `OutsourceRepo` 作 trait 名会与单 ZST 名歧义，
//! 故加 `Trait` 后缀。**outsource 域自封闭，无 cross-module 静态调用方**，命名空间上
//! 无强制保留 ZST 名 `OutsourceCompanyRepo` 等的需求——本任务选 `OutsourceRepoTrait`
//! 是命名一致性的考量（与 `ShelfRepoTrait` / `CustomerRepoTrait` 视觉一致）。
//!
//! ## 为什么 trait 可以直接对 `&mut PgConnection` 实现
//! `Transaction<'_, Postgres>` 与 `PoolConnection<Postgres>` 都
//! `DerefMut<Target = PgConnection>`，故 `&mut *tx` / `&mut *conn` 即
//! `&mut PgConnection`，可直接喂给 `sql::XxxRepo::yyy`。
//! 旧 `PgOutsourceRepo<'a>` 转发壳（与 iam 2026-09-22 同步）从未存在，task 直接
//! 落地「trait 对 `&mut PgConnection` 实现」的最终形态。
//!
//! ## automock
//! `#[cfg_attr(test, mockall::automock)]` 在 trait 上声明，生成 `MockOutsourceRepo`
//! 供 `service_tests` 注入。方法签名里的 `<'a>` 显式生命周期是 mockall 0.15 + async_trait
//! 的硬性要求（沿用 iam/uow 注释结论）。当前 service 全部走 `tests/outsource_*_api.rs`
//! 集成测试守护，service 内联 mod tests 5 个 helper 是纯函数（`parse_price` /
//! `parse_snowflake_id` / `format_price` / `current_id_to_snowflake_unique`），无需
//! mock 注入。
//!
//! ## 错误类型
//! repo trait 方法 → `sqlx::Error`（与 sql.rs 签名 1:1，唯一性 / 业务校验在 service
//! 层做）。

use async_trait::async_trait;
use chrono::NaiveDateTime;
use rust_decimal::Decimal;
use sqlx::PgConnection;

pub mod sql;

// 重导出 sql.rs 中的 ZST struct / NewXxx insert 与 model 表行类型，让上层继续用
// `super::repo::{TOutsourceCompany, OutsourceCompanyRepo, NewOutsourceQuote, ...}` 这种
// 路径不破。
pub use super::model::{
    NewOutsourceCompany, NewOutsourceCompanyProcess, NewOutsourceQuote, NewOutsourceQuoteEvent,
    NewOutsourceShipment, TOutsourceCompany, TOutsourceCompanyProcess, TOutsourceQuote,
    TOutsourceQuoteEvent, TOutsourceShipment,
};
pub use sql::{
    OutsourceCompanyProcessRepo, OutsourceCompanyRepo, OutsourceQuoteEventRepo, OutsourceQuoteRepo,
    OutsourceShipmentRepo,
};

/// outsource 域数据访问胖 trait。
///
/// 单 trait 合并 3 ZST（company + company_process + quote + quote_event + shipment）
/// 共 23 方法：company 8 + company_process 4 + quote 11 + quote_event 1 + shipment 6。
///
/// 方法签名 = `sql.rs` 固有静态方法去 executor 形参。`<'a>` 显式生命周期是 mockall
/// 0.15 automock 在 `async_trait` 上下文的硬性要求。
#[cfg_attr(test, mockall::automock)]
#[async_trait]
pub trait OutsourceRepoTrait: Send {
    // ── t_outsource_company（8）──
    async fn company_get_by_id(
        &mut self,
        id: i64,
        include_deleted: bool,
    ) -> Result<Option<TOutsourceCompany>, sqlx::Error>;
    async fn company_get_by_name<'a>(
        &mut self,
        name: &'a str,
    ) -> Result<Option<TOutsourceCompany>, sqlx::Error>;
    async fn company_list_by_ids<'a>(
        &mut self,
        ids: &'a [i64],
    ) -> Result<Vec<TOutsourceCompany>, sqlx::Error>;
    async fn company_list_with_filters<'a>(
        &mut self,
        name_like: Option<&'a str>,
        is_active: Option<bool>,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<TOutsourceCompany>, sqlx::Error>;
    async fn company_count_with_filters<'a>(
        &mut self,
        name_like: Option<&'a str>,
        is_active: Option<bool>,
    ) -> Result<i64, sqlx::Error>;
    async fn company_create(
        &mut self,
        new: NewOutsourceCompany,
    ) -> Result<TOutsourceCompany, sqlx::Error>;
    #[allow(clippy::too_many_arguments)]
    async fn company_update<'a>(
        &mut self,
        id: i64,
        version: i32,
        name: Option<&'a str>,
        contact_name: Option<Option<&'a str>>,
        contact_phone: Option<Option<&'a str>>,
        address: Option<Option<&'a str>>,
        is_active: Option<bool>,
        updated_by: i64,
    ) -> Result<u64, sqlx::Error>;
    async fn company_soft_delete(
        &mut self,
        id: i64,
        version: i32,
        updated_by: i64,
    ) -> Result<u64, sqlx::Error>;

    // ── t_outsource_company_process（4）── 用 `junction_` 前缀消歧义
    async fn junction_list_by_company(
        &mut self,
        company_id: i64,
        include_deleted: bool,
    ) -> Result<Vec<TOutsourceCompanyProcess>, sqlx::Error>;
    async fn junction_list_company_ids_by_process(
        &mut self,
        process_id: i64,
    ) -> Result<Vec<i64>, sqlx::Error>;
    async fn junction_create(
        &mut self,
        new: NewOutsourceCompanyProcess,
    ) -> Result<TOutsourceCompanyProcess, sqlx::Error>;
    async fn junction_soft_delete_by_company(
        &mut self,
        company_id: i64,
        updated_by: i64,
    ) -> Result<u64, sqlx::Error>;

    // ── t_outsource_quote（11）──
    async fn quote_get_by_id(
        &mut self,
        id: i64,
        include_deleted: bool,
    ) -> Result<Option<TOutsourceQuote>, sqlx::Error>;
    async fn quote_get_active_for_tuple(
        &mut self,
        part_id: i64,
        company_id: i64,
        process_id: i64,
    ) -> Result<Option<TOutsourceQuote>, sqlx::Error>;
    async fn quote_list_all_approved(&mut self) -> Result<Vec<TOutsourceQuote>, sqlx::Error>;
    async fn quote_list_active_by_part_process(
        &mut self,
        part_id: i64,
        process_id: i64,
        exclude_id: i64,
    ) -> Result<Vec<TOutsourceQuote>, sqlx::Error>;
    #[allow(clippy::too_many_arguments)]
    async fn quote_list_with_filters<'a>(
        &mut self,
        status: Option<&'a str>,
        statuses: &'a [String],
        part_id: Option<i64>,
        part_ids_in: &'a [i64],
        outsource_company_id: Option<i64>,
        sort_by: &'a str,
        sort_dir: &'a str,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<TOutsourceQuote>, sqlx::Error>;
    #[allow(clippy::too_many_arguments)]
    async fn quote_count_with_filters<'a>(
        &mut self,
        status: Option<&'a str>,
        statuses: &'a [String],
        part_id: Option<i64>,
        part_ids_in: &'a [i64],
        outsource_company_id: Option<i64>,
    ) -> Result<i64, sqlx::Error>;
    async fn quote_create(
        &mut self,
        new: NewOutsourceQuote,
    ) -> Result<TOutsourceQuote, sqlx::Error>;
    async fn quote_update<'a>(
        &mut self,
        id: i64,
        version: i32,
        price: Option<Decimal>,
        note: Option<Option<&'a str>>,
        updated_by: i64,
    ) -> Result<u64, sqlx::Error>;
    async fn quote_submit(
        &mut self,
        id: i64,
        version: i32,
        updated_by: i64,
    ) -> Result<u64, sqlx::Error>;
    async fn quote_approve<'a>(
        &mut self,
        id: i64,
        version: i32,
        review_note: Option<&'a str>,
        updated_by: i64,
    ) -> Result<u64, sqlx::Error>;
    async fn quote_reject<'a>(
        &mut self,
        id: i64,
        version: i32,
        review_note: &'a str,
        updated_by: i64,
    ) -> Result<u64, sqlx::Error>;
    #[allow(clippy::too_many_arguments)]
    async fn quote_reject_competitors<'a>(
        &mut self,
        part_id: i64,
        process_id: i64,
        exclude_id: i64,
        review_note: &'a str,
        updated_by: i64,
    ) -> Result<u64, sqlx::Error>;
    async fn quote_soft_delete(
        &mut self,
        id: i64,
        version: i32,
        updated_by: i64,
    ) -> Result<u64, sqlx::Error>;

    // ── t_outsource_quote_event（1）──
    async fn quote_event_create(
        &mut self,
        new: NewOutsourceQuoteEvent,
    ) -> Result<TOutsourceQuoteEvent, sqlx::Error>;

    // ── t_outsource_shipment（6）──
    async fn shipment_get_by_id(
        &mut self,
        id: i64,
        include_deleted: bool,
    ) -> Result<Option<TOutsourceShipment>, sqlx::Error>;
    async fn shipment_create(
        &mut self,
        new: NewOutsourceShipment,
    ) -> Result<TOutsourceShipment, sqlx::Error>;
    async fn shipment_find_open_for_batch(
        &mut self,
        batch_id: i64,
    ) -> Result<Option<TOutsourceShipment>, sqlx::Error>;
    async fn shipment_mark_received(
        &mut self,
        id: i64,
        version: i32,
        updated_by: i64,
    ) -> Result<u64, sqlx::Error>;
    async fn shipment_reconcile_update(
        &mut self,
        id: i64,
        version: i32,
        unit_price: Option<Decimal>,
        quantity: Option<i32>,
        is_billed: Option<bool>,
        updated_by: i64,
    ) -> Result<u64, sqlx::Error>;
    #[allow(clippy::too_many_arguments)]
    async fn shipment_count_reconciliation_for_company<'a>(
        &mut self,
        company_id: i64,
        part_ids_in: &'a [i64],
        sent_from: Option<NaiveDateTime>,
        sent_to: Option<NaiveDateTime>,
        received_from: Option<NaiveDateTime>,
        received_to: Option<NaiveDateTime>,
    ) -> Result<i64, sqlx::Error>;

    // ── 跨域 helper（service 散落的 inline SQL 抽 trait） ──
    // 2026-09-22 refactor：service 内的 inline SQL（`t_part` / `t_process` / `t_part_batch`
    // 跨域 SELECT）下沉为 trait 方法，避免 service 需要 `&mut PgConnection` 二次借用。
    // 与 com/customer 的 `lookup_names` / `count_parts_using_customer` 同形。

    /// `t_part` 按 id 查存在性（仅未软删）。供 create_quote 校验 part_id。
    async fn part_exists(&mut self, part_id: i64) -> Result<bool, sqlx::Error>;
    /// `t_part` 按关键字模糊搜（drawing_no OR name）。供 list_quotes 关键字过滤。
    async fn part_keyword_search<'a>(
        &mut self,
        keyword: &'a str,
    ) -> Result<Vec<i64>, sqlx::Error>;
    /// `t_process` 按 id 查 category。供 create_quote 校验 OUTSOURCE 类别。
    async fn process_get_category(
        &mut self,
        process_id: i64,
    ) -> Result<Option<String>, sqlx::Error>;
    /// `t_process` 按 ids 查 `(id, code, name, category)`（仅未软删）。
    /// 供 build_with_processes 与 quote_out_many 使用。
    async fn process_map_full<'a>(
        &mut self,
        process_ids: &'a [i64],
    ) -> Result<Vec<(i64, String, String, String)>, sqlx::Error>;
    /// `t_process` 按 ids 查 `(id, code, name)`（仅未软删）。供 quote_out_many。
    async fn process_map_short<'a>(
        &mut self,
        process_ids: &'a [i64],
    ) -> Result<Vec<(i64, String, String)>, sqlx::Error>;
    /// `t_process` 按 ids 查 `(id, category)`（仅未软删）。供 validate_processes_outsource。
    async fn process_map_category<'a>(
        &mut self,
        process_ids: &'a [i64],
    ) -> Result<Vec<(i64, String)>, sqlx::Error>;
    /// `t_part` 按 ids 查 `(id, serial_no, drawing_no, name, is_urgent, unit_price::text)`。
    /// 供 quote_out_many 拼装 part 显示字段。
    #[allow(clippy::type_complexity)]
    async fn part_map_for_quote<'a>(
        &mut self,
        part_ids: &'a [i64],
    ) -> Result<Vec<(i64, Option<String>, String, String, bool, Option<String>)>, sqlx::Error>;
    /// `t_outsource_company` 按 ids 查 `(id, name)`（仅未软删）。供 quote_out_many。
    async fn company_map_name<'a>(
        &mut self,
        company_ids: &'a [i64],
    ) -> Result<Vec<(i64, String)>, sqlx::Error>;
    /// `t_part` 按 id 查 `(drawing_no, name)`。供 shipment_out 单条拼装。
    async fn part_drawing_name(
        &mut self,
        part_id: i64,
    ) -> Result<Option<(String, String)>, sqlx::Error>;
    /// `t_process` 按 id 查 name。供 shipment_out 单条拼装。
    async fn process_get_name(&mut self, process_id: i64) -> Result<Option<String>, sqlx::Error>;
    /// `t_part_batch` 按 id 查 batch_no。供 shipment_out 单条拼装。
    async fn batch_get_no(&mut self, batch_id: i64) -> Result<Option<i32>, sqlx::Error>;
}

/// 把 `OutsourceRepoTrait` 直接对 `&mut PgConnection` 实现——handler/service 借
/// `&mut *tx` 或 `&mut *conn` 即可调用 `sql::XxxRepo::yyy`，零转发壳。
///
/// trait 方法收 `&mut self`，impl 在 `&mut PgConnection` 上时
/// `self: &mut &mut PgConnection`，两次 deref 才得到 `PgConnection`：
/// `*self: &mut PgConnection`，`**self: PgConnection`，
/// 故喂给 `sql::XxxRepo::yyy` 须写 `&mut **self`（reborrow，避免 move 引用本身）。
#[async_trait]
impl OutsourceRepoTrait for &mut PgConnection {
    // ── t_outsource_company（8）── 一行委托 sql::OutsourceCompanyRepo ───
    async fn company_get_by_id(
        &mut self,
        id: i64,
        include_deleted: bool,
    ) -> Result<Option<TOutsourceCompany>, sqlx::Error> {
        OutsourceCompanyRepo::get_by_id(&mut **self, id, include_deleted).await
    }

    async fn company_get_by_name<'b>(
        &mut self,
        name: &'b str,
    ) -> Result<Option<TOutsourceCompany>, sqlx::Error> {
        OutsourceCompanyRepo::get_by_name(&mut **self, name).await
    }

    async fn company_list_by_ids<'b>(
        &mut self,
        ids: &'b [i64],
    ) -> Result<Vec<TOutsourceCompany>, sqlx::Error> {
        OutsourceCompanyRepo::list_by_ids(&mut **self, ids).await
    }

    async fn company_list_with_filters<'b>(
        &mut self,
        name_like: Option<&'b str>,
        is_active: Option<bool>,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<TOutsourceCompany>, sqlx::Error> {
        OutsourceCompanyRepo::list_with_filters(&mut **self, name_like, is_active, limit, offset)
            .await
    }

    async fn company_count_with_filters<'b>(
        &mut self,
        name_like: Option<&'b str>,
        is_active: Option<bool>,
    ) -> Result<i64, sqlx::Error> {
        OutsourceCompanyRepo::count_with_filters(&mut **self, name_like, is_active).await
    }

    async fn company_create(
        &mut self,
        new: NewOutsourceCompany,
    ) -> Result<TOutsourceCompany, sqlx::Error> {
        OutsourceCompanyRepo::create(&mut **self, new).await
    }

    #[allow(clippy::too_many_arguments)]
    async fn company_update<'b>(
        &mut self,
        id: i64,
        version: i32,
        name: Option<&'b str>,
        contact_name: Option<Option<&'b str>>,
        contact_phone: Option<Option<&'b str>>,
        address: Option<Option<&'b str>>,
        is_active: Option<bool>,
        updated_by: i64,
    ) -> Result<u64, sqlx::Error> {
        OutsourceCompanyRepo::update(
            &mut **self,
            id,
            version,
            name,
            contact_name,
            contact_phone,
            address,
            is_active,
            updated_by,
        )
        .await
    }

    async fn company_soft_delete(
        &mut self,
        id: i64,
        version: i32,
        updated_by: i64,
    ) -> Result<u64, sqlx::Error> {
        OutsourceCompanyRepo::soft_delete(&mut **self, id, version, updated_by).await
    }

    // ── t_outsource_company_process（4）── 一行委托 sql::OutsourceCompanyProcessRepo
    async fn junction_list_by_company(
        &mut self,
        company_id: i64,
        include_deleted: bool,
    ) -> Result<Vec<TOutsourceCompanyProcess>, sqlx::Error> {
        OutsourceCompanyProcessRepo::list_by_company(&mut **self, company_id, include_deleted).await
    }

    async fn junction_list_company_ids_by_process(
        &mut self,
        process_id: i64,
    ) -> Result<Vec<i64>, sqlx::Error> {
        OutsourceCompanyProcessRepo::list_company_ids_by_process(&mut **self, process_id).await
    }

    async fn junction_create(
        &mut self,
        new: NewOutsourceCompanyProcess,
    ) -> Result<TOutsourceCompanyProcess, sqlx::Error> {
        OutsourceCompanyProcessRepo::create(&mut **self, new).await
    }

    async fn junction_soft_delete_by_company(
        &mut self,
        company_id: i64,
        updated_by: i64,
    ) -> Result<u64, sqlx::Error> {
        OutsourceCompanyProcessRepo::soft_delete_by_company(&mut **self, company_id, updated_by)
            .await
    }

    // ── t_outsource_quote（11）── 一行委托 sql::OutsourceQuoteRepo ─────────
    async fn quote_get_by_id(
        &mut self,
        id: i64,
        include_deleted: bool,
    ) -> Result<Option<TOutsourceQuote>, sqlx::Error> {
        OutsourceQuoteRepo::get_by_id(&mut **self, id, include_deleted).await
    }

    async fn quote_get_active_for_tuple(
        &mut self,
        part_id: i64,
        company_id: i64,
        process_id: i64,
    ) -> Result<Option<TOutsourceQuote>, sqlx::Error> {
        OutsourceQuoteRepo::get_active_for_tuple(&mut **self, part_id, company_id, process_id).await
    }

    async fn quote_list_all_approved(&mut self) -> Result<Vec<TOutsourceQuote>, sqlx::Error> {
        OutsourceQuoteRepo::list_all_approved(&mut **self).await
    }

    async fn quote_list_active_by_part_process(
        &mut self,
        part_id: i64,
        process_id: i64,
        exclude_id: i64,
    ) -> Result<Vec<TOutsourceQuote>, sqlx::Error> {
        OutsourceQuoteRepo::list_active_by_part_process(&mut **self, part_id, process_id, exclude_id)
            .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn quote_list_with_filters<'b>(
        &mut self,
        status: Option<&'b str>,
        statuses: &'b [String],
        part_id: Option<i64>,
        part_ids_in: &'b [i64],
        outsource_company_id: Option<i64>,
        sort_by: &'b str,
        sort_dir: &'b str,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<TOutsourceQuote>, sqlx::Error> {
        OutsourceQuoteRepo::list_with_filters(
            &mut **self,
            status,
            statuses,
            part_id,
            part_ids_in,
            outsource_company_id,
            sort_by,
            sort_dir,
            limit,
            offset,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn quote_count_with_filters<'b>(
        &mut self,
        status: Option<&'b str>,
        statuses: &'b [String],
        part_id: Option<i64>,
        part_ids_in: &'b [i64],
        outsource_company_id: Option<i64>,
    ) -> Result<i64, sqlx::Error> {
        OutsourceQuoteRepo::count_with_filters(
            &mut **self,
            status,
            statuses,
            part_id,
            part_ids_in,
            outsource_company_id,
        )
        .await
    }

    async fn quote_create(
        &mut self,
        new: NewOutsourceQuote,
    ) -> Result<TOutsourceQuote, sqlx::Error> {
        OutsourceQuoteRepo::create(&mut **self, new).await
    }

    async fn quote_update<'b>(
        &mut self,
        id: i64,
        version: i32,
        price: Option<Decimal>,
        note: Option<Option<&'b str>>,
        updated_by: i64,
    ) -> Result<u64, sqlx::Error> {
        OutsourceQuoteRepo::update(&mut **self, id, version, price, note, updated_by).await
    }

    async fn quote_submit(
        &mut self,
        id: i64,
        version: i32,
        updated_by: i64,
    ) -> Result<u64, sqlx::Error> {
        OutsourceQuoteRepo::submit(&mut **self, id, version, updated_by).await
    }

    async fn quote_approve<'b>(
        &mut self,
        id: i64,
        version: i32,
        review_note: Option<&'b str>,
        updated_by: i64,
    ) -> Result<u64, sqlx::Error> {
        OutsourceQuoteRepo::approve(&mut **self, id, version, review_note, updated_by).await
    }

    async fn quote_reject<'b>(
        &mut self,
        id: i64,
        version: i32,
        review_note: &'b str,
        updated_by: i64,
    ) -> Result<u64, sqlx::Error> {
        OutsourceQuoteRepo::reject(&mut **self, id, version, review_note, updated_by).await
    }

    #[allow(clippy::too_many_arguments)]
    async fn quote_reject_competitors<'b>(
        &mut self,
        part_id: i64,
        process_id: i64,
        exclude_id: i64,
        review_note: &'b str,
        updated_by: i64,
    ) -> Result<u64, sqlx::Error> {
        OutsourceQuoteRepo::reject_competitors(
            &mut **self,
            part_id,
            process_id,
            exclude_id,
            review_note,
            updated_by,
        )
        .await
    }

    async fn quote_soft_delete(
        &mut self,
        id: i64,
        version: i32,
        updated_by: i64,
    ) -> Result<u64, sqlx::Error> {
        OutsourceQuoteRepo::soft_delete(&mut **self, id, version, updated_by).await
    }

    // ── t_outsource_quote_event（1）── 一行委托 sql::OutsourceQuoteEventRepo ─
    async fn quote_event_create(
        &mut self,
        new: NewOutsourceQuoteEvent,
    ) -> Result<TOutsourceQuoteEvent, sqlx::Error> {
        OutsourceQuoteEventRepo::create(&mut **self, new).await
    }

    // ── t_outsource_shipment（6）── 一行委托 sql::OutsourceShipmentRepo ─────
    async fn shipment_get_by_id(
        &mut self,
        id: i64,
        include_deleted: bool,
    ) -> Result<Option<TOutsourceShipment>, sqlx::Error> {
        OutsourceShipmentRepo::get_by_id(&mut **self, id, include_deleted).await
    }

    async fn shipment_create(
        &mut self,
        new: NewOutsourceShipment,
    ) -> Result<TOutsourceShipment, sqlx::Error> {
        OutsourceShipmentRepo::create(&mut **self, new).await
    }

    async fn shipment_find_open_for_batch(
        &mut self,
        batch_id: i64,
    ) -> Result<Option<TOutsourceShipment>, sqlx::Error> {
        OutsourceShipmentRepo::find_open_for_batch(&mut **self, batch_id).await
    }

    async fn shipment_mark_received(
        &mut self,
        id: i64,
        version: i32,
        updated_by: i64,
    ) -> Result<u64, sqlx::Error> {
        OutsourceShipmentRepo::mark_received(&mut **self, id, version, updated_by).await
    }

    async fn shipment_reconcile_update(
        &mut self,
        id: i64,
        version: i32,
        unit_price: Option<Decimal>,
        quantity: Option<i32>,
        is_billed: Option<bool>,
        updated_by: i64,
    ) -> Result<u64, sqlx::Error> {
        OutsourceShipmentRepo::reconcile_update(
            &mut **self,
            id,
            version,
            unit_price,
            quantity,
            is_billed,
            updated_by,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn shipment_count_reconciliation_for_company<'b>(
        &mut self,
        company_id: i64,
        part_ids_in: &'b [i64],
        sent_from: Option<NaiveDateTime>,
        sent_to: Option<NaiveDateTime>,
        received_from: Option<NaiveDateTime>,
        received_to: Option<NaiveDateTime>,
    ) -> Result<i64, sqlx::Error> {
        OutsourceShipmentRepo::count_reconciliation_for_company(
            &mut **self,
            company_id,
            part_ids_in,
            sent_from,
            sent_to,
            received_from,
            received_to,
        )
        .await
    }

    // ── 跨域 helper（11）── 一行委托 `sqlx::query_as` 跨表 SELECT ─────────
    async fn part_exists(&mut self, part_id: i64) -> Result<bool, sqlx::Error> {
        let row: Option<(i64,)> = sqlx::query_as(
            "SELECT id FROM t_part WHERE id = $1 AND deleted_at IS NULL",
        )
        .bind(part_id)
        .fetch_optional(&mut **self)
        .await?;
        Ok(row.is_some())
    }

    async fn part_keyword_search<'b>(
        &mut self,
        keyword: &'b str,
    ) -> Result<Vec<i64>, sqlx::Error> {
        let rows: Vec<(i64,)> = sqlx::query_as(
            "SELECT id FROM t_part WHERE deleted_at IS NULL AND \
             (drawing_no ILIKE $1 OR name ILIKE $1) LIMIT 10000",
        )
        .bind(format!("%{}%", keyword))
        .fetch_all(&mut **self)
        .await?;
        Ok(rows.into_iter().map(|r| r.0).collect())
    }

    async fn process_get_category(
        &mut self,
        process_id: i64,
    ) -> Result<Option<String>, sqlx::Error> {
        let row: Option<(String,)> = sqlx::query_as(
            "SELECT category FROM t_process WHERE id = $1 AND deleted_at IS NULL",
        )
        .bind(process_id)
        .fetch_optional(&mut **self)
        .await?;
        Ok(row.map(|r| r.0))
    }

    async fn process_map_full<'b>(
        &mut self,
        process_ids: &'b [i64],
    ) -> Result<Vec<(i64, String, String, String)>, sqlx::Error> {
        sqlx::query_as(
            "SELECT id, code, name, category FROM t_process \
             WHERE id = ANY($1) AND deleted_at IS NULL",
        )
        .bind(process_ids)
        .fetch_all(&mut **self)
        .await
    }

    async fn process_map_short<'b>(
        &mut self,
        process_ids: &'b [i64],
    ) -> Result<Vec<(i64, String, String)>, sqlx::Error> {
        sqlx::query_as(
            "SELECT id, code, name FROM t_process \
             WHERE id = ANY($1) AND deleted_at IS NULL",
        )
        .bind(process_ids)
        .fetch_all(&mut **self)
        .await
    }

    async fn process_map_category<'b>(
        &mut self,
        process_ids: &'b [i64],
    ) -> Result<Vec<(i64, String)>, sqlx::Error> {
        sqlx::query_as(
            "SELECT id, category FROM t_process WHERE id = ANY($1) AND deleted_at IS NULL",
        )
        .bind(process_ids)
        .fetch_all(&mut **self)
        .await
    }

    #[allow(clippy::type_complexity)]
    async fn part_map_for_quote<'b>(
        &mut self,
        part_ids: &'b [i64],
    ) -> Result<Vec<(i64, Option<String>, String, String, bool, Option<String>)>, sqlx::Error>
    {
        sqlx::query_as(
            "SELECT id, serial_no, drawing_no, name, is_urgent, unit_price::text \
             FROM t_part WHERE id = ANY($1) AND deleted_at IS NULL",
        )
        .bind(part_ids)
        .fetch_all(&mut **self)
        .await
    }

    async fn company_map_name<'b>(
        &mut self,
        company_ids: &'b [i64],
    ) -> Result<Vec<(i64, String)>, sqlx::Error> {
        sqlx::query_as(
            "SELECT id, name FROM t_outsource_company \
             WHERE id = ANY($1) AND deleted_at IS NULL",
        )
        .bind(company_ids)
        .fetch_all(&mut **self)
        .await
    }

    async fn part_drawing_name(
        &mut self,
        part_id: i64,
    ) -> Result<Option<(String, String)>, sqlx::Error> {
        sqlx::query_as("SELECT drawing_no, name FROM t_part WHERE id = $1")
            .bind(part_id)
            .fetch_optional(&mut **self)
            .await
    }

    async fn process_get_name(&mut self, process_id: i64) -> Result<Option<String>, sqlx::Error> {
        sqlx::query_scalar("SELECT name FROM t_process WHERE id = $1")
            .bind(process_id)
            .fetch_optional(&mut **self)
            .await
    }

    async fn batch_get_no(&mut self, batch_id: i64) -> Result<Option<i32>, sqlx::Error> {
        sqlx::query_scalar("SELECT batch_no FROM t_part_batch WHERE id = $1")
            .bind(batch_id)
            .fetch_optional(&mut **self)
            .await
    }
}

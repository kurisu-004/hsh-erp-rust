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
    OutsourceCompanyProcessRepo, OutsourceCompanyRepo, OutsourcePoolRepo, OutsourceQuotableRepo,
    OutsourceQuoteEventRepo, OutsourceQuoteRepo, OutsourceSendableRepo, OutsourceShipmentRepo,
};

// ===========================================================================
//  读模型行结构（2026-10-03 新增）
// ===========================================================================
//
// 既有跨表投影（`part_map_for_quote` / `process_map_short` 等）都返回 tuple，
// 但 4 个新 list 端点每行 13~21 列 —— tuple 到第 5 列就不可读，且列序错位
// 是静默 bug。故改用具名 `FromRow` 结构，声明在 `repo/mod.rs`（trait 签名
// 引用处），SQL 字符串仍在 `repo/sql.rs`。
//
// 全部 `sqlx::FromRow` + 运行时 `query_as`（**非 `query!` 宏**），
// 因此不需要重新生成 `.sqlx/`。

/// `GET /outsource-companies/{id}/sent-parts` 行。
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct OutsourceSentPartRow {
    pub id: i64,
    pub version: i32,
    pub quote_id: i64,
    pub part_id: i64,
    pub drawing_no: Option<String>,
    pub name: Option<String>,
    pub is_urgent: bool,
    /// L2 客户名（`t_customer`，即 part 直属客户）。
    pub customer_name: Option<String>,
    /// L1 客户名（`t_customer.parent_id`）。
    pub parent_customer_name: Option<String>,
    pub process_id: i64,
    pub process_name: Option<String>,
    pub batch_no: Option<i32>,
    pub quantity: i32,
    /// `unit_price::text`（Decimal 字符串；避免 Decimal 精度往返）。
    pub unit_price: String,
    pub sent_at: chrono::NaiveDateTime,
    pub received_at: Option<chrono::NaiveDateTime>,
    pub status: String,
    pub is_billed: bool,
}

/// `GET /outsource-shipments/in-flight` 行。
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct OutsourceInFlightRow {
    /// `t_part.id`。
    pub id: i64,
    /// `t_part_batch.id`。
    pub batch_id: i64,
    pub batch_no: i32,
    /// `t_part_batch.quantity`（剩余待收量）。
    pub quantity: i32,
    /// `t_part_batch.version`（**不是** shipment.version）。
    pub version: i32,
    pub serial_no: Option<String>,
    pub drawing_no: Option<String>,
    pub name: Option<String>,
    pub is_urgent: bool,
    pub customer_name: Option<String>,
    pub parent_customer_name: Option<String>,
    pub process_id: i64,
    pub process_name: Option<String>,
    pub outsource_company_id: i64,
    pub outsource_company_name: Option<String>,
    pub sent_at: chrono::NaiveDateTime,
}

/// `GET /outsource-quotes/quotable-parts` 行。
///
/// 一零件一行（2026-10-03 简化前是「零件 × OUTSOURCE 工序」，`shelf_*` /
/// `next_process_*` 四列随之删除，见 `repo/sql.rs::OutsourceQuotableRepo` 的头注释）。
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct OutsourceQuotableRow {
    pub id: i64,
    pub serial_no: Option<String>,
    pub drawing_no: String,
    pub name: String,
    pub is_urgent: bool,
    pub unit_price: String,
    pub customer_id: i64,
    pub customer_name: Option<String>,
    pub parent_customer_name: Option<String>,
}

/// `GET /outsource-sendable` 行。
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct OutsourceSendableRow {
    /// `t_part_batch.version`（批次级 OCC）。
    pub batch_version: i32,
    pub batch_id: i64,
    pub batch_no: i32,
    pub batch_quantity: i32,
    /// `t_part_batch.status`（`PENDING` / `IN_PROCESS`）。
    pub source_status: String,
    pub part_id: i64,
    pub part_serial_no: Option<String>,
    pub part_drawing_no: Option<String>,
    pub part_name: Option<String>,
    /// `t_part.planned_delivery_date`（Postgres `date` → 文本）。
    pub planned_delivery_date: Option<String>,
    pub is_urgent: bool,
    pub customer_name: Option<String>,
    pub parent_customer_name: Option<String>,
    /// **只服务 SQL 层的过滤，Rust 侧不消费**：`OutsourceSendableRepo::list` 的
    /// 外层 `WHERE ($2::bigint IS NULL OR d.customer_id = $2)` 要投影出这一列才能
    /// 引用它。VO 不暴露客户 id（前端只拿 `customer_path`），故 `service/sendable.rs`
    /// 从不读这个字段 —— 保留投影是为了让 list / count 的过滤位置保持同构（见
    /// `repo/sql.rs::OutsourceSendableRepo` 头注释）。
    pub customer_id: Option<i64>,
    /// `t_shelf.code`（`LEFT JOIN t_shelf`）⇒ `PENDING` 且未上架的批次（无
    /// holder）为 `None`，VO 相应可空。
    pub shelf_code: Option<String>,
    /// `t_part_batch.current_process_id`（池归属权威依据），`JOIN t_process pr`
    /// （INNER）⇒ DB 层 NOT NULL。
    pub current_process_id: i64,
    /// `t_process.name`，同一 INNER JOIN ⇒ DB 层 NOT NULL。
    pub current_process_name: String,
    /// `t_process.requires_approval` —— `send_mode` 判定的输入（false = 免审批
    /// 直发 / true = 必须有已审批报价）。见 `service/sendable.rs::send_mode_of`。
    pub requires_approval: bool,
    pub quote_id: Option<i64>,
    pub price: Option<String>,
    pub outsource_company_id: Option<i64>,
    pub outsource_company_name: Option<String>,
    /// SQL `to_jsonb(array_agg(json_build_object(...)))` 的结果（单个 JSONB 值，
    /// 不是 `json[]` —— 后者 sqlx 解不进 `serde_json::Value`）。
    /// APPROVAL 行恒为 `[]`（SQL 侧 CASE 短路）。
    pub company_options: serde_json::Value,
}

/// `GET /outsource-pool/{process_id}` 的 `companies[]` 单条：某工序下的一家
/// 外协公司 + 它在该工序的外协批次持有数。
///
/// 2026-10-03 新增。**无在途批次的公司也要出现**（`held_count = 0`）——
/// 前端看板要渲染空公司列当拖拽目标。
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct OutsourcePoolCompanyRow {
    pub company_id: i64,
    pub name: String,
    /// `COUNT(pb.id)::bigint`（LEFT JOIN ⇒ 无在途批次时为 0，不是 NULL）。
    pub held_count: i64,
}

/// `GET /outsource-pool/state` 的 `items[]` 单条：某公司在某工序在外协的一个批次。
///
/// 2026-10-03 新增。全部字段由 `OutsourcePoolRepo::list_held` **一条 SQL** 解析
/// 完毕（公司 / 工序 / 客户路径 / shipment 字段 / 下一道工序都在 SQL 内 JOIN
/// 或 LATERAL 子查询里），service 层零回查。
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct OutsourceHeldBatchRow {
    pub batch_id: i64,
    pub part_id: i64,
    pub batch_no: i32,
    /// **当前余量**（`t_part_batch.quantity`，不是 shipment.quantity）——
    /// 前端拿它做「部分接收」输入框的 max 值（口径与
    /// `GET /outsource-shipments/in-flight` 一致）。
    pub quantity: i32,
    pub serial_no: Option<String>,
    pub drawing_no: String,
    pub name: String,
    pub system_delivery_date: Option<chrono::NaiveDate>,
    pub planned_delivery_date: Option<chrono::NaiveDate>,
    pub is_urgent: bool,
    /// L2 叶子客户名。
    pub customer_name: Option<String>,
    /// L1 一级集团名。
    pub parent_customer_name: Option<String>,
    /// `t_part.applicant_name` LEFT JOIN `t_applicant.name`（非 FK，字符串匹配）。
    pub applicant_name: Option<String>,
    /// `t_part_batch.location`，恒为 `"OUTSOURCE_COMPANY"`。
    pub batch_location: String,
    /// 工单级备注（`t_part.note`；DB 无 batch 级 remark 字段）。
    pub note: Option<String>,
    /// `t_part_batch.version`（**不是** shipment.version）—— 前端拿它当
    /// `receive-from-outsource` 的 OCC 锚。
    pub batch_version: i32,
    pub sent_at: Option<chrono::NaiveDateTime>,
    /// `t_outsource_shipment.unit_price::text`（Decimal 字符串）。
    pub price: Option<String>,
    /// 下一道工序 id；`COALESCE(..., 0)` ⇒ 无下一 step 时为 **0**
    /// （沿 `prod::queue::vo::queue::PendingBatchItem.current_process_step_id`
    /// 的 0 兜底口径：JSON 里非 nullable，语义为字符串 `"0"` = 未设）。
    pub receive_next_process_id: i64,
    pub receive_next_process_name: Option<String>,
}

/// outsource 域数据访问胖 trait。
///
/// 单 trait 合并 6 ZST（company + company_process + quote + quote_event + shipment，
/// 外加 2026-10-03 新增的 quotable / sendable / pool 三个读模型 ZST），共 57 方法：
/// company 8 + company_process 4 + quote 13 + quote_event 1 + shipment 9
/// + quotable 2 + sendable 3 + pool 4 + 跨域 helper 13。
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

    // ── t_outsource_quote（13）──
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

    // ── t_outsource_shipment（9）──
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
    async fn shipment_count_for_company<'a>(
        &mut self,
        company_id: i64,
        part_ids_in: &'a [i64],
        sent_from: Option<NaiveDateTime>,
        sent_to: Option<NaiveDateTime>,
        received_from: Option<NaiveDateTime>,
        received_to: Option<NaiveDateTime>,
    ) -> Result<i64, sqlx::Error>;
    /// 2026-10-03 新增：对账页 list（与 `shipment_count_for_company` 同 WHERE 口径）。
    #[allow(clippy::too_many_arguments)]
    async fn shipment_list_for_company<'a>(
        &mut self,
        company_id: i64,
        part_ids_in: &'a [i64],
        sent_from: Option<NaiveDateTime>,
        sent_to: Option<NaiveDateTime>,
        received_from: Option<NaiveDateTime>,
        received_to: Option<NaiveDateTime>,
        sort_by: &'a str,
        sort_dir: &'a str,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<OutsourceSentPartRow>, sqlx::Error>;
    /// 2026-10-03 新增：外协在途批次 list。
    async fn shipment_list_in_flight<'a>(
        &mut self,
        keyword_pat: Option<&'a str>,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<OutsourceInFlightRow>, sqlx::Error>;
    async fn shipment_count_in_flight<'a>(
        &mut self,
        keyword_pat: Option<&'a str>,
    ) -> Result<i64, sqlx::Error>;

    // ── quotable-parts（2026-10-03 新增，可建报价的未下发零件，一零件一行） ──
    async fn quotable_list<'a>(
        &mut self,
        keyword_pat: Option<&'a str>,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<OutsourceQuotableRow>, sqlx::Error>;
    async fn quotable_count<'a>(
        &mut self,
        keyword_pat: Option<&'a str>,
    ) -> Result<i64, sqlx::Error>;

    // ── sendable（2026-10-03 新增，可发送外协的活跃批次，一批次一行） ──
    async fn sendable_list<'a>(
        &mut self,
        keyword_pat: Option<&'a str>,
        customer_id: Option<i64>,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<OutsourceSendableRow>, sqlx::Error>;
    async fn sendable_count<'a>(
        &mut self,
        keyword_pat: Option<&'a str>,
        customer_id: Option<i64>,
    ) -> Result<i64, sqlx::Error>;
    /// 2026-10-03 新增：按外协工序取可发送候选（看板左列）。
    /// 与 `sendable_list` 同核心 SQL、只把外层过滤换成 `current_process_id = $1`。
    async fn sendable_list_by_process(
        &mut self,
        process_id: i64,
    ) -> Result<Vec<OutsourceSendableRow>, sqlx::Error>;

    // ── pool（2026-10-03 新增，按外协工序切 tab 的看板三端点） ──
    /// 候选侧按工序分组计数（`GROUP BY current_process_id`，与
    /// `sendable_list_by_process` 行粒度一致）。
    async fn pool_group_sendable_counts(&mut self) -> Result<Vec<(i64, i64)>, sqlx::Error>;
    /// 在途侧按工序分组计数（`GROUP BY current_process_id`）。
    async fn pool_group_in_flight_counts(&mut self) -> Result<Vec<(i64, i64)>, sqlx::Error>;
    /// 该工序映射的全部活跃外协公司 + 各自的在外协批次数（LEFT JOIN ⇒ 0 也返回）。
    async fn pool_list_companies_with_held(
        &mut self,
        process_id: i64,
    ) -> Result<Vec<OutsourcePoolCompanyRow>, sqlx::Error>;
    /// 某公司在某工序在外协的全部批次（一条 SQL 解析完全部展示 / 派生字段）。
    async fn pool_list_held(
        &mut self,
        company_id: i64,
        process_id: i64,
    ) -> Result<Vec<OutsourceHeldBatchRow>, sqlx::Error>;

    // ── 跨域 helper（service 散落的 inline SQL 抽 trait） ──
    // 2026-09-22 refactor：service 内的 inline SQL（`t_part` / `t_process` / `t_part_batch`
    // 跨域 SELECT）下沉为 trait 方法，避免 service 需要 `&mut PgConnection` 二次借用。
    // 与 com/customer 的 `lookup_names` / `count_parts_using_customer` 同形。

    /// `t_part` 按 id 查存在性（仅未软删）。供 create_quote 校验 part_id。
    async fn part_exists(&mut self, part_id: i64) -> Result<bool, sqlx::Error>;
    /// `t_part` 按关键字模糊搜（drawing_no OR name）。供 list_quotes 关键字过滤。
    async fn part_keyword_search<'a>(&mut self, keyword: &'a str) -> Result<Vec<i64>, sqlx::Error>;
    /// `t_part` 按客户子树取零件 id（2026-10-04 新增）。供 list_quotes 的
    /// `customer_id` 过滤展开。
    ///
    /// 谓词形状与 `OutsourceSendableRepo` 的 `customer_id` 谓词**逐字同形**
    /// （`customer_id = $1 OR customer_id IN (直接子客户)`，反向引用见
    /// `repo/sql.rs::SENDABLE_CUSTOMER_SUBTREE_PREDICATE` 的注释）：实测
    /// `t_part.customer_id` 指向的都是叶子客户，前端选的常是 L1，只判等值时 L1 必然
    /// 零命中。**等值那一支保留** ⇒ 传 L2 id 的行为与展开前一致。
    ///
    /// 「展开一层即完整」是 2026-10-04 生产库实测结论（零件全挂 L2、L3 数量 0），
    /// **API 层不强制**（`create_customer` 不校验 `parent_id` 是否指向根客户）；出现
    /// L3 后本谓词需改成递归 CTE，且漏报**是静默的**（`total` 偏小、不报错）。
    ///
    /// **软删节点行为不对称**（2026-10-04 review 第 1 轮登记，不是 bug，别反复查）：
    /// 传一个已软删的 L2 id 时等值那一支不过滤 `deleted_at`，其零件照样命中；而传它
    /// 的父客户时该软删 L2 被子查询的 `c2.deleted_at IS NULL` 排除 ⇒ 同一批零件
    /// 「按自己查得到、按父亲查不到」。零件可见性不受客户 ACL 约束故不是权限漏洞，
    /// 业务上客户一旦被引用就被 `BIZ_CUSTOMER_IN_USE` 挡住软删，几乎不可达。
    ///
    /// **`LIMIT 10000` 的依据与已知取舍**（2026-10-04 review 第 1 轮登记）：这个数字
    /// 是从 `part_keyword_search` 抄来的（那边要的是「万级关键词结果求交」，两者在
    /// service 层求交），**与「某客户子树的零件数」没有因果关系**，纯形式一致。实际
    /// 余量：全库 `t_part` 1874 行（2026-10-04 实测），所以任何单棵子树的规模上界就是
    /// 全库 1874 ⇒ **最坏情况余量也有 5 倍以上**（10000 / 1874 ≈ 5.3），实测中单棵
    /// 子树只是全库的一个零头，今天不可能截断。但 ⚠️ 本查询**没有 `ORDER BY`** ⇒ 一旦
    /// 真的触顶，返回的是**任意 10000 条**（非确定性子集、同一请求两次可能不同），
    /// `total` 偏小且零命中守卫不触发 ⇒ 静默少报。已知取舍，本轮不改成 count+warn、
    /// 不加 `ORDER BY`（属计划外改动）。
    async fn part_ids_by_customer(&mut self, customer_id: i64) -> Result<Vec<i64>, sqlx::Error>;
    /// `t_process` 按 id 查 category。供 create_quote 校验 OUTSOURCE 类别。
    async fn process_get_category(
        &mut self,
        process_id: i64,
    ) -> Result<Option<String>, sqlx::Error>;
    /// `t_process` 按 id 查 `requires_approval`（仅未软删）。
    ///
    /// 2026-10-03 新增：供 `prod::batch::send_to_outsource` 写侧守「需审批的工序
    /// 不许 `direct=true` 直发」——该列此前只有读侧（sendable / pool 的判定 SQL）
    /// 在用，写侧零校验 ⇒ 绕过 UI 直接调 API 就能对需审批工序直发。读法与
    /// `process_get_category` 同形（同表、同 `deleted_at IS NULL`、返回 `Option`
    /// 让调用方自己决定「不存在」怎么处理）。
    ///
    /// **为何不并进 `process_get_category`（2026-10-03 review 第 2 轮登记）**：两者读
    /// 的是 `t_process` 同一行的相邻两列，合到一个 `process_get_flag_row` 里确实能
    /// 少一次往返。但代价是回归面从 outsource 域扩到全部 `process_get_category`
    /// 调用方（`outsource/service/quote.rs` 的建报价校验、`prod::batch::
    /// send_to_outsource` 的类别校验），且返回类型要从 `Option<String>` 变成一个
    /// 两字段结构体 —— 收益（省一次同表主键查询）远小于改面。故本轮保持独立，等真有
    /// 第三个同表 flag 列时再合并。
    async fn process_get_requires_approval(
        &mut self,
        process_id: i64,
    ) -> Result<Option<bool>, sqlx::Error>;
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
    /// `t_part` 按 ids 查
    /// `(id, serial_no, drawing_no, name, is_urgent, unit_price::text, customer_name, l1_customer_name)`。
    /// 供 quote_out_many 拼装 part 显示字段。
    ///
    /// 2026-10-03 扩 2 列：L2 / L1 客户名（`t_customer` ⋈ 自引用），供 service 拼
    /// `customer_path`（此前 `OutsourceQuoteOut.customer_path` 恒 `None` → 前端报价
    /// 一览「客户」列全 `—`）。
    #[allow(clippy::type_complexity)]
    async fn part_map_for_quote<'a>(
        &mut self,
        part_ids: &'a [i64],
    ) -> Result<
        Vec<(
            i64,
            Option<String>,
            String,
            String,
            bool,
            Option<String>,
            Option<String>,
            Option<String>,
        )>,
        sqlx::Error,
    >;
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
    /// `t_part` 按 id 查 `(L2 客户名, L1 客户名)`，供 `shipment_out` 真算
    /// `customer_path`。两条 JOIN 与 list 侧 `OutsourceSentPartRow` 的
    /// `t_customer` / `t_customer.parent_id` 逐条一致（各自 `deleted_at IS NULL`
    /// 才给名，part 软删不影响取名 —— list 侧是 `LEFT JOIN t_part`）。
    async fn part_customer_names(
        &mut self,
        part_id: i64,
    ) -> Result<(Option<String>, Option<String>), sqlx::Error>;
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

    // ── t_outsource_quote（13）── 一行委托 sql::OutsourceQuoteRepo ─────────
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
        OutsourceQuoteRepo::list_active_by_part_process(
            &mut **self,
            part_id,
            process_id,
            exclude_id,
        )
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

    // ── t_outsource_shipment（9）── 一行委托 sql::OutsourceShipmentRepo ─────
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
    async fn shipment_count_for_company<'b>(
        &mut self,
        company_id: i64,
        part_ids_in: &'b [i64],
        sent_from: Option<NaiveDateTime>,
        sent_to: Option<NaiveDateTime>,
        received_from: Option<NaiveDateTime>,
        received_to: Option<NaiveDateTime>,
    ) -> Result<i64, sqlx::Error> {
        OutsourceShipmentRepo::count_for_company(
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

    #[allow(clippy::too_many_arguments)]
    async fn shipment_list_for_company<'b>(
        &mut self,
        company_id: i64,
        part_ids_in: &'b [i64],
        sent_from: Option<NaiveDateTime>,
        sent_to: Option<NaiveDateTime>,
        received_from: Option<NaiveDateTime>,
        received_to: Option<NaiveDateTime>,
        sort_by: &'b str,
        sort_dir: &'b str,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<OutsourceSentPartRow>, sqlx::Error> {
        OutsourceShipmentRepo::list_for_company(
            &mut **self,
            company_id,
            part_ids_in,
            sent_from,
            sent_to,
            received_from,
            received_to,
            sort_by,
            sort_dir,
            limit,
            offset,
        )
        .await
    }

    async fn shipment_list_in_flight<'a>(
        &mut self,
        keyword_pat: Option<&'a str>,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<OutsourceInFlightRow>, sqlx::Error> {
        OutsourceShipmentRepo::list_in_flight(&mut **self, keyword_pat, limit, offset).await
    }

    async fn shipment_count_in_flight<'a>(
        &mut self,
        keyword_pat: Option<&'a str>,
    ) -> Result<i64, sqlx::Error> {
        OutsourceShipmentRepo::count_in_flight(&mut **self, keyword_pat).await
    }

    // ── quotable-parts（2）── 一行委托 sql::OutsourceQuotableRepo ─────────
    async fn quotable_list<'a>(
        &mut self,
        keyword_pat: Option<&'a str>,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<OutsourceQuotableRow>, sqlx::Error> {
        OutsourceQuotableRepo::list(&mut **self, keyword_pat, limit, offset).await
    }

    async fn quotable_count<'a>(
        &mut self,
        keyword_pat: Option<&'a str>,
    ) -> Result<i64, sqlx::Error> {
        OutsourceQuotableRepo::count(&mut **self, keyword_pat).await
    }

    // ── sendable（3）── 一行委托 sql::OutsourceSendableRepo ───────────────
    async fn sendable_list<'a>(
        &mut self,
        keyword_pat: Option<&'a str>,
        customer_id: Option<i64>,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<OutsourceSendableRow>, sqlx::Error> {
        OutsourceSendableRepo::list(&mut **self, keyword_pat, customer_id, limit, offset).await
    }

    async fn sendable_count<'a>(
        &mut self,
        keyword_pat: Option<&'a str>,
        customer_id: Option<i64>,
    ) -> Result<i64, sqlx::Error> {
        OutsourceSendableRepo::count(&mut **self, keyword_pat, customer_id).await
    }

    async fn sendable_list_by_process(
        &mut self,
        process_id: i64,
    ) -> Result<Vec<OutsourceSendableRow>, sqlx::Error> {
        OutsourceSendableRepo::list_by_process(&mut **self, process_id).await
    }

    // ── pool（4）── 一行委托 sql::OutsourcePoolRepo ──────────────────────
    async fn pool_group_sendable_counts(&mut self) -> Result<Vec<(i64, i64)>, sqlx::Error> {
        OutsourcePoolRepo::group_sendable_counts(&mut **self).await
    }

    async fn pool_group_in_flight_counts(&mut self) -> Result<Vec<(i64, i64)>, sqlx::Error> {
        OutsourcePoolRepo::group_in_flight_counts(&mut **self).await
    }

    async fn pool_list_companies_with_held(
        &mut self,
        process_id: i64,
    ) -> Result<Vec<OutsourcePoolCompanyRow>, sqlx::Error> {
        OutsourcePoolRepo::list_companies_with_held(&mut **self, process_id).await
    }

    async fn pool_list_held(
        &mut self,
        company_id: i64,
        process_id: i64,
    ) -> Result<Vec<OutsourceHeldBatchRow>, sqlx::Error> {
        OutsourcePoolRepo::list_held(&mut **self, company_id, process_id).await
    }

    // ── 跨域 helper（14）── 一行委托 `sqlx::query_as` 跨表 SELECT ─────────
    async fn part_exists(&mut self, part_id: i64) -> Result<bool, sqlx::Error> {
        let row: Option<(i64,)> =
            sqlx::query_as("SELECT id FROM t_part WHERE id = $1 AND deleted_at IS NULL")
                .bind(part_id)
                .fetch_optional(&mut **self)
                .await?;
        Ok(row.is_some())
    }

    async fn part_keyword_search<'b>(&mut self, keyword: &'b str) -> Result<Vec<i64>, sqlx::Error> {
        let rows: Vec<(i64,)> = sqlx::query_as(
            "SELECT id FROM t_part WHERE deleted_at IS NULL AND \
             (drawing_no ILIKE $1 OR name ILIKE $1) LIMIT 10000",
        )
        .bind(format!("%{}%", keyword))
        .fetch_all(&mut **self)
        .await?;
        Ok(rows.into_iter().map(|r| r.0).collect())
    }

    // 2026-10-04 review 第 1 轮：谓词与 `LIMIT 10000` 的依据（含「无 ORDER BY ⇒
    // 触顶时静默返回非确定性子集」这个已知取舍）统一写在 trait 处同名方法的 doc 上，
    // 避免两份拷贝各自漂移。此处只放字面量，不重复论证。
    async fn part_ids_by_customer(&mut self, customer_id: i64) -> Result<Vec<i64>, sqlx::Error> {
        let rows: Vec<(i64,)> = sqlx::query_as(
            "SELECT id FROM t_part WHERE deleted_at IS NULL \
             AND (customer_id = $1 \
                  OR customer_id IN (SELECT c2.id FROM t_customer c2 \
                                     WHERE c2.parent_id = $1 AND c2.deleted_at IS NULL)) \
             LIMIT 10000",
        )
        .bind(customer_id)
        .fetch_all(&mut **self)
        .await?;
        Ok(rows.into_iter().map(|r| r.0).collect())
    }

    async fn process_get_category(
        &mut self,
        process_id: i64,
    ) -> Result<Option<String>, sqlx::Error> {
        let row: Option<(String,)> =
            sqlx::query_as("SELECT category FROM t_process WHERE id = $1 AND deleted_at IS NULL")
                .bind(process_id)
                .fetch_optional(&mut **self)
                .await?;
        Ok(row.map(|r| r.0))
    }

    async fn process_get_requires_approval(
        &mut self,
        process_id: i64,
    ) -> Result<Option<bool>, sqlx::Error> {
        let row: Option<(bool,)> = sqlx::query_as(
            "SELECT requires_approval FROM t_process WHERE id = $1 AND deleted_at IS NULL",
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
    ) -> Result<
        Vec<(
            i64,
            Option<String>,
            String,
            String,
            bool,
            Option<String>,
            Option<String>,
            Option<String>,
        )>,
        sqlx::Error,
    > {
        sqlx::query_as(
            "SELECT p.id, p.serial_no, p.drawing_no, p.name, p.is_urgent, p.unit_price::text, \
                    c.name, cp.name \
             FROM t_part p \
             LEFT JOIN t_customer c ON c.id = p.customer_id AND c.deleted_at IS NULL \
             LEFT JOIN t_customer cp ON cp.id = c.parent_id AND cp.deleted_at IS NULL \
             WHERE p.id = ANY($1) AND p.deleted_at IS NULL",
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

    async fn part_customer_names(
        &mut self,
        part_id: i64,
    ) -> Result<(Option<String>, Option<String>), sqlx::Error> {
        // LEFT JOIN 出 L2 / L1 两个可空名（都可能是 NULL：未挂客户 / 客户已软删）；
        // part 行不存在时 fetch_optional 返 None，塌成 (None, None)。
        let row: Option<(Option<String>, Option<String>)> = sqlx::query_as(
            "SELECT c.name, cp.name \
             FROM t_part p \
             LEFT JOIN t_customer c ON c.id = p.customer_id AND c.deleted_at IS NULL \
             LEFT JOIN t_customer cp ON cp.id = c.parent_id AND cp.deleted_at IS NULL \
             WHERE p.id = $1",
        )
        .bind(part_id)
        .fetch_optional(&mut **self)
        .await?;
        Ok(row.unwrap_or((None, None)))
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

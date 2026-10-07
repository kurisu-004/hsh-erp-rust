//! outsource service 层入口
//!
//! 按职责拆为：
//! - `company`  — 外协公司 CRUD + 工序映射（list / create / get / update（含工序映射
//!   整体替换）/ soft-delete / by-process）
//! - `quote`    — 报价状态机（list / create / submit / approve / reject / soft-delete）
//!   + `quotable-parts` picker
//! - `shipment` — 对账单更新（reconcile-update）+ 对账页 sent-parts + 在途 in-flight
//! - `move`     — `POST /outsource-queue/move`（三合一移动写端点，取代 `prod::batch`
//!   的外协收发三个单边端点）
//!
//! 外协看板两个只读端点（`/outsource-queue/snapshot` +
//! `/outsource-queue/processes/{id}`）**不走本层**，实现见 `super::board`（与其 repo /
//! VO 一并成子模块，范本 `prod/queue/board`）：它是纯只读聚合，固定 SQL 条数要能被
//! 源码级护栏单独圈住，与走胖 trait 的 CRUD 写路径混在一个目录里就圈不出来了。
//! 移动写端点同样不进 `board/`（它不是聚合读），但也不挂 `OutsourceService`：它收
//! `&mut PgConnection` 而非胖 trait，形如 ZST（范本 `prod::queue::QueueService`）。
//!
//! `sendable` 子模块（`GET /outsource-sendable` 的实现）已于 2026-10-09 删除；留在
//! 该目录的三个纯函数 `send_mode_of` / `can_send_of` / `decode_company_options` 是
//! 看板候选列与旧端点共用的判定真源，**仍归本模块**（`pub(crate)`）—— 详见
//! `service/sendable.rs` 的文件头。
//!
//! 对外 API（`handler.rs` 调用面）保持原方法名（`OutsourceService::xxx`），handler 通过
//! `crate::modules::outsource::service::OutsourceService` 引用。
//!
//! 2026-09-22 refactor（outsource 对齐 iam 事务分层范式）：
//! - 原单文件 `service.rs`（1273 行超 1000 行上限）拆为 4 文件
//! - 方法签名全部改为 `<R: OutsourceRepoTrait>(&self, mut repo: R, ...)`（by-value；
//!   trait 已直接 `impl for &mut PgConnection`），handler 借 `&mut *tx` / `&mut *conn`
//!   喂给 trait 即可。
//! - 事务移交 handler：service 不知事务——handler `pool.begin()` + `tx.commit()` 包外。
//! - `OutsourceService` 字段仅 `Arc<SnowflakeIdGenerator>`（无 Redis session / WS 等
//!   post-commit 副作用需求；与 iam AccountService / com CustomerService 同形）。
//!
//! 2026-10-03 新增（读侧补齐）：4 个 list 端点上线上，见各子模块头注释。
//!
//! 2026-10-09 删除两个子模块：`pool`（三条 `/outsource-pool/*` 旧读被 `super::board`
//! 的看板两读取代，`OutsourceService::pool_counts` / `pool_by_process` / `pool_state`
//! 随之删除）与 `sendable`（`GET /outsource-sendable` 的 list 端点被看板候选列取代，
//! `OutsourceService::list_sendable` 删除；三个共用纯函数保留在本目录）。
//!
//! 同日公司 / 报价两域收敛（见 `handler.rs` 路由表）：
//! - `set_company_processes` 删除（功能吸收进 `update_company` 的 `process_ids`）；
//! - `get_quote` / `update_quote` 删除（对应端点硬切下线，前端零消费）；
//! - `submit_quote` / `soft_delete_quote` / `soft_delete_company` 改为收**调用方传的**
//!   `version`，不再用 service 自己刚读到的值自守乐观锁；
//! - 新增共享 helper `parse_optional_snowflake`（可选雪花 ID query 形参 → `Option<i64>`）。

#![allow(
    clippy::collapsible_if,
    clippy::type_complexity,
    clippy::too_many_arguments,
    unused_imports,
    unused_variables,
    unused_mut,
    deprecated
)]

use std::sync::Arc;

use rust_decimal::Decimal;

use crate::infra::snowflake::SnowflakeIdGenerator;
use crate::shared::error::{AppError, code};

mod company;
#[path = "move.rs"]
pub mod move_svc;
mod quote;
pub(crate) mod sendable;
mod shipment;

pub use move_svc::OutsourceMoveService;

const DEFAULT_LIMIT: i64 = 50;
const MAX_LIMIT: i64 = 500;

/// 2026-10-03 新增：4 个新 list 端点统一用 `clamp(1, 200)`（对齐 part 域旧
/// `list_outsource_in_flight` / `list_outsource_sendable` 的分页上限）。
const LIST_MAX_LIMIT: i64 = 200;

/// 拼客户路径：有 L1 给 `L1 / L2`，无 L1 时只给 L2 名，两侧都缺返回 `None`。
///
/// 「无 L1」含两种成因：`parent_id IS NULL`，或 L1 客户自身 `deleted_at IS NOT NULL`
/// （取名的 LEFT JOIN 带软删过滤）。**不含自指** —— `t_customer` 有
/// `ck_t_customer_no_self_parent` CHECK，`parent_id = id` 不可能。
///
/// 2026-10-03 起 outsource 域全部 VO 的 `customer_path` 都走本函数。范式抄
/// `prod::queue::repo::sql.rs`（`CandidateRow → PoolBatchItem`）。
pub(crate) fn join_customer_path(l1: Option<&str>, l2: Option<&str>) -> Option<String> {
    match (l1, l2) {
        (Some(p), Some(l)) => Some(format!("{p} / {l}")),
        (_, Some(l)) => Some(l.to_string()),
        _ => None,
    }
}

/// 把 keyword 归一化成 `ILIKE` 通配串；trim 后为空视为无过滤。
pub(crate) fn keyword_pattern(keyword: Option<&str>) -> Option<String> {
    keyword
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| format!("%{s}%"))
}

fn not_found_company(id: i64) -> AppError {
    AppError::biz(
        code::BIZ_OUTSOURCE_COMPANY_NOT_FOUND,
        format!("outsource company {id} not found"),
    )
}

fn not_found_quote(id: i64) -> AppError {
    AppError::biz(
        code::BIZ_OUTSOURCE_QUOTE_NOT_FOUND,
        format!("outsource quote {id} not found"),
    )
}

fn not_found_shipment(id: i64) -> AppError {
    AppError::biz(
        code::BIZ_OUTSOURCE_SHIPMENT_NOT_FOUND,
        format!("outsource shipment {id} not found"),
    )
}

fn version_conflict() -> AppError {
    AppError::biz(code::VERSION_CONFLICT, "数据已被他人修改，请刷新后重试")
}

/// 把字符串价格（DB 端 `Numeric(12,2)`）解析为 Decimal。
pub(crate) fn parse_price(s: &str) -> Result<Decimal, AppError> {
    s.trim().parse::<Decimal>().map_err(|_| {
        AppError::biz(
            code::BIZ_INVALID_VALUE,
            format!("price 不是合法的数字: {s:?}"),
        )
    })
}

/// 把 Decimal 序列化为保留 2 位小数的字符串（前端显示）。
pub(crate) fn format_price(d: &Decimal) -> String {
    format!("{:.2}", d)
}

/// 把 `i64` 字符串解析为雪花 ID i64；解析失败返回 BIZ_INVALID_VALUE 400。
///
/// 2026-09-22：原 `super::parse_snowflake_id`（自由函数），拆 service 时下放为
/// `pub(crate)` 共享 helper（company / quote 子模块都用）。
pub(crate) fn parse_snowflake_id(s: &str, field: &str) -> Result<i64, AppError> {
    let t = s.trim();
    if t.is_empty() {
        return Err(AppError::biz(
            code::BIZ_INVALID_VALUE,
            format!("{field} 不能为空"),
        ));
    }
    t.parse::<i64>().map_err(|_| {
        AppError::biz(
            code::BIZ_INVALID_VALUE,
            format!("{field} 不是合法雪花 ID: {s:?}"),
        )
    })
}

/// 可选的雪花 ID query 形参 → `Option<i64>`：`None` / 全空白 ⇒ `None`（不过滤）。
///
/// 与 [`parse_snowflake_id`] 的区别只在于**缺省不是错**：列表端点的可选筛选维度
/// 靠 `None` 表达「不过滤」，空串（前端把搜索框清空后序列化出来的形态）与 `None`
/// 同义。
pub(crate) fn parse_optional_snowflake(
    raw: Option<&str>,
    field: &str,
) -> Result<Option<i64>, AppError> {
    match raw.map(str::trim).filter(|s| !s.is_empty()) {
        None => Ok(None),
        Some(t) => t
            .parse::<i64>()
            .map(Some)
            .map_err(|_| AppError::biz(code::BIZ_INVALID_VALUE, format!("{field} 非整数"))),
    }
}

/// 2026-09-14 Phase 3 follow-up（current_id_to_snowflake 修复）：
/// 事件 id 直接用传入的 `SnowflakeIdGenerator::next_id()`。
/// 真实雪花 id 保证全局唯一，避免并发场景下 `current.id ^ 时间戳` 近似 id 的撞 id 风险。
pub(crate) fn current_id_to_snowflake(snowflake: &SnowflakeIdGenerator) -> i64 {
    snowflake.next_id()
}

/// outsource 域 service（2026-09-22 refactor 对齐 iam 事务分层范式）
///
/// 字段仅 `snowflake`（事务已移交 handler；service 不知事务）。实例为轻壳，
/// 可直接 `Arc<OutsourceService>` 存 `AppState`；方法签名收 `mut repo: R`
/// （by-value；生产 `R = &mut PgConnection`，单测 `R = MockOutsourceRepo`），
/// 单测用 `MockOutsourceRepo` 直接注入。
///
/// impl 块分布在 `company` / `quote` / `shipment` 三个子模块（Rust 允许多文件共同 impl
/// 同一 struct），每个 trait 扩展子模块注入本子域方法，让单文件行数保持在 800 行内。
pub struct OutsourceService {
    snowflake: Arc<SnowflakeIdGenerator>,
}

impl OutsourceService {
    /// 构造：仅需雪花 ID 生成器。
    pub fn new(snowflake: Arc<SnowflakeIdGenerator>) -> Self {
        Self { snowflake }
    }
}

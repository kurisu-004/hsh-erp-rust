//! outsource service 层入口
//!
//! 按职责拆为：
//! - `company` — 外协公司 CRUD + 工序映射（list / create / get / update / soft-delete /
//!   by-process / set-processes）
//! - `quote`   — 报价 CRUD + 状态机（list / create / get / update / submit / approve /
//!   reject / soft-delete）
//! - `shipment`— 对账单更新（reconcile-update）
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
mod quote;
mod shipment;

const DEFAULT_LIMIT: i64 = 50;
const MAX_LIMIT: i64 = 500;

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

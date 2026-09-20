//! process_chain 域数据模型
//!
//! 对应表（migration 017；026 起 FK 方向翻转）：
//! - `t_part_process_chain` —— 工艺链 header（与 part 1:1；2026-09-16 起由
//!   `t_part.process_chain_id` 指向本表，本表不再持有 `part_id` 列）
//! - `t_process_chain_step` —— 多步子表
//!
//! 行结构 + builder pattern：
//! - `TPartProcessChain` / `TProcessChainStep` —— sqlx FromRow
//! - `NewProcessChainStep` —— service 层构造 INSERT 输入

use chrono::NaiveDateTime;

/// `t_part_process_chain` 行
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct TPartProcessChain {
    pub id: i64,
    #[allow(dead_code)]
    pub name: String,
    pub version: i32,
    #[allow(dead_code)]
    pub note: Option<String>,
    #[allow(dead_code)]
    pub created_at: NaiveDateTime,
    #[allow(dead_code)]
    pub created_by: Option<i64>,
    #[allow(dead_code)]
    pub updated_at: NaiveDateTime,
    #[allow(dead_code)]
    pub updated_by: Option<i64>,
    #[allow(dead_code)]
    pub deleted_at: Option<NaiveDateTime>,
}

/// `t_process_chain_step` 行
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct TProcessChainStep {
    pub id: i64,
    #[allow(dead_code)]
    pub chain_id: i64,
    #[allow(dead_code)]
    pub sort_order: i32,
    #[allow(dead_code)]
    pub process_id: i64,
    #[allow(dead_code)]
    pub estimated_minutes: i32,
    /// 单步备注（车间操作员参考，如"必须干燥 24h 后才能上 CNC"）。NULL = 无备注。
    #[allow(dead_code)]
    pub note: Option<String>,
    pub version: i32,
    #[allow(dead_code)]
    pub created_at: NaiveDateTime,
    #[allow(dead_code)]
    pub created_by: Option<i64>,
    #[allow(dead_code)]
    pub updated_at: NaiveDateTime,
    #[allow(dead_code)]
    pub updated_by: Option<i64>,
    #[allow(dead_code)]
    pub deleted_at: Option<NaiveDateTime>,
}

/// 新 step 行 INSERT 输入（service 层构造）。
#[derive(Debug, Clone)]
pub struct NewProcessChainStep {
    pub sort_order: i32,
    pub process_id: i64,
    pub estimated_minutes: i32,
    /// 单步备注；空串视作 None（service 层 trim）。
    pub note: Option<String>,
}

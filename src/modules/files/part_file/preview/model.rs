//! `files::part_file::preview` 的表行模型。
//!
//! 本表（`t_file_preview`）**全仓只有本子模块读写**（见 `migrations/` 里建表注释的
//! 「为什么独立成表」一节），故行结构也只在本子模块内定义，不进 `part_file::model`
//! —— 后者按惯例只放 `t_part_file` 的 `TPartFile` 投影。

use chrono::NaiveDateTime;
use serde::Serialize;

use crate::shared::types::serialize_i64;

/// `t_file_preview` 全列行结构（**query! 宏直接映射**，无手写 `FromRow`）。
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct TFilePreview {
    #[serde(serialize_with = "serialize_i64")]
    pub id: i64,
    /// 内容寻址种子（32 位 hex）。⚠️ 纯哈希、绝不带文件名，见建表注释。
    pub preview_seed: String,
    pub src_object_key: String,
    /// python 侧 CI job id；`None` = 尚未提交成功。
    pub ci_job_id: Option<String>,
    /// `PENDING` / `READY` / `FAILED`（本仓无 DB ENUM）。
    pub state: String,
    pub page_count: i32,
    pub error_message: Option<String>,
    /// 惰性轮询节流戳；终态（READY / FAILED）为 `NULL`。
    pub next_poll_at: Option<NaiveDateTime>,
}

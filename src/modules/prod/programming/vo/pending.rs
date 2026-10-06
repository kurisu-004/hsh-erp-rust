//! `GET /api/v2/prod/programming/pending` 的出参（行项 + 顶层响应）。
//!
//! 域级 VO 契约（仅 Serialize / 禁入 extractor / 与前端 schema 对应 / 子文件划分
//! 取舍）见 [`super`] 模块 doc；此处只留逐字段口径。
//!
//! 2026-10-01 新增：与 `prod::batch` / `worker_pool` 同形 VO，仅 `Serialize`
//! 不 `Deserialize`。
//!
//! 2026-10-03 由 13 字段扩到 15：加 `batch_id` / `batch_version`。原因是本端点的唯一
//! 写出口 `POST /api/v2/prod/batches/{batch_id}/release-from-programming` **以批次
//! 为锚**（批次 id 走 URL path，入参 `PlaceOnShelfRequest.version` 又对
//! `t_part_batch.version` 做 OCC 校验），而列表行原来只给 part 级 id + part 级
//! `version`（`t_part.version`），前端根本拼不出这个请求。

use chrono::NaiveDate;
use serde::Serialize;

use crate::shared::types::{serialize_i64, serialize_i64_opt};

/// `GET /api/v2/prod/programming/pending` 单条结构。
///
/// 字段顺序按业务语义分组：工单标识 → 展示字段 → 交期 → 客户 → CNC 程序标记
/// → 批次锚点。
#[derive(Debug, Clone, Serialize)]
pub struct ProgrammingItemOut {
    #[serde(serialize_with = "serialize_i64")]
    pub id: i64,
    /// 乐观锁版本号（前端行内操作回传用）。
    pub version: i32,
    /// 工单序列号（手工工单可空）
    pub serial_no: Option<String>,
    pub name: String,
    pub drawing_no: String,
    pub quantity: i32,
    pub status: String,
    pub is_urgent: bool,
    /// 计划交期（`String` 而非 `NaiveDate`；NULL 走 `"1970-01-01"` 兜底，
    /// DB `NOT NULL` 已保证非空，此为防御性）。
    pub planned_delivery_date: String,
    /// 系统交期（`NaiveDate` 原生序列化；NULL → JSON `null`）。
    pub system_delivery_date: Option<NaiveDate>,
    /// L2 叶子客户名
    pub customer_name: Option<String>,
    /// L1 一级集团名；L2 未挂 parent 时为 None
    pub parent_customer_name: Option<String>,
    /// 是否已上传 G_CODE（真相源 `EXISTS t_part_file kind='G_CODE' AND
    /// deleted_at IS NULL`，与 part 域旧端点 / worker_pool 候选池同源）。
    pub has_cnc_program: bool,
    /// 2026-10-03 新增：该 part 的 **PROGRAMMING 活跃批次** 雪花 id（JSON string）。
    ///
    /// 取值口径（`repo.rs::PROGRAMMING_BATCH_JOIN`，与 [`Self::version`] 严格区分）：
    /// `status = 'PROGRAMMING' AND deleted_at IS NULL` 的批次中 `id` 最大者。
    /// 只认 PROGRAMMING 是因为 `release_from_programming` 硬要求源状态是 PROGRAMMING
    /// （`prod/batch/service/programming.rs` 的 `from != PartStatus::PROGRAMMING`
    /// 直接 20103）—— 给别的状态的批次 id 等于给前端一个必然失败的锚点。
    /// 多个 PROGRAMMING 批次时取 `id` 最大者（雪花 ID 随时间单调递增，即最新那个）。
    /// 无 PROGRAMMING 批次 → `None`：前端据此禁用「下发」按钮，语义正确。
    ///
    /// ⚠️ 本 VO 的 `version` 是 **part 级**（`t_part.version`），与批次 OCC 无关；
    /// 批次 OCC 只认 [`Self::batch_version`]。
    #[serde(serialize_with = "serialize_i64_opt")]
    pub batch_id: Option<i64>,
    /// 2026-10-03 新增：`batch_id` 那个批次的乐观锁版本号（`t_part_batch.version`），
    /// 供前端作 `release-from-programming` 的 OCC 版本回传。与 `batch_id` 同生共死
    /// （`batch_id = None` 时本字段也必为 `None`）。
    #[serde(default)]
    pub batch_version: Option<i32>,
}

/// `GET /api/v2/prod/programming/pending` 顶层响应。
///
/// ⚠️ `total` / `limit` / `offset` 是**分页计数类 `i64`，故意不走
/// `serialize_i64`**（2026-10-01 review 第 1 轮 C 项确认）：它们是行数 / 偏移量，
/// 远小于 `2^53`，不存在 JS 精度截断风险；序列化形态与
/// `part/vo/part.rs::PartListOut`（以及被替换的 part 域旧端点）**逐字一致**，
/// 前端从旧端点切到本端点时该层无需改动。只有雪花 ID 字段（`ProgrammingItemOut::id`）
/// 需要 `serialize_i64` → JSON string。
#[derive(Debug, Clone, Serialize)]
pub struct ProgrammingListOut {
    pub items: Vec<ProgrammingItemOut>,
    pub total: i64,
    pub limit: i64,
    pub offset: i64,
}

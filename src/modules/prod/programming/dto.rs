//! prod::programming 子模块 DTO —— 入参（Query string）
//!
//! 2026-10-01 新增：与 `prod::batch` / `worker_pool` 同形 DTO 模块，仅入参
//! （`Deserialize`）。出参结构见 [`super::vo`]。
//!
//! ## 反序列化兜底
//! - `limit` / `offset` 走 `crate::shared::types::deserialize_i64_opt` —— 前端可能
//!   发数字也可能发字符串，统一按字符串 `parse` 成 `i64`。
//! - `has_cnc_program` 走本文件私有 `deserialize_bool_opt` —— query string 里只
//!   有字面量 `true` / `false`，且前端可能发 `has_cnc_program=`（空串）表示
//!   「不筛选」，空串按缺省（None）处理而不是 422。

use serde::{Deserialize, Deserializer};

use crate::shared::types::deserialize_i64_opt;

/// `GET /api/v2/prod/programming/pending` Query 参数。
#[derive(Debug, Clone, Deserialize)]
pub struct ProgrammingListQuery {
    /// Tab 切换三态：`Some(true)` 仅已上传 G_CODE、`Some(false)` 仅未上传、
    /// `None`（缺省）全部。
    #[serde(default, deserialize_with = "deserialize_bool_opt")]
    pub has_cnc_program: Option<bool>,
    /// 模糊匹配 `name` / `drawing_no` / `serial_no`（`ILIKE '%kw%'`）。
    pub keyword: Option<String>,
    /// 工单序列号精确匹配（`p.serial_no = $n`）。
    pub serial_no: Option<String>,
    /// 排序列白名单：`CREATED_AT` / `UPDATED_AT` / `PLANNED_DELIVERY_DATE` /
    /// `REQUEST_DATE` / `SERIAL_NO` / `DRAWING_NO` / `NAME`；其它值退化为
    /// `PLANNED_DELIVERY_DATE`（**不报错**，见 repo 白名单兜底）。
    #[serde(default)]
    pub sort_by: Option<String>,
    /// `ASC` / `DESC`（缺省 `ASC`）；非 `DESC` 一律按 `ASC` 处理。
    #[serde(default)]
    pub sort_dir: Option<String>,
    #[serde(default, deserialize_with = "deserialize_i64_opt")]
    pub limit: Option<i64>,
    #[serde(default, deserialize_with = "deserialize_i64_opt")]
    pub offset: Option<i64>,
}

/// `has_cnc_program` 的 query 反序列化：缺省 / 空串 → `None`（不过滤）。
///
/// 非 `true` / `false` 的字面量 → 反序列化错误（axum `Query` 层 400）。
fn deserialize_bool_opt<'de, D: Deserializer<'de>>(d: D) -> Result<Option<bool>, D::Error> {
    let raw: Option<String> = Option::deserialize(d)?;
    match raw.as_deref().map(str::trim) {
        None | Some("") => Ok(None),
        Some(v) if v.eq_ignore_ascii_case("true") => Ok(Some(true)),
        Some(v) if v.eq_ignore_ascii_case("false") => Ok(Some(false)),
        Some(v) => Err(serde::de::Error::custom(format!(
            "has_cnc_program 必须是 true / false，收到：{v}"
        ))),
    }
}

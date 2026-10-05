//! prod::process_design 子模块 DTO —— 入参（Query string）
//!
//! 2026-10-05 新增：与 `prod::programming` / `prod::batch` 同形 DTO 模块，仅入参
//! （`Deserialize`）。出参结构见 [`super::vo`]。
//!
//! ## 刻意**不提供**的入参
//! - `status`：写死在 SQL 常量里（`PENDING` 是本页业务闸门，不是筛选旋钮）
//! - `keyword`：前端本地过滤，后端不接收
//! - `sort_by`：排序键固定 `serial_no`，没有第二个可选列
//! - `row_type` / `include_assemblies`：本端点存在的意义就是没有那道
//!   `AND assembly_id IS NULL` 守卫，把「要不要子件」做成开关等于把守卫换个地方藏
//!
//! ## 反序列化兜底
//! - `limit` / `offset` 走本文件私有 `deserialize_i64_opt_lenient` —— URL query 没有
//!   类型之分，数字一律以字符串到达，统一按字符串 `parse` 成 `i64`（**带引号的 `"50"`
//!   属非法字面量 → 400**）；**空串 / 全空白 / 数字两侧空白按缺省（None）** 处理
//!   （与 `prod::programming` 同口径），避免 `?limit=` 落到 axum `Query` 层 400 纯文本。
//!   真正的解析逻辑仍复用 `crate::shared::types::deserialize_i64`，两处 parse 语义
//!   不会漂移。
//!
//! 注：serde_urlencoded 0.7.1 的 `deserialize_option` 对空串同样 `visit_some`
//! （即 `?limit=` 会得到 `Some("")` 而非 `None`），故自定义反序列化器显式兜住
//! `Some("")` 这一路。

use serde::{Deserialize, Deserializer};

use crate::shared::types::deserialize_i64;

/// `GET /api/v2/prod/process-design/parts` Query 参数。
#[derive(Debug, Clone, Deserialize)]
pub struct ProcessDesignListQuery {
    /// `ASC` / `DESC`（缺省 `ASC`）；非 `DESC` 一律按 `ASC` 处理。
    #[serde(default)]
    pub sort_dir: Option<String>,
    #[serde(default, deserialize_with = "deserialize_i64_opt_lenient")]
    pub limit: Option<i64>,
    #[serde(default, deserialize_with = "deserialize_i64_opt_lenient")]
    pub offset: Option<i64>,
}

/// `limit` / `offset` 的 query 反序列化：缺省 / 空串 / 全空白 → `None`（走缺省值）。
///
/// 沿 `prod::programming` 的同名私有函数（2026-10-05 照抄）：URL query 没有类型之分，
/// 裸 `?limit=` 落到 axum `Query` 层会吐 400 **纯文本**（不走 `R` 包络），前端无从
/// 解析错误体。故空串 / 全空白按缺省处理。
///
/// 非空串交给 `shared::types::deserialize_i64` 解析（用
/// `serde::de::value::StringDeserializer` 把已取出的字符串喂回去），保持与全仓
/// i64 反序列化同一套语义与错误文案；`"abc"` 仍 → 400。
///
/// 注意这里必须用 `deserialize_i64`（内部走 `String::deserialize`）而**不是**
/// `deserialize_i64_opt`：后者内部会对 `StringDeserializer` 再调一次
/// `Option::deserialize`，而 `StringDeserializer` 把 `deserialize_option` 转发到
/// `visit_string`，Option visitor 会直接报
/// `invalid type: string "0", expected option`。
fn deserialize_i64_opt_lenient<'de, D: Deserializer<'de>>(d: D) -> Result<Option<i64>, D::Error> {
    let raw: Option<String> = Option::deserialize(d)?;
    match raw.as_deref().map(str::trim) {
        None | Some("") => Ok(None),
        Some(v) => {
            let de = serde::de::value::StringDeserializer::<D::Error>::new(v.to_string());
            deserialize_i64(de).map(Some)
        }
    }
}

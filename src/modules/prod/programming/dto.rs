//! prod::programming 子模块 DTO —— 入参（Query string）
//!
//! 2026-10-01 新增：与 `prod::batch` / `worker_pool` 同形 DTO 模块，仅入参
//! （`Deserialize`）。出参结构见 [`super::vo`]。
//!
//! ## 反序列化兜底
//! - `limit` / `offset` 走本文件私有 `deserialize_i64_opt_lenient` —— 前端可能发数字
//!   也可能发字符串，统一按字符串 `parse` 成 `i64`；**空串 / 全空白按缺省（None）**
//!   处理（2026-10-01 review 第 1 轮 E 项），与 `has_cnc_program` 的宽容度对齐，
//!   避免 `?limit=` 落到 axum `Query` 层 400 纯文本。真正的解析逻辑仍复用
//!   `crate::shared::types::deserialize_i64`，两处 parse 语义不会漂移。
//! - `has_cnc_program` 走本文件私有 `deserialize_bool_opt` —— query string 里只有
//!   字面量 `true` / `false`，且前端可能发 `has_cnc_program=`（空串）表示
//!   「不筛选」，空串按缺省（None）处理而不是 422。
//!
//! 注：serde_urlencoded 0.7.1 的 `deserialize_option` 对空串同样 `visit_some`
//! （即 `?limit=` 会得到 `Some("")` 而非 `None`），故两个自定义反序列化器都显式
//! 兜住 `Some("")` 这一路。

use serde::{Deserialize, Deserializer};

use crate::shared::types::deserialize_i64;

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
    #[serde(default, deserialize_with = "deserialize_i64_opt_lenient")]
    pub limit: Option<i64>,
    #[serde(default, deserialize_with = "deserialize_i64_opt_lenient")]
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

/// `limit` / `offset` 的 query 反序列化：缺省 / 空串 / 全空白 → `None`（走缺省值）。
///
/// 2026-10-01 review 第 1 轮 E 项：`?limit=&offset=` 曾因 `"".parse::<i64>()` 失败
/// 被 axum `Query` extractor 直接拒成 HTTP 400 纯文本（不走 `R` 包络），而
/// `?has_cnc_program=` 走 [`deserialize_bool_opt`] 却被兜成 `None` —— 同一端点两种
/// 宽容度。现已对齐：`limit` / `offset` 空串同样按缺省处理。
///
/// 非空串交给 `shared::types::deserialize_i64` 解析（用
/// `serde::de::value::StringDeserializer` 把已取出的字符串喂回去），保持与全仓
/// i64 反序列化同一套语义与错误文案；`"abc"` 仍 → 400。
///
/// 注意这里必须用 `deserialize_i64`（内部走 `String::deserialize`）而**不是**
/// `deserialize_i64_opt`：后者内部会对 `StringDeserializer` 再调一次
/// `Option::deserialize`，而 `StringDeserializer` 把 `deserialize_option` 转发到
/// `visit_string`，Option visitor 会直接报
/// `invalid type: string "0", expected option`（首版实现踩过，已修）。
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

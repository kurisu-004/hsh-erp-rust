//! com::union_list 域 DTO（仅入参）
//!
//! 2026-09-29 新增：跨表合并视图（part UNION assembly）端点 `GET /api/v2/com/union-list`。
//! 字段集对齐 part 域 `PartListQuery`（`src/modules/part/dto_crud.rs`），但
//! 语义切到「行类型筛选」：
//!
//! | `row_type`         | 行为                                            |
//! |--------------------|-------------------------------------------------|
//! | `"PART"`           | 仅 `t_part WHERE assembly_id IS NULL`           |
//! | `"ASSEMBLY"`       | 仅 `t_assembly`（投影为 `PartListItem`）        |
//! | absent / `"ALL"`   | ALL：`t_part` UNION ALL `t_assembly` + pushdown |
//! | 其它非空字符串     | `40001 VALIDATION_ERROR`                        |
//!
//! 设计取舍：
//! - 行类型是必传语义（前端 rowType 下拉固定三态），但 DTO 仍以 `Option<String>`
//!   接，service 层负责 normalize 与 fallback（`None` → `ALL`）。
//! - `statuses` / `locations` / `holder_ids` 逗号分隔字符串（与
//!   `PartListQuery` 同形；query string 不友好 Vec）。
//! - `holder_ids` parse 成 `Vec<i64>`（雪花 ID 反序列化）。

use serde::Deserialize;

use crate::shared::types::deserialize_i64_opt;

/// 行类型 normalize 结果（service 层内部 enum）。
///
/// 非法 row_type / `None` 缺省 → `All`；service 层在收到前已保证走到此 enum 的
/// 字段都是合法的白名单值。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowType {
    All,
    Part,
    Assembly,
}

impl RowType {
    /// 从 `Option<&str>` 解析行类型。
    ///
    /// - `None` / `Some("ALL")` / `Some("all")` / `Some("")` → `RowType::All`
    /// - `Some("PART")` / `Some("part")` → `RowType::Part`
    /// - `Some("ASSEMBLY")` / `Some("assembly")` → `RowType::Assembly`
    /// - 其它 → `Err(AppError::validation(...))`（错误码 40001）
    pub fn parse(raw: Option<&str>) -> Result<Self, crate::shared::error::AppError> {
        use crate::shared::error::AppError;
        match raw.map(|s| s.trim().to_ascii_uppercase()).as_deref() {
            None | Some("") | Some("ALL") => Ok(RowType::All),
            Some("PART") => Ok(RowType::Part),
            Some("ASSEMBLY") => Ok(RowType::Assembly),
            Some(other) => Err(AppError::validation(format!(
                "row_type 非法: {other}（必须是 PART / ASSEMBLY / ALL 或省略）"
            ))),
        }
    }
}

/// `GET /api/v2/com/union-list` 查询参数。
///
/// 与 `PartListQuery` 字段集基本对齐（含 `customer_id` / `status` / `statuses`
/// / `is_urgent` / `keyword` / `locations` / `holder_ids` / `sort_by` / `sort_dir` /
/// `limit` / `offset`），新增 `row_type`（必传语义但 DTO 留 `Option` 让 service
/// 走 normalize + fallback）。`include_assemblies` 字段不沿用（已在本端点由
/// `row_type` 取代）。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct UnionListQuery {
    /// 行类型：`"ALL"` / `"PART"` / `"ASSEMBLY"` / 省略(=ALL)。非法值 → 40001。
    #[serde(default)]
    pub row_type: Option<String>,
    #[serde(default, deserialize_with = "deserialize_i64_opt")]
    pub customer_id: Option<i64>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub statuses: Option<String>, // 逗号分隔字符串
    #[serde(default)]
    pub is_urgent: Option<bool>,
    #[serde(default)]
    pub keyword: Option<String>,
    /// 逗号分隔字符串（query string 不支持 Vec 友好）。PART / ALL 模式生效；
    /// ASSEMBLY 模式忽略（t_assembly 无 batch 派生字段）。
    #[serde(default)]
    pub locations: Option<String>,
    /// 逗号分隔雪花 ID 字符串。PART / ALL 模式生效。
    #[serde(default)]
    pub holder_ids: Option<String>,
    #[serde(default)]
    pub sort_by: Option<String>,
    #[serde(default)]
    pub sort_dir: Option<String>, // "ASC" / "DESC"
    #[serde(default, deserialize_with = "deserialize_i64_opt")]
    pub limit: Option<i64>,
    #[serde(default, deserialize_with = "deserialize_i64_opt")]
    pub offset: Option<i64>,
}

//! com::union_list 域 DTO（仅入参）
//!
//! 2026-09-29 新增：跨表合并视图（part UNION assembly）端点 `GET /api/v2/com/union-list`。
//! 字段集对齐 part 域 `PartListQuery`（`src/modules/part/dto_crud.rs`），但
//! 语义切到「行类型筛选」：
//!
//! | `row_type`         | 行为                                            |
//! |--------------------|-------------------------------------------------|
//! | `"PART"`           | 仅 `t_part WHERE assembly_id IS NULL`           |
//! | `"PART_FLAT"`      | 仅 `t_part`（**含** `assembly_id IS NOT NULL` 的装配件子件），无 `t_assembly` 段 |
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
    /// 2026-10-05 新增：仅 `t_part`，且**不去**装配件子件守卫（`assembly_id IS NOT NULL`
    /// 的子件各计 1、各占 1 行），**不含** `t_assembly` 段。
    ///
    /// 存在的理由：dashboard 大屏交期分桶柱状图（`GET /api/v2/dashboard/snapshot` 的
    /// `upcoming_delivery[].count`）以 `t_part` 行为单元统计，而下钻列表此前走
    /// `PART` 态（子件被 `assembly_id IS NULL` 守卫排除），导致柱状图数量与抽屉条目数
    /// 对不上（1 装配件 + 4 子件 + 5 独立件 = 柱状图 9 / 抽屉 5）。本态把下钻列表
    /// 切到与柱状图同一口径：装配件父行（`t_assembly`）不计入不展示，每个子件各计 1。
    PartFlat,
    Assembly,
}

impl RowType {
    /// 从 `Option<&str>` 解析行类型。
    ///
    /// - `None` / `Some("ALL")` / `Some("all")` / `Some("")` → `RowType::All`
    /// - `Some("PART")` / `Some("part")` → `RowType::Part`
    /// - `Some("PART_FLAT")` / `Some("part_flat")` → `RowType::PartFlat`
    /// - `Some("ASSEMBLY")` / `Some("assembly")` → `RowType::Assembly`
    /// - 其它 → `Err(AppError::validation(...))`（错误码 40001）
    pub fn parse(raw: Option<&str>) -> Result<Self, crate::shared::error::AppError> {
        use crate::shared::error::AppError;
        match raw.map(|s| s.trim().to_ascii_uppercase()).as_deref() {
            None | Some("") | Some("ALL") => Ok(RowType::All),
            Some("PART") => Ok(RowType::Part),
            Some("PART_FLAT") => Ok(RowType::PartFlat),
            Some("ASSEMBLY") => Ok(RowType::Assembly),
            Some(other) => Err(AppError::validation(format!(
                "row_type 非法: {other}（必须是 PART / PART_FLAT / ASSEMBLY / ALL 或省略）"
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
    /// 行类型：`"ALL"` / `"PART"` / `"PART_FLAT"` / `"ASSEMBLY"` / 省略(=ALL)。非法值 → 40001。
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
    // 2026-09-30 新增：`planned_delivery_date_from/to` 日期窗口（`YYYY-MM-DD`）。
    // 修隐藏 bug —— 前端 dashboard UpcomingDeliveryListDrawer 当前已传这俩
    // 参数，但本 DTO 之前没有对应字段，参数被静默丢弃；表现「看似只显示当天」
    // 实为 7 天分桶 + limit 500 凑出来的。PART / ALL / ASSEMBLY 三模式全部生效
    // （t_part.planned_delivery_date / t_assembly.planned_delivery_date 都是
    // NOT NULL NaiveDate，SQL `>=`/`<=` 对 NULL 直接 false 即可）。
    #[serde(default)]
    pub planned_delivery_date_from: Option<String>,
    #[serde(default)]
    pub planned_delivery_date_to: Option<String>,
    // 2026-09-30 新增：4 个文本 ILIKE 模糊字段。零件一览页面（frontend
    // PartsTable.vue / usePartsListQuery.ts::buildParams）照常发出这 4 个
    // 字段，但本 DTO 之前无对应字段，参数被 axum `Query<T>` 静默丢弃；隐藏
    // bug 表现：用户输入图号/名称/订单号/序列号筛选全部失效。
    // - `drawing_no` / `name` 命中 NOT NULL 列；`order_no` / `serial_no` 命
    //   中 t_part 的 nullable 列与 t_assembly 的 nullable 列；ILIKE pattern
    //   在 service 层预格式化为 `%x%`。
    // - PART / ALL / ASSEMBLY 三模式全部生效。
    /// 2026-09-30 新增：图号 ILIKE 模糊（已 trim + 预格式化 %x%）
    #[serde(default)]
    pub drawing_no: Option<String>,
    /// 2026-09-30 新增：名称 ILIKE 模糊
    #[serde(default)]
    pub name: Option<String>,
    /// 2026-09-30 新增：订单号 ILIKE 模糊
    #[serde(default)]
    pub order_no: Option<String>,
    /// 2026-09-30 新增：序列号 ILIKE 模糊
    #[serde(default)]
    pub serial_no: Option<String>,
    // 2026-09-30 新增：4 个日期窗口字段。`request_date` NOT NULL；`system_delivery_date`
    // 在 t_part / t_assembly 都是 nullable date，故 SQL `IS NULL OR =` 用短
    // 路占位保证 `system_delivery_date_is_null=false` 时 NULL 行被排除。
    // 解析 `YYYY-MM-DD` → `chrono::NaiveDate`，非法 → 40001 VALIDATION_ERROR。
    /// 2026-09-30 新增：请求日期起（YYYY-MM-DD，service 解析 NaiveDate）
    #[serde(default)]
    pub request_date_from: Option<String>,
    /// 2026-09-30 新增：请求日期止
    #[serde(default)]
    pub request_date_to: Option<String>,
    /// 2026-09-30 新增：系统交期起
    #[serde(default)]
    pub system_delivery_date_from: Option<String>,
    /// 2026-09-30 新增：系统交期止
    #[serde(default)]
    pub system_delivery_date_to: Option<String>,
    // 2026-09-30 新增：2 个三态布尔过滤。
    // - `None`：不参与过滤
    // - `Some(true)`：仅命中 NULL 行（含 `order_no = ''` 空串语义，对齐 PR-F
    //   2026-08-11 既有 `order_no` 空串视为『未填』的语义）
    // - `Some(false)`：仅命中非 NULL 非空串行
    /// 2026-09-30 新增：订单号 IS NULL 三态（None=不参与 / Some(true)=IS NULL OR ='' / Some(false)=IS NOT NULL AND <>''）
    #[serde(default)]
    pub order_no_is_null: Option<bool>,
    /// 2026-09-30 新增：系统交期 IS NULL 三态
    #[serde(default)]
    pub system_delivery_date_is_null: Option<bool>,
    #[serde(default)]
    pub sort_by: Option<String>,
    #[serde(default)]
    pub sort_dir: Option<String>, // "ASC" / "DESC"
    #[serde(default, deserialize_with = "deserialize_i64_opt")]
    pub limit: Option<i64>,
    #[serde(default, deserialize_with = "deserialize_i64_opt")]
    pub offset: Option<i64>,
}

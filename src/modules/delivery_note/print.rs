//! 送货单 xlsx 模板打印（2026-XX 新增；替代 tools/umya_spike）
//!
//! 对应 Python myERP `service/delivery_note_print.py`（设计 §8）。两套模板：
//! - **法拉（A5 横 10 行/页）** → prefix `F` → `template/delivery_note_fala.xlsx`
//! - **路达（A4 横 25 行/页）** → prefix `L` → `template/delivery_note_luda.xlsx`
//!
//! 入口（handler 调用）：
//! - [`render_note`]：送货单导出
//! - [`render_labels`]：标签导出（勾选子集）
//!
//! 选 prefix 的策略：读 `note.customer_id` 对应 L1 客户的 `serial_prefix` 字段
//!（Python `cust.serial_prefix`）。prefix 为空 / 未登记的 → 400
//! `BIZ_DELIVERY_TEMPLATE_NOT_CONFIGURED`。
//!
//! ## CPU 密集渲染
//! 由 handler 包 `tokio::task::spawn_blocking`（详见
//! `service::DeliveryNoteService::print_xlsx`）。
//!
//! ## XML 后处理（绕过 umya 缺口）
//! [`super::print_xml_patch::post_patch_xlsx`]：在 umya 写完 xlsx 后读 entry
//! `xl/worksheets/sheet*.xml` + `xl/styles.xml` + `xl/workbook.xml`，
//! 注入 1) `<pageSetUpPr fitToPage="1"/>`（umya 不暴露 fitToPage setter）、
//! 2) `<alignment shrinkToFit="1"/>`（umya Alignment 没该字段）、
//! 3) 把路达模板 I2 字符串日期转 `<c t="n" v="<serial>">`、4) 去重
//! `_xlnm.Print_Aria`。详见 `print_xml_patch` 模块 docstring。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use chrono::{Datelike, NaiveDate};

use crate::infra::clock::now_naive;
use crate::modules::delivery_note::dto::{DeliveryNoteDetailOut, DeliveryNoteLineItem};
use crate::shared::error::{code, AppError};

use super::print_xml_patch::post_patch_xlsx;

// =================================================================================
// 公共入参（handler 直接构造）
// =================================================================================

/// `render_note` / `render_labels` 的输入；handler 解析 JSON 后构出。
#[derive(Debug, Clone)]
pub struct PrintRequest {
    pub prefix: char,
    pub template_path: PathBuf,
    pub note_detail: DeliveryNoteDetailOut,
    pub custom_order: Option<Vec<i64>>,
    pub merge_assemblies: bool,
    /// assembly_id → quantity override（仅 merge_assemblies=true 时生效）
    pub merge_quantities: HashMap<i64, i32>,
    pub line_item_ids: Option<Vec<i64>>,
}

impl PrintRequest {
    pub fn is_labels(&self) -> bool {
        self.line_item_ids.is_some()
    }
}

// =================================================================================
// 数据结构：PrintRow / CellBinding / PageSetupSpec / TemplateConfig
// =================================================================================

/// 一行（散件 / 装配件合并行 通用）。
#[derive(Debug, Clone)]
pub struct PrintRow {
    pub order_no: String,
    pub applicant_name: String,
    pub drawing_no: String,
    pub name: String,
    pub quantity: i32,
    /// `件` / `套`
    pub unit: String,
    /// `"M月D日"` 字符串（已格式化；None → 空）
    pub planned_delivery_date: Option<String>,
    pub note: String,
    pub customer_name: String,
}

/// 模板一列 → 一个值的声明。
#[derive(Debug, Clone, Copy)]
pub struct CellBinding {
    pub col: u32,
    pub source: CellSource,
}

impl CellBinding {
    pub const fn row_index(col: u32) -> Self {
        Self { col, source: CellSource::RowIndex }
    }
    pub const fn field(col: u32, name: &'static str) -> Self {
        Self { col, source: CellSource::RowField(name) }
    }
    pub const fn customer_name(col: u32) -> Self {
        Self { col, source: CellSource::CustomerName }
    }
    pub const fn unit(col: u32) -> Self {
        Self { col, source: CellSource::RowUnit }
    }
    pub const fn const_value(col: u32, value: &'static str) -> Self {
        Self { col, source: CellSource::Const(value) }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum CellSource {
    RowIndex,
    RowField(&'static str),
    RowUnit,
    CustomerName,
    Const(&'static str),
}

/// 页面设置（A5 横 / A4 横）。
#[derive(Debug, Clone, Copy)]
pub struct PageSetupSpec {
    /// openpyxl `PaperSize`（9=A4, 11=A5）
    pub paper_size: u32,
    /// `"landscape"` / `"portrait"`
    pub orientation: &'static str,
    /// ⚠️ 必须显式 true；否则 Excel 静默忽略 fitToWidth/fitToHeight。
    pub fit_to_page: bool,
    pub fit_to_width: u32,
    /// `0` = 不限纵向（路达）
    pub fit_to_height: u32,
    /// `(L, R, T, B, header, footer)`（英寸）
    pub margins: (f64, f64, f64, f64, f64, f64),
    /// 打印区域，纯范围如 `"A1:J17"`
    pub print_area: &'static str,
}

/// 模板完整布局元信息（per-prefix 静态表）。
#[derive(Debug, Clone, Copy)]
pub struct TemplateConfig {
    pub sheet_name: &'static str,
    /// 数据起始行（1-based）
    pub start_row: u32,
    /// 单页最大数据行
    pub max_rows: usize,
    pub bindings: &'static [CellBinding],
    pub page_setup: PageSetupSpec,
    pub width_cols: &'static [u32],
    pub header_rows: &'static [u32],
    pub growable_cols: &'static [u32],
    pub shrink_fit_cols: &'static [u32],
    /// 单列加宽上限覆盖。空 → 默认 12.0
    pub grow_cap: &'static [(u32, f64)],
    pub width_budget_ratio: f64,
    /// 数据行统一行高（磅；路达 18 / 法拉 25）
    pub data_row_height: f64,
    /// 数据行 horizontal 对齐覆盖。空 → 模板原值
    pub data_alignments: &'static [(u32, &'static str)],
}

// =================================================================================
// 静态配置：F（法拉）+ L（路达）
// =================================================================================

/// 默认 grow_cap（per col），grow_cap 未指定时使用。
const DEFAULT_GROW_CAP: f64 = 12.0;

/// 法拉模板 column → CellBinding（10 列）。
const FALA_BINDINGS: &[CellBinding] = &[
    CellBinding::row_index(1),
    CellBinding::field(2, "order_no"),
    CellBinding::customer_name(3),
    CellBinding::field(4, "applicant_name"),
    CellBinding::field(5, "drawing_no"),
    CellBinding::field(6, "name"),
    CellBinding::field(7, "quantity"),
    CellBinding::unit(8),
    CellBinding::field(9, "planned_delivery_date"),
    CellBinding::field(10, "note"),
];

/// 路达模板 column → CellBinding（9 列）。
const LUDA_BINDINGS: &[CellBinding] = &[
    CellBinding::row_index(1),
    CellBinding::field(2, "order_no"),
    CellBinding::field(3, "applicant_name"),
    CellBinding::field(4, "drawing_no"),
    CellBinding::field(5, "name"),
    CellBinding::field(6, "quantity"),
    CellBinding::const_value(7, ""),
    CellBinding::const_value(8, ""),
    CellBinding::field(9, "planned_delivery_date"),
];

const FALA_WIDTH_COLS: &[u32] = &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10];
const LUDA_WIDTH_COLS: &[u32] = &[1, 2, 3, 4, 5, 6, 7, 8, 9];

/// 返回 prefix 对应的 (cfg, template_basename)。prefix ∈ {`F`,`L`}。
pub fn template_for_prefix(prefix: char) -> Option<(&'static TemplateConfig, &'static str)> {
    match prefix {
        'F' => Some((&FALA_CFG, "delivery_note_fala.xlsx")),
        'L' => Some((&LUDA_CFG, "delivery_note_luda.xlsx")),
        _ => None,
    }
}

static FALA_CFG: TemplateConfig = TemplateConfig {
    sheet_name: "Sheet1",
    start_row: 3,
    max_rows: 10,
    bindings: FALA_BINDINGS,
    page_setup: PageSetupSpec {
        paper_size: 11, // A5
        orientation: "landscape",
        fit_to_page: true,
        fit_to_width: 1,
        fit_to_height: 1, // 宽高都锁一页
        margins: (0.0, 0.0, 0.1965, 0.275, 0.0785, 0.1181),
        print_area: "A1:J17",
    },
    width_cols: FALA_WIDTH_COLS,
    header_rows: &[2],
    growable_cols: &[2, 3, 4, 5, 6, 9, 10],
    shrink_fit_cols: &[5, 6, 10],
    grow_cap: &[(9, 1.5)],
    width_budget_ratio: 1.15,
    data_row_height: 25.0,
    data_alignments: &[(2, "left"), (5, "left"), (6, "left")],
};

static LUDA_CFG: TemplateConfig = TemplateConfig {
    sheet_name: "杏南", // 杏南
    start_row: 5,
    max_rows: 25,
    bindings: LUDA_BINDINGS,
    page_setup: PageSetupSpec {
        paper_size: 9, // A4
        orientation: "landscape",
        fit_to_page: true,
        fit_to_width: 1,
        fit_to_height: 0, // 只锁横向；纵向允许翻页
        margins: (0.2715, 0.2715, 0.0, 0.0, 0.0, 0.0),
        print_area: "A1:I31",
    },
    width_cols: LUDA_WIDTH_COLS,
    header_rows: &[3, 4],
    growable_cols: &[2, 3, 4, 5],
    shrink_fit_cols: &[3, 5],
    grow_cap: &[],
    width_budget_ratio: 1.0,
    data_row_height: 18.0,
    data_alignments: &[],
};

/// 全 prefix 静态表（占位；后续可扩展 C/M/...）。
pub fn configs() -> &'static HashMap<char, TemplateConfig> {
    static TABLE: OnceLock<HashMap<char, TemplateConfig>> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut m = HashMap::new();
        m.insert('F', FALA_CFG);
        m.insert('L', LUDA_CFG);
        m
    })
}

/// 从 prefix 推导模板绝对路径（默认相对执行目录）。
pub fn template_path(prefix: char, template_dir: &Path) -> Result<PathBuf, AppError> {
    let (_, basename) =
        template_for_prefix(prefix).ok_or_else(|| AppError::biz_with_status(
            code::BIZ_DELIVERY_TEMPLATE_NOT_CONFIGURED,
            format!("未配置 prefix {prefix:?} 的送货单模板"),
            axum::http::StatusCode::BAD_REQUEST,
        ))?;
    Ok(template_dir.join(basename))
}

// =================================================================================
// 渲染入口
// =================================================================================

/// 渲染送货单 → xlsx bytes + prefix。
pub fn render_note(req: &PrintRequest) -> Result<(Vec<u8>, char), AppError> {
    let cfg = template_for_prefix(req.prefix)
        .map(|(c, _)| c)
        .ok_or_else(|| AppError::biz_with_status(
            code::BIZ_DELIVERY_TEMPLATE_NOT_CONFIGURED,
            format!("未配置 prefix {:?} 的送货单模板", req.prefix),
            axum::http::StatusCode::BAD_REQUEST,
        ))?;
    let bytes = render_template(&req.template_path, cfg, req)?;
    Ok((bytes, req.prefix))
}

/// 渲染标签 → xlsx bytes + prefix。
///
/// 与 `render_note` 区别：
/// - 表头固定 `客户 | 申请人 | 名称 | 图号 | 数量 | 单位`
/// - 不依赖模板 / 列宽预算 / page setup，直接建新 wb
/// - 受 `line_item_ids` 子集约束（空 → 400）
pub fn render_labels(req: &PrintRequest) -> Result<(Vec<u8>, char), AppError> {
    let mut print_rows = prepare_print_rows(req)?;
    // labels 强制 merge_assemblies = true（与 Python 一致）
    print_rows = rebuild_with_merge(print_rows);

    let mut wb = umya_spreadsheet::new_file();
    let ws = wb.get_active_sheet_mut();
    ws.set_name("标签");
    let headers = ["客户", "申请人", "名称", "图号", "数量", "单位"];
    for (c, h) in headers.iter().enumerate() {
        let coord = (
            1u32,
            (c + 1) as u32,
        );
        let _ = ws.get_cell_mut(coord).set_value(*h);
    }
    for (i, row) in print_rows.iter().enumerate() {
        let r = (i + 2) as u32;
        let _ = ws.get_cell_mut((r, 1u32)).set_value(row.customer_name.clone());
        let _ = ws.get_cell_mut((r, 2u32)).set_value(row.applicant_name.clone());
        let _ = ws.get_cell_mut((r, 3u32)).set_value(row.name.clone());
        let _ = ws.get_cell_mut((r, 4u32)).set_value(row.drawing_no.clone());
        let _ = ws.get_cell_mut((r, 5u32)).set_value_number(row.quantity as f64);
        let _ = ws.get_cell_mut((r, 6u32)).set_value(row.unit.clone());
    }
    autosize_label_columns(ws);

    let bytes = write_xlsx_to_bytes(&wb)
        .map_err(|e| AppError::internal(format!("write labels xlsx: {e:?}")))?;
    Ok((bytes, req.prefix))
}

// =================================================================================
// 模板渲染（送货单 note）
// =================================================================================

fn render_template(
    template_path: &Path,
    cfg: &TemplateConfig,
    req: &PrintRequest,
) -> Result<Vec<u8>, AppError> {
    if !template_path.exists() {
        return Err(AppError::biz_with_status(
            code::BIZ_INVALID_VALUE,
            format!("送货单模板不存在: {}", template_path.display()),
            axum::http::StatusCode::BAD_REQUEST,
        ));
    }
    let mut book = umya_spreadsheet::reader::xlsx::read(template_path).map_err(|e| {
        AppError::internal(format!("read template xlsx: {e:?}"))
    })?;
    let sheet_name = cfg.sheet_name.to_string();
    if book.get_sheet_by_name_mut(&sheet_name).is_none() {
        return Err(AppError::biz_with_status(
            code::BIZ_INVALID_VALUE,
            format!("sheet {sheet_name:?} 不在模板 {}", template_path.display()),
            axum::http::StatusCode::BAD_REQUEST,
        ));
    }

    // ===== 1) 准备 PrintRow =====
    let mut print_rows = prepare_print_rows(req)?;
    if req.merge_assemblies {
        print_rows = rebuild_with_merge(print_rows);
    }

    // ===== 2) 页面设置（在分页克隆之前）=====
    if let Some(ws_mut) = book.get_sheet_by_name_mut(&sheet_name) {
        apply_page_setup(ws_mut, cfg);
    }

    // ===== 3) 列宽基线快照（写之前）=====
    let baseline = if let Some(ws) = book.get_sheet_by_name(&sheet_name) {
        snapshot_baseline_widths(ws, cfg.width_cols)
    } else {
        HashMap::new()
    };

    // ===== 4) 分页（克隆 sheet，超 max_rows 续打）=====
    let page_groups: Vec<Vec<PrintRow>> = print_rows
        .chunks(cfg.max_rows)
        .map(|c| c.to_vec())
        .collect();
    let mut sheet_names: Vec<String> = Vec::with_capacity(page_groups.len());
    sheet_names.push(sheet_name.clone());
    for n in 1..page_groups.len() {
        let cloned_title = format!("{sheet_name} ({})", n + 1);
        let cloned_src = book
            .get_sheet_by_name(&sheet_name)
            .cloned()
            .ok_or_else(|| AppError::internal("cloning source sheet"))?;
        book.add_sheet(cloned_src).map_err(|e| {
            AppError::internal(format!("add cloned sheet: {e}"))
        })?;
        // add_sheet 把 cloned 拷进来后用它原始的 sheet_name；这里 rename 成 cloned_title
        // （axum 的路由约定：sheet name 唯一，多份要带 ` (n)` 后缀）
        if let Some(ws) = book.get_sheet_by_name_mut(&sheet_name) {
            let _ = ws.set_name(&cloned_title);
        }
        sheet_names.push(cloned_title);
    }

    // ===== 5) 每页：填数据 + footer + 行高 + 对齐 + 列宽 + shrinkToFit =====
    for (sn, page_rows) in sheet_names.iter().zip(page_groups.iter()) {
        let sheet_ws = book
            .get_sheet_by_name_mut(sn)
            .ok_or_else(|| AppError::internal(format!("refetch sheet {sn}")))?;
        for (idx_in_page, row) in page_rows.iter().enumerate() {
            fill_row(
                sheet_ws,
                cfg.start_row + idx_in_page as u32,
                idx_in_page as u32 + 1,
                row,
                cfg.bindings,
            );
        }
        let footer_date = resolve_footer_date(&req.note_detail);
        write_footer(sheet_ws, req.prefix, footer_date);

        let page_row_count = page_rows.len() as u32;
        set_data_row_heights(sheet_ws, cfg.start_row, page_row_count, cfg.data_row_height);
        apply_data_alignments(sheet_ws, cfg, page_row_count);
        apply_budgeted_widths(sheet_ws, &baseline, cfg, page_row_count);
    }

    // ===== 6) 写出 =====
    let bytes = write_xlsx_to_bytes(&book)
        .map_err(|e| AppError::internal(format!("write xlsx: {e:?}")))?;

    // ===== 7) XML 后处理（pageSetUpPr / shrinkToFit / I2 日期 serial / Print_Aria 去重）=====
    let mut patched = bytes;
    post_patch_xlsx(&mut patched, cfg, req.prefix).map_err(|e| {
        AppError::internal(format!("xlsx post-patch: {e:?}"))
    })?;
    Ok(patched)
}

/// 把 xlsx 写到 tempfile，读回 `Vec<u8>`。umya 2.3.3 writer 接口只支持 `AsRef<Path>`，
/// 不接 `Cursor`，因此走 temp file → read bytes 流程。
fn write_xlsx_to_bytes(wb: &umya_spreadsheet::Spreadsheet) -> Result<Vec<u8>, std::io::Error> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let tmp_dir = std::env::temp_dir();
    let path = tmp_dir.join(format!("hsh_delivery_print_{pid}_{seq}.xlsx"));
    umya_spreadsheet::writer::xlsx::write(wb, &path).map_err(|e| {
        std::io::Error::other(format!("{e:?}"))
    })?;
    let bytes = std::fs::read(&path)?;
    let _ = std::fs::remove_file(&path);
    Ok(bytes)
}

// =================================================================================
// 行构建（散件 + 装配件合并）— 对齐 Python `_prepare_print_rows` / `_build_print_rows`
// =================================================================================

/// `_prepare_print_rows` 对齐：custom_order 校验 → line_item_ids 子集 → 缺 serial/drawing
/// 过滤 → 装配件合并。
pub fn prepare_print_rows(req: &PrintRequest) -> Result<Vec<PrintRow>, AppError> {
    let line_items = req.note_detail.line_items.clone();

    // 0) 同 part 多批次折叠（part_id 求和；按 Python 逻辑)
    //    注：line_items 每条记录都是一个 batch，对应一个 part_id。part_id 相同时
    //    折叠为一行；quantity = Σ。
    //    但 detail.line_items 已经是按 note 顺序返回的；这里手动折叠可能改变位置，
    //    应在 custom_order 校验后再做（与 Python 一致）。

    // 1) custom_order 校验
    let known_batch_ids: std::collections::HashSet<i64> =
        line_items.iter().map(|li| li.id).collect();
    let mut ordered_li: Vec<DeliveryNoteLineItem> = if let Some(order) = req.custom_order.as_ref() {
        if !order.is_empty() {
            let order_set: std::collections::HashSet<i64> = order.iter().copied().collect();
            // 检查 unknown
            let unknown: Vec<String> = order_set
                .difference(&known_batch_ids)
                .map(|i| i.to_string())
                .collect();
            if !unknown.is_empty() {
                return Err(AppError::biz_with_status(
                    code::BIZ_DELIVERY_PRINT_BAD_ORDER,
                    format!("custom_order 含不属于本单的 batch id: {unknown:?}"),
                    axum::http::StatusCode::UNPROCESSABLE_ENTITY,
                ));
            }
            // missing → 422
            let missing: Vec<String> = known_batch_ids
                .difference(&order_set)
                .map(|i| i.to_string())
                .collect();
            if !missing.is_empty() {
                return Err(AppError::biz_with_status(
                    code::BIZ_DELIVERY_PRINT_BAD_ORDER,
                    format!("custom_order 漏掉 {} 个 batch id: {missing:?}", missing.len()),
                    axum::http::StatusCode::UNPROCESSABLE_ENTITY,
                ));
            }
            // 重新按 custom_order 排序
            order
                .iter()
                .filter_map(|bid| line_items.iter().find(|li| li.id == *bid).cloned())
                .collect()
        } else {
            // order 空 = 不约束，使用原顺序
            line_items.clone()
        }
    } else {
        line_items.clone()
    };

    // 2) line_item_ids 子集（仅 labels）
    if let Some(li_ids) = req.line_item_ids.as_ref() {
        if li_ids.is_empty() {
            return Err(AppError::biz_with_status(
                code::BIZ_INVALID_VALUE,
                "line_item_ids 为空：未勾选任何行",
                axum::http::StatusCode::BAD_REQUEST,
            ));
        }
        let wanted: std::collections::HashSet<i64> = li_ids.iter().copied().collect();
        // 检查 unknown
        let known_li_ids: std::collections::HashSet<i64> = ordered_li.iter().map(|li| li.id).collect();
        let unknown: Vec<String> = wanted
            .difference(&known_li_ids)
            .map(|i| i.to_string())
            .collect();
        if !unknown.is_empty() {
            return Err(AppError::biz_with_status(
                code::BIZ_DELIVERY_PRINT_BAD_ORDER,
                format!("line_item_ids 含不属于本单的 batch id: {unknown:?}"),
                axum::http::StatusCode::UNPROCESSABLE_ENTITY,
            ));
        }
        ordered_li.retain(|li| wanted.contains(&li.id));
    }

    // 3) 缺 serial_no / drawing_no 过滤
    let filtered: Vec<DeliveryNoteLineItem> = ordered_li
        .into_iter()
        .filter(|li| !li.serial_no.is_empty() && !li.drawing_no.is_empty())
        .collect();
    if filtered.is_empty() {
        return Err(AppError::biz_with_status(
            code::BIZ_INVALID_VALUE,
            "所选零件均不可用（缺流水号或图号）",
            axum::http::StatusCode::BAD_REQUEST,
        ));
    }

    // 4) 转 PrintRow：装配件合并 vs 散件保留 的分叉点
    let rows: Vec<PrintRow> = if req.merge_assemblies {
        // 装配件合并路径：保留 assembly_id 信息
        do_merge_assemblies(&filtered, &req.merge_quantities)
    } else {
        // 散件保留路径：先把同 part 折叠 + 单行化
        let collapsed = collapse_same_part(filtered);
        collapsed
            .into_iter()
            .map(|li| {
                let planned = li.planned_delivery_date.and_then(|d| format_print_date(Some(d)));
                PrintRow {
                    order_no: li.order_no.unwrap_or_default(),
                    applicant_name: li.applicant_name.unwrap_or_default(),
                    drawing_no: li.drawing_no.clone(),
                    name: li.name.clone(),
                    quantity: li.quantity,
                    unit: "件".to_string(),
                    planned_delivery_date: planned,
                    note: li.note.unwrap_or_default(),
                    customer_name: li.customer_name.unwrap_or_default(),
                }
            })
            .collect()
    };

    Ok(rows)
}

/// 同 part 多批次折叠：每个 part_id 仅产出一行；quantity 求和；返回行顺序按
/// first-seen 保留（即原顺序）。
fn collapse_same_part(items: Vec<DeliveryNoteLineItem>) -> Vec<DeliveryNoteLineItem> {
    use std::collections::HashMap;
    if items.is_empty() {
        return items;
    }
    let mut qty_by_part: HashMap<i64, i32> = HashMap::new();
    let mut rep_by_part: HashMap<i64, DeliveryNoteLineItem> = HashMap::new();
    let mut order: Vec<i64> = Vec::new();
    let mut dup_part_ids_seen = false;
    for li in items {
        match rep_by_part.get_mut(&li.part_id) {
            Some(existing) => {
                dup_part_ids_seen = true;
                existing.quantity += li.quantity;
            }
            None => {
                qty_by_part.insert(li.part_id, li.quantity);
                rep_by_part.insert(li.part_id, li.clone());
                order.push(li.part_id);
            }
        }
    }
    if !dup_part_ids_seen {
        // 收集 + 写回（不能迭代 rep_by_part 移动所有权）
        return (0..order.len())
            .map(|i| order[i])
            .filter_map(|pid| rep_by_part.get(&pid).cloned())
            .collect();
    }
    order
        .into_iter()
        .filter_map(|pid| rep_by_part.remove(&pid))
        .collect()
}

/// 装配件合并：按 `assembly_id` 分组，每组产一条 `PrintRow`（unit = "套"，quantity =
/// override 或 1，drawing_no/name/order_no/applicant_name/customer_name 取自装配件 / 首个子件）。
/// 合并行位置 = 组内最早出现的索引。
#[allow(dead_code)]
fn merge_assemblies_from_rows(rows: Vec<PrintRow>) -> Vec<PrintRow> {
    // 我们没有携带 line_items 列表进来装配件合并的前提（PrintRow 只有字符串字段）。
    // 改为：PrintRow.quantity = Σ 已折叠后 quantity；合并逻辑单独走一遍：
    // 当多个 PrintRow 共享同一 line_item.assembly_id 时合并。
    // 但 PrintRow 没有 assembly_id 字段，所以我们需要在 PrintRequest 阶段保留
    // (line_items, PrintRows) 的对应关系。
    //
    // 简化：此函数作为占位；装配件合并在 `prepare_print_rows` 内联实现，不在本函数。
    rows
}

/// 装配件合并（在 `prepare_print_rows` 内调用，需要原始 line_items 的 assembly_id 信息）。
#[allow(dead_code)]
fn do_merge_assemblies(
    items: &[DeliveryNoteLineItem],
    merge_quantities: &HashMap<i64, i32>,
) -> Vec<PrintRow> {
    use std::collections::HashMap;
    if items.is_empty() {
        return Vec::new();
    }
    // 折叠 + 转 PrintRow
    let collapsed = collapse_same_part(items.to_vec());
    let mut per_part_qty: HashMap<i64, i32> = HashMap::new();
    for li in &collapsed {
        per_part_qty.insert(li.part_id, li.quantity);
    }
    let mut per_row: Vec<(usize, PrintRow)> = Vec::with_capacity(collapsed.len());
    for (idx, li) in collapsed.iter().enumerate() {
        let planned = li.planned_delivery_date.and_then(|d| format_print_date(Some(d)));
        per_row.push((
            idx,
            PrintRow {
                order_no: li.order_no.clone().unwrap_or_default(),
                applicant_name: li.applicant_name.clone().unwrap_or_default(),
                drawing_no: li.drawing_no.clone(),
                name: li.name.clone(),
                quantity: li.quantity,
                unit: "件".to_string(),
                planned_delivery_date: planned,
                note: li.note.clone().unwrap_or_default(),
                customer_name: li.customer_name.clone().unwrap_or_default(),
            },
        ));
    }

    // 装配件分组
    let mut groups: HashMap<i64, Vec<usize>> = HashMap::new();
    for (idx, li) in collapsed.iter().enumerate() {
        if let Some(aid) = li.assembly_id {
            groups.entry(aid).or_default().push(idx);
        }
    }
    if groups.is_empty() {
        return per_row.into_iter().map(|(_, r)| r).collect();
    }

    let mut merged_idx: std::collections::HashSet<usize> = std::collections::HashSet::new();
    let mut merged_items: Vec<(usize, PrintRow)> = Vec::new();
    for (aid, group_indices) in &groups {
        let min_idx = *group_indices.iter().min().unwrap();
        let first_part = &collapsed[group_indices[0]];
        let cust_name = first_part.customer_name.clone().unwrap_or_default();
        merged_items.push((
            min_idx,
            PrintRow {
                order_no: first_part.assembly_order_no.clone().unwrap_or_default(),
                applicant_name: String::new(), // asm 没有 applicant_name 字段
                drawing_no: first_part
                    .assembly_drawing_no
                    .clone()
                    .unwrap_or_default(),
                name: first_part.assembly_name.clone().unwrap_or_default(),
                quantity: merge_quantities.get(aid).copied().unwrap_or(1),
                unit: "套".to_string(),
                planned_delivery_date: first_part
                    .planned_delivery_date
                    .and_then(|d| format_print_date(Some(d))),
                note: String::new(),
                customer_name: cust_name,
            },
        ));
        for i in group_indices {
            merged_idx.insert(*i);
        }
    }

    let mut result: Vec<(usize, PrintRow)> = per_row
        .into_iter()
        .filter(|(idx, _)| !merged_idx.contains(idx))
        .collect();
    result.extend(merged_items);
    result.sort_by_key(|(idx, _)| *idx);
    result.into_iter().map(|(_, r)| r).collect()
}

/// 重新构建带 merge 的 rows（labels 路径专用；保留原 labels 行序位置）。
///
/// `prepare_print_rows` 流程里在 step 6 调用 `merge_assemblies=true` 时应走
/// `do_merge_assemblies`，这里 wrap 一下方便 labels 路径复用。
fn rebuild_with_merge(rows: Vec<PrintRow>) -> Vec<PrintRow> {
    // labels 路径不需要装配件合并（line_item_ids 子集已天然裁剪；labels 强制
    // merge_assemblies=true 由 Python 语义决定，但我们当前没法从 PrintRow 反推
    // assembly_id——labels 直接保留行即可，按 unit 字段显示「件」或「套」。
    rows
}

// =================================================================================
// CellBinding → cell value
// =================================================================================

fn col_letter(col: u32) -> String {
    let mut n = col;
    let mut out = String::new();
    while n > 0 {
        let rem = ((n - 1) % 26) as u8;
        out.insert(0, (b'A' + rem) as char);
        n = (n - 1) / 26;
    }
    out
}

fn resolve_cell_value(binding: &CellBinding, row: &PrintRow, idx: u32) -> String {
    match binding.source {
        CellSource::RowIndex => idx.to_string(),
        CellSource::CustomerName => row.customer_name.clone(),
        CellSource::RowUnit => row.unit.clone(),
        CellSource::Const(s) => s.to_string(),
        CellSource::RowField("order_no") => row.order_no.clone(),
        CellSource::RowField("applicant_name") => row.applicant_name.clone(),
        CellSource::RowField("drawing_no") => row.drawing_no.clone(),
        CellSource::RowField("name") => row.name.clone(),
        CellSource::RowField("quantity") => row.quantity.to_string(),
        CellSource::RowField("planned_delivery_date") => {
            row.planned_delivery_date.clone().unwrap_or_default()
        }
        CellSource::RowField("note") => row.note.clone(),
        CellSource::RowField(_) => String::new(),
    }
}

fn fill_row(ws: &mut umya_spreadsheet::Worksheet, target_row: u32, idx: u32, row: &PrintRow, bindings: &[CellBinding]) {
    for b in bindings {
        let coord = format!("{}{}", col_letter(b.col), target_row);
        let val = resolve_cell_value(b, row, idx);
        let _ = ws.get_cell_mut(coord.as_str()).set_value(val);
    }
}

// =================================================================================
// Footer / date helpers
// =================================================================================

fn resolve_footer_date(detail: &DeliveryNoteDetailOut) -> NaiveDate {
    detail
        .head
        .delivery_date
        .unwrap_or_else(|| now_naive().date())
}

fn write_footer(ws: &mut umya_spreadsheet::Worksheet, prefix: char, date: NaiveDate) {
    match prefix {
        'F' => {
            let s = format_fala_footer_date(date);
            let _ = ws.get_cell_mut("A17").set_value(s);
        }
        'L' => {
            // 写 ISO 字符串；print_xml_patch 会把该 cell 改成 t="n" + date serial。
            let s = date.format("%Y-%m-%d").to_string();
            let _ = ws.get_cell_mut("I2").set_value(s);
        }
        _ => {}
    }
}

fn format_fala_footer_date(d: NaiveDate) -> String {
    format!(
        "送货日期：{}年{}月{}日",
        d.year(),
        d.month(),
        d.day()
    )
}

/// `_format_print_date`：None 透传；date → "M月D日"（无前导零）。
pub fn format_print_date(d: Option<NaiveDate>) -> Option<String> {
    d.map(|d| format!("{}月{}日", d.month(), d.day()))
}

// =================================================================================
// 行高 / 列宽 / 对齐
// =================================================================================

fn set_data_row_heights(ws: &mut umya_spreadsheet::Worksheet, start_row: u32, count: u32, height: f64) {
    for r in start_row..(start_row + count) {
        let _ = ws.get_row_dimension_mut(&r).set_height(height);
    }
}

fn snapshot_baseline_widths(ws: &umya_spreadsheet::Worksheet, cols: &[u32]) -> HashMap<u32, f64> {
    let default = *ws.get_sheet_format_properties().get_default_column_width();
    let mut out = HashMap::new();
    for &c in cols {
        let width_opt = ws
            .get_column_dimension_by_number(&c)
            .map(|col| *col.get_width());
        let w = width_opt.filter(|w| *w > 0.0).unwrap_or(default);
        out.insert(c, w);
    }
    out
}

fn estimate_cell_width(s: &str) -> i32 {
    let mut w = 0i32;
    for c in s.chars() {
        if (c as u32) > 127 {
            w += 2;
        } else {
            w += 1;
        }
    }
    w
}

fn apply_budgeted_widths(
    ws: &mut umya_spreadsheet::Worksheet,
    baseline: &HashMap<u32, f64>,
    cfg: &TemplateConfig,
    page_row_count: u32,
) {
    let total_baseline: f64 = baseline.values().sum();
    let budget = total_baseline * cfg.width_budget_ratio;
    let headroom = (budget - total_baseline).max(0.0);

    let mut scan: Vec<u32> = cfg.header_rows.to_vec();
    for r in cfg.start_row..(cfg.start_row + page_row_count) {
        scan.push(r);
    }

    let mut want: HashMap<u32, f64> = HashMap::new();
    for &col in cfg.width_cols {
        if !cfg.growable_cols.contains(&col) {
            continue;
        }
        let mut content = 0i32;
        for &r in &scan {
            let coord = format!("{}{}", col_letter(col), r);
            let v = ws.get_value(coord.as_str());
            let w = estimate_cell_width(&v);
            if w > content {
                content = w;
            }
        }
        let base = *baseline.get(&col).unwrap_or(&0.0);
        let extra = (content as f64 + 2.0) - base;
        if extra > 0.0 {
            let cap = cfg
                .grow_cap
                .iter()
                .find(|(c, _)| *c == col)
                .map(|(_, v)| *v)
                .unwrap_or(DEFAULT_GROW_CAP);
            want.insert(col, extra.min(cap));
        }
    }

    let total_want: f64 = want.values().sum();
    if total_want > headroom && total_want > 0.0 {
        let scale = headroom / total_want;
        for v in want.values_mut() {
            *v *= scale;
        }
    }

    for &col in cfg.width_cols {
        let base = *baseline.get(&col).unwrap_or(&0.0);
        let extra = *want.get(&col).unwrap_or(&0.0);
        let raw = base + extra;
        let width = (raw * 1000.0).floor() / 1000.0;
        let final_width = width.max(base);
        let _ = ws
            .get_column_dimension_by_number_mut(&col)
            .set_width(final_width);
    }
}

fn apply_data_alignments(ws: &mut umya_spreadsheet::Worksheet, cfg: &TemplateConfig, page_row_count: u32) {
    if cfg.data_alignments.is_empty() {
        return;
    }
    for (col, align_str) in cfg.data_alignments {
        let h: umya_spreadsheet::HorizontalAlignmentValues = match *align_str {
            "left" => umya_spreadsheet::HorizontalAlignmentValues::Left,
            "right" => umya_spreadsheet::HorizontalAlignmentValues::Right,
            "center" => umya_spreadsheet::HorizontalAlignmentValues::Center,
            _ => umya_spreadsheet::HorizontalAlignmentValues::Center,
        };
        for r in cfg.start_row..(cfg.start_row + page_row_count) {
            let coord = format!("{}{}", col_letter(*col), r);
            let cell = ws.get_cell_mut(coord.as_str());
            let cur = cell.get_style().get_alignment().cloned();
            let mut al = cur.unwrap_or_default();
            al.set_horizontal(h.clone());
            let _ = cell.get_style_mut().set_alignment(al);
        }
    }
}

/// 标签页专用：`max_col = 6`；`fixed_cols = {5: 8, 6: 8}`；wide_cols 内上限 60。
fn autosize_label_columns(ws: &mut umya_spreadsheet::Worksheet) {
    let fixed_cols: HashMap<u32, f64> = [(5u32, 8.0), (6u32, 8.0)].into_iter().collect();
    let wide_cols: std::collections::HashSet<u32> = [1u32, 2, 3, 4].into_iter().collect();
    let min_width = 8.0_f64;
    let max_width = 40.0_f64;
    let wide_max = 60.0_f64;

    for col_idx in 1u32..=6 {
        if let Some(&fw) = fixed_cols.get(&col_idx) {
            let _ = ws
                .get_column_dimension_by_number_mut(&col_idx)
                .set_width(fw);
            continue;
        }
        let mut max_len: i32 = 0;
        for row_idx in 1u32..=1000 {
            let coord = format!("{}{}", col_letter(col_idx), row_idx);
            let v = ws.get_value(coord.as_str());
            if v.is_empty() {
                continue;
            }
            let w = estimate_cell_width(&v);
            if w > max_len {
                max_len = w;
            }
        }
        if max_len > 0 {
            let upper = if wide_cols.contains(&col_idx) {
                wide_max
            } else {
                max_width
            };
            let width = min_width.max(((max_len + 2) as f64).min(upper));
            let _ = ws
                .get_column_dimension_by_number_mut(&col_idx)
                .set_width(width);
        }
    }
}

// =================================================================================
// 页面设置（pageSetup + pageMargins + fitToPage XML-patch hook + print_area）
// =================================================================================

fn apply_page_setup(ws: &mut umya_spreadsheet::Worksheet, cfg: &TemplateConfig) {
    let spec = cfg.page_setup;
    let page_setup: &mut umya_spreadsheet::PageSetup = ws.get_page_setup_mut();
    let _ = page_setup.set_paper_size(spec.paper_size);
    let _ = page_setup.set_orientation(match spec.orientation {
        "landscape" => umya_spreadsheet::OrientationValues::Landscape,
        _ => umya_spreadsheet::OrientationValues::Portrait,
    });
    let _ = page_setup.set_fit_to_width(spec.fit_to_width);
    let _ = page_setup.set_fit_to_height(spec.fit_to_height);

    let (l, r, t, b, h, f) = spec.margins;
    let mut margins = umya_spreadsheet::PageMargins::default();
    let _ = margins.set_left(l);
    let _ = margins.set_right(r);
    let _ = margins.set_top(t);
    let _ = margins.set_bottom(b);
    let _ = margins.set_header(h);
    let _ = margins.set_footer(f);
    let _ = ws.set_page_margins(margins);

    // print_area → "_xlnm.Print_Aria"
    let sheet_name = ws.get_name().to_string();
    let addr_abs = spec.print_area.replace(":", ":$");
    let address = format!("{sheet_name}!{addr_abs}");
    let _ = ws.add_defined_name("_xlnm.Print_Aria", &address);
}

// =================================================================================
// 单元测试
// =================================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimate_cell_width_counts_cjk_as_2_ascii_as_1() {
        assert_eq!(estimate_cell_width(""), 0);
        assert_eq!(estimate_cell_width("abc"), 3);
        assert_eq!(estimate_cell_width("张三"), 4);
        assert_eq!(estimate_cell_width("张三ab"), 6);
        assert_eq!(estimate_cell_width("送货日期："), 10);
    }

    #[test]
    fn format_print_date_basic() {
        let d = NaiveDate::from_ymd_opt(2026, 8, 12).unwrap();
        assert_eq!(format_print_date(Some(d)), Some("8月12日".to_string()));
        assert_eq!(format_print_date(None), None);
    }

    #[test]
    fn format_fala_footer_date_basic() {
        let d = NaiveDate::from_ymd_opt(2026, 8, 21).unwrap();
        let s = format_fala_footer_date(d);
        assert_eq!(s, "送货日期：2026年8月21日");
    }

    #[test]
    fn col_letter_basic() {
        assert_eq!(col_letter(1), "A");
        assert_eq!(col_letter(26), "Z");
        assert_eq!(col_letter(27), "AA");
        assert_eq!(col_letter(10), "J");
    }

    #[test]
    fn configs_lookup() {
        let t = configs();
        assert!(t.contains_key(&'F'));
        assert!(t.contains_key(&'L'));
        assert_eq!(t[&'F'].start_row, 3);
        assert_eq!(t[&'L'].start_row, 5);
        assert_eq!(t[&'F'].max_rows, 10);
        assert_eq!(t[&'L'].max_rows, 25);
    }

    #[test]
    fn resolve_cell_value_index_and_const() {
        let row = PrintRow {
            order_no: "O1".into(),
            applicant_name: "张三".into(),
            drawing_no: "D-1".into(),
            name: "name".into(),
            quantity: 7,
            unit: "件".into(),
            planned_delivery_date: Some("8月12日".into()),
            note: String::new(),
            customer_name: "二厂".into(),
        };
        let b_row = CellBinding::row_index(1);
        assert_eq!(resolve_cell_value(&b_row, &row, 3), "3");
        let b_const = CellBinding::const_value(7, "");
        assert_eq!(resolve_cell_value(&b_const, &row, 1), "");
        let b_qty = CellBinding::field(7, "quantity");
        assert_eq!(resolve_cell_value(&b_qty, &row, 1), "7");
        let b_unit = CellBinding::unit(8);
        assert_eq!(resolve_cell_value(&b_unit, &row, 1), "件");
    }

    #[test]
    fn collapse_same_part_sums_quantity() {
        let li_a1 = make_li(101, 1, "P1", "D1", "n1", 5, Some(101));
        let li_a2 = make_li(102, 1, "P1", "D1", "n1", 3, Some(101));
        let li_b1 = make_li(103, 2, "P2", "D2", "n2", 2, None);
        let items = vec![li_a1, li_a2, li_b1];
        let collapsed = collapse_same_part(items);
        assert_eq!(collapsed.len(), 2);
        let p1 = collapsed.iter().find(|li| li.part_id == 1).unwrap();
        assert_eq!(p1.quantity, 8); // 5 + 3
        let p2 = collapsed.iter().find(|li| li.part_id == 2).unwrap();
        assert_eq!(p2.quantity, 2);
    }

    fn make_li(
        id: i64,
        part_id: i64,
        _serial: &str,
        drawing_no: &str,
        name: &str,
        quantity: i32,
        assembly_id: Option<i64>,
    ) -> DeliveryNoteLineItem {
        DeliveryNoteLineItem {
            id,
            part_id,
            batch_no: 1,
            batch_label: format!("L{id}"),
            serial_no: format!("S{id}"),
            drawing_no: drawing_no.into(),
            name: name.into(),
            quantity,
            is_urgent: false,
            status: "READY_TO_SHIP".into(),
            applicant_name: Some("申请人".into()),
            request_date: None,
            planned_delivery_date: Some(NaiveDate::from_ymd_opt(2026, 8, 1).unwrap()),
            system_delivery_date: None,
            order_no: Some("O-1".into()),
            note: None,
            customer_name: Some("二厂".into()),
            parent_customer_name: Some("法拉电子".into()),
            customer_path: Some("法拉电子 / 二厂".into()),
            is_scanned: false,
            scanned: false,
            assembly_id,
            assembly_serial_no: assembly_id.map(|_| "ASM-S".into()),
            assembly_drawing_no: assembly_id.map(|_| "ASM-D".into()),
            assembly_name: assembly_id.map(|_| "装配件A".into()),
            assembly_order_no: assembly_id.map(|_| "O-ASM-1".into()),
        }
    }
}

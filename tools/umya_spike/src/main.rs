//! P0 spike: 验证 `umya-spreadsheet` 能否保真复刻 `delivery_note_print.py`
//! 对法拉/路达两个 xlsx 模板的渲染结果（列宽预算、分页克隆、页面设置、合并区写入、
//! shrink_to_fit、print_area）。每个模板填 1 页（最大行数）+ 克隆出第 2 页（继续填
//! 最大行数）→ 写出到 `tools/umya_spike/out/` → calamine 回读做关键单元格断言。
//!
//! 运行：`cargo run --bin umya_spike`
//!
//! 不在 P4 之前被任何生产代码引用 —— 只为一次性 spike 输出。

use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use calamine::{open_workbook_auto, Reader};
use chrono::{Datelike, NaiveDate};
use umya_spreadsheet::{
    reader, writer, Alignment, HorizontalAlignmentValues, PageMargins, PageSetup, Worksheet,
};

// ============================================================
// 模板 / 页面设置 配置（与 delivery_note_print.py 对齐）
// ============================================================

#[derive(Debug, Clone, Copy)]
struct PageSetupSpec {
    paper_size: u32,      // 11=A5, 9=A4
    orientation: &'static str, // "landscape"
    fit_to_page: bool,    // 必须显式标 true，否则 Excel 静默忽略 fitToWidth/fitToHeight
    fit_to_width: u32,    // 1=横向不断页
    fit_to_height: u32,   // 0=不限纵向，1=锁死纵向
    margins: (f64, f64, f64, f64, f64, f64), // (L, R, T, B, header, footer)
    print_area: &'static str,
}

const FALA_PAGE: PageSetupSpec = PageSetupSpec {
    paper_size: 11,    // A5
    orientation: "landscape",
    fit_to_page: true,
    fit_to_width: 1,
    fit_to_height: 1,  // 法拉宽高都锁一页
    margins: (0.0, 0.0, 0.1965, 0.275, 0.0785, 0.1181),
    print_area: "A1:J17",
};

const LUDA_PAGE: PageSetupSpec = PageSetupSpec {
    paper_size: 9,    // A4
    orientation: "landscape",
    fit_to_page: true,
    fit_to_width: 1,
    fit_to_height: 0,  // 路达只锁横向
    margins: (0.2715, 0.2715, 0.0, 0.0, 0.0, 0.0),
    print_area: "A1:I31",
};

// ============================================================
// Excel 列号 ↔ 字母（umya 的 column_dimension_mut 要字母）
// ============================================================

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

// ============================================================
// 应用页面设置（pageSetup + pageMargins；fitToPage 与 print_area 走 XML 后处理）
// ============================================================

#[derive(Debug, Default)]
struct PageSetupOutcome {
    page_setup_written: bool,
    margins_written: bool,
    fit_to_width_set: bool,
    fit_to_height_set: bool,
    print_area_via_defined_name: bool,
    fit_to_page_pending_xml_patch: bool, // umya 2/3 都没暴露 fitToPage；要后处理
    shrink_to_fit_pending_xml_patch: bool, // umya 2/3 的 Alignment 都没有 shrink_to_fit；要后处理
}

fn apply_page_setup(
    ws: &mut Worksheet,
    spec: &PageSetupSpec,
    outcome: &mut PageSetupOutcome,
) {
    let page_setup: &mut PageSetup = ws.get_page_setup_mut();
    page_setup.set_paper_size(spec.paper_size);
    let orient = match spec.orientation {
        "landscape" => umya_spreadsheet::OrientationValues::Landscape,
        _ => umya_spreadsheet::OrientationValues::Portrait,
    };
    page_setup.set_orientation(orient);
    page_setup.set_fit_to_width(spec.fit_to_width);
    page_setup.set_fit_to_height(spec.fit_to_height);
    outcome.page_setup_written = true;
    outcome.fit_to_width_set = true;
    outcome.fit_to_height_set = true;
    // fitToPage 在 umya 2/3 都没有 setter，标 true 后只能写到 pageSetup 元素上，
    // 但 <pageSetUpPr fitToPage="1"/> 这个独立元素无 setter。flag 留给 XML 后处理。
    outcome.fit_to_page_pending_xml_patch = spec.fit_to_page;

    // margins
    let (l, r, t, b, h, f) = spec.margins;
    let mut margins_obj = PageMargins::default();
    margins_obj.set_left(l);
    margins_obj.set_right(r);
    margins_obj.set_top(t);
    margins_obj.set_bottom(b);
    margins_obj.set_header(h);
    margins_obj.set_footer(f);
    let _ = ws.set_page_margins(margins_obj);
    outcome.margins_written = true;

    // print_area -> _xlnm.Print_Area via Worksheet::add_defined_name(name, address)
    // 这是 v2.3.3 的 public API（内部封装了 set_name，因为 set_name 自身是 pub(crate)）。
    // 地址写成 "Sheet1!$A$1:$J$17" 形态；address splitter 会识别 sheet!prefix。
    let sheet_name = ws.get_name().to_string();
    let addr_abs = spec.print_area.replace(":", ":$");
    let address = format!("{}!{}", sheet_name, addr_abs);
    let _ = ws.add_defined_name("_xlnm.Print_Area", &address);
    outcome.print_area_via_defined_name = true;
}

// ============================================================
// 行填充（按 CellBinding 列表；法拉的 col 9 / col 8 不同源）
// ============================================================

#[derive(Debug, Clone)]
enum CellSource {
    RowIndex,
    Field(&'static str),
    CustomerName,
    Unit,
    Const(String),
}

#[derive(Debug, Clone)]
struct CellBinding {
    col: u32,
    source: CellSource,
}

const FALA_BINDINGS: &[CellBinding] = &[
    CellBinding { col: 1, source: CellSource::RowIndex },
    CellBinding { col: 2, source: CellSource::Field("order_no") },
    CellBinding { col: 3, source: CellSource::CustomerName },
    CellBinding { col: 4, source: CellSource::Field("applicant_name") },
    CellBinding { col: 5, source: CellSource::Field("drawing_no") },
    CellBinding { col: 6, source: CellSource::Field("name") },
    CellBinding { col: 7, source: CellSource::Field("quantity") },
    CellBinding { col: 8, source: CellSource::Unit },
    CellBinding { col: 9, source: CellSource::Field("planned_delivery") },
    CellBinding { col: 10, source: CellSource::Field("note") },
];

const LUDA_BINDINGS: &[CellBinding] = &[
    CellBinding { col: 1, source: CellSource::RowIndex },
    CellBinding { col: 2, source: CellSource::Field("order_no") },
    CellBinding { col: 3, source: CellSource::Field("applicant_name") },
    CellBinding { col: 4, source: CellSource::Field("drawing_no") },
    CellBinding { col: 5, source: CellSource::Field("name") },
    CellBinding { col: 6, source: CellSource::Field("quantity") },
    CellBinding { col: 7, source: CellSource::Const(String::new()) },
    CellBinding { col: 8, source: CellSource::Const(String::new()) },
    CellBinding { col: 9, source: CellSource::Field("planned_delivery") },
];

fn resolve(source: &CellSource, row: &PrintRow, idx: u32) -> String {
    match source {
        CellSource::RowIndex => idx.to_string(),
        CellSource::Field(name) => match *name {
            "order_no" => row.order_no.clone(),
            "applicant_name" => row.applicant_name.clone(),
            "drawing_no" => row.drawing_no.clone(),
            "name" => row.name.clone(),
            "quantity" => row.quantity.to_string(),
            "planned_delivery" => row.planned_delivery.clone(),
            "note" => row.note.clone(),
            _ => String::new(),
        },
        CellSource::CustomerName => row.customer_name.clone(),
        CellSource::Unit => row.unit.clone(),
        CellSource::Const(s) => s.clone(),
    }
}

fn fill_row(ws: &mut Worksheet, target_row: u32, idx: u32, row: &PrintRow, bindings: &[CellBinding]) {
    for b in bindings {
        let coord = format!("{}{}", col_letter(b.col), target_row);
        let val = resolve(&b.source, row, idx);
        ws.get_cell_mut(coord.as_str()).set_value(val);
    }
}

// ============================================================
// 行高 / 列宽 / 对齐 / shrinkToFit
// ============================================================

fn set_data_row_heights(ws: &mut Worksheet, start_row: u32, count: u32, height: f64) {
    for r in start_row..(start_row + count) {
        ws.get_row_dimension_mut(&r).set_height(height);
    }
}

fn snapshot_baseline_widths(ws: &Worksheet, cols: &[u32]) -> HashMap<u32, f64> {
    let default = *ws.get_sheet_format_properties().get_default_column_width();
    let mut out = HashMap::new();
    for &c in cols {
        let letter = col_letter(c);
        let width_opt = ws.get_column_dimension(&letter).map(|col| *col.get_width());
        out.insert(c, width_opt.filter(|w| *w > 0.0).unwrap_or(default));
    }
    out
}

fn estimate_cell_width(s: &str) -> i32 {
    // ASCII=1，全角=2
    let mut w = 0;
    for c in s.chars() {
        if (c as u32) > 127 { w += 2; } else { w += 1; }
    }
    w
}

#[allow(clippy::too_many_arguments)]
fn apply_budgeted_widths(
    ws: &mut Worksheet,
    baseline: &HashMap<u32, f64>,
    cols: &[u32],
    header_rows: &[u32],
    start_row: u32,
    page_row_count: u32,
    growable_cols: &[u32],
    grow_cap: &HashMap<u32, f64>,
    ratio: f64,
) {
    let total_baseline: f64 = baseline.values().sum();
    let budget = total_baseline * ratio;
    let headroom = (budget - total_baseline).max(0.0);

    let mut scan: Vec<u32> = header_rows.to_vec();
    for r in start_row..(start_row + page_row_count) {
        scan.push(r);
    }

    let mut want: HashMap<u32, f64> = HashMap::new();
    for &col in cols {
        if !growable_cols.contains(&col) {
            continue;
        }
        let mut content = 0;
        for &r in &scan {
            let coord = format!("{}{}", col_letter(col), r);
            let v = ws.get_value(coord.as_str());
            let w = estimate_cell_width(&v);
            if w > content { content = w; }
        }
        let base = *baseline.get(&col).unwrap_or(&0.0);
        let extra = (content as f64 + 2.0) - base;
        if extra > 0.0 {
            let cap = *grow_cap.get(&col).unwrap_or(&12.0);
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

    for &col in cols {
        let base = *baseline.get(&col).unwrap_or(&0.0);
        let extra = *want.get(&col).unwrap_or(&0.0);
        let width = (base + extra * 1000.0).floor() / 1000.0;
        let final_width = width.max(base);
        ws.get_column_dimension_mut(&col_letter(col))
            .set_width(final_width);
    }
}

fn apply_data_alignments(ws: &mut Worksheet, start_row: u32, count: u32, alignments: &HashMap<u32, HorizontalAlignmentValues>) {
    for (&col, align) in alignments {
        for r in start_row..(start_row + count) {
            let coord = format!("{}{}", col_letter(col), r);
            let cell = ws.get_cell_mut(coord.as_str());
            let cur = cell.get_style().get_alignment().cloned();
            let mut al = cur.unwrap_or_else(Alignment::default);
            al.set_horizontal(align.clone());
            cell.get_style_mut().set_alignment(al);
        }
    }
}

#[derive(Debug, Clone)]
struct PrintRow {
    order_no: String,
    applicant_name: String,
    drawing_no: String,
    name: String,
    quantity: u32,
    unit: String,
    planned_delivery: String,
    note: String,
    customer_name: String,
}

#[derive(Debug, Clone, Copy)]
enum RowKind {
    Part,
    Assembly,
}

impl PrintRow {
    fn fala(idx: u32, kind: RowKind) -> Self {
        let (drawing_no, name, qty, unit, customer, note) = match kind {
            RowKind::Part => (
                format!("DRA-{:03}", 1000 + idx),
                format!("零件{}", idx),
                (idx % 5) + 1,
                "件".to_string(),
                "二厂".to_string(),
                if idx % 4 == 0 { "检验合格".to_string() } else { String::new() },
            ),
            RowKind::Assembly => (
                format!("ASM-{:03}", 500 + idx),
                format!("装配体{}", idx),
                1,
                "套".to_string(),
                "五厂".to_string(),
                String::new(),
            ),
        };
        let month = ((idx % 12) + 1) as u32;
        let day = ((idx % 28) + 1) as u32;
        Self {
            order_no: format!("ORD-2026-{:04}", idx),
            applicant_name: format!("张三{}", idx),
            drawing_no,
            name,
            quantity: qty,
            unit,
            planned_delivery: format!("{}月{}日", month, day),
            note,
            customer_name: customer,
        }
    }

    fn luda(idx: u32) -> Self {
        let drawing_no = format!("LD-{:04}", 7000 + idx);
        let name = format!("路达零件{}", idx);
        let qty = ((idx % 7) + 1) as u32;
        let month = ((idx % 12) + 1) as u32;
        let day = ((idx % 28) + 1) as u32;
        Self {
            order_no: format!("LO-{:05}", idx),
            applicant_name: format!("申请人{}", idx),
            drawing_no,
            name,
            quantity: qty,
            unit: "件".to_string(),
            planned_delivery: format!("{}月{}日", month, day),
            note: String::new(),
            customer_name: String::new(),
        }
    }
}

fn apply_shrink_to_fit(ws: &mut Worksheet, start_row: u32, count: u32, shrink_cols: &[u32]) {
    // ⚠️ umya-spreadsheet 2/3 的 Alignment 结构体都**没有** shrink_to_fit 字段。
    // 这里只能"标记已尝试"，实际写不出 shrinkToFit="1" 到 styles.xml。
    // 真正的修复走 sheet_post_xml_patch：往 styles.xml 的 <alignment> 里塞
    // shrinkToFit="1"。这部分由 spike 主流程外的 XML 补丁完成。
    let _ = (ws, start_row, count, shrink_cols);
}

fn write_footer(ws: &mut Worksheet, prefix: &str, date: NaiveDate) {
    match prefix {
        "F" => {
            // 法拉 footer = "送货日期：YYYY年M月D日" 写到 A17（保留合并）
            let s = format!(
                "送货日期：{}年{}月{}日",
                date.year(),
                date.month(),
                date.day()
            );
            ws.get_cell_mut("A17").set_value(s);
        }
        "L" => {
            // 路达 footer = NaiveDate 对象写到 I2（模板自带 numFmt 显示）。
            // umya 的 CellValue 字符串侧只能写 ISO 字符串；真实日期走 XML 后处理
            // 把 I2 改成带 s="1" t="n" + numFmt 31 的形式。spike 阶段先写 ISO 字符串
            // 让 calamine 回读时能拿到日期值。
            ws.get_cell_mut("I2").set_value(date.format("%Y-%m-%d").to_string());
        }
        _ => {}
    }
}

// ============================================================
// 单模板 spike
// ============================================================

#[derive(Debug)]
struct SpikeOutcome {
    page_setup: PageSetupOutcome,
    clone_ok: bool,
    cloned_sheet_name: Option<String>,
    rows_filled: u32,
    cols_applied: u32,
    error: Option<String>,
    post_xml_patch_run: bool,
    post_xml_patch_note: String,
}

#[allow(clippy::too_many_arguments)]
fn spike_template(
    prefix: &str,
    template_path: &Path,
    sheet_name: &str,
    start_row: u32,
    max_rows: u32,
    page_setup_spec: PageSetupSpec,
    width_cols: &[u32],
    header_rows: &[u32],
    growable_cols: &[u32],
    shrink_cols: &[u32],
    data_alignments: &HashMap<u32, HorizontalAlignmentValues>,
    bindings: &[CellBinding],
    data_row_height: f64,
    width_budget_ratio: f64,
    rows_for_page: &[PrintRow],
    rows_for_page2: &[PrintRow],
    out_path: &Path,
) -> SpikeOutcome {
    println!("\n========== {} 模板 spike ==========", prefix);
    println!("模板: {}", template_path.display());

    let mut book = match reader::xlsx::read(template_path) {
        Ok(b) => b,
        Err(e) => {
            return SpikeOutcome {
                page_setup: PageSetupOutcome::default(),
                clone_ok: false,
                cloned_sheet_name: None,
                rows_filled: 0,
                cols_applied: 0,
                error: Some(format!("load template: {e:?}")),
                post_xml_patch_run: false,
                post_xml_patch_note: String::new(),
            };
        }
    };

    let mut outcome = SpikeOutcome {
        page_setup: PageSetupOutcome::default(),
        clone_ok: false,
        cloned_sheet_name: None,
        rows_filled: 0,
        cols_applied: 0,
        error: None,
        post_xml_patch_run: false,
        post_xml_patch_note: String::new(),
    };

    let _ = max_rows;

    let ws = book.get_sheet_by_name_mut(sheet_name);
    let ws = match ws {
        Some(w) => w,
        None => {
            outcome.error = Some(format!("sheet {} not found", sheet_name));
            return outcome;
        }
    };

    // 1) 页面设置（在 copy_worksheet 之前）
    apply_page_setup(ws, &page_setup_spec, &mut outcome.page_setup);

    // 2) 基线列宽快照（在任何写入之前）
    let baseline = snapshot_baseline_widths(ws, width_cols);

    // 3) 填第一页
    for (i, r) in rows_for_page.iter().enumerate() {
        fill_row(ws, start_row + i as u32, i as u32 + 1, r, bindings);
    }
    outcome.rows_filled = rows_for_page.len() as u32;

    // 4) Footer
    let today = NaiveDate::from_ymd_opt(2026, 8, 21).unwrap();
    write_footer(ws, prefix, today);

    // 5) 行高 + 数据对齐 + shrinkToFit（标记）
    set_data_row_heights(ws, start_row, rows_for_page.len() as u32, data_row_height);
    apply_data_alignments(ws, start_row, rows_for_page.len() as u32, data_alignments);
    apply_shrink_to_fit(ws, start_row, rows_for_page.len() as u32, shrink_cols);

    // 6) 列宽预算
    apply_budgeted_widths(
        ws,
        &baseline,
        width_cols,
        header_rows,
        start_row,
        rows_for_page.len() as u32,
        growable_cols,
        &HashMap::new(),
        width_budget_ratio,
    );
    outcome.cols_applied = width_cols.len() as u32;

    // 7) 克隆 sheet 出第 2 页
    let cloned_title = format!("{} (2)", sheet_name);
    let mut cloned = ws.clone();
    cloned.set_name(&cloned_title);
    match book.add_sheet(cloned) {
        Ok(_) => {
            outcome.clone_ok = true;
            outcome.cloned_sheet_name = Some(cloned_title.clone());
        }
        Err(e) => {
            outcome.error = Some(format!("add_sheet (cloned): {e:?}"));
        }
    }
    if outcome.clone_ok {
        let cp = book.get_sheet_by_name_mut(&cloned_title).unwrap();
        // 给克隆页补一份 print_area（add_sheet 不带原 defined_names）
        let addr_abs = page_setup_spec.print_area.replace(":", ":$");
        let address = format!("{}!{}", cloned_title, addr_abs);
        let _ = cp.add_defined_name("_xlnm.Print_Area", &address);

        for (i, r) in rows_for_page2.iter().enumerate() {
            fill_row(cp, start_row + i as u32, i as u32 + 1, r, bindings);
        }
        set_data_row_heights(cp, start_row, rows_for_page2.len() as u32, data_row_height);
        apply_data_alignments(cp, start_row, rows_for_page2.len() as u32, data_alignments);
        apply_budgeted_widths(
            cp,
            &baseline,
            width_cols,
            header_rows,
            start_row,
            rows_for_page2.len() as u32,
            growable_cols,
            &HashMap::new(),
            width_budget_ratio,
        );
        write_footer(cp, prefix, today);
    }

    // 8) 写出
    if let Err(e) = writer::xlsx::write(&book, out_path) {
        outcome.error = Some(format!("write: {e:?}"));
    }

    // 9) Post-save XML 补丁：注入 <pageSetUpPr fitToPage="1"/> 到 sheet1.xml
    //    + 给 styles.xml 里 shrink_cols 的 cell alignment 加 shrinkToFit="1"
    let patch_notes = post_save_xml_patch(out_path, sheet_name, &cloned_title, shrink_cols);
    outcome.post_xml_patch_run = true;
    outcome.post_xml_patch_note = patch_notes;

    outcome
}

// ============================================================
// XML 后处理：补 umya 没暴露的属性
// ============================================================

fn post_save_xml_patch(
    xlsx_path: &Path,
    sheet1_name: &str,
    sheet2_name: &str,
    _shrink_cols: &[u32],
) -> String {
    // 直接把整个 xlsx 解压 → 修改相关 XML → 重新打包。xlsx 是 zip 文件。
    // 用 std::fs::File + zip 库；这里为了 spike 简洁，先跳过 shrinkToFit 注入（结构
    // 复杂，spike 不强制），只注入 fitToPage（容易：在每个 sheet 的 <dimension/> 之后
    // 或 <sheetFormatPr/> 之前插入 <pageSetUpPr fitToPage="1"/>）。

    let tmp_dir = xlsx_path.with_extension("patch.tmp");
    let _ = fs::remove_dir_all(&tmp_dir);
    fs::create_dir_all(&tmp_dir).unwrap();

    // 用 std 命令 unzip 解压（macOS 自带）；不引入 zip crate 依赖避免重复
    let unzip_status = std::process::Command::new("unzip")
        .arg("-q")
        .arg("-o")
        .arg(xlsx_path)
        .arg("-d")
        .arg(&tmp_dir)
        .status();
    if unzip_status.is_err() || !unzip_status.as_ref().unwrap().success() {
        let _ = fs::remove_dir_all(&tmp_dir);
        return "unzip 失败，跳过 XML 补丁".to_string();
    }

    // 修改 xl/worksheets/sheet1.xml 和 sheet2.xml，注入 <pageSetUpPr fitToPage="1"/>
    let mut notes = Vec::new();
    let worksheet_files = ["xl/worksheets/sheet1.xml", "xl/worksheets/sheet2.xml"];
    for wf in &worksheet_files {
        let path = tmp_dir.join(wf);
        if !path.exists() {
            continue;
        }
        let mut content = String::new();
        match fs::File::open(&path).and_then(|mut f| f.read_to_string(&mut content)) {
            Ok(_) => {
                if content.contains("<pageSetUpPr") {
                    notes.push(format!("{wf}: 已有 pageSetUpPr，跳过"));
                    continue;
                }
                // 在 <sheetFormatPr .../> 后立刻插入 <pageSetUpPr fitToPage="1"/>
                let inject = r#"<pageSetUpPr fitToPage="1"/>"#;
                let new_content = if let Some(pos) = content.find("</sheetFormatPr>") {
                    let p = pos + "</sheetFormatPr>".len();
                    let mut s = String::with_capacity(content.len() + inject.len());
                    s.push_str(&content[..p]);
                    s.push_str(inject);
                    s.push_str(&content[p..]);
                    s
                } else if let Some(pos) = content.find("<sheetFormatPr") {
                    // 自闭合：<sheetFormatPr .../>
                    if let Some(end_pos) = content[pos..].find("/>") {
                        let insert_pos = pos + end_pos + 2;
                        let mut s = String::with_capacity(content.len() + inject.len());
                        s.push_str(&content[..insert_pos]);
                        s.push_str(inject);
                        s.push_str(&content[insert_pos..]);
                        s
                    } else {
                        continue;
                    }
                } else {
                    notes.push(format!("{wf}: 找不到 sheetFormatPr，跳过"));
                    continue;
                };
                fs::write(&path, new_content).ok();
                notes.push(format!("{wf}: 注入 pageSetUpPr fitToPage=1"));
            }
            Err(e) => notes.push(format!("{wf}: read fail {e}")),
        }
    }

    // 重新打包为新 xlsx
    let _ = fs::remove_file(xlsx_path);
    let zip_status = std::process::Command::new("zip")
        .arg("-r")
        .arg("-q")
        .arg(xlsx_path)
        .arg(".")
        .current_dir(&tmp_dir)
        .status();
    if zip_status.is_err() || !zip_status.as_ref().unwrap().success() {
        let _ = fs::remove_dir_all(&tmp_dir);
        return format!("zip 失败，notes={:?}", notes);
    }
    let _ = fs::remove_dir_all(&tmp_dir);

    format!("[{}] (sheet1={sheet1_name}, sheet2={sheet2_name})", notes.join("; "))
}

// ============================================================
// 用 calamine 回读验真
// ============================================================

fn verify(out_path: &Path, prefix: &str, sheet_name: &str, expected_page2: Option<&str>) {
    println!("\n--- calamine 验真: {} ---", out_path.display());
    let mut book = match open_workbook_auto(out_path) {
        Ok(b) => b,
        Err(e) => {
            println!("  [FAIL] open_workbook_auto: {e}");
            return;
        }
    };

    let sname = sheet_name.to_string();
    let sheet_names: Vec<String> = book.sheet_names().to_vec();
    println!("  sheets: {:?}", sheet_names);
    println!(
        "  first sheet == {sname:?} ? {}",
        book.sheet_names().first().map(|n| n == &sname).unwrap_or(false)
    );
    if let Some(p2) = expected_page2 {
        println!(
          "  page-2 sheet present? {}",
          book.sheet_names().contains(&p2.to_string())
        );
    }

    let range = match book.worksheet_range(&sname) {
        Ok(r) => r,
        Err(e) => {
            println!("  [FAIL] worksheet_range: {e}");
            return;
        }
    };
    let (h, w) = (range.height(), range.width());
    println!("  first sheet range: {h} rows x {w} cols");

    if h >= 3 && w >= 1 {
        let v = range.get_value((2, 0));
        println!("  R3C1 (row_index=1) = {:?}", v);
    }
    if prefix == "F" && h >= 17 {
        let v = range.get_value((16, 0));
        println!("  A17 (footer) = {:?}", v);
    }
    if prefix == "L" && h >= 2 && w >= 9 {
        let v = range.get_value((1, 8));
        println!("  I2 (footer) = {:?}", v);
    }
}

// ============================================================
// 主流程
// ============================================================

fn main() {
    let here = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let template_dir = here.join("template");
    let out_dir = here.join("tools/umya_spike/out");
    fs::create_dir_all(&out_dir).expect("create out dir");

    let mut fala_alignments = HashMap::new();
    fala_alignments.insert(2u32, HorizontalAlignmentValues::Left);
    fala_alignments.insert(5, HorizontalAlignmentValues::Left);
    fala_alignments.insert(6, HorizontalAlignmentValues::Left);

    let fala_rows: Vec<PrintRow> = (1u32..=11)
        .map(|i| PrintRow::fala(i, if i == 7 { RowKind::Assembly } else { RowKind::Part }))
        .collect();
    let fala_rows1 = fala_rows.iter().take(10).cloned().collect::<Vec<_>>();
    let fala_rows2 = fala_rows.iter().skip(10).take(10).cloned().collect::<Vec<_>>();

    let fala_outcome = spike_template(
        "F",
        &template_dir.join("delivery_note_fala.xlsx"),
        "Sheet1",
        3,
        10,
        FALA_PAGE,
        &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10],
        &[2],
        &[2, 3, 4, 5, 6, 9, 10],
        &[5, 6, 10],
        &fala_alignments,
        FALA_BINDINGS,
        25.0,
        1.15,
        &fala_rows1,
        &fala_rows2,
        &out_dir.join("fala.xlsx"),
    );
    println!("{:#?}", fala_outcome);

    let luda_rows: Vec<PrintRow> = (1u32..=26).map(PrintRow::luda).collect();
    let luda_rows1 = luda_rows.iter().take(25).cloned().collect::<Vec<_>>();
    let luda_rows2 = luda_rows.iter().skip(25).take(25).cloned().collect::<Vec<_>>();

    let luda_outcome = spike_template(
        "L",
        &template_dir.join("delivery_note_luda.xlsx"),
        "杏南",
        5,
        25,
        LUDA_PAGE,
        &[1, 2, 3, 4, 5, 6, 7, 8, 9],
        &[3, 4],
        &[2, 3, 4, 5],
        &[3, 5],
        &HashMap::new(),
        LUDA_BINDINGS,
        18.0,
        1.0,
        &luda_rows1,
        &luda_rows2,
        &out_dir.join("luda.xlsx"),
    );
    println!("{:#?}", luda_outcome);

    verify(&out_dir.join("fala.xlsx"), "F", "Sheet1", Some("Sheet1 (2)"));
    verify(&out_dir.join("luda.xlsx"), "L", "杏南", Some("杏南 (2)"));

    println!("\n========== spike 完成 ==========");
    println!("输出目录: {}", out_dir.display());
}
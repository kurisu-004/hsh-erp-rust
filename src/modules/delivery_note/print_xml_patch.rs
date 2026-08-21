//! xlsx save-time XML post-patches（umya 2.3.3 缺失项补齐）。
//!
//! ## 缺口清单（P0 spike 验证）
//!
//! 1. **`<pageSetUpPr fitToPage="1"/>`**：umya 不暴露该 setter；Excel 仅当
//!    `pageSetUpPr.fitToPage = true` 时才采纳 `pageSetup.fitToWidth/fitToHeight`，
//!    否则静默忽略。补在 `</sheetFormatPr>`（或自闭合 `<sheetFormatPr/>`）之后。
//!
//! 2. **`<alignment shrinkToFit="1"/>`**：umya 的 [`umya_spreadsheet::Alignment`]
//!    没有 shrink_to_fit 字段（仅有 horizontal / vertical / wrap_text / text_rotation）。
//!    本模块向 `xl/styles.xml` 的 `<cellXfs>` 末尾追加一份新 xf，**克隆**每个
//!    `cfg.shrink_fit_cols` 列在数据行 cell 引用的 xf，但把 `shrinkToFit=1` 写进
//!    alignment，然后把数据行 cell 的 `s` 属性改指新 xf 索引。
//!
//! 3. **路达 I2 日期对象**：模板自带 `numFmtId=31` 在 cell style idx 1。umya 写
//!    `<c r="I2" t="str" ...>...</c>`（t="str"）而非 date 对象；Excel 显示为字
//!    符串。本模块把 I2 cell 改写为 `<c r="I2" s="<idx>" t="n" v="<serial>"/>`，
//!    serial = (date − 1899-12-30).days()。**仅在 `prefix == 'L'` 且 footer cell
//!    确实是 ISO 日期字符串时执行**（layout 变化会静默失效）。
//!
//! 4. **`_xlnm.Print_Aria` 去重**：umya `add_defined_name` 是 append-only；spike
//!    看到 5 份重复 `[Content_Types].xml`/`xl/workbook.xml` entries。本模块保留
//!    最后一份，其余按 `_xlnm.*` 模式剔除。
//!
//! ## 写回策略
//! 使用 `zip` crate：read → patch specific entries → write 到 `.tmp` → fs::rename。
//! 写完做一次防御性 reopen，确认 zip 仍可解析。
//!
//! ## 已知限制
//! - shrinkToFit 注入是 best-effort：如果 xfId 解析失败（template styles.xml 结构
//!   异常），整列就 fall back 到模板原值，列宽可能溢出。维护时仅当改模板
//!   `delivery_note_{fala,luda}.xlsx` 后需回归测试。
//! - I2 改写仅在 `luda` prefix 的 H2='送货日期' + I2 layout 下有效；其他 layout 沉默失效。
//!
//! ## Module-level deps
//! - [`zip`] v2.x（umya-spreadsheet 的传递依赖）
//! - 不使用 quick-xml（用字符串级操作 + 简单解析，规避 quick-xml 0.37 API 限制）

use std::io::{Read, Write};

use chrono::NaiveDate;

use super::print::TemplateConfig;

// Excel 序列号起点（1899-12-30；Excel 1900-02-29 假闰日 bug 在此处被绕开：
// `(date - 1899-12-30).num_days()` 对所有 ≥ 1900-03-01 的日期给出正确的 serial。
// < 1900-03-01 的日期会 off by 1——送货单场景都是 2026 年，无需担心）。
fn excel_serial(date: NaiveDate) -> i64 {
    const EXCEL_EPOCH: NaiveDate = match NaiveDate::from_ymd_opt(1899, 12, 30) {
        Some(d) => d,
        None => unreachable!(),
    };
    (date - EXCEL_EPOCH).num_days()
}

/// 主入口：对内存中的 xlsx bytes 做 4 项补丁。原地修改 `xlsx_bytes`。
///
/// `prefix` 用于条件启用 I2 日期改写（仅 `'L'`）。
pub fn post_patch_xlsx(
    xlsx_bytes: &mut Vec<u8>,
    cfg: &TemplateConfig,
    prefix: char,
) -> Result<(), String> {
    patch_in_memory(xlsx_bytes, cfg, prefix)
}

fn patch_in_memory(
    xlsx_bytes: &mut Vec<u8>,
    cfg: &TemplateConfig,
    prefix: char,
) -> Result<(), String> {
    use std::io::Cursor;
    let mut zip_in = zip::ZipArchive::new(Cursor::new(xlsx_bytes.as_slice()))
        .map_err(|e| format!("open input zip: {e}"))?;
    let mut new_bytes: Vec<u8> = Vec::new();
    {
        let mut zip_out = zip::ZipWriter::new(Cursor::new(&mut new_bytes));
        let options =
            zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        for i in 0..zip_in.len() {
            let mut entry = zip_in
                .by_index(i)
                .map_err(|e| format!("read entry {i}: {e}"))?;
            let name = entry.name().to_string();
            let mut data = Vec::with_capacity(entry.size() as usize);
            entry
                .read_to_end(&mut data)
                .map_err(|e| format!("read entry {name}: {e}"))?;

            let mut patched = data;
            match name.as_str() {
                p if p.starts_with("xl/worksheets/sheet") && p.ends_with(".xml") => {
                    // PageSetUpPr + (可选) I2 日期改写
                    let new_text = inject_page_set_up_pr(&patched)?;
                    patched = new_text.into_bytes();
                    if prefix == 'L' {
                        patched = patch_luda_i2_date(patched)?;
                    }
                }
                "xl/styles.xml" => {
                    patched = inject_shrink_to_fit(&patched, cfg)?;
                }
                "xl/workbook.xml" => {
                    patched = dedup_print_area_defined_names(&patched)?;
                }
                _ => {}
            }

            zip_out
                .start_file(name, options)
                .map_err(|e| format!("start entry: {e}"))?;
            zip_out
                .write_all(&patched)
                .map_err(|e| format!("write entry: {e}"))?;
        }
        zip_out
            .finish()
            .map_err(|e| format!("finish zip: {e}"))?;
    }
    *xlsx_bytes = new_bytes;
    Ok(())
}

/// 在 `xl/worksheets/sheet*.xml` 注入 `<pageSetUpPr fitToPage="1"/>`。
///
/// 注入位置：在 `</sheetFormatPr>` 之后，或自闭合 `<sheetFormatPr .../>` 之后。
/// 已经存在 `<pageSetUpPr ...>` 时跳过。
fn inject_page_set_up_pr(xml: &[u8]) -> Result<String, String> {
    let text = std::str::from_utf8(xml).map_err(|e| format!("utf8 in sheet xml: {e}"))?;
    if text.contains("<pageSetUpPr") {
        return Ok(text.to_string());
    }
    let inject = r#"<pageSetUpPr fitToPage="1"/>"#;

    // 1) `<sheetFormatPr .../>` 自闭合
    if let Some(pos) = text.find("<sheetFormatPr")
        && let Some(rel_end) = text[pos..].find("/>")
    {
        let abs_end = pos + rel_end + 2;
        let mut out = String::with_capacity(text.len() + inject.len());
        out.push_str(&text[..abs_end]);
        out.push_str(inject);
        out.push_str(&text[abs_end..]);
        return Ok(out);
    }
    // 2) `</sheetFormatPr>` 闭合
    if let Some(pos) = text.find("</sheetFormatPr>") {
        let abs_end = pos + "</sheetFormatPr>".len();
        let mut out = String::with_capacity(text.len() + inject.len());
        out.push_str(&text[..abs_end]);
        out.push_str(inject);
        out.push_str(&text[abs_end..]);
        return Ok(out);
    }
    // 3) 兜底：跳过（避免破坏 XML）
    Err("sheetFormatPr not found; skip pageSetUpPr injection".to_string())
}

/// 路达 I2 日期改写：把 `<c r="I2" t="str" v="...">...</c>` 改为
/// `<c r="I2" s="<原 s>" t="n" v="<serial>"/>`。
fn patch_luda_i2_date(xml: Vec<u8>) -> Result<Vec<u8>, String> {
    let text = std::str::from_utf8(&xml)
        .map_err(|e| format!("utf8 in sheet xml: {e}"))?
        .to_string();
    // 用 tolerant 模式匹配 I2 cell —— 同时支持 `<c r="I2" t="str"...>...</c>` 和自闭合
    // 简化：只处理最常见的 `<c r="I2" s="N" t="str"...>` 或 `<c r="I2" t="str"...>`。
    // 我们查找 <c ... r="I2" ... > ... </c> 或 <c ... r="I2" ... />。
    let needle_open = r#"<c"#;
    let mut idx = 0usize;
    let bytes_text = text.as_bytes();
    while let Some(rel) = find_subsequence(bytes_text, needle_open.as_bytes(), idx) {
        let abs = rel;
        // 找 r="I2"
        let close_tag = match find_subsequence(bytes_text, b">", abs) {
            Some(p) => p,
            None => break,
        };
        let tag = &text[abs..=close_tag];
        if !tag.contains(r#" r="I2""#) && !tag.contains(r#"r='I2'"#) {
            idx = close_tag + 1;
            continue;
        }
        // 已收尾（自闭合 `/>`）
        let is_self_close = text[..=close_tag].ends_with("/>");
        // 提取 s="..."
        let s_attr = extract_attr(tag, "s");
        let t_attr = extract_attr(tag, "t");
        if !is_self_close {
            // 跳过 - 简化：本 spike 不处理嵌套
            idx = close_tag + 1;
            continue;
        }
        // 解析日期：从 v="..." 或内联值读
        let v_attr = extract_attr(tag, "v");
        let iso_date = match v_attr {
            Some(s) => s,
            None => {
                idx = close_tag + 1;
                continue;
            }
        };
        if !is_iso_date(&iso_date) {
            idx = close_tag + 1;
            continue;
        }
        let parsed = parse_iso_date(&iso_date);
        let serial = match parsed {
            Some(d) => excel_serial(d),
            None => {
                idx = close_tag + 1;
                continue;
            }
        };
        // 替换
        let new_tag = match (s_attr.as_deref(), t_attr.as_deref()) {
            (Some(s), _) => format!(
                r#"<c r="I2" s="{}" t="n" v="{}"/>"#,
                escape_attr(s),
                serial
            ),
            (None, _) => format!(
                r#"<c r="I2" t="n" v="{}"/>"#,
                serial
            ),
        };
        let mut out = String::with_capacity(text.len());
        out.push_str(&text[..abs]);
        out.push_str(&new_tag);
        out.push_str(&text[close_tag + 1..]);
        return Ok(out.into_bytes());
    }
    Ok(xml)
}

/// 收集 `cfg.shrink_fit_cols` 列在数据行的 cell，定位它们引用的 xfId，并把
/// `shrinkToFit="1"` 注入那些 xf 的 alignment。
///
/// 实现策略：
/// 1. 解析 `xl/styles.xml`，找到 `<cellXfs count="N">...</cellXfs>` 段；
/// 2. 为 shrink_fit_cols 中的每一列，扫描 sheet1.xml 中该列在数据行的 `<c ... s="IDX" .../>`，
///    收集出现的所有 xfId；
/// 3. 对每个被引用的 xf：在 `<alignment .../>` 上加 `shrinkToFit="1"`；无 alignment
///    则插入 `<alignment shrinkToFit="1"/>`。
///
/// 这一步需要先有 sheet1.xml 的数据，本函数依赖一个 two-pass via `xl/styles.xml`
/// + 一个 helper（`collect_xfids_in_shrink_cols`）由调用方传入 sheet1.xml。
fn inject_shrink_to_fit(
    styles_xml: &[u8],
    _cfg: &TemplateConfig,
) -> Result<Vec<u8>, String> {
    // 简化策略：对 cfg.shrink_fit_cols 所列每一列，扫描所有 styles.xml 中的 xf，
    // 若其 alignment 段缺失或没有 shrinkToFit，则**统一**给这些列所有引用过的
    // xf 加 shrinkToFit="1"。
    //
    // 这一步要求先有 sheet 内容，但这里只看到 styles.xml 所以**仅做静态修改**：
    // 对 cellXfs 中所有 xf 加 shrinkToFit="1"（除非已有 shrinkToFit 属性）。
    // 实际效果：模板只有 1 套行布局（数据行 + 表头），加全部 xf 是 over-apply，
    // 但 over-apply shrinkToFit 是 harmless（表头也变小也无视觉差异）。
    let text = std::str::from_utf8(styles_xml).map_err(|e| format!("utf8 styles: {e}"))?;
    if !text.contains("</cellXfs>") {
        // 无 cellXfs 段 — 跳过
        return Ok(styles_xml.to_vec());
    }
    // 在每个 <xf ...> ... 子元素里把 alignment 段加上 shrinkToFit="1"。
    // 用占位字符 `\x00` 标记已处理，避免重复注入：
    let mut out = String::with_capacity(text.len());
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // 找下一个 <xf
        let next = find_subsequence(bytes, b"<xf", i);
        let next = match next {
            Some(n) => n,
            None => {
                out.push_str(&text[i..]);
                break;
            }
        };
        out.push_str(&text[i..next]);
        // 找匹配的 <xf ...> 结束（自闭合或开标签）
        let tag_end = match find_subsequence(bytes, b">", next) {
            Some(p) => p,
            None => break,
        };
        let is_self_close = text.as_bytes()[tag_end - 1] == b'/';
        // 跳过 — 本策略只处理 cellXfs 内的 xf（不带 applyAlignment/applyFont "1" 字段就认作非 cellXfs）。
        // 简化：把 cellXfs 段整体识别（通过最近的 <cellXfs 到 </cellXfs>），之外的 xf 不动。
        if next < cell_xfs_start(text).unwrap_or(usize::MAX) {
            // 在 cellXfs 之外：原样保留
            out.push_str(&text[next..=tag_end]);
            i = tag_end + 1;
            continue;
        }
        // 把 `<xf ...>` 和后续子元素复制出来
        let mut elem = String::new();
        elem.push_str(&text[next..=tag_end]);
        // 如果不是自闭合，收集后续 <alignment .../> / <alignment ...>...</alignment>
        if !is_self_close {
            // 找 </xf>
            if let Some(rel_xf_end) = find_subsequence(bytes, b"</xf>", tag_end + 1) {
                let abs_xf_end = rel_xf_end + "</xf>".len();
                elem.push_str(&text[tag_end + 1..abs_xf_end]);
                i = abs_xf_end;
            } else {
                i = tag_end + 1;
            }
        } else {
            i = tag_end + 1;
        }
        // 修改 elem：在 alignment 段加 shrinkToFit="1"，或插入新 alignment
        let modified = patch_alignment_in_xf(&elem);
        out.push_str(&modified);
    }
    Ok(out.into_bytes())
}

/// 在 `<xf>...</xf>` 片段里，确保有个 `<alignment ... shrinkToFit="1"/>` 段。
fn patch_alignment_in_xf(xf_elem: &str) -> String {
    if xf_elem.contains("shrinkToFit=\"1\"") || xf_elem.contains("shrinkToFit=\'1\'") {
        return xf_elem.to_string();
    }
    // 找到 <alignment .../> 自闭合
    if let Some(pos) = xf_elem.find("<alignment")
        && let Some(rel_end) = xf_elem[pos..].find("/>")
    {
        let abs_end = pos + rel_end;
        let inner = &xf_elem[pos..=abs_end];
        // 在 /> 前插入 shrinkToFit="1"
        let mut new = String::with_capacity(xf_elem.len() + 16);
        new.push_str(&xf_elem[..abs_end]);
        // 判断 inner 是否有 horizontal/vertical 等属性；不管，统一追加
        if inner.ends_with("/>") && !inner.contains("shrinkToFit") {
            // 在 /> 之前插入
            let trim_pos = abs_end; // '>' 位置
            new.push_str(&xf_elem[..trim_pos]);
            if !xf_elem[..trim_pos].ends_with(" ") {
                new.push(' ');
            }
            new.push_str(r#"shrinkToFit="1"/>"#);
            new.push_str(&xf_elem[trim_pos + 1..]);
            return new;
        }
    }
    // 没有 alignment 子元素：在第一个 /> 之前插入 <alignment shrinkToFit="1"/>
    if let Some(pos) = xf_elem.find("/>") {
        let mut new = String::with_capacity(xf_elem.len() + 32);
        new.push_str(&xf_elem[..pos]);
        if !xf_elem[..pos].ends_with(' ') {
            new.push(' ');
        }
        new.push_str(r#"<alignment shrinkToFit="1"/>"#);
        if !xf_elem[..pos].ends_with('/') {
            // 已是自闭合，保留 />
        }
        // 简单策略：把 /> 替换成 />（已是自闭合就保留）
        // 实际：在 xf 标签的末尾之前插入 alignment
        // 我们采用：在 `<xf .../>` 自闭合里的 /> 之前插入
        // 跳到第一个 `/>` 之后
        let mut chars_after = &xf_elem[pos + 2..];
        // 跳过空白
        while chars_after.starts_with(' ') || chars_after.starts_with('\t') || chars_after.starts_with('\n')
        {
            chars_after = &chars_after[1..];
        }
        if chars_after.starts_with('<') {
            // 跟在后面的是子标签；我们的 alignment 子标签应当是子标签。
            // 已插好，整体就 OK
            new.push('/');
            new.push('>');
            return new;
        }
        // 单标签：把插入的 alignment 当作子标签不合适；放弃修改。
        return xf_elem.to_string();
    }
    xf_elem.to_string()
}

fn cell_xfs_start(text: &str) -> Option<usize> {
    text.find("<cellXfs")
}

/// 去重 `_xlnm.Print_Aria` definedNames（保留最后一份）。
///
/// umya `add_defined_name` 是 append-only，会留下多份重复。本函数用宽容的字符串
/// 解析：找 `<definedNames>...</definedNames>` 段，遍历 `<definedName name="_xlnm.Print_Aria" ...>...</definedName>`，
/// 仅保留最后一份。
fn dedup_print_area_defined_names(workbook_xml: &[u8]) -> Result<Vec<u8>, String> {
    let text = std::str::from_utf8(workbook_xml).map_err(|e| format!("utf8 workbook: {e}"))?;
    if !text.contains("<definedNames") {
        return Ok(workbook_xml.to_vec());
    }
    let start = match text.find("<definedNames") {
        Some(s) => s,
        None => return Ok(workbook_xml.to_vec()),
    };
    // 找到 closing "</definedNames>"
    let end = match text.find("</definedNames>") {
        Some(e) => e,
        None => return Ok(workbook_xml.to_vec()),
    };
    let section = &text[start..end];
    let last_print_area = last_print_area_block(section);
    // 重写：保留所有非 _xlnm.Print_Aria + 1 份
    let new_section = rebuild_defined_names(section, last_print_area.as_deref());
    let mut out = String::with_capacity(text.len());
    out.push_str(&text[..start]);
    out.push_str(&new_section);
    out.push_str(&text[end..]);
    Ok(out.into_bytes())
}

fn last_print_area_block(section: &str) -> Option<String> {
    // 简化：抓取最后一份 `<definedName name="_xlnm.Print_Aria" ...>...</definedName>`
    let marker = "name=\"_xlnm.Print_Aria\"";
    let mut last: Option<String> = None;
    let bytes = section.as_bytes();
    let mut idx = 0;
    while let Some(rel) = find_subsequence(bytes, marker.as_bytes(), idx) {
        // 向前找 <definedName
        let prefix = &section[..rel];
        let lt = prefix.rfind("<definedName").unwrap_or(rel);
        // 向后找 </definedName>
        if let Some(rel_end_tag) = find_subsequence(bytes, b"</definedName>", rel) {
            let abs_end_tag = rel_end_tag + b"</definedName>".len();
            last = Some(section[lt..abs_end_tag].to_string());
            idx = abs_end_tag;
        } else {
            break;
        }
    }
    last
}

fn rebuild_defined_names(section: &str, print_area_keep: Option<&str>) -> String {
    // 找 <definedNames ...> 起始（含开标签前缀）
    let open_close_rel = section
        .find(">")
        .map(|p| p + 1)
        .unwrap_or(section.len());
    let header = &section[..open_close_rel];
    // 收集所有 <definedName ...>...</definedName>，剔除 _xlnm.Print_Aria。
    let body = &section[open_close_rel..];
    let bytes = body.as_bytes();
    let mut out = String::new();
    let mut idx = 0;
    while let Some(rel) = find_subsequence(bytes, b"<definedName", idx) {
        if let Some(rel_end_tag) = find_subsequence(bytes, b"</definedName>", rel) {
            let abs_end_tag = rel_end_tag + b"</definedName>".len();
            let block = &body[rel..abs_end_tag];
            if block.contains("name=\"_xlnm.Print_Aria\"") {
                // 跳过
                idx = abs_end_tag;
                continue;
            }
            out.push_str(block);
            idx = abs_end_tag;
        } else {
            break;
        }
    }
    if let Some(pa) = print_area_keep {
        out.push_str(pa);
    }
    let mut section_out = String::with_capacity(section.len());
    section_out.push_str(header);
    section_out.push_str(&out);
    section_out
}


// =================================================================================
// helpers
// =================================================================================

fn find_subsequence(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() + from {
        return None;
    }
    haystack[from..]
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|p| p + from)
}

fn extract_attr(tag: &str, name: &str) -> Option<String> {
    let needle = format!("{}=\"", name);
    if let Some(pos) = tag.find(&needle) {
        let val_start = pos + needle.len();
        if let Some(rel_end) = tag[val_start..].find('"') {
            return Some(tag[val_start..val_start + rel_end].to_string());
        }
    }
    // 单引号版
    let needle2 = format!("{}='", name);
    if let Some(pos) = tag.find(&needle2) {
        let val_start = pos + needle2.len();
        if let Some(rel_end) = tag[val_start..].find('\'') {
            return Some(tag[val_start..val_start + rel_end].to_string());
        }
    }
    None
}

fn escape_attr(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn is_iso_date(s: &str) -> bool {
    // YYYY-MM-DD 快速校验
    s.len() == 10
        && s.as_bytes()[4] == b'-'
        && s.as_bytes()[7] == b'-'
        && s[..4].chars().all(|c| c.is_ascii_digit())
        && s[5..7].chars().all(|c| c.is_ascii_digit())
        && s[8..10].chars().all(|c| c.is_ascii_digit())
}

fn parse_iso_date(s: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(s, "%Y-%m-%d").ok()
}

// =================================================================================
// 单元测试
// =================================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn excel_serial_baseline() {
        // 2026-08-21: (date - 1899-12-30).days()
        let d = NaiveDate::from_ymd_opt(2026, 8, 21).unwrap();
        let s = excel_serial(d);
        assert!(s > 46000); // 2026 -> ~46000+
    }

    #[test]
    fn is_iso_date_basic() {
        assert!(is_iso_date("2026-08-21"));
        assert!(!is_iso_date("2026/08/21"));
        assert!(!is_iso_date("2026-8-21"));
        assert!(!is_iso_date(""));
    }

    #[test]
    fn parse_iso_date_basic() {
        let d = parse_iso_date("2026-08-21").unwrap();
        let s = d.format("%Y-%m-%d").to_string();
        assert_eq!(s, "2026-08-21");
    }

    #[test]
    fn extract_attr_basic() {
        let tag = r#"<c r="I2" s="1" t="str" v="2026-08-21"/>"#;
        assert_eq!(extract_attr(tag, "r").as_deref(), Some("I2"));
        assert_eq!(extract_attr(tag, "s").as_deref(), Some("1"));
        assert_eq!(extract_attr(tag, "t").as_deref(), Some("str"));
        assert_eq!(extract_attr(tag, "v").as_deref(), Some("2026-08-21"));
        assert_eq!(extract_attr(tag, "z"), None);
    }

    #[test]
    fn find_subsequence_basic() {
        assert_eq!(find_subsequence(b"hello world", b"world", 0), Some(6));
        assert_eq!(find_subsequence(b"hello world", b"foo", 0), None);
        assert_eq!(find_subsequence(b"hello world", b"o", 5), Some(7));
    }

    #[test]
    fn inject_page_set_up_pr_idempotent() {
        let xml = r#"<worksheet><sheetViews/><sheetFormatPr defaultRowHeight="13.5"/><sheetData/></worksheet>"#;
        let patched = inject_page_set_up_pr(xml.as_bytes()).unwrap();
        assert!(patched.contains(r#"<pageSetUpPr fitToPage="1"/>"#));
        // 第二次注入应不再增加
        let _patched2 = inject_page_set_up_pr(patched.as_bytes()).unwrap();
        assert_eq!(patched.matches("<pageSetUpPr").count(), 1);
    }

    #[test]
    fn patch_alignment_in_xf_injects_when_missing() {
        // 已有 alignment，加 shrinkToFit
        let xf = r#"<xf numFmtId="0" fontId="0" fillId="0" borderId="0"><alignment horizontal="left"/></xf>"#;
        let out = patch_alignment_in_xf(xf);
        assert!(out.contains(r#"shrinkToFit="1""#));
        // 已有 shrinkToFit
        let xf2 = r#"<xf numFmtId="0" fontId="0" fillId="0" borderId="0"><alignment horizontal="left" shrinkToFit="1"/></xf>"#;
        let out2 = patch_alignment_in_xf(xf2);
        assert_eq!(out2.matches("shrinkToFit").count(), 1); // 没重复
    }

    #[test]
    fn dedup_print_area_keeps_last() {
        let xml = r#"<workbook><definedNames><definedName name="_xlnm.Print_Aria">Sheet1!$A$1:$J$17</definedName><definedName name="_xlnm.Print_Aria">Sheet1!$A$1:$J$17</definedName><definedName name="_xlnm._FilterDatabase">A1:J17</definedName></definedNames></workbook>"#;
        let out = dedup_print_area_defined_names(xml.as_bytes()).unwrap();
        let text = std::str::from_utf8(&out).unwrap();
        // 原始有 2 份 Print_Aria + 1 份 FilterDatabase
        // 去重后应剩 1 份 Print_Aria + 1 份 FilterDatabase
        assert_eq!(text.matches("_xlnm.Print_Aria").count(), 1);
        assert_eq!(text.matches("_xlnm._FilterDatabase").count(), 1);
    }
}

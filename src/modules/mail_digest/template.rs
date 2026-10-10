//! mail_digest 域正文渲染（**纯函数、无状态、无 IO**，便于 lib 单测覆盖）
//!
//! 产出 = 一封「按系统交期日期分组的 HTML 表格」邮件：逾期节 + 未来节。
//!
//! ## 邮件客户端的三条硬约束（决定了本文件几乎所有写法）
//!
//! 1. **不写死外部资源**：无外部图片、无外链 CSS、无 `<style>` 块。QQ 邮箱 /
//!    腾讯企业邮 / Gmail 默认拦远程内容，`<style>` 块会被整块剥掉，外部图片会变成
//!    一堆裂图 + 隐私告警。所有样式写成**行内 `style="..."`**。
//! 2. **表格布局，不吃 flex / grid**：主流客户端（Outlook / 旧版 Gmail）对
//!    布局模型支持参差，`display:flex` 会被拍成纵向。全程 `<table>` + `align` /
//!    `valign`。
//! 3. **颜色要有 `bgcolor` 兜底**：Outlook 桌面版忽略 `background-color`，
//!    只认 `bgcolor` 属性 ⇒ 关键底色两处都写。
//!
//! ## 安全：所有插值一律 HTML 转义
//!
//! 图号 / 名称 / 客户名 / 订单号 / 序列号**全是用户输入**，直接拼进正文就是一个
//! 存储型 XSS 面（收件人的邮件客户端会替我们执行注入的脚本）。故正文里
//! **每一个**插值点都走 [`html_escape`]，单测 [`tests::escaping_covers_every_special_char`]
//! 逐字符钉死。
//!
//! 主题行（`Subject:` 头）**不是** HTML，但仍是 SMTP header 面：
//! 主题里的 CR / LF 在 [`crate::modules::mail_digest::service::validate_subject_template`]
//! 就被拒（21704），渲染层不再重复拦。

use chrono::NaiveDate;

use crate::modules::mail_digest::dto::{NormalizedConfig, VisibleColumn};
use crate::modules::mail_digest::repo::DigestRow;

// ============================================================================
// 入参 / 出参
// ============================================================================

/// 渲染入参。
///
/// `overdue_rows` / `upcoming_rows` 由 service 层按 `include_overdue` /
/// `include_upcoming` 决定**是否去查**（关掉的节不查，省一次 SQL）—— 但
/// **渲染层自己也按 `cfg.include_*` 决定渲不渲染**，不靠「切片恰好是空的」来推断。
///
/// ⚠️ 2026-10-13（review 第 1 轮 M5）两处原本是「不区分」的：`render()` 无条件调用
/// 两次 `render_section`，而关掉的节因为没查库而拿到空切片，于是正文里照样出现
/// 「已逾期（系统交期早于今日）（0 条） / 本节无符合条件的工单」。管理员关掉逾期节却
/// 在信里看到一条空节，**第一反应是「统计口径是不是坏了」**而不是「这节我没要」——
/// 这是会直接变成工单的问题。⇒ 节开关现在真的作用于正文。
///
/// ⚠️ **行数与 `cfg.include_*` 同源**（2026-10-13 review 第 2 轮 #12）：「关掉的节
/// 行数恒 0」这条不变式由 `service::fetch_rows` 保证（`if cfg.include_x { 查库 } else
/// { Vec::new() }`），渲染层不重复判定。本文件因此**不**把 `overdue_count` /
/// `upcoming_count` 写成「实际渲染了几行」——对当前两个生产构造点（`service::preview`
/// 与 `service::deliver`）两者恒相等，而流水表（§2.5）沿用「节关掉则该 count 恒 0」
/// 的口径，**不是**「没有逾期工单」；分叉会让同一份数据有两个解释。
#[derive(Debug, Clone)]
pub struct RenderInput<'a> {
    pub cfg: &'a NormalizedConfig,
    pub overdue_rows: &'a [DigestRow],
    pub upcoming_rows: &'a [DigestRow],
    /// 窗口下界（来自 `infra::clock::now_naive().date()`，**不来自 DB**）。
    pub today: NaiveDate,
}

/// 渲染结果。
#[derive(Debug, Clone)]
pub struct RenderedMail {
    pub subject: String,
    pub html: String,
    pub overdue_count: i32,
    pub upcoming_count: i32,
    /// 实际参与渲染的列（**空列白名单会退回缺省 8 列**，见 [`filter_known_columns`]）。
    pub columns: Vec<String>,
}

// ============================================================================
// 主题
// ============================================================================

/// 生成邮件主题。
///
/// 缺省形态：`交期汇总 2026-10-13（未来14天）` —— **与前端任务书逐字对齐**。
/// `cfg.upcoming_days` 恒出现在主题里，即便 `include_upcoming = false`
/// （那封信仍然会带上窗口天数这个上下文，读信人一眼知道统计口径）。
///
/// `subject_template` 支持两个占位符，其余字符原样输出：
/// - `{date}` → `today` 的 `YYYY-MM-DD`
/// - `{days}` → `upcoming_days`
///
/// 未知占位符**原样保留**（不报错）：模板是给人看的文案，允许 `[草稿]` 这类方括号，
/// 只有 `{xxx}` 形态会被替换。
pub fn build_subject(cfg: &NormalizedConfig, today: NaiveDate) -> String {
    match cfg.subject_template.as_deref().map(str::trim) {
        None | Some("") => format!(
            "交期汇总 {}（未来{}天）",
            today.format("%Y-%m-%d"),
            cfg.upcoming_days
        ),
        Some(tpl) => tpl
            .replace("{date}", &today.format("%Y-%m-%d").to_string())
            .replace("{days}", &cfg.upcoming_days.to_string()),
    }
}

// ============================================================================
// 正文
// ============================================================================

/// 渲染一封完整的汇总邮件。
pub fn render(input: &RenderInput) -> RenderedMail {
    let columns = filter_known_columns(&input.cfg.visible_columns);
    let subject = build_subject(input.cfg, input.today);

    let mut html = String::with_capacity(8192);
    html.push_str(HTML_HEAD);

    // ── 概览行：**只列已启用的节**（2026-10-13 review 第 2 轮 m2）──
    // 关掉的节 service 层没查库 ⇒ 行数恒 0，若照旧打「逾期 0 条」，管理员关掉逾期节
    // 后信的第一行仍是「…逾期 0 条 / 未来 12 条」—— 那与 M5 修掉的「空节」是同一种
    // 误读（看起来像统计口径坏了），只是从整节缩到了一个数字。故直接不列。
    let overdue_txt = if input.cfg.include_overdue {
        format!("逾期 {} 条", input.overdue_rows.len())
    } else {
        String::new()
    };
    let upcoming_txt = if input.cfg.include_upcoming {
        format!("未来 {} 条", input.upcoming_rows.len())
    } else {
        String::new()
    };
    let counts = [overdue_txt, upcoming_txt]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" / ");
    let counts = if counts.is_empty() {
        "（本邮件未启用任何内容节）".to_string()
    } else {
        counts
    };
    html.push_str(&format!(
        "<tr><td style=\"padding:12px 16px;font-size:13px;color:#606266;\">\
         统计口径：系统交期；今日 = {}；未来窗口 = {} 天；\
         {counts}</td></tr>",
        html_escape(&input.today.format("%Y-%m-%d").to_string()),
        input.cfg.upcoming_days,
    ));
    html.push_str(HR);

    // ── 逾期节 / 未来节：关掉的节**整段不输出**（2026-10-13 M5）──
    // 节开关直接读 `cfg.include_*`，不新增 `RenderInput` 字段：那里已经有了同一对
    // 开关的来源，再抄一份进 `RenderInput` 只会给「两者不一致」开一个口子。
    if input.cfg.include_overdue {
        html.push_str(&render_section(
            "已逾期（系统交期早于今日）",
            input.overdue_rows,
            &columns,
            input.today,
            true,
        ));
    }

    if input.cfg.include_upcoming {
        html.push_str(&render_section(
            "即将到期（今日起未来窗口内）",
            input.upcoming_rows,
            &columns,
            input.today,
            false,
        ));
    }

    // ── 空态：**一节都没渲染**时才给一句人话（2026-10-13 m2）──
    // 只要渲染了至少一节，空态就由那一节自己的「本节无符合条件的工单」承担；两节都开
    // 且都无行时再补一句全局空态，是同一件事说三遍（原先正是 4 段连续冗余文案）。
    // 「两节都关」在配置层已被 21704 拒掉，这里是渲染层对纯函数调用的优雅兜底。
    if !input.cfg.include_overdue && !input.cfg.include_upcoming {
        html.push_str(
            "<tr><td style=\"padding:24px 16px;text-align:center;color:#909399;\">\
             本邮件未启用任何内容节</td></tr>",
        );
    }

    html.push_str(HTML_TAIL);
    RenderedMail {
        subject,
        html,
        overdue_count: input.overdue_rows.len() as i32,
        upcoming_count: input.upcoming_rows.len() as i32,
        columns: columns.iter().map(|c| c.as_str().to_string()).collect(),
    }
}

/// 兜底「全空白名单 → 退回缺省 8 列」，**不是**「剔除未知列」。
///
/// ⚠️ 2026-10-13（review 第 1 轮 M7）本函数的 doc 原先写的是「剔除未知列…命中即
/// `tracing::warn!`」，那是**抄错了路径**：形参是 `&[VisibleColumn]` —— **已解析过的
/// 枚举**，结构上就装不进未知值。脏值（DB 里 `visible_columns` 里的陌生列名）的真实
/// 处置在 `service::normalize_row → dto::parse_visible_columns`，那里直接返 `21704`
/// 硬错（配置错误要当场暴露，**不静默丢弃**），根本走不到渲染层。
///
/// 所以这里只兜**一种**情形：白名单为空数组（绕过 API 的直连写入 / 将来某个 seed）。
/// 空列白名单会让正文没有任何数据列，一张只有表头的表格毫无意义 ⇒ 退回缺省集合并
/// `warn!`（命中即说明有脏数据，需要有人去查源头）。
fn filter_known_columns(cols: &[VisibleColumn]) -> Vec<VisibleColumn> {
    if cols.is_empty() {
        tracing::warn!(
            "mail_digest: visible_columns 为空，渲染退回缺省 8 列（DB 里可能存了空数组）"
        );
        return VisibleColumn::DEFAULT_SET.to_vec();
    }
    cols.to_vec()
}

/// 渲染一节（逾期节或未来节）。空集合输出「本节无符合条件的工单」。
fn render_section(
    title: &str,
    rows: &[DigestRow],
    columns: &[VisibleColumn],
    today: NaiveDate,
    is_overdue: bool,
) -> String {
    let mut out = String::new();
    let accent = if is_overdue { COLOR_DANGER } else { COLOR_INFO };
    out.push_str(&format!(
        "<tr><td style=\"padding:14px 16px 6px 16px;\">\
         <span style=\"font-size:15px;font-weight:bold;color:{accent};\">{}</span>\
         <span style=\"font-size:13px;color:#909399;\">（{} 条）</span></td></tr>",
        html_escape(title),
        rows.len()
    ));

    if rows.is_empty() {
        out.push_str(
            "<tr><td style=\"padding:6px 16px 14px 16px;font-size:13px;color:#909399;\">\
             本节无符合条件的工单</td></tr>",
        );
        return out;
    }

    // 表头
    out.push_str("<tr>");
    for col in columns {
        out.push_str(&format!(
            "<th style=\"{TH_STYLE}\">{}</th>",
            html_escape(col.label())
        ));
    }
    out.push_str("</tr>");

    // 按 system_delivery_date 分组，组内画一条分隔标题行。
    // SQL 已按 (交期 ASC, id ASC) 排好，故线性扫描即可，**不排序**（避免渲染层
    // 与 SQL 各排一次而口径分叉）。
    let mut current: Option<NaiveDate> = None;
    for row in rows {
        let Some(date) = row.system_delivery_date else {
            // 理论上被 SQL 的窗口谓词排除了（`IS NOT NULL`），但真出现时不能 panic：
            // 归到「无交期」组里照常渲染，只是不显示剩余天数。
            if current.is_some() {
                out.push_str("</table></td></tr><tr><td colspan=\"");
                out.push_str(&columns.len().to_string());
                out.push_str("\" style=\"padding-left:0;\">");
                out.push_str(&group_header(
                    "无交期",
                    is_overdue,
                    today,
                    None,
                    columns.len(),
                ));
                out.push_str("<table width=\"100%\" cellspacing=\"0\" cellpadding=\"0\" style=\"border-collapse:collapse;\">");
                current = None;
            }
            out.push_str(&data_row(row, columns, today));
            continue;
        };
        if current != Some(date) {
            if current.is_some() {
                out.push_str("</table></td></tr><tr><td colspan=\"");
                out.push_str(&columns.len().to_string());
                out.push_str("\" style=\"padding-left:0;\">");
            }
            out.push_str(&group_header(
                &date.format("%Y-%m-%d").to_string(),
                is_overdue,
                today,
                Some(date),
                columns.len(),
            ));
            out.push_str("<table width=\"100%\" cellspacing=\"0\" cellpadding=\"0\" style=\"border-collapse:collapse;\">");
            current = Some(date);
        }
        out.push_str(&data_row(row, columns, today));
    }
    out.push_str("</table></td></tr>");
    out
}

/// 组标题行：日期 + 逾期天数 / 剩余天数标注。
///
/// ⚠️ `colspan` **必须**是真实列数（`2026-10-13` review 第 1 轮 m1）：原先这里写的是
/// 字面量 `"*"`，而 HTML 的 `colspan="*"` 不是合法取值、客户端一律**回落到 1** ——
/// 日期组标题行于是只盖住第一列，不再横跨整张表。形参 `colspan` 由
/// [`render_section`] 传 `columns.len()`（同一函数里另外两处 `<td colspan="…">` 本来
/// 就是这么写的）。
fn group_header(
    label: &str,
    is_overdue: bool,
    today: NaiveDate,
    date: Option<NaiveDate>,
    colspan: usize,
) -> String {
    let Some(date) = date else {
        return format!(
            "<tr><td colspan=\"{colspan}\" style=\"{GROUP_STYLE}\">{}</td></tr>",
            html_escape(label)
        );
    };
    let delta = (date - today).num_days();
    let (note, color) = if is_overdue {
        (format!("已逾期 {} 天", -delta), COLOR_DANGER)
    } else if delta == 0 {
        ("今天到期".to_string(), COLOR_DANGER)
    } else if delta == 1 {
        ("明天到期".to_string(), COLOR_WARNING)
    } else {
        (format!("剩余 {} 天", delta), COLOR_INFO)
    };
    format!(
        "<tr><td colspan=\"{colspan}\" style=\"{GROUP_STYLE}\">{} <span style=\"color:{color};\">（{note}）</span></td></tr>",
        html_escape(label)
    )
}

/// 渲染一行数据（按列白名单逐列取值；所有取值转义）。
fn data_row(row: &DigestRow, columns: &[VisibleColumn], today: NaiveDate) -> String {
    // 逾期行的交期字样用红色标出；装配件行的 `quantity` 单位是「套」。
    let urgent_bg = if row.is_urgent {
        " bgcolor=\"#fdf6ec\""
    } else {
        ""
    };
    let mut out = format!("<tr{urgent_bg}>");
    for col in columns {
        let text = cell_text(row, *col, today);
        out.push_str(&format!(
            "<td style=\"{TD_STYLE}\">{}</td>",
            html_escape(&text)
        ));
    }
    out.push_str("</tr>");
    out
}

/// 单个单元格的**纯文本**内容（HTML 转义在 [`data_row`] 里统一做，这里只出文本）。
fn cell_text(row: &DigestRow, col: VisibleColumn, today: NaiveDate) -> String {
    match col {
        VisibleColumn::SerialNo => row.serial_no.clone().unwrap_or_default(),
        VisibleColumn::DrawingNo => row.drawing_no.clone(),
        VisibleColumn::Name => row.name.clone(),
        VisibleColumn::Quantity => {
            // ⚠️ 行单位：装配件的 quantity 是**套数**，散件是件数（沿用 dashboard 的
            // 交期面板行单位约定）。单位直接跟在数字后，避免读者按件数理解装配件。
            if row.is_assembly() {
                format!("{} 套", row.quantity)
            } else {
                row.quantity.to_string()
            }
        }
        VisibleColumn::CustomerName => row.customer_name.clone().unwrap_or_default(),
        VisibleColumn::Status => row.status.clone(),
        VisibleColumn::SystemDeliveryDate => row
            .system_delivery_date
            .map(|d| d.format("%Y-%m-%d").to_string())
            .unwrap_or_default(),
        VisibleColumn::RemainingDays => match row.system_delivery_date {
            // 今天到期显示 0；逾期显示负数，语义是「已经晚了几天」。
            Some(d) => (d - today).num_days().to_string(),
            None => String::new(),
        },
        VisibleColumn::RowType => {
            if row.is_assembly() {
                "装配件".to_string()
            } else {
                "零件".to_string()
            }
        }
        VisibleColumn::IsUrgent => {
            if row.is_urgent {
                "是".to_string()
            } else {
                "否".to_string()
            }
        }
        VisibleColumn::OrderNo => row.order_no.clone().unwrap_or_default(),
    }
}

/// HTML 转义：`&` `<` `>` `"` `'` 五个字符。
///
/// ⚠️ **`&` 必须第一个替换**，否则 `&lt;` 里的 `&` 会被后续步骤再转义成
/// `&amp;lt;`（双重转义，正文里就会出现字面量 `&lt;`）。
/// 顺序错了是个不会报错的 bug —— 单测 [`tests::escaping_covers_every_special_char`]
/// 专门钉它。
pub fn html_escape(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(ch),
        }
    }
    out
}

// ============================================================================
// 样式常量（全部内联，见模块 doc 的「邮件客户端三条硬约束」）
// ============================================================================

const COLOR_DANGER: &str = "#f56c6c";
const COLOR_WARNING: &str = "#e6a23c";
const COLOR_INFO: &str = "#409eff";

/// 表头单元格样式（`bg` 两处都写：Outlook 只认 `bgcolor` 属性）。
const TH_STYLE: &str = "border:1px solid #ebeef5;padding:6px 8px;font-size:13px;\
                        background-color:#fafafa;text-align:left;white-space:nowrap;";

/// 数据单元格样式。
const TD_STYLE: &str = "border:1px solid #ebeef5;padding:6px 8px;font-size:13px;color:#303133;";

/// 日期组标题样式。
const GROUP_STYLE: &str = "padding:8px 8px 4px 8px;font-size:13px;font-weight:bold;color:#303133;\
                           background-color:#f4f4f5;";

/// 节间分隔线。
const HR: &str = "<tr><td style=\"padding:0 16px;\"><hr style=\"border:none;border-top:1px solid #ebeef5;\"></td></tr>";

/// `charset=UTF-8` 放在**第一个 meta**：`Content-Type` 头是 `text/html`（不带
/// charset），正文里的中文完全靠这个 meta 决定编码 —— 缺了它 Outlook / 部分
/// 企业邮会按 GBK 解码，正文全变乱码。
const HTML_HEAD: &str = "<!DOCTYPE html><html><head><meta charset=\"UTF-8\"></head>\
                         <body style=\"margin:0;padding:0;\">\
                         <table width=\"100%\" cellspacing=\"0\" cellpadding=\"0\" \
                         style=\"border-collapse:collapse;font-family:'Microsoft YaHei',SimSun,sans-serif;\">";

const HTML_TAIL: &str = "</table></body></html>";

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn d(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    fn row(drawing_no: &str, date: Option<&str>, asm: bool) -> DigestRow {
        DigestRow {
            id: 1,
            serial_no: Some("SN-001".into()),
            drawing_no: drawing_no.into(),
            name: "法兰盘".into(),
            quantity: 12,
            status: "IN_PROCESS".into(),
            system_delivery_date: date.map(d),
            is_urgent: false,
            order_no: Some("PO-9".into()),
            customer_name: Some("某某机械".into()),
            row_type: if asm { "ASSEMBLY" } else { "PART" }.into(),
        }
    }

    fn input<'a>(
        cfg: &'a NormalizedConfig,
        o: &'a [DigestRow],
        u: &'a [DigestRow],
    ) -> RenderInput<'a> {
        RenderInput {
            cfg,
            overdue_rows: o,
            upcoming_rows: u,
            today: d("2026-10-13"),
        }
    }

    /// HTML 转义逐字符钉死（含「`&` 必须先转」这个顺序陷阱）。
    #[test]
    fn escaping_covers_every_special_char() {
        assert_eq!(html_escape("&"), "&amp;");
        assert_eq!(html_escape("<"), "&lt;");
        assert_eq!(html_escape(">"), "&gt;");
        assert_eq!(html_escape("\""), "&quot;");
        assert_eq!(html_escape("'"), "&#39;");
        // 顺序陷阱：`&` 先转，否则 `&lt;` 会被二次转义成 `&amp;lt;`
        assert_eq!(html_escape("<a>"), "&lt;a&gt;");
        assert_eq!(html_escape("&lt;"), "&amp;lt;");
        assert_eq!(
            html_escape(r#"<script>alert("x")</script>"#),
            "&lt;script&gt;alert(&quot;x&quot;)&lt;/script&gt;"
        );
        // 普通中文 / 数字 / 空格原样透传
        assert_eq!(html_escape("法兰盘 A-1"), "法兰盘 A-1");
    }

    /// 缺省主题逐字对齐前端契约：`交期汇总 2026-10-13（未来14天）`。
    #[test]
    fn default_subject_matches_frozen_contract() {
        let cfg = NormalizedConfig::default();
        assert_eq!(
            build_subject(&cfg, d("2026-10-13")),
            "交期汇总 2026-10-13（未来14天）"
        );
    }

    /// 自定义主题模板替换 `{date}` / `{days}`；未知 `{xxx}` 原样保留。
    #[test]
    fn subject_template_replaces_known_placeholders_only() {
        let cfg = NormalizedConfig {
            subject_template: Some("【{date}】交期预警 {days} 天内（草稿 {ver}）".into()),
            ..NormalizedConfig::default()
        };
        assert_eq!(
            build_subject(&cfg, d("2026-10-13")),
            "【2026-10-13】交期预警 14 天内（草稿 {ver}）"
        );
    }

    /// 空体（两节都开着但都无行）→ **只**由各节自己的「本节无符合条件的工单」承担
    /// 空态，不再补一句全局的「无符合条件的工单」（2026-10-13 m2：原先是 5 段同义文案）。
    #[test]
    fn empty_input_renders_empty_state_not_blank_table() {
        let cfg = NormalizedConfig::default();
        let out = render(&input(&cfg, &[], &[]));
        assert_eq!(out.overdue_count, 0);
        assert_eq!(out.upcoming_count, 0);
        assert_eq!(
            out.html.matches("本节无符合条件的工单").count(),
            2,
            "两节各一句，**没有**多余的全局空态：{}",
            out.html
        );
        assert!(!out.html.contains("<th"), "无数据时不应渲染表头");
    }

    /// ⚠️ 回归（2026-10-13 review 第 1 轮 M5）：**节开关真的不渲染该节**。
    ///
    /// 原先 `render()` 无条件调用两次 `render_section`，关掉的节因没查库而拿到空切片，
    /// 于是正文里照样出现「已逾期（…）（0 条）/ 本节无符合条件的工单」——管理员关掉
    /// 逾期节却在信里看到一条空节，第一反应是「统计口径是不是坏了」。
    #[test]
    fn closed_section_is_not_rendered_at_all() {
        let rows = vec![row("DWG-U", Some("2026-10-15"), false)];
        let cfg = NormalizedConfig {
            include_overdue: false,
            ..NormalizedConfig::default()
        };
        let out = render(&input(&cfg, &[], &rows));
        assert!(
            !out.html.contains("已逾期"),
            "关掉的逾期节不应出现任何字样：{}",
            out.html
        );
        assert!(out.html.contains("即将到期"), "开着的未来节必须渲染");
        assert!(out.html.contains("DWG-U"));
        assert_eq!(out.overdue_count, 0);
        assert_eq!(out.upcoming_count, 1);
    }

    /// 反向：关掉未来节时，未来节也不渲染（另一半开关）。
    #[test]
    fn closed_upcoming_section_is_not_rendered_at_all() {
        let rows = vec![row("DWG-O", Some("2026-10-09"), false)];
        let cfg = NormalizedConfig {
            include_upcoming: false,
            ..NormalizedConfig::default()
        };
        let out = render(&input(&cfg, &rows, &[]));
        assert!(!out.html.contains("即将到期"), "{}", out.html);
        assert!(out.html.contains("已逾期"));
        assert!(out.html.contains("DWG-O"));
    }

    /// ⚠️ 回归（2026-10-13 review 第 2 轮 #2）：**概览行只列已启用的节**。
    ///
    /// M5 把「关掉的节整段不渲染」修掉了，但概览行当时仍无条件打
    /// 「逾期 {} 条 / 未来 {} 条」⇒ 管理员关掉逾期节后，信的第一行还是
    /// 「…逾期 0 条 / 未来 12 条」。那 0 **不是**「没有逾期工单」，而是「这节没启用」，
    /// 与 M5 修掉的空节是同一种误读（看起来像统计口径坏了），只是从整节缩到了一个数字。
    #[test]
    fn overview_line_lists_only_enabled_sections() {
        let one_overdue = vec![row("DWG-O", Some("2026-10-09"), false)];
        let one_upcoming = vec![row("DWG-U", Some("2026-10-15"), false)];
        let overview = |html: &str| {
            html.split("统计口径：")
                .nth(1)
                .unwrap_or("")
                .split("</td>")
                .next()
                .unwrap_or("")
                .trim()
                .to_string()
        };
        let both_on = NormalizedConfig::default();

        // ① 两节开、都无行
        let out = render(&input(&both_on, &[], &[]));
        assert_eq!(
            overview(&out.html),
            "系统交期；今日 = 2026-10-13；未来窗口 = 14 天；逾期 0 条 / 未来 0 条"
        );

        // ② 只开逾期节（未来节关掉）
        let only_overdue = NormalizedConfig {
            include_upcoming: false,
            ..NormalizedConfig::default()
        };
        let out = render(&input(&only_overdue, &one_overdue, &[]));
        let line = overview(&out.html);
        assert!(line.ends_with("逾期 1 条"), "{line}");
        // 「未来窗口 = N 天」是**统计口径**（恒出现），不是节；被禁的是「未来 N 条」
        assert!(
            !line.contains("未来 0 条"),
            "关掉的未来节不该在概览行报「未来 0 条」：{line}"
        );

        // ③ 只开未来节（逾期节关掉）—— 本条就是 #2 的核心场景
        let only_upcoming = NormalizedConfig {
            include_overdue: false,
            ..NormalizedConfig::default()
        };
        let out = render(&input(&only_upcoming, &[], &one_upcoming));
        let line = overview(&out.html);
        assert!(line.ends_with("未来 1 条"), "{line}");
        assert!(
            !line.contains("逾期 0 条"),
            "关掉的逾期节不该在概览行报「逾期 0 条」：{line}"
        );

        // ④ 两节全关（配置层已被 21704 拒；渲染层兜底）
        let none_on = NormalizedConfig {
            include_overdue: false,
            include_upcoming: false,
            ..NormalizedConfig::default()
        };
        let out = render(&input(&none_on, &[], &[]));
        let line = overview(&out.html);
        assert!(line.ends_with("（本邮件未启用任何内容节）"), "{line}");
        assert!(!line.contains("逾期"), "{line}");
        assert!(!line.contains("未来 0 条"), "{line}");
    }

    /// 组标题的 `colspan` 必须是真实列数（原先写字面量 `"*"`，客户端回落到 1）。
    #[test]
    fn group_header_colspan_equals_column_count() {
        let cfg = NormalizedConfig {
            visible_columns: vec![VisibleColumn::DrawingNo, VisibleColumn::Name],
            ..NormalizedConfig::default()
        };
        let rows = vec![row("DWG-1", Some("2026-10-15"), false)];
        let out = render(&input(&cfg, &[], &rows));
        assert!(
            out.html.contains("colspan=\"2\""),
            "2 列 ⇒ 组标题 colspan 应为 2，且不得出现非法的 \"*\"：{}",
            out.html
        );
        assert!(!out.html.contains("colspan=\"*\""));
    }

    /// 逾期节的交期标红并带「已逾期 N 天」标注。
    #[test]
    fn overdue_group_is_marked_with_days_late() {
        let cfg = NormalizedConfig::default();
        let rows = vec![row("DWG-1", Some("2026-10-09"), false)]; // 今天 -4 天
        let out = render(&input(&cfg, &rows, &[]));
        assert!(out.html.contains("已逾期 4 天"), "缺逾期标注：{}", out.html);
        assert_eq!(out.overdue_count, 1);
        // 表头齐全
        assert!(out.html.contains("序列号"));
        assert!(out.html.contains("系统交期"));
    }

    /// 未来节的「今日 / 明日」分别标注，其余按剩余天数。
    #[test]
    fn upcoming_groups_mark_today_and_tomorrow() {
        let cfg = NormalizedConfig::default();
        let rows = vec![
            row("DWG-T", Some("2026-10-13"), false), // 今天
            row("DWG-M", Some("2026-10-14"), false), // 明天
            row("DWG-L", Some("2026-10-20"), false), // +7
        ];
        let out = render(&input(&cfg, &[], &rows));
        assert!(out.html.contains("今天到期"));
        assert!(out.html.contains("明天到期"));
        assert!(out.html.contains("剩余 7 天"));
        assert_eq!(out.upcoming_count, 3);
    }

    /// 按 `system_delivery_date` 分组：每个日期**只出一个组标题**。
    ///
    /// ⚠️ 断言钉的是**组标题里的天数标注**（`剩余 N 天`），不是日期字面量本身 ——
    /// 日期字面量在「系统交期」列里每行都会再出现一次，按它计数会把「2 行同日期」
    /// 误判成「2 个组」。标注只在组标题里出现，是分组数量的唯一无歧义信号。
    #[test]
    fn rows_are_grouped_by_delivery_date() {
        let cfg = NormalizedConfig::default();
        let rows = vec![
            row("DWG-1", Some("2026-10-15"), false), // +2
            row("DWG-2", Some("2026-10-15"), false), // +2（同组）
            row("DWG-3", Some("2026-10-16"), false), // +3
        ];
        let out = render(&input(&cfg, &[], &rows));
        assert_eq!(
            out.html.matches("剩余 2 天").count(),
            1,
            "同日期（10-15）只出一个组标题：{}",
            out.html
        );
        assert_eq!(out.html.matches("剩余 3 天").count(), 1, "10-16 自成一组");
        // 三行都在（即使同组的第二行也必须渲染出来）
        for dwg in ["DWG-1", "DWG-2", "DWG-3"] {
            assert!(out.html.contains(dwg), "缺行 {dwg}");
        }
        assert_eq!(out.upcoming_count, 3);
    }

    /// 装配件的 `quantity` 单位是「套」，散件无单位。
    #[test]
    fn assembly_quantity_carries_set_unit() {
        let cfg = NormalizedConfig {
            visible_columns: vec![VisibleColumn::Quantity, VisibleColumn::RowType],
            ..NormalizedConfig::default()
        };
        let rows = vec![
            row("ASM-1", Some("2026-10-15"), true),
            row("PART-1", Some("2026-10-15"), false),
        ];
        let out = render(&input(&cfg, &[], &rows));
        assert!(out.html.contains("12 套"), "装配件缺「套」单位");
        assert!(out.html.contains("装配件"));
        assert!(out.html.contains("零件"));
    }

    /// 列白名单驱动：只渲染被选中的列。
    #[test]
    fn only_whitelisted_columns_are_rendered() {
        let cfg = NormalizedConfig {
            visible_columns: vec![VisibleColumn::DrawingNo, VisibleColumn::Name],
            ..NormalizedConfig::default()
        };
        let rows = vec![row("DWG-9", Some("2026-10-15"), false)];
        let out = render(&input(&cfg, &[], &rows));
        assert_eq!(
            out.columns,
            vec!["drawing_no".to_string(), "name".to_string()]
        );
        assert!(out.html.contains("图号"));
        assert!(!out.html.contains("序列号"), "未选中的列不应出现表头");
        assert!(!out.html.contains("SN-001"), "未选中的列不应出现数据");
    }

    /// 空列白名单退回缺省 8 列（否则会渲出一张只有表头的空表）。
    #[test]
    fn empty_column_set_falls_back_to_default() {
        let cfg = NormalizedConfig {
            visible_columns: Vec::new(),
            ..NormalizedConfig::default()
        };
        let rows = vec![row("DWG-1", Some("2026-10-15"), false)];
        let out = render(&input(&cfg, &[], &rows));
        assert_eq!(out.columns.len(), VisibleColumn::DEFAULT_SET.len());
        assert!(out.html.contains("序列号"));
    }

    /// XSS 回归：所有用户输入字段都被转义，裸 `<script>` 不得出现在正文里。
    #[test]
    fn user_input_is_escaped_in_body() {
        let cfg = NormalizedConfig::default();
        let rows = vec![DigestRow {
            drawing_no: "<script>alert(1)</script>".into(),
            name: "a\"b'c&d".into(),
            customer_name: Some("<img src=x onerror=alert(2)>".into()),
            order_no: Some("<b>x</b>".into()),
            serial_no: Some("<i>sn</i>".into()),
            ..row("", Some("2026-10-15"), false)
        }];
        let out = render(&input(&cfg, &[], &rows));
        assert!(!out.html.contains("<script>"), "裸 <script> 泄进了正文");
        assert!(!out.html.contains("<img src=x"), "裸 <img> 泄进了正文");
        assert!(!out.html.contains("<i>sn</i>"), "裸 <i> 泄进了正文");
        assert!(out.html.contains("&lt;script&gt;"));
        assert!(out.html.contains("&quot;"), "双引号未转义");
        assert!(out.html.contains("&#39;"), "单引号未转义");
        assert!(out.html.contains("&amp;d"), "& 未转义或被二次转义");
    }

    /// 邮件自足性：无外部图片 / 外链 / `<style>` 块（客户端默认拦远程内容）。
    #[test]
    fn html_has_no_external_resources() {
        let cfg = NormalizedConfig::default();
        let rows = vec![row("DWG-1", Some("2026-10-15"), false)];
        let out = render(&input(&cfg, &[], &rows));
        assert!(
            !out.html.contains("<style"),
            "出现了 <style> 块（客户端会剥掉）"
        );
        assert!(!out.html.contains("<img"), "出现了外部/内嵌图片");
        assert!(
            !out.html.contains("http://") && !out.html.contains("https://"),
            "出现外链"
        );
        // charset 必须是第一个 meta，否则 Outlook / 企业邮按 GBK 解码会乱码
        assert!(
            out.html
                .starts_with("<!DOCTYPE html><html><head><meta charset=\"UTF-8\">")
        );
    }
}

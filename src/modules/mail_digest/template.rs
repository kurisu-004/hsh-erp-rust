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
//! ## 打印友好（2026-10-16 需求「表格列要对齐，支持打印」）
//!
//! - **列对齐**：**每个日期组一张自带表头的子表**，`<colgroup>` / 组标题 / 表头
//!   `<th>` / 数据行全在**同一张表**里；每个 `<col>` 写**显式宽度百分比**，表格加
//!   `table-layout:fixed`。⚠️ `<colgroup>` 只对它**所在那张表**的列宽生效 —— 表头若留在
//!   外层表里，表头单元格由另一套布局算法定宽，「横向扫行对得上」在表头这一行就不成立。
//!   没有 `fixed` + 显式宽度时 13 列全由浏览器按内容自适应 ⇒「名称」长一点就把后面几列
//!   挤歪。
//! - **不写 `@media print`**：约束 1 已经说明 `<style>` 块会被整块剥掉，
//!   写进去等于什么都没写 ⇒ 全部内联。
//! - **底色在打印路径里会丢**：`background-color` 遇到部分打印路径 /
//!   Outlook 桌面版会被丢弃，故所有有底色的单元格额外内联
//!   `print-color-adjust:exact;-webkit-print-color-adjust:exact`（见各样式常量）。
//! - **不用 `float` / `position`**：两者在邮件与打印路径里都会产生不可预期的漂移。
//!
//! ## 安全：所有插值一律 HTML 转义
//!
//! 图号 / 名称 / 客户名 / 订单号 / 序列号 / 批次号 / 工人姓名 / 外协公司名**全是
//! 用户输入**，直接拼进正文就是一个存储型 XSS 面（收件人的邮件客户端会替我们执行
//! 注入的脚本）。故正文里**每一个**插值点都走 [`html_escape`]，单测
//! [`tests::escaping_covers_every_special_char`] 逐字符钉死。
//!
//! 主题行（`Subject:` 头）**不是** HTML，但仍是 SMTP header 面：
//! 主题里的 CR / LF 在 [`crate::modules::mail_digest::service::validate_subject_template`]
//! 就被拒（21704），渲染层不再重复拦。

use chrono::NaiveDate;

use crate::modules::mail_digest::dto::{
    NormalizedConfig, VisibleColumn, batch_location_label, batch_status_label, part_status_label,
};
use crate::modules::mail_digest::repo::{DigestBatchRow, DigestRow};

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
    /// 实际参与渲染的列（**空列白名单会退回缺省 10 列**，见 [`filter_known_columns`]）。
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

/// 兜底「全空白名单 → 退回缺省 10 列」，**不是**「剔除未知列」。
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
            "mail_digest: visible_columns 为空，渲染退回缺省 10 列（DB 里可能存了空数组）"
        );
        return VisibleColumn::DEFAULT_SET.to_vec();
    }
    cols.to_vec()
}

/// 一张子表的 `<table …>` 开标签（含打印与对齐相关的三个属性）。
///
/// ⚠️ `table-layout:fixed` 是「列要对齐」的前提：没有它时宽度由内容决定，
/// 某一行的名称长一点，后面几列就整体错位。
const fn table_open() -> &'static str {
    "<table width=\"100%\" cellspacing=\"0\" cellpadding=\"0\" \
     style=\"border-collapse:collapse;table-layout:fixed;\">"
}

/// `<colgroup>`：按**本次实际渲染的列**逐列给显式宽度百分比。
///
/// ⚠️ **每张子表各写一份**（分组渲染会开多张子表）：`<colgroup>` 是表内元素、不跨表
/// 共享，只在第一张表写的话，后面的组退回自适应布局（列还是歪的）。
///
/// ⚠️ 宽度是**归一化后的整数百分比**，不是 [`VisibleColumn::width_weight`] 的原值 ——
/// 白名单是任意子集，直接用原值会凑不出 100%（例如只勾 4 列时合计 41%）⇒ 表格整体
/// 宽度塌掉、fixed 布局按内容回退成自适应，等于白做。故：先求当次列集合的权重和，
/// 逐列取 `权重 * 100 / 总权重`（向下取整），**最后一列吸收余数**，保证合计恒 100。
///
/// ⚠️ 同时写 `width="N%"` 属性与 `style="width:N%"`：前者给老客户端，后者给现代
/// 客户端（与本文件「颜色两处都写」同一取舍）。
fn colgroup(columns: &[VisibleColumn]) -> String {
    if columns.is_empty() {
        return String::new();
    }
    let total: u32 = columns.iter().map(|c| c.width_weight()).sum();
    let last = columns.len() - 1;
    let mut out = String::from("<colgroup>");
    let mut used = 0u32;
    for (i, col) in columns.iter().enumerate() {
        let pct = if i == last {
            100 - used
        } else {
            let p = col.width_weight() * 100 / total;
            used += p;
            p
        };
        out.push_str(&format!("<col width=\"{pct}%\" style=\"width:{pct}%;\">"));
    }
    out.push_str("</colgroup>");
    out
}

/// 子表的收尾：内层 `<table>` + 外层那格的 `</td></tr>`（见 [`open_group`]）。
const SUB_TABLE_TAIL: &str = "</table></td></tr>";

/// 渲染一节（逾期节或未来节）。空集合输出「本节无符合条件的工单」。
///
/// ## 骨架：**外层表 → 每个日期组一行 → 那一格里的子表**
///
/// ```text
/// 外层 table
///   <tr><td>节标题（X 条）</td></tr>
///   <tr><td colspan="N">        ← 每组一行，N = 本次渲染列数
///     <table … table-layout:fixed>
///       <colgroup>…</colgroup>  ← 列宽只对本表生效
///       <tr><td colspan="N">2026-10-15（剩余 2 天）</td></tr>
///       <tr><th>…</th>…</tr>   ← 表头与 <colgroup> 同表 ⇒ 列宽对齐
///       <tr>工单行</tr> <tr>批次子行…</tr> …
///     </table>
///   </td></tr>
///   …下一个日期组…
/// ```
///
/// ⚠️ **表头必须与 `<colgroup>` 在同一张表里**：`<colgroup>` 是表内元素、只对本表的列宽
/// 生效，表头留在外层表时由另一套布局算法定宽 ⇒ 横向扫行在表头这一行对不上（需求⑥要的
/// 「列对齐」就没了）。所以表头**每组一张**（组多时重复出现，但每组都与本组的列宽同源）。
/// 回归：`tests::header_row_shares_table_with_colgroup`。
///
/// ⚠️ 外层包裹**成对**：一张内层表必须整体落在外层表的一个 `<td colspan="N">` 里
/// （[`open_group`] 开 / [`SUB_TABLE_TAIL`] 关）。内层表若是外层表的直接子节点，
/// 浏览器会把外层表提前闭合，后续行的归属由解析器的隐式纠正决定 —— 邮件里表现为
/// 某一组整体串到节标题外面。
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

    // 按 system_delivery_date 分组，组内画一条分隔标题行。
    // SQL 已按 (交期 ASC, id ASC) 排好，故线性扫描即可，**不排序**（避免渲染层
    // 与 SQL 各排一次而口径分叉）。
    //
    // `Option<Option<NaiveDate>>` 的外层是「**已经开过组**」的标志位：交期为空的行归到
    // 「无交期」组，若外层也用 `Option<NaiveDate>`，开头的无交期行会被当成「与当前组同组」
    // 而渲染到组外。
    let mut opened = false;
    let mut current: Option<Option<NaiveDate>> = None;
    for row in rows {
        let key = row.system_delivery_date;
        if current != Some(key) {
            if opened {
                out.push_str(SUB_TABLE_TAIL);
            }
            open_group(&mut out, columns, key, is_overdue, today);
            current = Some(key);
            opened = true;
        }
        push_row_with_batches(&mut out, row, columns, today);
    }
    if opened {
        out.push_str(SUB_TABLE_TAIL);
    }
    out
}

/// 开一个日期组：外层一行一格 + 内层一张自带 `<colgroup>` / 组标题 / 表头的子表。
///
/// 收尾由 [`SUB_TABLE_TAIL`] 负责（`</table></td></tr>`，恰好与这里开的三个标签配平）。
/// 交期为空（`None`）时组标题取「无交期」、不显示剩余天数。
fn open_group(
    out: &mut String,
    columns: &[VisibleColumn],
    date: Option<NaiveDate>,
    is_overdue: bool,
    today: NaiveDate,
) {
    out.push_str(&format!(
        "<tr><td colspan=\"{}\" style=\"padding-left:0;\">",
        columns.len()
    ));
    out.push_str(table_open());
    out.push_str(&colgroup(columns));
    let label = date
        .map(|d| d.format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| NO_DATE_LABEL.to_string());
    out.push_str(&group_header(
        &label,
        is_overdue,
        today,
        date,
        columns.len(),
    ));
    out.push_str(&header_row(columns));
}

/// 表头行（一个 `<tr>` + 每列一个 `<th>`）。
///
/// ⚠️ 它必须紧跟在同表的 `<colgroup>` 之后渲染 —— 见 [`render_section`] 的骨架图与
/// 「表头必须与 `<colgroup>` 同表」那条。
fn header_row(columns: &[VisibleColumn]) -> String {
    let mut out = String::from("<tr>");
    for col in columns {
        out.push_str(&format!(
            "<th style=\"{TH_STYLE}\">{}</th>",
            html_escape(col.label())
        ));
    }
    out.push_str("</tr>");
    out
}

/// 一个工单行 + **紧跟其下**的该工单的批次子行（2026-10-16 需求 2）。
///
/// ⚠️ 「紧跟」是硬要求：批次按 id 集合一次拉回、在 Rust 侧按 `(row_type, id)` 分组
/// 装进 [`DigestRow::batches`]，若渲染时按工单集中渲染批次，邮件里就会出现
/// 「先列完所有工单、再列所有批次」，读信人无从对应。
/// 无批次的工单**不渲子行**（子行是「这个工单名下有批次」的证据，空子行是噪声）。
fn push_row_with_batches(
    out: &mut String,
    row: &DigestRow,
    columns: &[VisibleColumn],
    today: NaiveDate,
) {
    out.push_str(&data_row(row, columns, today));
    for batch in &row.batches {
        out.push_str(&batch_row(batch, columns));
    }
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

/// 渲染一个工单行（按列白名单逐列取值；所有取值转义）。
fn data_row(row: &DigestRow, columns: &[VisibleColumn], today: NaiveDate) -> String {
    // 逾期行的交期字样用红色标出；装配件行的 `quantity` 单位是「套」。
    let urgent_bg = if row.is_urgent {
        format!(" bgcolor=\"{COLOR_URGENT_BG}\"")
    } else {
        String::new()
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

/// 渲染一个批次**子行**（2026-10-16 需求 2/3）。
///
/// ## 只填 5 列，其余留空
///
/// `序列号` / `批次` / `数量` / `状态` / `位置` 五列有值，其余（图号 / 名称 / 客户 /
/// 交期 / 剩余天数 / 工单类型 / 是否紧急 / 订单号）是**工单级**属性，填在子行上会
/// 让读信人以为那一批件的交期与工单不同。留空而不是 `—`：子行本来就该是空的，
/// 空单元格读起来是「这一格属于上一行的信息」。
///
/// ⚠️ **序列号列在子行上是「这一批属于哪个子件」**（装配件一个装配件行下面挂多个子件
/// 的批次，只看批次号分不清是哪个子件的）；散件工单的批次恒为空（那不是缺数据，见
/// [`DigestBatchRow::part_serial_no`]）。空则留空、**不回退到图号**。
///
/// ⚠️ **勾掉任意一列都会让它整个消失**（列白名单是逐列的）：例如管理员勾掉
/// `location` 时子行的位置信息就没有出口了 —— 这是列白名单的固有行为，不是 bug。
///
/// ## 视觉区分不只靠颜色
///
/// 淡底 + `bgcolor` 属性（Outlook 只认属性）+ **批次格的全角空格缩进**。批次列只写
/// **纯数字**（`1` / `2` / `3`，不加「第…批」前缀）—— 缩进与淡底一起承担「这是一行
/// 子行」的信号：打印时底色可能整片丢掉，而全角空格在任何字体下都保留，子行与工单行
/// 仍然分得开。
///
/// ⚠️ 带缩进的是**批次格**、不是子行的**首格**：`DEFAULT_SET` 的列序是
/// `serial_no, drawing_no, name, batch_no, …`，批次格是**第 4 格**；而子行首格在勾了
/// 序列号列时装配件子件序列号（散件恒空），离「这是一行子行」的视觉信号最远 ⇒
/// **认子行靠的是批次格的缩进 + 淡底，不是首格**。
fn batch_row(batch: &DigestBatchRow, columns: &[VisibleColumn]) -> String {
    let mut out = format!("<tr bgcolor=\"{COLOR_BATCH_BG}\">");
    for col in columns {
        let text = match col {
            // 2026-10-16：空则空串、不回退到图号（图号是另一列的内容，在本列重复
            // 它会让「序列号」这一列的语义漂成「零件标识」）。
            VisibleColumn::SerialNo => batch.part_serial_no.clone().unwrap_or_default(),
            // 全角空格 = 缩进（等宽一个汉字宽，黑白打印也保留）
            VisibleColumn::BatchNo => format!("　{}", batch.batch_no),
            VisibleColumn::Quantity => batch.quantity.to_string(),
            VisibleColumn::Status => {
                batch_status_label(&batch.status, batch.is_repairing).to_string()
            }
            VisibleColumn::Location => {
                batch_location_label(batch.location.as_deref(), batch.holder_name.as_deref())
            }
            // ⚠️ 工单行在 `批次` / `位置` 这两列上也是空的（与子行的「空」语义一致）：
            // 这两个是批次级列，不出现在工单行上，不给人「工单也有一个位置」的错觉。
            _ => String::new(),
        };
        out.push_str(&format!(
            "<td style=\"{TD_STYLE_BATCH}\">{}</td>",
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
        // 工单行在批次号列上留空（批次号是批次级信息，见 `batch_row` 的 doc）
        VisibleColumn::BatchNo => String::new(),
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
        // 2026-10-16：状态列输出**中文**（订单管理语义），不再是 `IN_PROCESS` 这种
        // 英文原字面量。未知值原样回显（见 `dto::part_status_label`）。
        VisibleColumn::Status => part_status_label(&row.status).to_string(),
        // 工单行的位置列留空（批次级信息，见 `batch_row` 的 doc）
        VisibleColumn::Location => String::new(),
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

/// 加急工单行的淡橙底（`<tr bgcolor>` 属性）。
const COLOR_URGENT_BG: &str = "#fdf6ec";

/// 批次子行的淡灰底（`<tr bgcolor>` 属性）。
const COLOR_BATCH_BG: &str = "#f7f8fa";

/// `system_delivery_date` 为空的行归入的组标题（SQL 的窗口谓词已排除该形态，这里是
/// 渲染层的兜底：真出现时照常渲染，只是不显示剩余天数）。
const NO_DATE_LABEL: &str = "无交期";

/// 表头单元格样式（`bg` 两处都写：Outlook 只认 `bgcolor` 属性）。
///
/// 末尾的 `print-color-adjust:exact;-webkit-print-color-adjust:exact;` 是**打印保底色**：
/// Chrome / Edge 默认会省掉打印时的背景色，除非显式要求保留；两个前缀都写是因为旧版
/// WebKit（多数企业邮内嵌浏览器）只认 `-webkit-` 那一个。它在这三个有底色的样式常量
/// 里逐字重复 —— 刻意不抽成具名常量：`style="..."` 要编译期字面量、`concat!` 不接受
/// 具名常量入参，抽象出去只能在旁边再 `format!` 一次，反而多一处会漏改的地方。
const TH_STYLE: &str = "border:1px solid #ebeef5;padding:6px 8px;font-size:13px;\
                        background-color:#fafafa;text-align:left;white-space:nowrap;print-color-adjust:exact;-webkit-print-color-adjust:exact;";

/// 数据单元格样式（无底色 ⇒ 不需要打印保底色属性）。
const TD_STYLE: &str = "border:1px solid #ebeef5;padding:6px 8px;font-size:13px;color:#303133;";

/// 批次子行单元格样式（淡灰底 + 打印保底色 + 上下略收窄，让子行更贴工单行）。
const TD_STYLE_BATCH: &str = "border:1px solid #ebeef5;padding:4px 8px;font-size:13px;\
                              color:#606266;background-color:#f7f8fa;print-color-adjust:exact;-webkit-print-color-adjust:exact;";

/// 日期组标题样式。
const GROUP_STYLE: &str = "padding:8px 8px 4px 8px;font-size:13px;font-weight:bold;color:#303133;\
                           background-color:#f4f4f5;print-color-adjust:exact;-webkit-print-color-adjust:exact;";

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
            batches: Vec::new(),
        }
    }

    /// 一个批次子行（`location` / `holder_name` 缺省 = `OFFICE` 的解析不出形态；
    /// `part_serial_no` 缺省 `None` = 散件工单的批次，那一串恒空）。
    fn batch(no: i32, status: &str) -> DigestBatchRow {
        DigestBatchRow {
            id: 900 + no as i64,
            batch_no: no,
            part_serial_no: None,
            quantity: 5,
            status: status.into(),
            is_repairing: false,
            location: None,
            holder_name: None,
            owner_type: "PART".into(),
            owner_id: 1,
        }
    }

    /// 装配件的批次子行：带子件序列号 + `owner_type = 'ASSEMBLY'`。
    fn child_batch(no: i32, serial_no: Option<&str>) -> DigestBatchRow {
        DigestBatchRow {
            owner_type: "ASSEMBLY".into(),
            part_serial_no: serial_no.map(str::to_string),
            ..batch(no, "IN_PROCESS")
        }
    }

    fn with_batches(mut r: DigestRow, bs: Vec<DigestBatchRow>) -> DigestRow {
        r.batches = bs;
        r
    }

    /// 批次子行的**批次格**是否恰好等于 `　{n}`（整格等值）。
    ///
    /// ⚠️ **不要写 `html.contains("\u{3000}1")`**（2026-10-16）：去掉「第…批」前缀之后
    /// `　1` 是 `　10` / `　11` 的**子串** ⇒ 一个有 10 批的工单能让「批次列出现/消失」
    /// 那类断言假绿（批次 10 一条就足以让 `contains("\u{3000}1")` 与
    /// `!contains("\u{3000}1")` 同时给出错误答案）。`>\u{3000}1<` 带上两侧界 ⇒
    /// `>　10<` 里不含 `>　1<`，恢复旧 marker `第1批` 不具备的前缀安全性。
    fn has_batch_cell(html: &str, no: i32) -> bool {
        html.contains(&format!(">\u{3000}{no}<"))
    }

    /// 子行上**不含**批次号 `no` 的整格（`has_batch_cell` 的否定形态，供「列被勾掉后不应
    /// 出现批次号」这类断言用 —— 同样带界，避免 `　10` 顶掉 `　1` 的断言）。
    fn lacks_batch_cell(html: &str, no: i32) -> bool {
        !has_batch_cell(html, no)
    }

    /// 取出某个 `<tr …>…</tr>` 块里各 `<td>` 的**纯文本**。
    ///
    /// 用于「子行只填 5 列、其余留空」这类结构性断言。单元格形如
    /// `<td style="…">TEXT</td>`，而样式串里没有 `>`（只有 `:` 与 `;`）⇒ 第一个
    /// `>` 之后到 `</td>` 之间就是文本。
    ///
    /// ⚠️ 形参 `marker` 是**子串**匹配（`html.find`），传批次号时**必须**用
    /// [`has_batch_cell`] 那种带界的形态先确认该格真的存在，否则 `　1` 会先命中
    /// `　10`、返回的是另一行的格子。
    fn row_cells(html: &str, marker: &str) -> Vec<String> {
        let start = html
            .find(marker)
            .unwrap_or_else(|| panic!("正文里找不到标记 {marker}：\n{html}"));
        let tr_start = html[..start].rfind("<tr").expect("标记之前必有 <tr");
        let tr_end = html[start..].find("</tr>").expect("标记所在行必有 </tr>") + start;
        html[tr_start..tr_end]
            .split("<td")
            .skip(1)
            .map(|cell| {
                let gt = cell.find('>').expect("单元格必有属性结束符") + 1;
                cell[gt..]
                    .split("</td>")
                    .next()
                    .unwrap_or_default()
                    .to_string()
            })
            .collect()
    }

    /// 解析 `<col …>` 的宽度百分比序列（`<col width="N%" …>`）。
    fn col_widths(html: &str) -> Vec<u32> {
        html.split("<col ")
            .skip(1)
            .filter_map(|c| c.split("width=\"").nth(1))
            .filter_map(|c| c.split('%').next())
            .filter_map(|c| c.parse::<u32>().ok())
            .collect()
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

    /// 空列白名单退回缺省 10 列（否则会渲出一张只有表头的空表）。
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

    // =======================================================================
    //  2026-10-16：批次子行 / 状态中文 / 列对齐 / 打印友好
    // =======================================================================

    /// 批次子行**紧跟**工单行（需求 2），且多个工单各自跟自己的那批（不串行）。
    #[test]
    fn batch_sub_rows_follow_their_own_order_row() {
        let cfg = NormalizedConfig::default();
        let rows = vec![
            with_batches(
                row("DWG-A", Some("2026-10-15"), false),
                vec![batch(1, "IN_PROCESS"), batch(2, "INSPECTION")],
            ),
            with_batches(
                row("DWG-B", Some("2026-10-15"), false),
                vec![batch(1, "PENDING")],
            ),
        ];
        let out = render(&input(&cfg, &[], &rows));
        let pos = |needle: &str| {
            out.html
                .find(needle)
                .unwrap_or_else(|| panic!("正文里找不到 {needle}：\n{}", out.html))
        };
        // A 工单行 → A 的两个批次 → B 工单行 → B 的批次
        // （批次格只写纯数字 + 全角缩进；标记带 `>` / `<` 两侧界，`　1` 才不会被
        //  `　10` 顶掉 —— 见 `has_batch_cell` 的 doc）
        let (a, b1, b2, b) = (
            pos("DWG-A"),
            pos(">\u{3000}1<"),
            pos(">\u{3000}2<"),
            pos("DWG-B"),
        );
        assert!(
            a < b1 && b1 < b2 && b2 < b,
            "批次子行必须紧跟各自工单行：\n{}",
            out.html
        );
        assert_eq!(
            out.upcoming_count, 2,
            "子行**不**计入节行数（行单位仍是工单）"
        );
    }

    /// 子行只填 5 列（序列号 / 批次 / 数量 / 状态 / 位置），其余留空。
    #[test]
    fn batch_sub_row_fills_only_five_columns() {
        let cfg = NormalizedConfig::default(); // 缺省 10 列
        let mut b = child_batch(3, Some("ASM-1-03"));
        b.status = "OUTSOURCE".into();
        b.location = Some("OUTSOURCE_COMPANY".into());
        b.holder_name = Some("宏远外协".into());
        let rows = vec![with_batches(
            row("DWG-A", Some("2026-10-15"), false),
            vec![b],
        )];
        let out = render(&input(&cfg, &[], &rows));
        let cells = row_cells(&out.html, ">\u{3000}3<");
        assert_eq!(
            cells.len(),
            cfg.visible_columns.len(),
            "子行必须与工单行等宽"
        );
        // 非空单元格恰为 5 个（顺序 = 缺省列顺序里的序列号 / 批次 / 数量 / 状态 / 位置）
        let filled: Vec<&String> = cells.iter().filter(|c| !c.is_empty()).collect();
        assert_eq!(filled.len(), 5, "只该填 5 列，实际：{filled:?}");
        assert!(
            cells.iter().any(|c| c == "ASM-1-03"),
            "序列号列填子件序列号：{filled:?}"
        );
        assert!(cells.iter().any(|c| c == "\u{3000}3"));
        assert!(
            cells.iter().any(|c| c == "5"),
            "数量不带「套」后缀：{filled:?}"
        );
        assert!(cells.iter().any(|c| c == "外协中"));
        assert!(cells.iter().any(|c| c == "外协 宏远外协"));
    }

    /// ★ 装配件子行的序列号格填**子件序列号**（2026-10-16）—— 一个装配件行下面挂着
    /// 多个子件的批次，只看批次号分不清「这一批是哪个子件的」。
    #[test]
    fn assembly_child_batch_renders_its_serial_no() {
        let cfg = NormalizedConfig {
            visible_columns: vec![VisibleColumn::SerialNo, VisibleColumn::BatchNo],
            ..NormalizedConfig::default()
        };
        let rows = vec![with_batches(
            row("ASM-1", Some("2026-10-15"), true),
            vec![
                child_batch(1, Some("ASM-1-01")),
                child_batch(2, Some("ASM-1-02")),
            ],
        )];
        let out = render(&input(&cfg, &[], &rows));
        let cells = row_cells(&out.html, ">\u{3000}2<");
        assert_eq!(cells, vec!["ASM-1-02", "\u{3000}2"], "{cells:?}");
    }

    /// ⚠️ 子件序列号缺失（装配件**追加**的子件 `serial_no` 恒 NULL）⇒ 序列号格
    /// **留空、不回退到图号**。
    ///
    /// 两条独立事实钉在一起：① 「空则留空」是用户明确定的取舍；② **不回退图号**——
    /// 图号是「图号」列的内容，在序列号列重复它会让这一列的语义漂成「零件标识」。
    #[test]
    fn assembly_child_without_serial_no_leaves_cell_blank() {
        let cfg = NormalizedConfig {
            visible_columns: vec![
                VisibleColumn::SerialNo,
                VisibleColumn::DrawingNo,
                VisibleColumn::BatchNo,
            ],
            ..NormalizedConfig::default()
        };
        let rows = vec![with_batches(
            row("CHILD-9", Some("2026-10-15"), true),
            vec![child_batch(1, None)],
        )];
        let out = render(&input(&cfg, &[], &rows));
        // 工单行的图号 / 序列号有值，子行两格都空（`>\u{3000}1<` 定位子行）
        let cells = row_cells(&out.html, ">\u{3000}1<");
        // 列序 = [序列号, 图号, 批次]：子行前两格都空（`CHILD-9` 只出现在工单行）
        assert_eq!(
            cells,
            vec!["", "", "\u{3000}1"],
            "序列号格必须留空、不得回退到图号：{cells:?}"
        );
    }

    /// 一个批次都没有的工单**不渲子行**（子行是「有批次」的证据，空子行是噪声）。
    #[test]
    fn order_without_batches_renders_no_sub_row() {
        let cfg = NormalizedConfig::default();
        let rows = vec![row("DWG-A", Some("2026-10-15"), false)];
        let out = render(&input(&cfg, &[], &rows));
        assert!(lacks_batch_cell(&out.html, 1), "无子行时不得出现批次格");
        assert!(lacks_batch_cell(&out.html, 0));
        // 行数与子行无关：仍是 1 行
        assert_eq!(out.upcoming_count, 1);
    }

    /// 状态列输出中文（订单管理语义），未知值**原样回显**不静默变空串。
    #[test]
    fn status_column_is_chinese_and_unknown_falls_back_to_literal() {
        let pairs = [
            ("PENDING", "待生产"),
            ("PROGRAMMING", "编程中"),
            ("IN_PROCESS", "生产中"),
            ("INSPECTION", "待品检"),
            ("READY_TO_SHIP", "待送货"),
            ("DELIVERED", "已送货"),
            ("REPAIRING", "返修中"),
            ("OUTSOURCE", "外协中"),
            ("COMPLETED", "已完成"),
            ("CANCELLED", "已取消"),
        ];
        for (raw, want) in pairs {
            assert_eq!(part_status_label(raw), want, "{raw} 的中文标签不符");
        }
        assert_eq!(
            part_status_label("SOMETHING_NEW"),
            "SOMETHING_NEW",
            "未知状态必须原样回显（fail-loud），不能变空串"
        );

        // 工单行：状态列出中文，不再是英文原字面量
        let cfg = NormalizedConfig {
            visible_columns: vec![VisibleColumn::Status],
            ..NormalizedConfig::default()
        };
        let mut r = row("DWG-A", Some("2026-10-15"), false);
        r.status = "IN_PROCESS".into();
        let out = render(&input(&cfg, &[], &[r]));
        assert!(out.html.contains("生产中"), "{}", out.html);
        assert!(!out.html.contains("IN_PROCESS"), "英文原字面量仍出现了");
    }

    /// `is_repairing = true` ⇒ 子行显示「返修中」，**且**优先级高于 `status`。
    #[test]
    fn repairing_batch_renders_repairing_label() {
        let cfg = NormalizedConfig {
            visible_columns: vec![VisibleColumn::Status],
            ..NormalizedConfig::default()
        };
        let mut b = batch(1, "IN_PROCESS");
        b.is_repairing = true;
        let rows = vec![with_batches(
            row("DWG-A", Some("2026-10-15"), false),
            vec![b],
        )];
        let out = render(&input(&cfg, &[], &rows));
        assert!(out.html.contains("返修中"), "{}", out.html);
        // ⚠️ 工单行没有 is_repairing（`t_part` 上没这列）⇒ 工单行仍是「生产中」
        assert!(out.html.contains("生产中"), "{}", out.html);
    }

    /// 位置文案逐档对齐前端 `partsListColumnDefs.ts`；解析不出时 `—`。
    #[test]
    fn batch_location_labels_match_frontend_format() {
        let cases = [
            (Some("PRODUCTION_SHELF"), Some("A-01"), "货架 A-01"),
            (Some("INSPECTION_SHELF"), Some("B-02"), "品检 B-02"),
            (Some("WORKER"), Some("张三"), "张三"),
            (Some("OUTSOURCE_COMPANY"), Some("宏远外协"), "外协 宏远外协"),
            (Some("OFFICE"), None, "—"),
            (None, None, "—"),
            // 未知 location（DB 无枚举约束，将来可能多一个取值）
            (Some("SOME_NEW_PLACE"), Some("X"), "—"),
        ];
        for (loc, holder, want) in cases {
            assert_eq!(
                batch_location_label(loc, holder),
                want,
                "location={loc:?} holder={holder:?} 的位置文案不符"
            );
        }
    }

    /// 终态批次的位置列渲成 `—`（迁移清掉了终态的 location / holder）。
    #[test]
    fn terminal_batch_renders_dash_for_location() {
        let cfg = NormalizedConfig {
            visible_columns: vec![
                VisibleColumn::BatchNo,
                VisibleColumn::Status,
                VisibleColumn::Location,
            ],
            ..NormalizedConfig::default()
        };
        let rows = vec![with_batches(
            row("DWG-A", Some("2026-10-15"), false),
            vec![batch(1, "COMPLETED")],
        )];
        let out = render(&input(&cfg, &[], &rows));
        let cells = row_cells(&out.html, ">\u{3000}1<");
        assert!(cells.iter().any(|c| c == "—"), "{cells:?}");
        assert!(cells.iter().any(|c| c == "已完成"), "{cells:?}");
    }

    /// `<colgroup>` 的宽度**合计恒 100%**（不是权重原值之和），且列数 = 渲染列数。
    #[test]
    fn colgroup_widths_sum_to_one_hundred() {
        for cols in [
            VisibleColumn::ALL.to_vec(),
            VisibleColumn::DEFAULT_SET.to_vec(),
            vec![VisibleColumn::DrawingNo, VisibleColumn::Name],
            vec![VisibleColumn::Status],
            vec![
                VisibleColumn::Location,
                VisibleColumn::BatchNo,
                VisibleColumn::RemainingDays,
                VisibleColumn::OrderNo,
            ],
        ] {
            let cfg = NormalizedConfig {
                visible_columns: cols.clone(),
                ..NormalizedConfig::default()
            };
            let rows = vec![with_batches(
                row("DWG-A", Some("2026-10-15"), false),
                vec![batch(1, "IN_PROCESS")],
            )];
            let out = render(&input(&cfg, &[], &rows));
            let widths = col_widths(&out.html);
            assert_eq!(
                widths.len(),
                cols.len(),
                "<col> 数必须等于本次渲染的列数（cols={:?}）",
                cols.len()
            );
            assert_eq!(
                widths.iter().sum::<u32>(),
                100,
                "列宽合计必须恒 100%，实际 {widths:?}"
            );
            assert!(
                out.html.contains("table-layout:fixed"),
                "缺 table-layout:fixed"
            );
            assert!(out.html.contains("<colgroup>"), "缺 <colgroup>");
        }
    }

    /// ⚠️ **表头 `<th>` 与 `<colgroup>` 必须落在同一张 `<table …>` 里**（需求⑥「列对齐」
    /// 的真断言）。
    ///
    /// `<colgroup>` 是表内元素、只对它**所在那张表**的列宽生效。表头若留在外层表里，
    /// 表头单元格由另一套布局算法定宽 ⇒ 数据列齐了、表头不齐，横向扫行照样对不上。
    /// 只断言「有 N 个 `<col>` 且百分比合计 100」的那条**对表头一无所知**，表头错位时
    /// 它照样全绿。
    #[test]
    fn header_row_shares_table_with_colgroup() {
        let cfg = NormalizedConfig::default();
        let rows = vec![
            with_batches(
                row("DWG-1", Some("2026-10-15"), false),
                vec![batch(1, "IN_PROCESS")],
            ),
            row("DWG-2", Some("2026-10-15"), false), // 同组
            row("DWG-3", Some("2026-10-16"), false), // 第二组 ⇒ 第二张子表
        ];
        let html = &render(&input(&cfg, &[], &rows)).html;
        let colgroups: Vec<usize> = html.match_indices("<colgroup>").map(|(i, _)| i).collect();
        // 表头**行**（`<tr>` 紧跟 `<th>`），不是一个 `<th>`
        let ths: Vec<usize> = html.match_indices("<tr><th ").map(|(i, _)| i).collect();
        assert_eq!(
            colgroups.len(),
            2,
            "两个日期组 ⇒ 两张子表各一份 <colgroup>：\n{html}"
        );
        assert_eq!(
            ths.len(),
            colgroups.len(),
            "**每张子表都要有自己的一行表头**（表头不在子表内就等于没有表头）：\n{html}"
        );
        // 「同一个 `<table …>` 开标签」= 两个标记之前最近的 `<table` 是同一处下标
        let table_open_before = |pos: usize| {
            html[..pos]
                .rfind("<table")
                .unwrap_or_else(|| panic!("{pos} 之前没有 <table 开标签：\n{html}"))
        };
        for (cg, th) in colgroups.iter().zip(ths.iter()) {
            assert_eq!(
                table_open_before(*cg),
                table_open_before(*th),
                "表头 <th> 与 <colgroup> 不在同一张表 ⇒ 列对齐在表头行不成立：\n{html}"
            );
        }
        // 组标题也在同一张子表内：组标题行紧随本表 `<colgroup>` 之后
        assert!(
            html.contains("</colgroup><tr><td colspan=\"10\" style=\""),
            "组标题应紧随本表 <colgroup>：\n{html}"
        );
    }

    /// 打印友好：底色保底属性在位、且**没有** `<style>` 块（客户端会整块剥掉）。
    #[test]
    fn print_attributes_present_and_no_style_block() {
        let cfg = NormalizedConfig::default();
        let mut urgent = row("DWG-A", Some("2026-10-15"), false);
        urgent.is_urgent = true;
        let rows = vec![with_batches(urgent, vec![batch(1, "IN_PROCESS")])];
        let out = render(&input(&cfg, &[], &rows));
        assert!(
            !out.html.contains("<style"),
            "打印属性必须全内联；出现 <style> 块客户端会剥掉"
        );
        assert!(
            out.html.contains("print-color-adjust:exact"),
            "缺打印保底色属性"
        );
        assert!(out.html.contains("-webkit-print-color-adjust:exact"));
        // 加急行 / 批次子行的底色：属性 + style 两处都写
        assert!(out.html.contains("bgcolor=\"#fdf6ec\""), "加急行缺 bgcolor");
        assert!(
            out.html.contains("bgcolor=\"#f7f8fa\""),
            "批次子行缺 bgcolor"
        );
        // 子行不只靠颜色区分：批次格的全角缩进是第二重信号（批次格只写纯数字）
        assert!(
            has_batch_cell(&out.html, 1),
            "批次格应是「全角缩进 + 纯数字」"
        );
        assert!(
            !out.html.contains("第1批"),
            "批次格只写纯数字，不带「第…批」前缀"
        );
    }

    /// 13 列（`VisibleColumn::ALL` 全开）时组标题 `colspan` 仍取**真实列数**。
    #[test]
    fn colspan_matches_column_count_with_all_columns() {
        assert_eq!(VisibleColumn::ALL.len(), 13, "白名单应含 13 列");
        let cfg = NormalizedConfig {
            visible_columns: VisibleColumn::ALL.to_vec(),
            ..NormalizedConfig::default()
        };
        let rows = vec![with_batches(
            row("DWG-A", Some("2026-10-15"), false),
            vec![batch(1, "IN_PROCESS")],
        )];
        let out = render(&input(&cfg, &[], &rows));
        assert!(out.html.contains("colspan=\"13\""), "{}", out.html);
        assert!(!out.html.contains("colspan=\"*\""));
        // 子行与工单行同为 13 列（fixed 布局下列宽必须齐）
        assert_eq!(row_cells(&out.html, ">\u{3000}1<").len(), 13);
    }

    /// 勾掉「批次」列后子行的批次号消失（列白名单逐列生效），其余列不受影响。
    #[test]
    fn dropping_batch_column_removes_batch_number_only() {
        let cfg = NormalizedConfig {
            visible_columns: vec![
                VisibleColumn::Name,
                VisibleColumn::Status,
                VisibleColumn::Location,
            ],
            ..NormalizedConfig::default()
        };
        let mut b = batch(1, "IN_PROCESS");
        b.location = Some("WORKER".into());
        b.holder_name = Some("李四".into());
        let rows = vec![with_batches(
            row("DWG-A", Some("2026-10-15"), false),
            vec![b],
        )];
        let out = render(&input(&cfg, &[], &rows));
        assert!(
            lacks_batch_cell(&out.html, 1),
            "批次列被勾掉后不应出现批次号"
        );
        assert!(out.html.contains("李四"), "位置列仍在：{}", out.html);
        assert_eq!(row_cells(&out.html, "李四").len(), 3);
    }
}

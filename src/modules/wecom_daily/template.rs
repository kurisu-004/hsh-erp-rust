//! wecom_daily 域模板层：**块** → **分片** → 企业微信 `markdown` v1 文本
//! （纯函数、零 IO、易单测）
//!
//! ## ★ 为什么是 `markdown` **v1** 而不是 `markdown_v2`
//!
//! 官方群机器人 webhook 两种 markdown：
//!
//! | | `markdown`（v1） | `markdown_v2` |
//! |---|---|---|
//! | 表格 / 列表 | ❌ | ✅ |
//! | **字体颜色**（`<font color="info">`） | ✅ | ❌ |
//! | 客户端版本门槛 | 无 | **4.1.36+**（安卓 4.1.38+），**低版本退化为纯文本** |
//!
//! 本域选 v1，两条理由：
//! 1. **颜色是日报的核心信号**：逾期数橙红（要催的那批）与统计区间的灰（这是哪一段
//!    时间的数）是这份日报最该被一眼看到的东西。低版本退化成纯文本 ⇒ 整份日报在老
//!    客户端上变成一坨没格式的文本，**风险不可接受**。
//! 2. v1 牺牲的表格能力，本域**本来也不需要**：日报是「几个数字 + 一张条形图」的
//!    摘要，不是明细表（明细在邮件域与统计页）。
//!
//! ## 分片：块级、**不截断**
//!
//! 渲染器产出的是**块**（[`Block`]）而不是字符串：顺序固定
//! `Title → Overview → Delivery → Orders… → WorkersHead → WorkerRow×k → Footer`。
//! ⚠️ **`WorkerRow` 不可再拆**（它是一整行）⇒ **所有分片都发生在块边界上，永远不会把
//! 一行劈成两半** —— 把一行劈开会得到两条半个名字的工人，看着像数据坏了。
//!
//! ### ⚠️ 第三节的清单块**逐行可拆**（与工人行刻意相反）
//!
//! 清单行的形状是「一个装配件 + 它的 N 个子件」：**一行装配件带 30 个子件就是 31 个
//! 渲染行**。若沿用工人行那种「一组一个块、不可拆」，这 31 行会变成一个 ≈4.2 KB 的
//! 单块 —— 比单条消息预算（`4096 − RESERVE`）还大，于是**一个装配件就能让整份日报
//! 渲染失败**（`21804`），而现场看到的现象只是「今天装配件特别多」。
//!
//! 所以清单块的粒度是**一行**：装配件行、子件行各是一个块，`assemble` 可以在任意两行
//! 之间断开。
//!
//! **粘在一起与可拆的边界、以及拆分后果**（逐条钉住，改渲染前先读）：
//!
//! | 不可拆（同一个块内） | 可拆（块边界上断开） | 后果 |
//! |---|---|---|
//! | 第三节小标题 + **第一组**的组标题 + 该组首行 | 该组其余行 | 每片至多以「组标题 + 首行」开头，不会出现只有标题没有内容的片 |
//! | 后续每组：组标题 + 该组首行 | 该组其余行 | 组内某片可能以若干**子件行**开头，而它们的装配件行在上一片 |
//! | 组末脚注「另有 N 条未列出」 | — | 脚注可能落在下一片 |
//!
//! 「子件行与父行被拆到两片」是**可接受**的：子件行自带序列号 / 名称 / 数量 / 位置，
//! 单看一行就完整（这正是它逐字重复父行全部字段的原因），而标题块每片都重复、给出
//! 日期上下文。反过来「把一行劈成两半」不可接受 —— 那会得到半个链接 URL。
//!
//! **两趟贪心装填**：
//!
//! ```text
//! pass 1：逐块装入当前片，超则开新片。每片预算 = 4096 − RESERVE，RESERVE = 48。
//! pass 2：已知 N，为每片的 Title 块标题追加「（i/N）」（N == 1 时不追加）。
//! ```
//!
//! ⚠️ **RESERVE 必须存在**：「（i/N）」要等装填完才知道 N（自指），不预留则
//! 「第 1 片刚好 4090 字节、pass 2 追加后缀后 4095」会炸。
//! ⚠️ `String::len()` 就是 UTF-8 **字节数**（企业微信按字节算上限）。
//! ⚠️ **标题块在每一片都重复** —— 否则第 2 片在群里是几行孤零零的工人数据、
//! 没有日期上下文。
//!
//! ## 三个防御性分支（都不是常态路径，但**必须有**，一律 `21804`、**绝不静默截断**）
//!
//! | 情形 | 处置 |
//! |---|---|
//! | 单个块自身 > 4096 − RESERVE | `21804`，错误消息**指明是哪一节** |
//! | 装填结果片数 > `max_chunks` | `21804`，文案给出两个可行动方向 |
//! | `max_chunks > 20` | **保存配置时**即拒 `21804`（企业微信官方限频 20 条/分钟） |

use std::collections::HashMap;

use chrono::{NaiveDate, NaiveDateTime};

use crate::modules::wecom_daily::dto::{
    NormalizedConfig, SECTION_DELIVERY, SECTION_ORDERS, SECTION_OVERVIEW, SECTION_WORKERS,
    WorkerBarRow,
};
use crate::shared::error::{AppError, code};

/// 企业微信单条 `markdown` 消息的字节上限（官方文档）。
pub const CHUNK_BYTE_LIMIT: usize = 4096;

/// pass 1 的每片预算预留（给 pass 2 的「（i/N）」后缀）。
///
/// ⚠️ 后缀最长形态 `（20/20）` = 11 字节（全角括号各 3 字节），48 是三倍余量。
/// 取整值是为了让「预算」在代码里一眼可算，不依赖某个后缀的字面量。
pub const RESERVE: usize = 48;

/// 字符条形图的格数（每行的条**恒占满**这么宽）。
pub const BAR_CELLS: usize = 12;

/// 实心格 / 空心格（`█` U+2588 / `░` U+2591）。
const BAR_FULL: char = '█';
const BAR_EMPTY: char = '░';

// ============================================================================
// 入参 / 出参
// ============================================================================

/// 第三节清单的**一行**（渲染入参形态；数据由 `service::fetch_rows` 装配好）。
///
/// ⚠️ 它是**渲染入参**而不是表投影：序列号 / 名称 / 数量来自 `t_part`（或
/// `t_assembly`），但 `link` 与 `location` 是本域自己算出来的（图纸预览直链、
/// 「生产架:A-03」这种中文位置），表里没有这两列。
#[derive(Debug, Clone)]
pub struct OrderRow {
    /// `t_part.id` 或 `t_assembly.id`（**渲染层不读它**）。
    ///
    /// ⚠️ 它存在的理由是**省钱**：图纸链路（`include_drawings`）要向 `files` 域提交 CI
    /// job，而 CI **按页计费** ⇒ 只能为「**本次真的会被列出来的**」那些行去提交。若按
    /// 未截断的匹配集提交，`order_list_limit` 挡掉的那些行（现场可能成百上千）会白花
    /// 一次 CI 的钱，而它们一个字符都不会出现在消息里。
    /// service 层据此在**截断之后**收集 id 集合（见 `service::collect_order_groups`）。
    pub owner_id: i64,
    /// 是否**装配件的子件行**（渲染时前缀成 `>   └ `）。
    ///
    /// ⚠️ 层级靠 `└` 这个字符表达，**不靠行首空格**：`markdown` v1 会把行首连续空格
    /// 当缩进吃掉（本域已有的证据见 `worker_line_blocks` 的口径 3 —— 正因为如此数值
    /// 才不做右对齐补空格）。把子件行整体套进 `<font color="comment">` 更糟：名称
    /// 本身是 `[名称](直链)` 链接，而**链接嵌在 font 标签里能否被 v1 解析没有保证**，
    /// 赌输了这一节就退化成「一堆不可点的灰字」。所以层级只由 `└` 承担 —— 它在任何
    /// 客户端、任何版本下都是字面量。
    pub is_child: bool,
    /// `t_part.serial_no` / `t_assembly.serial_no`，**可空**。
    ///
    /// ⚠️ `None` 时**留空**：不回退图号、不显示「无」。留空时该字段连同它后面的分隔
    /// 空格一起去掉（见 `order_line`）—— 留一个悬空空格在真机上会变成不稳定的缩进，
    /// 而它不承载任何信息。
    pub serial_no: Option<String>,
    /// 名称（`varchar(200) NOT NULL`）；有 [`OrderRow::link`] 时渲染成链接文字。
    pub name: String,
    /// 件数 / 套数（渲染成 `×N`）。
    pub quantity: i32,
    /// 图纸预览直链。`None` ⇒ 名称渲染成**纯文字**（不加方括号与圆括号）。
    ///
    /// ⚠️ 本字段是 Part 2b 的**接缝**：那一轮把 `t_part_file` → `t_file_preview` →
    /// 直链的链路接上后，这里才会有值。为 `None` 时清单退化成纯文字行，其余形态
    /// （分组 / 缩进 / 截断 / 位置）逐字不变。
    pub link: Option<String>,
    /// 位置的中文展示值（`生产架:A-03` / `工人:张三` / `外协:某某`）。
    /// `None` ⇒ **整项不显示**，连分隔空格都不留（`OFFICE` / 无活跃批次 / holder 行
    /// 已软删三种情形都是它）。
    pub location: Option<String>,
}

/// 第三节的**一组**：逾期组，或某个交期日一组。
#[derive(Debug, Clone)]
pub struct OrderGroup {
    /// 组是哪一类（决定组标题是「逾期未交」还是那个日期）。
    pub kind: OrderGroupKind,
    /// 要渲染的行（装配件行在前、它的子件行紧随其后）。
    pub rows: Vec<OrderRow>,
    /// 未列出的**行**数（子件行算一条）；`0` = 没截断，脚注不出现。
    ///
    /// ⚠️ 报的是**行**数而不是工单数：额度是按行扣的，按工单数报会在「装配件子件多」
    /// 时显得「只少了几条」，而群里其实少了一大片。
    pub omitted: i64,
    /// 该组的**匹配工单数**（**不受** `order_list_limit` 影响）—— 组标题的「N 条」。
    pub total: i32,
}

/// 组的类别。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderGroupKind {
    /// 逾期未交（`system_delivery_date < today`，**不按日期再细分**）。
    Overdue,
    /// 某个交期日（`today <= sdd < today + upcoming_days`）。
    OnDate(NaiveDate),
}

impl OrderGroupKind {
    /// 组标题里 `·` 左边的那段文字（日期组用 `%Y-%m-%d`）。
    fn label(self) -> String {
        match self {
            Self::Overdue => "逾期未交".to_string(),
            Self::OnDate(d) => d.format("%Y-%m-%d").to_string(),
        }
    }
}

/// 一次日报渲染所需的**全部**数字（`service::fetch_rows` 的返回值）。
///
/// ⚠️ 它不是 `T*`（表行投影）也不是 wire 出参：`PreviewOut` 只是它的「加 chunks」
/// 版本。放在本文件是因为它就是渲染器的入参聚合。
#[derive(Debug, Clone, Default)]
pub struct ReportRows {
    pub overview_batch_count: i32,
    pub overview_quantity: i32,
    /// 完工单数（**不进正文**，见 `PreviewOut.completed_count` 的注释）。
    pub completed_count: i32,
    /// 工人主动送检的批次数（第一节正文第二行的「送检 x 批」）。
    pub sent_batch_count: i32,
    /// 当前在制单数（第一节正文第二行的「在制 y 单」，口径见 `repo::OverviewEvents::wip`）。
    pub in_process_count: i32,
    /// 第二节**唯一**的数字：本期出货的去重工单数（= `COUNT(DISTINCT part_id)`）。
    pub note_count: i32,
    /// 第三节逾期组的**未截断匹配工单数**（同时进标题占位符 `{overdue_count}` 与
    /// 流水快照列 `t_wecom_daily_log.overdue_count`）。
    pub overdue_count: i32,
    /// 第三节日期组（`today <= sdd < today + upcoming_days`）的未截断匹配工单数。
    pub upcoming_count: i32,
    /// 第三节的分组清单（**已按 `order_list_limit` 截断**，见 [`OrderGroup::omitted`]）。
    ///
    /// ⚠️ 空 vec = 第三节一条工单都没有 ⇒ 渲染层**只出小标题、不出空态行**（与第四节
    /// 「没有工人行就只出小标题」同款）：「今天没有要催的工单」本身就是好消息，每天
    /// 在日报里加一行「无」是往好消息上刷噪音。管理员仍然能从 `/preview` 的
    /// `sections[]` 看到这一节是开着的。
    pub order_groups: Vec<OrderGroup>,
    /// 图纸预览直链：`owner id → 直链`。
    ///
    /// ⚠️ 键的 id 空间**是 `t_part_file.part_id` 那一列**：散件行传 `t_part.id`、
    /// 装配件行传 `t_assembly.id`（该列 polymorphic、无物理外键）⇒ 两类行用同一个
    /// 字段名承载，**没有**「装配件的直链挂在子件上」这种改写。
    ///
    /// 取不到的工单**不在 map 里**（`get` 得 `None`）⇒ 该行名称渲染成纯文字。
    /// 四种成因同一后果（没有 PDF 图纸 / 预算耗尽 / CI 终局失败 / 打 python 失败），
    /// 见 `files::part_file::preview::service::PreviewService::ensure_ready_for_parts`。
    pub drawing_links: HashMap<i64, String>,
    /// 第三节清单里**本次真的会被列出的**那些行的 owner id（装配件 + 散件 + 子件），
    /// 图纸链路的入参。
    ///
    /// ⚠️ **渲染层不读它**（渲染只认 `OrderRow.link`）；它是给 service 层的「取完数、
    /// 还了连接之后再接图纸」用的中间量。必须与 [`Self::order_groups`] **同一次**
    /// 生成、且**在 `order_list_limit` 截断之后**收集 —— 清单变了而它没跟着变，直链就会
    /// 挂到不在清单里的行上；而 CI 按页计费 ⇒ 多提交一次就多花一次钱。
    pub drawing_part_ids: Vec<i64>,
    /// 全量工人行（`worker_top_n` 的挡人发生在渲染层，**不是**这里）。
    pub worker_rows: Vec<WorkerBarRow>,
}

impl ReportRows {
    /// 各工人 `quantity` 之和（一致性校验用，见 `service::worker_quantity_total`）。
    pub fn worker_quantity_total(&self) -> i32 {
        worker_quantity_total(&self.worker_rows)
    }
}

/// [`ReportRows::worker_quantity_total`] 的自由函数形态（service 层的一致性校验
/// 需要只拿行数组时用）。
pub fn worker_quantity_total(rows: &[WorkerBarRow]) -> i32 {
    rows.iter().map(|r| r.quantity).sum()
}

/// [`render`] 的入参。
#[derive(Debug, Clone, Copy)]
pub struct RenderInput<'a> {
    pub cfg: &'a NormalizedConfig,
    pub rows: &'a ReportRows,
    /// 统计基准日（= 发送当日）—— 标题模板的 `{date}` 用它。
    pub today: NaiveDate,
    /// 统计窗口 `(from_ts, to_ts)`，由 `dto::report_window` 算出（**跨天**）。
    ///
    /// ⚠️ 它同时进**标题块**的「统计区间」那一行：窗口一跨天，「今日 00:00 ~ 今日
    /// 23:59」这种按日历日写死的说法就**必然是假的**（窗口头一天），而读者唯一能核对
    /// 的就是这一行 —— 它错了，整份日报的可信度就没了。
    pub window: (NaiveDateTime, NaiveDateTime),
}

/// [`render`] 的出参。
#[derive(Debug, Clone)]
pub struct Rendered {
    /// 渲染后的标题（**不含**分页后缀；后缀只进各片的 Title 块）。
    pub subject: String,
    /// 将按顺序发送的 N 条，逐条 ≤ [`CHUNK_BYTE_LIMIT`] 字节。
    pub chunks: Vec<String>,
    /// 本次实际生效的节。
    pub sections: Vec<String>,
    /// 全量工人行数（可能 > `chunks` 里出现的行数：`worker_top_n > 0` 时挡掉了尾部）。
    pub worker_row_total: i32,
}

// ============================================================================
// 块
// ============================================================================

/// 块种类（顺序固定，见文件头）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockKind {
    /// 标题 + 统计区间（**每片都重复**）。
    Title,
    /// 第一节：全厂报工总览。
    Overview,
    /// 第二节：本期出货。
    Delivery,
    /// 第三节：**清单为空**时只有小标题（一个块）。
    OrdersHead,
    /// 第三节的**一个渲染行**（装配件行 / 子件行 / 「组标题 + 首行」的粘头块）。
    ///
    /// ⚠️ 它是本域**唯一**允许「一个块装多行」的正文块（最多 3 行：小标题 + 组标题 +
    /// 首行），拆分的边界与后果见文件头「第三节的清单块逐行可拆」。
    OrderLine,
    /// 第三节组末的截断脚注（`order_list_limit` 真的挡掉了行时才有）。
    OrderFooter,
    /// 第四节的小标题（一次）。
    WorkersHead,
    /// 第四节的一行工人（**不可再拆**）。
    WorkerRow,
    /// 第四节的脚注（`worker_top_n` 挡人时才有）。
    Footer,
}

impl BlockKind {
    /// 错误消息里用的节名（防御性分支要**指明是哪一节**）。
    fn section_label(self) -> &'static str {
        match self {
            Self::Title => "标题",
            Self::Overview => "第一节（全厂报工总览）",
            Self::Delivery => "第二节（本期出货）",
            Self::OrdersHead | Self::OrderLine | Self::OrderFooter => {
                "第三节（逾期未交 / 交期工单）"
            }
            Self::WorkersHead | Self::WorkerRow | Self::Footer => "第四节（工人报工条形图）",
        }
    }
}

/// 一个可分片的块。`section` 供错误消息定位，`kind` 供 pass 2 认出标题块。
#[derive(Debug, Clone)]
struct Block {
    kind: BlockKind,
    text: String,
}

impl Block {
    fn new(kind: BlockKind, text: String) -> Self {
        Self { kind, text }
    }
}

// ============================================================================
// 渲染入口
// ============================================================================

/// 渲染并装填分片。
///
/// 失败只有一种：**装不下**（`21804`）。本函数**永不返回空 `chunks`**（四节全关
/// 由 service 层先拦掉，`21804`），也不做任何内容截断。
pub fn render(input: &RenderInput) -> Result<Rendered, AppError> {
    let cfg = input.cfg;
    let rows = input.rows;
    let today = input.today;

    let subject = render_subject(cfg, rows, today);
    let blocks = build_blocks(input, &subject);
    let chunks = assemble(&blocks, cfg.max_chunks)?;

    let mut sections = Vec::with_capacity(4);
    if cfg.include_overview {
        sections.push(SECTION_OVERVIEW.to_string());
    }
    if cfg.include_delivery {
        sections.push(SECTION_DELIVERY.to_string());
    }
    if cfg.include_orders {
        sections.push(SECTION_ORDERS.to_string());
    }
    if cfg.include_workers {
        sections.push(SECTION_WORKERS.to_string());
    }

    Ok(Rendered {
        subject,
        chunks,
        sections,
        worker_row_total: rows.worker_rows.len() as i32,
    })
}

/// 标题渲染：`subject_template` 为空 ⇒ 域内默认串。
///
/// ## 占位符（**只认这五个**，未知占位符**原样保留**）
/// `{date}` / `{batch_count}` / `{quantity}` / `{note_count}` / `{overdue_count}`。
/// 照 `mail_digest` 的既有口径：未知占位符原样保留（管理员能一眼看出自己写错了什么）
/// 比静默吞掉好；**没有别的**内置占位符可换。
///
/// ⚠️ **`{note_count}` 喂的是第二节的**唯一**数字**（本期出货的去重工单数）：第二节
/// 改口径后不再统计「送货单张数」，但**占位符名保留**——存量模板里已经写着
/// `{note_count}`，删掉这个替换会让它在标题里变成一串没人解释的 `{note_count}`。
/// （2026-10-23 起 `rows.note_count` 承载这个数，`note_part_count` 那一键随列下线。）
fn render_subject(cfg: &NormalizedConfig, rows: &ReportRows, today: NaiveDate) -> String {
    let tpl = cfg
        .subject_template
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or(DEFAULT_SUBJECT_TEMPLATE);
    tpl.replace("{date}", &today.format("%Y-%m-%d").to_string())
        .replace("{batch_count}", &rows.overview_batch_count.to_string())
        .replace("{quantity}", &rows.overview_quantity.to_string())
        .replace("{note_count}", &rows.note_count.to_string())
        .replace("{overdue_count}", &rows.overdue_count.to_string())
}

/// 域内默认标题模板（`subject_template` 为空时用）。
pub const DEFAULT_SUBJECT_TEMPLATE: &str = "# 生产日报 · {date}";

/// 按固定顺序产出全部块。**关掉的节完全不出现**（不是「出现标题 + 0」）。
fn build_blocks(input: &RenderInput, subject: &str) -> Vec<Block> {
    let cfg = input.cfg;
    let rows = input.rows;
    let (from_ts, to_ts) = input.window;

    let mut out: Vec<Block> = Vec::new();

    // ── 标题块（每片都重复；统计区间那行也重复，好让每片自带上下文）──
    out.push(Block::new(
        BlockKind::Title,
        format!(
            "{subject}\n\n> 统计区间 <font color=\"comment\">{} ~ {}</font>",
            from_ts.format("%m-%d %H:%M"),
            to_ts.format("%m-%d %H:%M")
        ),
    ));

    if cfg.include_overview {
        out.push(Block::new(
            BlockKind::Overview,
            format!(
                "## 一、全厂报工总览\n\
                 > 报工：<font color=\"info\">{}</font> 批  共 <font color=\"info\">{}</font> 件\n\
                 > 送检 <font color=\"info\">{}</font> 批  在制 <font color=\"info\">{}</font> 单",
                rows.overview_batch_count,
                rows.overview_quantity,
                rows.sent_batch_count,
                rows.in_process_count,
            ),
        ));
    }

    if cfg.include_delivery {
        out.push(Block::new(
            BlockKind::Delivery,
            format!(
                "## 二、本期出货\n\
                 > 出货：<font color=\"info\">{}</font> 单",
                rows.note_count,
            ),
        ));
    }

    if cfg.include_orders {
        out.extend(order_blocks(cfg, rows));
    }

    if cfg.include_workers {
        out.push(Block::new(
            BlockKind::WorkersHead,
            format!("## 四、工人报工（{}）", cfg.worker_metric.heading_suffix(),),
        ));
        // ── 工人行：`worker_top_n > 0` 时只取前 N（**管理员主动设的展示上限**）──
        let cap = if cfg.worker_top_n > 0 {
            (cfg.worker_top_n as usize).min(rows.worker_rows.len())
        } else {
            rows.worker_rows.len()
        };
        let shown = &rows.worker_rows[..cap];
        out.extend(worker_line_blocks(cfg, shown));
        // ⚠️ 脚注**只在「真的挡掉了人」时**出现；`worker_top_n == 0`（不限）不出现。
        let omitted = rows.worker_rows.len() - shown.len();
        if omitted > 0 {
            out.push(Block::new(
                BlockKind::Footer,
                format!(
                    "> <font color=\"comment\">另有 {omitted} 人在此省略（配置「最多列出工人」= {}）</font>",
                    cfg.worker_top_n
                ),
            ));
        }
    }

    out
}

/// 第三节的清单块（**逾期组在前，日期组按日期升序接在后面** —— 顺序由
/// `service::fetch_rows` 的分组保证，本函数只照着渲）。
///
/// ## 一行长什么样（逐字形态）
///
/// ```text
/// > F1001 [名称甲](https://…/p1.jpg) ×2  工人:张三
/// >   └ F1003 [子件丙](https://…/p3.jpg) ×4  品检架:B-11
/// ```
///
/// 即 `序列号 [名称](图纸直链) ×数量  位置`，四个字段各自的边界见
/// [`OrderRow`] 的字段注释。
///
/// ## 组标题
///
/// `> **逾期未交 · 12 条**` / `> **2026-10-15 · 7 条**` —— 「N 条」是
/// [`OrderGroup::total`]（**未截断**的匹配工单数），不是实际列出的行数；被挡掉的部分
/// 由组末脚注单独交代。
///
/// ## 空清单
///
/// ⚠️ **一条工单都没有时只出小标题**，不出任何组标题、不出「无」行（理由见
/// [`ReportRows::order_groups`]）。
fn order_blocks(cfg: &NormalizedConfig, rows: &ReportRows) -> Vec<Block> {
    let heading = format!("## 三、逾期未交 / {} 日内交期", cfg.upcoming_days);
    if rows.order_groups.is_empty() {
        return vec![Block::new(BlockKind::OrdersHead, heading)];
    }

    let mut out: Vec<Block> = Vec::new();
    for (gi, group) in rows.order_groups.iter().enumerate() {
        // ⚠️ 组**恒**至少有一行（`order_list_limit == 0` 表示不限，否则至少留第一行），
        // 所以 `first()` 不会落空；真落空时也让标题自己成块，不 panic。
        let mut lines = group.rows.iter();
        let mut block = String::new();
        if gi == 0 {
            // 第一组把小标题也吸进来 —— 否则会出现「一小节只有标题、下一片才有内容」。
            block.push_str(&heading);
            block.push('\n');
        }
        block.push_str(&group_heading(group));
        if let Some(first) = lines.next() {
            block.push('\n');
            block.push_str(&order_line(first));
            out.push(Block::new(BlockKind::OrderLine, block));
        } else {
            out.push(Block::new(BlockKind::OrderLine, block));
        }
        for r in lines {
            out.push(Block::new(BlockKind::OrderLine, order_line(r)));
        }
        if group.omitted > 0 {
            out.push(Block::new(
                BlockKind::OrderFooter,
                format!(
                    "> <font color=\"comment\">另有 {} 条未列出</font>",
                    group.omitted
                ),
            ));
        }
    }
    out
}

/// 组标题行：`> **逾期未交 · 12 条**` / `> **2026-10-15 · 7 条**`。
fn group_heading(group: &OrderGroup) -> String {
    format!("> **{} · {} 条**", group.kind.label(), group.total)
}

/// 一个清单行：`序列号 [名称](直链) ×数量  位置`。
///
/// ## ⚠️ 三个「缺了就不出现」的字段
///
/// 1. **序列号**为 `None` ⇒ 字段**连同它后面的一个分隔空格**一起去掉（不是留下双空格
///    悬在那里：`markdown` v1 会把行首连续空格当缩进吃掉，留下的空格会让同一类行的
///    行首宽度变得不可预测）；
/// 2. **图纸直链**为 `None` ⇒ 名称渲染成**纯文字**（不加 `[]()`），因为一个指向空处的
///    链接比没有链接更让人困惑；
/// 3. **位置**为 `None` ⇒ **整项不显示**，连它前面那两个分隔空格一起去掉（判据见
///    [`OrderRow::location`]）。
///
/// ⚠️ 本函数**不做任何转义**：本域渲染的是 markdown 而非 HTML，名称里的 `<` / `>`
/// 原样透传（守卫 ⑭ 对全篇有效）。
fn order_line(r: &OrderRow) -> String {
    let prefix = if r.is_child {
        CHILD_PREFIX
    } else {
        ORDER_PREFIX
    };
    let name = match r.link.as_deref() {
        Some(url) => format!("[{}]({url})", r.name),
        None => r.name.clone(),
    };
    let head = match r.serial_no.as_deref() {
        // 有序列号：`> F1001 名称 …`（序列号与其后恰一个空格）。
        Some(s) => format!("{prefix}{s} {name}"),
        // 无序列号：整个字段去掉，行首恒是 `prefix`。
        None => format!("{prefix}{name}"),
    };
    let mut line = format!("{head} ×{}", r.quantity);
    if let Some(loc) = r.location.as_deref() {
        line.push_str("  ");
        line.push_str(loc);
    }
    line
}

/// 清单行的前缀：工单行 `> `，子件行 `>   └ `。
///
/// ⚠️ 子件前缀里那 3 个空格**不承载信息**（v1 会吃掉行首连续空格），层级靠 `└`
/// 这个字面量 —— 理由见 [`OrderRow::is_child`]。
const ORDER_PREFIX: &str = "> ";
const CHILD_PREFIX: &str = ">   └ ";

/// 工人行块（每行一个块，**不可再拆**）。
///
/// ## 三条排版口径（每条都有反例）
///
/// 1. **条形放行尾、数值放行首**：`█`(U+2588) / `░`(U+2591) 在不同字体下是全宽
///    还是比例宽**不可控**，放行尾则宽度抖动不可见；数值列的行首参差是可接受的排版噪声。
/// 2. **姓名按「显示宽度」补空格**（CJK 计 2、ASCII 计 1）：否则 `张三`（宽 4）与
///    `王`（宽 2）起跑的条不在同一列。
/// 3. ⚠️ **`markdown` v1 的行首连续空格会被当缩进吃掉** ⇒ 数值**不做右对齐补空格**，
///    直接从行首写起。
fn worker_line_blocks(cfg: &NormalizedConfig, shown: &[WorkerBarRow]) -> Vec<Block> {
    if shown.is_empty() {
        // ⚠️ 没有工人行时**只出小标题、不出空态行**：`## 四、工人报工（送件数）` 下面
        // 什么都没有，管理员看到的是「今天没人报工」而不是「数据坏了」。
        return Vec::new();
    }
    let name_width = shown
        .iter()
        .map(|r| display_width(&r.worker_name))
        .max()
        .unwrap_or(0);
    let max_v = shown
        .iter()
        .map(|r| cfg.worker_metric.value_of(r))
        .max()
        .unwrap_or(0);

    shown
        .iter()
        .map(|r| {
            let v = cfg.worker_metric.value_of(r);
            let pad = " ".repeat(name_width.saturating_sub(display_width(&r.worker_name)));
            Block::new(
                BlockKind::WorkerRow,
                format!("> {v} {}{pad} {}", r.worker_name, bar_cells(v, max_v)),
            )
        })
        .collect()
}

/// 条形：满格数 `round(BAR_CELLS * v / max_v)`（`max_v == 0` 时全部空条），
/// **恒占满 [`BAR_CELLS`] 格**（不足补 `░`）⇒ 每行等宽。
fn bar_cells(v: i32, max_v: i32) -> String {
    let filled = if max_v <= 0 || v <= 0 {
        0
    } else {
        let raw = (BAR_CELLS as f64) * (v as f64) / (max_v as f64);
        let rounded = raw.round() as usize;
        rounded.min(BAR_CELLS)
    };
    let mut s = String::with_capacity(BAR_CELLS * 3);
    for _ in 0..filled {
        s.push(BAR_FULL);
    }
    for _ in filled..BAR_CELLS {
        s.push(BAR_EMPTY);
    }
    s
}

/// 显示宽度：CJK / 全角按 2 列，其余按 1 列。
///
/// ⚠️ 用 East Asian **Wide / Fullwidth** 区段判定（与浏览器、终端的默认行为同源）；
/// `█` / `░` 落在 **Ambiguous** 区段，按 1 列算 —— 它们恒在**行尾**，宽度抖动不参与
/// 对齐计算（见 `worker_line_blocks` 的口径 1）。
pub fn display_width(s: &str) -> usize {
    s.chars().map(char_width).sum()
}

fn char_width(c: char) -> usize {
    let cp = c as u32;
    let wide = (0x1100..=0x115F).contains(&cp)   // 韩文字母
        || (0x2E80..=0x303E).contains(&cp)      // CJK 部首、标点
        || (0x3041..=0x33FF).contains(&cp)      // 假名、CJK 兼容
        || (0x3400..=0x4DBF).contains(&cp)      // CJK 扩展 A
        || (0x4E00..=0x9FFF).contains(&cp)      // CJK 统一表意
        || (0xA000..=0xA4CF).contains(&cp)      // 彝文
        || (0xAC00..=0xD7A3).contains(&cp)      // 韩文音节
        || (0xF900..=0xFAFF).contains(&cp)      // CJK 兼容表意
        || (0xFE10..=0xFE19).contains(&cp)      // 竖排标点
        || (0xFE30..=0xFE6F).contains(&cp)      // CJK 兼容形式
        || (0xFF00..=0xFF60).contains(&cp)      // 全角
        || (0xFFE0..=0xFFE6).contains(&cp)
        || (0x20000..=0x3FFFD).contains(&cp); // CJK 扩展 B 及以上
    if wide { 2 } else { 1 }
}

// ============================================================================
// 两趟装填
// ============================================================================

/// pass 1（装填 + 单块超限守卫）→ `max_chunks` 守卫 → pass 2（追加页码后缀）。
///
/// ## ⚠️ **标题块在每一片里都重复**（一份**副本**，不是「只有第一片有」）
///
/// 开新片时先把标题块复制进去 —— 否则第 2 片在群里是几行孤零零的工人数据、
/// 没有日期上下文。这也是 pass 2 能「无脑给每片的第 0 块加页码后缀」的前提：
/// **每片的第 0 块恒是标题块**。
fn assemble(blocks: &[Block], max_chunks: i16) -> Result<Vec<String>, AppError> {
    let budget = CHUNK_BYTE_LIMIT - RESERVE;
    let Some(title) = blocks.first() else {
        return Ok(Vec::new());
    };
    // 标题块自身超限也要拦（`subject_template` 很长时它就是最大块）。
    if title.text.len() > budget {
        return Err(block_too_large(title, budget));
    }
    let rest = &blocks[1..];

    // ── pass 1：逐块装入当前片，超则开新片（新片**先放标题副本**）──
    let mut groups: Vec<Vec<&Block>> = Vec::new();
    let mut cur: Vec<&Block> = vec![title];
    let mut cur_bytes = title.text.len();
    for b in rest {
        let own = b.text.len();
        if own > budget {
            return Err(block_too_large(b, budget));
        }
        if cur_bytes + 1 + own > budget {
            groups.push(std::mem::replace(&mut cur, vec![title]));
            cur_bytes = title.text.len();
        }
        cur_bytes += 1 + own;
        cur.push(b);
    }
    groups.push(cur);

    // ── 防御性分支 ②：片数超上限 ⇒ 报错并给出两个可行动方向，绝不截断 ──
    let total = groups.len();
    let cap = max_chunks.max(0) as usize;
    if total > cap {
        return Err(AppError::biz(
            code::BIZ_WECOM_CONFIG_INVALID,
            format!(
                "本次日报需要 {total} 条，但配置上限为 {cap}；\
                 请调高「最多拆几条」或调低「最多列出工人」"
            ),
        ));
    }

    // ── pass 2：已知 N，为每片的标题块（恒为第 0 块）追加「（i/N）」（N == 1 时不追加）──
    let out: Vec<String> = groups
        .iter()
        .enumerate()
        .map(|(idx, group)| {
            let mut s = String::new();
            for (j, b) in group.iter().enumerate() {
                if j > 0 {
                    s.push('\n');
                }
                if j == 0 && total > 1 {
                    s.push_str(&with_page_suffix(&b.text, idx + 1, total));
                } else {
                    s.push_str(&b.text);
                }
            }
            // ⚠️ RESERVE 保证这里恒成立；debug 下钉死。
            debug_assert!(
                s.len() <= CHUNK_BYTE_LIMIT,
                "pass 2 之后超了 {CHUNK_BYTE_LIMIT} 字节（RESERVE 失效）：{}",
                s.len()
            );
            s
        })
        .collect();

    // 正常路径恒为 `Ok`；这道 `Result` 让「万一 RESERVE 被改小」变成一条可读的
    // 报错，而不是一个 panic。
    if out.iter().any(|s| s.len() > CHUNK_BYTE_LIMIT) {
        return Err(AppError::biz(
            code::BIZ_WECOM_CONFIG_INVALID,
            format!("渲染结果超出单条消息 {CHUNK_BYTE_LIMIT} 字节上限（内部装填预算失效）"),
        ));
    }
    Ok(out)
}

/// 防御性分支 ①的文案：**指明是哪一节**（当前最大块是 `WorkersHead`，
/// 但 `subject_template` 很长时是标题块）。
fn block_too_large(b: &Block, budget: usize) -> AppError {
    AppError::biz(
        code::BIZ_WECOM_CONFIG_INVALID,
        format!(
            "{}的内容单块就有 {} 字节，超过单条消息预算 {budget} 字节：\
             这是渲染层的防御性上限，不是内容超长——请缩短标题模板（配置「消息标题」）",
            b.kind.section_label(),
            b.text.len(),
        ),
    )
}

/// 给标题块的**首行**行尾追加「（i/N）」。
fn with_page_suffix(title_block: &str, i: usize, n: usize) -> String {
    let suffix = format!("（{i}/{n}）");
    match title_block.find('\n') {
        Some(at) => format!("{}{}{}", &title_block[..at], suffix, &title_block[at..]),
        None => format!("{title_block}{suffix}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_with(top_n: i32, max_chunks: i16) -> NormalizedConfig {
        NormalizedConfig {
            worker_top_n: top_n,
            max_chunks,
            ..NormalizedConfig::default()
        }
    }

    fn today() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 10, 18).expect("固定测试日期合法")
    }

    fn rows_with_workers(n: usize) -> ReportRows {
        ReportRows {
            overview_batch_count: 128,
            overview_quantity: 456,
            completed_count: 37,
            sent_batch_count: 86,
            in_process_count: 92,
            note_count: 214,
            overdue_count: 12,
            upcoming_count: 7,
            order_groups: order_groups_fixture(),
            drawing_links: HashMap::new(),
            drawing_part_ids: Vec::new(),
            worker_rows: (0..n)
                .map(|i| WorkerBarRow {
                    worker_name: format!("工人{i:03}"),
                    badge_code: format!("W-{i:03}"),
                    batch_count: (n - i) as i32,
                    quantity: ((n - i) * 3) as i32,
                })
                .collect(),
        }
    }

    /// 一个清单行（测试用默认值：有序列号、无位置、无链接）。
    fn order_row(serial: &str, name: &str, qty: i32) -> OrderRow {
        OrderRow {
            // ⚠️ fixture 的 owner_id 只为让结构体编译得过，渲染层不读它（见该字段 doc）。
            owner_id: 0,
            is_child: false,
            serial_no: Some(serial.to_string()),
            name: name.to_string(),
            quantity: qty,
            link: None,
            location: None,
        }
    }

    /// 一个子件行（只差 `is_child` 与前缀）。
    fn child_row(serial: &str, name: &str, qty: i32) -> OrderRow {
        OrderRow {
            is_child: true,
            ..order_row(serial, name, qty)
        }
    }

    /// 一组（默认没截断）。
    fn group(kind: OrderGroupKind, rows: Vec<OrderRow>, total: i32) -> OrderGroup {
        OrderGroup {
            kind,
            rows,
            omitted: 0,
            total,
        }
    }

    /// 全域 fixture 的第三节清单：**逾期组**（散件 1 行 + 装配件 1 行带 2 个子件）
    /// 与**一个日期组**（2 行）—— 覆盖「子件缩进」「位置」「组标题」三种形态。
    fn order_groups_fixture() -> Vec<OrderGroup> {
        let mut parent = order_row("F2000", "装配件乙", 1);
        parent.location = Some("生产架:A-03".to_string());
        vec![
            group(
                OrderGroupKind::Overdue,
                vec![
                    {
                        let mut r = order_row("F1001", "名称甲", 2);
                        r.location = Some("工人:张三".to_string());
                        r
                    },
                    parent,
                ],
                12,
            ),
            group(
                OrderGroupKind::OnDate(today()),
                vec![
                    child_row("F1003", "子件丙", 4),
                    child_row("F1004", "子件丁", 5),
                ],
                7,
            ),
        ]
    }

    /// 缺省配置下的统计窗口（锚点 = 缺省推送时刻 20:05，跨天）。
    fn window() -> (NaiveDateTime, NaiveDateTime) {
        crate::modules::wecom_daily::dto::report_window(&NormalizedConfig::default(), today())
    }

    fn render_ok(cfg: &NormalizedConfig, rows: &ReportRows) -> Rendered {
        render(&RenderInput {
            cfg,
            rows,
            today: today(),
            window: window(),
        })
        .expect("不该报 21804")
    }

    fn lines_of(chunk: &str) -> Vec<&str> {
        chunk.lines().collect()
    }

    /// 行里含条形 ⇒ 是工人行。
    fn is_bar_line(line: &str) -> bool {
        line.contains(BAR_FULL) || line.contains(BAR_EMPTY)
    }

    /// ★ 守卫 ①：分片**永远不把一个工人行劈成两半**。
    #[test]
    fn blocks_never_split_a_worker_row() {
        // 名字长到单个行块就接近预算 ⇒ 必然多片，且 WorkerRow 不可拆。
        let rows = ReportRows {
            worker_rows: (0..12)
                .map(|i| WorkerBarRow {
                    worker_name: format!("{}{i:02}", "超长工人名字".repeat(90)),
                    badge_code: format!("W{i}"),
                    batch_count: 100 - i,
                    quantity: 10 * (100 - i),
                })
                .collect(),
            ..ReportRows::default()
        };
        let cfg = cfg_with(0, 20);
        let out = render_ok(&cfg, &rows);
        assert!(
            out.chunks.len() >= 2,
            "这批数据必须逼出多片，实际 {}",
            out.chunks.len()
        );

        // 收集每片里的工人行，逐条都能与「本域渲染出来的完整行」对上。
        let mut seen = 0usize;
        for chunk in &out.chunks {
            for line in lines_of(chunk) {
                if is_bar_line(line) {
                    assert!(
                        line.starts_with("> ")
                            && (line.ends_with(BAR_FULL) || line.ends_with(BAR_EMPTY)),
                        "工人行被劈开了（行首/行尾形态不对）：{line}"
                    );
                    seen += 1;
                }
            }
        }
        assert_eq!(
            seen, 12,
            "12 个工人必须各自完整出现一次（不劈开、不丢、不重）"
        );
    }

    /// ★ 守卫 ②：`N == 1` 时不出现「（1/1）」。
    #[test]
    fn single_chunk_has_no_page_suffix() {
        let rows = rows_with_workers(3);
        let out = render_ok(&cfg_with(0, 3), &rows);
        assert_eq!(out.chunks.len(), 1);
        assert!(
            !out.chunks[0].contains("（1/1）"),
            "单条消息不该挂「（1/1）」：{}",
            out.chunks[0]
        );
        // 出参的 subject **也不带**后缀（后缀只进各片的 Title 块）。
        assert_eq!(out.subject, "# 生产日报 · 2026-10-18");
    }

    /// ★ 守卫 ③：200 工人 + 中文名 + 四节全开 ⇒ 逐片 ≤ 4096 **字节**。
    #[test]
    fn every_chunk_within_byte_budget() {
        let rows = rows_with_workers(200);
        let cfg = cfg_with(0, 20);
        let out = render_ok(&cfg, &rows);
        assert!(out.chunks.len() >= 2, "200 个工人必须多片");
        for (i, c) in out.chunks.iter().enumerate() {
            assert!(
                c.len() <= CHUNK_BYTE_LIMIT,
                "第 {} 片 {} 字节 > {CHUNK_BYTE_LIMIT}",
                i + 1,
                c.len()
            );
            // 中文按 3 字节 ⇒ 每片远小于字符数能给出的印象，这里钉住「按字节算」。
            assert!(!c.is_empty());
        }
    }

    /// ★ 守卫 ④：单块超限 ⇒ `21804` + **指明是哪一节**，且不产出任何被砍掉的内容。
    #[test]
    fn oversize_single_block_is_rejected_not_truncated() {
        let rows = rows_with_workers(2);
        let cfg = NormalizedConfig {
            subject_template: Some("# ".to_string() + &"标".repeat(4000)),
            ..cfg_with(0, 20)
        };
        let err = render(&RenderInput {
            cfg: &cfg,
            rows: &rows,
            today: today(),
            window: window(),
        })
        .expect_err("标题块超限必须报错");
        assert_eq!(err.code(), code::BIZ_WECOM_CONFIG_INVALID);
        let msg = err.to_string();
        assert!(msg.contains("标题"), "错误消息要指明是哪一节：{msg}");
    }

    /// ★ 守卫 ⑤：片数超 `max_chunks` ⇒ `21804`，**不截断**。
    #[test]
    fn exceeding_max_chunks_is_rejected_not_truncated() {
        let rows = rows_with_workers(200);
        let cfg = cfg_with(0, 1);
        let err = render(&RenderInput {
            cfg: &cfg,
            rows: &rows,
            today: today(),
            window: window(),
        })
        .expect_err("超上限必须报错");
        assert_eq!(err.code(), code::BIZ_WECOM_CONFIG_INVALID);
        let msg = err.to_string();
        assert!(msg.contains("最多拆几条"), "要给出可行动方向：{msg}");
        assert!(
            msg.contains("最多列出工人"),
            "要给出第二个可行动方向：{msg}"
        );
        assert!(!msg.contains("截断"), "本域分片不是截断：{msg}");
    }

    /// ★ 守卫 ⑥：关掉的节**完全不出现**（不是「出现标题 + 0」）。
    #[test]
    fn closed_section_is_not_rendered_at_all() {
        let rows = rows_with_workers(2);
        let cases: [(bool, &str, &str); 4] = [
            (false, "一、全厂报工总览", "报工："),
            (false, "二、本期出货", "出货："),
            (false, "三、逾期未交", "逾期未交"),
            (false, "四、工人报工", "工人报工"),
        ];
        for (idx, (keep_open, heading, needle)) in cases.into_iter().enumerate() {
            let mut cfg = cfg_with(0, 20);
            // 四个开关按 cases 顺序逐个关掉（其余保持开）。
            match idx {
                0 => cfg.include_overview = keep_open,
                1 => cfg.include_delivery = keep_open,
                2 => cfg.include_orders = keep_open,
                _ => cfg.include_workers = keep_open,
            }
            let out = render_ok(&cfg, &rows);
            let all = out.chunks.join("\n");
            assert!(!all.contains(heading), "关掉的节不该出现标题 {heading}");
            assert!(!all.contains(needle), "关掉的节不该出现内容 {needle}");
        }
    }

    /// ★ 守卫 ⑯：第一节 / 第二节的**逐字正文**（口径一改，最容易悄悄漂的就是这两块）。
    ///
    /// ⚠️ 「全文不得再出现『今日』」这一条才是本用例的重点：窗口跨天之后（见
    /// `dto::report_window`），任何留着「今日」的措辞都**必然是错的** —— 它会把昨天
    /// 20:05 之后到今天 20:05 之前的那一段说成「今天」。
    #[test]
    fn overview_and_delivery_sections_render_the_agreed_wording() {
        let rows = rows_with_workers(1);
        let out = render_ok(&cfg_with(0, 20), &rows);
        let all = out.chunks.join("\n");
        assert!(all.contains("## 一、全厂报工总览"), "{all}");
        assert!(
            all.contains(
                "> 报工：<font color=\"info\">128</font> 批  共 <font color=\"info\">456</font> 件"
            ),
            "{all}"
        );
        assert!(
            all.contains(
                "> 送检 <font color=\"info\">86</font> 批  在制 <font color=\"info\">92</font> 单"
            ),
            "{all}"
        );
        assert!(all.contains("## 二、本期出货"), "{all}");
        assert!(
            all.contains("> 出货：<font color=\"info\">214</font> 单"),
            "{all}"
        );
        assert!(
            !all.contains("今日"),
            "窗口跨天，全文不该再出现「今日」措辞：{all}"
        );
    }

    /// ★ 守卫 ⑰：标题块的「统计区间」是**真实的跨天窗口两端**，不是写死的日历日。
    ///
    /// 缺省锚点 20:05、`today() = 2026-10-18` ⇒ 窗口 `[10-17 20:05, 10-18 20:05)`。
    /// ⚠️ 变异验证：把 `report_window` 改回「当天 00:00 ~ 当天 23:59」时本条立刻红。
    #[test]
    fn title_block_shows_the_real_cross_day_window() {
        let rows = rows_with_workers(1);
        let out = render_ok(&cfg_with(0, 20), &rows);
        let all = out.chunks.join("\n");
        assert!(
            all.contains("> 统计区间 <font color=\"comment\">10-17 20:05 ~ 10-18 20:05</font>"),
            "{all}"
        );
    }

    /// ★ 守卫 ⑦：姓名按**显示宽度**对齐 ⇒ 条形起始列一致。
    ///
    /// ⚠️ **fixture 里的数值刻意都是两位**（42 / 25 / 30）：本域**刻意不做数值的
    /// 右对齐补空格**（`markdown` v1 会吃掉行首连续空格，见 `worker_line_blocks` 的
    /// 口径 3），所以「条形起始列」= `"> " + 数值 + " " + 定宽姓名域 + " "`，
    /// 数值宽度一变列就跟着变。**姓名域定宽**这一条才是本守卫要钉的不变量 ——
    /// 用同宽数值才能同时把它和「条形起始列一致」这两句话都断言到。
    /// 另有一条守卫专门覆盖「数值宽度不同时姓名域仍定宽」。
    #[test]
    fn bar_rows_are_display_width_aligned() {
        let rows = ReportRows {
            worker_rows: vec![
                WorkerBarRow {
                    worker_name: "张三".into(),
                    badge_code: "W1".into(),
                    batch_count: 3,
                    quantity: 42,
                },
                WorkerBarRow {
                    worker_name: "王五".into(),
                    badge_code: "W2".into(),
                    batch_count: 2,
                    quantity: 25,
                },
                WorkerBarRow {
                    worker_name: "李".into(),
                    badge_code: "W3".into(),
                    batch_count: 1,
                    quantity: 30,
                },
            ],
            ..ReportRows::default()
        };
        let out = render_ok(&cfg_with(0, 20), &rows);
        let bar_lines: Vec<&str> = out
            .chunks
            .iter()
            .flat_map(|c| lines_of(c))
            .filter(|l| is_bar_line(l))
            .collect();
        assert_eq!(bar_lines.len(), 3);
        let columns: Vec<usize> = bar_lines
            .iter()
            .map(|l| display_width(&l[..l.find(BAR_FULL).expect("行内必有实心格")]))
            .collect();
        assert_eq!(
            columns[0], columns[1],
            "张三(宽4) 与 王五(宽4)：{bar_lines:?}"
        );
        assert_eq!(
            columns[1], columns[2],
            "王五(宽4) 与 李(宽2，需补 2 空格)：{bar_lines:?}"
        );
    }

    /// 姓名域**定宽**这一条与数值宽度无关（数值 1 位与 2 位混排时仍成立）。
    #[test]
    fn name_field_is_fixed_width_regardless_of_value_width() {
        let rows = ReportRows {
            worker_rows: vec![
                WorkerBarRow {
                    worker_name: "张三丰".into(),
                    badge_code: "W1".into(),
                    batch_count: 1,
                    quantity: 7,
                },
                WorkerBarRow {
                    worker_name: "李".into(),
                    badge_code: "W2".into(),
                    batch_count: 1,
                    quantity: 700,
                },
            ],
            ..ReportRows::default()
        };
        let out = render_ok(&cfg_with(0, 20), &rows);
        let all = out.chunks.join("\n");
        // 姓名域宽 = max(display_width) = 6（「张三丰」）；「李」后面补 4 个空格
        // （补空格 + 分隔空格 = 5 个连续空格）。
        assert!(all.contains("> 7 张三丰 ░░░░░░░░░░░░"), "{all}");
        assert!(all.contains("> 700 李     ████████████"), "{all}");
    }

    /// ★ 守卫 ⑧：条**恒占满** 12 格（最短条全 `░`）。
    #[test]
    fn bar_length_always_fills_bar_cells() {
        let rows = ReportRows {
            worker_rows: vec![
                WorkerBarRow {
                    worker_name: "甲".into(),
                    badge_code: "W1".into(),
                    batch_count: 1,
                    quantity: 100,
                },
                WorkerBarRow {
                    worker_name: "乙".into(),
                    badge_code: "W2".into(),
                    batch_count: 1,
                    quantity: 1,
                },
            ],
            ..ReportRows::default()
        };
        let out = render_ok(&cfg_with(0, 20), &rows);
        for chunk in &out.chunks {
            for line in lines_of(chunk).into_iter().filter(|l| is_bar_line(l)) {
                let bar: String = line
                    .chars()
                    .filter(|c| *c == BAR_FULL || *c == BAR_EMPTY)
                    .collect();
                assert_eq!(
                    bar.chars().count(),
                    BAR_CELLS,
                    "条必须恒占满 {BAR_CELLS} 格：{line}"
                );
            }
        }
        let all = out.chunks.join("\n");
        // 最短那条是全空条（1/100 × 12 ≈ 0 满格）。
        assert!(all.contains("> 1 乙 ░░░░░░░░░░░░"), "最短条应全空：{all}");
    }

    /// ★ 守卫 ⑨：数值在**行首**、条形在**行尾**（且数值不做右对齐补空格）。
    #[test]
    fn bar_is_last_and_value_is_first() {
        let rows = rows_with_workers(3);
        let out = render_ok(&cfg_with(0, 20), &rows);
        for chunk in &out.chunks {
            for line in lines_of(chunk).into_iter().filter(|l| is_bar_line(l)) {
                assert!(line.starts_with("> "), "行首必须是引用 + 数值：{line}");
                let rest = &line[2..];
                let value: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
                assert!(!value.is_empty(), "数值必须紧跟行首（不补空格）：{line}");
                assert!(
                    line.ends_with(BAR_EMPTY) || line.ends_with(BAR_FULL),
                    "行尾必须是条形：{line}"
                );
            }
        }
    }

    /// ★ 守卫 ⑩：未知占位符**原样保留**。
    #[test]
    fn unknown_placeholder_in_subject_template_is_kept_verbatim() {
        let rows = rows_with_workers(1);
        let cfg = NormalizedConfig {
            subject_template: Some("# {date} {unknown} {batch_count}".into()),
            ..cfg_with(0, 20)
        };
        let out = render_ok(&cfg, &rows);
        assert_eq!(out.subject, "# 2026-10-18 {unknown} 128");
        assert!(out.chunks[0].contains("{unknown}"), "原文要出现在消息里");
    }

    /// ★ 守卫 ⑪：`worker_metric` 同时切换**条长依据**与**标题 / 行内数值**。
    #[test]
    fn worker_metric_switches_bar_metric_and_heading() {
        // 两个人：A 的 `quantity` 大但 `batch_count` 小；B 反之 ⇒ 两套口径的排序相反。
        let rows = ReportRows {
            worker_rows: vec![
                WorkerBarRow {
                    worker_name: "甲".into(),
                    badge_code: "W1".into(),
                    batch_count: 2,
                    quantity: 100,
                },
                WorkerBarRow {
                    worker_name: "乙".into(),
                    badge_code: "W2".into(),
                    batch_count: 9,
                    quantity: 5,
                },
            ],
            ..ReportRows::default()
        };
        let by_q = render_ok(
            &NormalizedConfig {
                worker_metric: crate::modules::wecom_daily::dto::WorkerMetric::Quantity,
                ..cfg_with(0, 20)
            },
            &rows,
        );
        let all_q = by_q.chunks.join("\n");
        assert!(all_q.contains("工人报工（送件数）"), "{all_q}");
        assert!(all_q.contains("> 100 甲 ████████████"), "{all_q}");
        assert!(all_q.contains("> 5 乙 "), "{all_q}");

        let by_b = render_ok(
            &NormalizedConfig {
                worker_metric: crate::modules::wecom_daily::dto::WorkerMetric::Batch,
                ..cfg_with(0, 20)
            },
            &rows,
        );
        let all_b = by_b.chunks.join("\n");
        assert!(all_b.contains("工人报工（报工批次数）"), "{all_b}");
        assert!(all_b.contains("> 2 甲 ██"), "{all_b}");
        assert!(all_b.contains("> 9 乙 ████████████"), "{all_b}");
    }

    /// ★ 守卫 ⑫：脚注文案能分辨「管理员设的上限」与「字节截断」。
    #[test]
    fn footer_distinguishes_config_cap_from_byte_truncation() {
        let rows = rows_with_workers(20);
        let cfg = cfg_with(15, 20);
        let out = render_ok(&cfg, &rows);
        let all = out.chunks.join("\n");
        assert!(
            all.contains("另有 5 人在此省略（配置「最多列出工人」= 15）"),
            "脚注要点名是配置上限：{all}"
        );
        assert!(!all.contains("截断"), "本域没有字节截断这件事：{all}");
        assert_eq!(
            out.worker_row_total, 20,
            "worker_row_total 是挡人前的真实行数"
        );
    }

    /// ★ 守卫 ⑬：`worker_top_n == 0`（不限）⇒ 脚注不出现。
    #[test]
    fn footer_absent_when_worker_top_n_is_zero() {
        let rows = rows_with_workers(20);
        let out = render_ok(&cfg_with(0, 20), &rows);
        let all = out.chunks.join("\n");
        assert!(!all.contains("在此省略"), "不限人数时不该有省略脚注：{all}");
    }

    /// ★ 守卫 ⑭：本域是 markdown 不是 HTML ⇒ **不做任何转义**，
    /// 工人名里的 `<` / `>` 原样透传，而 `<font color=…>` 自身成对。
    #[test]
    fn markdown_special_chars_are_not_swallowed() {
        let rows = ReportRows {
            note_count: 1,
            overdue_count: 1,
            worker_rows: vec![WorkerBarRow {
                worker_name: "A<B>C".into(),
                badge_code: "W1".into(),
                batch_count: 1,
                quantity: 1,
            }],
            ..ReportRows::default()
        };
        let out = render_ok(&cfg_with(0, 20), &rows);
        let all = out.chunks.join("\n");
        assert!(all.contains("A<B>C"), "工人名里的尖括号必须原样透传：{all}");
        // `<font …>` / `</font>` 必须成对（title 区 + 概览 + 出货 + 交期各若干）。
        assert_eq!(
            all.matches("<font").count(),
            all.matches("</font>").count(),
            "font 标签不成对：{all}"
        );
        assert!(all.matches("<font").count() > 0);
    }

    /// ★ 守卫 ⑮：**「（i/N）」后缀的字节数恒 ≤ RESERVE**（pass 1 为它预留的预算）。
    ///
    /// 这是 `RESERVE` 存在的**直接**理由，且它把「预留够不够」变成一条可测的确定事实
    /// 而不是「刚好没撞上」。⚠️ 变异验证：`RESERVE` 改成 0 时本条立刻红
    /// （最坏后缀 `（20/20）` 是 11 字节 > 0）。
    #[test]
    fn page_suffix_bytes_never_exceed_reserve() {
        const BASE: &str = "# 生产日报 · 2026-10-18";
        for (i, n) in [(1usize, 1usize), (1, 2), (9, 20), (20, 20)] {
            let grown = with_page_suffix(BASE, i, n).len() - BASE.len();
            assert!(
                grown <= RESERVE,
                "第 {i}/{n} 片的后缀涨了 {grown} 字节 > RESERVE {RESERVE}"
            );
        }
        // 后缀只落在**首行**：多行标题块的后两行必须逐字不变。
        let multi = with_page_suffix("# 标题\n\n> 统计区间 ……", 2, 5);
        assert!(multi.starts_with("# 标题（2/5）"), "{multi}");
        assert!(multi.ends_with("> 统计区间 ……"), "{multi}");
        const { assert!(RESERVE > 0, "RESERVE 必须为正，否则 pass 2 没有任何预留") };
    }

    /// 标题块**每片都重复**（否则第 2 片在群里没有日期上下文）。
    #[test]
    fn title_block_is_repeated_in_every_chunk() {
        let rows = rows_with_workers(200);
        let out = render_ok(&cfg_with(0, 20), &rows);
        assert!(out.chunks.len() >= 2);
        for (i, c) in out.chunks.iter().enumerate() {
            let first = lines_of(c)[0];
            assert!(
                first.contains("# 生产日报 · 2026-10-18"),
                "第 {} 片首行应是带页码的标题：{first}",
                i + 1
            );
            assert!(
                first.contains(&format!("（{}/{}）", i + 1, out.chunks.len())),
                "第 {} 片首行缺页码：{first}",
                i + 1
            );
            assert!(c.contains("统计区间"), "统计区间行也每片重复：{c}");
        }
    }

    /// 标题块**超长**时同样进单块超限守卫（`subject_template` 很长 ⇒ 它进 Title 块）。
    #[test]
    fn long_subject_template_goes_through_the_oversize_guard() {
        let rows = rows_with_workers(1);
        let cfg = NormalizedConfig {
            subject_template: Some("x".repeat(CHUNK_BYTE_LIMIT)),
            ..cfg_with(0, 20)
        };
        let err = render(&RenderInput {
            cfg: &cfg,
            rows: &rows,
            today: today(),
            window: window(),
        })
        .expect_err("超长标题必须报错");
        assert!(err.to_string().contains("标题"));
    }

    /// `display_width` 的基线（CJK 2 列、ASCII 1 列）。
    #[test]
    fn display_width_counts_cjk_as_two() {
        assert_eq!(display_width("张三"), 4);
        assert_eq!(display_width("李"), 2);
        assert_eq!(display_width("ab"), 2);
        assert_eq!(display_width("张a"), 3);
    }

    /// `bar_cells` 的取整边界（0 / 1 / max_v / max_v=0）。
    #[test]
    fn bar_cells_rounding_edges() {
        assert_eq!(bar_cells(0, 0), "░".repeat(12));
        assert_eq!(bar_cells(5, 0), "░".repeat(12), "全 0 时不能除零");
        assert_eq!(bar_cells(10, 10), "█".repeat(12));
        assert_eq!(bar_cells(5, 10), "█".repeat(6) + &"░".repeat(6));
        assert_eq!(
            bar_cells(1, 10),
            "█".to_string() + &"░".repeat(11),
            "round(1.2)=1"
        );
        assert_eq!(bar_cells(0, 10), "░".repeat(12));
        assert_eq!(bar_cells(99, 10), "█".repeat(12), "上界夹逼");
    }

    /// 四个节开关的 slug 与 `PreviewOut.sections[]` 一致。
    #[test]
    fn sections_reflect_enabled_flags() {
        let rows = rows_with_workers(1);
        let out = render_ok(&cfg_with(0, 20), &rows);
        assert_eq!(
            out.sections,
            vec!["overview", "delivery", "orders", "workers"],
            "缺省四节全开"
        );
        let cfg = NormalizedConfig {
            include_delivery: false,
            include_workers: false,
            ..cfg_with(0, 20)
        };
        let out = render_ok(&cfg, &rows);
        assert_eq!(out.sections, vec!["overview", "orders"]);
    }

    /// 空 `subject_template`（空串 / 纯空白）⇒ 走域内默认串。
    #[test]
    fn blank_subject_template_falls_back_to_default() {
        let rows = rows_with_workers(1);
        for blank in ["", "   "] {
            let cfg = NormalizedConfig {
                subject_template: Some(blank.to_string()),
                ..cfg_with(0, 20)
            };
            let out = render_ok(&cfg, &rows);
            assert_eq!(out.subject, "# 生产日报 · 2026-10-18");
        }
    }

    /// 没有工人行时**只出小标题、不出空态行**。
    #[test]
    fn no_worker_rows_yields_heading_only() {
        let out = render_ok(&cfg_with(0, 20), &ReportRows::default());
        let all = out.chunks.join("\n");
        assert!(all.contains("## 四、工人报工（送件数）"));
        assert!(!all.contains("░░░"), "没有工人就不该有空条：{all}");
        assert_eq!(out.worker_row_total, 0);
    }
    // ========================================================================
    // 第三节：交期清单（缩进 / 缺省字段 / 截断 / 拆片）
    // ========================================================================

    /// ★ 守卫 ⑱：第三节**逐字行形态** = `序列号 [名称](直链) ×数量  位置`。
    ///
    /// ⚠️ 这条守卫逐字钉住规格给的四个字段与它们的分隔符：序列号后**一个**空格、
    /// 名称的方括号、`×数量`、位置前**两个**空格。分隔符数量变了不会报错、只会让人
    /// 在群里读不出哪一列是哪一列。
    #[test]
    fn order_line_shape_is_verbatim() {
        let mut r = order_row("F1001", "名称甲", 2);
        r.link = Some("https://cos.hsh-erp.cloud/previews/seed01/p1.jpg".to_string());
        r.location = Some("工人:张三".to_string());
        assert_eq!(
            order_line(&r),
            "> F1001 [名称甲](https://cos.hsh-erp.cloud/previews/seed01/p1.jpg) ×2  工人:张三"
        );

        // 无直链 ⇒ 名称是**纯文字**（不留空的 `[]()`，那会渲成一个点不动的链接）。
        let mut plain = order_row("F1002", "名称乙", 1);
        plain.location = Some("生产架:A-03".to_string());
        assert_eq!(order_line(&plain), "> F1002 名称乙 ×1  生产架:A-03");
    }

    /// ★ 守卫 ⑲：子件行带 `└ ` 缩进前缀，且**层级只靠 `└`**（不靠行首空格）。
    #[test]
    fn child_rows_render_with_the_tree_prefix() {
        let mut r = child_row("F1003", "子件丙", 4);
        r.location = Some("品检架:B-11".to_string());
        assert_eq!(order_line(&r), ">   └ F1003 子件丙 ×4  品检架:B-11");
        // 父行与子件行的前缀逐字不同（否则层级在渲染层就已经丢了）。
        assert!(order_line(&order_row("F2000", "装配件乙", 1)).starts_with(ORDER_PREFIX));
        assert!(order_line(&r).starts_with(CHILD_PREFIX));
    }

    /// ★ 守卫 ⑳：**序列号为空则留空**（不回退图号、不显示「无」），且**不留悬空
    /// 空格**。
    ///
    /// ⚠️ 变异验证：把 `None` 分支写成 `format!("{prefix} {name}")` 时本条立刻红 ——
    /// 悬空空格在 `markdown` v1 里会被当缩进吃掉，同一类行的行首宽度从此不可预测。
    #[test]
    fn missing_serial_number_renders_blank_without_a_dangling_space() {
        let mut r = order_row("F1001", "名称甲", 2);
        r.serial_no = None;
        r.location = Some("工人:张三".to_string());
        assert_eq!(order_line(&r), "> 名称甲 ×2  工人:张三");
        assert!(!order_line(&r).contains("无"), "不许用「无」之类的占位字");
        assert!(!order_line(&r).starts_with(">  "), "行首不许留双空格");

        // 子件行同理：前缀之后直接是名称。
        let c = OrderRow {
            serial_no: None,
            ..child_row("F1003", "子件丙", 4)
        };
        assert_eq!(order_line(&c), ">   └ 子件丙 ×4");
    }

    /// ★ 守卫 ㉑：**位置拿不到就整项不显示**（连分隔空格都不留）。
    ///
    /// 四种「拿不到」在 service 层归一成同一个 `None`（`OFFICE` / 无活跃批次 /
    /// holder 行已软删），渲染层只认「有值就显示、没有就什么都没有」。
    #[test]
    fn missing_location_drops_the_whole_item() {
        let mut r = order_row("F1001", "名称甲", 2);
        assert_eq!(order_line(&r), "> F1001 名称甲 ×2");
        assert!(!order_line(&r).ends_with(' '), "行尾不许留悬空空格");
        r.location = Some(String::new());
        assert_eq!(
            order_line(&r),
            "> F1001 名称甲 ×2  ",
            "空串仍按「有值」处理（防御）"
        );
    }

    /// ★ 守卫 ㉒：组标题是 `**逾期未交 · N 条**` / `**2026-10-15 · N 条**`，且小标题
    /// 与**第一组**的组标题 + 首行在同一个块里（不出现「只有标题没有内容」的片）。
    #[test]
    fn order_groups_render_heading_and_sticky_first_row() {
        let rows = rows_with_workers(1);
        let out = render_ok(&cfg_with(0, 20), &rows);
        let all = out.chunks.join("\n");
        assert!(all.contains("## 三、逾期未交 / 3 日内交期"), "{all}");
        assert!(all.contains("> **逾期未交 · 12 条**"), "{all}");
        assert!(
            all.contains(&format!("> **{} · 7 条**", today().format("%Y-%m-%d"))),
            "{all}"
        );
        // 小标题与第一组的组标题 + 首行同块（顺序相邻、中间无别的块）。
        assert!(
            all.contains(
                "## 三、逾期未交 / 3 日内交期\n> **逾期未交 · 12 条**\n> F1001 名称甲 ×2  工人:张三"
            ),
            "{all}"
        );
    }

    /// ★ 守卫 ㉓：**清单为空时只出小标题**，不出组标题、不出空态行。
    ///
    /// 与第四节「没有工人行就只出小标题」同款：`PreviewOut.sections[]` 已经告诉
    /// 管理员这一节是开着的，正文再补一行「无」只是往好消息上刷噪音。
    #[test]
    fn empty_order_list_renders_heading_only() {
        let rows = ReportRows::default();
        let out = render_ok(&cfg_with(0, 20), &rows);
        let all = out.chunks.join("\n");
        assert!(all.contains("## 三、逾期未交 / 3 日内交期"), "{all}");
        assert!(!all.contains("逾期未交 ·"), "空清单不出组标题：{all}");
        assert!(!all.contains("另有"), "空清单不出截断脚注：{all}");
    }

    /// ★ 守卫 ㉔：`include_orders == false` ⇒ 整节**完全不出现**。
    #[test]
    fn closed_orders_section_is_absent_even_with_rows() {
        let mut rows = rows_with_workers(1);
        rows.order_groups = order_groups_fixture();
        let cfg = NormalizedConfig {
            include_orders: false,
            ..cfg_with(0, 20)
        };
        let all = render_ok(&cfg, &rows).chunks.join("\n");
        assert!(!all.contains("三、逾期未交"), "{all}");
        assert!(!all.contains("名称甲"), "{all}");
    }

    /// ★ 守卫 ㉕：截断脚注**只在真的挡掉了行时**出现，文案逐字是「另有 N 条未列出」。
    #[test]
    fn truncation_footer_names_the_withheld_row_count() {
        let mut rows = rows_with_workers(1);
        rows.order_groups = vec![group(
            OrderGroupKind::Overdue,
            (0..5)
                .map(|i| order_row(&format!("F{i}"), &format!("名称{i}"), 1))
                .collect(),
            5,
        )];
        // omitted=3 是 service 层算好的（额度 2 / 共 5 行）。
        rows.order_groups[0].omitted = 3;
        let all = render_ok(&cfg_with(0, 20), &rows).chunks.join("\n");
        assert!(
            all.contains("> <font color=\"comment\">另有 3 条未列出</font>"),
            "{all}"
        );
        // 脚注在组末（该组最后一行之后）。
        let at_footer = all.find("另有 3 条未列出").expect("脚注存在");
        let at_last_row = all.find("F4").expect("组内最后一行存在");
        assert!(at_last_row < at_footer, "脚注必须落在组末：{all}");

        // omitted = 0 ⇒ 不出现脚注。
        rows.order_groups[0].omitted = 0;
        let all = render_ok(&cfg_with(0, 20), &rows).chunks.join("\n");
        assert!(!all.contains("另有"), "没截断就不该有脚注：{all}");
    }

    /// ★ 守卫 ㉖：**清单块逐行可拆** —— 一套装配件带 30 个子件会逼出多片，但每一行
    /// 都**完整**（链接、序列号、数量、位置都不被劈开），且 31 行一个不少。
    ///
    /// ⚠️ 每行都挂上 ≈95 字节的图纸直链（Part 2b 接上后的真实形态）才够把 31 行撑到
    /// 4096 字节之外 —— 只用短名时 31 行装得进一片，这条守卫就测不到「可拆」了。
    /// ⚠️ 变异验证：把清单块改成「一个装配件一个块、不可拆」时本条立刻红（那条路下
    /// 31 行会变成 ≈4.6 KB 的单块，直接撞单块超限 `21804`）。
    #[test]
    fn order_rows_split_across_chunks_without_breaking_a_line() {
        let url = |seed: &str| {
            Some(format!(
                "https://cos.hsh-erp.cloud/previews/{seed}/p1.jpg?eo-img.resize=l/2560"
            ))
        };
        let mut parent = order_row("F2000", "装配件乙", 1);
        parent.link = url("ba6ad9c78280e31bc23ef746e78e27d8");
        parent.location = Some("生产架:A-03".to_string());
        let mut rows = vec![parent];
        for i in 0..30 {
            let mut c = child_row(&format!("F3{i:03}"), &format!("子件{i:02}"), i + 1);
            c.link = url(&format!("seed{i:02}0000000000000000000000000"));
            c.location = Some("工人:张三".to_string());
            rows.push(c);
        }
        // 期望行 = `order_line` 逐字渲染的结果（一条完整行就是一个整体）。
        let expected: Vec<String> = rows.iter().map(order_line).collect();
        assert!(
            expected.iter().map(|s| s.len()).sum::<usize>() > CHUNK_BYTE_LIMIT,
            "fixture 必须真的超过单片预算，否则测不到拆片"
        );

        let report = ReportRows {
            order_groups: vec![group(OrderGroupKind::Overdue, rows, 31)],
            ..ReportRows::default()
        };
        // 只留第三节：其余三节的正文行也是 `> ` 开头，混进来会让「逐行比对」失焦。
        let cfg = NormalizedConfig {
            include_overview: false,
            include_delivery: false,
            include_workers: false,
            ..cfg_with(0, 20)
        };
        let out = render_ok(&cfg, &report);
        assert!(
            out.chunks.len() >= 2,
            "31 行长名行必然多片，实际 {}",
            out.chunks.len()
        );

        let mut seen: Vec<String> = Vec::new();
        for (i, c) in out.chunks.iter().enumerate() {
            assert!(c.len() <= CHUNK_BYTE_LIMIT, "第 {} 片超 4096 字节", i + 1);
            for line in c.lines() {
                // 标题块第二行（统计区间）与组标题（`**…**`）也是 `> ` 开头，都不是
                // 清单行 —— 拿它们去比对期望集合只会得到一条看不懂的失败信息。
                if line.starts_with("> 统计区间")
                    || line.starts_with("> **")
                    || (!line.starts_with(CHILD_PREFIX) && !line.starts_with(ORDER_PREFIX))
                {
                    continue;
                }
                assert!(
                    expected.iter().any(|e| e == line),
                    "第 {} 片里出现了被劈开 / 不存在的行：{line}",
                    i + 1
                );
                seen.push(line.to_string());
            }
        }
        assert_eq!(seen.len(), 31, "31 行一个不少、不重");
        assert_eq!(seen, expected, "顺序逐字不变（子件仍紧跟其父行）");
    }

    /// ★ 守卫 ㉗：子件行**紧跟**它的装配件行（组内顺序不被分片打乱）。
    #[test]
    fn child_rows_stay_directly_after_their_parent_row() {
        let rows = ReportRows {
            order_groups: vec![group(
                OrderGroupKind::Overdue,
                vec![
                    order_row("F2000", "装配件乙", 1),
                    child_row("F3001", "子件一", 2),
                    child_row("F3002", "子件二", 3),
                    order_row("F1001", "散件甲", 4),
                    child_row("F3003", "孤儿子件", 5),
                ],
                3,
            )],
            ..ReportRows::default()
        };
        let all = render_ok(&cfg_with(0, 20), &rows).chunks.join("\n");
        let order: Vec<usize> = ["装配件乙", "子件一", "子件二", "散件甲", "孤儿子件"]
            .iter()
            .map(|needle| all.find(needle).unwrap_or_else(|| panic!("{needle} 缺失")))
            .collect();
        assert!(
            order.windows(2).all(|w| w[0] < w[1]),
            "组内行序必须与入参一致：{all}"
        );
    }
}

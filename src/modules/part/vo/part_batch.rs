//! part 域 batch 详情 / 列表 / 子域相关出参 VO（2026-09-22 PR4 重构）

use serde::Serialize;

use crate::modules::part::service::crud::TPartScanRow;
use crate::modules::prod::batch::model::PartBatchScanRow;
use crate::shared::types::{serialize_i64, serialize_i64_opt};

/// `POST /parts/batch` per-item 失败明细。
///
/// `part_id`：`Some(id)` = INSERT 成功但 detail lookup 失败；
///            `None` = INSERT 本身失败。
#[derive(Debug, Clone, Serialize)]
pub struct PartBatchCreateFailure {
    #[serde(serialize_with = "serialize_i64_opt")]
    pub part_id: Option<i64>,
    pub code: i32,
    pub message: String,
    pub item_index: usize,
}

/// `POST /parts/batch` 出参：`created` 与 `failed` 互斥。
///
/// 2026-09-16 M2-B review 第 1 轮：`cleanup_tmp_keys` 新增字段。
/// - 含义：本批次成功 INSERT 后、需要 commit 后异步清理的 tmp 对象 key 列表
///   （client 已直传到 COS tmp 区，已被 service 端 head+copy 到 CAS key）。
/// - 用途：handler 在 `tx.commit()` 之后 `tokio::spawn` 批量 `cos.delete_object(&key)`
///   兜底，避免 commit 失败却已触发 COS 删除产生孤儿。
/// - 前端不需要该字段（`#[serde(default)]` 兜空，前端忽略）；后端用 `out.cleanup_tmp_keys`。
/// - legacy（无 binding）路径该列表为空，前端 / 集成测试无需关注。
#[derive(Debug, Clone, Serialize)]
pub struct PartBatchCreateOut {
    pub created: Vec<crate::modules::part::vo::PartDetailOut>,
    pub failed: Vec<PartBatchCreateFailure>,
    /// commit 后由 handler spawn 异步清理的 tmp 对象 key 列表。
    #[serde(default)]
    pub cleanup_tmp_keys: Vec<String>,
}

/// `GET /parts/by-serial/{serial_no}/part-batches` 出参：工单窄字段。
/// 字段严格来自 `t_part`（仅 8 列 + id），不复用 `PartDetailOut` 的 28 列 flatten。
#[derive(Debug, Clone, Serialize)]
pub struct PartScanInfoOut {
    #[serde(serialize_with = "serialize_i64")]
    pub id: i64,
    pub drawing_no: String, // b 图号
    pub name: String,       // 名称
    pub quantity: i32,      // 数量
    #[serde(serialize_with = "serialize_i64")]
    pub customer_id: i64, // 客户（仅 FK，不冗余 customer_name）
    pub system_delivery_date: Option<chrono::NaiveDate>, // 系统交期
    pub is_urgent: bool,    // 是否加急
    pub order_no: Option<String>, // 订单号
    pub note: Option<String>, // 备注
}

/// `GET /parts/by-serial/{serial_no}/part-batches` 出参：单批次窄字段。
/// `holder_name` 由 service 层经 repo `list_active_by_part_id_with_holder` 解析。
#[derive(Debug, Clone, Serialize)]
pub struct PartBatchScanOut {
    #[serde(serialize_with = "serialize_i64")]
    pub id: i64,
    pub quantity: i32,
    pub status: String,              // PartBatchStatus 字符串形态
    pub holder_name: Option<String>, // 当前持有人/货架名称（解析自 t_shelf/t_user/t_worker）
    pub version: i32,                // 乐观锁版本号（前端 to-ship 用）
}

/// Scan context 完整出参：工单 + 全部未删批次（按 batch_no 升序）。
#[derive(Debug, Clone, Serialize)]
pub struct PartScanContextOut {
    pub part: PartScanInfoOut,
    pub batches: Vec<PartBatchScanOut>,
}

/// `PartScanInfoOut::from(TPartScanRow)`：service 内私有窄字段 FromRow
/// (`src/modules/part/service/crud.rs::TPartScanRow`) → DTO 字段对拷。
impl From<TPartScanRow> for PartScanInfoOut {
    fn from(p: TPartScanRow) -> Self {
        Self {
            id: p.id,
            drawing_no: p.drawing_no,
            name: p.name,
            quantity: p.quantity,
            customer_id: p.customer_id,
            system_delivery_date: p.system_delivery_date,
            is_urgent: p.is_urgent,
            order_no: p.order_no,
            note: p.note,
        }
    }
}

/// `PartBatchScanOut::from(PartBatchScanRow)`：repo 解析出的批次窄字段 → DTO。
impl From<PartBatchScanRow> for PartBatchScanOut {
    fn from(p: PartBatchScanRow) -> Self {
        Self {
            id: p.id,
            quantity: p.quantity,
            status: p.status,
            holder_name: p.holder_name,
            version: p.version,
        }
    }
}

/// `POST /parts/match-by-excel-items` 出参：单行匹配结果（**恒存在**，未匹配行也返回）。
///
/// 2026-10-06 重做。旧结构（`{drawing_no, serial_no, part_id, status, message}`）
/// 与前端已实现的读取方式不匹配：前端按 `row_no` 关联并读 `parts` 数组，而旧
/// 结构两者都没有 ⇒ `new Map(results.map(r => [r.row_no, r]))` 的 key 全是
/// `undefined` ⇒ 每一行都判「未匹配」。
///
/// **数组长度恒等于请求 `items` 长度、顺序一致**（前端按 `row_no` 关联，
/// 顺序仅供人读）；未匹配的行也必须出现（`match_type = NONE` + `parts: []`）。
#[derive(Debug, Clone, Serialize)]
pub struct MatchByExcelItemResult {
    /// 关联键，回显请求的 `row_no`。
    pub row_no: i32,
    /// 命中的判据档位（`NONE` = 四档全落空）。
    pub match_type: ExcelMatchType,
    /// 候选零件（含现值，供前端做「原值 vs 新值」对比与默认勾选）。
    pub parts: Vec<PartMatchInfoOut>,
    /// 档位异常提示（无异常时是空数组，不是 null）。
    pub warnings: Vec<String>,
}

/// Excel 行 → 候选零件的**匹配档位**（闭合 5 值）。
///
/// 2026-10-06 新增。序列化形态是大写字符串，与本仓「无 DB ENUM、Rust enum 校验」
/// 的约定一致 —— 范本 `vo::part::ChainState`。
///
/// 2026-10-06 review 第 1 轮：`rename_all` 由 `UPPERCASE` 改为
/// `SCREAMING_SNAKE_CASE`。`UPPERCASE` 只把字母变大写、**不插下划线**，
/// `PartCode` 经它是 `PARTCODE`（少一个下划线）；`SCREAMING_SNAKE_CASE` 直接产出
/// `PART_CODE`，于是**不需要**变体级 `#[serde(rename)]`，线格式只有一个真源。
/// （前一版是「`rename_all = UPPERCASE` + 5 个显式 rename」：两者并存时显式 rename
/// 恒胜、`rename_all` 零作用，读者却会以为 `PARTCODE` 是当前行为 —— 误导性残留。）
/// 5 个变体名都是单词或双词，在这条规则下全部正确产出线格式。
///
/// 优先级是 `PART_CODE > ASSEMBLY_CODE > PART_NAME > ASSEMBLY_NAME > NONE`
/// （单行单档位、先命中先占）：能按图号唯一定位就不该退到名称去冒险匹配。
///
/// 只 `Serialize`：本枚举只出现在**响应**侧（`MatchByExcelItemResult`），全仓
/// 没有它的反序列化入口（前端读 `match_type` 用的是 TS 字面量联合，不是本枚举的
/// 往返）。派生一个永不使用的 `Deserialize` 会让人误以为存在入参路径。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ExcelMatchType {
    /// 按图号命中 `t_part.drawing_no`（候选是零件本身）。
    PartCode,
    /// 按图号命中 `t_assembly.drawing_no`（候选是该装配件的**有效子件**）。
    AssemblyCode,
    /// 按名称命中 `t_part.name`（名称兜底，跨客户同名极容易超限）。
    PartName,
    /// 按名称命中 `t_assembly.name`（名称兜底，候选是该装配件的有效子件）。
    AssemblyName,
    /// 四档判据均不存在或全部落空。
    None,
}

impl ExcelMatchType {
    /// 候选 cap 提示里用的中文判据名（`warnings` 文案用）。
    pub fn label_zh(self) -> &'static str {
        match self {
            Self::PartCode => "按图号匹配",
            Self::AssemblyCode => "按装配件图号匹配",
            Self::PartName => "按名称匹配",
            Self::AssemblyName => "按装配件名称匹配",
            Self::None => "未匹配",
        }
    }
}

/// `POST /parts/match-by-excel-items` 出参里的单个候选零件。
///
/// 2026-10-06 新增。字段集是前端「现值对比 + 默认勾选 + 提交 OCC」的最小全集：
/// - `version`：**必须**回传，它是前端提交时的 OCC 依据；缺了整笔更新请求会因
///   后端 `version: i32` 无 `#[serde(default)]` 而 400。
/// - `order_no` / `system_delivery_date` / `assembly_name`：前端 `isEmptyTarget`
///   默认勾选判据与「原值 vs 新值」对比的数据源。
/// - `drawing_no` / `name`：展示用；`t_part.drawing_no` 是 NOT NULL 列，故类型是
///   `String` 而非 `Option`。
#[derive(Debug, Clone, Serialize)]
pub struct PartMatchInfoOut {
    #[serde(serialize_with = "serialize_i64")]
    pub part_id: i64,
    pub version: i32,
    pub drawing_no: String,
    pub name: String,
    pub order_no: Option<String>,
    pub system_delivery_date: Option<chrono::NaiveDate>,
    #[serde(serialize_with = "serialize_i64_opt")]
    pub assembly_id: Option<i64>,
    pub assembly_name: Option<String>,
}

/// `POST /parts/batch-update-order-info` 出参：成功 N、跳过 N、失败列表。
///
/// 2026-10-06：`updated: i64` 改名为 `updated_count: i64`（前端只读这一个数字，
/// 显式命名为 count 后不必再靠字段名猜语义）。**永远 HTTP 200 + 信封**，
/// 全部失败也不抛业务错误（前端依赖部分成功语义）。
#[derive(Debug, Clone, Serialize)]
pub struct BatchUpdateOrderInfoOut {
    pub updated_count: i64,
    pub failed: Vec<BatchUpdateOrderInfoFailure>,
    /// 2026-10-06 新增：请求里 `skip = true` 的行数（未写库）。
    pub skipped_count: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct BatchUpdateOrderInfoFailure {
    #[serde(serialize_with = "serialize_i64")]
    pub part_id: i64,
    pub code: i32,
    /// 2026-10-06：**不得**含 sqlx 原始错误文本（旧实现 `format!("{e}")` 把
    /// sqlx 内部错误泄进 200 响应体，与 `AppError::into_response` 已把
    /// `Database` 的 message 换成字面量「数据库错误」的口径不一致）。
    /// 细节走 `tracing::warn!` 落服务端日志。
    pub message: String,
}

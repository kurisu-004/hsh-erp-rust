//! part 域 CRUD + part 级 lifecycle DTO（Phase PR-CRUD 2026-08-25）
//!
//! 2026-10-02 批次路由迁 prod 域后，本文件**只剩 part 级 / 多批次级**入参：
//! `cancel`（BATCH-N 翻转该 part 全部活跃批次）、`force-complete`（BATCH-N）、
//! `soft-delete`、以及全部 list / 批量创建 / 文件工具入参。17 个**以单个批次为
//! 操作对象**的流转入参（`deliver` / `complete` / `place-on-shelf` / `to-ship` …）
//! 已迁到 `crate::modules::prod::batch::dto`。
//!
//! 命名约定：
//! - `CreateXxxRequest` / `UpdateXxxRequest`：写操作入参
//! - `XxxListQuery`：列表查询参数
//!
//! 出参（*Out 类型）已迁移到 `super::vo`（2026-09-22 PR4 重构）：DTO 仅含
//! axum extractor 反序列化目标（`#[derive(Deserialize)]`），VO 仅含 handler
//! 返回序列化目标（`#[derive(Serialize)]`），二者不再同文件。

use rust_decimal::Decimal;
use serde::Deserialize;

use crate::shared::types::{deserialize_i64, deserialize_i64_opt};

// ===== Create =====

/// `POST /parts` 入参：单件创建工单。
///
/// 2026-10-05 新增 `unit_price` / `total_price`：此前建单 DTO 不收金额，
/// INSERT 也不带这两列，PDF 批量上传解析出的单价被整条链丢弃（DB 默认 0）。
/// **JSON 里必须是字符串**（`"95.00"`），不能写裸数字 `95` —— crate 的
/// `rust_decimal` 只开了 `serde-with-str`，`Decimal` 的 Deserialize 实现
/// 唯一入口是字符串，写数字会在反序列化阶段直接 400。
#[derive(Debug, Clone, Deserialize)]
pub struct PartCreateRequest {
    pub name: String,
    pub drawing_no: String,
    pub applicant_name: String,
    pub quantity: i32,
    pub request_date: chrono::NaiveDate,
    pub planned_delivery_date: chrono::NaiveDate,
    #[serde(default)]
    pub is_urgent: bool,
    #[serde(deserialize_with = "deserialize_i64")]
    pub customer_id: i64,
    #[serde(default, deserialize_with = "deserialize_i64_opt")]
    pub assembly_id: Option<i64>,
    #[serde(default)]
    pub order_no: Option<String>,
    #[serde(default)]
    pub system_delivery_date: Option<chrono::NaiveDate>,
    #[serde(default)]
    pub note: Option<String>,
    /// 2026-10-05 新增：单价（JSON 字符串，如 `"95.00"`；缺省 = 0）
    #[serde(default)]
    pub unit_price: Option<Decimal>,
    /// 2026-10-05 新增：总价（JSON 字符串，如 `"950.00"`；缺省 = 0）
    #[serde(default)]
    pub total_price: Option<Decimal>,
}

// ===== Batch create =====

/// `POST /parts/batch` 单 item 入参：与 `PartCreateRequest` 字段集对齐，
/// 但 `customer_id` 提到 batch 级别（共享）。
///
/// 2026-09-16 M2-B 新增可选字段：
/// - `drawing_file`：上传图纸 PDF（kind=DRAWING）绑定
/// - `model3d_file`：上传 3D 模型（kind=3D_MODEL）绑定
///
/// 两个字段都形如 [`FileBindingIn`]，由前端从 `POST /part-files/upload-intents`
/// 拿到 `tmp_key` 后填回。`batch_create_parts` service 会先并发 head/copy 所有
/// binding 项，**任一失败** → 整体回滚（让用户重试整批）。
///
/// 2026-10-05 新增 `unit_price` / `total_price`：PDF 批量上传的解析结果里有单价，
/// 但本 item DTO 不收、`NewPartCreate` 也不带，金额在整条链上被丢弃。与
/// [`PartCreateRequest`] 同约束：**JSON 里必须是字符串**（`"95.00"`），裸数字 95
/// 会在反序列化阶段直接 400（crate 的 `rust_decimal` 只开了 `serde-with-str`）。
#[derive(Debug, Clone, Deserialize)]
pub struct PartBatchCreateItem {
    pub name: String,
    pub drawing_no: String,
    pub applicant_name: String,
    pub quantity: i32,
    pub request_date: chrono::NaiveDate,
    pub planned_delivery_date: chrono::NaiveDate,
    #[serde(default)]
    pub is_urgent: bool,
    #[serde(default)]
    pub order_no: Option<String>,
    #[serde(default)]
    pub system_delivery_date: Option<chrono::NaiveDate>,
    #[serde(default)]
    pub note: Option<String>,
    #[serde(default, deserialize_with = "deserialize_i64_opt")]
    pub assembly_id: Option<i64>,
    /// 2026-10-05 新增：单价（JSON 字符串，如 `"95.00"`；缺省 = 0）
    #[serde(default)]
    pub unit_price: Option<Decimal>,
    /// 2026-10-05 新增：总价（JSON 字符串，如 `"950.00"`；缺省 = 0）
    #[serde(default)]
    pub total_price: Option<Decimal>,
    /// 2026-09-16 M2-B 新增：上传图纸 PDF 绑定（kind=DRAWING，file_type=PDF）
    #[serde(default)]
    pub drawing_file: Option<FileBindingIn>,
    /// 2026-09-16 M2-B 新增：上传 3D 模型绑定（kind=3D_MODEL，file_type 由扩展名推导）
    #[serde(default)]
    pub model3d_file: Option<FileBindingIn>,
}

/// `POST /parts/batch` 单个文件绑定子结构（drawing_file / model3d_file）。
///
/// 由前端在拿到 `POST /part-files/upload-intents` 返回的 `tmp_key` 后填回。
/// `content_sha256` 必须与 upload-intents 提交时一致（CAS 命中场景下
/// upload-intents 返回 `dedup_hit=true`，前端跳过上传，把 `existing_file` 拼
/// 回 PartFileOut，本字段为 None——即 `binding` 也为 None）。
///
/// 2026-09-16 M2-B 新增；2026-09-29 扁平化新增 `ext` 字段（client 声明）。
/// CAS key 模板五段→两段后，ext 需作为 file_type 推导源随 binding 上行，
/// 避免 service 端 `policy::ext_of` 在中文 / 多段扩展（.tar.gz）边界上与
/// client 不一致。`ext` 缺省为空（None），由 service 走兼容回退
/// `policy::ext_of(original_filename)`。
#[derive(Debug, Clone, Deserialize)]
pub struct FileBindingIn {
    pub tmp_key: String,
    pub content_sha256: String,
    pub original_filename: String,
    #[serde(deserialize_with = "deserialize_i64")]
    pub file_size: i64,
    pub content_type: String,
    /// 2026-09-29 新增：扩展名（小写、不含点）。缺省 None（service 端兼容回退）。
    #[serde(default)]
    pub ext: Option<String>,
}

/// `POST /parts/batch` 入参：批量创建（共享 customer_id）。
#[derive(Debug, Clone, Deserialize)]
pub struct PartBatchCreateRequest {
    #[serde(deserialize_with = "deserialize_i64")]
    pub customer_id: i64,
    pub items: Vec<PartBatchCreateItem>,
}

// ===== Update =====

/// `PUT /parts/{id}` 入参：字段可选 UPDATE。
///
/// `version` 必填（OCC）；其它字段未传 → DB 不动。
///
/// 2026-09-16 PR-2 瘦身（migration 027）：删 `actual_delivery_date` 入参
/// （t_part 列已删；实际交付日期由 t_part_event DELIVERED 事件派生，不接
/// 受手工改）。
///
/// 2026-09-27 part 域前后端字段对齐：增 `unit_price?` / `total_price?` 入参
/// （`Option<Decimal>`）。DB 列 `t_part.unit_price` / `t_part.total_price`
/// 为 NUMERIC(12,2) / NUMERIC(14,2) NOT NULL DEFAULT 0，由 rust_decimal
/// 反序列化为 string → Decimal 避免 JS 浮点丢精度。
#[derive(Debug, Clone, Deserialize)]
pub struct PartUpdateRequest {
    pub version: i32,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub drawing_no: Option<String>,
    #[serde(default)]
    pub applicant_name: Option<String>,
    #[serde(default)]
    pub quantity: Option<i32>,
    #[serde(default)]
    pub order_no: Option<String>,
    #[serde(default)]
    pub system_delivery_date: Option<chrono::NaiveDate>,
    #[serde(default)]
    pub planned_delivery_date: Option<chrono::NaiveDate>,
    #[serde(default)]
    pub note: Option<String>,
    #[serde(default)]
    pub is_urgent: Option<bool>,
    /// 2026-09-27 新增：单价（NUMERIC(12,2)）。`None` = DB 不动，`Some(v)` = 覆盖。
    /// `Decimal` 由 axum extractor 从 JSON string 反序列化得到。
    #[serde(default)]
    pub unit_price: Option<Decimal>,
    /// 2026-09-27 新增：总价（NUMERIC(14,2)）。`None` = DB 不动，`Some(v)` = 覆盖。
    #[serde(default)]
    pub total_price: Option<Decimal>,
}

// ===== List =====

/// `GET /parts` 查询参数（2026-09-29 简化：移除 `row_type` / `include_assemblies`）。
///
/// `customer_id`：单值；service 层用 `expand_customer_id` 展开为 L1+L2 ids。
/// `status` / `statuses`：单值 / 多值互不冲突；service 层二选一传入。
/// `locations` / `holder_ids`：逗号分隔。`locations` 是 `t_part_batch.location`
/// 字符串白名单（OFFICE / PRODUCTION_SHELF / WORKER / INSPECTION_SHELF /
/// OUTSOURCE_COMPANY）；`holder_ids` 是雪花 ID 字符串，service 层 deserialize 成
/// `Vec<i64>` 后查 `t_part_batch.current_holder_id`（多态：t_shelf /
/// t_worker / t_outsource_company 任一表匹配即命中）。
/// 两者均查 `t_part_batch`（真相源在 batch；t_part 已无 location /
/// current_holder_id 列），按 part 下任意 active batch 命中即返。
/// `sort_by` 白名单（CREATED_AT / UPDATED_AT / PLANNED_DELIVERY_DATE /
/// REQUEST_DATE / SERIAL_NO / DRAWING_NO / NAME），其它退化为 `CREATED_AT`。
///
/// 2026-09-29 简化：原 `row_type` / `include_assemblies` 三态合并矩阵已下沉
/// 到新端点 `GET /api/v2/com/union-list`（plan §1-3）。本端点（`GET /parts`）
/// 只查 `t_part WHERE assembly_id IS NULL`，不再承担 ALL/ASSEMBLY 合并。需
/// 跨行类型筛选的 caller 切换到 `/com/union-list?row_type=ALL|PART|PART_FLAT|ASSEMBLY`。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct PartListQuery {
    #[serde(default, deserialize_with = "deserialize_i64_opt")]
    pub customer_id: Option<i64>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub statuses: Option<String>, // 逗号分隔字符串（query string 不支持 Vec 友好）
    #[serde(default)]
    pub is_urgent: Option<bool>,
    #[serde(default)]
    pub keyword: Option<String>,
    /// 2026-09-17 PR-4 修复：locations 逗号分隔字符串（query string 不支持 Vec
    /// 友好，与 `statuses` 同形）。透传到 repo `PartListFilters.locations`，
    /// 查 `t_part_batch.location = ANY(...)`。
    #[serde(default)]
    pub locations: Option<String>,
    /// 2026-09-17 PR-4 修复：holder_ids 逗号分隔雪花 ID 字符串。service 层 parse
    /// 成 `Vec<i64>`，透传到 repo `PartListFilters.holder_ids`，查
    /// `t_part_batch.current_holder_id = ANY(...)`（多态 holder：t_shelf /
    /// t_worker / t_outsource_company 任一匹配即命中）。
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

// ===== Soft-delete =====

/// `POST /parts/{id}/soft-delete` 入参：`version` 必填（OCC）。
#[derive(Debug, Clone, Deserialize)]
pub struct PartSoftDeleteRequest {
    pub version: i32,
}

// ===== Lifecycle =====
/// `POST /parts/{id}/cancel` 入参。
///
/// cancel 保持 part 级（PR-B3 §4.3 D3）：级联取消全部活跃批次 → part 翻转
/// CANCELLED。`reason` 优先作为事件 note。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct CancelRequest {
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
}
/// `POST /parts/{id}/force-complete` 入参（2026-09-30 新增）。
///
/// MANAGER 单角色守卫；完全绕状态机把 part + 所有活跃批次强推到 COMPLETED。
/// 仅可填 `note`（会拼 `[FORCE]` 前缀写入事件日志追溯），不收 `batch_id` /
/// `version`（逃生通道不走 OCC，依赖 SQL 行锁串行化）。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ForceCompleteRequest {
    #[serde(default)]
    pub note: Option<String>,
}
/// `GET /parts/by-work-type/{work_type_id}` 入参（query）。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ByWorkTypeQuery {
    #[serde(default, deserialize_with = "deserialize_i64_opt")]
    pub shelf_id: Option<i64>,
    #[serde(default)]
    pub limit: Option<i64>,
    #[serde(default)]
    pub offset: Option<i64>,
}

/// `GET /parts/pickable-by-work-type/{work_type_id}` 入参（query）。
pub type PickableByWorkTypeQuery = ByWorkTypeQuery;

/// `GET /parts/by-worker/{worker_id}` 入参（query）。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ByWorkerQuery {
    #[serde(default)]
    pub limit: Option<i64>,
    #[serde(default)]
    pub offset: Option<i64>,
}

// ===== 文件 / Excel 工具 =====

/// `POST /parts/batch-with-pdfs` multipart 入参：JSON + PDFs。
///
/// 2026-10-05：本 DTO **不收** `unit_price` / `total_price`（该端点无前端调用方，
/// 金额字段的补齐只做在 `POST /parts` / `POST /parts/batch` 两个真实建单端点上）。
/// 序列号仍由 service 按 L1 客户 `serial_prefix` 派发（master 1 个 +
/// 子件 `{master}-{NN}`），与另两个端点同一套派发器。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct BatchWithPdfsRequest {
    #[serde(deserialize_with = "deserialize_i64")]
    pub customer_id: i64,
    #[serde(default)]
    pub applicant_name: Option<String>,
    #[serde(default)]
    pub request_date: Option<chrono::NaiveDate>,
    #[serde(default)]
    pub planned_delivery_date: Option<chrono::NaiveDate>,
    #[serde(default)]
    pub is_urgent: Option<bool>,
    #[serde(default)]
    pub note: Option<String>,
}

/// `POST /parts/match-by-excel-items` 入参：采购订单 Excel 明细行 → 候选零件。
///
/// 2026-10-06 重做：该端点的契约由前端「解析系统交期和订单号」对话框定义（本结构
/// 此前对着一份从未实现过的富契约，线上表现为「每一行都判未匹配、候选 0、
/// 提交按钮永久 disabled」）。要点：
///
/// - `row_no: i32` **必填**（`Option` 也不行）——它是响应的关联键，前端按它把
///   结果挂回 Excel 行。
/// - **不收** `delivery_date` / `unit_price` / `quantity`：前端 Excel 解析器
///   `parseDateOrNull` 对无法识别的日期文本**原样透传**，声明 `Option<NaiveDate>`
///   会让无法识别的文本把**整个请求**打成 400；且这 3 个字段后端完全用不到
///   （系统交期由前端本地预填进 date-picker）。serde 默认忽略未知字段，
///   前端继续发也无害。
/// - **删掉** `serial_no`：前端从不发，采购订单 Excel 也没有该列。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct MatchByExcelItemsRequest {
    pub items: Vec<MatchByExcelItem>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct MatchByExcelItem {
    /// Excel 行号（前端 1-based），响应按它回填。缺字段 ⇒ 整请求反序列化失败。
    pub row_no: i32,
    /// 订单行号，仅用于错误提示定位，不参与匹配。
    #[serde(default)]
    pub line_no: Option<String>,
    /// Excel 物料代码 → 对 `t_part.drawing_no` / `t_assembly.drawing_no` 做精确匹配。
    #[serde(default)]
    pub drawing_no: Option<String>,
    /// Excel 订单物料描述 → 对 `t_part.name` / `t_assembly.name` 做精确匹配。
    #[serde(default)]
    pub name: Option<String>,
}

/// `POST /parts/batch-update-order-info` 入参。
///
/// ⚠️ **`items` 数的是候选行，不是 Excel 行**（2026-10-06 review 第 3 轮 R3-6
/// 登记）：上限是 `service::phase1::events::BATCH_UPDATE_ORDER_INFO_MAX_ITEMS`
/// = 2000，而 match 端点的上限 `MATCH_MAX_ITEMS` = 2000 数的是 Excel 行、每行最多
/// 产出 20 个候选 ⇒ 一次合法 match 最多产出 40000 个候选行。因此存在硬崖：match
/// 端 200 行全命中、且候选多为「空目标」（前端 `isEmptyTarget` 默认勾选）时，提交
/// 4000+ 候选行会被**整单 422、一行不写**。
/// 用户可在确认框（已显示「将更新 N 个零件」）里手动取消勾选降到 2000 以下 ——
/// 谈不上死路，但这条崖此前没写进任何注释。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct BatchUpdateOrderInfoRequest {
    pub items: Vec<BatchUpdateOrderInfoItem>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct BatchUpdateOrderInfoItem {
    #[serde(deserialize_with = "deserialize_i64")]
    pub part_id: i64,
    /// 乐观锁期望版本（由 match 端点原样回传）。
    pub version: i32,
    /// 三态：缺省 = 不改该列；`null` = **清空成 NULL**；给值 = 写入。
    ///
    /// 2026-10-06 改用 `deserialize_some`：前端 date-picker 可清空，旧语义
    /// （单层 `Option`，`None` 一律「不改」）下用户清空系统交期会**静默无效**。
    /// 范本：assembly 域 `AssemblyUpdate`（`Option<Option<_>>` 同约定）。
    #[serde(default, deserialize_with = "crate::shared::types::deserialize_some")]
    pub order_no: Option<Option<String>>,
    /// 三态：缺省 = 不改该列；`null` = 清空成 NULL；给值 = 写入。
    ///
    /// 2026-10-06 review 第 1 轮：`NaiveDate` 放宽为 `String`，非法文本不再
    /// **打掉整个请求**，而是降级为「该行进 `failed[]` + HTTP 200 + 信封」。
    ///
    /// 根因在写入端（前端）：`parseDateOrNull`（`frontend/src/utils/
    /// purchaseOrderExcelParser.ts:104-117`）对 dayjs 认不出的文本（`2026年8月1日` /
    /// `待定` …）**原样透传**（`:116` `return text`），该值被预填进候选行的
    /// `systemDeliveryDate`（`PurchaseOrderImportDialog.vue:637`）并在提交时原样
    /// 发出（同文件 `:670`）。而 `el-date-picker` **不会**洗掉 model 值
    /// （`use-common-picker.mjs:25-40`：`parseDate` 失败只让展示用的 `parsedValue`
    /// 变空，`props.modelValue` 不被改写）—— 用户眼里看到的是「清空的输入框」，
    /// 实际发出去的是那句中文。
    ///
    /// 声明成 `Option<NaiveDate>` 时这类文本在 **axum JsonRejection** 层就 400，
    /// 且 body 是**纯文本、不是 `R` 信封** ⇒ 用户已勾选的整批回填全部作废，
    /// 前端连错误码都读不到。
    ///
    /// 同一功能的 match 端点已经用「**不声明** `delivery_date`」规避了这一类风险，
    /// 写端点原先没有对应处置 —— 契约内部不自洽，本轮补齐。
    ///
    /// **向后兼容**：`NaiveDate` 的 serde 格式就是 `%Y-%m-%d`，故对**所有合法日期**
    /// 「`String` + service 侧逐行 `parse_from_str`」与原 `NaiveDate` 反序列化行为
    /// **完全一致**，前端无需再对齐。唯一新增的行为是把「非法文本」从整请求 400
    /// 降级为该行失败（正是「永远 200 + 信封」契约想要的）。service 侧逐行校验见
    /// `PartService::batch_update_order_info`。
    ///
    /// 前端本轮同时在**预填处**过滤（治本），本字段的宽松是治标兜底：前端漏一处、
    /// 别的调用方漏一处，都不再打掉整批。
    #[serde(default, deserialize_with = "crate::shared::types::deserialize_some")]
    pub system_delivery_date: Option<Option<String>>,
    #[serde(default, deserialize_with = "crate::shared::types::deserialize_some")]
    pub note: Option<Option<String>>,
    /// 2026-10-06 新增：`Some(true)` 时本行**不写库**，只计入 `skipped_count`。
    /// 前端对「候选非空但人工判定不该回填」的行用它跳过。
    #[serde(default)]
    pub skip: Option<bool>,
}

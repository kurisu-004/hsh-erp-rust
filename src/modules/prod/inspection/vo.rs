//! prod::inspection 子模块 VO —— 出参（handler 响应序列化层）
//!
//! 2026-10-05 新增：与 `prod::batch` / `prod::programming` 同形 VO 模块，仅
//! `Serialize` 不 `Deserialize`（禁止出现在 axum extractor 反序列化侧）。
//!
//! 2026-10-07 新增：待品检队列读出参（`InspectionQueueItemOut` /
//! `InspectionQueueListOut`）自 `prod::batch::vo` 迁入，**字段集与序列化形态逐字
//! 未变**（前端 Zod schema 依赖 `items[*]` 恰好 13 个 key、`total` / `limit` /
//! `offset` 是 JSON string 这三点）。
//!
//! i64 一律走 `serialize_i64` → JSON string（雪花 ID > 2^53，JS `Number` 会丢
//! 精度，参见 `shared::types` 模块 doc）。
//!
//! ## 两组出参
//! - 扫码树（`GET /scan/{serial_no}`）：`ScanTreeOut` / `ScanAssemblyOut` /
//!   `ScanPartOut` / `ScanBatchOut`，形状是一棵三层树 + 命中来源标记：
//!   ```text
//!   ScanTreeOut
//!   ├─ hit_kind / scanned_serial_no   命中来源与原始扫码串
//!   ├─ assembly: Option<ScanAssemblyOut>   装配件节点（**没有批次**）
//!   └─ children: Vec<ScanPartOut>          顶层零件节点
//!      └─ children: Vec<ScanBatchOut>       该零件的全部批次
//!   ```
//!   `children` 恒为**非空语义**的数组（装配件无活跃子件时会是空数组，前端直接
//!   渲染「该装配件没有子件」），不返回 `null` —— 减少前端一层 `?? []` 判空。
//!   字段集刻意收窄：不投 `applicant_name` / `note` / 价格三列 /
//!   `process_chain_id` / `next_process_id` / 送货单号 —— 扫码树只回答「这是谁的件、
//!   现在在什么状态、每个批次能点什么动作」。
//! - 队列（`GET /queue`）：`InspectionQueueListOut` / `InspectionQueueItemOut`
//!   （扁平分页列表，见下方各自 doc）。
//!
//! **本文件按层平铺而非 `vo/` 子目录**：本域 VO 是纯类型容器（无逻辑、无互调），
//! 两组共 7 个结构体，按 `prod::process_design` / `prod::shelf_process` 等平级单文件
//! 域的形态留在 `vo.rs`；真正需要「按职责拆文件 + 精确 re-export」的是会自己长出
//! 逻辑的 VO 子目录（`prod::dashboard` / `prod::programming`）。

use chrono::NaiveDate;
use serde::Serialize;

use crate::modules::prod::inspection::model::InspectionQueueRow;
use crate::shared::types::serialize_i64;

/// `GET /api/v2/prod/inspection/scan/{serial_no}` 顶层响应（扫码树）。
///
/// 字段顺序按「命中来源 → 装配件 → 子节点」三段分组，与树的渲染顺序一致。
#[derive(Debug, Clone, Serialize)]
pub struct ScanTreeOut {
    /// 命中来源：`"ASSEMBLY"` = 扫到的是装配件条码；`"PART"` = 扫到的是
    /// 独立件或装配件子件的条码。
    ///
    /// 取值只有这两个字面量（service 内由私有 `HitKind` 枚举收敛，杜绝拼错）；
    /// 出参类型刻意是 `String` 而非枚举，与响应契约逐字一致，前端 Zod 侧按
    /// `z.enum(['ASSEMBLY','PART'])` 校验即可。
    pub hit_kind: String,
    /// 回显原始扫码串（trim 后的值），便于前端把扫码结果与历史记录对齐。
    pub scanned_serial_no: String,
    /// 装配件节点。仅 `hit_kind == "ASSEMBLY"` 或扫中的零件是某个装配件的
    /// 子件时有值；独立件树为 `null`。
    ///
    /// ⚠️ 装配件节点**没有批次**：`t_assembly` 在 `t_part_batch` 里没有行，批次
    /// 只挂在 `ScanPartOut::children` 上。前端不要在装配件层找「送检」动作的锚。
    pub assembly: Option<ScanAssemblyOut>,
    /// 顶层零件节点。装配件树 = 该装配件的**全部**子件（不止被扫中的那个）；
    /// 独立件树 = `[被扫中的那个 part]`。
    pub children: Vec<ScanPartOut>,
}

/// 装配件节点（无批次）。
#[derive(Debug, Clone, Serialize)]
pub struct ScanAssemblyOut {
    #[serde(serialize_with = "serialize_i64")]
    pub id: i64,
    pub serial_no: Option<String>,
    pub name: String,
    pub drawing_no: String,
    /// `t_assembly.status` 原文（7 态）。
    pub status: String,
    pub quantity: i32,
    pub is_urgent: bool,
    pub system_delivery_date: Option<NaiveDate>,
    /// L2 叶子客户名（`LEFT JOIN t_customer`，取不到为 `null`）。
    pub customer_name: Option<String>,
}

/// 零件节点（其 `children` 是该零件的全部批次）。
#[derive(Debug, Clone, Serialize)]
pub struct ScanPartOut {
    #[serde(serialize_with = "serialize_i64")]
    pub id: i64,
    pub serial_no: Option<String>,
    pub name: String,
    pub drawing_no: String,
    /// `t_part.status` 原文（8 态）。
    pub status: String,
    pub quantity: i32,
    pub is_urgent: bool,
    pub system_delivery_date: Option<NaiveDate>,
    pub customer_name: Option<String>,
    /// `t_part.version` —— **仅展示**。
    ///
    /// ⚠️ 与 [`ScanBatchOut::version`] 严格区分：任何批次写动作
    /// （`to-ship` / `to-process` / `to-inspection`）的 OCC 锚都是**批次**的
    /// version，本字段不参与。
    pub version: i32,
    /// 该零件的全部批次（**不按状态过滤**，含 `COMPLETED` / `CANCELLED` 等
    /// 终态），按 `batch_no ASC, id ASC` 排序。
    pub children: Vec<ScanBatchOut>,
}

/// 批次节点（本端点唯一的写操作锚）。
#[derive(Debug, Clone, Serialize)]
pub struct ScanBatchOut {
    /// `t_part_batch.id` —— 前端拿它当
    /// `POST /api/v2/prod/batches/{batch_id}/to-ship|to-process|to-inspection`
    /// 的路径参数。
    #[serde(serialize_with = "serialize_i64")]
    pub id: i64,
    pub batch_no: i32,
    pub quantity: i32,
    /// `t_part_batch.status` 原文（8 态）。**状态闸门由前端按本字段决定**：
    /// 后端不过滤、不改写，前端据此显示「送检」等按钮。
    pub status: String,
    /// `t_part_batch.version` —— 前端作 OCC 锚回传。
    ///
    /// ⚠️ 必须是**批次**版本，不是 `t_part.version`（见 [`ScanPartOut::version`]）。
    pub version: i32,
    /// 返修中标记；前端据此禁用「指定工序」（后端对返修中批次返 20118）。
    pub is_repairing: bool,
    /// 位置（`PRODUCTION_SHELF` / `WORKER` / `INSPECTION_SHELF` / `OFFICE` …）。
    pub location: Option<String>,
    /// 当前位置持有者名（货架 / 工人 / 外协公司三表 `COALESCE`）。
    pub current_holder_display: Option<String>,
    /// 批次当前工序名（来自 `current_process_id` → `t_process.name`）。
    ///
    /// ⚠️ `INSPECTION` / `DELIVERED` 批次**恒为 `null`**：这两态按「出池清
    /// `current_process_id`」不变式把该列置 NULL，这是**正确**结果不是缺陷。
    pub process_name: Option<String>,
    /// 该批次所属零件就是被扫中的那个 → 前端高亮。
    ///
    /// 由来：`t_part_batch` **没有序列号列**，命中关系只能由 service 在内存里
    /// 比对 `batch.part_id == 命中 part.id` 得出。装配件树里**只有**被扫中的那个
    /// 子件的批次为 `true`；扫装配件条码时无命中零件，故全为 `false`。
    pub is_scanned: bool,
}

// ===== 待品检队列（`GET /queue`） =====

/// `GET /api/v2/prod/inspection/queue` 出参项。
///
/// 2026-10-07 自 `prod::batch::vo` 迁入（域迁移，字段逐字未改）。本 VO 只服务
/// 待品检队列页，字段严格对齐前端 7 个数据列
/// （序列号 / 图号 / 名称 / 批次 / 数量 / 系统交期 / 客户）+ 操作列所需的
/// 锚点（`batch_id` / `version` / `part_id` / `is_urgent` / `customer_id`）。
/// 返修两条端点（`GET /api/v2/prod/batches/repair` / `repairing`）继续用
/// `prod::batch::vo::InspectionBatchListItemOut`（28 字段，本 VO 不共用）——
/// 共用会让那 15 个字段在待品检页成为无用负载。
#[derive(Debug, Clone, Serialize)]
pub struct InspectionQueueItemOut {
    /// 三个写端点的路径参数 + 扫码选择行标识。
    #[serde(serialize_with = "serialize_i64")]
    pub batch_id: i64,
    /// 批次列。
    pub batch_no: i32,
    /// 数量列 + 部分通过弹窗上限（`POST /prod/batches/{batch_id}/to-ship` 的
    /// `quantity` 不得超过本值）。
    pub quantity: i32,
    /// OCC 锚 `t_part_batch.version`（**不是** `t_part.version`）。
    pub version: i32,
    /// 详情页 `/parts/{part_id}`。
    #[serde(serialize_with = "serialize_i64")]
    pub part_id: i64,
    /// 序列号列（`t_part.serial_no` 可空：手工工单可没序列号）。
    pub serial_no: Option<String>,
    /// 图号列。
    pub drawing_no: String,
    /// 名称列。
    pub name: String,
    /// 系统交期列。可空 → JSON `null`。
    pub system_delivery_date: Option<NaiveDate>,
    /// 加急红底。
    pub is_urgent: bool,
    /// 客户表头筛选的入参回显（caller 选中 L1 / L2 都用它）。
    #[serde(serialize_with = "serialize_i64")]
    pub customer_id: i64,
    pub customer_name: Option<String>,
    pub l1_customer_name: Option<String>,
}

impl From<InspectionQueueRow> for InspectionQueueItemOut {
    fn from(r: InspectionQueueRow) -> Self {
        Self {
            batch_id: r.batch_id,
            batch_no: r.batch_no,
            quantity: r.quantity,
            version: r.version,
            part_id: r.part_id,
            serial_no: r.serial_no,
            drawing_no: r.drawing_no,
            name: r.name,
            system_delivery_date: r.system_delivery_date,
            is_urgent: r.is_urgent,
            customer_id: r.customer_id,
            customer_name: r.customer_name,
            l1_customer_name: r.l1_customer_name,
        }
    }
}

/// `GET /api/v2/prod/inspection/queue` 出参（分页）。
#[derive(Debug, Clone, Serialize)]
pub struct InspectionQueueListOut {
    pub items: Vec<InspectionQueueItemOut>,
    #[serde(serialize_with = "serialize_i64")]
    pub total: i64,
    #[serde(serialize_with = "serialize_i64")]
    pub limit: i64,
    #[serde(serialize_with = "serialize_i64")]
    pub offset: i64,
}

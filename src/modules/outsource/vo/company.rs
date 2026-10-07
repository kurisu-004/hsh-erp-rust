//! outsource 域 company 端点响应 VO（2026-09-22 PR4 重构）

use serde::Serialize;

use crate::shared::types::serialize_i64;

/// 外协公司详情（不含工序映射）。
///
/// 2026-10-09 删 `created_at` / `updated_at`：公司一览是对账 / 报价 / 看板三处的
/// 公司下拉数据源，前端只渲染「名称 + 联系人 + 启停用」，两列时间戳无任何消费方，
/// 而每次写端点都会让它们变化 ⇒ 纯粹的缓存抖动。
#[derive(Debug, Clone, Serialize)]
pub struct OutsourceCompanyOut {
    #[serde(serialize_with = "serialize_i64")]
    pub id: i64,
    pub name: String,
    pub contact_name: Option<String>,
    pub contact_phone: Option<String>,
    pub address: Option<String>,
    pub is_active: bool,
    /// OCC 锚（`POST /{id}/update` 与 `POST /{id}/soft-delete` 必传）。
    pub version: i32,
}

/// 外协公司详情（含工序映射）。
///
/// 2026-10-09 删两个时间字段（与 [`OutsourceCompanyOut`] 同款理由）。
#[derive(Debug, Clone, Serialize)]
pub struct OutsourceCompanyWithProcessesOut {
    #[serde(serialize_with = "serialize_i64")]
    pub id: i64,
    pub name: String,
    pub contact_name: Option<String>,
    pub contact_phone: Option<String>,
    pub address: Option<String>,
    pub is_active: bool,
    pub version: i32,
    pub processes: Vec<OutsourceCompanyProcessLinkOut>,
}

/// 公司 ↔ 工序 映射出参。
///
/// 2026-10-09 删两个字段：
/// - `category`：前端勾选框的候选集来自 `GET /proc/processes?category=OUTSOURCE`
///   （独立端点），本字段与那份候选集恒等，冗余；
/// - `sort_order`：**从不由 VO 消费** —— 它只被写侧 `replace_processes` 赋值、被
///   看板 `pool_list_companies_with_held` 的 `ORDER BY MIN(cp.sort_order)` 读，
///   两条都不经过本 VO。映射的展示顺序由请求数组顺序决定。
#[derive(Debug, Clone, Serialize)]
pub struct OutsourceCompanyProcessLinkOut {
    #[serde(serialize_with = "serialize_i64")]
    pub process_id: i64,
    pub process_code: String,
    pub process_name: String,
}

/// 外协公司列表出参。
#[derive(Debug, Clone, Serialize)]
pub struct OutsourceCompanyListOut {
    pub items: Vec<OutsourceCompanyOut>,
    pub total: i64,
    pub limit: i64,
    pub offset: i64,
}

/// `GET /outsource-companies/by-process/{process_id}` 的窄出参（2026-10-09 新增）。
///
/// 前端只 map `id` + `name` 两个字段（工序对话框的公司多选），而 `is_active` 是
/// **结构性冗余**：service 层已经在 Rust 里 `filter(|c| c.is_active)` 掉了停用公司，
/// 能出现在本列表里的行恒为启用，再返一列 `is_active` 等于把「已被后端消掉的事实」
/// 重新交给前端判断 —— 前端真要写 `c.is_active` 判据就是在读一个恒为 `true` 的值。
#[derive(Debug, Clone, Serialize)]
pub struct OutsourceCompanyOptionOut {
    #[serde(serialize_with = "serialize_i64")]
    pub id: i64,
    pub name: String,
}

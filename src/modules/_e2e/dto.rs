//! _e2e 模块 DTO
//!
//! 命名与 seed.ts 入参 key 一一对应（snake_case）。

use serde::{Deserialize, Serialize};

/// 探测响应：携带 enabled 状态方便前端/测试日志断言。
#[derive(Debug, Serialize)]
pub struct ProbeResp {
    pub status: &'static str,
    pub enabled: bool,
}

/// reset 响应：返回清掉的元数据行数（业务表本身不动）。
#[derive(Debug, Serialize)]
pub struct ResetResp {
    pub cleared: i64,
}

/// L1 / L2 客户 seed 入参。L1 必带 serial_prefix；L2 必带 parent_id。
#[derive(Debug, Deserialize)]
pub struct SeedCustomerReq {
    pub name: String,
    #[serde(default)]
    pub parent_id: Option<String>, // 雪花 ID 字符串（防 JS 截断）
    #[serde(default)]
    pub serial_prefix: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct SeedApplicantReq {
    pub name: String,
    pub customer_id: String,
}

#[derive(Debug, Deserialize)]
pub struct SeedWorkerReq {
    pub name: String,
    /// 工种 code（如 "送货司机"）；在 handler 内反查 work_type_id。
    pub work_type_code: String,
}

#[derive(Debug, Deserialize)]
pub struct SeedPartReq {
    pub serial: String,         // 序列号（如 "A1-0001"）
    pub customer_id: String,    // L2 客户 id
    pub applicant_name: String, // 与 t_part.applicant_name 字符串字段一致
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub drawing_no: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct SeedOutsourceCompanyReq {
    pub name: String,
}

#[derive(Debug, Deserialize)]
pub struct SeedOutsourceQuoteReq {
    pub part_id: String,
    pub company_id: String,
    pub process_id: String,
    pub price: f64,
}

#[derive(Debug, Deserialize)]
pub struct SeedDeliveryNoteReq {
    #[serde(default = "default_status")]
    pub status: String, // DRAFT / SUBMITTED / PICKED_UP / ARCHIVED
    pub customer_id: String,
}

fn default_status() -> String {
    "DRAFT".into()
}

#[derive(Debug, Deserialize)]
pub struct SeedUserReq {
    pub username: String,
    /// 角色列表（MANAGER / CLERK / INSPECTOR / CNC_PROGRAMMER / SHELF_ACCOUNT）。
    /// 大写字符串，与 t_user_role.role 列约束一致。
    pub role_codes: Vec<String>,
    #[serde(default)]
    pub phone: Option<String>,
    #[serde(default)]
    pub full_name: Option<String>,
    /// 可选明文密码（缺省 "changeme"，与 alembic prod_data seed 对齐）。
    #[serde(default)]
    pub password: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct RevokeSessionReq {
    pub username: String,
}

/// 各 seed handler 的统一出参：返回新行的雪花 id。
#[derive(Debug, Serialize)]
pub struct SeedCreatedResp {
    pub id: String, // 雪花 ID 字符串（前端约定）
}

/// 2026-09-15 新增：hard_delete_outsource_company 出参。
/// 物理删一行 t_outsource_company + 清 t_e2e_seeded 元数据。
/// `deleted` 始终为 true（idempotent：id 不存在也返 true）。
#[derive(Debug, Serialize)]
pub struct HardDeleteResp {
    pub deleted: bool,
}

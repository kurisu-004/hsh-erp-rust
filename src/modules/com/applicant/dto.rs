//! applicant 域 DTO（仅入参）
//!
//! 对应 Python myERP/schema/applicant.py。
//!
//! 出参 VO（`ApplicantOut` / `ApplicantListOut`）已抽离到 `super::vo`，本文件不再 derive Serialize。
//!
//! 2026-09-22 PR4：出参结构平移到 `vo/applicant.rs`，对齐 iam 范本。

use serde::Deserialize;

// ---- 入参 ----

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ApplicantListQuery {
    #[serde(default)]
    pub customer_id: Option<String>,
    #[serde(default)]
    pub name_like: Option<String>,
    #[serde(default)]
    pub limit: Option<i64>,
    #[serde(default)]
    pub offset: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ApplicantCreateRequest {
    pub name: String,
    pub customer_id: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ApplicantUpdateRequest {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub customer_id: Option<String>,
}

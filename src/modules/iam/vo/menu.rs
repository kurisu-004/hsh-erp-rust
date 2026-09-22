//! iam 域菜单 VO

use serde::Serialize;

/// 菜单树节点（递归 children）
#[derive(Debug, Clone, Serialize)]
pub struct MenuNodeOut {
    #[serde(serialize_with = "crate::shared::types::serialize_i64")]
    pub id: i64,
    pub version: i32,
    pub parent_id: Option<String>,
    pub code: String,
    pub title: String,
    pub path: Option<String>,
    pub icon: Option<String>,
    pub sort_order: i32,
    #[serde(default)]
    pub children: Vec<MenuNodeOut>,
}
//! part_file 域数据模型（Phase PR-CRUD 2026-08-25）
//!
//! 对齐 `migrations/20260811100007_007_create_file_tables.sql:12-31` 16 列完整投影。
//! `part_id` 是 polymorphic（指向 `t_part.id` 或 `t_assembly.id`），本轮只处理 `t_part`
//! 路径（kind='DRAWING' 上传）。

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct TPartFile {
    pub id: i64,
    pub part_id: i64,
    pub kind: String,                              // DRAWING / 3D_MODEL / G_CODE / SETUP_SHEET / ASSEMBLY_MASTER / CAD_2D
    pub file_type: String,                         // uppercased extension
    pub object_key: String,                        // COS object key
    pub original_filename: String,
    pub file_size: i64,
    pub content_type: String,
    pub upload_status: String,                     // READY / PENDING / FAILED
    pub content_sha256: Option<String>,
    pub version: i32,
    pub created_at: chrono::NaiveDateTime,
    pub created_by: Option<i64>,
    pub updated_at: chrono::NaiveDateTime,
    pub updated_by: Option<i64>,
    pub deleted_at: Option<chrono::NaiveDateTime>,
    pub paired_file_id: Option<i64>,
}

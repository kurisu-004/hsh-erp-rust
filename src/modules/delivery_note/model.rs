//! delivery_note 域数据模型占位
//!
// 对应 Python myERP/model/delivery_note.py。包含：
// - sqlx `FromRow` 行结构（含 version 乐观锁、deleted_at 软删、created/updated 审计字段）
// - 域枚举（DB 用 varchar，应用层用 enum 校验）

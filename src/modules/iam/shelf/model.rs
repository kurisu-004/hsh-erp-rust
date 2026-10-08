//! 货架子模块数据模型
//!
//! 对应 Python myERP/model/shelf.py。包含：
//! - sqlx `FromRow` 行结构（含 version 乐观锁、deleted_at 软删、created/updated 审计字段）
//! - 域枚举（DB 用 varchar，应用层用 enum 校验）
//!
//! `TShelf` 是**全仓唯一的 `t_shelf` 行结构**（14 列，含 `location` 与
//! `capacity`）：iam 账号侧的 SHELF_ACCOUNT scope 校验、货架 CRUD、part / prod /
//! shared 各处对 `ShelfRepo` 的调用，读的都是这一份。
//!
//! `zone`: `'PRODUCTION'` | `'INSPECTION'`（DB varchar，无 enum 约束；应用层校验）。

use sqlx::FromRow;

/// `t_shelf` 行结构（CRUD / picker / to-inspection / to-process 全投影）。
///
/// `zone`: `'PRODUCTION'` | `'INSPECTION'`（DB varchar，无 enum 约束；
/// 应用层校验）。
///
/// `location`：物理位置描述（货架所在通道/楼层），由 MANAGER 创建/更新
/// 时填写，可空。
///
/// `capacity`（2026-10-10）：负载上限（件数）。`None` 或 `<= 0` = 不限，
/// 选架排序时恒排最后（见 `shared::shelf::select::pick_least_loaded`）。
#[derive(Debug, Clone, FromRow)]
pub struct TShelf {
    pub id: i64,
    pub code: String,
    pub name: String,
    pub zone: String,
    pub location: Option<String>,
    pub is_active: bool,
    pub display_order: i32,
    pub capacity: Option<i32>,
    pub version: i32,
    pub created_at: chrono::NaiveDateTime,
    pub created_by: Option<i64>,
    pub updated_at: chrono::NaiveDateTime,
    pub updated_by: Option<i64>,
    pub deleted_at: Option<chrono::NaiveDateTime>,
}

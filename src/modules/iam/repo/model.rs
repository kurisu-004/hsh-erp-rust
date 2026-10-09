//! iam 域数据模型
//!
//! 2026-10-11（wx BFF 重构 B1）：`User` 新增 `worker_id: Option<i64>` —— 系统账号
//! 「正式绑定」到 `t_worker.id`，取代此前把 `t_user.id` 直接当 `t_worker.id` 用的
//! 错误假设。详见该字段的 doc 与
//! `migrations/20261011120000_001_add_user_worker_id.sql`。

use chrono::NaiveDateTime;

/// `t_user` 行
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct User {
    pub id: i64,
    pub username: String,
    pub password_hash: String,
    pub full_name: String,
    pub phone: Option<String>,
    pub is_active: bool,
    pub last_login_at: Option<NaiveDateTime>,
    pub refresh_token_version: i32,
    /// 该系统账号绑定的工人（`t_worker.id`）；`None` = 非工人账号 / 尚未绑定。
    ///
    /// 2026-10-11 随 wx BFF 重构新增。此前 `t_user` 与 `t_worker` 之间**没有任何
    /// 映射**，`GET /api/v2/wx/worker/stats` 只能把 `t_user.id` 直接当
    /// `t_part_event.worker_id`（语义上是 `t_worker.id`）用，实测对任何真实用户
    /// 恒返 `batch_count = 0`；本列是正式绑定关系（见
    /// `migrations/20261011120000_001_add_user_worker_id.sql`）。
    ///
    /// - **无物理外键**（仓库铁律）：普通 `bigint` + partial 索引
    ///   `ix_t_user_worker_id`，指向的工人是否存在 / 是否软删由 service 层校验；
    /// - 该列**跨域**指向 `prod::worker` 域的 `t_worker`，但 iam 侧只作为
    ///   「账号自身属性」存读，不在本域 SQL 里 JOIN `t_worker`，故不违反
    ///   `iam_domain_depends_on_no_other_domain` 域隔离护栏；
    /// - **当前无回填**：一次性回填脚本是
    ///   `scripts/sql/20261011_backfill_t_user_worker_id.sql`（须人工确认后手工
    ///   执行），库中存量账号的该列一律为 NULL。
    pub worker_id: Option<i64>,
    pub version: i32,
    pub created_at: NaiveDateTime,
    pub created_by: Option<i64>,
    pub updated_at: NaiveDateTime,
    pub updated_by: Option<i64>,
    pub deleted_at: Option<NaiveDateTime>,
}

/// `t_user_role` 行
///
/// `scope_type` / `scope_id` 仅在 `role = SHELF_ACCOUNT` 时非空（scope_type 固定 `shelf`）。
/// 唯一约束实名 `uk_t_user_role_user_role_scope`，且它是**普通 UNIQUE 约束（不是
/// partial）** —— 建表语句里没有 `WHERE deleted_at IS NULL` 谓词。PostgreSQL 的
/// UNIQUE 视 NULL 互不相等，故 `(user_id, role, NULL, NULL)` 这种含 NULL 的组合
/// **可以被重复插入**；「软删后可重新添加同一角色」得以成立，但「软删前不重复」
/// 并非由索引保证，而是 service 层 `has_user_role_with_scope`（`IS NOT DISTINCT FROM`
/// 预检）兜住的。详见 `docs/api/iam.md` 的「已知偏差登记」。
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct UserRole {
    pub id: i64,
    pub user_id: i64,
    pub role: String,
    pub scope_type: Option<String>,
    pub scope_id: Option<i64>,
    pub version: i32,
    pub created_at: NaiveDateTime,
    pub created_by: Option<i64>,
    pub updated_at: NaiveDateTime,
    pub updated_by: Option<i64>,
    pub deleted_at: Option<NaiveDateTime>,
}

/// `t_wx_identity` 行（企业微信 userid → 系统账号 `t_user.id` 的预绑定映射）
///
/// 2026-10-10 自 `modules/wx/repo.rs` 搬入本文件：映射关系属 iam 域的账号数据，
/// wx 域只是消费方。`corp_id` 参与唯一索引的依据是官方「userid 企业内唯一」，
/// 单企业部署下它是常量，属为多企业预留。
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct WxIdentity {
    pub id: i64,
    /// 企业 ID（来自 `WECOM_CORPID`；多企业部署时同一 userid 在不同企业独立）
    pub corp_id: String,
    /// 企业微信 userid（自建应用返回明文；存小写——企微 userid 不区分大小写）
    pub wx_user_id: String,
    /// 对应的系统账号雪花 ID
    pub user_id: i64,
    /// 乐观锁版本（解绑 soft_delete 时带条件）
    pub version: i32,
    pub created_at: NaiveDateTime,
    pub created_by: Option<i64>,
    pub updated_at: NaiveDateTime,
    pub updated_by: Option<i64>,
    pub deleted_at: Option<NaiveDateTime>,
}

/// `t_wx_identity` INSERT 入参（id 由调用方用雪花生成，审计字段同批填好）
#[derive(Debug, Clone)]
pub struct WxIdentityInsert {
    pub id: i64,
    pub corp_id: String,
    /// 已 trim + 转小写的 userid
    pub wx_user_id: String,
    pub user_id: i64,
    pub created_at: NaiveDateTime,
    pub created_by: Option<i64>,
}

/// `t_menu` 行（`parent_id` 自引用，CHECK `ck_t_menu_no_self_loop` 禁止自环）
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Menu {
    pub id: i64,
    pub parent_id: Option<i64>,
    pub code: String,
    pub title: String,
    pub path: Option<String>,
    pub icon: Option<String>,
    pub sort_order: i32,
    pub is_active: bool,
    pub version: i32,
    pub created_at: NaiveDateTime,
    pub created_by: Option<i64>,
    pub updated_at: NaiveDateTime,
    pub updated_by: Option<i64>,
    pub deleted_at: Option<NaiveDateTime>,
}

//! RBAC 子模块 SQL 真源（`t_role` / `t_permission` / `t_role_permission` +
//! 与 `t_role_menu` 的授权读写）
//!
//! 采用与 `iam::shelf::repo` 相同的形态：ZST struct `RbacRepo` + 固有静态方法，
//! executor 泛型 `impl PgExecutor<'_>`（`&mut PgConnection` / `&mut Transaction`
//! 都满足）。不用胖 trait —— 本模块零 lib 单测（全部行为由
//! `tests/iam/rbac.rs` 的端到端用例守护），trait 的 `mockall::automock`
//! 无消费方。
//!
//! ## 零外键关联
//! 三张表之间、以及与 `t_user_role.role` / `t_role_menu.role` 之间**一律 varchar
//! 关联**（migration 的文件头已说明理由）。SQL 侧因此必须自己带软删 / 有效性
//! 谓词，不能指望 DB 拦：
//! - 读 code 时 `JOIN t_menu` / `JOIN t_permission` 并各自过滤
//!   `deleted_at IS NULL AND is_active`，顺带过滤掉「权限点已下线」的授权行；
//! - 改名走 service 层显式三表级联 UPDATE，不靠任何 DB 特性。
//!
//! ## 审计与软删
//! 读侧一律 `deleted_at IS NULL`；写侧 UPDATE 带 `WHERE id=$1 AND version=$2`
//! 返回 `rows_affected`，0 行由 service 转 OCC 冲突。

use sqlx::{PgConnection, PgExecutor};

use crate::modules::iam::rbac::model::{TPermission, TRole};

// ===========================================================================
// 入参 DTO
// ===========================================================================

/// `t_role` INSERT 入参（id 由 service 用雪花生成）
pub struct RoleInsert<'a> {
    pub id: i64,
    pub code: &'a str,
    pub name: &'a str,
    pub description: Option<&'a str>,
    pub created_at: chrono::NaiveDateTime,
    pub created_by: Option<i64>,
}

/// `t_role` 部分更新入参（`code` 为 `None` = 不改）
pub struct RolePartialUpdate<'a> {
    pub code: Option<&'a str>,
    pub name: &'a str,
    pub description: Option<&'a str>,
    pub updated_at: chrono::NaiveDateTime,
    pub updated_by: Option<i64>,
}

pub struct RbacRepo;

impl RbacRepo {
    // =======================================================================
    // t_role
    // =======================================================================

    /// 角色列表 + 每个角色的**在册用户数**。
    ///
    /// - `user_count` 口径：`t_user.is_active = true` **且** `t_user_role.deleted_at
    ///   IS NULL` 的行数；额外带 `t_user.deleted_at IS NULL` 与 is_active 等价
    ///   （本仓停用账号一律同时置 `is_active=false` + 置 `deleted_at`），写出
    ///   两条谓词是为了让口径自解释、不依赖那个「等价」不变式。
    /// - 排序：`is_system DESC`（ADMIN 等内置角色置顶）、再 `code ASC`。
    pub async fn list_roles_with_user_count<'e, E: PgExecutor<'e>>(
        executor: E,
    ) -> Result<Vec<RoleWithUserCount>, sqlx::Error> {
        sqlx::query_as!(
            RoleWithUserCount,
            r#"
            SELECT r.id, r.code, r.name, r.description, r.is_system, r.is_active,
                   r.version, r.created_at, r.created_by, r.updated_at, r.updated_by,
                   r.deleted_at,
                   COUNT(ur.id) FILTER (
                       WHERE ur.id IS NOT NULL
                         AND ur.deleted_at IS NULL
                         AND u.id IS NOT NULL
                         AND u.is_active = TRUE
                         AND u.deleted_at IS NULL
                   ) AS "user_count!"
            FROM t_role r
            LEFT JOIN t_user_role ur ON ur.role = r.code
            LEFT JOIN t_user u ON u.id = ur.user_id
            WHERE r.deleted_at IS NULL
            GROUP BY r.id, r.code, r.name, r.description, r.is_system, r.is_active,
                     r.version, r.created_at, r.created_by, r.updated_at, r.updated_by,
                     r.deleted_at
            ORDER BY r.is_system DESC, r.code ASC
            "#,
        )
        .fetch_all(executor)
        .await
    }

    /// 按 id 查角色（未软删）。id 是雪花，取不到即 404 归因。
    pub async fn get_role_by_id<'e, E: PgExecutor<'e>>(
        executor: E,
        id: i64,
    ) -> Result<Option<TRole>, sqlx::Error> {
        sqlx::query_as!(
            TRole,
            r#"
            SELECT id, code, name, description, is_system, is_active, version,
                   created_at, created_by, updated_at, updated_by, deleted_at
            FROM t_role
            WHERE id = $1 AND deleted_at IS NULL
            "#,
            id,
        )
        .fetch_optional(executor)
        .await
    }

    /// 按 code 查角色（未软删）。用于「code 是否已被占用」与 ADMIN 判定。
    pub async fn get_role_by_code<'e, E: PgExecutor<'e>>(
        executor: E,
        code: &str,
    ) -> Result<Option<TRole>, sqlx::Error> {
        sqlx::query_as!(
            TRole,
            r#"
            SELECT id, code, name, description, is_system, is_active, version,
                   created_at, created_by, updated_at, updated_by, deleted_at
            FROM t_role
            WHERE code = $1 AND deleted_at IS NULL
            "#,
            code,
        )
        .fetch_optional(executor)
        .await
    }

    pub async fn create_role<'e, E: PgExecutor<'e>>(
        executor: E,
        ins: &RoleInsert<'_>,
    ) -> Result<(), sqlx::Error> {
        sqlx::query!(
            r#"
            INSERT INTO t_role (id, code, name, description, is_system, is_active,
                                version, created_at, created_by, updated_at, updated_by)
            VALUES ($1, $2, $3, $4, FALSE, TRUE, 0, $5, $6, $5, $6)
            "#,
            ins.id,
            ins.code,
            ins.name,
            ins.description,
            ins.created_at,
            ins.created_by,
        )
        .execute(executor)
        .await?;
        Ok(())
    }

    /// 部分更新（OCC）。`code` 省略 = 不改。
    ///
    /// ⚠️ **改名的事务边界在 service**：本方法只改 `t_role` 一行，
    /// 三张关联表（`t_user_role` / `t_role_menu` / `t_role_permission`）的级联
    /// 由 [`Self::cascade_rename_role_code`] 在**同一事务**里紧随其后调用。
    /// 拆成两个方法而不是一条跨表 SQL，是为了让「哪些行被改了」能被行数断言。
    #[allow(clippy::too_many_arguments)]
    pub async fn update_role<'e, E: PgExecutor<'e>>(
        executor: E,
        id: i64,
        version: i32,
        args: &RolePartialUpdate<'_>,
    ) -> Result<u64, sqlx::Error> {
        sqlx::query!(
            r#"
            UPDATE t_role
            SET code        = COALESCE($3, code),
                name        = $4,
                description = $5,
                updated_at  = $6,
                updated_by  = $7,
                version     = version + 1
            WHERE id = $1 AND version = $2 AND deleted_at IS NULL
            "#,
            id,
            version,
            args.code,
            args.name,
            args.description,
            args.updated_at,
            args.updated_by,
        )
        .execute(executor)
        .await
        .map(|r| r.rows_affected())
    }

    /// 停用（`delete = false`）：只置 `is_active = false`，行保留、version + 1。
    pub async fn deactivate_role<'e, E: PgExecutor<'e>>(
        executor: E,
        id: i64,
        version: i32,
        when: chrono::NaiveDateTime,
        updated_by: Option<i64>,
    ) -> Result<u64, sqlx::Error> {
        sqlx::query!(
            r#"
            UPDATE t_role
            SET is_active = FALSE, updated_at = $3, updated_by = $4, version = version + 1
            WHERE id = $1 AND version = $2 AND deleted_at IS NULL
            "#,
            id,
            version,
            when,
            updated_by,
        )
        .execute(executor)
        .await
        .map(|r| r.rows_affected())
    }

    /// 软删（`delete = true`）：置 `deleted_at` + `is_active = false`，version + 1。
    pub async fn soft_delete_role<'e, E: PgExecutor<'e>>(
        executor: E,
        id: i64,
        version: i32,
        when: chrono::NaiveDateTime,
        updated_by: Option<i64>,
    ) -> Result<u64, sqlx::Error> {
        sqlx::query!(
            r#"
            UPDATE t_role
            SET deleted_at = $3, is_active = FALSE, updated_at = $3, updated_by = $4,
                version = version + 1
            WHERE id = $1 AND version = $2 AND deleted_at IS NULL
            "#,
            id,
            version,
            when,
            updated_by,
        )
        .execute(executor)
        .await
        .map(|r| r.rows_affected())
    }

    /// 改 code 后**同事务**级联三张关联表。
    ///
    /// 刻意不 JOIN、不用触发器：级联的**结果**要被集成用例逐表断言，
    /// 三条独立的 UPDATE 才能让「哪张表改了多少行」在返回值里看得见。
    ///
    /// 只改 `deleted_at IS NULL` 的行 —— 已软删的授权行留原 code 是有意的
    /// 审计痕迹（它们不会再被读侧联查命中）。
    pub async fn cascade_rename_role_code(
        conn: &mut PgConnection,
        old_code: &str,
        new_code: &str,
        when: chrono::NaiveDateTime,
        updated_by: Option<i64>,
    ) -> Result<(), sqlx::Error> {
        for sql in [
            r#"UPDATE t_user_role SET role = $2, updated_at = $3, updated_by = $4
               WHERE role = $1 AND deleted_at IS NULL"#,
            r#"UPDATE t_role_menu SET role = $2, updated_at = $3, updated_by = $4
               WHERE role = $1 AND deleted_at IS NULL"#,
            r#"UPDATE t_role_permission SET role_code = $2, updated_at = $3, updated_by = $4
               WHERE role_code = $1 AND deleted_at IS NULL"#,
        ] {
            sqlx::query(sql)
                .bind(old_code)
                .bind(new_code)
                .bind(when)
                .bind(updated_by)
                .execute(&mut *conn)
                .await?;
        }
        Ok(())
    }

    /// 软删某角色的全部 `t_user_role` 授权行（删角色时连带），返回受影响行数
    /// （= 本次被踢下线的用户数，供 `affected_user_count` 回给前端）。
    pub async fn soft_delete_user_roles_by_role_code<'e, E: PgExecutor<'e>>(
        executor: E,
        role_code: &str,
        when: chrono::NaiveDateTime,
        updated_by: Option<i64>,
    ) -> Result<u64, sqlx::Error> {
        sqlx::query!(
            r#"
            UPDATE t_user_role
            SET deleted_at = $2, updated_at = $2, updated_by = $3, version = version + 1
            WHERE role = $1 AND deleted_at IS NULL
            "#,
            role_code,
            when,
            updated_by,
        )
        .execute(executor)
        .await
        .map(|r| r.rows_affected())
    }

    /// 持有某角色的**在册用户 id**（踢下线用）。
    pub async fn list_active_user_ids_by_role_code<'e, E: PgExecutor<'e>>(
        executor: E,
        role_code: &str,
    ) -> Result<Vec<i64>, sqlx::Error> {
        sqlx::query_scalar!(
            r#"
            SELECT DISTINCT u.id
            FROM t_user_role ur
            JOIN t_user u ON u.id = ur.user_id
            WHERE ur.role = $1
              AND ur.deleted_at IS NULL
              AND u.is_active = TRUE
              AND u.deleted_at IS NULL
            ORDER BY u.id
            "#,
            role_code,
        )
        .fetch_all(executor)
        .await
    }

    /// 按**全部**角色 code 联查权限码集合（登录 / refresh 装载 session 缓存用）。
    ///
    /// ⚠️ ADMIN **不写行**（通配在 `require_permission` 里），故 ADMIN 用户的
    /// 本方法天然返回空 vec —— 这正是 BE-2 要的口径，不额外特判。
    pub async fn list_permission_codes_by_roles<'e, E: PgExecutor<'e>>(
        executor: E,
        roles: &[String],
    ) -> Result<Vec<String>, sqlx::Error> {
        if roles.is_empty() {
            return Ok(Vec::new());
        }
        sqlx::query_scalar!(
            r#"
            SELECT DISTINCT p.code
            FROM t_role_permission rp
            JOIN t_permission p ON p.code = rp.permission_code
            WHERE rp.role_code = ANY($1)
              AND rp.deleted_at IS NULL
              AND p.is_active = TRUE
              AND p.deleted_at IS NULL
            ORDER BY p.code
            "#,
            roles,
        )
        .fetch_all(executor)
        .await
    }

    /// 该用户当前是否持有某角色（`t_user_role` 有未软删行）。
    ///
    /// `deactivate_user` 的 ADMIN 护栏快筛用：停用非管理员账号是最高频的运维
    /// 动作，不该每次都跑一遍「数管理员」的查询。
    pub async fn user_has_role<'e, E: PgExecutor<'e>>(
        executor: E,
        user_id: i64,
        role_code: &str,
    ) -> Result<bool, sqlx::Error> {
        sqlx::query_scalar!(
            r#"
            SELECT EXISTS (
                SELECT 1 FROM t_user_role
                WHERE user_id = $1 AND role = $2 AND deleted_at IS NULL
            ) AS "exists!"
            "#,
            user_id,
            role_code,
        )
        .fetch_one(executor)
        .await
    }

    // =======================================================================
    // t_permission
    // =======================================================================

    /// 全部**启用中**权限点，按 `domain ASC, code ASC`。
    pub async fn list_active_permissions<'e, E: PgExecutor<'e>>(
        executor: E,
    ) -> Result<Vec<TPermission>, sqlx::Error> {
        sqlx::query_as!(
            TPermission,
            r#"
            SELECT id, code, name, domain, description, is_active, version,
                   created_at, created_by, updated_at, updated_by, deleted_at
            FROM t_permission
            WHERE is_active = TRUE AND deleted_at IS NULL
            ORDER BY domain ASC, code ASC
            "#,
        )
        .fetch_all(executor)
        .await
    }

    /// 一批权限码里哪些是**启用中**的（grants 提交的入参校验）。
    pub async fn filter_active_permission_codes<'e, E: PgExecutor<'e>>(
        executor: E,
        codes: &[String],
    ) -> Result<Vec<String>, sqlx::Error> {
        if codes.is_empty() {
            return Ok(Vec::new());
        }
        sqlx::query_scalar!(
            r#"
            SELECT code FROM t_permission
            WHERE code = ANY($1) AND is_active = TRUE AND deleted_at IS NULL
            ORDER BY code
            "#,
            codes,
        )
        .fetch_all(executor)
        .await
    }

    /// 一批菜单码里哪些是**启用中**的（grants 提交的入参校验）。
    pub async fn filter_active_menu_codes<'e, E: PgExecutor<'e>>(
        executor: E,
        codes: &[String],
    ) -> Result<Vec<String>, sqlx::Error> {
        if codes.is_empty() {
            return Ok(Vec::new());
        }
        sqlx::query_scalar!(
            r#"
            SELECT code FROM t_menu
            WHERE code = ANY($1) AND is_active = TRUE AND deleted_at IS NULL
            ORDER BY code
            "#,
            codes,
        )
        .fetch_all(executor)
        .await
    }

    /// 全部启用中的菜单码（ADMIN 的 grants 读侧返回全量用）。
    pub async fn list_all_active_menu_codes<'e, E: PgExecutor<'e>>(
        executor: E,
    ) -> Result<Vec<String>, sqlx::Error> {
        sqlx::query_scalar!(
            r#"
            SELECT code FROM t_menu
            WHERE is_active = TRUE AND deleted_at IS NULL
            ORDER BY code
            "#,
        )
        .fetch_all(executor)
        .await
    }

    /// 全部启用中的权限码（ADMIN 的 grants 读侧返回全量用）。
    pub async fn list_all_active_permission_codes<'e, E: PgExecutor<'e>>(
        executor: E,
    ) -> Result<Vec<String>, sqlx::Error> {
        sqlx::query_scalar!(
            r#"
            SELECT code FROM t_permission
            WHERE is_active = TRUE AND deleted_at IS NULL
            ORDER BY code
            "#,
        )
        .fetch_all(executor)
        .await
    }

    // =======================================================================
    // t_role_permission / t_role_menu —— 授权读取与整包替换
    // =======================================================================

    /// 某角色已授权的**启用中**权限码。
    ///
    /// 联查 `t_permission` 并过滤 `is_active AND deleted_at IS NULL`：权限点下线
    /// 即等同收回，孤儿 code（FK 缺失时可能出现的悬挂授权）自然被滤掉，
    /// 不会把库里不存在的 code 喂给前端。
    pub async fn list_permission_codes_by_role<'e, E: PgExecutor<'e>>(
        executor: E,
        role_code: &str,
    ) -> Result<Vec<String>, sqlx::Error> {
        sqlx::query_scalar!(
            r#"
            SELECT p.code
            FROM t_role_permission rp
            JOIN t_permission p ON p.code = rp.permission_code
            WHERE rp.role_code = $1
              AND rp.deleted_at IS NULL
              AND p.is_active = TRUE
              AND p.deleted_at IS NULL
            ORDER BY p.code
            "#,
            role_code,
        )
        .fetch_all(executor)
        .await
    }

    /// 某角色已授权的**启用中**菜单码。
    pub async fn list_menu_codes_by_role<'e, E: PgExecutor<'e>>(
        executor: E,
        role_code: &str,
    ) -> Result<Vec<String>, sqlx::Error> {
        sqlx::query_scalar!(
            r#"
            SELECT m.code
            FROM t_role_menu rm
            JOIN t_menu m ON m.id = rm.menu_id
            WHERE rm.role = $1
              AND rm.deleted_at IS NULL
              AND m.is_active = TRUE
              AND m.deleted_at IS NULL
            ORDER BY m.code
            "#,
            role_code,
        )
        .fetch_all(executor)
        .await
    }

    /// 整包替换的第一步：软删该角色现有的全部授权行（菜单 + 权限点）。
    ///
    /// 软删而非硬删，与本仓其它授权表一致（留审计）。整包替换后重跑同一次提交
    /// 不会叠行：partial 唯一索引 + 软删保证 live 行恒为一组。
    pub async fn soft_delete_all_grants_by_role(
        conn: &mut PgConnection,
        role_code: &str,
        when: chrono::NaiveDateTime,
        updated_by: Option<i64>,
    ) -> Result<(), sqlx::Error> {
        for sql in [
            r#"UPDATE t_role_menu SET deleted_at = $2, updated_at = $2, updated_by = $3,
                                     version = version + 1
               WHERE role = $1 AND deleted_at IS NULL"#,
            r#"UPDATE t_role_permission SET deleted_at = $2, updated_at = $2, updated_by = $3,
                                           version = version + 1
               WHERE role_code = $1 AND deleted_at IS NULL"#,
        ] {
            sqlx::query(sql)
                .bind(role_code)
                .bind(when)
                .bind(updated_by)
                .execute(&mut *conn)
                .await?;
        }
        Ok(())
    }

    /// 整包替换的第二步：批量插入新的菜单授权行。
    ///
    /// `id` 与 `code` 一一对应（`ids[i]` 是 `codes[i]` 这条授权的雪花主键），
    /// `menu_id` 由 SQL 侧按 code 反查 —— 授权表存的是 `menu_id` 不是 `code`。
    ///
    /// ⚠️ 反查少一行就整条失败（`Protocol` 错误）：入参已由
    ///    `filter_active_menu_codes` 校验过全部存在，走到这一步只能是被并发删除，
    ///    此时宁可让整包替换回滚，也不写「少几条」的成功结果。
    pub async fn insert_role_menus(
        conn: &mut PgConnection,
        role_code: &str,
        ids: &[i64],
        menu_codes: &[String],
        when: chrono::NaiveDateTime,
        created_by: Option<i64>,
    ) -> Result<(), sqlx::Error> {
        if menu_codes.is_empty() {
            return Ok(());
        }
        let resolved = sqlx::query_scalar!(
            r#"
            SELECT id FROM t_menu
            WHERE code = ANY($1) AND is_active = TRUE AND deleted_at IS NULL
            "#,
            menu_codes,
        )
        .fetch_all(&mut *conn)
        .await?;
        if resolved.len() != menu_codes.len() {
            return Err(sqlx::Error::Protocol(
                "部分菜单码在事务内消失（并发删除）".into(),
            ));
        }
        // 按 `menu_codes` 的顺序逐条插：授权表的列是 `menu_id` 而非 `code`，
        // 且 `ids[i]` 必须与 `menu_codes[i]` 一一对应（雪花按入参序生成），
        // 逐条写让这个对应关系是**显式**的，而不是靠 SQL 里的 JOIN 顺序隐式成立。
        for (id, menu_id) in ids.iter().zip(resolved.iter()) {
            sqlx::query!(
                r#"
                INSERT INTO t_role_menu (id, role, menu_id, version, created_at, created_by,
                                         updated_at, updated_by)
                VALUES ($1, $2, $3, 0, $4, $5, $4, $5)
                "#,
                id,
                role_code,
                menu_id,
                when,
                created_by,
            )
            .execute(&mut *conn)
            .await?;
        }
        Ok(())
    }

    /// 整包替换的第二步：批量插入新的权限点授权行。
    pub async fn insert_role_permissions(
        conn: &mut PgConnection,
        role_code: &str,
        ids: &[i64],
        permission_codes: &[String],
        when: chrono::NaiveDateTime,
        created_by: Option<i64>,
    ) -> Result<(), sqlx::Error> {
        for (id, code) in ids.iter().zip(permission_codes.iter()) {
            sqlx::query!(
                r#"
                INSERT INTO t_role_permission (id, role_code, permission_code, version,
                                               created_at, created_by, updated_at, updated_by)
                VALUES ($1, $2, $3, 0, $4, $5, $4, $5)
                "#,
                id,
                role_code,
                code,
                when,
                created_by,
            )
            .execute(&mut *conn)
            .await?;
        }
        Ok(())
    }
}

/// `RoleWithUserCount` —— 角色列表的一行（`t_role` 全列 + 派生计数）。
///
/// 为什么不用 `TRole` + 单独的 count 查询：列表是「一屏看全部角色」，
/// 拆成 N+1 次计数会让面板打开变慢；`COUNT(...) FILTER` 一条 SQL 出全部。
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct RoleWithUserCount {
    pub id: i64,
    pub code: String,
    pub name: String,
    pub description: Option<String>,
    pub is_system: bool,
    pub is_active: bool,
    pub version: i32,
    pub created_at: chrono::NaiveDateTime,
    pub created_by: Option<i64>,
    pub updated_at: chrono::NaiveDateTime,
    pub updated_by: Option<i64>,
    pub deleted_at: Option<chrono::NaiveDateTime>,
    pub user_count: i64,
}

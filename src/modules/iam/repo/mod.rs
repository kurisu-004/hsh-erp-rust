pub mod sql;

pub mod model;

pub use model::{Menu, User, UserRole, WxIdentity, WxIdentityInsert};
pub use sql::user::{UserInsert, UserPartialUpdate};
pub use sql::user_role::{UserRoleInsert, UserRoleRow};

use async_trait::async_trait;
use chrono::NaiveDateTime;

use crate::modules::iam::rbac::model::TRole;
use crate::modules::iam::shelf::model::TShelf;

/// 命名约定（参考用户命名表）：
/// - 按主键查单条 → get_xxx_by_id / find_xxx_by_id
/// - 按条件查单条 → get_xxx_by_yyy
/// - 查列表（带分页/过滤） → list_xxx_with_*
/// - 计数 → count_xxx_by_*
/// - 插入 → create_xxx
/// - 更新 → update_xxx_*
/// - 删除（软删） → soft_delete_xxx
/// - 关系存在性 → has_xxx
/// - JOIN 过滤 → list_主实体_by_条件
#[cfg_attr(test, mockall::automock)]
#[async_trait]
pub trait IamRepoTrait: Send {
    // ── t_user（10）──
    async fn get_user_by_id(&mut self, id: i64) -> Result<Option<User>, sqlx::Error>;
    async fn get_user_by_username<'a>(
        &mut self,
        username_lower: &'a str,
    ) -> Result<Option<User>, sqlx::Error>;
    async fn list_users_with_filters<'a>(
        &mut self,
        username_like: Option<&'a str>,
        is_active: Option<bool>,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<User>, sqlx::Error>;
    async fn count_users_with_filters<'a>(
        &mut self,
        username_like: Option<&'a str>,
        is_active: Option<bool>,
    ) -> Result<i64, sqlx::Error>;
    async fn create_user(&mut self, user: &UserInsert) -> Result<(), sqlx::Error>;
    async fn update_user_partial<'a>(
        &mut self,
        id: i64,
        version: i32,
        args: &UserPartialUpdate<'a>,
    ) -> Result<u64, sqlx::Error>;
    async fn soft_delete_user(
        &mut self,
        id: i64,
        version: i32,
        when: NaiveDateTime,
        updated_by: Option<i64>,
    ) -> Result<u64, sqlx::Error>;
    async fn touch_user_last_login_at(
        &mut self,
        id: i64,
        when: NaiveDateTime,
    ) -> Result<(), sqlx::Error>;
    async fn increment_user_refresh_token_version(
        &mut self,
        id: i64,
        version: i32,
        when: NaiveDateTime,
        updated_by: Option<i64>,
    ) -> Result<u64, sqlx::Error>;
    async fn update_user_password_and_rotate<'a>(
        &mut self,
        id: i64,
        version: i32,
        password_hash: &'a str,
        when: NaiveDateTime,
        updated_by: Option<i64>,
    ) -> Result<u64, sqlx::Error>;

    // ── t_user_role（6）──
    async fn list_user_roles_by_user_id(
        &mut self,
        user_id: i64,
    ) -> Result<Vec<UserRoleRow>, sqlx::Error>;
    /// 批量版 `list_user_roles_by_user_id`：`list_users` 的 N+1 消解路径
    /// （一页 N 个账号 → 一次 `WHERE user_id = ANY($1)`）。
    async fn list_user_roles_by_user_ids(
        &mut self,
        user_ids: &[i64],
    ) -> Result<Vec<UserRoleRow>, sqlx::Error>;
    async fn get_user_role_by_id(&mut self, id: i64) -> Result<Option<UserRole>, sqlx::Error>;
    async fn has_user_role_with_scope<'a>(
        &mut self,
        user_id: i64,
        role: &'a str,
        scope_type: Option<&'a str>,
        scope_id: Option<i64>,
    ) -> Result<bool, sqlx::Error>;
    async fn create_user_role(&mut self, role_row: &UserRoleInsert) -> Result<(), sqlx::Error>;
    async fn soft_delete_user_role(
        &mut self,
        id: i64,
        version: i32,
        when: NaiveDateTime,
        updated_by: Option<i64>,
    ) -> Result<u64, sqlx::Error>;

    // ── t_menu（2，2026-10-14 第 2 条为 ADMIN 补）──
    async fn list_active_menus_by_roles<'a>(
        &mut self,
        roles: &'a [String],
    ) -> Result<Vec<Menu>, sqlx::Error>;

    /// 全部启用中的菜单，**不经角色过滤**。
    ///
    /// ADMIN 是元角色：它不进 `t_role_menu`（通配在代码里），登录态菜单树因此
    /// 不能走 `list_active_menus_by_roles(["ADMIN"])`（那会返回空树 ⇒ 管理员
    /// 登录后全站无入口）。`AccountService::menus_for_roles` 据此分支。
    async fn list_all_active_menus(&mut self) -> Result<Vec<Menu>, sqlx::Error>;

    // ── t_role_permission（1，2026-10-14 新增）──
    /// 按**全部角色 code**联查该用户的权限码集合（登录 / refresh 时装载进
    /// Redis session 缓存）。
    ///
    /// 走 `role_code = ANY($1)` 一条 SQL 而非逐角色查（N 个角色 N 次的写法
    /// 在登录路径上没必要）。
    async fn list_permission_codes_by_roles<'a>(
        &mut self,
        roles: &'a [String],
    ) -> Result<Vec<String>, sqlx::Error>;

    // ── t_role（4，2026-10-14 新增）──
    /// 按 code 查**未软删**的角色行（授予时判「存在且启用」用）。
    ///
    /// ⚠️ **只过滤 `deleted_at`、不过滤 `is_active`**：service 要把「不存在」
    ///    与「已停用」分成两条文案（现场排障时「角色不存在」和「角色被停用了」
    ///    是完全不同的问题），合并成一条 `Option` 就丢了这条信息。
    async fn get_active_role_by_code<'a>(
        &mut self,
        code: &'a str,
    ) -> Result<Option<TRole>, sqlx::Error>;

    /// 一批角色 code 里哪些是**启用中且未软删**的（登录侧「停用即降权」用）。
    ///
    /// 登录 / refresh / me 的角色解析先过这里，把停用角色的授权行整体滤掉：
    /// 该角色的权限 / 菜单 / 货架 scope 一并失效；停用为终态（当前无启用端点）。
    async fn list_active_role_codes<'a>(
        &mut self,
        codes: &'a [String],
    ) -> Result<Vec<String>, sqlx::Error>;

    /// 对 `t_role` 的 ADMIN 行取行锁（`SELECT … FOR UPDATE`），把自锁护栏的
    /// 「计数 → 判定 → 写」串行化（2026-10-14 新增）。
    ///
    /// 并发窗口：两个请求同时各撤销一个管理员时，各自数到 2 → 双双放行 → 归零。
    /// 行锁让第二个事务的计数在第一个提交之后执行（READ COMMITTED 逐语句快照）。
    async fn lock_admin_role_for_update(&mut self) -> Result<(), sqlx::Error>;

    /// active 且持有 ADMIN 角色的用户 id 列表（ADMIN 自锁护栏的唯一判据）。
    ///
    /// 与 `RbacRepo::list_active_user_ids_by_role_code` 同一条 SQL（用户域
    /// 与角色域两侧都要用，护栏阈值只写在用户域一处）。
    async fn list_active_admin_user_ids(&mut self) -> Result<Vec<i64>, sqlx::Error>;

    /// 该用户当前是否持有 ADMIN 角色（`t_user_role` 未软删行）。
    ///
    /// `deactivate_user` 的护栏先用它做快筛：**不是**管理员的账号走不到计数
    /// 查询（停用普通账号是最高频的运维动作，不该每次都数一遍管理员）。
    async fn has_active_admin_grants(&mut self, user_id: i64) -> Result<bool, sqlx::Error>;

    // ── t_shelf（1）──
    /// `t_shelf` 的唯一行结构是 `iam::shelf::model::TShelf`（含 `capacity`），
    /// 本方法直接委托货架子模块的 SQL 真源，本域不再维护第二份投影。
    async fn get_shelf_by_id(&mut self, id: i64) -> Result<Option<TShelf>, sqlx::Error>;

    // ── t_wx_identity（4，2026-10-10 自 wx 域搬入）──
    async fn get_wx_identity_by_corp_and_user<'a>(
        &mut self,
        corp_id: &'a str,
        wx_user_id: &'a str,
    ) -> Result<Option<WxIdentity>, sqlx::Error>;
    async fn get_wx_identity_by_user_id(
        &mut self,
        user_id: i64,
    ) -> Result<Vec<WxIdentity>, sqlx::Error>;
    async fn create_wx_identity(&mut self, identity: &WxIdentityInsert) -> Result<(), sqlx::Error>;
    async fn soft_delete_wx_identity(
        &mut self,
        id: i64,
        version: i32,
        when: NaiveDateTime,
        updated_by: Option<i64>,
    ) -> Result<u64, sqlx::Error>;
}

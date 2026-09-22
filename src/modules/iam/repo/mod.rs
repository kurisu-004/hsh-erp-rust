pub mod sql;

pub mod model;

pub use model::{Menu, Shelf, User, UserRole};
pub use sql::user::{UserInsert, UserPartialUpdate};
pub use sql::user_role::{UserRoleInsert, UserRoleRow};

use async_trait::async_trait;
use chrono::NaiveDateTime;

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

    // ── t_user_role（5）──
    async fn list_user_roles_by_user_id(
        &mut self,
        user_id: i64,
    ) -> Result<Vec<UserRoleRow>, sqlx::Error>;
    async fn get_user_role_by_id(
        &mut self,
        id: i64,
    ) -> Result<Option<UserRole>, sqlx::Error>;
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

    // ── t_menu（1）──
    async fn list_active_menus_by_roles<'a>(
        &mut self,
        roles: &'a [String],
    ) -> Result<Vec<Menu>, sqlx::Error>;

    // ── t_shelf（1）──
    async fn get_shelf_by_id(
        &mut self,
        id: i64,
    ) -> Result<Option<Shelf>, sqlx::Error>;
}
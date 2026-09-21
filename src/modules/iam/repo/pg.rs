//! `PgIamRepo`：借连接的 trait 实现
//!
//! `PgIamRepo<'a> { conn: &'a mut PgConnection }` —— 借 handler 开出的 `Transaction` 或
//! `pool.acquire()`，方法签名 = `IamRepo` trait，方法体 = `sql::XxxRepo::yyy(&mut *self.conn, ...)`
//! 一行委托。17 个方法同构实现。
//!
//! ## 生命周期短
//! `&mut PgConnection` 寿命 ≤ handler 单请求，不能 `Arc<PgIamRepo>` 存 `AppState`；
//! service 签名改为 `<R: IamRepo>(&self, repo: &mut R, ...)`，`PgIamRepo<'a>` 实例由
//! handler 在每请求栈上构造。

use async_trait::async_trait;
use chrono::NaiveDateTime;
use sqlx::PgConnection;

use super::sql::{MenuRepo, ShelfRepo, UserRepo, UserRoleRepo};
use super::IamRepo;

pub struct PgIamRepo<'a> {
    conn: &'a mut PgConnection,
}

impl<'a> PgIamRepo<'a> {
    pub fn new(conn: &'a mut PgConnection) -> Self {
        Self { conn }
    }
}

#[async_trait]
impl IamRepo for PgIamRepo<'_> {
    // ── t_user（10）── 一行委托 sql::UserRepo ────────────────────
    async fn get_by_id(
        &mut self,
        id: i64,
    ) -> Result<Option<crate::modules::iam::model::User>, sqlx::Error> {
        UserRepo::get_by_id(&mut *self.conn, id).await
    }

    async fn get_by_username<'a>(
        &mut self,
        username_lower: &'a str,
    ) -> Result<Option<crate::modules::iam::model::User>, sqlx::Error> {
        UserRepo::get_by_username(&mut *self.conn, username_lower).await
    }

    async fn list_with_filters<'a>(
        &mut self,
        username_like: Option<&'a str>,
        is_active: Option<bool>,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<crate::modules::iam::model::User>, sqlx::Error> {
        UserRepo::list_with_filters(&mut *self.conn, username_like, is_active, limit, offset).await
    }

    async fn count_with_filters<'a>(
        &mut self,
        username_like: Option<&'a str>,
        is_active: Option<bool>,
    ) -> Result<i64, sqlx::Error> {
        UserRepo::count_with_filters(&mut *self.conn, username_like, is_active).await
    }

    async fn create(
        &mut self,
        user: &crate::modules::iam::repo::UserInsert,
    ) -> Result<(), sqlx::Error> {
        UserRepo::create(&mut *self.conn, user).await
    }

    #[allow(clippy::too_many_arguments)]
    async fn update_partial<'a>(
        &mut self,
        id: i64,
        version: i32,
        full_name: Option<&'a str>,
        set_phone: bool,
        phone: Option<&'a str>,
        password_hash: Option<&'a str>,
        is_active: Option<bool>,
        when: NaiveDateTime,
        updated_by: Option<i64>,
    ) -> Result<u64, sqlx::Error> {
        UserRepo::update_partial(
            &mut *self.conn,
            id,
            version,
            full_name,
            set_phone,
            phone,
            password_hash,
            is_active,
            when,
            updated_by,
        )
        .await
    }

    async fn soft_delete(
        &mut self,
        id: i64,
        version: i32,
        when: NaiveDateTime,
        updated_by: Option<i64>,
    ) -> Result<u64, sqlx::Error> {
        UserRepo::soft_delete(&mut *self.conn, id, version, when, updated_by).await
    }

    async fn touch_login(&mut self, id: i64, when: NaiveDateTime) -> Result<(), sqlx::Error> {
        UserRepo::touch_login(&mut *self.conn, id, when).await
    }

    async fn increment_refresh_token_version(
        &mut self,
        id: i64,
        version: i32,
        when: NaiveDateTime,
        updated_by: Option<i64>,
    ) -> Result<u64, sqlx::Error> {
        UserRepo::increment_refresh_token_version(&mut *self.conn, id, version, when, updated_by)
            .await
    }

    async fn update_password_and_rotate<'a>(
        &mut self,
        id: i64,
        version: i32,
        password_hash: &'a str,
        when: NaiveDateTime,
        updated_by: Option<i64>,
    ) -> Result<u64, sqlx::Error> {
        UserRepo::update_password_and_rotate(
            &mut *self.conn,
            id,
            version,
            password_hash,
            when,
            updated_by,
        )
        .await
    }

    // ── t_user_role（5）── 一行委托 sql::UserRoleRepo ────────────
    async fn list_by_user(
        &mut self,
        user_id: i64,
    ) -> Result<Vec<crate::modules::iam::repo::UserRoleRow>, sqlx::Error> {
        UserRoleRepo::list_by_user(&mut *self.conn, user_id).await
    }

    async fn role_get_by_id(
        &mut self,
        id: i64,
    ) -> Result<Option<crate::modules::iam::model::UserRole>, sqlx::Error> {
        UserRoleRepo::get_by_id(&mut *self.conn, id).await
    }

    async fn exists_same_scope<'a>(
        &mut self,
        user_id: i64,
        role: &'a str,
        scope_type: Option<&'a str>,
        scope_id: Option<i64>,
    ) -> Result<bool, sqlx::Error> {
        UserRoleRepo::exists_same_scope(&mut *self.conn, user_id, role, scope_type, scope_id).await
    }

    async fn role_create(
        &mut self,
        role_row: &crate::modules::iam::repo::UserRoleInsert,
    ) -> Result<(), sqlx::Error> {
        UserRoleRepo::create(&mut *self.conn, role_row).await
    }

    async fn role_soft_delete(
        &mut self,
        id: i64,
        version: i32,
        when: NaiveDateTime,
        updated_by: Option<i64>,
    ) -> Result<u64, sqlx::Error> {
        UserRoleRepo::soft_delete(&mut *self.conn, id, version, when, updated_by).await
    }

    // ── t_menu（1）──
    async fn list_active_for_roles<'a>(
        &mut self,
        roles: &'a [String],
    ) -> Result<Vec<crate::modules::iam::model::Menu>, sqlx::Error> {
        MenuRepo::list_active_for_roles(&mut *self.conn, roles).await
    }

    // ── t_shelf（1）──
    async fn shelf_get_by_id(
        &mut self,
        id: i64,
    ) -> Result<Option<crate::modules::iam::model::Shelf>, sqlx::Error> {
        ShelfRepo::get_by_id(&mut *self.conn, id).await
    }
}

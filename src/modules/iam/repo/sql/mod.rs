//! iam 域 SQL 真源（按实体拆 5 子文件）+ 单一 `impl IamRepoTrait for &mut PgConnection` 块
//!
//! ## 结构
//! - `user.rs`        — `t_user` 10 个 SQL fns + `UserInsert` / `UserPartialUpdate<'_>` 入参
//! - `user_role.rs`   — `t_user_role` 6 个 SQL fns + `UserRoleInsert` / `UserRoleRow`
//! - `menu.rs`        — `t_menu` 1 个 SQL fn
//! - `wx_identity.rs` — `t_wx_identity` 4 个 SQL fns（2026-10-10 自 wx 域搬入）
//! - `mod.rs`（本文件）— 声明子模块 + **单一** `impl IamRepoTrait for &mut PgConnection` 块
//!   （覆盖全部 22 方法，按实体分组；call 各子文件 free fn）
//!
//! `t_shelf` 没有子文件：`t_shelf` 是货架子模块（`iam::shelf`）的实体，本域只作为
//! SHELF_ACCOUNT 角色 scope 校验的**消费方**读它，故 `get_shelf_by_id` 在下面的 impl
//! 块里直接委托 `iam::shelf::repo::ShelfRepo::get_by_id`，`t_shelf` 全仓只留一份行
//! 结构与一份 SQL（`iam::shelf::model::TShelf` + `iam::shelf::repo::sql`）。
//!
//! ## 为什么不是 4 个分散 impl 块
//! Rust coherence 规则：同 crate 内同一 trait 对同一类型至多一个 impl 块（auto trait
//! 例外：Send/Sync/Unpin）。本想每文件就地 impl，编译报 E0119 冲突——故统一收到本文件，
//! SQL 子文件仅放「真源」（free fn + 入参 DTO），调用点 `super::user::xxx`。
//!
//! ## SQL 真源 → trait 方法的薄委托
//! impl 块里每方法只一行 `super::xxx::yyy(&mut **self, ...).await`，无业务分支。
//!
//! ### 为什么方法体一律是 `&mut **self`（2026-10-10 复核）
//! 本 impl 是 `impl IamRepoTrait for &mut PgConnection`，方法签名里 `self` 的类型是
//! `&mut &mut PgConnection`。`*self` 是「内层那个 `&mut PgConnection` 本身」，按值
//! 用它等于把引用 move 出借来的内容（编译报 cannot move out of borrowed content）；
//! `&mut **self` 才是 **reborrow** —— 重新借一层 `&mut PgConnection` 交给子文件
//! free fn（其首参与之一致），`self` 本身保持可用。
//! **22 个方法 = 22 处 reborrow**，与本文件 impl 块的方法数、`repo/mod.rs` 里 trait
//! 的方法数三者恒等。`repo/mod.rs` 只承担 trait 声明与命名约定，不承载这个实现细节，
//! 故说明就地留在本文件。

pub mod menu;
pub mod user;
pub mod user_role;
pub mod wx_identity;

use async_trait::async_trait;
use chrono::NaiveDateTime;
use sqlx::PgConnection;

use crate::modules::iam::repo::model::{Menu, User, UserRole, WxIdentity};
use crate::modules::iam::repo::{
    IamRepoTrait, UserInsert, UserPartialUpdate, UserRoleInsert, UserRoleRow, WxIdentityInsert,
};
use crate::modules::iam::shelf::model::TShelf;

/// 统一 `IamRepoTrait for &mut PgConnection` 实现（按实体分组，零业务逻辑）
///
/// Rust 同一类型 + trait 至多一个 impl 块，故本块收在此处；子文件 sql/* 仅放 SQL 真源。
#[async_trait]
impl IamRepoTrait for &mut PgConnection {
    // ── t_user（10）──
    async fn get_user_by_id(&mut self, id: i64) -> Result<Option<User>, sqlx::Error> {
        user::get_user_by_id(&mut **self, id).await
    }
    async fn get_user_by_username<'b>(
        &mut self,
        username_lower: &'b str,
    ) -> Result<Option<User>, sqlx::Error> {
        user::get_user_by_username(&mut **self, username_lower).await
    }
    async fn list_users_with_filters<'b>(
        &mut self,
        username_like: Option<&'b str>,
        is_active: Option<bool>,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<User>, sqlx::Error> {
        user::list_users_with_filters(&mut **self, username_like, is_active, limit, offset).await
    }
    async fn count_users_with_filters<'b>(
        &mut self,
        username_like: Option<&'b str>,
        is_active: Option<bool>,
    ) -> Result<i64, sqlx::Error> {
        user::count_users_with_filters(&mut **self, username_like, is_active).await
    }
    async fn create_user(&mut self, user: &UserInsert) -> Result<(), sqlx::Error> {
        user::create_user(&mut **self, user).await
    }
    async fn update_user_partial<'b>(
        &mut self,
        id: i64,
        version: i32,
        args: &UserPartialUpdate<'b>,
    ) -> Result<u64, sqlx::Error> {
        user::update_user_partial(&mut **self, id, version, args).await
    }
    async fn soft_delete_user(
        &mut self,
        id: i64,
        version: i32,
        when: NaiveDateTime,
        updated_by: Option<i64>,
    ) -> Result<u64, sqlx::Error> {
        user::soft_delete_user(&mut **self, id, version, when, updated_by).await
    }
    async fn touch_user_last_login_at(
        &mut self,
        id: i64,
        when: NaiveDateTime,
    ) -> Result<(), sqlx::Error> {
        user::touch_user_last_login_at(&mut **self, id, when).await
    }
    async fn increment_user_refresh_token_version(
        &mut self,
        id: i64,
        version: i32,
        when: NaiveDateTime,
        updated_by: Option<i64>,
    ) -> Result<u64, sqlx::Error> {
        user::increment_user_refresh_token_version(&mut **self, id, version, when, updated_by).await
    }
    async fn update_user_password_and_rotate<'b>(
        &mut self,
        id: i64,
        version: i32,
        password_hash: &'b str,
        when: NaiveDateTime,
        updated_by: Option<i64>,
    ) -> Result<u64, sqlx::Error> {
        user::update_user_password_and_rotate(
            &mut **self,
            id,
            version,
            password_hash,
            when,
            updated_by,
        )
        .await
    }

    // ── t_user_role（6）──
    async fn list_user_roles_by_user_id(
        &mut self,
        user_id: i64,
    ) -> Result<Vec<UserRoleRow>, sqlx::Error> {
        user_role::list_user_roles_by_user_id(&mut **self, user_id).await
    }
    async fn list_user_roles_by_user_ids(
        &mut self,
        user_ids: &[i64],
    ) -> Result<Vec<UserRoleRow>, sqlx::Error> {
        user_role::list_user_roles_by_user_ids(&mut **self, user_ids).await
    }
    async fn get_user_role_by_id(&mut self, id: i64) -> Result<Option<UserRole>, sqlx::Error> {
        user_role::get_user_role_by_id(&mut **self, id).await
    }
    async fn has_user_role_with_scope<'b>(
        &mut self,
        user_id: i64,
        role: &'b str,
        scope_type: Option<&'b str>,
        scope_id: Option<i64>,
    ) -> Result<bool, sqlx::Error> {
        user_role::has_user_role_with_scope(&mut **self, user_id, role, scope_type, scope_id).await
    }
    async fn create_user_role(&mut self, role_row: &UserRoleInsert) -> Result<(), sqlx::Error> {
        user_role::create_user_role(&mut **self, role_row).await
    }
    async fn soft_delete_user_role(
        &mut self,
        id: i64,
        version: i32,
        when: NaiveDateTime,
        updated_by: Option<i64>,
    ) -> Result<u64, sqlx::Error> {
        user_role::soft_delete_user_role(&mut **self, id, version, when, updated_by).await
    }

    // ── t_menu（1）──
    async fn list_active_menus_by_roles<'b>(
        &mut self,
        roles: &'b [String],
    ) -> Result<Vec<Menu>, sqlx::Error> {
        menu::list_active_menus_by_roles(&mut **self, roles).await
    }

    // ── t_shelf（1）──
    async fn get_shelf_by_id(&mut self, id: i64) -> Result<Option<TShelf>, sqlx::Error> {
        crate::modules::iam::shelf::repo::ShelfRepo::get_by_id(&mut **self, id).await
    }

    // ── t_wx_identity（4，2026-10-10 自 wx 域搬入）──
    async fn get_wx_identity_by_corp_and_user<'b>(
        &mut self,
        corp_id: &'b str,
        wx_user_id: &'b str,
    ) -> Result<Option<WxIdentity>, sqlx::Error> {
        wx_identity::get_wx_identity_by_corp_and_user(&mut **self, corp_id, wx_user_id).await
    }
    async fn get_wx_identity_by_user_id(
        &mut self,
        user_id: i64,
    ) -> Result<Vec<WxIdentity>, sqlx::Error> {
        wx_identity::get_wx_identity_by_user_id(&mut **self, user_id).await
    }
    async fn create_wx_identity(&mut self, identity: &WxIdentityInsert) -> Result<(), sqlx::Error> {
        wx_identity::create_wx_identity(&mut **self, identity).await
    }
    async fn soft_delete_wx_identity(
        &mut self,
        id: i64,
        version: i32,
        when: NaiveDateTime,
        updated_by: Option<i64>,
    ) -> Result<u64, sqlx::Error> {
        wx_identity::soft_delete_wx_identity(&mut **self, id, version, when, updated_by).await
    }
}

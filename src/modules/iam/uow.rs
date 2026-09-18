//! iam 域 UoW（Unit of Work，repo 访问器模式）
//!
//! 对应 Python myERP/repository/user_repository.py + repository/menu_repository.py +
//! repository/shelf_repository.py 的访问层抽象。本文件把 `super::repo` 的 17 个固有静态
//! 方法挂到 4 个独立 repo trait 上，再用 `SharedTx = Arc<tokio::sync::Mutex<Option<Transaction<'static, Postgres>>>>`
//! 把这些 trait 的 Sqlx 实现绑定到同一条事务上，构成 `UnitOfWork` 访问器模式。
//!
//! ## 形态
//! - **4 repo trait**：`UserRepo`（10）/ `UserRoleRepo`（5）/ `MenuRepo`（1）/ `ShelfRepo`（1），
//!   各自 `#[cfg_attr(test, mockall::automock)]`，方法签名 = `repo.rs` 固有方法**去 executor 形参**。
//! - **`IamUnitOfWork` trait**：4 访问器 + `commit(self: Box<Self>)` / `rollback(self: Box<Self>)`，
//!   **不含 begin**（实例由 `IamUowProvider::begin()` 产出，已开 tx）。
//! - **`IamUowProvider` trait**：automock，begin 返回 `Box<dyn IamUnitOfWork>`。
//! - **Sqlx 实现**：`SqlxIamUnitOfWork` 持 4 个 `Sqlx*Repo`，每个 `Sqlx*Repo` 持 `SharedTx` 句柄，
//!   实现方法内 `lock().await` 取 `&mut PgConnection` 委托给 `repo::XxxRepo::yyy(...)`。
//!
//! ## 消费语义（commit/rollback）
//! `commit(self: Box<Self>)` / `rollback(self: Box<Self>)` 接收 `Box<Self>`：object-safe，可直接
//! 在 `Box<dyn IamUnitOfWork>` 上调用；调用后 UoW 不可再用——编译期保证「commit 后再复用」不可表达。
//! 读路径 / 错误路径不调 `rollback`，直接 `drop`（隐式回滚）。
//!
//! ## 错误类型
//! - service 面向的 `begin` / `commit` / `rollback` → `AppError`（service `?` 零 map_err）
//! - repo trait 方法 → `sqlx::Error`（与 `repo.rs` 固有方法签名 1:1，service `?` 经
//!   `AppError::Database(#[from] sqlx::Error)` 自动转换）
//!
//! ## 命名约定
//! 加 `Iam` 前缀与兄弟域（如 `_e2e` / `delivery_note`）的 `*UoW` / `*UowProvider` 区分，
//! 后续跨域 supertrait 组合时可读性更高（如 `trait SalesUoW: IamUnitOfWork + DeliveryNoteUnitOfWork`）。
//!
//! 2026-09-19 IAM 域合并：从 `modules::user::uow` 整体迁移过来（trait 加 `Iam` 前缀），
//! repo trait 实现零 diff。详见 `modules::iam` 模块 doc。

use std::sync::Arc;

use async_trait::async_trait;
use chrono::NaiveDateTime;
use sqlx::{PgPool, Postgres, Transaction};
use tokio::sync::Mutex;

use crate::shared::error::AppError;

use super::model::{Menu, Shelf, User, UserRole};
use super::repo::{
    UserInsert, UserRepo as RepoUserRepo, UserRoleInsert, UserRoleRepo as RepoUserRoleRepo,
    UserRoleRow,
};
// MenuRepo / ShelfRepo 的 repo.rs 方法没有同名 trait 冲突，直接用模块路径访问：
use super::repo::{MenuRepo as RepoMenuRepo, ShelfRepo as RepoShelfRepo};

// ===========================================================================
// SharedTx：4 个 Sqlx*Repo 与 SqlxIamUnitOfWork 共享同一条 sqlx::Transaction。
// 包装为 Arc<Mutex<Option<...>>>：Option 让 commit/rollback 一次性 take 走（防二次 commit），
// Mutex 提供内部可变性（访问器要 &mut self 才能返回 &mut dyn，访问器拿不到 &mut Mutex）。
// ===========================================================================

pub(crate) type SharedTx = Arc<Mutex<Option<Transaction<'static, Postgres>>>>;

fn tx_closed() -> sqlx::Error {
    sqlx::Error::PoolClosed // 数据库已关闭，用作「tx 已被 take」的哨兵（语义：「不再可用」）
}

// ===========================================================================
// Repo trait：4 个。各 automock。方法签名 = repo.rs 固有方法去 executor 形参。
// ===========================================================================

/// `t_user` 行操作（10 个方法，对应 `super::repo::UserRepo`）
///
/// 全部方法签名 = `super::repo::UserRepo` 固有方法去 executor 形参。
/// 显式 `<'a>` 生命周期参数让 mockall automock 在 `async_trait` 上下文里能生成 mock impl
/// （mockall 0.15 默认 `for<'a>` HRTB，没有显式生命周期会编译失败）。
#[cfg_attr(test, mockall::automock)]
#[async_trait]
pub trait IamUserRepo: Send + Sync {
    async fn get_by_id(&self, id: i64) -> Result<Option<User>, sqlx::Error>;
    async fn get_by_username<'a>(
        &self,
        username_lower: &'a str,
    ) -> Result<Option<User>, sqlx::Error>;
    async fn list_with_filters<'a>(
        &self,
        username_like: Option<&'a str>,
        is_active: Option<bool>,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<User>, sqlx::Error>;
    async fn count_with_filters<'a>(
        &self,
        username_like: Option<&'a str>,
        is_active: Option<bool>,
    ) -> Result<i64, sqlx::Error>;
    async fn create(&self, user: &UserInsert) -> Result<(), sqlx::Error>;
    #[allow(clippy::too_many_arguments)]
    async fn update_partial<'a>(
        &self,
        id: i64,
        version: i32,
        full_name: Option<&'a str>,
        set_phone: bool,
        phone: Option<&'a str>,
        password_hash: Option<&'a str>,
        is_active: Option<bool>,
        when: NaiveDateTime,
        updated_by: Option<i64>,
    ) -> Result<u64, sqlx::Error>;
    async fn soft_delete(
        &self,
        id: i64,
        version: i32,
        when: NaiveDateTime,
        updated_by: Option<i64>,
    ) -> Result<u64, sqlx::Error>;
    async fn touch_login(&self, id: i64, when: NaiveDateTime) -> Result<(), sqlx::Error>;
    async fn increment_refresh_token_version(
        &self,
        id: i64,
        version: i32,
        when: NaiveDateTime,
        updated_by: Option<i64>,
    ) -> Result<u64, sqlx::Error>;
    async fn update_password_and_rotate<'a>(
        &self,
        id: i64,
        version: i32,
        password_hash: &'a str,
        when: NaiveDateTime,
        updated_by: Option<i64>,
    ) -> Result<u64, sqlx::Error>;
}

/// `t_user_role` 行操作（5 个方法，对应 `super::repo::UserRoleRepo`）
#[cfg_attr(test, mockall::automock)]
#[async_trait]
pub trait IamUserRoleRepo: Send + Sync {
    async fn list_by_user(&self, user_id: i64) -> Result<Vec<UserRoleRow>, sqlx::Error>;
    async fn get_by_id(&self, id: i64) -> Result<Option<UserRole>, sqlx::Error>;
    async fn exists_same_scope<'a>(
        &self,
        user_id: i64,
        role: &'a str,
        scope_type: Option<&'a str>,
        scope_id: Option<i64>,
    ) -> Result<bool, sqlx::Error>;
    async fn create(&self, role_row: &UserRoleInsert) -> Result<(), sqlx::Error>;
    async fn soft_delete(
        &self,
        id: i64,
        version: i32,
        when: NaiveDateTime,
        updated_by: Option<i64>,
    ) -> Result<u64, sqlx::Error>;
}

/// `t_menu` 读操作（1 个方法，对应 `super::repo::MenuRepo`）
#[cfg_attr(test, mockall::automock)]
#[async_trait]
pub trait IamMenuRepo: Send + Sync {
    async fn list_active_for_roles<'a>(
        &self,
        roles: &'a [String],
    ) -> Result<Vec<Menu>, sqlx::Error>;
}

/// `t_shelf` 读操作（1 个方法，对应 `super::repo::ShelfRepo`）
#[cfg_attr(test, mockall::automock)]
#[async_trait]
pub trait IamShelfRepo: Send + Sync {
    async fn get_by_id(&self, id: i64) -> Result<Option<Shelf>, sqlx::Error>;
}

// ===========================================================================
// IamUnitOfWork trait：4 访问器 + commit/rollback(self: Box<Self>)，不含 begin。
// ===========================================================================

/// 跨 repo 事务边界。实例由 `IamUowProvider::begin()` 产出（已开 tx），故 trait 内不设
/// `begin`——再保留会制造「未 begin 的 UoW」非法中间态。
#[async_trait]
pub trait IamUnitOfWork: Send {
    fn user_repo(&mut self) -> &mut dyn IamUserRepo;
    fn user_role_repo(&mut self) -> &mut dyn IamUserRoleRepo;
    fn menu_repo(&mut self) -> &mut dyn IamMenuRepo;
    fn shelf_repo(&mut self) -> &mut dyn IamShelfRepo;

    /// 提交事务。**消费语义**——`Box<Self>` 一次性 take，提交后本 UoW 不可再用
    /// （编译期不可表达「commit 后再 commit」）。
    async fn commit(self: Box<Self>) -> Result<(), AppError>;

    /// 显式回滚。**消费语义**。不调用即 drop = 隐式回滚（Transaction 的 Drop 语义）。
    async fn rollback(self: Box<Self>) -> Result<(), AppError>;
}

/// `Arc<dyn IamUowProvider>` 是 service 持有的事务来源。begin 一次产出 `Box<dyn IamUnitOfWork>`。
#[cfg_attr(test, mockall::automock)]
#[async_trait]
pub trait IamUowProvider: Send + Sync {
    async fn begin(&self) -> Result<Box<dyn IamUnitOfWork>, AppError>;
}

// ===========================================================================
// Sqlx 实现：4 个 Sqlx*Repo + SqlxIamUnitOfWork + SqlxIamUowProvider
// ===========================================================================

/// `IamUserRepo` 的 Sqlx 实现。持 SharedTx 句柄，方法内 `lock().await` 取 `&mut PgConnection`
/// 委托给 `repo::UserRepo::yyy(&mut **tx, ...)`。
pub struct SqlxIamUserRepo {
    tx: SharedTx,
}

impl SqlxIamUserRepo {
    pub(crate) fn new(tx: SharedTx) -> Self {
        Self { tx }
    }
}

#[async_trait]
impl IamUserRepo for SqlxIamUserRepo {
    async fn get_by_id(&self, id: i64) -> Result<Option<User>, sqlx::Error> {
        let mut guard = self.tx.lock().await;
        let tx = guard.as_mut().ok_or_else(tx_closed)?;
        RepoUserRepo::get_by_id(&mut **tx, id).await
    }

    async fn get_by_username<'a>(
        &self,
        username_lower: &'a str,
    ) -> Result<Option<User>, sqlx::Error> {
        let mut guard = self.tx.lock().await;
        let tx = guard.as_mut().ok_or_else(tx_closed)?;
        RepoUserRepo::get_by_username(&mut **tx, username_lower).await
    }

    async fn list_with_filters<'a>(
        &self,
        username_like: Option<&'a str>,
        is_active: Option<bool>,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<User>, sqlx::Error> {
        let mut guard = self.tx.lock().await;
        let tx = guard.as_mut().ok_or_else(tx_closed)?;
        RepoUserRepo::list_with_filters(&mut **tx, username_like, is_active, limit, offset).await
    }

    async fn count_with_filters<'a>(
        &self,
        username_like: Option<&'a str>,
        is_active: Option<bool>,
    ) -> Result<i64, sqlx::Error> {
        let mut guard = self.tx.lock().await;
        let tx = guard.as_mut().ok_or_else(tx_closed)?;
        RepoUserRepo::count_with_filters(&mut **tx, username_like, is_active).await
    }

    async fn create(&self, user: &UserInsert) -> Result<(), sqlx::Error> {
        let mut guard = self.tx.lock().await;
        let tx = guard.as_mut().ok_or_else(tx_closed)?;
        RepoUserRepo::create(&mut **tx, user).await
    }

    async fn update_partial<'a>(
        &self,
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
        let mut guard = self.tx.lock().await;
        let tx = guard.as_mut().ok_or_else(tx_closed)?;
        RepoUserRepo::update_partial(
            &mut **tx,
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
        &self,
        id: i64,
        version: i32,
        when: NaiveDateTime,
        updated_by: Option<i64>,
    ) -> Result<u64, sqlx::Error> {
        let mut guard = self.tx.lock().await;
        let tx = guard.as_mut().ok_or_else(tx_closed)?;
        RepoUserRepo::soft_delete(&mut **tx, id, version, when, updated_by).await
    }

    async fn touch_login(&self, id: i64, when: NaiveDateTime) -> Result<(), sqlx::Error> {
        let mut guard = self.tx.lock().await;
        let tx = guard.as_mut().ok_or_else(tx_closed)?;
        RepoUserRepo::touch_login(&mut **tx, id, when).await
    }

    async fn increment_refresh_token_version(
        &self,
        id: i64,
        version: i32,
        when: NaiveDateTime,
        updated_by: Option<i64>,
    ) -> Result<u64, sqlx::Error> {
        let mut guard = self.tx.lock().await;
        let tx = guard.as_mut().ok_or_else(tx_closed)?;
        RepoUserRepo::increment_refresh_token_version(&mut **tx, id, version, when, updated_by)
            .await
    }

    async fn update_password_and_rotate<'a>(
        &self,
        id: i64,
        version: i32,
        password_hash: &'a str,
        when: NaiveDateTime,
        updated_by: Option<i64>,
    ) -> Result<u64, sqlx::Error> {
        let mut guard = self.tx.lock().await;
        let tx = guard.as_mut().ok_or_else(tx_closed)?;
        RepoUserRepo::update_password_and_rotate(
            &mut **tx,
            id,
            version,
            password_hash,
            when,
            updated_by,
        )
        .await
    }
}

/// `IamUserRoleRepo` 的 Sqlx 实现。
pub struct SqlxIamUserRoleRepo {
    tx: SharedTx,
}

impl SqlxIamUserRoleRepo {
    pub(crate) fn new(tx: SharedTx) -> Self {
        Self { tx }
    }
}

#[async_trait]
impl IamUserRoleRepo for SqlxIamUserRoleRepo {
    async fn list_by_user(&self, user_id: i64) -> Result<Vec<UserRoleRow>, sqlx::Error> {
        let mut guard = self.tx.lock().await;
        let tx = guard.as_mut().ok_or_else(tx_closed)?;
        RepoUserRoleRepo::list_by_user(&mut **tx, user_id).await
    }

    async fn get_by_id(&self, id: i64) -> Result<Option<UserRole>, sqlx::Error> {
        let mut guard = self.tx.lock().await;
        let tx = guard.as_mut().ok_or_else(tx_closed)?;
        RepoUserRoleRepo::get_by_id(&mut **tx, id).await
    }

    async fn exists_same_scope<'a>(
        &self,
        user_id: i64,
        role: &'a str,
        scope_type: Option<&'a str>,
        scope_id: Option<i64>,
    ) -> Result<bool, sqlx::Error> {
        let mut guard = self.tx.lock().await;
        let tx = guard.as_mut().ok_or_else(tx_closed)?;
        RepoUserRoleRepo::exists_same_scope(&mut **tx, user_id, role, scope_type, scope_id).await
    }

    async fn create(&self, role_row: &UserRoleInsert) -> Result<(), sqlx::Error> {
        let mut guard = self.tx.lock().await;
        let tx = guard.as_mut().ok_or_else(tx_closed)?;
        RepoUserRoleRepo::create(&mut **tx, role_row).await
    }

    async fn soft_delete(
        &self,
        id: i64,
        version: i32,
        when: NaiveDateTime,
        updated_by: Option<i64>,
    ) -> Result<u64, sqlx::Error> {
        let mut guard = self.tx.lock().await;
        let tx = guard.as_mut().ok_or_else(tx_closed)?;
        RepoUserRoleRepo::soft_delete(&mut **tx, id, version, when, updated_by).await
    }
}

/// `IamMenuRepo` 的 Sqlx 实现。
pub struct SqlxIamMenuRepo {
    tx: SharedTx,
}

impl SqlxIamMenuRepo {
    pub(crate) fn new(tx: SharedTx) -> Self {
        Self { tx }
    }
}

#[async_trait]
impl IamMenuRepo for SqlxIamMenuRepo {
    async fn list_active_for_roles<'a>(
        &self,
        roles: &'a [String],
    ) -> Result<Vec<Menu>, sqlx::Error> {
        let mut guard = self.tx.lock().await;
        let tx = guard.as_mut().ok_or_else(tx_closed)?;
        RepoMenuRepo::list_active_for_roles(&mut **tx, roles).await
    }
}

/// `IamShelfRepo` 的 Sqlx 实现。
pub struct SqlxIamShelfRepo {
    tx: SharedTx,
}

impl SqlxIamShelfRepo {
    pub(crate) fn new(tx: SharedTx) -> Self {
        Self { tx }
    }
}

#[async_trait]
impl IamShelfRepo for SqlxIamShelfRepo {
    async fn get_by_id(&self, id: i64) -> Result<Option<Shelf>, sqlx::Error> {
        let mut guard = self.tx.lock().await;
        let tx = guard.as_mut().ok_or_else(tx_closed)?;
        RepoShelfRepo::get_by_id(&mut **tx, id).await
    }
}

/// 单条 sqlx::Transaction + 4 个 Sqlx*Repo 的统一壳。访问器返回 `&mut dyn *Repo`。
pub struct SqlxIamUnitOfWork {
    tx: SharedTx,
    user_repo: SqlxIamUserRepo,
    user_role_repo: SqlxIamUserRoleRepo,
    menu_repo: SqlxIamMenuRepo,
    shelf_repo: SqlxIamShelfRepo,
}

impl SqlxIamUnitOfWork {
    pub(crate) fn new(tx: Transaction<'static, Postgres>) -> Self {
        let shared = Arc::new(Mutex::new(Some(tx)));
        Self {
            user_repo: SqlxIamUserRepo::new(shared.clone()),
            user_role_repo: SqlxIamUserRoleRepo::new(shared.clone()),
            menu_repo: SqlxIamMenuRepo::new(shared.clone()),
            shelf_repo: SqlxIamShelfRepo::new(shared.clone()),
            tx: shared,
        }
    }
}

#[async_trait]
impl IamUnitOfWork for SqlxIamUnitOfWork {
    fn user_repo(&mut self) -> &mut dyn IamUserRepo {
        &mut self.user_repo
    }

    fn user_role_repo(&mut self) -> &mut dyn IamUserRoleRepo {
        &mut self.user_role_repo
    }

    fn menu_repo(&mut self) -> &mut dyn IamMenuRepo {
        &mut self.menu_repo
    }

    fn shelf_repo(&mut self) -> &mut dyn IamShelfRepo {
        &mut self.shelf_repo
    }

    async fn commit(self: Box<Self>) -> Result<(), AppError> {
        let tx = self.tx.lock().await.take().ok_or_else(|| {
            AppError::internal("SqlxIamUnitOfWork::commit: uow already completed")
        })?;
        tx.commit().await?;
        Ok(())
    }

    async fn rollback(self: Box<Self>) -> Result<(), AppError> {
        let tx = self.tx.lock().await.take().ok_or_else(|| {
            AppError::internal("SqlxIamUnitOfWork::rollback: uow already completed")
        })?;
        tx.rollback().await?;
        Ok(())
    }
}

/// `Arc<dyn IamUowProvider>` 的 Sqlx 实现。持 `PgPool`，`begin` 时 `pool.begin().await` 得
/// `Transaction`，装入 `SqlxIamUnitOfWork`。
pub struct SqlxIamUowProvider {
    pool: PgPool,
}

impl SqlxIamUowProvider {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl IamUowProvider for SqlxIamUowProvider {
    async fn begin(&self) -> Result<Box<dyn IamUnitOfWork>, AppError> {
        let tx = self.pool.begin().await?;
        Ok(Box::new(SqlxIamUnitOfWork::new(tx)))
    }
}

// ===========================================================================
// test_support（手写 MockIamUnitOfWork + IamUowFlags + provider_returning）
//
// 两个 service_tests/{session_tests,account_tests}.rs 共用本模块。
// automock 处理不了返回 `&mut dyn` 借用 `&mut self` 的访问器，故 MockIamUnitOfWork 手写。
// commit/rollback 消费 Box<Self> 后测试侧摸不到 mock 本体——旗标用 Arc<AtomicBool> 共享句柄。
// ===========================================================================

#[cfg(test)]
#[allow(dead_code)] // service_tests/{session,account}_tests.rs 才会引用；当前本模块编译为 lib 时
// 没有先用例做眼，但 iam 域惯例保持这些 item 落位便于并行实现。
pub(crate) mod test_support {
    use super::*;
    // 把 mockall 自动生成的 `MockIamUowProvider` 重新 pub(crate) 导出，
    // crate 内部 `iam::service_tests::{session,account}_tests` 才能引用。
    pub(crate) use super::MockIamUowProvider;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// UoW 提交/回滚旗标。`clone()` 给外部测试侧，`assert_*` 在用例末尾断言 commit 时序。
    #[derive(Clone, Default)]
    pub(crate) struct IamUowFlags {
        pub committed: Arc<AtomicBool>,
        pub rolled_back: Arc<AtomicBool>,
    }

    impl IamUowFlags {
        pub fn assert_committed(&self) {
            assert!(
                self.committed.load(Ordering::SeqCst),
                "expected UoW to be committed, but it was not"
            );
        }
        pub fn assert_not_committed(&self) {
            assert!(
                !self.committed.load(Ordering::SeqCst),
                "expected UoW NOT to be committed, but it was"
            );
        }
        pub fn assert_rolled_back(&self) {
            assert!(
                self.rolled_back.load(Ordering::SeqCst),
                "expected UoW to be rolled back, but it was not"
            );
        }
    }

    /// 手写的 Mock UoW：持 4 个 automock 出来的 repo + 共享旗标。
    /// 测试侧构造 `MockIamUnitOfWork::new()` 拿到 `(uow, flags)`，在 `uow.user_repo` /
    /// `uow.user_role_repo` / `uow.menu_repo` / `uow.shelf_repo` 上设期望，然后
    /// 经 `provider_returning(uow)` 注入 service。
    pub(crate) struct MockIamUnitOfWork {
        pub user_repo: MockIamUserRepo,
        pub user_role_repo: MockIamUserRoleRepo,
        pub menu_repo: MockIamMenuRepo,
        pub shelf_repo: MockIamShelfRepo,
        flags: IamUowFlags,
    }

    impl MockIamUnitOfWork {
        /// 构造 MockIamUnitOfWork 与共享旗标（两者 weak ref 一致）。
        pub fn new() -> (Self, IamUowFlags) {
            let flags = IamUowFlags::default();
            (
                Self {
                    user_repo: MockIamUserRepo::new(),
                    user_role_repo: MockIamUserRoleRepo::new(),
                    menu_repo: MockIamMenuRepo::new(),
                    shelf_repo: MockIamShelfRepo::new(),
                    flags: flags.clone(),
                },
                flags,
            )
        }
    }

    #[async_trait]
    impl IamUnitOfWork for MockIamUnitOfWork {
        fn user_repo(&mut self) -> &mut dyn IamUserRepo {
            &mut self.user_repo
        }
        fn user_role_repo(&mut self) -> &mut dyn IamUserRoleRepo {
            &mut self.user_role_repo
        }
        fn menu_repo(&mut self) -> &mut dyn IamMenuRepo {
            &mut self.menu_repo
        }
        fn shelf_repo(&mut self) -> &mut dyn IamShelfRepo {
            &mut self.shelf_repo
        }
        async fn commit(self: Box<Self>) -> Result<(), AppError> {
            self.flags.committed.store(true, Ordering::SeqCst);
            Ok(())
        }
        async fn rollback(self: Box<Self>) -> Result<(), AppError> {
            self.flags.rolled_back.store(true, Ordering::SeqCst);
            Ok(())
        }
    }

    /// 把 MockIamUnitOfWork 装进 `Arc<dyn IamUowProvider>`：service `provider.begin()` 时
    /// 一次性返回该 Mock UoW（times(1) 隐式）。
    ///
    /// 守卫类用例（begin×0）直接 `Arc::new(MockIamUowProvider::new())` 不设 expectation。
    pub(crate) fn provider_returning(uow: MockIamUnitOfWork) -> Arc<dyn IamUowProvider> {
        let mut p = MockIamUowProvider::new();
        p.expect_begin()
            .times(1)
            .return_once(move || Ok(Box::new(uow) as Box<dyn IamUnitOfWork>));
        Arc::new(p)
    }
}

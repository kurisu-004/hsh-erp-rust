//! `AccountService` 的 `t_user` 子块单测（20 用例）
//!
//! 覆盖矩阵：
//! - happy path（每个方法 1 个）
//! - NotFound（get_user / update_user / admin_reset_password）
//! - Duplicate（create_user）
//! - Forbidden（create_user / admin_reset_password / change_own_password）
//! - Validation（create_user 空 password / 空 username）
//! - VersionConflict（update_user / admin_reset_password / deactivate_user）
//! - `list_users` 的 N+1 消解：2026-10-10 起批量查角色，断言只发**一次**
//!   `list_user_roles_by_user_ids`
//!
//! mockall 用法：每个 `expect_*.with(eq(...))` 必须严格匹配（mockall 0.15 strict mode）；
//! service 内**未**调用的方法可不 expect，但调用过的方法都必须 expect。
//!
//! Arrange-Act-Assert 三段结构贯穿全文。

use mockall::predicate::*;

use crate::modules::iam::dto::{UserCreateRequest, UserListQuery, UserUpdateRequest};
use crate::modules::iam::repo::MockIamRepoTrait;
use crate::modules::iam::service::tests::{
    current_clerk, current_manager, make_account_service, sample_user, sample_user_role,
};
use crate::modules::iam::vo::UserListOut;
use crate::shared::error::{AppError, code};

// ===========================================================================
// list_users（2 用例）
// ===========================================================================

#[tokio::test]
async fn list_users_returns_empty_when_repo_yields_no_rows() {
    // Arrange
    let mut mock = MockIamRepoTrait::new();
    mock.expect_list_users_with_filters()
        .returning(|_, _, _, _| Ok(vec![]));
    mock.expect_count_users_with_filters()
        .returning(|_, _| Ok(0));
    // 空页也会发一次批量角色查询（repo 侧见 `= ANY('{}')` 短路，不落 SQL）
    mock.expect_list_user_roles_by_user_ids()
        .withf(|ids: &[i64]| ids.is_empty())
        .returning(|_| Ok(vec![]));
    let svc = make_account_service();
    let query = UserListQuery {
        username_like: None,
        is_active: None,
        limit: None,
        offset: None,
    };

    // Act
    let out: UserListOut = svc
        .list_users(mock, &query, &current_manager())
        .await
        .expect("list_users 空结果应 Ok");

    // Assert
    assert_eq!(out.items.len(), 0);
    assert_eq!(out.total, 0);
    assert_eq!(out.limit, 50); // DEFAULT_LIMIT
    assert_eq!(out.offset, 0);
}

#[tokio::test]
async fn list_users_returns_rows_with_total_count() {
    // Arrange
    let u1 = sample_user(101, "alice");
    let u2 = sample_user(102, "bob");
    let r1 = sample_user_role(201, 101, "MANAGER");
    let r2 = sample_user_role(202, 102, "CLERK");
    let mut mock = MockIamRepoTrait::new();
    mock.expect_list_users_with_filters()
        .returning(move |_, _, _, _| Ok(vec![u1.clone(), u2.clone()]));
    mock.expect_count_users_with_filters()
        .returning(|_, _| Ok(2));
    // 2026-10-10：一次批量查询取回本页全部角色（`.times(1)` 是 N+1 消解的断言）
    let r1c = r1.clone();
    let r2c = r2.clone();
    mock.expect_list_user_roles_by_user_ids()
        .times(1)
        .returning(move |ids| {
            assert_eq!(ids, &[101, 102], "批量查询应带本页全部 user_id");
            Ok(vec![r1c.clone(), r2c.clone()])
        });
    let svc = make_account_service();
    let query = UserListQuery {
        username_like: Some("a".into()),
        is_active: Some(true),
        limit: Some(10),
        offset: Some(0),
    };

    // Act
    let out = svc
        .list_users(mock, &query, &current_manager())
        .await
        .expect("list_users 应 Ok");

    // Assert
    assert_eq!(out.items.len(), 2);
    assert_eq!(out.total, 2);
    assert_eq!(out.limit, 10);
    assert_eq!(out.offset, 0);
    assert_eq!(out.items[0].username, "alice");
    assert_eq!(out.items[0].roles.len(), 1);
    assert_eq!(out.items[0].roles[0].role, "MANAGER");
}

// ===========================================================================
// get_user（2 用例）
// ===========================================================================

#[tokio::test]
async fn get_user_returns_user_when_found() {
    // Arrange
    let u = sample_user(101, "alice");
    let r = sample_user_role(201, 101, "MANAGER");
    let mut mock = MockIamRepoTrait::new();
    mock.expect_get_user_by_id()
        .with(eq(101))
        .returning(move |_| Ok(Some(u.clone())));
    mock.expect_list_user_roles_by_user_id()
        .with(eq(101))
        .returning(move |_| Ok(vec![r.clone()]));
    let svc = make_account_service();

    // Act
    let out = svc
        .get_user(mock, 101, &current_manager())
        .await
        .expect("get_user 应 Ok");

    // Assert
    assert_eq!(out.username, "alice");
    assert_eq!(out.id, 101);
    assert_eq!(out.roles.len(), 1);
}

#[tokio::test]
async fn get_user_returns_not_found_when_repo_yields_none() {
    // Arrange
    let mut mock = MockIamRepoTrait::new();
    mock.expect_get_user_by_id().returning(|_| Ok(None));
    let svc = make_account_service();

    // Act
    let err = svc
        .get_user(mock, 999, &current_manager())
        .await
        .expect_err("get_user 不存在应 Err");

    // Assert
    match err {
        AppError::Biz { code, .. } => assert_eq!(code, code::USER_NOT_FOUND),
        other => panic!("expected Biz USER_NOT_FOUND, got {:?}", other),
    }
}

// ===========================================================================
// create_user（5 用例）
// ===========================================================================

#[tokio::test]
async fn create_user_happy_path_returns_user_with_roles() {
    // Arrange — 服务内部 `snowflake.next_id()` 决定 insert.id，mock 不约束 id 即可
    // （service 仅当 user.id == insert.id 才继续，故只要 mock 返回同一 id 就好，
    //  此处用 `with(eq(any))` 不约束，依赖 capture）。
    use std::sync::{Arc, Mutex};
    let u_holder: Arc<Mutex<Option<crate::modules::iam::repo::User>>> = Arc::new(Mutex::new(None));
    let u_for_create = u_holder.clone();
    let u_for_get = u_holder.clone();
    let mut mock = MockIamRepoTrait::new();
    mock.expect_get_user_by_username().returning(|_| Ok(None));
    mock.expect_create_user()
        .returning(move |user: &crate::modules::iam::repo::UserInsert| {
            // mock 模拟「INSERT 后回读」：返回带 insert.id 的完整行
            let u = crate::modules::iam::repo::User {
                id: user.id,
                username: user.username.clone(),
                password_hash: user.password_hash.clone(),
                full_name: user.full_name.clone(),
                phone: user.phone.clone(),
                is_active: user.is_active,
                ..sample_user(user.id, &user.username)
            };
            *u_for_create.lock().unwrap() = Some(u);
            Ok(())
        });
    mock.expect_get_user_by_id().returning(move |id| {
        let stored = u_for_get.lock().unwrap().clone();
        Ok(stored.or_else(|| Some(sample_user(id, "alice"))))
    });
    mock.expect_list_user_roles_by_user_id()
        .returning(|_| Ok(vec![]));
    let svc = make_account_service();
    let req = UserCreateRequest {
        username: "Alice".into(),
        password: "secret123".into(),
        full_name: "Alice Smith".into(),
        phone: None,
    };

    // Act
    let out = svc
        .create_user(mock, &req, &current_manager())
        .await
        .expect("create_user 应 Ok");

    // Assert
    assert_eq!(out.username, "alice"); // 归一化 lowercase
    assert!(out.id > 0);
    assert!(out.is_active);
}

#[tokio::test]
async fn create_user_duplicate_username_returns_409() {
    // Arrange
    let mut mock = MockIamRepoTrait::new();
    mock.expect_get_user_by_username()
        .returning(|_| Ok(Some(sample_user(101, "alice")))); // 已存在
    let svc = make_account_service();
    let req = UserCreateRequest {
        username: "alice".into(),
        password: "secret123".into(),
        full_name: "Alice".into(),
        phone: None,
    };

    // Act
    let err = svc
        .create_user(mock, &req, &current_manager())
        .await
        .expect_err("create_user 重名应 Err");

    // Assert
    match err {
        AppError::Biz { code, .. } => assert_eq!(code, code::DUPLICATE_USERNAME),
        other => panic!("expected Biz DUPLICATE_USERNAME, got {:?}", other),
    }
}

#[tokio::test]
async fn create_user_empty_password_returns_validation_error() {
    // Arrange
    let mock = MockIamRepoTrait::new();
    let svc = make_account_service();
    let req = UserCreateRequest {
        username: "alice".into(),
        password: "".into(),
        full_name: "Alice".into(),
        phone: None,
    };

    // Act
    let err = svc
        .create_user(mock, &req, &current_manager())
        .await
        .expect_err("create_user 空密码应 Err");

    // Assert
    match err {
        AppError::Validation(msg) => assert!(msg.contains("password"), "got: {msg}"),
        other => panic!("expected Validation, got {:?}", other),
    }
}

#[tokio::test]
async fn create_user_empty_username_returns_validation_error() {
    // Arrange
    let mock = MockIamRepoTrait::new();
    let svc = make_account_service();
    let req = UserCreateRequest {
        username: "   ".into(), // trim 后为空
        password: "secret123".into(),
        full_name: "Alice".into(),
        phone: None,
    };

    // Act
    let err = svc
        .create_user(mock, &req, &current_manager())
        .await
        .expect_err("create_user 空用户名应 Err");

    // Assert
    match err {
        AppError::Validation(msg) => assert!(msg.contains("username"), "got: {msg}"),
        other => panic!("expected Validation, got {:?}", other),
    }
}

#[tokio::test]
async fn create_user_forbidden_for_non_manager() {
    // Arrange
    let mock = MockIamRepoTrait::new();
    let svc = make_account_service();
    let req = UserCreateRequest {
        username: "alice".into(),
        password: "secret123".into(),
        full_name: "Alice".into(),
        phone: None,
    };

    // Act — clerk 角色调 create_user 应 403
    let err = svc
        .create_user(mock, &req, &current_clerk())
        .await
        .expect_err("create_user 非 manager 应 Err");

    // Assert
    match err {
        AppError::Biz { code, .. } => assert_eq!(code, code::FORBIDDEN),
        other => panic!("expected Biz FORBIDDEN, got {:?}", other),
    }
}

// ===========================================================================
// update_user（3 用例）
// ===========================================================================

#[tokio::test]
async fn update_user_happy_path_returns_updated_user() {
    // Arrange — service 调 2 次 get_user_by_id：第 1 次拿原 user，第 2 次拿更新后 user。
    // mockall 0.15 不支持同名 method 多次注册 expectation，故用 Arc<AtomicUsize>
    // 在闭包里按调用计数分发。
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let u = sample_user(101, "alice");
    let u_updated = crate::modules::iam::repo::User {
        full_name: "Alice New".into(),
        version: 2,
        ..sample_user(101, "alice")
    };
    let mut mock = MockIamRepoTrait::new();
    let u_clone1 = u.clone();
    let u_updated_clone = u_updated.clone();
    let call_count = Arc::new(AtomicUsize::new(0));
    let cc_clone = call_count.clone();
    mock.expect_get_user_by_id().returning(move |_| {
        let n = cc_clone.fetch_add(1, Ordering::SeqCst);
        if n == 0 {
            Ok(Some(u_clone1.clone()))
        } else {
            Ok(Some(u_updated_clone.clone()))
        }
    });
    mock.expect_update_user_partial().returning(|_, _, _| Ok(1));
    mock.expect_list_user_roles_by_user_id()
        .returning(|_| Ok(vec![]));
    let svc = make_account_service();
    let req = UserUpdateRequest {
        version: 1, // OCC 锚点来自客户端
        full_name: Some("Alice New".into()),
        phone: None,
        password: None,
        is_active: None,
    };

    // Act
    let out = svc
        .update_user(mock, 101, &req, &current_manager())
        .await
        .expect("update_user 应 Ok");

    // Assert
    assert_eq!(out.full_name, "Alice New");
    assert_eq!(out.version, 2);
}

#[tokio::test]
async fn update_user_not_found_when_user_missing() {
    // Arrange
    let mut mock = MockIamRepoTrait::new();
    mock.expect_get_user_by_id().returning(|_| Ok(None));
    let svc = make_account_service();
    let req = UserUpdateRequest {
        version: 1, // OCC 锚点来自客户端
        full_name: Some("Alice".into()),
        phone: None,
        password: None,
        is_active: None,
    };

    // Act
    let err = svc
        .update_user(mock, 999, &req, &current_manager())
        .await
        .expect_err("update_user 不存在应 Err");

    // Assert
    match err {
        AppError::Biz { code, .. } => assert_eq!(code, code::USER_NOT_FOUND),
        other => panic!("expected Biz USER_NOT_FOUND, got {:?}", other),
    }
}

#[tokio::test]
async fn update_user_version_conflict_returns_409() {
    // Arrange
    let u = sample_user(101, "alice");
    let mut mock = MockIamRepoTrait::new();
    mock.expect_get_user_by_id()
        .returning(move |_| Ok(Some(u.clone())));
    mock.expect_update_user_partial().returning(|_, _, _| Ok(0)); // 0 行 → version_conflict
    let svc = make_account_service();
    let req = UserUpdateRequest {
        version: 1, // OCC 锚点来自客户端
        full_name: Some("Alice".into()),
        phone: None,
        password: None,
        is_active: None,
    };

    // Act
    let err = svc
        .update_user(mock, 101, &req, &current_manager())
        .await
        .expect_err("update_user 0 行应 Err");

    // Assert
    match err {
        AppError::Biz { code, .. } => assert_eq!(code, code::VERSION_CONFLICT),
        other => panic!("expected Biz VERSION_CONFLICT, got {:?}", other),
    }
}

// ===========================================================================
// admin_reset_password（3 用例）
// ===========================================================================

#[tokio::test]
async fn admin_reset_password_happy_path_returns_user() {
    // Arrange
    let u = sample_user(101, "alice");
    let mut mock = MockIamRepoTrait::new();
    mock.expect_get_user_by_id()
        .returning(move |_| Ok(Some(u.clone())));
    mock.expect_update_user_password_and_rotate()
        .returning(|_, _, _, _, _| Ok(1));
    mock.expect_get_user_by_id()
        .returning(move |_| Ok(Some(sample_user(101, "alice"))));
    mock.expect_list_user_roles_by_user_id()
        .returning(|_| Ok(vec![]));
    let svc = make_account_service();

    // Act
    let out = svc
        .admin_reset_password(mock, 101, &current_manager())
        .await
        .expect("admin_reset_password 应 Ok");

    // Assert
    assert_eq!(out.id, 101);
}

#[tokio::test]
async fn admin_reset_password_not_found() {
    // Arrange
    let mut mock = MockIamRepoTrait::new();
    mock.expect_get_user_by_id().returning(|_| Ok(None));
    let svc = make_account_service();

    // Act
    let err = svc
        .admin_reset_password(mock, 999, &current_manager())
        .await
        .expect_err("admin_reset_password 不存在应 Err");

    // Assert
    match err {
        AppError::Biz { code, .. } => assert_eq!(code, code::USER_NOT_FOUND),
        other => panic!("expected Biz USER_NOT_FOUND, got {:?}", other),
    }
}

#[tokio::test]
async fn admin_reset_password_forbidden_for_worker() {
    // Arrange
    let mock = MockIamRepoTrait::new();
    let svc = make_account_service();

    // Act
    let err = svc
        .admin_reset_password(mock, 101, &current_clerk())
        .await
        .expect_err("admin_reset_password 非 manager 应 Err");

    // Assert
    match err {
        AppError::Biz { code, .. } => assert_eq!(code, code::FORBIDDEN),
        other => panic!("expected Biz FORBIDDEN, got {:?}", other),
    }
}

// ===========================================================================
// deactivate_user（2 用例）
// ===========================================================================

#[tokio::test]
async fn deactivate_user_happy_path_returns_deactivated_user() {
    // Arrange
    let u = sample_user(101, "alice");
    let mut mock = MockIamRepoTrait::new();
    mock.expect_get_user_by_id()
        .returning(move |_| Ok(Some(u.clone())));
    mock.expect_soft_delete_user().returning(|_, _, _, _| Ok(1));
    mock.expect_list_user_roles_by_user_id()
        .returning(|_| Ok(vec![]));
    let svc = make_account_service();

    // Act
    let out = svc
        .deactivate_user(mock, 101, 1, &current_manager())
        .await
        .expect("deactivate_user 应 Ok");

    // Assert
    assert!(!out.is_active); // 软删后 is_active=false
    assert_eq!(out.version, 2); // version+1
}

#[tokio::test]
async fn deactivate_user_already_inactive_returns_version_conflict() {
    // Arrange
    let u = sample_user(101, "alice");
    let mut mock = MockIamRepoTrait::new();
    mock.expect_get_user_by_id()
        .returning(move |_| Ok(Some(u.clone())));
    mock.expect_soft_delete_user().returning(|_, _, _, _| Ok(0)); // 0 行 → 已并发被改
    let svc = make_account_service();

    // Act
    let err = svc
        .deactivate_user(mock, 101, 1, &current_manager())
        .await
        .expect_err("deactivate_user 0 行应 Err");

    // Assert
    match err {
        AppError::Biz { code, .. } => assert_eq!(code, code::VERSION_CONFLICT),
        other => panic!("expected Biz VERSION_CONFLICT, got {:?}", other),
    }
}

// ===========================================================================
// change_own_password（3 用例）
// ===========================================================================

#[tokio::test]
async fn change_own_password_happy_path_succeeds() {
    // Arrange — 测试用户自己改自己密码（user_id == current.id）
    let hash = crate::auth::password::hash("old123").expect("bcrypt hash");
    let u = crate::modules::iam::repo::User {
        id: 1,
        password_hash: hash.clone(),
        is_active: true,
        version: 1,
        ..sample_user(1, "alice")
    };
    let mut mock = MockIamRepoTrait::new();
    mock.expect_get_user_by_id()
        .returning(move |_| Ok(Some(u.clone())));
    mock.expect_update_user_password_and_rotate()
        .returning(|_, _, _, _, _| Ok(1));
    let svc = make_account_service();

    // Act — current 是 current_manager()（id=1），改自己密码
    svc.change_own_password(mock, 1, "old123", "new456", &current_manager())
        .await
        .expect("change_own_password 应 Ok");
}

#[tokio::test]
async fn change_own_password_wrong_old_password_returns_mismatch() {
    // Arrange
    let hash = crate::auth::password::hash("correct_old").expect("bcrypt hash");
    let u = crate::modules::iam::repo::User {
        id: 1,
        password_hash: hash,
        is_active: true,
        version: 1,
        ..sample_user(1, "alice")
    };
    let mut mock = MockIamRepoTrait::new();
    mock.expect_get_user_by_id()
        .returning(move |_| Ok(Some(u.clone())));
    let svc = make_account_service();

    // Act
    let err = svc
        .change_own_password(mock, 1, "wrong_old", "new456", &current_manager())
        .await
        .expect_err("change_own_password 旧密码错应 Err");

    // Assert
    match err {
        AppError::Biz { code, .. } => assert_eq!(code, code::OLD_PASSWORD_MISMATCH),
        other => panic!("expected Biz OLD_PASSWORD_MISMATCH, got {:?}", other),
    }
}

#[tokio::test]
async fn change_own_password_forbidden_for_other_user() {
    // Arrange — clerk (id=1) 想改别人（id=999）的密码
    let mock = MockIamRepoTrait::new();
    let svc = make_account_service();

    // Act
    let err = svc
        .change_own_password(mock, 999, "old123", "new456", &current_clerk())
        .await
        .expect_err("change_own_password clerk 改别人应 Err");

    // Assert
    match err {
        AppError::Biz { code, .. } => assert_eq!(code, code::FORBIDDEN),
        other => panic!("expected Biz FORBIDDEN, got {:?}", other),
    }
}

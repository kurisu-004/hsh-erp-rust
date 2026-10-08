//! `AccountService` 的 `t_user_role` 子块单测（7 用例）
//!
//! 覆盖矩阵：happy path（add / remove / list）、NotFound（remove 别人的角色）、
//! Duplicate（add_role 查重命中）、Validation（MANAGER 带 scope）、
//! VersionConflict（remove_role 的 OCC 冲突）。
//!
//! `add_role` 是纯 INSERT（新行无 version）故不收 OCC 锚点；`remove_role` 收
//! `expected_version`，直接做 UPDATE 的 WHERE 条件。

use super::make_user_role_dummy;
use crate::auth::rbac::Role;
use crate::modules::iam::dto::UserAddRoleRequest;
use crate::modules::iam::repo::MockIamRepoTrait;
use crate::modules::iam::service::tests::{
    current_manager, make_account_service, sample_user, sample_user_role,
};
use crate::shared::error::code;

// ===========================================================================
// list_user_roles（2 用例）
// ===========================================================================

#[tokio::test]
async fn list_user_roles_empty_returns_empty_vec() {
    // Arrange
    let u = sample_user(101, "alice");
    let mut mock = MockIamRepoTrait::new();
    mock.expect_get_user_by_id()
        .returning(move |_| Ok(Some(u.clone())));
    mock.expect_list_user_roles_by_user_id()
        .returning(|_| Ok(vec![]));
    let svc = make_account_service();

    // Act
    let out = svc
        .list_user_roles(mock, 101, &current_manager())
        .await
        .expect("list_user_roles 空应 Ok");

    // Assert
    assert_eq!(out.len(), 0);
}

#[tokio::test]
async fn list_user_roles_with_roles_returns_role_list() {
    // Arrange
    let u = sample_user(101, "alice");
    let r1 = sample_user_role(201, 101, "MANAGER");
    let r2 = sample_user_role(202, 101, "CLERK");
    let mut mock = MockIamRepoTrait::new();
    mock.expect_get_user_by_id()
        .returning(move |_| Ok(Some(u.clone())));
    mock.expect_list_user_roles_by_user_id()
        .returning(move |_| Ok(vec![r1.clone(), r2.clone()]));
    let svc = make_account_service();

    // Act
    let out = svc
        .list_user_roles(mock, 101, &current_manager())
        .await
        .expect("list_user_roles 应 Ok");

    // Assert
    assert_eq!(out.len(), 2);
    assert_eq!(out[0].role, "MANAGER");
    assert_eq!(out[1].role, "CLERK");
}

// ===========================================================================
// add_role（3 用例）
// ===========================================================================

#[tokio::test]
async fn add_role_happy_path_returns_role() {
    // Arrange — 服务内部用 `snowflake.next_id()` 算 `insert.id`，mock 必须返回
    // 同 id 的角色才能让 `find(|r| r.id == insert.id)` 命中。
    use std::sync::{Arc, Mutex};
    let u = sample_user(101, "alice");
    let captured: Arc<Mutex<i64>> = Arc::new(Mutex::new(0));
    let cap_for_create = captured.clone();
    let cap_for_list = captured.clone();
    let mut mock = MockIamRepoTrait::new();
    mock.expect_get_user_by_id()
        .returning(move |_| Ok(Some(u.clone())));
    mock.expect_has_user_role_with_scope()
        .returning(|_, _, _, _| Ok(false));
    mock.expect_create_user_role().returning(
        move |role: &crate::modules::iam::repo::UserRoleInsert| {
            *cap_for_create.lock().unwrap() = role.id;
            Ok(())
        },
    );
    mock.expect_list_user_roles_by_user_id()
        .returning(move |uid| {
            let id = *cap_for_list.lock().unwrap();
            Ok(vec![sample_user_role(id, uid, "MANAGER")])
        });
    let svc = make_account_service();
    let req = UserAddRoleRequest {
        role: Role::Manager,
        scope_type: None,
        scope_id: None,
    };

    // Act
    let out = svc
        .add_role(mock, 101, &req, &current_manager())
        .await
        .expect("add_role 应 Ok");

    // Assert
    assert_eq!(out.role, "MANAGER");
    assert_eq!(out.id, *captured.lock().unwrap());
}

#[tokio::test]
async fn add_role_duplicate_returns_role_duplicate() {
    // Arrange
    let u = sample_user(101, "alice");
    let mut mock = MockIamRepoTrait::new();
    mock.expect_get_user_by_id()
        .returning(move |_| Ok(Some(u.clone())));
    mock.expect_has_user_role_with_scope()
        .returning(|_, _, _, _| Ok(true)); // 已存在
    let svc = make_account_service();
    let req = UserAddRoleRequest {
        role: Role::Manager,
        scope_type: None,
        scope_id: None,
    };

    // Act
    let err = svc
        .add_role(mock, 101, &req, &current_manager())
        .await
        .expect_err("add_role 重名应 Err");

    // Assert
    super::assert_biz_code(err, code::ROLE_DUPLICATE);
}

#[tokio::test]
async fn add_role_invalid_role_string_returns_validation_error() {
    // Arrange
    let u = sample_user(101, "alice");
    let mut mock = MockIamRepoTrait::new();
    mock.expect_get_user_by_id()
        .returning(move |_| Ok(Some(u.clone())));
    let svc = make_account_service();
    // 非 SHELF_ACCOUNT 但带 scope → validation error
    let req = UserAddRoleRequest {
        role: Role::Manager,
        scope_type: Some("shelf".into()),
        scope_id: Some(1),
    };

    // Act
    let err = svc
        .add_role(mock, 101, &req, &current_manager())
        .await
        .expect_err("MANAGER 带 scope 应 Err");

    // Assert
    match err {
        crate::shared::error::AppError::Validation(msg) => {
            assert!(msg.contains("scope"), "expected scope 错误信息，got: {msg}");
        }
        other => panic!("expected Validation, got {other:?}"),
    }
}

// ===========================================================================
// remove_role（2 用例）
// ===========================================================================

#[tokio::test]
async fn remove_role_happy_path_succeeds() {
    // Arrange
    let u = sample_user(101, "alice");
    let r = crate::modules::iam::repo::UserRole {
        id: 201,
        user_id: 101,
        role: "MANAGER".into(),
        scope_type: None,
        scope_id: None,
        version: 1,
        ..make_user_role_dummy(201)
    };
    let mut mock = MockIamRepoTrait::new();
    mock.expect_get_user_by_id()
        .returning(move |_| Ok(Some(u.clone())));
    mock.expect_get_user_role_by_id()
        .returning(move |_| Ok(Some(r.clone())));
    // 客户端传的 version 必须原样进 UPDATE 的 WHERE
    mock.expect_soft_delete_user_role()
        .withf(|_, version, _, _| *version == 7)
        .returning(|_, _, _, _| Ok(1));
    let svc = make_account_service();

    // Act
    svc.remove_role(mock, 101, 201, 7, &current_manager())
        .await
        .expect("remove_role 应 Ok");
}

#[tokio::test]
async fn remove_role_not_found_returns_role_not_found() {
    // Arrange
    let u = sample_user(101, "alice");
    let mut mock = MockIamRepoTrait::new();
    mock.expect_get_user_by_id()
        .returning(move |_| Ok(Some(u.clone())));
    // 角色存在但属于别的用户
    let r_other = crate::modules::iam::repo::UserRole {
        id: 201,
        user_id: 999,
        role: "MANAGER".into(),
        scope_type: None,
        scope_id: None,
        version: 1,
        ..make_user_role_dummy(201)
    };
    mock.expect_get_user_role_by_id()
        .returning(move |_| Ok(Some(r_other.clone())));
    let svc = make_account_service();

    // Act
    let err = svc
        .remove_role(mock, 101, 201, 1, &current_manager())
        .await
        .expect_err("remove_role 别人的角色应 Err");

    // Assert
    super::assert_biz_code(err, code::ROLE_NOT_FOUND);
}

#[tokio::test]
async fn remove_role_version_conflict_returns_409() {
    // Arrange
    let u = sample_user(101, "alice");
    let r = crate::modules::iam::repo::UserRole {
        id: 201,
        user_id: 101,
        role: "MANAGER".into(),
        scope_type: None,
        scope_id: None,
        version: 1,
        ..make_user_role_dummy(201)
    };
    let mut mock = MockIamRepoTrait::new();
    mock.expect_get_user_by_id()
        .returning(move |_| Ok(Some(u.clone())));
    mock.expect_get_user_role_by_id()
        .returning(move |_| Ok(Some(r.clone())));
    mock.expect_soft_delete_user_role()
        .returning(|_, _, _, _| Ok(0)); // 0 行 → OCC 冲突
    let svc = make_account_service();

    // Act
    let err = svc
        .remove_role(mock, 101, 201, 99, &current_manager())
        .await
        .expect_err("remove_role 0 行应 Err");

    // Assert
    super::assert_biz_code(err, code::VERSION_CONFLICT);
}

//! `AccountService` 的 `t_user_role` 子块单测
//!
//! 覆盖矩阵：happy path（add / remove / list）、NotFound（remove 别人的角色）、
//! Duplicate（add_role 查重命中）、Validation（MANAGER 带 scope）、
//! VersionConflict（remove_role 的 OCC 冲突）、角色码合法性（未知 / 停用 → 400）、
//! 授予 / 撤销后的踢人名单，以及撤销最后 ADMIN 的自锁护栏（400）。
//!
//! `add_role` 是纯 INSERT（新行无 version）故不收 OCC 锚点；`remove_role` 收
//! `expected_version`，直接做 UPDATE 的 WHERE 条件。

use super::make_user_role_dummy;
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
    mock.expect_get_active_role_by_code().returning(|_| {
        Ok(Some(crate::modules::iam::rbac::model::TRole::dummy(
            "MANAGER",
        )))
    });
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
        role: "MANAGER".into(),
        scope_type: None,
        scope_id: None,
    };

    // Act
    let (out, kicks) = svc
        .add_role(mock, 101, &req, &current_manager())
        .await
        .expect("add_role 应 Ok");

    // Assert
    assert_eq!(out.role, "MANAGER");
    assert_eq!(out.id, *captured.lock().unwrap());
    // 2026-10-14：授予后必须返回踢人名单（handler 在 commit 后据此踢 session）
    assert_eq!(kicks, vec![101], "授予后踢下线：KickList 必须是目标用户");
}

#[tokio::test]
async fn add_role_duplicate_returns_role_duplicate() {
    // Arrange
    let u = sample_user(101, "alice");
    let mut mock = MockIamRepoTrait::new();
    mock.expect_get_user_by_id()
        .returning(move |_| Ok(Some(u.clone())));
    mock.expect_get_active_role_by_code().returning(|_| {
        Ok(Some(crate::modules::iam::rbac::model::TRole::dummy(
            "MANAGER",
        )))
    });
    mock.expect_has_user_role_with_scope()
        .returning(|_, _, _, _| Ok(true)); // 已存在
    let svc = make_account_service();
    let req = UserAddRoleRequest {
        role: "MANAGER".into(),
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
    mock.expect_get_active_role_by_code().returning(|_| {
        Ok(Some(crate::modules::iam::rbac::model::TRole::dummy(
            "MANAGER",
        )))
    });
    let svc = make_account_service();
    // 非 SHELF_ACCOUNT 但带 scope → validation error
    let req = UserAddRoleRequest {
        role: "MANAGER".into(),
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
// remove_role（3 用例）
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

// ===========================================================================
// 2026-10-14 新增：踢人名单 + ADMIN 自锁护栏
// ===========================================================================

/// **授予后踢下线**：service 必须返回 `[目标用户]`，handler 据此在 commit 后
/// 调 `delete_all_user_sessions`。
///
/// 少返回一次的症状是「授予成功但目标用户继续用旧权限直到 session 自然过期」，
/// 且没有任何报错 —— 这条用例钉住的就是那个静默失败。
#[tokio::test]
async fn role_kick_授予后踢下线() {
    let u = sample_user(101, "alice");
    let mut mock = MockIamRepoTrait::new();
    mock.expect_get_user_by_id()
        .returning(move |_| Ok(Some(u.clone())));
    mock.expect_get_active_role_by_code().returning(|_| {
        Ok(Some(crate::modules::iam::rbac::model::TRole::dummy(
            "CLERK",
        )))
    });
    mock.expect_has_user_role_with_scope()
        .returning(|_, _, _, _| Ok(false));
    // 回读必须带**刚生成的那一行**：add_role 用 insert.id 定位，而 insert.id 是
    // snowflake 真值，只能从 create_user_role 的入参捕获（同 happy path 用例）
    use std::sync::{Arc, Mutex};
    let captured: Arc<Mutex<i64>> = Arc::new(Mutex::new(0));
    let cap_create = captured.clone();
    let cap_list = captured.clone();
    mock.expect_create_user_role().returning(
        move |row: &crate::modules::iam::repo::UserRoleInsert| {
            *cap_create.lock().unwrap() = row.id;
            Ok(())
        },
    );
    mock.expect_list_user_roles_by_user_id()
        .returning(move |uid| {
            Ok(vec![sample_user_role(
                *cap_list.lock().unwrap(),
                uid,
                "CLERK",
            )])
        });
    let svc = make_account_service();
    let req = UserAddRoleRequest {
        role: "CLERK".into(),
        scope_type: None,
        scope_id: None,
    };

    let (_, kicks) = svc
        .add_role(mock, 101, &req, &current_manager())
        .await
        .expect("授予自定义（非内置路径）角色应 Ok");

    assert_eq!(kicks, vec![101]);
}

/// **撤销最后 ADMIN 被拒**：撤 ADMIN 且 active 管理员只剩 1 个时必须 400，
/// 且**不**发出软删（handler 未 commit ⇒ 也不会踢下线）。
#[tokio::test]
async fn role_kick_撤销最后_admin被拒() {
    let u = sample_user(101, "root");
    let r = crate::modules::iam::repo::UserRole {
        id: 201,
        user_id: 101,
        role: "ADMIN".into(),
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
    // 只有一个 active 管理员 ⇒ 撤销会自锁
    mock.expect_list_active_admin_user_ids()
        .returning(|| Ok(vec![101]));
    // ⚠️ 不 expect_soft_delete_user_role：mockall 会对未预期调用 panic，
    // 这就是「护栏路径绝不触达软删」的断言。

    let svc = make_account_service();
    let err = svc
        .remove_role(mock, 101, 201, 1, &current_manager())
        .await
        .expect_err("撤销最后一个 ADMIN 应 Err");

    assert_eq!(
        err.code(),
        code::BAD_REQUEST,
        "护栏拒绝应是 HTTP 400 / 40000（不是 422 的 validation），实得 {err:?}"
    );
    assert_eq!(
        err.http_status(),
        axum::http::StatusCode::BAD_REQUEST,
        "护栏拒绝的 HTTP 状态必须是 400，实得 {err:?}"
    );
}

/// 管理员 ≥ 2 时撤销 ADMIN **放行**（护栏只在会归零时触发，
/// 否则管理员连正常撤权都做不了）。
#[tokio::test]
async fn role_kick_还有别的管理员时撤销_admin放行() {
    let u = sample_user(101, "root");
    let r = crate::modules::iam::repo::UserRole {
        id: 201,
        user_id: 101,
        role: "ADMIN".into(),
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
    mock.expect_list_active_admin_user_ids()
        .returning(|| Ok(vec![101, 102]));
    mock.expect_soft_delete_user_role()
        .returning(|_, _, _, _| Ok(1));

    let svc = make_account_service();
    let kicks = svc
        .remove_role(mock, 101, 201, 1, &current_manager())
        .await
        .expect("还有别的管理员时撤销 ADMIN 应 Ok");

    assert_eq!(kicks, vec![101], "撤销成功后同样要踢目标用户");
}

/// 自定义角色（`parse_role_string` 认不出）**不被 scope 校验拒**：
/// scope 填 NULL 时应一路走到 INSERT。
#[tokio::test]
async fn role_kick_自定义角色授予不触发scope校验() {
    let u = sample_user(101, "alice");
    let mut mock = MockIamRepoTrait::new();
    mock.expect_get_user_by_id()
        .returning(move |_| Ok(Some(u.clone())));
    mock.expect_get_active_role_by_code().returning(|_| {
        Ok(Some(crate::modules::iam::rbac::model::TRole::dummy(
            "QC_LEAD",
        )))
    });
    mock.expect_has_user_role_with_scope()
        .returning(|_, _, _, _| Ok(false));
    use std::sync::{Arc, Mutex};
    let captured: Arc<Mutex<i64>> = Arc::new(Mutex::new(0));
    let cap_create = captured.clone();
    let cap_list = captured.clone();
    mock.expect_create_user_role().returning(
        move |row: &crate::modules::iam::repo::UserRoleInsert| {
            *cap_create.lock().unwrap() = row.id;
            Ok(())
        },
    );
    mock.expect_list_user_roles_by_user_id()
        .returning(move |uid| {
            Ok(vec![sample_user_role(
                *cap_list.lock().unwrap(),
                uid,
                "QC_LEAD",
            )])
        });
    let svc = make_account_service();
    let req = UserAddRoleRequest {
        role: "QC_LEAD".into(),
        scope_type: None,
        scope_id: None,
    };

    let (out, _) = svc
        .add_role(mock, 101, &req, &current_manager())
        .await
        .expect("自定义角色授予应 Ok");

    assert_eq!(out.role, "QC_LEAD");
}

/// **未知角色码**（`t_role` 里没有）→ HTTP 400。
///
/// 这是「角色码校验从枚举 parse 放宽为查 t_role」的另一半：放宽后不存在
/// 的 code 不再由 serde / 枚举拦下，必须由 `validate_role_exists` 明确拒绝，
/// 否则脏 code 会一路写进 `t_user_role`（无 FK，DB 拦不住）。
#[tokio::test]
async fn add_role_unknown_role_code_returns_400() {
    let u = sample_user(101, "alice");
    let mut mock = MockIamRepoTrait::new();
    mock.expect_get_user_by_id()
        .returning(move |_| Ok(Some(u.clone())));
    // t_role 里查不到
    mock.expect_get_active_role_by_code()
        .returning(|_| Ok(None));

    let svc = make_account_service();
    let req = UserAddRoleRequest {
        role: "NO_SUCH_ROLE".into(),
        scope_type: None,
        scope_id: None,
    };

    let err = svc
        .add_role(mock, 101, &req, &current_manager())
        .await
        .expect_err("未知角色码应 Err");

    assert_eq!(err.code(), code::BAD_REQUEST, "未知角色码应 400：{err:?}");
    assert_eq!(err.http_status(), axum::http::StatusCode::BAD_REQUEST);
}

/// **已停用角色**（`is_active=false`）不可新授予 → HTTP 400。
///
/// 存量授予行不受影响（登录仍按 `t_user_role` 现存行判定），本用例只钉
/// 「新授予」这一支。
#[tokio::test]
async fn add_role_disabled_role_code_returns_400() {
    let u = sample_user(101, "alice");
    let mut mock = MockIamRepoTrait::new();
    mock.expect_get_user_by_id()
        .returning(move |_| Ok(Some(u.clone())));
    mock.expect_get_active_role_by_code().returning(|_| {
        let mut r = crate::modules::iam::rbac::model::TRole::dummy("QC_LEAD");
        r.is_active = false;
        Ok(Some(r))
    });

    let svc = make_account_service();
    let req = UserAddRoleRequest {
        role: "QC_LEAD".into(),
        scope_type: None,
        scope_id: None,
    };

    let err = svc
        .add_role(mock, 101, &req, &current_manager())
        .await
        .expect_err("停用角色应 Err");

    assert_eq!(err.code(), code::BAD_REQUEST, "停用角色应 400：{err:?}");
    assert!(
        err.to_string().contains("已停用"),
        "「存在但停用」与「不存在」文案应可区分：{err:?}"
    );
}

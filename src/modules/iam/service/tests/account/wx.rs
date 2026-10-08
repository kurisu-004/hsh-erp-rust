//! `AccountService` 的 `t_wx_identity` 子块单测（11 用例）
//!
//! 2026-10-10 新增：此前这 3 个方法**绕过 `IamRepoTrait`**、直接收 `&mut PgConnection`
//! 打 wx 域的 repo，所以它们既不在 `MockIamRepoTrait` 的 mock 面上、也没有任何单测。
//! 搬进 trait 之后 mock 面齐了，本文件把矩阵补齐：
//! - bind：成功 / 同 userid 幂等 / 撞别人 40108 / 反向冲突 40110 / 空 userid /
//!   超长 userid / 配置 corpid 为空 40109 / 目标账号不存在 404 / 非 Manager 403
//! - unbind：成功 / 无绑定幂等 / OCC 冲突 409
//! - get：已绑返单对象、未绑定返 `None`

use std::sync::atomic::Ordering;

use mockall::predicate::*;

use super::{assert_biz_code, sample_wx_identity, wx_bind_req};
use crate::modules::iam::repo::MockIamRepoTrait;
use crate::modules::iam::service::tests::{
    current_clerk, current_manager, make_account_service, sample_user,
};
use crate::shared::error::{AppError, code};

const CORP: &str = "ww-test-corp";

// ===========================================================================
// bind_wx_identity
// ===========================================================================

#[tokio::test]
async fn bind_wx_identity_happy_path_inserts_and_returns_row() {
    // Arrange
    let u = sample_user(101, "alice");
    let mut mock = MockIamRepoTrait::new();
    mock.expect_get_user_by_id()
        .returning(move |_| Ok(Some(u.clone())));
    // 同一个方法被 service 调两次（插入前查冲突 + 插入后回读），mockall 0.15
    // 不支持同名方法多次注册 expectation ⇒ 用调用计数分发
    let reads = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let reads_for_mock = reads.clone();
    mock.expect_get_wx_identity_by_corp_and_user()
        .returning(move |_, _| {
            if reads_for_mock.fetch_add(1, Ordering::SeqCst) == 0 {
                Ok(None) // wx→system 方向未命中
            } else {
                Ok(Some(sample_wx_identity(500, CORP, "zhangsan", 101)))
            }
        });
    mock.expect_count_active_wx_identities_by_user_id()
        .returning(|_| Ok(0)); // system→wx 方向也空
    let created: std::sync::Arc<std::sync::Mutex<Option<String>>> = Default::default();
    let cap = created.clone();
    mock.expect_create_wx_identity().returning(
        move |insert: &crate::modules::iam::repo::WxIdentityInsert| {
            // userid 归一化（trim + 小写）应在 service 层完成
            assert_eq!(insert.wx_user_id, "zhangsan");
            assert_eq!(insert.corp_id, CORP);
            assert_eq!(insert.user_id, 101);
            *cap.lock().unwrap() = Some(insert.wx_user_id.clone());
            Ok(())
        },
    );
    let svc = make_account_service();

    // Act
    let out = svc
        .bind_wx_identity(
            mock,
            101,
            &wx_bind_req("  ZhangSan  "),
            CORP,
            &current_manager(),
        )
        .await
        .expect("bind 应 Ok");

    // Assert
    assert_eq!(out.wx_user_id, "zhangsan");
    assert_eq!(out.corp_id, CORP);
    assert_eq!(out.user_id, 101);
    assert_eq!(out.id, 500);
    assert!(
        created.lock().unwrap().is_some(),
        "应真的调过 create_wx_identity"
    );
}

#[tokio::test]
async fn bind_wx_identity_same_userid_is_idempotent() {
    // Arrange —— 同 user_id + 同 userid 重复绑 → 幂等成功，不 INSERT
    let u = sample_user(101, "alice");
    let mut mock = MockIamRepoTrait::new();
    mock.expect_get_user_by_id()
        .returning(move |_| Ok(Some(u.clone())));
    mock.expect_get_wx_identity_by_corp_and_user()
        .returning(|_, _| Ok(Some(sample_wx_identity(500, CORP, "zhangsan", 101))));
    // create / count 均不应被调用（mockall 未注册的方法被调到会 panic）
    let svc = make_account_service();

    // Act
    let out = svc
        .bind_wx_identity(
            mock,
            101,
            &wx_bind_req("zhangsan"),
            CORP,
            &current_manager(),
        )
        .await
        .expect("重复绑同一 userid 应幂等成功");

    // Assert
    assert_eq!(out.id, 500);
    assert_eq!(out.wx_user_id, "zhangsan");
}

#[tokio::test]
async fn bind_wx_identity_userid_owned_by_other_user_returns_40108() {
    // Arrange —— wx→system 方向：该 userid 已属别的账号
    let u = sample_user(101, "alice");
    let mut mock = MockIamRepoTrait::new();
    mock.expect_get_user_by_id()
        .returning(move |_| Ok(Some(u.clone())));
    mock.expect_get_wx_identity_by_corp_and_user()
        .returning(|_, _| Ok(Some(sample_wx_identity(500, CORP, "zhangsan", 999))));
    let svc = make_account_service();

    // Act
    let err = svc
        .bind_wx_identity(
            mock,
            101,
            &wx_bind_req("zhangsan"),
            CORP,
            &current_manager(),
        )
        .await
        .expect_err("userid 已属他人应 Err");

    // Assert
    assert_biz_code(err, code::BIZ_WX_BINDING_DUPLICATE);
}

#[tokio::test]
async fn bind_wx_identity_reverse_conflict_returns_40110() {
    // Arrange —— system→wx 方向：本账号已绑了**别的** userid
    let u = sample_user(101, "alice");
    let mut mock = MockIamRepoTrait::new();
    mock.expect_get_user_by_id()
        .returning(move |_| Ok(Some(u.clone())));
    mock.expect_get_wx_identity_by_corp_and_user()
        .returning(|_, _| Ok(None));
    mock.expect_count_active_wx_identities_by_user_id()
        .returning(|_| Ok(1));
    mock.expect_get_wx_identity_by_user_id()
        .returning(|_| Ok(vec![sample_wx_identity(500, CORP, "lisi", 101)]));
    let svc = make_account_service();

    // Act
    let err = svc
        .bind_wx_identity(
            mock,
            101,
            &wx_bind_req("zhangsan"),
            CORP,
            &current_manager(),
        )
        .await
        .expect_err("本账号已绑别的 userid 应 Err");

    // Assert
    assert_biz_code(err, code::BIZ_WX_USER_ALREADY_BOUND);
}

#[tokio::test]
async fn bind_wx_identity_rejects_blank_userid() {
    // Arrange / Act
    let mock = MockIamRepoTrait::new();
    let svc = make_account_service();
    let err = svc
        .bind_wx_identity(mock, 101, &wx_bind_req("   "), CORP, &current_manager())
        .await
        .expect_err("空白 userid 应 Err");

    // Assert
    match err {
        AppError::Validation(msg) => assert!(msg.contains("wx_user_id"), "got: {msg}"),
        other => panic!("expected Validation, got {other:?}"),
    }
}

#[tokio::test]
async fn bind_wx_identity_rejects_oversized_userid() {
    // Arrange / Act
    let mock = MockIamRepoTrait::new();
    let svc = make_account_service();
    let long = "a".repeat(65);
    let err = svc
        .bind_wx_identity(mock, 101, &wx_bind_req(&long), CORP, &current_manager())
        .await
        .expect_err("超长 userid 应 Err");

    // Assert
    match err {
        AppError::Validation(msg) => assert!(msg.contains("长度超限"), "got: {msg}"),
        other => panic!("expected Validation, got {other:?}"),
    }
}

#[tokio::test]
async fn bind_wx_identity_blank_configured_corp_returns_40109() {
    // Arrange / Act —— 配置 corpid 为空（未配 WECOM_CORPID）
    let mock = MockIamRepoTrait::new();
    let svc = make_account_service();
    let err = svc
        .bind_wx_identity(
            mock,
            101,
            &wx_bind_req("zhangsan"),
            "  ",
            &current_manager(),
        )
        .await
        .expect_err("corpid 为空应 Err");

    // Assert
    assert_biz_code(err, code::BIZ_WX_NOT_CONFIGURED);
}

#[tokio::test]
async fn bind_wx_identity_unknown_target_user_returns_404() {
    // Arrange
    let mut mock = MockIamRepoTrait::new();
    mock.expect_get_user_by_id().returning(|_| Ok(None));
    let svc = make_account_service();

    // Act
    let err = svc
        .bind_wx_identity(
            mock,
            999,
            &wx_bind_req("zhangsan"),
            CORP,
            &current_manager(),
        )
        .await
        .expect_err("目标账号不存在应 Err");

    // Assert
    assert_biz_code(err, code::USER_NOT_FOUND);
}

#[tokio::test]
async fn bind_wx_identity_forbidden_for_non_manager() {
    // Arrange / Act
    let mock = MockIamRepoTrait::new();
    let svc = make_account_service();
    let err = svc
        .bind_wx_identity(mock, 101, &wx_bind_req("zhangsan"), CORP, &current_clerk())
        .await
        .expect_err("非 Manager 应 Err");

    // Assert
    assert_biz_code(err, code::FORBIDDEN);
}

// ===========================================================================
// unbind_wx_identity
// ===========================================================================

#[tokio::test]
async fn unbind_wx_identity_soft_deletes_with_client_version() {
    // Arrange
    let mut mock = MockIamRepoTrait::new();
    mock.expect_get_wx_identity_by_user_id()
        .returning(|_| Ok(vec![sample_wx_identity(500, CORP, "zhangsan", 101)]));
    // 客户端传的 version 必须原样进 UPDATE 的 WHERE
    mock.expect_soft_delete_wx_identity()
        .withf(|id, version, _, _| *id == 500 && *version == 3)
        .returning(|_, _, _, _| Ok(1));
    let svc = make_account_service();

    // Act
    svc.unbind_wx_identity(mock, 101, 3, &current_manager())
        .await
        .expect("unbind 应 Ok");
}

#[tokio::test]
async fn unbind_wx_identity_without_binding_is_idempotent() {
    // Arrange —— 无绑定时不发任何写（mockall 未注册的 soft_delete 被调到会 panic）
    let mut mock = MockIamRepoTrait::new();
    mock.expect_get_wx_identity_by_user_id()
        .returning(|_| Ok(vec![]));
    let svc = make_account_service();

    // Act
    svc.unbind_wx_identity(mock, 101, 1, &current_manager())
        .await
        .expect("无绑定重复解绑应幂等成功");
}

#[tokio::test]
async fn unbind_wx_identity_version_conflict_returns_409() {
    // Arrange
    let mut mock = MockIamRepoTrait::new();
    mock.expect_get_wx_identity_by_user_id()
        .returning(|_| Ok(vec![sample_wx_identity(500, CORP, "zhangsan", 101)]));
    mock.expect_soft_delete_wx_identity()
        .returning(|_, _, _, _| Ok(0));
    let svc = make_account_service();

    // Act
    let err = svc
        .unbind_wx_identity(mock, 101, 99, &current_manager())
        .await
        .expect_err("0 行应 OCC 冲突");

    // Assert
    assert_biz_code(err, code::VERSION_CONFLICT);
}

// ===========================================================================
// get_wx_identity
// ===========================================================================

#[tokio::test]
async fn get_wx_identity_returns_single_row_when_bound() {
    // Arrange
    let mut mock = MockIamRepoTrait::new();
    mock.expect_get_wx_identity_by_user_id()
        .returning(|_| Ok(vec![sample_wx_identity(500, CORP, "zhangsan", 101)]));

    // Act
    let out = make_account_service()
        .get_wx_identity(mock, 101, &current_manager())
        .await
        .expect("get 应 Ok");

    // Assert
    let out = out.expect("已绑时应返 Some");
    assert_eq!(out.id, 500);
    assert_eq!(out.wx_user_id, "zhangsan");
}

#[tokio::test]
async fn get_wx_identity_returns_none_when_not_bound() {
    // Arrange
    let mut mock = MockIamRepoTrait::new();
    mock.expect_get_wx_identity_by_user_id()
        .returning(|_| Ok(vec![]));

    // Act
    let out = make_account_service()
        .get_wx_identity(mock, 101, &current_manager())
        .await
        .expect("get 应 Ok");

    // Assert
    assert!(out.is_none(), "未绑时应返 None（HTTP data: null）");
}

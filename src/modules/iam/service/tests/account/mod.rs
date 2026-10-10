//! `AccountService` 单元测试的公共 fixture（跟随 service 侧 `account/` 的三块拆分）
//!
//! 与 service 侧 `account/{mod,user,role,wx}.rs` 一一对应的子文件：
//! - [`user`] —— `t_user` 的 CRUD / 密码（列表、详情、创建、更新、停用、自助改密、管理员重置）
//! - [`role`] —— `t_user_role` 的增删查 + SHELF_ACCOUNT scope 校验
//! - [`wx`]   —— `t_wx_identity` 的绑定 / 解绑 / 查询
//!
//! 共享的 `CurrentUser` / `MockIamRepoTrait` / sample 行构造等在上一级
//! `tests/mod.rs`（子文件用 `use super::*` 一并引入）。

mod role;
mod user;
mod wx;

use crate::modules::iam::dto::WxBindRequest;
use crate::modules::iam::service::AccountService;
use crate::shared::error::AppError;

/// 构造 `UserRole` 的最小化字段补全 helper。
pub(crate) fn make_user_role_dummy(id: i64) -> crate::modules::iam::repo::UserRole {
    let now = chrono::NaiveDate::from_ymd_opt(2026, 9, 23)
        .unwrap()
        .and_hms_opt(0, 0, 0)
        .unwrap();
    crate::modules::iam::repo::UserRole {
        id,
        user_id: 0, // 由具体 case 覆盖
        role: String::new(),
        scope_type: None,
        scope_id: None,
        version: 1,
        created_at: now,
        created_by: Some(1),
        updated_at: now,
        updated_by: Some(1),
        deleted_at: None,
    }
}

/// 构造一条 `t_wx_identity` 行（mock `get_wx_identity_by_*` 的返回值）。
pub(crate) fn sample_wx_identity(
    id: i64,
    corp_id: &str,
    wx_user_id: &str,
    user_id: i64,
) -> crate::modules::iam::repo::WxIdentity {
    let now = chrono::NaiveDate::from_ymd_opt(2026, 10, 10)
        .unwrap()
        .and_hms_opt(0, 0, 0)
        .unwrap();
    crate::modules::iam::repo::WxIdentity {
        id,
        corp_id: corp_id.to_string(),
        wx_user_id: wx_user_id.to_string(),
        user_id,
        version: 0,
        created_at: now,
        created_by: Some(1),
        updated_at: now,
        updated_by: Some(1),
        deleted_at: None,
    }
}

/// 三个 wx 写端点共用的样例请求体。
pub(crate) fn wx_bind_req(wx_user_id: &str) -> WxBindRequest {
    WxBindRequest {
        wx_user_id: wx_user_id.to_string(),
    }
}

/// 抽「断言是某个 biz code」的样板，供三个子文件复用。
#[track_caller]
pub(crate) fn assert_biz_code(err: AppError, expected: i32) {
    match err {
        AppError::Biz { code, .. } => assert_eq!(code, expected),
        other => panic!("expected Biz {expected}, got {other:?}"),
    }
}

/// 单元验证：`AccountService` 构造不 panic（字段仅 `snowflake`）。
#[test]
fn account_service_construction_does_not_panic() {
    let svc = AccountService::new(crate::modules::iam::service::tests::test_snowflake());
    let _: &AccountService = &svc;
}

/// `menus_for_roles` 的 mock 返回值构造（`t_menu` 行投影）。
pub(crate) fn sample_menu(
    id: i64,
    parent_id: Option<i64>,
    code: &str,
    sort_order: i32,
) -> crate::modules::iam::repo::model::Menu {
    let now = chrono::NaiveDate::from_ymd_opt(2026, 10, 14)
        .unwrap()
        .and_hms_opt(0, 0, 0)
        .unwrap();
    crate::modules::iam::repo::model::Menu {
        id,
        parent_id,
        code: code.to_string(),
        title: format!("菜单 {code}"),
        path: Some(format!("/{code}")),
        icon: None,
        sort_order,
        is_active: true,
        version: 0,
        created_at: now,
        created_by: None,
        updated_at: now,
        updated_by: None,
        deleted_at: None,
    }
}

/// **ADMIN 元角色的登录态菜单通配（2026-10-14 补）**：持有 ADMIN 时必须走
/// 「全量启用中菜单」查询，而不是按 `role_code='ADMIN'` 联查 `t_role_menu`。
///
/// 后者恒为空（ADMIN 不进授权表）⇒ 管理员登录后 `/iam/me` 的 `menus` 是空树，
/// 前端全站无入口，连角色面板自己的菜单都点不到。这条用例钉住分支存在：
/// 删掉分支后 `list_all_active_menus` 的期望未满足、mock 直接 panic。
#[tokio::test]
async fn menus_for_roles_admin_uses_all_active_menus() {
    use crate::modules::iam::repo::MockIamRepoTrait;

    let mut mock = MockIamRepoTrait::new();
    mock.expect_list_all_active_menus()
        .returning(|| Ok(vec![sample_menu(1, None, "home", 10)]));
    // 刻意不挂 `expect_list_active_menus_by_roles`：ADMIN 分支若误走联查会 panic
    let svc = AccountService::new(crate::modules::iam::service::tests::test_snowflake());

    let tree = svc
        .menus_for_roles(&mut mock, &["ADMIN".to_string()])
        .await
        .expect("ADMIN 的 menus_for_roles 应 Ok");

    assert_eq!(tree.len(), 1, "ADMIN 应拿到全量菜单：{tree:?}");
    assert_eq!(tree[0].code, "home");
}

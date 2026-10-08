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

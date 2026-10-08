//! prod::scan 报工台两条只读聚合端点的用例层（角色守卫 + 分页 + 调 [`repo`]）
//!
//! 与 `repo.rs` 的分工同 `prod::queue::board`：`repo` 只管 SQL 与行投影，
//! service 只管「谁能看到什么」（角色白名单 + 货架 scope 收窄）与分页信封。

use sqlx::PgConnection;

use crate::auth::rbac::{CurrentUser, Role};
use crate::modules::prod::scan::dto::{HeldQuery, PickableQuery};
use crate::modules::prod::scan::listing::repo::ScanListingRepo;
use crate::modules::prod::scan::vo::ScanListOut;
use crate::shared::error::AppError;

/// 两条端点共用的角色白名单。
const SCAN_LIST_ROLES: &[Role] = &[
    Role::Manager,
    Role::Clerk,
    Role::Inspector,
    Role::ShelfAccount,
];

/// 分页缺省与上限（两条端点一致）。
const DEFAULT_LIMIT: i64 = 50;
const MAX_LIMIT: i64 = 200;

/// `pickable` 的货架 scope 谓词入参。
///
/// 语义**逐条**对齐 `auth::rbac::CurrentUser::can_access_shelf`：
/// ```text
/// shelf_wildcard || shelf_ids.contains(&shelf_id) || has_role(Role::Manager)
/// ```
/// 即「谓词对某货架恒真」的两类账号（wildcard / Manager）返回 `None`（SQL 侧不加
/// 任何谓词），其余账号返回 scope 数组走 `sh.id = ANY($n)`。
///
/// ⚠️ `CurrentUser.shelf_ids` 是 `Vec<i64>`（JSON 层才是 string 序列）。
/// ⚠️ 空 scope 返回 `Some(vec![])` 而**不是** `None`：`ANY('{}')` 对任何货架都
/// 假，与 `can_access_shelf` 对任何货架都返 false 同形。若把空数组误判成
/// 「无限制」，未绑架的 SHELF_ACCOUNT 会看到全厂。
/// 空 scope 在生产里的真实成因：`iam::service::session::resolve_roles_and_scope`
/// 会把绑到「已停用 / 已软删 / 不存在」货架的 `scope_id` 过滤掉（登录时求值），
/// 于是这类账号登录后 `shelf_ids == []` 且 `shelf_wildcard == false`。
///
/// ⚠️ **与 `shared::shelf::select::shelf_scope_for` 的分歧是刻意的，不要合并**
/// （那个函数的 doc 里已写明同一件事）：写侧那条多一条「非货架账号不限」的分支，
/// 因为 `shelf_ids` 只对 `Role::ShelfAccount` 填 —— 一个从未被授予货架范围的
/// `Inspector`，`shelf_ids` 恒为 `[]`，照本函数收窄的话它的选架 scope 恒空、
/// 送检一律 `40301`，而送检恰恰是品检员的主业。
///
/// 读侧不受影响：本函数服务的是「工人能取哪些件」，把无货架范围的 Clerk /
/// Inspector 收窄成空列表在那个语境下无害（他本来就不该替工人取件）。
///
/// ⚠️ **Clerk / Inspector 的行为后果**：本端点的角色白名单含 Clerk / Inspector，
/// 而这两类角色按惯例不配 `t_user_role` 的 SHELF_ACCOUNT 行 ⇒ `shelf_ids` 为空
/// 且 `shelf_wildcard = false` ⇒ 收口后**返回空列表**。写侧 `worker-scan` 的
/// `require_any_role(&[Manager, ShelfAccount])` 只放行 Manager/ShelfAccount，故对
/// 这两类账号不存在「列表给出但提交被拒」的落差。若业务上要放开，唯一经产品
/// API 可达的办法是给它们**逐架**配 `scope_id` 的 SHELF_ACCOUNT 行
/// （`POST /iam/users/{id}/roles`）。
fn pickable_shelf_scope(current: &CurrentUser) -> Option<Vec<i64>> {
    if current.shelf_wildcard || current.has_role(Role::Manager) {
        None
    } else {
        Some(current.shelf_ids.clone())
    }
}

/// 报工台只读聚合 service（ZST）。
pub struct ScanListingService;

impl ScanListingService {
    /// `GET /api/v2/prod/scan/pickable`：可领取件（生产架上、绑了该工种的工序）。
    ///
    /// 只读端点：handler 走 `pool.acquire()` 不开事务。
    pub async fn list_pickable(
        conn: &mut PgConnection,
        work_type_id: i64,
        query: &PickableQuery,
        current: &CurrentUser,
    ) -> Result<ScanListOut, AppError> {
        current.require_any_role(SCAN_LIST_ROLES)?;
        let limit = query.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
        let offset = query.offset.unwrap_or(0).max(0);
        let shelf_scope = pickable_shelf_scope(current);
        let items =
            ScanListingRepo::fetch_pickable(conn, work_type_id, limit, offset, shelf_scope.clone())
                .await?;
        let total = ScanListingRepo::count_pickable(conn, work_type_id, shelf_scope).await?;
        Ok(ScanListOut {
            items,
            total,
            limit,
            offset,
        })
    }

    /// `GET /api/v2/prod/scan/held`：工人当前持有件。
    ///
    /// 放回 / 送检页的**唯一**数据源，故出参比 `pickable` 多承担一层语义：
    /// 批次的工序链位置（`chain_state` 三值 + `chain_next_process_*`）。
    /// 货架 scope 收窄不适用（行已在工人手上，不是架上的候选池）。
    pub async fn list_held(
        conn: &mut PgConnection,
        worker_id: i64,
        query: &HeldQuery,
        current: &CurrentUser,
    ) -> Result<ScanListOut, AppError> {
        current.require_any_role(SCAN_LIST_ROLES)?;
        let limit = query.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
        let offset = query.offset.unwrap_or(0).max(0);
        let items = ScanListingRepo::fetch_held(conn, worker_id, limit, offset).await?;
        let total = ScanListingRepo::count_held(conn, worker_id).await?;
        Ok(ScanListOut {
            items,
            total,
            limit,
            offset,
        })
    }
}

#[cfg(test)]
mod tests {
    //! `pickable_shelf_scope` 与 `shared::shelf::select::shelf_scope_for` 的
    //! **分歧**必须被钉住 —— 它们长得几乎一样（前半段逐条相同），而合并是错的。
    //!
    //! 两者的差异只有一条：Clerk / Inspector（无 SHELF_ACCOUNT 角色行 ⇒
    //! `shelf_ids` 恒空）在本函数下得到 `Some(vec![])`（看不见任何架），
    //! 在选架函数下得到 `None`（不限）。写侧必须不限，否则品检员一律送不了检。
    use super::*;

    /// 造一个带指定角色 / 货架范围的 `CurrentUser`。
    fn mk(roles: Vec<Role>, wildcard: bool, ids: Vec<i64>) -> CurrentUser {
        CurrentUser {
            id: 1,
            username: "u".into(),
            roles,
            shelf_wildcard: wildcard,
            shelf_ids: ids,
        }
    }

    #[test]
    fn clerk_and_inspector_get_empty_scope_on_read_side() {
        for role in [Role::Clerk, Role::Inspector] {
            assert_eq!(
                pickable_shelf_scope(&mk(vec![role], false, vec![])),
                Some(vec![]),
                "{role:?} 在读侧必须收窄成空 scope（看不见任何架）"
            );
        }
    }

    #[test]
    fn shelf_account_scope_is_passed_through_verbatim() {
        assert_eq!(
            pickable_shelf_scope(&mk(vec![Role::ShelfAccount], false, vec![7, 9])),
            Some(vec![7, 9])
        );
    }

    #[test]
    fn manager_and_wildcard_are_unrestricted() {
        assert_eq!(
            pickable_shelf_scope(&mk(vec![Role::Manager], false, vec![])),
            None
        );
        // wildcard 且带受限 id：仍是不限（wildcard 优先）
        assert_eq!(
            pickable_shelf_scope(&mk(vec![Role::ShelfAccount], true, vec![7])),
            None
        );
    }

    /// 与写侧的分歧：同一个 Clerk，在选架层**不受**收窄。
    #[test]
    fn diverges_from_write_side_shelf_scope_on_purpose() {
        use crate::shared::shelf::select::shelf_scope_for;
        for role in [Role::Clerk, Role::Inspector] {
            let user = mk(vec![role], false, vec![]);
            assert_eq!(pickable_shelf_scope(&user), Some(vec![]), "读侧收窄");
            assert_eq!(
                shelf_scope_for(&user),
                None,
                "写侧不限 —— 这条分歧是刻意的（见 pickable_shelf_scope 的 doc），\
                 把它改成与读侧一致会让品检员一律送不了检"
            );
        }
    }
}

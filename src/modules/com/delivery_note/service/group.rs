//! P1：送货分组（DeliveryGroupService）CRUD。
//!
//! ## 2026-09-22 D-5 + review 第 1 轮修正（service by-value trait）
//! 所有方法签名改为 `<R: DeliveryNoteRepoTrait>(&self, mut repo: R, ...)`（iam 严格
//! 范本）。snowflake 改为 `&self.snowflake`。跨域 ZST（`CustomerRepo::xxx`）走
//! `&mut *repo.conn_mut()`；私有 helper（`validate_group_members` /
//! `member_name_lookup`）收 `&mut PgConnection`，caller 喂 `&mut *repo.conn_mut()`。
//!
//! ## 2026-10-17 客户树任意深度后的三处口径变更
//! 1. 分组键**仍是 L1 根**（`customer_id` 必须 `parent_id IS NULL`）—— 这条语义
//!    不随客户层级加深而改变（送货单的判定键 / 编号前缀键都是根）。
//! 2. **成员**从「L1 的直接子」放宽为「L1 的任意后代」（`validate_group_members`）。
//! 3. 本文件的成员显示名与 `ungrouped_customers` 候选集从 `list_children`
//!    （**直接**子）改成 `shared::customer::subtree_ids` + `paths_of`（任意深度）。

use std::collections::HashSet;

use crate::auth::rbac::{CurrentUser, Role};
use crate::infra::clock::now_naive;
use crate::modules::com::customer::repo::CustomerRepo;
use crate::modules::com::delivery_note::repo::DeliveryNoteRepoTrait;
use crate::shared::customer::{paths_of, subtree_ids};
use crate::shared::error::{AppError, code};

use super::super::model::{DeliveryGroup, DeliveryGroupMember};
use super::super::vo::{
    DeliveryGroupListOut, DeliveryGroupMemberOut, DeliveryGroupOut, UngroupedCustomerOut,
};
use super::inner::{
    group_not_found, member_name_lookup, validate_group_members, validate_group_name,
    version_conflict,
};

use super::DeliveryGroupService;

/// `/api/v2/com/delivery/group` 4 个端点的角色白名单。
///
/// 2026-10-08 加入 `Inspector`：品检员在送货单扫码入单页要能看 / 改分组（按 L2 归属
/// 分单是扫码前的准备动作），此前只有 Manager / Clerk 能改，导致品检员被卡在这一步。
const GROUP_ROLES: &[Role] = &[Role::Manager, Role::Clerk, Role::Inspector];

impl DeliveryGroupService {
    pub async fn list_for_l1<R: DeliveryNoteRepoTrait>(
        &self,
        mut repo: R,
        l1_id: i64,
        current: &CurrentUser,
    ) -> Result<DeliveryGroupListOut, AppError> {
        // 读端点比写端点多放行 `CncProgrammer`（编程员要能看分单视图）；
        // 写端点用 `GROUP_ROLES`（Manager / Clerk / Inspector）。
        current.require_any_role(&[
            Role::Manager,
            Role::Clerk,
            Role::Inspector,
            Role::CncProgrammer,
        ])?;

        let l1 = CustomerRepo::get_by_id(&mut *repo.conn_mut(), l1_id, false)
            .await?
            .ok_or_else(|| super::inner::customer_not_found(l1_id))?;
        if l1.parent_id.is_some() {
            return Err(AppError::biz(
                code::BIZ_INVALID_VALUE,
                format!("customer {l1_id} is not an L1 root (parent_id must be NULL)"),
            ));
        }

        let groups = repo.group_list_by_customer(l1_id, false).await?;
        let group_ids: Vec<i64> = groups.iter().map(|g| g.id).collect();
        let members = repo
            .group_list_members_by_group_ids(&group_ids, false)
            .await?;
        // 2026-10-17：成员显示名的候选集由「L1 的**直接**子」（`list_children`）改成
        // 「L1 的**全部后代**」（`subtree_ids`）—— 分组成员可以是任意深度的后代，
        // 直接子列表在三层以上的树上根本匹配不上，会把成员显示成「(已删除)」。
        // 名字取 `paths_of` 的 `own_name`（本节点单段名），一次往返拿齐。
        let mut descendant_ids = subtree_ids(&mut *repo.conn_mut(), l1_id).await?;
        // 剔除 L1 自身（`subtree_ids` 含自身）：分组键就是它，列成「未分组候选」或
        // 成员都没有业务含义。
        descendant_ids.retain(|id| *id != l1_id);
        let paths = paths_of(&mut *repo.conn_mut(), &descendant_ids).await?;
        let own_name_of =
            |cid: i64| -> Option<String> { paths.get(&cid).map(|p| p.own_name.clone()) };

        let mut groups_out = Vec::with_capacity(groups.len());
        let mut membered_ids: HashSet<i64> = HashSet::new();
        for g in &groups {
            let mut mems = Vec::new();
            for m in members.iter().filter(|m| m.group_id == g.id) {
                membered_ids.insert(m.customer_id);
                let name = own_name_of(m.customer_id).unwrap_or_else(|| "(已删除)".to_string());
                mems.push(DeliveryGroupMemberOut {
                    customer_id: m.customer_id,
                    customer_name: name,
                });
            }
            groups_out.push(Self::assemble_group_out(g, mems));
        }

        // 未分组候选 = 该根的**全部后代**里还没进任何分组的那些。
        // 2026-10-17：旧形取的是「直接子」，两层树下恰好等于全部叶子；深树上若仍取
        // 直接子，L3/L4 客户会从候选里消失，用户没法把它们排进分组。
        let ungrouped_customers: Vec<UngroupedCustomerOut> = descendant_ids
            .into_iter()
            .filter(|cid| !membered_ids.contains(cid))
            .filter_map(|cid| own_name_of(cid).map(|name| UngroupedCustomerOut { id: cid, name }))
            .collect();

        Ok(DeliveryGroupListOut {
            groups: groups_out,
            ungrouped_customers,
        })
    }

    pub async fn create<R: DeliveryNoteRepoTrait>(
        &self,
        mut repo: R,
        req: super::super::dto::CreateDeliveryGroupRequest,
        current: &CurrentUser,
    ) -> Result<DeliveryGroupOut, AppError> {
        current.require_any_role(GROUP_ROLES)?;

        let l1 = CustomerRepo::get_by_id(&mut *repo.conn_mut(), req.customer_id, false)
            .await?
            .ok_or_else(|| super::inner::customer_not_found(req.customer_id))?;
        if l1.parent_id.is_some() {
            return Err(AppError::biz(
                code::BIZ_INVALID_VALUE,
                format!("customer {} is not an L1 root", req.customer_id),
            ));
        }

        let name = validate_group_name(&req.name)?;

        if repo
            .group_get_by_name(req.customer_id, &name, false)
            .await?
            .is_some()
        {
            return Err(AppError::biz(
                code::BIZ_DELIVERY_GROUP_DUPLICATE_NAME,
                format!(
                    "group name '{name}' already exists under customer {}",
                    req.customer_id
                ),
            ));
        }

        let validated_members =
            validate_group_members(&mut *repo.conn_mut(), &l1.id, &req.member_customer_ids).await?;

        let now = now_naive();
        let group = DeliveryGroup {
            id: self.snowflake.next_id(),
            customer_id: req.customer_id,
            name: name.clone(),
            version: 0,
            created_at: now,
            created_by: Some(current.id),
            updated_at: now,
            updated_by: Some(current.id),
            deleted_at: None,
        };
        repo.group_insert(&group).await?;

        for customer_id in &validated_members {
            let m = DeliveryGroupMember {
                id: self.snowflake.next_id(),
                group_id: group.id,
                customer_id: *customer_id,
                created_at: now,
                created_by: Some(current.id),
                deleted_at: None,
            };
            repo.group_insert_member(&m).await?;
        }

        let mut member_outs: Vec<DeliveryGroupMemberOut> =
            Vec::with_capacity(validated_members.len());
        for cid in &validated_members {
            let name = member_name_lookup(&mut *repo.conn_mut(), *cid)
                .await
                .unwrap_or_else(|_| "(已删除)".into());
            member_outs.push(DeliveryGroupMemberOut {
                customer_id: *cid,
                customer_name: name,
            });
        }
        Ok(Self::assemble_group_out(&group, member_outs))
    }

    pub async fn update<R: DeliveryNoteRepoTrait>(
        &self,
        mut repo: R,
        group_id: i64,
        req: super::super::dto::UpdateDeliveryGroupRequest,
        current: &CurrentUser,
    ) -> Result<DeliveryGroupOut, AppError> {
        current.require_any_role(GROUP_ROLES)?;

        let group = repo
            .group_get_by_id(group_id, false)
            .await?
            .ok_or_else(|| group_not_found(group_id))?;

        if group.version != req.version {
            return Err(version_conflict(group_id, group.version, req.version));
        }

        let mut next_name = group.name.clone();
        let mut name_changed = false;
        if let Some(ref raw) = req.name {
            let new_name = validate_group_name(raw)?;
            if new_name != group.name {
                if repo
                    .group_get_by_name(group.customer_id, &new_name, false)
                    .await?
                    .is_some()
                {
                    return Err(AppError::biz(
                        code::BIZ_DELIVERY_GROUP_DUPLICATE_NAME,
                        format!(
                            "group name '{new_name}' already exists under customer {}",
                            group.customer_id
                        ),
                    ));
                }
                next_name = new_name;
                name_changed = true;
            }
        }

        let mut new_member_ids: Vec<i64> = Vec::new();
        let mut replace_members = false;
        if let Some(ref new_ids) = req.member_customer_ids {
            let validated =
                validate_group_members(&mut *repo.conn_mut(), &group.customer_id, new_ids).await?;
            new_member_ids = validated;
            replace_members = true;
        }

        let now = now_naive();

        if name_changed {
            let affected = repo
                .group_update(group_id, group.version, &next_name, now, Some(current.id))
                .await?;
            if affected == 0 {
                return Err(AppError::biz(
                    code::VERSION_CONFLICT,
                    "concurrent modification detected",
                ));
            }
        }

        if replace_members {
            repo.group_soft_delete_members_by_group(group_id, now)
                .await?;
            for cid in &new_member_ids {
                let m = DeliveryGroupMember {
                    id: self.snowflake.next_id(),
                    group_id,
                    customer_id: *cid,
                    created_at: now,
                    created_by: Some(current.id),
                    deleted_at: None,
                };
                repo.group_insert_member(&m).await?;
            }
        }

        let updated = repo
            .group_get_by_id(group_id, true)
            .await?
            .ok_or_else(|| group_not_found(group_id))?;
        let raw_members = repo
            .group_list_members_by_group_ids(&[group_id], false)
            .await?;
        let mut members: Vec<DeliveryGroupMemberOut> = Vec::with_capacity(raw_members.len());
        for m in raw_members.into_iter().filter(|m| m.group_id == group_id) {
            let name = member_name_lookup(&mut *repo.conn_mut(), m.customer_id)
                .await
                .unwrap_or_else(|_| "(已删除)".into());
            members.push(DeliveryGroupMemberOut {
                customer_id: m.customer_id,
                customer_name: name,
            });
        }
        Ok(Self::assemble_group_out(&updated, members))
    }

    pub async fn soft_delete<R: DeliveryNoteRepoTrait>(
        &self,
        mut repo: R,
        group_id: i64,
        req: super::super::dto::DeliveryGroupIdRequest,
        current: &CurrentUser,
    ) -> Result<(), AppError> {
        current.require_any_role(GROUP_ROLES)?;

        let group = repo
            .group_get_by_id(group_id, false)
            .await?
            .ok_or_else(|| group_not_found(group_id))?;

        if group.version != req.version {
            return Err(version_conflict(group_id, group.version, req.version));
        }

        let now = now_naive();
        let affected = repo
            .group_soft_delete(group_id, req.version, now, Some(current.id))
            .await?;
        if affected == 0 {
            return Err(AppError::biz(
                code::VERSION_CONFLICT,
                "concurrent modification detected",
            ));
        }
        repo.group_soft_delete_members_by_group(group_id, now)
            .await?;
        Ok(())
    }

    fn assemble_group_out(
        g: &DeliveryGroup,
        members: Vec<DeliveryGroupMemberOut>,
    ) -> DeliveryGroupOut {
        DeliveryGroupOut {
            id: g.id,
            name: g.name.clone(),
            members,
            version: g.version,
        }
    }
}

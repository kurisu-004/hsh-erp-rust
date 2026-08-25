//! P1：送货分组（DeliveryGroupService）CRUD。

use std::collections::HashSet;

use sqlx::PgConnection;

use crate::auth::rbac::{CurrentUser, Role};
use crate::infra::clock::now_naive;
use crate::infra::snowflake::SnowflakeIdGenerator;
use crate::modules::customer::repo::CustomerRepo;
use crate::shared::error::{code, AppError};

use super::super::dto::{
    DeliveryGroupListOut, DeliveryGroupMemberOut, DeliveryGroupOut, UngroupedCustomerOut,
};
use super::super::model::{DeliveryGroup, DeliveryGroupMember};
use super::super::repo::DeliveryGroupRepo;
use super::inner::{
    group_not_found, l1_children_lookup, validate_group_name, validate_l2_members,
    version_conflict,
};

use super::DeliveryGroupService;

impl DeliveryGroupService {
    pub async fn list_for_l1(
        conn: &mut PgConnection,
        l1_id: i64,
        current: &CurrentUser,
    ) -> Result<DeliveryGroupListOut, AppError> {
        current.require_any_role(&[
            Role::Manager,
            Role::Clerk,
            Role::Inspector,
            Role::CncProgrammer,
        ])?;

        let l1 = CustomerRepo::get_by_id(&mut *conn, l1_id, false)
            .await?
            .ok_or_else(|| super::inner::customer_not_found(l1_id))?;
        if l1.parent_id.is_some() {
            return Err(AppError::biz(
                code::BIZ_INVALID_VALUE,
                format!("customer {l1_id} is not an L1 root (parent_id must be NULL)"),
            ));
        }

        let groups = DeliveryGroupRepo::list_by_customer(&mut *conn, l1_id, false).await?;
        let group_ids: Vec<i64> = groups.iter().map(|g| g.id).collect();
        let members =
            DeliveryGroupRepo::list_members_by_group_ids(&mut *conn, &group_ids, false).await?;
        let l2_children = CustomerRepo::list_children(&mut *conn, l1_id, false).await?;

        let mut groups_out = Vec::with_capacity(groups.len());
        let mut membered_l2_ids: HashSet<i64> = HashSet::new();
        for g in &groups {
            let mut mems = Vec::new();
            for m in members.iter().filter(|m| m.group_id == g.id) {
                membered_l2_ids.insert(m.customer_id);
                let name = l2_children
                    .iter()
                    .find(|c| c.id == m.customer_id)
                    .map(|c| c.name.clone())
                    .unwrap_or_else(|| "(已删除)".to_string());
                mems.push(DeliveryGroupMemberOut {
                    customer_id: m.customer_id,
                    customer_name: name,
                });
            }
            groups_out.push(Self::assemble_group_out(g, mems));
        }

        let ungrouped_customers: Vec<UngroupedCustomerOut> = l2_children
            .into_iter()
            .filter(|c| !membered_l2_ids.contains(&c.id))
            .map(|c| UngroupedCustomerOut {
                id: c.id,
                name: c.name,
            })
            .collect();

        Ok(DeliveryGroupListOut {
            groups: groups_out,
            ungrouped_customers,
        })
    }

    pub async fn create(
        conn: &mut PgConnection,
        snowflake: &SnowflakeIdGenerator,
        req: super::super::dto::CreateDeliveryGroupRequest,
        current: &CurrentUser,
    ) -> Result<DeliveryGroupOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk])?;

        let l1 = CustomerRepo::get_by_id(&mut *conn, req.customer_id, false)
            .await?
            .ok_or_else(|| super::inner::customer_not_found(req.customer_id))?;
        if l1.parent_id.is_some() {
            return Err(AppError::biz(
                code::BIZ_INVALID_VALUE,
                format!("customer {} is not an L1 root", req.customer_id),
            ));
        }

        let name = validate_group_name(&req.name)?;

        if DeliveryGroupRepo::get_by_name(&mut *conn, req.customer_id, &name, false)
            .await?
            .is_some()
        {
            return Err(AppError::biz(
                code::BIZ_DELIVERY_GROUP_DUPLICATE_NAME,
                format!("group name '{name}' already exists under customer {}", req.customer_id),
            ));
        }

        let validated_members =
            validate_l2_members(conn, &l1.id, &req.member_customer_ids).await?;

        let now = now_naive();
        let group = DeliveryGroup {
            id: snowflake.next_id(),
            customer_id: req.customer_id,
            name: name.clone(),
            version: 0,
            created_at: now,
            created_by: Some(current.id),
            updated_at: now,
            updated_by: Some(current.id),
            deleted_at: None,
        };
        DeliveryGroupRepo::insert(&mut *conn, &group).await?;

        for customer_id in &validated_members {
            let m = DeliveryGroupMember {
                id: snowflake.next_id(),
                group_id: group.id,
                customer_id: *customer_id,
                created_at: now,
                created_by: Some(current.id),
                deleted_at: None,
            };
            DeliveryGroupRepo::insert_member(&mut *conn, &m).await?;
        }

        let mut member_outs: Vec<DeliveryGroupMemberOut> =
            Vec::with_capacity(validated_members.len());
        for cid in &validated_members {
            let name = l1_children_lookup(conn, *cid)
                .await
                .unwrap_or_else(|_| "(已删除)".into());
            member_outs.push(DeliveryGroupMemberOut {
                customer_id: *cid,
                customer_name: name,
            });
        }
        Ok(Self::assemble_group_out(&group, member_outs))
    }

    pub async fn update(
        conn: &mut PgConnection,
        snowflake: &SnowflakeIdGenerator,
        group_id: i64,
        req: super::super::dto::UpdateDeliveryGroupRequest,
        current: &CurrentUser,
    ) -> Result<DeliveryGroupOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk])?;

        let group = DeliveryGroupRepo::get_by_id(&mut *conn, group_id, false)
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
                if DeliveryGroupRepo::get_by_name(&mut *conn, group.customer_id, &new_name, false)
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
            let validated = validate_l2_members(conn, &group.customer_id, new_ids).await?;
            new_member_ids = validated;
            replace_members = true;
        }

        let now = now_naive();

        if name_changed {
            let affected = DeliveryGroupRepo::update(
                &mut *conn,
                group_id,
                group.version,
                &next_name,
                now,
                Some(current.id),
            )
            .await?;
            if affected == 0 {
                return Err(AppError::biz(
                    code::VERSION_CONFLICT,
                    "concurrent modification detected",
                ));
            }
        }

        if replace_members {
            DeliveryGroupRepo::soft_delete_members_by_group(&mut *conn, group_id, now).await?;
            for cid in &new_member_ids {
                let m = DeliveryGroupMember {
                    id: snowflake.next_id(),
                    group_id,
                    customer_id: *cid,
                    created_at: now,
                    created_by: Some(current.id),
                    deleted_at: None,
                };
                DeliveryGroupRepo::insert_member(&mut *conn, &m).await?;
            }
        }

        let updated = DeliveryGroupRepo::get_by_id(&mut *conn, group_id, true)
            .await?
            .ok_or_else(|| group_not_found(group_id))?;
        let raw_members =
            DeliveryGroupRepo::list_members_by_group_ids(&mut *conn, &[group_id], false).await?;
        let mut members: Vec<DeliveryGroupMemberOut> = Vec::with_capacity(raw_members.len());
        for m in raw_members.into_iter().filter(|m| m.group_id == group_id) {
            let name = l1_children_lookup(conn, m.customer_id)
                .await
                .unwrap_or_else(|_| "(已删除)".into());
            members.push(DeliveryGroupMemberOut {
                customer_id: m.customer_id,
                customer_name: name,
            });
        }
        Ok(Self::assemble_group_out(&updated, members))
    }

    pub async fn soft_delete(
        conn: &mut PgConnection,
        group_id: i64,
        req: super::super::dto::DeliveryGroupIdRequest,
        current: &CurrentUser,
    ) -> Result<(), AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk])?;

        let group = DeliveryGroupRepo::get_by_id(&mut *conn, group_id, false)
            .await?
            .ok_or_else(|| group_not_found(group_id))?;

        if group.version != req.version {
            return Err(version_conflict(group_id, group.version, req.version));
        }

        let now = now_naive();
        let affected =
            DeliveryGroupRepo::soft_delete(&mut *conn, group_id, req.version, now, Some(current.id))
                .await?;
        if affected == 0 {
            return Err(AppError::biz(
                code::VERSION_CONFLICT,
                "concurrent modification detected",
            ));
        }
        DeliveryGroupRepo::soft_delete_members_by_group(&mut *conn, group_id, now).await?;
        Ok(())
    }

    fn assemble_group_out(
        g: &DeliveryGroup,
        members: Vec<DeliveryGroupMemberOut>,
    ) -> DeliveryGroupOut {
        DeliveryGroupOut {
            id: g.id,
            customer_id: g.customer_id,
            name: g.name.clone(),
            members,
            version: g.version,
            created_at: g.created_at,
            updated_at: g.updated_at,
        }
    }
}

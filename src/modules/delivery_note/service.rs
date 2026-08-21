//! delivery_note 域业务逻辑
//!
//! 对应 Python myERP/service/delivery_note_service.py（及 _<d>_*.py helper）。
//! 实施约定：方法签名接收 `&mut PgConnection`，由 handler 开 tx 并 commit。
//!
//! ## Phase P1 范围
//! 本期落「送货分组」CRUD 服务 + `classify()` 纯函数：
//! - `DeliveryGroupService::{list_for_l1, create, update, soft_delete}`
//! - `classify` + `NoteScope` + `GroupWithMemberIds`（模块内自由函数 / 类型）
//!
//! 送货单生命周期（create / add_parts / submit / recall / pickup / soft_delete）+ 扫码入单
//! 留到 P2 / P3，对应方法签名 / 错误码段已在 model / dto / error 中占位。

use std::collections::HashSet;

use sqlx::PgConnection;

use crate::auth::rbac::{CurrentUser, Role};
use crate::infra::clock::now_naive;
use crate::infra::snowflake::SnowflakeIdGenerator;
use crate::modules::customer::model::TCustomer;
use crate::modules::customer::repo::CustomerRepo;
use crate::shared::error::{code, AppError};

use super::dto::{
    CreateDeliveryGroupRequest, DeliveryGroupIdRequest, DeliveryGroupListOut,
    DeliveryGroupMemberOut, DeliveryGroupOut, UngroupedCustomerOut,
    UpdateDeliveryGroupRequest,
};
use super::model::{DeliveryGroup, DeliveryGroupMember, NoteScope};
use super::repo::DeliveryGroupRepo;

/// 分组名最大长度（与 DB 列 `varchar(100)` 对齐）
const NAME_MAX_LEN: usize = 100;

// ---------------------------------------------------------------------------
//  NoteScope / classify  (设计 §3.2)
// ---------------------------------------------------------------------------

/// service 内部用的分组形态（DB 装载 + 内存投影，避免暴露 `DeliveryGroup` 全字段）
#[derive(Debug, Clone)]
pub struct GroupWithMemberIds {
    pub group_id: i64,
    pub member_ids: Vec<i64>,
}

impl NoteScope {
    /// 设计 §3.2 分类函数（D4 + D5 规则）：
    /// - groups 为空 → `L1Wide`（遗留行为）
    /// - groups 非空 → 命中成员集 → `Group(g.id)`；否则 `Leaf(leaf_customer_id)`
    ///
    /// L1 自身（`leaf_customer_id == l1_id`）按 D5 视为「单厂单」→ `Leaf(l1_id)`：
    /// `groups.member_ids` 不可能包含 L1（成员必须是 L2 子节点）。
    pub fn classify(leaf_customer_id: i64, groups: &[GroupWithMemberIds]) -> Self {
        if groups.is_empty() {
            return Self::L1Wide;
        }
        for g in groups {
            if g.member_ids.contains(&leaf_customer_id) {
                return Self::Group(g.group_id);
            }
        }
        Self::Leaf(leaf_customer_id)
    }
}

#[cfg(test)]
mod classify_tests {
    use super::*;

    fn g(id: i64, members: &[i64]) -> GroupWithMemberIds {
        GroupWithMemberIds {
            group_id: id,
            member_ids: members.to_vec(),
        }
    }

    #[test]
    fn classify_no_groups_returns_l1wide() {
        let scope = NoteScope::classify(101, &[]);
        assert_eq!(scope, NoteScope::L1Wide);
    }

    #[test]
    fn classify_member_returns_group() {
        // leaf=102 是 group 10 的成员
        let groups = vec![g(10, &[101, 102, 103])];
        assert_eq!(NoteScope::classify(102, &groups), NoteScope::Group(10));
    }

    #[test]
    fn classify_non_member_returns_leaf() {
        // leaf=104 不在任何 group 中
        let groups = vec![g(10, &[101, 102, 103])];
        assert_eq!(NoteScope::classify(104, &groups), NoteScope::Leaf(104));
    }

    #[test]
    fn classify_with_l1_self_returns_leaf_l1_id() {
        // L1 自身（leaf=100）即使有 group 配置，也走单厂单（成员必须 L2 子节点）
        let groups = vec![g(10, &[101, 102, 103])];
        assert_eq!(NoteScope::classify(100, &groups), NoteScope::Leaf(100));
    }

    #[test]
    fn classify_first_matching_group_wins() {
        // 同一 leaf 在两个 group 中都配置了（数据库不允许，但代码防御）：取首个
        let groups = vec![g(10, &[102]), g(20, &[102])];
        assert_eq!(NoteScope::classify(102, &groups), NoteScope::Group(10));
    }
}

// ---------------------------------------------------------------------------
//  DeliveryGroupService
// ---------------------------------------------------------------------------

pub struct DeliveryGroupService;

impl DeliveryGroupService {
    // ---------- list ----------

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

        // 1. 校验 L1 存在且是 L1（parent_id IS NULL）
        let l1 = CustomerRepo::get_by_id(&mut *conn, l1_id, false)
            .await?
            .ok_or_else(|| customer_not_found(l1_id))?;
        if l1.parent_id.is_some() {
            return Err(AppError::biz(
                code::BIZ_INVALID_VALUE,
                format!("customer {l1_id} is not an L1 root (parent_id must be NULL)"),
            ));
        }

        // 2. 拉分组 + 成员 + L2 子客户 + 组外 L2 计算
        let groups = DeliveryGroupRepo::list_by_customer(&mut *conn, l1_id, false).await?;
        let group_ids: Vec<i64> = groups.iter().map(|g| g.id).collect();
        let members =
            DeliveryGroupRepo::list_members_by_group_ids(&mut *conn, &group_ids, false).await?;
        let l2_children = CustomerRepo::list_children(&mut *conn, l1_id, false).await?;

        // 组装 groups（含 members）
        let mut groups_out = Vec::with_capacity(groups.len());
        let mut membered_l2_ids: HashSet<i64> = HashSet::new();
        for g in &groups {
            let mut mems = Vec::new();
            for m in members.iter().filter(|m| m.group_id == g.id) {
                membered_l2_ids.insert(m.customer_id);
                // 取 L2 名：批量解析后再 zip，避免 N+1
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

        // 组外 L2（l2_children 减去已入组的）
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

    // ---------- create ----------

    pub async fn create(
        conn: &mut PgConnection,
        snowflake: &SnowflakeIdGenerator,
        req: CreateDeliveryGroupRequest,
        current: &CurrentUser,
    ) -> Result<DeliveryGroupOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk])?;

        // 1. L1 校验
        let l1 = CustomerRepo::get_by_id(&mut *conn, req.customer_id, false)
            .await?
            .ok_or_else(|| customer_not_found(req.customer_id))?;
        if l1.parent_id.is_some() {
            return Err(AppError::biz(
                code::BIZ_INVALID_VALUE,
                format!("customer {} is not an L1 root", req.customer_id),
            ));
        }

        // 2. name 校验（trim 后 1..=100）
        let name = validate_group_name(&req.name)?;

        // 3. 重名检测
        if DeliveryGroupRepo::get_by_name(&mut *conn, req.customer_id, &name, false)
            .await?
            .is_some()
        {
            return Err(AppError::biz(
                code::BIZ_DELIVERY_GROUP_DUPLICATE_NAME,
                format!("group name '{name}' already exists under customer {}", req.customer_id),
            ));
        }

        // 4. 校验每个 member_customer_ids[i]：必须是当前 l1 的直接 L2 子节点
        let validated_members = validate_l2_members(conn, &l1.id, &req.member_customer_ids).await?;

        // 5. 插入分组
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

        // 6. 插入成员（复用雪花 id；同 tx）
        let mut member_models = Vec::with_capacity(validated_members.len());
        for customer_id in &validated_members {
            member_models.push(DeliveryGroupMember {
                id: snowflake.next_id(),
                group_id: group.id,
                customer_id: *customer_id,
                created_at: now,
                created_by: Some(current.id),
                deleted_at: None,
            });
        }
        for m in &member_models {
            DeliveryGroupRepo::insert_member(&mut *conn, m).await?;
        }

        // 7. 回读组装出参（成员名从已经解析好的 list 中取）
        let mut member_outs: Vec<DeliveryGroupMemberOut> = Vec::with_capacity(validated_members.len());
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

    // ---------- update ----------

    pub async fn update(
        conn: &mut PgConnection,
        snowflake: &SnowflakeIdGenerator,
        group_id: i64,
        req: UpdateDeliveryGroupRequest,
        current: &CurrentUser,
    ) -> Result<DeliveryGroupOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk])?;

        // 1. 读出当前 group
        let group = DeliveryGroupRepo::get_by_id(&mut *conn, group_id, false)
            .await?
            .ok_or_else(|| group_not_found(group_id))?;

        if group.version != req.version {
            return Err(AppError::biz(
                code::VERSION_CONFLICT,
                format!(
                    "group {} version conflict: have {}, request {}",
                    group_id, group.version, req.version
                ),
            ));
        }

        // 2. name 处理：None=不改；Some(trim 后空)=400；Some=与 DB 同值则跳过重名校验
        let mut next_name = group.name.clone();
        let mut name_changed = false;
        if let Some(ref raw) = req.name {
            let new_name = validate_group_name(raw)?;
            if new_name != group.name {
                // 重名校验（仅在确实变化时）
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

        // 3. 成员处理：None=不改；Some(vec)=全量替换
        let mut new_member_ids: Vec<i64> = Vec::new();
        let mut replace_members = false;
        if let Some(ref new_ids) = req.member_customer_ids {
            // 校验每个新成员 L2 是 group.customer_id 的直接子节点
            let validated = validate_l2_members(conn, &group.customer_id, new_ids).await?;
            new_member_ids = validated;
            replace_members = true;
        }

        let now = now_naive();

        // 4. 写库
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
            // 软删旧成员（仅活跃的）
            DeliveryGroupRepo::soft_delete_members_by_group(&mut *conn, group_id, now).await?;
            // 插入新成员（同 tx 撞 partial unique 即 23505 → 已存在冲突）
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

        // 5. 回读组装
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

    // ---------- soft_delete ----------

    pub async fn soft_delete(
        conn: &mut PgConnection,
        group_id: i64,
        req: DeliveryGroupIdRequest,
        current: &CurrentUser,
    ) -> Result<(), AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk])?;

        let group = DeliveryGroupRepo::get_by_id(&mut *conn, group_id, false)
            .await?
            .ok_or_else(|| group_not_found(group_id))?;

        if group.version != req.version {
            return Err(AppError::biz(
                code::VERSION_CONFLICT,
                format!(
                    "group {} version conflict: have {}, request {}",
                    group_id, group.version, req.version
                ),
            ));
        }

        let now = now_naive();

        // 软删分组头
        let affected =
            DeliveryGroupRepo::soft_delete(&mut *conn, group_id, req.version, now, Some(current.id))
                .await?;
        if affected == 0 {
            return Err(AppError::biz(
                code::VERSION_CONFLICT,
                "concurrent modification detected",
            ));
        }

        // 级联软删成员（同 tx）
        DeliveryGroupRepo::soft_delete_members_by_group(&mut *conn, group_id, now).await?;

        Ok(())
    }

    // ---------- helpers ----------

    fn assemble_group_out(g: &DeliveryGroup, members: Vec<DeliveryGroupMemberOut>) -> DeliveryGroupOut {
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

// ---------------------------------------------------------------------------
//  私有 helpers
// ---------------------------------------------------------------------------

fn customer_not_found(id: i64) -> AppError {
    AppError::biz(
        code::BIZ_CUSTOMER_NOT_FOUND,
        format!("customer {id} not found"),
    )
}

fn group_not_found(id: i64) -> AppError {
    AppError::biz(
        code::BIZ_DELIVERY_GROUP_NOT_FOUND,
        format!("delivery group {id} not found"),
    )
}

fn validate_group_name(raw: &str) -> Result<String, AppError> {
    let trimmed = raw.trim().to_string();
    if trimmed.is_empty() {
        return Err(AppError::validation("group name must not be empty"));
    }
    if trimmed.chars().count() > NAME_MAX_LEN {
        return Err(AppError::validation(format!(
            "group name length must be <= {NAME_MAX_LEN}"
        )));
    }
    Ok(trimmed)
}

/// 校验所有 member_customer_ids 都是 `l1_id` 的直接 L2 子节点，
/// 并检查每个 L2 未被其他活跃分组占用（21415 冲突）。
///
/// 返回值：去重 + 顺序稳定的 L2 id 列表（保持 caller 入参顺序）。
async fn validate_l2_members(
    conn: &mut PgConnection,
    l1_id: &i64,
    ids: &[i64],
) -> Result<Vec<i64>, AppError> {
    let mut seen: HashSet<i64> = HashSet::new();
    let mut ordered: Vec<i64> = Vec::with_capacity(ids.len());
    for raw_id in ids {
        if !seen.insert(*raw_id) {
            continue; // 同请求内去重
        }
        let cust = CustomerRepo::get_by_id(&mut *conn, *raw_id, false)
            .await?
            .ok_or_else(|| customer_not_found(*raw_id))?;
        if cust.parent_id.as_ref() != Some(l1_id) {
            return Err(AppError::biz(
                code::BIZ_INVALID_VALUE,
                format!(
                    "customer {raw_id} is not an L2 child of L1 {l1_id} (parent_id={:?})",
                    cust.parent_id
                ),
            ));
        }
        if DeliveryGroupRepo::list_active_member_by_customer(&mut *conn, *raw_id)
            .await?
            .is_some()
        {
            return Err(AppError::biz(
                code::BIZ_DELIVERY_GROUP_MEMBER_CONFLICT,
                format!("customer {raw_id} is already an active member of another group"),
            ));
        }
        ordered.push(*raw_id);
    }
    Ok(ordered)
}

/// 同步单条：取 L2 客户名（仅在已确认 parent_id 的前提下用）。
async fn l1_children_lookup(conn: &mut PgConnection, l2_id: i64) -> Result<String, AppError> {
    let c = CustomerRepo::get_by_id(&mut *conn, l2_id, true)
        .await?
        .ok_or_else(|| customer_not_found(l2_id))?;
    Ok(c.name)
}

/// 已软删客户行占位名（list_for_l1 中通过 l2_children 反查的 fallback）
#[allow(dead_code)]
fn _ensure_tcustomer_used(_: &TCustomer) {}
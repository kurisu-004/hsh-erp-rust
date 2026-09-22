//! customer 域 CRUD service
//!
//! 列表 / 详情 / 创建 / 更新 / 软删 —— 共 5 个端点。
//!
//! ## 业务约束（service 层 enforce）
//! - L1：`parent_id IS NULL`，`serial_prefix` 必须为单个大写字母
//! - L2：`parent_id` 非 NULL（指向 L1），`serial_prefix` 必须 NULL
//!
//! ## soft-delete "in use" 检查
//! 与 Python `service/customer.py:soft_delete_customer` 对齐：先查 `t_part.customer_id` 与
//! `t_assembly.customer_id` 的非软删引用计数，>0 即拒软删，抛 `20113 BIZ_CUSTOMER_IN_USE`。
//! **不**检查 `t_customer.parent_id`（子 L2），所以「L1 仍有 L2 子节点」**不会**触发此码——
//! L1 子节点的语义由前端在删除前显式 cascade 处理，与 Python 行为一致。
//!
//! 2026-09-22 重构：跨域 inline SQL（`t_part` / `t_assembly` 引用计数）从本文件下沉到
//! `CustomerRepo::count_parts_using_customer` / `count_assemblies_using_customer`——
//! service 仍只持 `Arc<SnowflakeIdGenerator>` 字段，所有跨 repo 操作经 `repo: R` 形参。
//!
//! ## 事务边界（2026-09-22 重构对齐 iam 范本）
//! 事务移交 handler（与 20 个 handler 文件现状对齐）：service 仅业务逻辑，所有跨 repo
//! 操作经 `repo: R`（by-value；`R: CustomerRepo`）参数传入——handler/service 借
//! `&mut *tx` / `&mut *conn` 喂给 `CustomerRepo` trait（trait 已直接
//! `impl for &mut PgConnection`）。service 不知事务——handler `pool.begin()` +
//! `tx.commit()` 包外。
//!
//! `CustomerService` 是 unit struct（无字段依赖，iam 范本 §6）；方法签名
//! `<R: CustomerRepoTrait>(&self, mut repo: R, ...)`，生产 `R = &mut PgConnection`。

use std::sync::Arc;

use crate::auth::rbac::{CurrentUser, Role};
use crate::infra::snowflake::SnowflakeIdGenerator;
use crate::modules::com::customer::dto::*;
use crate::modules::com::customer::model::TCustomer;
use crate::modules::com::customer::repo::CustomerRepoTrait;
use crate::modules::com::customer::vo::{CustomerListOut, CustomerOut};
use crate::shared::error::{AppError, code};

const DEFAULT_LIMIT: i64 = 50;
const MAX_LIMIT: i64 = 500;

fn customer_not_found() -> AppError {
    AppError::biz(code::BIZ_CUSTOMER_NOT_FOUND, "客户不存在")
}

fn version_conflict() -> AppError {
    AppError::biz(code::VERSION_CONFLICT, "数据已被他人修改，请刷新后重试")
}

/// 把 service 的 `(parent_id: i64, name: String)` 拼成 `CustomerOut`。
fn to_customer_out(c: TCustomer, parent_name: Option<String>) -> CustomerOut {
    CustomerOut {
        id: c.id,
        name: c.name,
        parent_id: c.parent_id.map(|v| v.to_string()),
        parent_name,
        serial_prefix: c.serial_prefix,
        version: c.version,
        created_at: c.created_at,
        updated_at: c.updated_at,
    }
}

/// 校验 `serial_prefix`：必须为单个 ASCII 大写字母；空 / 多字符 / 小写都拒。
fn check_serial_prefix(s: &str) -> Result<String, AppError> {
    let upper = s.to_uppercase();
    if upper.len() != 1 || !upper.chars().next().unwrap().is_ascii_uppercase() {
        return Err(AppError::biz(
            code::BIZ_INVALID_VALUE,
            "serial_prefix 必须是单个大写字母",
        ));
    }
    Ok(upper)
}

/// customer 域 service（2026-09-22 重构后）
///
/// 字段仅 `snowflake`（事务已移交 handler）。实例为轻壳，可直接
/// `Arc<CustomerService>` 存 `AppState`；方法签名收 `mut repo: R`（by-value；
/// 生产 `R = &mut PgConnection`，单测 `R = MockCustomerRepo`），单测用 `MockCustomerRepo`
/// 直接注入。
pub struct CustomerService {
    snowflake: Arc<SnowflakeIdGenerator>,
}

impl CustomerService {
    /// 构造：仅需雪花 ID 生成器。
    pub fn new(snowflake: Arc<SnowflakeIdGenerator>) -> Self {
        Self { snowflake }
    }

    // =======================================================================
    // 列表 / 详情
    // =======================================================================

    pub async fn list_customers<R: CustomerRepoTrait>(
        &self,
        mut repo: R,
        query: &CustomerListQuery,
        user: &CurrentUser,
    ) -> Result<CustomerListOut, AppError> {
        user.require_any_role(&[
            Role::Manager,
            Role::Clerk,
            Role::CncProgrammer,
            Role::Inspector,
        ])?;

        let limit = query.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
        let offset = query.offset.unwrap_or(0).max(0);
        let name_like = query
            .name_like
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        // parent_id 与 is_root 互斥：parent_id 优先；二者皆无则不按层级过滤
        let parent_id_i64: Option<i64> = match &query.parent_id {
            Some(s) if !s.is_empty() => Some(
                s.parse::<i64>()
                    .map_err(|_| AppError::biz(code::BIZ_INVALID_VALUE, "parent_id 非整数"))?,
            ),
            _ => None,
        };
        let is_root = if parent_id_i64.is_some() {
            None
        } else {
            query.is_root
        };

        let items = repo
            .list_with_filters(name_like, parent_id_i64, is_root, limit, offset)
            .await?;
        let total = repo
            .count_with_filters(name_like, parent_id_i64, is_root)
            .await?;

        // 父客户名补全（防 N+1：一次 list_by_ids 拿齐）
        let parent_ids: Vec<i64> = items.iter().filter_map(|c| c.parent_id).collect();
        let parents = repo.list_by_ids(&parent_ids, true).await?;
        let parent_map: std::collections::HashMap<i64, String> =
            parents.into_iter().map(|p| (p.id, p.name)).collect();

        let out_items = items
            .into_iter()
            .map(|c| {
                let parent_name = c.parent_id.and_then(|pid| parent_map.get(&pid).cloned());
                to_customer_out(c, parent_name)
            })
            .collect();

        Ok(CustomerListOut {
            items: out_items,
            total,
            limit,
            offset,
        })
    }

    pub async fn get_customer<R: CustomerRepoTrait>(
        &self,
        mut repo: R,
        id: i64,
        user: &CurrentUser,
    ) -> Result<CustomerOut, AppError> {
        user.require_any_role(&[
            Role::Manager,
            Role::Clerk,
            Role::CncProgrammer,
            Role::Inspector,
        ])?;

        let c = repo
            .get_by_id(id, false)
            .await?
            .ok_or_else(customer_not_found)?;

        let parent_name = match c.parent_id {
            Some(pid) => repo
                .get_by_id(pid, true)
                .await?
                .map(|p| p.name),
            None => None,
        };

        Ok(to_customer_out(c, parent_name))
    }

    // =======================================================================
    // 创建 / 更新 / 软删
    // =======================================================================

    pub async fn create_customer<R: CustomerRepoTrait>(
        &self,
        mut repo: R,
        req: &CustomerCreateRequest,
        user: &CurrentUser,
    ) -> Result<CustomerOut, AppError> {
        user.require_any_role(&[Role::Manager, Role::Clerk])?;

        let name = req.name.trim();
        if name.is_empty() {
            return Err(AppError::biz(code::BIZ_INVALID_VALUE, "name 不能为空"));
        }

        let parent_id = match &req.parent_id {
            Some(s) if !s.is_empty() => Some(
                s.parse::<i64>()
                    .map_err(|_| AppError::biz(code::BIZ_INVALID_VALUE, "parent_id 非整数"))?,
            ),
            _ => None,
        };

        // L1 必须带 serial_prefix，L2 必须不带
        let prefix_upper: Option<String> = match (&req.serial_prefix, parent_id.is_some()) {
            (Some(s), false) if !s.is_empty() => Some(check_serial_prefix(s)?),
            (Some(_), true) => {
                return Err(AppError::biz(
                    code::BIZ_INVALID_VALUE,
                    "二级客户不得提供 serial_prefix",
                ));
            }
            (None, false) => {
                return Err(AppError::biz(
                    code::BIZ_INVALID_VALUE,
                    "一级客户必须提供 serial_prefix",
                ));
            }
            _ => None,
        };

        let id = self.snowflake.next_id();
        let c = repo
            .create(id, name, parent_id, prefix_upper.as_deref(), user.id)
            .await
            .map_err(
                |e| match e.as_database_error().and_then(|d| d.code()).as_deref() {
                    // uk_t_customer_root_prefix：同 L1 不可重 prefix
                    Some("23505") => AppError::biz(code::BIZ_INVALID_VALUE, "serial_prefix 已存在"),
                    _ => AppError::from(e),
                },
            )?;

        Ok(to_customer_out(c, None))
    }

    pub async fn update_customer<R: CustomerRepoTrait>(
        &self,
        mut repo: R,
        id: i64,
        req: &CustomerUpdateRequest,
        user: &CurrentUser,
    ) -> Result<CustomerOut, AppError> {
        user.require_any_role(&[Role::Manager, Role::Clerk])?;

        let current = repo
            .get_by_id(id, false)
            .await?
            .ok_or_else(customer_not_found)?;

        // name: Some("") ⇒ 显式清空（拒）；None ⇒ 不修改
        let name_update: Option<&str> = match req.name.as_deref() {
            Some(s) => {
                let t = s.trim();
                if t.is_empty() {
                    return Err(AppError::biz(code::BIZ_INVALID_VALUE, "name 不能为空"));
                }
                Some(t)
            }
            None => None,
        };

        // serial_prefix 三态编码：None ⇒ 不改；Some(None) ⇒ 清空；Some(Some(v)) ⇒ 改
        // 把改值场景的 owned String 保留到 await 之后（避免生命周期/借用与 `?` 早返冲突）。
        let new_prefix_owned: Option<String>;
        let prefix_update: Option<Option<&str>> = match &req.serial_prefix {
            None => None,
            Some(None) => {
                // 显式清空：仅 L1 允许
                if current.parent_id.is_none() {
                    Some(None)
                } else {
                    return Err(AppError::biz(
                        code::BIZ_INVALID_VALUE,
                        "二级客户无需清空 serial_prefix",
                    ));
                }
            }
            Some(Some(s)) => {
                // serial_prefix 只在 L1 可改（L2 的 prefix 由 L1 派生）
                if current.parent_id.is_some() {
                    return Err(AppError::biz(
                        code::BIZ_INVALID_VALUE,
                        "二级客户不得修改 serial_prefix",
                    ));
                }
                let upper = check_serial_prefix(s)?;
                new_prefix_owned = Some(upper);
                Some(Some(new_prefix_owned.as_deref().unwrap()))
            }
        };

        // parent_id：暂不在 update 阶段支持（与 Python `exclude_unset` 行为对齐：
        // Python 也只允许改 name / serial_prefix；移级是单独操作）。
        if req.parent_id.is_some() {
            return Err(AppError::biz(
                code::BIZ_INVALID_VALUE,
                "本接口不支持修改 parent_id，请走 soft-delete + 重建",
            ));
        }

        let affected = repo
            .update(id, current.version, name_update, prefix_update, user.id)
            .await
            .map_err(
                |e| match e.as_database_error().and_then(|d| d.code()).as_deref() {
                    // uq_t_customer_root_prefix：把 L1 的 prefix 改成另一个 L1 已占用的值
                    Some("23505") => AppError::biz(code::BIZ_INVALID_VALUE, "serial_prefix 已存在"),
                    _ => AppError::from(e),
                },
            )?;
        if affected == 0 {
            return Err(version_conflict());
        }

        // 回读最新行 + 父客户名
        self.get_customer(repo, id, user).await
    }

    /// 软删：置 `deleted_at = now()` + `version + 1`。
    /// 软删前查 `t_part.customer_id` 与 `t_assembly.customer_id` 非软删引用计数，
    /// 大于 0 即拒（20113 BIZ_CUSTOMER_IN_USE）。引用计数 SQL 已下沉到
    /// `CustomerRepoTrait::count_parts_using_customer` 与
    /// `CustomerRepoTrait::count_assemblies_using_customer`。
    pub async fn soft_delete_customer<R: CustomerRepoTrait>(
        &self,
        mut repo: R,
        id: i64,
        user: &CurrentUser,
    ) -> Result<(), AppError> {
        user.require_any_role(&[Role::Manager, Role::Clerk])?;

        let current = repo
            .get_by_id(id, false)
            .await?
            .ok_or_else(customer_not_found)?;

        // 与 Python `service/customer.py:soft_delete_customer` 对齐：检查 part + assembly 引用
        let part_count = repo.count_parts_using_customer(id).await?;
        let assembly_count = repo.count_assemblies_using_customer(id).await?;
        if part_count > 0 || assembly_count > 0 {
            return Err(AppError::biz(
                code::BIZ_CUSTOMER_IN_USE,
                format!("客户仍被引用（part={part_count}, assembly={assembly_count}），无法软删"),
            ));
        }

        let affected = repo.soft_delete(id, current.version, user.id).await?;
        if affected == 0 {
            return Err(version_conflict());
        }
        Ok(())
    }
}

//! customer 域 CRUD service
//!
//! 列表 / 详情 / 创建 / 更新 / 软删 —— 共 5 个端点。
//!
//! ## 业务约束（service 层 enforce）
//! - 根（`parent_id IS NULL`）：`serial_prefix` 必须为单个大写字母
//! - 非根（`parent_id` 非 NULL，指向任意层级的父节点）：`serial_prefix` 必须 NULL
//!
//! 2026-10-17：客户层级由固定两层（L1 集团 + L2 叶子）放开为**任意深度**。上面两条
//! 判据本身**深度无关**（判的是「有没有 parent_id」），真正随深度变的是下面两处：
//! - `parent_id` 必须指向**已存在且未软删**的父客户（否则造出断链，根与路径都解析不出）
//! - 软删的「有子节点」闸门（见下）
//!
//! ## soft-delete 的两道闸门（2026-10-17 改写）
//! 顺序：**先子节点、再单据引用**，任一命中即拒。
//! 1. 子节点闸门（**新增**）：有任意深度的未软删子节点 ⇒ `20124 BIZ_CUSTOMER_HAS_CHILDREN`。
//!    消息里带下级节点数（= 子树大小 - 1）。
//!    ⚠️ 旧的两层实现**不**查 `t_customer.parent_id`，「L1 仍有 L2」**不会**触发任何码——
//!    子节点由前端在删除前显式 cascade。任意深度下那套隐式约定已经不成立：子节点可能挂在
//!    第 5 层、前端拿不到（列表按 50 条截断的历史问题也会让它看不见子节点），且单据挂叶子，
//!    级联删除会连带软删已被 part / assembly 引用的节点 ⇒ 改为服务端显式拒绝。
//! 2. 单据引用闸门（不变）：`t_part.customer_id` 与 `t_assembly.customer_id` 的非软删
//!    引用计数，>0 即拒软删，抛 `20113 BIZ_CUSTOMER_IN_USE`。对齐 Python
//!    `service/customer.py:soft_delete_customer`。跨域 inline SQL 已下沉到
//!    `CustomerRepoTrait::count_parts_using_customer` / `count_assemblies_using_customer`。
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

/// 2026-10-17：**不传 `limit` ⇒ 不分页、返回全量**（旧的 `DEFAULT_LIMIT = 50`
/// 已删除）。客户列表的语义是「整棵树」而不是「一页行」：前端 `listCustomers()` 不传
/// limit，把整份列表当全集用（拼树 / 反查祖先 / 做 label）。旧口径下节点一多就
/// **静默截断**——筛选器里凭空少一批客户且零报错，比报错更难查。
/// 显式传了 `limit` 才走分页，并夹到 `MAX_LIMIT`。
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

        // 2026-10-17：`None` = 不分页（全量，见 MAX_LIMIT 上方口径）；`Some` 才夹到
        // `[1, MAX_LIMIT]`。`offset` 只在分页模式下生效：没有页就没有「第几页」。
        let limit = query.limit.map(|l| l.clamp(1, MAX_LIMIT));
        let offset = if limit.is_some() {
            query.offset.unwrap_or(0).max(0)
        } else {
            0
        };
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

        // 全量模式下 `limit` 回填**实际行数**、offset 恒 0：前端 `CustomerListResult`
        // 有这两个字段，缺了会与 Python 旧响应对不上（那个年代恒定 50/0）。
        let out_limit = limit.unwrap_or(items.len() as i64);
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
            limit: out_limit,
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
            Some(pid) => repo.get_by_id(pid, true).await?.map(|p| p.name),
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

        // 2026-10-17 新增：层级放开为任意深度后，父节点可以是**任意层级**的客户，
        // 但必须真实存在且未软删 —— 否则造出一条断链，之后 `root_id_of`（派序列号取根
        // 的 serial_prefix）与 `customer_path` 都解析不出根，而现场看到的是「建客户
        // 成功、之后派号报 20308 / 路径少一段」，排查方向完全错位。DB 侧对 parent_id
        // **无**约束（无自 FK），这道校验只能在此处做。
        if let Some(pid) = parent_id
            && repo.get_by_id(pid, false).await?.is_none()
        {
            return Err(AppError::biz(
                code::BIZ_CUSTOMER_NOT_FOUND,
                format!("父客户 {pid} 不存在或已软删"),
            ));
        }

        // 根必须带 serial_prefix，非根必须不带（判据是「有没有 parent_id」，与层级深度无关）
        let prefix_upper: Option<String> = match (&req.serial_prefix, parent_id.is_some()) {
            (Some(s), false) if !s.is_empty() => Some(check_serial_prefix(s)?),
            (Some(_), true) => {
                return Err(AppError::biz(
                    code::BIZ_INVALID_VALUE,
                    "非一级客户不得提供 serial_prefix",
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
                        "非一级客户无需清空 serial_prefix",
                    ));
                }
            }
            Some(Some(s)) => {
                // serial_prefix 只在一级客户可改（序列号前缀是根的属性，由根派生）
                if current.parent_id.is_some() {
                    return Err(AppError::biz(
                        code::BIZ_INVALID_VALUE,
                        "非一级客户不得修改 serial_prefix",
                    ));
                }
                let upper = check_serial_prefix(s)?;
                new_prefix_owned = Some(upper);
                Some(Some(new_prefix_owned.as_deref().unwrap()))
            }
        };

        // 2026-10-17：**刻意**不支持修改 `parent_id`（客户层级不可移动）——
        // 这是产品决定而非「暂未实现」。移动节点会让挂在整棵子树上的
        // l1_customer_name / customer_path 全部漂移，而已派发的序列号前缀又是**按建单
        // 时**的根取的，事后移级无法回溯修正；要做也只能是「删除后在目标父节点下重建」。
        if req.parent_id.is_some() {
            return Err(AppError::biz(
                code::BIZ_INVALID_VALUE,
                "不支持修改 parent_id（客户层级不可移动）；请删除后在目标父节点下重建",
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
    ///
    /// 两道闸门（2026-10-17，详见模块 doc）：
    /// 1. 有未软删子节点 ⇒ `20124 BIZ_CUSTOMER_HAS_CHILDREN`（消息带下级节点数）
    /// 2. `t_part.customer_id` / `t_assembly.customer_id` 非软删引用计数 > 0 ⇒
    ///    `20113 BIZ_CUSTOMER_IN_USE`。引用计数 SQL 已下沉到
    ///    `CustomerRepoTrait::count_parts_using_customer` 与
    ///    `CustomerRepoTrait::count_assemblies_using_customer`。
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

        // 2026-10-17 新增第一道闸门：有子节点即拒（任意深度，见模块 doc）。
        // 放在单据引用检查**之前**：子节点是更「浅」的原因，先报它才不会让用户
        // 先删掉一堆零件才发现真正的原因是「这客户下面还有下级」。
        // `subtree_ids` 只用来取消息里的数量（子树大小 - 1 = 下级节点数）。
        if repo.has_children(id).await? {
            let child_count = repo.subtree_ids(id).await?.len().saturating_sub(1);
            return Err(AppError::biz(
                code::BIZ_CUSTOMER_HAS_CHILDREN,
                format!("客户仍有 {child_count} 个下级节点，无法软删（请先处理全部下级节点）"),
            ));
        }

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

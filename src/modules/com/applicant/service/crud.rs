//! applicant 域 CRUD service
//!
//! 列表 / 详情 / 创建 / 更新 / 软删 —— 共 5 个端点。
//!
//! ## 业务规则（与 Python 一致）
//! - 角色：Manager + Clerk 可读写（service 层二次守卫）
//! - customer_id 必须指向 L1（一级集团 parent_id IS NULL）—— 不允许挂到 L2
//! - 同一 L1 下姓名唯一（DB partial unique 兜底）
//! - 软删时被 t_part.applicant_name 引用 → 拒软删（21004）
//!
//! 2026-09-22 重构：原 `service.rs::lookup_customer_names` 的 inline t_customer SQL
//! 收敛到 `CustomerRepo::lookup_names`；service 跨域调用走 `CustomerRepo::lookup_names`
//! ZST 静态方法（详见 `list_applicants` doc）。
//!
//! ## 事务边界（2026-09-22 重构对齐 iam 范本）
//! 事务移交 handler（与 20 个 handler 文件现状对齐）：service 仅业务逻辑，所有跨 repo
//! 操作经 `repo: R`（by-value；`R: ApplicantRepoTrait`）参数传入——handler/service 借
//! `&mut *tx` / `&mut *conn` 喂给 `ApplicantRepoTrait` trait（trait 已直接
//! `impl for &mut PgConnection`）。service 不知事务——handler `pool.begin()` +
//! `tx.commit()` 包外。
//!
//! ## list_applicants 跨域特例
//! `list_applicants` 需同时跨域（`ApplicantRepoTrait::list_with_filters` +
//! `CustomerRepoTrait::lookup_names`）。两个 trait 都含 `count_with_filters` 同名方法，
//! UFCS / method-call 都会触发 E0034 歧义；rust 2024 borrow checker 也不允许同一连接
//! 喂两次 `&mut *conn`。故 list_applicants 单列：直接收 `&mut PgConnection`，按需
//! ZST 静态方法内 `&mut *conn` 顺序 reborrow（与 shelf `picker.rs` 范本一致——
//! picker 同样不抽 trait 直接收 conn 静态调用）。
//!
//! `ApplicantService` 是带字段 struct（仅 `snowflake`：iam 范本 §6）；get/create/
//! update/soft_delete 4 端点方法签名 `<R: ApplicantRepoTrait>(&self, mut repo: R, ...)`，
//! 生产 `R = &mut PgConnection`。

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::auth::rbac::{CurrentUser, Role};
use crate::infra::snowflake::SnowflakeIdGenerator;
use crate::modules::com::applicant::dto::*;
use crate::modules::com::applicant::model::TApplicant;
use crate::modules::com::applicant::repo::{ApplicantRepo as ApplicantRepoImpl, ApplicantRepoTrait};
use crate::modules::com::customer::repo::CustomerRepo as CustomerRepoImpl;
use crate::shared::error::{AppError, code};

const DEFAULT_LIMIT: i64 = 100;
const MAX_LIMIT: i64 = 500;

fn applicant_not_found() -> AppError {
    AppError::biz(code::BIZ_APPLICANT_NOT_FOUND, "申请人不存在")
}
fn version_conflict() -> AppError {
    AppError::biz(code::VERSION_CONFLICT, "乐观锁冲突，请刷新后重试")
}
fn duplicate_name() -> AppError {
    AppError::biz(
        code::BIZ_APPLICANT_DUPLICATE_NAME,
        "同一客户下已存在同名申请人",
    )
}
fn bad_customer() -> AppError {
    AppError::biz(
        code::BIZ_APPLICANT_BAD_CUSTOMER,
        "customer_id 必须指向一级客户（L1）",
    )
}
fn in_use() -> AppError {
    AppError::biz(code::BIZ_APPLICANT_IN_USE, "申请人被零件引用，无法软删")
}

fn require_role(current: &CurrentUser) -> Result<(), AppError> {
    current.require_any_role(&[Role::Manager, Role::Clerk])
}

fn to_applicant_out(a: TApplicant, customer_name: Option<String>) -> ApplicantOut {
    ApplicantOut {
        id: a.id,
        name: a.name,
        customer_id: a.customer_id,
        customer_name,
        version: a.version,
        created_at: a.created_at,
        updated_at: a.updated_at,
    }
}

/// applicant 域 service（2026-09-22 重构后）
///
/// 字段仅 `snowflake`（事务已移交 handler；跨域 `CustomerRepo` 借 `&mut *tx` 喂两次
/// reborrow，service 不持第二个 repo 引用）。实例为轻壳，可直接
/// `Arc<ApplicantService>` 存 `AppState`；方法签名收 `mut repo: R, mut customer_repo: R3`
/// （by-value；生产 `R = R3 = &mut PgConnection`）。
pub struct ApplicantService {
    snowflake: Arc<SnowflakeIdGenerator>,
}

impl ApplicantService {
    /// 构造：仅需雪花 ID 生成器。
    pub fn new(snowflake: Arc<SnowflakeIdGenerator>) -> Self {
        Self { snowflake }
    }

    /// list_applicants 同时跨域（ApplicantRepo + CustomerRepo::lookup_names）。
    /// 收 `&mut PgConnection` 直接连接而非双 trait 形参——原因：
    /// 1. rust 2024 borrow checker 不允许对同一 `&mut PgConnection` 做两次 `&mut *`
    ///    （E0499，rustc 在 fn 调用参数同时 reborrow 会拒绝）。
    /// 2. 即便把两个 trait 合并为 `R: ApplicantRepoTrait + CustomerRepoTrait`，两 trait
    ///    都含 `get_by_id` / `list_with_filters` / `count_with_filters` / `create` /
    ///    `update` / `soft_delete` 重名方法，方法调用会歧义（E0034）。
    /// 3. 故 list_applicants 单列：直接收 `&mut PgConnection`，按需 trait 方法内 reborrow
    ///    顺序调用（同 shelf 范本 picker.rs 不抽 trait，直接收 conn 静态调用）。
    ///
    /// 其它 4 端点（get/create/update/soft_delete）只触 applicant 单 trait，保留
    /// 泛型 `<R: ApplicantRepoTrait>` 形态；测试可通过 `MockApplicantRepo` 注入。
    pub async fn list_applicants(
        &self,
        conn: &mut sqlx::PgConnection,
        query: &ApplicantListQuery,
        current: &CurrentUser,
    ) -> Result<ApplicantListOut, AppError> {
        require_role(current)?;

        let customer_id = match &query.customer_id {
            Some(s) => Some(s.parse::<i64>().map_err(|_| bad_customer())?),
            None => None,
        };
        let name_like = query.name_like.as_deref().and_then(|s| {
            let t = s.trim();
            if t.is_empty() { None } else { Some(t) }
        });
        let limit = query.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
        let offset = query.offset.unwrap_or(0).max(0);

        // 直接走 ZST 静态方法（与原 `customer/service.rs` 等同模式）：两 trait 都含
        // `count_with_filters` 同名方法，UFCS / method-call 都会触发 E0034 歧义；
        // 改走 `ApplicantRepoImpl::xxx(&mut *conn, ...)` 静态调用完美避歧义。
        // 顺序 reborrow 每次都是新的一次性借用窗口，borrow checker 接受。
        let rows = ApplicantRepoImpl::list_with_filters(
            &mut *conn,
            customer_id,
            name_like,
            limit,
            offset,
        )
        .await?;
        let total =
            ApplicantRepoImpl::count_with_filters(&mut *conn, customer_id, name_like).await?;

        // 一次性补 customer_name（防 N+1；空列表短路 → lookup_names 内部空数组短路）。
        let ids: Vec<i64> = rows.iter().map(|a| a.customer_id).collect();
        let names = if ids.is_empty() {
            Vec::new()
        } else {
            let unique: Vec<i64> = {
                let mut s = HashSet::new();
                for id in &ids {
                    s.insert(*id);
                }
                s.into_iter().collect()
            };
            let pairs = CustomerRepoImpl::lookup_names(&mut *conn, &unique).await?;
            let map: HashMap<i64, String> = pairs.into_iter().collect();
            ids.into_iter().map(|id| map.get(&id).cloned()).collect()
        };
        let items = rows
            .into_iter()
            .zip(names)
            .map(|(a, n)| to_applicant_out(a, n))
            .collect();

        Ok(ApplicantListOut {
            items,
            total,
            limit,
            offset,
        })
    }

    pub async fn get_applicant<R: ApplicantRepoTrait>(
        &self,
        mut repo: R,
        id: i64,
        current: &CurrentUser,
    ) -> Result<ApplicantOut, AppError> {
        require_role(current)?;
        let row = repo
            .get_by_id(id, false)
            .await?
            .ok_or_else(applicant_not_found)?;
        let customer_name = repo.customer_name(row.customer_id).await?;
        Ok(to_applicant_out(row, customer_name))
    }

    pub async fn create_applicant<R: ApplicantRepoTrait>(
        &self,
        mut repo: R,
        req: &ApplicantCreateRequest,
        current: &CurrentUser,
    ) -> Result<ApplicantOut, AppError> {
        require_role(current)?;

        let name = req.name.trim();
        if name.is_empty() {
            return Err(AppError::biz(code::BIZ_INVALID_VALUE, "申请人姓名不能为空"));
        }
        let customer_id = req.customer_id.parse::<i64>().map_err(|_| bad_customer())?;

        // L1 校验
        if !repo.l1_customer_exists(customer_id).await? {
            return Err(bad_customer());
        }
        // 重名校验（DB partial unique 兜底，但前置可给更友好错误码）
        if repo
            .find_by_name_and_customer(name, customer_id, false)
            .await?
            .is_some()
        {
            return Err(duplicate_name());
        }

        let new_id = self.snowflake.next_id();
        repo.create(new_id, name, customer_id, Some(current.id)).await?;

        // 重读一次拿 server-side defaults（version / created_at / updated_at）
        let row = repo
            .get_by_id(new_id, false)
            .await?
            .ok_or_else(applicant_not_found)?;
        let customer_name = repo.customer_name(row.customer_id).await?;
        Ok(to_applicant_out(row, customer_name))
    }

    pub async fn update_applicant<R: ApplicantRepoTrait>(
        &self,
        mut repo: R,
        id: i64,
        req: &ApplicantUpdateRequest,
        current: &CurrentUser,
    ) -> Result<ApplicantOut, AppError> {
        require_role(current)?;

        let row = repo
            .get_by_id(id, false)
            .await?
            .ok_or_else(applicant_not_found)?;

        // name: Some("") ⇒ 显式清空（拒，校验失败）；None ⇒ 不修改
        let new_name: Option<&str> = match req.name.as_deref() {
            Some(s) => {
                let t = s.trim();
                if t.is_empty() {
                    return Err(AppError::biz(code::BIZ_INVALID_VALUE, "申请人姓名不能为空"));
                }
                Some(t)
            }
            None => None,
        };

        // customer_id：Some(s) ⇒ 解析并 L1 校验；None ⇒ 不修改
        let new_customer_id: Option<i64> = match req.customer_id.as_deref() {
            Some(s) => Some(s.parse::<i64>().map_err(|_| bad_customer())?),
            None => None,
        };
        if let Some(cid) = new_customer_id
            && !repo.l1_customer_exists(cid).await?
        {
            return Err(bad_customer());
        }

        let affected = repo
            .update(id, row.version, new_name, new_customer_id, Some(current.id))
            .await?;
        if affected == 0 {
            return Err(version_conflict());
        }

        let updated = repo
            .get_by_id(id, false)
            .await?
            .ok_or_else(applicant_not_found)?;
        let customer_name = repo.customer_name(updated.customer_id).await?;
        Ok(to_applicant_out(updated, customer_name))
    }

    pub async fn soft_delete_applicant<R: ApplicantRepoTrait>(
        &self,
        mut repo: R,
        id: i64,
        current: &CurrentUser,
    ) -> Result<(), AppError> {
        require_role(current)?;

        let row = repo
            .get_by_id(id, false)
            .await?
            .ok_or_else(applicant_not_found)?;

        // in-use 校验：被 t_part 引用则拒
        let ref_count = repo
            .count_parts_using_applicant_name(&row.name, row.customer_id)
            .await?;
        if ref_count > 0 {
            return Err(in_use());
        }

        let affected = repo
            .soft_delete(id, row.version, Some(current.id))
            .await?;
        if affected == 0 {
            return Err(version_conflict());
        }
        Ok(())
    }
}

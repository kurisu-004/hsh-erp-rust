//! applicant 域业务逻辑
//!
//! 对应 Python myERP/service/applicant.py。
//!
//! ## 业务规则（与 Python 一致）
//! - 角色：Manager + Clerk 可读写（service 层二次守卫）
//! - customer_id 必须指向 L1（一级集团 parent_id IS NULL）—— 不允许挂到 L2
//! - 同一 L1 下姓名唯一（DB partial unique 兜底）
//! - 软删时被 t_part.applicant_name 引用 → 拒软删（21004）

use std::collections::{HashMap, HashSet};

use sqlx::PgConnection;

use crate::auth::rbac::{CurrentUser, Role};
use crate::infra::snowflake::SnowflakeIdGenerator;
use crate::modules::applicant::dto::*;
use crate::modules::applicant::model::TApplicant;
use crate::modules::applicant::repo::ApplicantRepo;
use crate::shared::error::{code, AppError};

const DEFAULT_LIMIT: i64 = 100;
const MAX_LIMIT: i64 = 500;

fn applicant_not_found() -> AppError {
    AppError::biz(code::BIZ_APPLICANT_NOT_FOUND, "申请人不存在")
}
fn version_conflict() -> AppError {
    AppError::biz(code::VERSION_CONFLICT, "乐观锁冲突，请刷新后重试")
}
fn duplicate_name() -> AppError {
    AppError::biz(code::BIZ_APPLICANT_DUPLICATE_NAME, "同一客户下已存在同名申请人")
}
fn bad_customer() -> AppError {
    AppError::biz(code::BIZ_APPLICANT_BAD_CUSTOMER, "customer_id 必须指向一级客户（L1）")
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

pub struct ApplicantService;

impl ApplicantService {
    pub async fn list_applicants(
        conn: &mut PgConnection,
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

        let rows = ApplicantRepo::list_with_filters(
            &mut *conn, customer_id, name_like, limit, offset,
        ).await?;
        let total = ApplicantRepo::count_with_filters(
            &mut *conn, customer_id, name_like,
        ).await?;

        // 一次性补 customer_name（防 N+1；空列表短路）
        let names = if rows.is_empty() {
            vec![]
        } else {
            lookup_customer_names(&mut *conn, rows.iter().map(|a| a.customer_id).collect()).await?
        };
        let items = rows
            .into_iter()
            .zip(names)
            .map(|(a, n)| to_applicant_out(a, n))
            .collect();

        Ok(ApplicantListOut { items, total, limit, offset })
    }

    pub async fn get_applicant(
        conn: &mut PgConnection,
        id: i64,
        current: &CurrentUser,
    ) -> Result<ApplicantOut, AppError> {
        require_role(current)?;
        let row = ApplicantRepo::get_by_id(&mut *conn, id, false).await?
            .ok_or_else(applicant_not_found)?;
        let customer_name = ApplicantRepo::customer_name(&mut *conn, row.customer_id).await?;
        Ok(to_applicant_out(row, customer_name))
    }

    pub async fn create_applicant(
        conn: &mut PgConnection,
        sf: &SnowflakeIdGenerator,
        req: &ApplicantCreateRequest,
        current: &CurrentUser,
    ) -> Result<ApplicantOut, AppError> {
        require_role(current)?;

        let name = req.name.trim();
        if name.is_empty() {
            return Err(AppError::biz(
                code::BIZ_INVALID_VALUE,
                "申请人姓名不能为空",
            ));
        }
        let customer_id = req.customer_id.parse::<i64>().map_err(|_| bad_customer())?;

        // L1 校验
        if !ApplicantRepo::l1_customer_exists(&mut *conn, customer_id).await? {
            return Err(bad_customer());
        }
        // 重名校验（DB partial unique 兜底，但前置可给更友好错误码）
        if ApplicantRepo::find_by_name_and_customer(&mut *conn, name, customer_id, false)
            .await?
            .is_some()
        {
            return Err(duplicate_name());
        }

        let new_id = sf.next_id();
        ApplicantRepo::create(
            &mut *conn, new_id, name, customer_id, Some(current.id),
        ).await?;

        // 重读一次拿 server-side defaults（version / created_at / updated_at）
        let row = ApplicantRepo::get_by_id(&mut *conn, new_id, false).await?
            .ok_or_else(applicant_not_found)?;
        let customer_name = ApplicantRepo::customer_name(&mut *conn, row.customer_id).await?;
        Ok(to_applicant_out(row, customer_name))
    }

    pub async fn update_applicant(
        conn: &mut PgConnection,
        id: i64,
        req: &ApplicantUpdateRequest,
        current: &CurrentUser,
    ) -> Result<ApplicantOut, AppError> {
        require_role(current)?;

        let row = ApplicantRepo::get_by_id(&mut *conn, id, false).await?
            .ok_or_else(applicant_not_found)?;

        // name: Some("") ⇒ 显式清空（拒，校验失败）；None ⇒ 不修改
        let new_name: Option<&str> = match req.name.as_deref() {
            Some(s) => {
                let t = s.trim();
                if t.is_empty() {
                    return Err(AppError::biz(
                        code::BIZ_INVALID_VALUE,
                        "申请人姓名不能为空",
                    ));
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
            && !ApplicantRepo::l1_customer_exists(&mut *conn, cid).await?
        {
            return Err(bad_customer());
        }

        let affected = ApplicantRepo::update(
            &mut *conn,
            id,
            row.version,
            new_name,
            new_customer_id,
            Some(current.id),
        ).await?;
        if affected == 0 {
            return Err(version_conflict());
        }

        let updated = ApplicantRepo::get_by_id(&mut *conn, id, false).await?
            .ok_or_else(applicant_not_found)?;
        let customer_name = ApplicantRepo::customer_name(&mut *conn, updated.customer_id).await?;
        Ok(to_applicant_out(updated, customer_name))
    }

    pub async fn soft_delete_applicant(
        conn: &mut PgConnection,
        id: i64,
        current: &CurrentUser,
    ) -> Result<(), AppError> {
        require_role(current)?;

        let row = ApplicantRepo::get_by_id(&mut *conn, id, false).await?
            .ok_or_else(applicant_not_found)?;

        // in-use 校验：被 t_part 引用则拒
        let ref_count = ApplicantRepo::count_parts_using_applicant_name(
            &mut *conn, &row.name, row.customer_id,
        ).await?;
        if ref_count > 0 {
            return Err(in_use());
        }

        let affected = ApplicantRepo::soft_delete(
            &mut *conn, id, row.version, Some(current.id),
        ).await?;
        if affected == 0 {
            return Err(version_conflict());
        }
        Ok(())
    }
}

/// 一次性批量查 customer_name，unique by id（防 N+1）。
///
/// 输入按出现顺序返回 `Option<String>`；列表为空时短路返回空 `Vec`，
/// 避免对空 IN 子句做无谓 round-trip。
async fn lookup_customer_names(
    conn: &mut PgConnection,
    ids: Vec<i64>,
) -> Result<Vec<Option<String>>, AppError> {
    if ids.is_empty() {
        return Ok(vec![]);
    }
    let unique: Vec<i64> = {
        let mut s = HashSet::new();
        for id in &ids {
            s.insert(*id);
        }
        s.into_iter().collect()
    };
    let names: Vec<(i64, Option<String>)> = sqlx::query_as(
        "SELECT id, name FROM t_customer WHERE id = ANY($1) AND deleted_at IS NULL",
    )
    .bind(&unique)
    .fetch_all(&mut *conn)
    .await
    .map_err(AppError::from)?;
    let map: HashMap<i64, String> = names
        .into_iter()
        .filter_map(|(i, n)| n.map(|s| (i, s)))
        .collect();
    Ok(ids.into_iter().map(|id| map.get(&id).cloned()).collect())
}
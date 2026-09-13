//! _e2e seed handlers —— 2026-09-14 新增
//!
//! 全部 handler 开头先 `e2e_guard(&state)?`；handler 签名不取 `current: CurrentUser`
//! 以绕过 JWT 校验。各域直接走 repo（不走 service，service 有 RBAC 校验）。
//!
//! 所有 SQL 使用动态 `sqlx::query` / `query_scalar`（不依赖 .sqlx 离线元数据）。

use std::sync::Arc;

use axum::extract::State;
use axum::Json;
use chrono::NaiveDateTime;
use rust_decimal::Decimal;
use sqlx::PgConnection;
use std::str::FromStr;

use crate::auth::password;
use crate::infra::clock::now_naive;
use crate::modules::applicant::repo::ApplicantRepo;
use crate::modules::customer::repo::CustomerRepo;
use crate::modules::user::repo::{UserInsert, UserRepo, UserRoleInsert, UserRoleRepo};
use crate::modules::worker::repo::WorkerRepo;
use crate::shared::error::{code, AppError};
use crate::shared::response::R;
use crate::state::AppState;

use super::dto::{
    ProbeResp, ResetResp, RevokeSessionReq, SeedApplicantReq, SeedCreatedResp, SeedCustomerReq,
    SeedDeliveryNoteReq, SeedOutsourceCompanyReq, SeedOutsourceQuoteReq, SeedPartReq, SeedUserReq,
    SeedWorkerReq,
};
use super::e2e_guard;

/// 固定 MANAGER id（与 alembic prod_data seed 对齐；用作 created_by / updated_by 占位）
const SEED_ACTOR_ID: i64 = 1;

/// 把 e2e_seeded 元数据写入 `t_e2e_seeded`（reset 的依据）。
async fn mark_seeded(
    tx: &mut PgConnection,
    entity: &str,
    entity_id: i64,
) -> Result<(), AppError> {
    sqlx::query("INSERT INTO t_e2e_seeded (entity, entity_id) VALUES ($1, $2) ON CONFLICT DO NOTHING")
        .bind(entity)
        .bind(entity_id)
        .execute(&mut *tx)
        .await?;
    Ok(())
}

fn created_by_some() -> Option<i64> {
    Some(SEED_ACTOR_ID)
}

// ===========================================================================
//  POST /probe
// ===========================================================================

pub async fn probe(State(state): State<Arc<AppState>>) -> Result<Json<R<ProbeResp>>, AppError> {
    e2e_guard(&state)?;
    Ok(Json(R::ok(ProbeResp {
        status: "ok",
        enabled: true,
    })))
}

// ===========================================================================
//  POST /reset
// ===========================================================================

pub async fn reset(State(state): State<Arc<AppState>>) -> Result<Json<R<ResetResp>>, AppError> {
    e2e_guard(&state)?;
    let mut tx = state.pool.begin().await?;
    // RETURNING 1 取每行的 1；不返回原表数据；count = fetch_all().len()
    let rows = sqlx::query_scalar::<_, i32>("DELETE FROM t_e2e_seeded RETURNING 1")
        .fetch_all(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Json(R::ok(ResetResp {
        cleared: rows.len() as i64,
    })))
}

// ===========================================================================
//  POST /seed/customer
// ===========================================================================

pub async fn seed_customer(
    State(state): State<Arc<AppState>>,
    Json(req): Json<SeedCustomerReq>,
) -> Result<Json<R<SeedCreatedResp>>, AppError> {
    e2e_guard(&state)?;
    let mut tx = state.pool.begin().await?;

    let parent_id: Option<i64> = match req.parent_id.as_deref() {
        Some(s) if !s.is_empty() => Some(s.parse::<i64>().map_err(|_| {
            AppError::biz(code::BAD_REQUEST, format!("parent_id 不是合法 i64: {s}"))
        })?),
        _ => None,
    };
    let serial_prefix: Option<&str> = req
        .serial_prefix
        .as_deref()
        .filter(|s| !s.is_empty());

    let id = state.snowflake.next_id();
    let created = CustomerRepo::create(
        &mut *tx,
        id,
        &req.name,
        parent_id,
        serial_prefix,
        SEED_ACTOR_ID,
    )
    .await?;
    mark_seeded(&mut tx, "customer", created.id).await?;
    tx.commit().await?;
    Ok(Json(R::ok(SeedCreatedResp {
        id: created.id.to_string(),
    })))
}

// ===========================================================================
//  POST /seed/applicant
// ===========================================================================

pub async fn seed_applicant(
    State(state): State<Arc<AppState>>,
    Json(req): Json<SeedApplicantReq>,
) -> Result<Json<R<SeedCreatedResp>>, AppError> {
    e2e_guard(&state)?;
    let mut tx = state.pool.begin().await?;

    let customer_id: i64 = req
        .customer_id
        .parse::<i64>()
        .map_err(|_| AppError::biz(code::BAD_REQUEST, "customer_id 不是合法 i64"))?;

    let id = state.snowflake.next_id();
    ApplicantRepo::create(&mut *tx, id, &req.name, customer_id, created_by_some()).await?;
    mark_seeded(&mut tx, "applicant", id).await?;
    tx.commit().await?;
    Ok(Json(R::ok(SeedCreatedResp { id: id.to_string() })))
}

// ===========================================================================
//  POST /seed/worker
// ===========================================================================

pub async fn seed_worker(
    State(state): State<Arc<AppState>>,
    Json(req): Json<SeedWorkerReq>,
) -> Result<Json<R<SeedCreatedResp>>, AppError> {
    e2e_guard(&state)?;
    let mut tx = state.pool.begin().await?;

    // 工种 code → id（工种必须存在，否则 404 20901）
    let work_type_id: Option<i64> = sqlx::query_scalar(
        "SELECT id FROM t_work_type WHERE code = $1 AND deleted_at IS NULL",
    )
    .bind(&req.work_type_code)
    .fetch_optional(&mut *tx)
    .await?;
    let work_type_id = work_type_id.ok_or_else(|| {
        AppError::biz(
            code::BIZ_WORK_TYPE_NOT_FOUND,
            format!("工种不存在: {}", req.work_type_code),
        )
    })?;

    // 工人 badge_code：缺省用 e2e-w-<snowflake>（同一 test 内多次 seed 同名避免撞唯一索引）
    let snowflake_int: i64 = state.snowflake.next_id();
    // 用 snowflake id 末 6 位作唯一后缀，保证重复 name 也不撞唯一索引
    let badge_code = format!("E2E-{:06}", snowflake_int.abs() % 1_000_000);

    let id = state.snowflake.next_id();
    let created = WorkerRepo::create(
        &mut *tx,
        id,
        &badge_code,
        &req.name,
        None,
        None,
        Some(work_type_id),
        SEED_ACTOR_ID,
    )
    .await?;
    mark_seeded(&mut tx, "worker", created.id).await?;
    tx.commit().await?;
    Ok(Json(R::ok(SeedCreatedResp {
        id: created.id.to_string(),
    })))
}

// ===========================================================================
//  POST /seed/part
// ===========================================================================

pub async fn seed_part(
    State(state): State<Arc<AppState>>,
    Json(req): Json<SeedPartReq>,
) -> Result<Json<R<SeedCreatedResp>>, AppError> {
    e2e_guard(&state)?;
    let mut tx = state.pool.begin().await?;

    let customer_id: i64 = req
        .customer_id
        .parse::<i64>()
        .map_err(|_| AppError::biz(code::BAD_REQUEST, "customer_id 不是合法 i64"))?;
    let name = req.name.unwrap_or_else(|| req.applicant_name.clone());
    let drawing_no = req
        .drawing_no
        .unwrap_or_else(|| format!("E2E-DWG-{}", &req.serial));

    let id = state.snowflake.next_id();
    let now: NaiveDateTime = now_naive();
    // 动态 INSERT（.sqlx 无元数据；与 customer::repo::create 字段顺序对齐）
    sqlx::query(
        r#"INSERT INTO t_part (
              id, serial_no, name, drawing_no, applicant_name,
              quantity, unit_price, total_price,
              request_date, planned_delivery_date,
              status, is_urgent,
              customer_id,
              version,
              created_at, created_by, updated_at, updated_by
           )
           VALUES (
              $1, $2, $3, $4, $5,
              1, 0, 0,
              CURRENT_DATE, CURRENT_DATE,
              'PENDING', false,
              $6,
              0,
              $7, $8, $7, $8
           )"#,
    )
    .bind(id)
    .bind(&req.serial)
    .bind(&name)
    .bind(&drawing_no)
    .bind(&req.applicant_name)
    .bind(customer_id)
    .bind(now)
    .bind(created_by_some())
    .execute(&mut *tx)
    .await?;

    mark_seeded(&mut tx, "part", id).await?;
    tx.commit().await?;
    Ok(Json(R::ok(SeedCreatedResp {
        id: id.to_string(),
    })))
}

// ===========================================================================
//  POST /seed/outsource_company
// ===========================================================================

pub async fn seed_outsource_company(
    State(state): State<Arc<AppState>>,
    Json(req): Json<SeedOutsourceCompanyReq>,
) -> Result<Json<R<SeedCreatedResp>>, AppError> {
    e2e_guard(&state)?;
    let mut tx = state.pool.begin().await?;

    let id = state.snowflake.next_id();
    let now: NaiveDateTime = now_naive();
    sqlx::query(
        r#"INSERT INTO t_outsource_company (
              id, name, is_active, version,
              created_at, created_by, updated_at, updated_by
           )
           VALUES (
              $1, $2, true, 0,
              $3, $4, $3, $4
           )"#,
    )
    .bind(id)
    .bind(&req.name)
    .bind(now)
    .bind(created_by_some())
    .execute(&mut *tx)
    .await?;

    mark_seeded(&mut tx, "outsource_company", id).await?;
    tx.commit().await?;
    Ok(Json(R::ok(SeedCreatedResp {
        id: id.to_string(),
    })))
}

// ===========================================================================
//  POST /seed/outsource_quote
// ===========================================================================

pub async fn seed_outsource_quote(
    State(state): State<Arc<AppState>>,
    Json(req): Json<SeedOutsourceQuoteReq>,
) -> Result<Json<R<SeedCreatedResp>>, AppError> {
    e2e_guard(&state)?;
    let mut tx = state.pool.begin().await?;

    let part_id: i64 = req
        .part_id
        .parse::<i64>()
        .map_err(|_| AppError::biz(code::BAD_REQUEST, "part_id 不是合法 i64"))?;
    let company_id: i64 = req
        .company_id
        .parse::<i64>()
        .map_err(|_| AppError::biz(code::BAD_REQUEST, "company_id 不是合法 i64"))?;
    let process_id: i64 = req
        .process_id
        .parse::<i64>()
        .map_err(|_| AppError::biz(code::BAD_REQUEST, "process_id 不是合法 i64"))?;

    let id = state.snowflake.next_id();
    let now: NaiveDateTime = now_naive();
    // t_outsource_quote DDL：price numeric(12,2), status default 'DRAFT', quantity nullable, is_direct default false, is_billed default false
    sqlx::query(
        r#"INSERT INTO t_outsource_quote (
              id, part_id, outsource_company_id, process_id, price,
              status, version,
              created_at, created_by, updated_at, updated_by,
              is_billed, is_direct
           )
           VALUES (
              $1, $2, $3, $4, $5,
              'DRAFT', 0,
              $6, $7, $6, $7,
              false, false
           )"#,
    )
    .bind(id)
    .bind(part_id)
    .bind(company_id)
    .bind(process_id)
    .bind(Decimal::from_str(&format!("{}", req.price)).map_err(|e| {
        AppError::biz(code::BAD_REQUEST, format!("price 解析失败: {e}"))
    })?)
    .bind(now)
    .bind(created_by_some())
    .execute(&mut *tx)
    .await?;

    mark_seeded(&mut tx, "outsource_quote", id).await?;
    tx.commit().await?;
    Ok(Json(R::ok(SeedCreatedResp {
        id: id.to_string(),
    })))
}

// ===========================================================================
//  POST /seed/delivery_note
// ===========================================================================

pub async fn seed_delivery_note(
    State(state): State<Arc<AppState>>,
    Json(req): Json<SeedDeliveryNoteReq>,
) -> Result<Json<R<SeedCreatedResp>>, AppError> {
    e2e_guard(&state)?;
    let mut tx = state.pool.begin().await?;

    let customer_id: i64 = req
        .customer_id
        .parse::<i64>()
        .map_err(|_| AppError::biz(code::BAD_REQUEST, "customer_id 不是合法 i64"))?;

    let id = state.snowflake.next_id();
    let now: NaiveDateTime = now_naive();
    // delivery_note_no：e2e 不与 counter 抢号；用 fake "DN-E2E-<id>" 即可（uq_t_delivery_note_no_active
    // 走 partial unique on deleted_at IS NULL；新单 deleted_at NULL → 不能撞。所以用唯一后缀即可）
    let delivery_note_no = format!("DN-E2E-{:X}", id.abs() as u64);

    sqlx::query(
        r#"INSERT INTO t_delivery_note (
              id, delivery_note_no, customer_id, status,
              version,
              created_at, created_by, updated_at, updated_by,
              delivery_date
           )
           VALUES (
              $1, $2, $3, $4,
              0,
              $5, $6, $5, $6,
              CURRENT_DATE
           )"#,
    )
    .bind(id)
    .bind(&delivery_note_no)
    .bind(customer_id)
    .bind(&req.status)
    .bind(now)
    .bind(created_by_some())
    .execute(&mut *tx)
    .await?;

    mark_seeded(&mut tx, "delivery_note", id).await?;
    tx.commit().await?;
    Ok(Json(R::ok(SeedCreatedResp {
        id: id.to_string(),
    })))
}

// ===========================================================================
//  POST /seed/user
// ===========================================================================

pub async fn seed_user(
    State(state): State<Arc<AppState>>,
    Json(req): Json<SeedUserReq>,
) -> Result<Json<R<SeedCreatedResp>>, AppError> {
    e2e_guard(&state)?;
    let mut tx = state.pool.begin().await?;

    let username = req.username.trim().to_lowercase();
    let full_name = req.full_name.unwrap_or_else(|| username.clone());
    let plain = req.password.as_deref().unwrap_or("changeme");
    let password_hash = password::hash(plain)?;

    let id = state.snowflake.next_id();
    let now: NaiveDateTime = now_naive();

    // t_user（用 UserRepo.create 走 repo 校验字段顺序）
    UserRepo::create(
        &mut *tx,
        &UserInsert {
            id,
            username: username.clone(),
            password_hash,
            full_name,
            phone: req.phone,
            is_active: true,
            created_at: now,
            created_by: created_by_some(),
        },
    )
    .await?;

    // 每个 role 一行 t_user_role
    for role in &req.role_codes {
        let rid = state.snowflake.next_id();
        UserRoleRepo::create(
            &mut *tx,
            &UserRoleInsert {
                id: rid,
                user_id: id,
                role: role.clone(),
                scope_type: None,
                scope_id: None,
                created_at: now,
                created_by: created_by_some(),
            },
        )
        .await?;
    }

    mark_seeded(&mut tx, "user", id).await?;
    tx.commit().await?;
    Ok(Json(R::ok(SeedCreatedResp {
        id: id.to_string(),
    })))
}

// ===========================================================================
//  POST /revoke-session
// ===========================================================================

pub async fn revoke_session(
    State(state): State<Arc<AppState>>,
    Json(req): Json<RevokeSessionReq>,
) -> Result<Json<R<()>>, AppError> {
    e2e_guard(&state)?;
    let mut tx = state.pool.begin().await?;
    let user_id: Option<i64> = sqlx::query_scalar(
        "SELECT id FROM t_user WHERE username = $1 AND deleted_at IS NULL",
    )
    .bind(req.username.trim().to_lowercase())
    .fetch_optional(&mut *tx)
    .await?;
    let user_id = user_id.ok_or_else(|| {
        AppError::biz(code::BIZ_USER_ACCOUNT_NOT_FOUND, "用户不存在")
    })?;
    tx.commit().await?;
    // Redis session 全清（独立于 PG tx；不是事务里）
    state.session.delete_all_user_sessions(user_id).await?;
    Ok(Json(R::ok_empty()))
}

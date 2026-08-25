//! part 域 CRUD 业务逻辑
//!
//! 包含 8 个公开方法：
//! - `create_part` / `batch_create_parts` —— 工单创建
//! - `list_parts` / `get_part` / `get_part_by_serial` —— 工单查询
//! - `update_part` / `soft_delete_part` —— 工单修改
//! - `upload_drawing` —— 图纸 PDF 上传到 COS + 落 `t_part_file`
//!
//! 配合 helper（`map_create_error` / `expand_customer_id` / `lookup_customer_names`）
//! 复用错误码映射、客户过滤展开、客户名解析三段公共逻辑。

use sqlx::PgConnection;
use std::sync::Arc;

use crate::auth::rbac::{CurrentUser, Role};
use crate::infra::snowflake::SnowflakeIdGenerator;
use crate::modules::customer::repo::CustomerRepo;
use crate::modules::part::model::NewPartEvent;
use crate::modules::part::repo::part::{NewPartCreate, PartListFilters, PartUpdate};
use crate::modules::part::repo::PartRepo;
use crate::modules::part_file::model::TPartFile;
use crate::modules::part_file::repo::{hash_bytes, NewPartFile, PartFileRepo};
use crate::shared::error::{code, AppError};
use crate::state::AppState;

use super::super::dto_crud::{
    PartBatchCreateRequest, PartCreateRequest, PartDetailOut, PartListItem, PartListOut,
    PartListQuery, PartUpdateRequest,
};
use super::{BATCH_CREATE_PARTS_MAX_ITEMS, PartService};

impl PartService {
    pub async fn create_part(
        conn: &mut PgConnection,
        snowflake: &SnowflakeIdGenerator,
        req: &PartCreateRequest,
        current: &CurrentUser,
    ) -> Result<PartDetailOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk])?;
        if req.name.trim().is_empty()
            || req.drawing_no.trim().is_empty()
            || req.applicant_name.trim().is_empty()
        {
            return Err(AppError::validation(
                "name / drawing_no / applicant_name 均不可为空",
            ));
        }
        if req.quantity <= 0 {
            return Err(AppError::validation("quantity 必须 > 0"));
        }
        let _customer = CustomerRepo::get_by_id(&mut *conn, req.customer_id, false)
            .await?
            .ok_or_else(|| {
                AppError::biz(
                    code::BIZ_CUSTOMER_NOT_FOUND,
                    format!("customer {} 不存在", req.customer_id),
                )
            })?;
        let new_id = snowflake.next_id();
        let new = NewPartCreate {
            id: new_id,
            name: req.name.trim(),
            drawing_no: req.drawing_no.trim(),
            applicant_name: req.applicant_name.trim(),
            quantity: req.quantity,
            request_date: req.request_date,
            planned_delivery_date: req.planned_delivery_date,
            is_urgent: req.is_urgent,
            customer_id: req.customer_id,
            assembly_id: req.assembly_id,
            order_no: req.order_no.as_deref(),
            system_delivery_date: req.system_delivery_date,
            note: req.note.as_deref(),
            created_by: current.id,
        };
        if let Err(e) = PartRepo::create_part(&mut *conn, new).await {
            return Err(map_create_error(e));
        }
        let part = PartRepo::get_part_detail(&mut *conn, new_id)
            .await?
            .ok_or_else(|| {
                AppError::biz(code::BIZ_PART_NOT_FOUND, "新建 part 查不到")
            })?;
        let (cn, l1cn) = lookup_customer_names(conn, part.customer_id).await?;
        let current_batch_id =
            PartRepo::find_current_inspection_batch_id(conn, part.id).await?;
        Ok(PartDetailOut::from_with_customer_extra(
            part,
            current_batch_id,
            cn,
            l1cn,
        ))
    }

    pub async fn batch_create_parts(
        conn: &mut PgConnection,
        snowflake: &SnowflakeIdGenerator,
        req: &PartBatchCreateRequest,
        current: &CurrentUser,
    ) -> Result<
        crate::modules::part::dto_crud::PartBatchCreateOut,
        AppError,
    > {
        current.require_any_role(&[Role::Manager, Role::Clerk])?;
        if req.items.is_empty() {
            return Err(AppError::validation("items 不能为空"));
        }
        if req.items.len() > BATCH_CREATE_PARTS_MAX_ITEMS {
            return Err(AppError::validation(format!(
                "items 数量 {} 超过上限 {}",
                req.items.len(),
                BATCH_CREATE_PARTS_MAX_ITEMS
            )));
        }
        // 共享 customer_id 一次性校验
        let _customer = CustomerRepo::get_by_id(&mut *conn, req.customer_id, false)
            .await?
            .ok_or_else(|| {
                AppError::biz(
                    code::BIZ_CUSTOMER_NOT_FOUND,
                    format!("customer {} 不存在", req.customer_id),
                )
            })?;

        let mut created = Vec::new();
        let mut failed = Vec::new();
        for (idx, item) in req.items.iter().enumerate() {
            let new_id = snowflake.next_id();
            let new = NewPartCreate {
                id: new_id,
                name: item.name.trim(),
                drawing_no: item.drawing_no.trim(),
                applicant_name: item.applicant_name.trim(),
                quantity: item.quantity,
                request_date: item.request_date,
                planned_delivery_date: item.planned_delivery_date,
                is_urgent: item.is_urgent,
                customer_id: req.customer_id,
                assembly_id: item.assembly_id,
                order_no: item.order_no.as_deref(),
                system_delivery_date: item.system_delivery_date,
                note: item.note.as_deref(),
                created_by: current.id,
            };
            // per-item savepoint：让一个 item 失败（DB 22001/23505/23503 等）后
            // 后续 item 仍能在同一外层事务内继续，避免「一处失败拖垮整批」。
            // `sp_name` 由 usize `idx` 拼出（不是用户输入），用
            // `AssertSqlSafe` 跳过 sqlx 的动态字符串审计。
            use sqlx::AssertSqlSafe;
            let sp_name = format!("batch_item_{idx}");
            sqlx::raw_sql(AssertSqlSafe(format!("SAVEPOINT {sp_name}")))
                .execute(&mut *conn)
                .await?;
            match PartRepo::create_part(&mut *conn, new).await {
                Ok(_) => {
                    sqlx::raw_sql(AssertSqlSafe(format!("RELEASE SAVEPOINT {sp_name}")))
                        .execute(&mut *conn)
                        .await?;
                    match PartRepo::get_part_detail(&mut *conn, new_id).await {
                        Ok(Some(p)) => {
                            let (cn, l1cn) = lookup_customer_names(conn, p.customer_id).await?;
                            let current_batch_id =
                                PartRepo::find_current_inspection_batch_id(conn, p.id).await?;
                            created.push(PartDetailOut::from_with_customer_extra(
                                p,
                                current_batch_id,
                                cn,
                                l1cn,
                            ));
                        }
                        _ => {
                            failed.push(crate::modules::part::dto_crud::PartBatchCreateFailure {
                                part_id: Some(new_id),
                                code: code::BIZ_PART_NOT_FOUND,
                                message: "inserted but detail lookup failed".into(),
                                item_index: idx,
                            });
                        }
                    }
                }
                Err(e) => {
                    sqlx::raw_sql(AssertSqlSafe(format!("ROLLBACK TO SAVEPOINT {sp_name}")))
                        .execute(&mut *conn)
                        .await?;
                    let mapped = map_create_error(e);
                    failed.push(crate::modules::part::dto_crud::PartBatchCreateFailure {
                        part_id: None,
                        code: mapped.code(),
                        message: format!("{mapped}"),
                        item_index: idx,
                    });
                }
            }
        }
        Ok(crate::modules::part::dto_crud::PartBatchCreateOut { created, failed })
    }

    pub async fn list_parts(
        conn: &mut PgConnection,
        query: &PartListQuery,
        current: &CurrentUser,
    ) -> Result<PartListOut, AppError> {
        current.require_any_role(&[
            Role::Manager,
            Role::Clerk,
            Role::Inspector,
            Role::CncProgrammer,
        ])?;
        let limit = query.limit.unwrap_or(50).clamp(1, 200);
        let offset = query.offset.unwrap_or(0).max(0);
        let sort_by = [
            "CREATED_AT",
            "UPDATED_AT",
            "PLANNED_DELIVERY_DATE",
            "REQUEST_DATE",
            "SERIAL_NO",
            "DRAWING_NO",
            "NAME",
        ]
        .iter()
        .find(|&&s| Some(s) == query.sort_by.as_deref())
        .copied()
        .unwrap_or("CREATED_AT");
        let sort_dir = if query
            .sort_dir
            .as_deref()
            .map(|s| s.eq_ignore_ascii_case("ASC"))
            .unwrap_or(false)
        {
            "ASC"
        } else {
            "DESC"
        };

        let customer_ids_owned: Vec<i64>;
        let customer_ids: &[i64] = if let Some(cid) = query.customer_id {
            customer_ids_owned = expand_customer_id(conn, cid).await?;
            &customer_ids_owned
        } else {
            &[]
        };
        let statuses_owned: Vec<String> = query
            .statuses
            .as_deref()
            .map(|s| {
                s.split(',')
                    .filter(|x| !x.is_empty())
                    .map(|s| s.to_string())
                    .collect()
            })
            .unwrap_or_default();

        let filters = PartListFilters {
            customer_ids,
            status: query.status.as_deref(),
            statuses: &statuses_owned,
            is_urgent: query.is_urgent,
            keyword: query.keyword.as_deref(),
            sort_by,
            sort_dir,
            limit,
            offset,
            include_deleted: false,
        };
        let rows = PartRepo::list_with_filters(&mut *conn, &filters).await?;
        let total = PartRepo::count_with_filters(&mut *conn, &filters).await?;

        let mut items = Vec::with_capacity(rows.len());
        for p in rows {
            let (cn, l1cn) = lookup_customer_names(conn, p.customer_id).await?;
            items.push(PartListItem {
                part: p,
                customer_name: cn,
                l1_customer_name: l1cn,
            });
        }
        Ok(PartListOut {
            items,
            total,
            limit,
            offset,
        })
    }

    pub async fn get_part(
        conn: &mut PgConnection,
        part_id: i64,
        current: &CurrentUser,
    ) -> Result<PartDetailOut, AppError> {
        current.require_any_role(&[
            Role::Manager,
            Role::Clerk,
            Role::Inspector,
            Role::CncProgrammer,
        ])?;
        let part = PartRepo::get_part_detail(&mut *conn, part_id)
            .await?
            .ok_or_else(|| {
                AppError::biz(
                    code::BIZ_PART_NOT_FOUND,
                    format!("part {part_id} 不存在或已删除"),
                )
            })?;
        let (cn, l1cn) = lookup_customer_names(conn, part.customer_id).await?;
        let current_batch_id =
            PartRepo::find_current_inspection_batch_id(conn, part.id).await?;
        Ok(PartDetailOut::from_with_customer_extra(
            part,
            current_batch_id,
            cn,
            l1cn,
        ))
    }

    pub async fn get_part_by_serial(
        conn: &mut PgConnection,
        serial_no: &str,
        current: &CurrentUser,
    ) -> Result<PartDetailOut, AppError> {
        current.require_any_role(&[
            Role::Manager,
            Role::Clerk,
            Role::Inspector,
            Role::CncProgrammer,
        ])?;
        let p = PartRepo::get_by_serial(&mut *conn, serial_no, false)
            .await?
            .ok_or_else(|| {
                AppError::biz(
                    code::BIZ_PART_NOT_FOUND,
                    format!("serial_no {serial_no} 不存在"),
                )
            })?;
        Self::get_part(conn, p.id, current).await
    }

    pub async fn update_part(
        conn: &mut PgConnection,
        part_id: i64,
        req: &PartUpdateRequest,
        current: &CurrentUser,
    ) -> Result<PartDetailOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk])?;
        let n = PartRepo::update_part(
            &mut *conn,
            part_id,
            req.version,
            PartUpdate {
                name: req.name.as_deref(),
                drawing_no: req.drawing_no.as_deref(),
                applicant_name: req.applicant_name.as_deref(),
                quantity: req.quantity,
                order_no: req.order_no.as_deref(),
                system_delivery_date: req.system_delivery_date,
                planned_delivery_date: req.planned_delivery_date,
                actual_delivery_date: req.actual_delivery_date,
                note: req.note.as_deref(),
                is_urgent: req.is_urgent,
                updated_by: current.id,
            },
        )
        .await?;
        if n == 0 {
            return Err(AppError::biz(
                code::VERSION_CONFLICT,
                format!("part {part_id} 版本冲突或已删除"),
            ));
        }
        Self::get_part(conn, part_id, current).await
    }

    pub async fn soft_delete_part(
        conn: &mut PgConnection,
        snowflake: &SnowflakeIdGenerator,
        part_id: i64,
        expected_version: i32,
        current: &CurrentUser,
    ) -> Result<(), AppError> {
        current.require_role(Role::Manager)?;
        let n = PartRepo::soft_delete_part(&mut *conn, part_id, expected_version, current.id).await?;
        match n {
            1 => {
                PartRepo::insert_part_event(
                    &mut *conn,
                    NewPartEvent {
                        id: snowflake.next_id(),
                        part_id,
                        event_type: "SOFT_DELETED",
                        from_status: None,
                        to_status: None,
                        batch_id: None,
                        quantity: None,
                        drawing_code: None,
                        badge_code: None,
                        note: Some("manager soft-delete"),
                        created_by: Some(current.id),
                    },
                )
                .await?;
                Ok(())
            }
            _ => {
                // soft_delete SQL 0 行可能由 4 类原因触发，分支映射到不同错误码：
                // 1) part_id 不存在                  → 20101 BIZ_PART_NOT_FOUND (404)
                // 2) 已软删                          → 20101 BIZ_PART_NOT_FOUND (404, "已软删")
                // 3) version 不匹配                  → 40901 VERSION_CONFLICT (409)
                // 4) 已挂送货单 (delivery_note_id)   → 21420 BIZ_DELIVERY_NOTE_LOCKED_PART (409)
                // 5) 终态 (DELIVERED/COMPLETED)       → 20120 BIZ_PART_NOT_DELETABLE (409)
                let p = PartRepo::get_by_id(&mut *conn, part_id, true).await?;
                match p {
                    None => Err(AppError::biz(
                        code::BIZ_PART_NOT_FOUND,
                        format!("part {part_id} 不存在"),
                    )),
                    Some(p) if p.deleted_at.is_some() => Err(AppError::biz(
                        code::BIZ_PART_NOT_FOUND,
                        format!("part {part_id} 已软删"),
                    )),
                    Some(p) if p.version != expected_version => Err(AppError::biz(
                        code::VERSION_CONFLICT,
                        format!("part {part_id} 版本冲突（期望 {expected_version}，实际 {}）", p.version),
                    )),
                    Some(p) if p.delivery_note_id.is_some() => Err(AppError::biz(
                        code::BIZ_DELIVERY_NOTE_LOCKED_PART,
                        format!("part {part_id} 已挂送货单，禁 soft-delete"),
                    )),
                    Some(p) if matches!(p.status.as_str(), "DELIVERED" | "COMPLETED") => {
                        Err(AppError::biz(
                            code::BIZ_PART_NOT_DELETABLE,
                            format!("part {part_id} 状态 {} 终态禁删", p.status),
                        ))
                    }
                    Some(p) => {
                        // 兜底：理论上 soft_delete SQL 已包含 `deleted_at IS NULL`
                        // 守卫，此分支不可达。映射成 50000 让上游看到错误模式。
                        Err(AppError::internal(format!(
                            "soft_delete_part 兜底：part {part_id} status={} 触发未识别条件",
                            p.status
                        )))
                    }
                }
            }
        }
    }

    /// 上传 part drawing PDF（multipart 处理上传到 COS + INSERT t_part_file）。
    #[allow(clippy::too_many_arguments)]
    pub async fn upload_drawing(
        conn: &mut PgConnection,
        snowflake: &SnowflakeIdGenerator,
        state: &Arc<AppState>,
        part_id: i64,
        bytes: &[u8],
        original_filename: &str,
        content_type: &str,
        current: &CurrentUser,
    ) -> Result<TPartFile, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk])?;
        if bytes.is_empty() {
            return Err(AppError::biz(
                code::BIZ_PART_FILE_TOO_LARGE,
                "PDF 字节为空",
            ));
        }
        if bytes.len() > 50 * 1024 * 1024 {
            return Err(AppError::biz(
                code::BIZ_PART_FILE_TOO_LARGE,
                "PDF > 50MB",
            ));
        }
        if content_type != "application/pdf" {
            return Err(AppError::biz(
                code::BIZ_PART_FILE_BAD_TYPE,
                format!("不支持的 content_type: {content_type}"),
            ));
        }
        // 上传前 part 必须存在
        let p = PartRepo::get_part_detail(&mut *conn, part_id)
            .await?
            .ok_or_else(|| {
                AppError::biz(
                    code::BIZ_PART_FILE_OWNER_NOT_FOUND,
                    format!("part {part_id} 不存在"),
                )
            })?;

        let sha = hash_bytes(bytes);
        let new_file_id = snowflake.next_id();
        let real_key = format!("parts/{}/drawings/{}.pdf", p.id, new_file_id);
        state
            .cos
            .put_object(&real_key, bytes.to_vec(), content_type)
            .await
            .map_err(|e| {
                AppError::biz(
                    code::BIZ_PART_FILE_UPLOAD_FAILED,
                    format!("COS 上传失败: {e}"),
                )
            })?;
        PartFileRepo::create_part_file(
            &mut *conn,
            NewPartFile {
                id: new_file_id,
                part_id,
                kind: "DRAWING",
                file_type: "PDF",
                object_key: &real_key,
                original_filename,
                file_size: bytes.len() as i64,
                content_type,
                upload_status: "READY",
                content_sha256: Some(&sha),
                created_by: current.id,
            },
        )
        .await
        .map_err(|e| {
            if let sqlx::Error::Database(db) = &e
                && db.code().as_deref() == Some("23505")
            {
                return AppError::biz(code::BIZ_PART_FILE_DUPLICATE, "相同 PDF 已存在");
            }
            AppError::from(e)
        })?;
        let pf = PartFileRepo::get_by_part_kind(&mut *conn, part_id, "DRAWING")
            .await?
            .ok_or_else(|| AppError::internal("刚 INSERT 的 file 查不到"))?;
        Ok(pf)
    }
}

// ===== helpers =====

/// `create_part` 的 sqlx 错误码映射：唯一索引冲突（`23505`） → 业务语义
/// `BIZ_PART_NOT_FOUND`（serial_no 已被使用；可能是软删旧件占号导致
/// `uk_t_part_serial_no` 触发。当前 INSERT 路径 serial_no 写 NULL，partial
/// unique 不生效；此分支为预留，等 serial_no 变成可写时启用）。
///
/// `pub(super)`：暴露给 `lifecycle.rs`（如需要）。
pub(super) fn map_create_error(e: sqlx::Error) -> AppError {
    if let sqlx::Error::Database(db) = &e
        && db.code().as_deref() == Some("23505")
    {
        return AppError::biz(
            code::BIZ_PART_NOT_FOUND,
            "serial_no 已被使用（可能软删旧件占号）",
        );
    }
    AppError::from(e)
}

/// 展开 `customer_id` 为 `[id]`（含自身 + 子节点）。
///
/// 语义：
/// - L1 客户（无 parent_id）→ 自身 + 全部 L2 子节点 ids
/// - L2 客户（有 parent_id）→ 自身 + 同 L1 下所有兄弟 L2 ids
async fn expand_customer_id(conn: &mut PgConnection, cid: i64) -> Result<Vec<i64>, AppError> {
    let row: Option<(i64, Option<i64>)> = sqlx::query_as(
        "SELECT id, parent_id FROM t_customer WHERE id = $1 AND deleted_at IS NULL",
    )
    .bind(cid)
    .fetch_optional(&mut *conn)
    .await?;
    let (_id, parent_id) =
        row.ok_or_else(|| {
            AppError::biz(
                code::BIZ_CUSTOMER_NOT_FOUND,
                format!("customer {cid} 不存在"),
            )
        })?;
    if let Some(p) = parent_id {
        let mut rows: Vec<i64> = sqlx::query_scalar(
            "SELECT id FROM t_customer WHERE parent_id = $1 AND deleted_at IS NULL",
        )
        .bind(p)
        .fetch_all(&mut *conn)
        .await?;
        if !rows.contains(&cid) {
            rows.push(cid);
        }
        Ok(rows)
    } else {
        sqlx::query_scalar(
            "SELECT id FROM t_customer WHERE (parent_id = $1 OR id = $1) AND deleted_at IS NULL",
        )
        .bind(cid)
        .fetch_all(&mut *conn)
        .await
        .map_err(Into::into)
    }
}

/// 取客户名 + L1 名（用于 `PartDetailOut` / `PartListItem` 冗余字段）。
///
/// 返回 `(Some(name), Some(l1_name))`：当自身为 L1 时 l1_name 与 name 同；
/// 当 customer 不存在 → `(None, None)`（service 层可容忍）。
async fn lookup_customer_names(
    conn: &mut PgConnection,
    customer_id: i64,
) -> Result<(Option<String>, Option<String>), AppError> {
    let row: Option<(String, Option<i64>)> = sqlx::query_as(
        "SELECT name, parent_id FROM t_customer WHERE id = $1 AND deleted_at IS NULL",
    )
    .bind(customer_id)
    .fetch_optional(&mut *conn)
    .await?;
    let (name, parent_id) = match row {
        Some(r) => r,
        None => return Ok((None, None)),
    };
    let l1_name = match parent_id {
        Some(pid) => {
            sqlx::query_scalar::<_, String>(
                "SELECT name FROM t_customer WHERE id = $1 AND deleted_at IS NULL",
            )
            .bind(pid)
            .fetch_optional(&mut *conn)
            .await?
        }
        None => Some(name.clone()),
    };
    Ok((Some(name), l1_name))
}
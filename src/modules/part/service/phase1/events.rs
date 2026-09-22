//! part 域 Phase 1 事件历史 + 位置树 + 批量创建增强端点。
//!
//! 2026-09-22 D-6 重构：从原 `phase1.rs` 3084 行按业务动作拆出。
//! 含以下端点：
//! - `list_events`（GET /parts/{id}/events）
//! - `location_tree`（GET /parts/location-tree）
//! - `batch_with_pdfs`（POST /parts/batch-with-pdfs）
//! - `match_by_excel_items`（POST /parts/match-by-excel-items）
//! - `batch_update_order_info`（POST /parts/batch-update-order-info）

use crate::auth::rbac::{CurrentUser, Role};
use crate::infra::snowflake::SnowflakeIdGenerator;
use crate::modules::com::customer::repo::CustomerRepo;
use crate::modules::part::repo::NewPartCreate;
use crate::modules::part::repo::PartRepoTrait;
use crate::modules::part::repo::PartUpdate;
use crate::modules::part_batch::repo::PartBatchRepo;
use crate::modules::prod::worker::repo::WorkerRepo;
use crate::modules::shelf::repo::ShelfRepo;
use crate::shared::error::{AppError, code};

use super::super::super::dto_crud::{
    BatchUpdateOrderInfoOut, BatchUpdateOrderInfoRequest, BatchWithPdfsRequest,
    LocationTreeNodeOut, LocationTreeOut, MatchByExcelItemResult, MatchByExcelItemsRequest,
    PartEventOut,
};
use super::EventListRow;
use super::super::PartService;
use super::{HolderCountRow, OutsourceLite};

impl PartService {
    /// `GET /parts/{id}/events`：事件历史。
    pub async fn list_events<R: PartRepoTrait>(
        mut repo: R,
        part_id: i64,
        current: &CurrentUser,
    ) -> Result<Vec<PartEventOut>, AppError> {
        current.require_any_role(&[
            Role::Manager,
            Role::Clerk,
            Role::Inspector,
            Role::CncProgrammer,
        ])?;
        let _ = repo
            .get_part_inspected(part_id)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_PART_NOT_FOUND, "part 不存在"))?;
        let rows: Vec<EventListRow> = sqlx::query_as::<_, EventListRow>(
            "SELECT id, event_type, from_status, to_status, batch_id, quantity,              drawing_code, badge_code, note, created_at, created_by              FROM t_part_event WHERE part_id = $1 ORDER BY id DESC",
        )
        .bind(part_id)
        .fetch_all(repo.conn_mut())
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| PartEventOut {
                id: r.id,
                event_type: r.event_type,
                from_status: r.from_status,
                to_status: r.to_status,
                batch_id: r.batch_id,
                quantity: r.quantity,
                drawing_code: r.drawing_code,
                badge_code: r.badge_code,
                note: r.note,
                created_at: r.created_at,
                created_by: r.created_by,
            })
            .collect())
    }

    /// `GET /parts/location-tree`：按 shelf/status 聚合位置树。
    pub async fn location_tree<R: PartRepoTrait>(
        mut repo: R,
        current: &CurrentUser,
    ) -> Result<LocationTreeOut, AppError> {
        current.require_any_role(&[
            Role::Manager,
            Role::Clerk,
            Role::CncProgrammer,
            Role::Inspector,
        ])?;
        // 收集每个 shelf 的批次计数（按 location IN ('PRODUCTION_SHELF', 'INSPECTION_SHELF')）
        let shelf_counts: Vec<HolderCountRow> = sqlx::query_as::<_, HolderCountRow>(
            "SELECT b.current_holder_id AS holder_id, COUNT(*) AS n              FROM t_part_batch b JOIN t_part p ON p.id = b.part_id              WHERE b.deleted_at IS NULL AND p.deleted_at IS NULL              AND b.location IN ('PRODUCTION_SHELF', 'INSPECTION_SHELF')              AND b.status NOT IN ('CANCELLED', 'COMPLETED')              GROUP BY b.current_holder_id",
        )
        .fetch_all(repo.conn_mut())
        .await?;
        let mut shelf_count_map: std::collections::HashMap<i64, i64> =
            std::collections::HashMap::new();
        for r in shelf_counts {
            shelf_count_map.insert(r.holder_id, r.n);
        }
        // workers
        let worker_counts: Vec<HolderCountRow> = sqlx::query_as::<_, HolderCountRow>(
            "SELECT b.current_holder_id AS holder_id, COUNT(*) AS n              FROM t_part_batch b JOIN t_part p ON p.id = b.part_id              WHERE b.deleted_at IS NULL AND p.deleted_at IS NULL              AND b.location = 'WORKER'              AND b.status NOT IN ('CANCELLED', 'COMPLETED')              GROUP BY b.current_holder_id",
        )
        .fetch_all(repo.conn_mut())
        .await?;
        let mut worker_count_map: std::collections::HashMap<i64, i64> =
            std::collections::HashMap::new();
        for r in worker_counts {
            worker_count_map.insert(r.holder_id, r.n);
        }
        // outsource
        let outsource_counts: Vec<HolderCountRow> = sqlx::query_as::<_, HolderCountRow>(
            "SELECT b.current_holder_id AS holder_id, COUNT(*) AS n              FROM t_part_batch b JOIN t_part p ON p.id = b.part_id              WHERE b.deleted_at IS NULL AND p.deleted_at IS NULL              AND b.location = 'OUTSOURCE_COMPANY'              AND b.status NOT IN ('CANCELLED', 'COMPLETED')              GROUP BY b.current_holder_id",
        )
        .fetch_all(repo.conn_mut())
        .await?;
        let mut outsource_count_map: std::collections::HashMap<i64, i64> =
            std::collections::HashMap::new();
        for r in outsource_counts {
            outsource_count_map.insert(r.holder_id, r.n);
        }
        // OFFICE 计数：所有 PENDING / PROGRAMMING 状态的工单
        let office_count: i64 = sqlx::query_scalar(
            r#"
            SELECT COUNT(*) AS "n!"
            FROM t_part p
            WHERE p.deleted_at IS NULL
              AND p.status IN ('PENDING', 'PROGRAMMING')
            "#,
        )
        .fetch_one(repo.conn_mut())
        .await?;
        // 装载 active shelf / worker / outsource
        let shelves = ShelfRepo::list_with_filters(repo.conn_mut(), None, None, Some(true), 500, 0)
            .await
            .unwrap_or_default();
        let workers = WorkerRepo::list_with_filters(repo.conn_mut(), None, Some(true), 500, 0)
            .await
            .unwrap_or_default();
        // outsource 域为 Phase 2 stub，直接 SQL 取 active 列表
        let outsource_rows: Vec<(i64, String, bool)> = sqlx::query_as(
            "SELECT id, name, is_active FROM t_outsource_company WHERE deleted_at IS NULL",
        )
        .fetch_all(repo.conn_mut())
        .await
        .unwrap_or_default();
        let outsources: Vec<OutsourceLite> = outsource_rows
            .into_iter()
            .map(|(id, name, is_active)| OutsourceLite {
                id,
                name,
                is_active,
            })
            .collect();
        let mut items: Vec<LocationTreeNodeOut> = Vec::new();
        // OFFICE 父节点
        items.push(LocationTreeNodeOut {
            id: "OFFICE".into(),
            label: "办公室".into(),
            kind: "OFFICE".into(),
            parent_id: None,
            count: office_count,
        });
        // PRODUCTION_SHELF 父节点
        let production_shelves: Vec<_> = shelves
            .iter()
            .filter(|s| s.zone == "PRODUCTION" && s.is_active)
            .collect();
        let production_total: i64 = production_shelves
            .iter()
            .filter_map(|s| shelf_count_map.get(&s.id).copied())
            .sum();
        items.push(LocationTreeNodeOut {
            id: "PRODUCTION_SHELF".into(),
            label: "生产货架".into(),
            kind: "PRODUCTION_SHELF".into(),
            parent_id: None,
            count: production_total,
        });
        for s in production_shelves {
            items.push(LocationTreeNodeOut {
                id: s.id.to_string(),
                label: format!("{} {}", s.code, s.name),
                kind: "SHELF".into(),
                parent_id: None,
                count: shelf_count_map.get(&s.id).copied().unwrap_or(0),
            });
        }
        // WORKER 父节点
        let workers_active: Vec<_> = workers.iter().filter(|w| w.is_active).collect();
        let worker_total: i64 = workers_active
            .iter()
            .filter_map(|w| worker_count_map.get(&w.id).copied())
            .sum();
        items.push(LocationTreeNodeOut {
            id: "WORKER".into(),
            label: "工人".into(),
            kind: "WORKER".into(),
            parent_id: None,
            count: worker_total,
        });
        for w in workers_active {
            items.push(LocationTreeNodeOut {
                id: w.id.to_string(),
                label: w.name.clone(),
                kind: "WORKER".into(),
                parent_id: None,
                count: worker_count_map.get(&w.id).copied().unwrap_or(0),
            });
        }
        // INSPECTION_SHELF 父节点
        let inspection_shelves: Vec<_> = shelves
            .iter()
            .filter(|s| s.zone == "INSPECTION" && s.is_active)
            .collect();
        let inspection_total: i64 = inspection_shelves
            .iter()
            .filter_map(|s| shelf_count_map.get(&s.id).copied())
            .sum();
        items.push(LocationTreeNodeOut {
            id: "INSPECTION_SHELF".into(),
            label: "品检货架".into(),
            kind: "INSPECTION_SHELF".into(),
            parent_id: None,
            count: inspection_total,
        });
        for s in inspection_shelves {
            items.push(LocationTreeNodeOut {
                id: s.id.to_string(),
                label: format!("{} {}", s.code, s.name),
                kind: "SHELF".into(),
                parent_id: None,
                count: shelf_count_map.get(&s.id).copied().unwrap_or(0),
            });
        }
        // OUTSOURCE_COMPANY 父节点
        let outsources_active: Vec<_> = outsources.iter().filter(|o| o.is_active).collect();
        let outsource_total: i64 = outsources_active
            .iter()
            .filter_map(|o| outsource_count_map.get(&o.id).copied())
            .sum();
        items.push(LocationTreeNodeOut {
            id: "OUTSOURCE_COMPANY".into(),
            label: "外协公司".into(),
            kind: "OUTSOURCE_COMPANY".into(),
            parent_id: None,
            count: outsource_total,
        });
        for o in outsources_active {
            items.push(LocationTreeNodeOut {
                id: o.id.to_string(),
                label: o.name.clone(),
                kind: "OUTSOURCE_COMPANY".into(),
                parent_id: None,
                count: outsource_count_map.get(&o.id).copied().unwrap_or(0),
            });
        }
        Ok(LocationTreeOut { items })
    }

    /// `POST /parts/batch-with-pdfs`：multipart JSON + PDFs。
    pub async fn batch_with_pdfs<R: PartRepoTrait>(
        mut repo: R,
        snowflake: &SnowflakeIdGenerator,
        req: &BatchWithPdfsRequest,
        pdf_files: &[Vec<u8>],
        current: &CurrentUser,
    ) -> Result<crate::modules::part::dto_crud::PartDetailOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk])?;
        if req.customer_id == 0 {
            return Err(AppError::biz(
                code::BIZ_CUSTOMER_NOT_FOUND,
                "customer_id 必填",
            ));
        }
        let _customer = CustomerRepo::get_by_id(repo.conn_mut(), req.customer_id, false)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_CUSTOMER_NOT_FOUND, "customer 不存在"))?;
        // 解析 PDF 总页数
        let mut page_count: i32 = 0;
        if !pdf_files.is_empty() {
            for pdf in pdf_files {
                let doc = lopdf::Document::load_mem(pdf).map_err(|e| {
                    AppError::biz(code::BIZ_ASSEMBLY_PDF_INVALID, format!("PDF 解析失败: {e}"))
                })?;
                page_count += doc.get_pages().len() as i32;
        }
        }
        // 子件数量上限 99（page2..N 共 99 个）
        let child_count = std::cmp::max(0, page_count - 1);
        if child_count > 99 {
            return Err(AppError::biz(
                code::BIZ_ASSEMBLY_TOO_MANY_CHILDREN,
                format!("batch-with-pdfs 子件最多 99 个，当前 PDF 页数={page_count}"),
            ));
        }

        let today = chrono::Local::now().date_naive();
        let new_id = snowflake.next_id();
        let name = format!("装配件-{}", today.format("%Y%m%d"));
        let drawing_no = format!("ASM-{}", today.format("%Y%m%d"));

        // 若有 PDF → 派 master serial（从 L1 客户 serial_prefix 拿）
        let master_serial: Option<String> = if page_count > 0 {
            let l1_id: i64 = sqlx::query_scalar(
                "SELECT COALESCE(parent_id, id) FROM t_customer WHERE id = $1 AND deleted_at IS NULL",
            )
            .bind(req.customer_id)
            .fetch_one(repo.conn_mut())
            .await?;
            let prefix_str: Option<String> = sqlx::query_scalar(
                "SELECT serial_prefix FROM t_customer WHERE id = $1 AND deleted_at IS NULL",
            )
            .bind(l1_id)
            .fetch_one(repo.conn_mut())
            .await?;
            let p = prefix_str.ok_or_else(|| {
                AppError::biz(
                    code::BIZ_CUSTOMER_NO_SERIAL_PREFIX,
                    "L1 客户无 serial_prefix",
                )
            })?;
            let ch = p
                .chars()
                .next()
                .ok_or_else(|| AppError::biz(code::BIZ_INVALID_VALUE, "serial_prefix 为空"))?;
            Some(crate::shared::serial::acquire(repo.conn_mut(), ch).await?)
        } else {
            None
        };

        let new = NewPartCreate {
            id: new_id,
            name: &name,
            drawing_no: &drawing_no,
            applicant_name: req.applicant_name.as_deref().unwrap_or(""),
            quantity: 1,
            request_date: req.request_date.unwrap_or(today),
            planned_delivery_date: req.planned_delivery_date.unwrap_or(today),
            is_urgent: req.is_urgent.unwrap_or(false),
            customer_id: req.customer_id,
            assembly_id: None,
            order_no: None,
            system_delivery_date: None,
            note: req.note.as_deref(),
            created_by: current.id,
        };
        repo.create_part(new).await?;
        // master 设置 serial_no（仅在有 PDF 时）
        if let Some(sn) = &master_serial {
            sqlx::query("UPDATE t_part SET serial_no = $1 WHERE id = $2 AND deleted_at IS NULL")
                .bind(sn)
                .bind(new_id)
                .execute(repo.conn_mut())
                .await?;
        }
        // 初始批次
        let initial_batch_id = snowflake.next_id();
        PartBatchRepo::create_initial_batch(
            repo.conn_mut(),
            crate::modules::part_batch::repo::NewInitialBatch {
                id: initial_batch_id,
                part_id: new_id,
                quantity: 1,
                location: None,
                created_by: Some(current.id),
            },
        )
        .await?;

        // 自动派发子件（page2..N）
        if child_count > 0 {
            let master_serial = master_serial
                .as_deref()
                .expect("master_serial present when child_count > 0");
            for i in 1..=child_count {
                let child_id = snowflake.next_id();
                let child_serial = format!("{}-{:02}", master_serial, i);
                let child = NewPartCreate {
                    id: child_id,
                    name: &format!("{name}-{:02}", i),
                    drawing_no: &format!("{drawing_no}-{:02}", i),
                    applicant_name: req.applicant_name.as_deref().unwrap_or(""),
                    quantity: 1,
                    request_date: req.request_date.unwrap_or(today),
                    planned_delivery_date: req.planned_delivery_date.unwrap_or(today),
                    is_urgent: req.is_urgent.unwrap_or(false),
                    customer_id: req.customer_id,
                    assembly_id: None,
                    order_no: None,
                    system_delivery_date: None,
                    note: req.note.as_deref(),
                    created_by: current.id,
                };
                repo.create_part(child).await?;
                sqlx::query(
                    "UPDATE t_part SET serial_no = $1 WHERE id = $2 AND deleted_at IS NULL",
                )
                .bind(&child_serial)
                .bind(child_id)
                .execute(repo.conn_mut())
                .await?;
                // 初始批次
                PartBatchRepo::create_initial_batch(
                    repo.conn_mut(),
                    crate::modules::part_batch::repo::NewInitialBatch {
                        id: snowflake.next_id(),
                        part_id: child_id,
                        quantity: 1,
                        location: None,
                        created_by: Some(current.id),
                    },
                )
                .await?;
            }
        }

        // 重读 master
        let part: crate::modules::part::model::TPart =
            repo.get_by_id(new_id, false)
                .await?
                .ok_or_else(|| AppError::biz(code::BIZ_PART_NOT_FOUND, "create 后查不到"))?;
        Ok(
            crate::modules::part::dto_crud::PartDetailOut::from_with_customer_extra(
                part, None, None, None,
            ),
        )
    }

    /// `POST /parts/match-by-excel-items`：Excel 行（drawing_no 或 serial_no）→ 现有 part id。
    pub async fn match_by_excel_items<R: PartRepoTrait>(
        mut repo: R,
        req: &MatchByExcelItemsRequest,
        current: &CurrentUser,
    ) -> Result<Vec<MatchByExcelItemResult>, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk])?;
        let mut out: Vec<MatchByExcelItemResult> = Vec::new();
        for item in &req.items {
            // 先尝试 serial_no
            if let Some(sn) = item.serial_no.as_deref().filter(|s| !s.is_empty()) {
                let rows: Vec<(i64,)> = sqlx::query_as::<_, (i64,)>(
                    "SELECT id FROM t_part WHERE serial_no = $1 AND deleted_at IS NULL",
                )
                .bind(sn)
                .fetch_all(repo.conn_mut())
                .await?;
                if rows.len() == 1 {
                    out.push(MatchByExcelItemResult {
                        drawing_no: item.drawing_no.clone(),
                        serial_no: Some(sn.into()),
                        part_id: Some(rows[0].0),
                        status: "MATCHED".into(),
                        message: None,
                    });
                    continue;
                }
                if rows.is_empty() {
                    out.push(MatchByExcelItemResult {
                        drawing_no: item.drawing_no.clone(),
                        serial_no: Some(sn.into()),
                        part_id: None,
                        status: "NOT_FOUND".into(),
                        message: Some("serial_no 找不到".into()),
                    });
                    continue;
                }
                out.push(MatchByExcelItemResult {
                    drawing_no: item.drawing_no.clone(),
                    serial_no: Some(sn.into()),
                    part_id: None,
                    status: "AMBIGUOUS".into(),
                    message: Some(format!("{} 个匹配", rows.len())),
                });
                continue;
            }
            // 再尝试 drawing_no（取最近一条 active）
            if let Some(dn) = item.drawing_no.as_deref().filter(|s| !s.is_empty()) {
                let rows: Vec<(i64,)> = sqlx::query_as::<_, (i64,)>(
                    "SELECT id FROM t_part WHERE drawing_no = $1 AND deleted_at IS NULL \
                     ORDER BY id DESC LIMIT 5",
                )
                .bind(dn)
                .fetch_all(repo.conn_mut())
                .await?;
                if rows.len() == 1 {
                    out.push(MatchByExcelItemResult {
                        drawing_no: Some(dn.into()),
                        serial_no: item.serial_no.clone(),
                        part_id: Some(rows[0].0),
                        status: "MATCHED".into(),
                        message: None,
                    });
                    continue;
                }
                if rows.is_empty() {
                    out.push(MatchByExcelItemResult {
                        drawing_no: Some(dn.into()),
                        serial_no: item.serial_no.clone(),
                        part_id: None,
                        status: "NOT_FOUND".into(),
                        message: Some("drawing_no 找不到".into()),
                    });
                    continue;
                }
                out.push(MatchByExcelItemResult {
                    drawing_no: Some(dn.into()),
                    serial_no: item.serial_no.clone(),
                    part_id: None,
                    status: "AMBIGUOUS".into(),
                    message: Some(format!("{} 个匹配", rows.len())),
                });
                continue;
            }
            out.push(MatchByExcelItemResult {
                drawing_no: item.drawing_no.clone(),
                serial_no: item.serial_no.clone(),
                part_id: None,
                status: "NOT_FOUND".into(),
                message: Some("serial_no 和 drawing_no 均缺失".into()),
            });
        }
        Ok(out)
    }

    /// `POST /parts/batch-update-order-info`：批量回填 order_no / system_delivery_date / note。
    pub async fn batch_update_order_info<R: PartRepoTrait>(
        mut repo: R,
        req: &BatchUpdateOrderInfoRequest,
        current: &CurrentUser,
    ) -> Result<BatchUpdateOrderInfoOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk])?;
        if req.items.is_empty() {
            return Err(AppError::validation("items 不能为空"));
        }
        let mut updated = 0_i64;
        let mut failed: Vec<super::super::super::dto_crud::BatchUpdateOrderInfoFailure> =
            Vec::new();
        for item in &req.items {
            let upd = PartUpdate {
                name: None,
                drawing_no: None,
                applicant_name: None,
                quantity: None,
                order_no: item.order_no.as_deref(),
                system_delivery_date: item.system_delivery_date,
                planned_delivery_date: None,
                note: item.note.as_deref(),
                is_urgent: None,
                updated_by: current.id,
            };
            let n = repo.update_part(item.part_id, item.version, upd).await;
            match n {
                Ok(1) => updated += 1,
                Ok(_) => failed.push(
                    super::super::super::dto_crud::BatchUpdateOrderInfoFailure {
                        part_id: item.part_id,
                        code: code::VERSION_CONFLICT,
                        message: "版本冲突或 part 已软删".into(),
                    },
                ),
                Err(e) => failed.push(
                    super::super::super::dto_crud::BatchUpdateOrderInfoFailure {
                        part_id: item.part_id,
                        code: code::DATABASE,
                        message: format!("{e}"),
                    },
                ),
            }
        }
        Ok(BatchUpdateOrderInfoOut { updated, failed })
    }
}
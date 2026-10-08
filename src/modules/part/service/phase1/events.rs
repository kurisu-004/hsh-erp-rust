//! part 域 Phase 1 事件历史 + 位置树 + 批量创建增强端点。
//!
//! 2026-09-22 D-6 重构：从原 `phase1.rs` 3084 行按业务动作拆出。
//! 含以下端点：
//! - `list_events`（GET /parts/{id}/events）
//! - `location_tree`（GET /parts/location-tree）
//! - `batch_with_pdfs`（POST /parts/batch-with-pdfs）
//! - `batch_update_order_info`（POST /parts/batch-update-order-info）
//!
//! 2026-10-06：`match_by_excel_items`（POST /parts/match-by-excel-items）连同它的
//! 分档决策纯函数迁到同目录的 `excel_match.rs`（本文件原 548 行里它是唯一一段
//! 「一次请求 → 一份候选表」的批处理逻辑，与事件/位置树无共同点）。

use crate::auth::rbac::{CurrentUser, Role};
use crate::infra::snowflake::SnowflakeIdGenerator;
use crate::modules::com::customer::repo::CustomerRepo;
use crate::modules::iam::shelf::repo::ShelfRepo;
use crate::modules::part::dto_crud::{BatchUpdateOrderInfoRequest, BatchWithPdfsRequest};
use crate::modules::part::repo::NewPartCreate;
use crate::modules::part::repo::PartRepoTrait;
use crate::modules::part::vo::{
    BatchUpdateOrderInfoFailure, BatchUpdateOrderInfoOut, LocationTreeNodeOut, LocationTreeOut,
    PartEventOut,
};
use crate::modules::prod::batch::repo::PartBatchRepo;
use crate::modules::prod::worker::repo::WorkerRepo;
use crate::shared::error::{AppError, code};

use super::super::PartService;
use super::EventListRow;
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
    ///
    /// 2026-10-05：序列号改由 `create_part` INSERT 期写入（此前是 INSERT 后一句
    /// `UPDATE t_part SET serial_no = $1`），派发器与另两个建单端点共用
    /// `PartRepoTrait::serial_prefix_for_customer` + `shared::serial::acquire`。
    /// 行为不变：有 PDF 才派 master 号、子件号仍是 `{master}-{NN}`、PDF 页数为 0
    /// 时 master 与（无）子件都不派号。`BatchWithPdfsRequest` 无前端调用方。
    ///
    /// 2026-10-05 错误码变化：`serial_prefix_for_customer` 改用 `fetch_optional`，
    /// 此前同一段是两条 `fetch_one`（先折 L1、再取 prefix）。所以「L2 自身未软删、
    /// L1 父行已软删」从空结果集 `RowNotFound` → `AppError::Database`（**500**）
    /// 变为 `20102 BIZ_CUSTOMER_NOT_FOUND`（**404**）。该查询只在 PDF 页数 > 0
    /// 时执行。
    pub async fn batch_with_pdfs<R: PartRepoTrait>(
        mut repo: R,
        snowflake: &SnowflakeIdGenerator,
        req: &BatchWithPdfsRequest,
        pdf_files: &[Vec<u8>],
        current: &CurrentUser,
    ) -> Result<crate::modules::part::vo::PartDetailOut, AppError> {
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
            let ch = repo.serial_prefix_for_customer(req.customer_id).await?;
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
            serial_no: master_serial.as_deref(),
            unit_price: None,
            total_price: None,
        };
        repo.create_part(new).await?;
        // 初始批次
        let initial_batch_id = snowflake.next_id();
        PartBatchRepo::create_initial_batch(
            repo.conn_mut(),
            crate::modules::prod::batch::repo::NewInitialBatch {
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
                    // 子件号从 master 号派生，不再向 t_serial_counter 派发
                    serial_no: Some(&child_serial),
                    unit_price: None,
                    total_price: None,
                };
                repo.create_part(child).await?;
                // 初始批次
                PartBatchRepo::create_initial_batch(
                    repo.conn_mut(),
                    crate::modules::prod::batch::repo::NewInitialBatch {
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
        let part: crate::modules::part::model::TPart = repo
            .get_by_id(new_id, false)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_PART_NOT_FOUND, "create 后查不到"))?;
        Ok(
            crate::modules::part::vo::PartDetailOut::from_with_customer_extra(
                part, None, None, None,
            ),
        )
    }

    /// `POST /parts/batch-update-order-info`：批量回填 order_no / system_delivery_date / note。
    ///
    /// 2026-10-06 重做：
    /// - 走专用窄写 `PartRepo::update_order_info`（三态列）而不是 `update_part`
    ///   + `PartUpdate`（单层 `Option` 表达不了「显式清空」，改它会波及行内编辑链路）。
    /// - 响应改 `{updated_count, failed, skipped_count}`（`skip = true` 的行不写库）。
    /// - **永远 200 + 信封**，全部失败也不抛业务错误（前端依赖部分成功语义）。
    ///
    /// 2026-10-06 review 第 1 轮新增两条闸门（都在**进循环之前**，超限整单拒、
    /// 不发一条 UPDATE）：
    /// - `items.len() > BATCH_UPDATE_ORDER_INFO_MAX_ITEMS` → 40001。写端点的每行
    ///   都要**一条** UPDATE 往返，N 万行会在**一条池化连接**上串行跑 N 次，
    ///   把那一条连接占满到超时（match 端点是批量读、上限语义相同但不占连接）。
    /// - `system_delivery_date` 的「给值」这一态允许是**非日期文本**（前端 Excel
    ///   解析器原样透传 dayjs 认不出的文本，见 `BatchUpdateOrderInfoItem` 的字段
    ///   doc 与前端根因位置）。逐行解析：失败 ⇒ 该行进 `failed[]`（40001）、
    ///   **不写库**，其余行照常写 —— 绝不能让它在 extractor 层 400 打掉整批。
    ///
    /// 2026-10-06 review 第 3 轮 R3-1 补齐 **`order_no` / `note` 的长度闸门**：
    /// 上一轮只给日期做了「DB 装不下 ⇒ 行级 40001」，这两个兄弟列仍是「同样的事
    /// ⇒ 打 DB、报 50001」。两个语义、两条排障方向，详见循环内该闸门处的注释。
    ///
    /// ## 三层闸门的先后（2026-10-06 R3-1 + R3-8 登记）
    ///
    /// 1. **入参级**（循环之前）：`items` 空 / 超上限 ⇒ 整单拒（40001），一条
    ///    UPDATE 都不发。
    /// 2. **行级 skip**（循环内最先）：`skip = true` ⇒ 该行**连校验都不做**，只计
    ///    `skipped_count`。语义是「这行别碰」，所以后面两道闸门对它一律不生效 ——
    ///    否则会出现「被跳过的行却报了 failed」这种自相矛盾的响应。
    /// 3. **行级入参合法性**：`order_no` / `note` 超长、系统交期非日期 ⇒ 该行进
    ///    `failed[]`（40001）、**不写库**，其余行照常写。
    pub async fn batch_update_order_info<R: PartRepoTrait>(
        mut repo: R,
        req: &BatchUpdateOrderInfoRequest,
        current: &CurrentUser,
    ) -> Result<BatchUpdateOrderInfoOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk])?;
        if req.items.is_empty() {
            return Err(AppError::validation("items 不能为空"));
        }
        if req.items.len() > BATCH_UPDATE_ORDER_INFO_MAX_ITEMS {
            return Err(AppError::validation(format!(
                "items 最多 {BATCH_UPDATE_ORDER_INFO_MAX_ITEMS} 行，当前 {} 行",
                req.items.len()
            )));
        }
        let mut updated_count = 0_i64;
        let mut skipped_count = 0_i64;
        let mut failed: Vec<BatchUpdateOrderInfoFailure> = Vec::new();
        for item in &req.items {
            // 2026-10-06 新增：`skip = true` 的行是「人工判定不该回填」，不写库、
            // 不计成功、不计失败，只计入 skipped_count。
            if item.skip == Some(true) {
                skipped_count += 1;
                continue;
            }
            // 2026-10-06 review 第 3 轮 R3-1：三列的**长度闸门**。
            //
            // 上一轮只给 `system_delivery_date` 做了「用户可输入但 DB 装不下 ⇒
            // 行级 40001」，`order_no` 却仍是「同样的事 ⇒ 打 DB、报 50001」——
            // 两个兄弟列、两种语义、同一个循环，而 50001 会把排障方向误导到数据库。
            //
            // `order_no` 超长在本功能里**现实可达且不需要用户犯蠢**：
            // 前端 `purchaseOrderExcelParser.ts:155` 直接读采购订单「单据编号」首行
            // 原文进 `docNo`（无长度校验、无截断），`PurchaseOrderImportDialog.vue:636`
            // 把它作为**每一个**候选行的默认 `orderNo` ⇒ 一张 PO 的单据编号超 30 字，
            // **整批每一行**都撞 `t_part.order_no varchar(30)`（baseline:1112），
            // 整单 0 行写成功。候选行的 el-input（`:148-153`）也没有 `maxlength`。
            //
            // 闸门放在 skip 判定**之后**：skip 的语义是「这行别碰」，被跳过的行连校验
            // 都不该做（否则会出现「skip 掉的行却报了 failed」这种自相矛盾的结果）。
            //
            // 长度按 **char** 计数（不是字节）—— `varchar(n)` 在 PG 里数的也是字符数，
            // 按字节判会在中文订单号上误杀（30 个汉字 = 90 字节，但列放得下）。
            //
            // **刻意不 trim**（与日期闸门不同）：这里量的是「将要写进 `varchar(n)` 的
            // 那串字符」的原长度，不是解析语义。trim 后计数、却把原串写库会留下一个
            // 洞——`"   " + 30 字`（trim 后 30 字）过闸门、原串 36 字照样被 PG 拒，
            // 于是又退回 50001，正是本闸门要消灭的那条排障歧路。（2026-10-06 review
            // 第 3 轮实现时的自查：先写了 trim 版，被自己的用例打出来，改成原长度。）
            if let Some(v) = item.order_no.as_ref().and_then(|v| v.as_deref())
                && v.chars().count() > ORDER_NO_MAX_CHARS
            {
                failed.push(BatchUpdateOrderInfoFailure {
                    part_id: item.part_id,
                    code: code::VALIDATION_ERROR,
                    message: format!("订单号超长（最多 {ORDER_NO_MAX_CHARS} 字）"),
                });
                continue;
            }
            if let Some(v) = item.note.as_ref().and_then(|v| v.as_deref())
                && v.chars().count() > NOTE_MAX_CHARS
            {
                failed.push(BatchUpdateOrderInfoFailure {
                    part_id: item.part_id,
                    code: code::VALIDATION_ERROR,
                    message: format!("备注超长（最多 {NOTE_MAX_CHARS} 字）"),
                });
                continue;
            }
            // 三态日期的逐行解析（review 第 1 轮 MAJOR-1）：缺省 ⇒ 该列不动；
            // 显式 null ⇒ 清成 NULL；给值 ⇒ 必须能解析成日期，否则该行失败。
            let sys_date = match parse_tristate_date(&item.system_delivery_date) {
                Ok(d) => d,
                Err(raw) => {
                    failed.push(BatchUpdateOrderInfoFailure {
                        part_id: item.part_id,
                        code: code::VALIDATION_ERROR,
                        message: format!("系统交期格式非法：{raw}"),
                    });
                    continue;
                }
            };
            let n = repo
                .update_order_info(
                    item.part_id,
                    item.version,
                    item.order_no.as_ref().map(|v| v.as_deref()),
                    sys_date,
                    item.note.as_ref().map(|v| v.as_deref()),
                    current.id,
                )
                .await;
            match n {
                Ok(1) => updated_count += 1,
                // 0 行 = 版本冲突 / 已软删 / 不存在，三者 SQL 层不可区分，沿用旧口径
                Ok(_) => failed.push(BatchUpdateOrderInfoFailure {
                    part_id: item.part_id,
                    code: code::VERSION_CONFLICT,
                    message: "版本冲突或 part 已软删".into(),
                }),
                Err(e) => {
                    // 2026-10-06：详情只进服务端日志。响应体里的 message 走统一中文
                    // 文案，不再 `format!("{e}")` —— sqlx 原始错误（表名列名、约束名）
                    // 不该泄给客户端，且与 `AppError::into_response` 已把 Database 的
                    // message 换成字面量「数据库错误」的口径不一致。
                    tracing::warn!(
                        part_id = item.part_id,
                        error = %e,
                        "batch-update-order-info 单行更新失败"
                    );
                    failed.push(BatchUpdateOrderInfoFailure {
                        part_id: item.part_id,
                        code: code::DATABASE,
                        message: "数据库错误，写入失败".into(),
                    });
                }
            }
        }
        Ok(BatchUpdateOrderInfoOut {
            updated_count,
            failed,
            skipped_count,
        })
    }
}

/// 2026-10-06 review 第 1 轮新增：单请求 `items` 行数上限（写端点）。
///
/// 与 match 端点的 [`excel_match::MATCH_MAX_ITEMS`](crate::modules::part::service::
/// phase1::excel_match::MATCH_MAX_ITEMS) 同为 2000，但**各自定义**：写端点的理由是
/// 「每行一条 UPDATE 往返会长时间独占一条池化连接」，match 端点的理由是「一次批量
/// 读」，耦合过去会让后续任一侧改上限时另一侧的理由对不上。（两个常量彼此的 doc
/// 互指：见 `MATCH_MAX_ITEMS` 侧的对应说明。）
///
/// ⚠️ **两者数的不是同一个东西**（2026-10-06 review 第 3 轮 R3-6 登记）：
/// `MATCH_MAX_ITEMS` 数的是 **Excel 行**，本常量数的是 **候选行**（行 × 候选）。
/// 一次合法 match 最多产出 `2000 行 × 20 候选 = 40000` 个候选行，于是存在一条
/// **硬崖**：match 端 200 行全命中、且候选多为「空目标」（前端 `isEmptyTarget`
/// 默认勾选）时，提交 4000+ 候选行会被本常量**整单 422、一行不写**。
/// 用户可在确认框（已显示「将更新 N 个零件」）里手动取消勾选降到 2000 以下，
/// 谈不上死路，但这条崖此前没写进任何注释。
pub const BATCH_UPDATE_ORDER_INFO_MAX_ITEMS: usize = 2000;

/// 2026-10-06 review 第 3 轮 R3-1 新增：`t_part.order_no` 的长度上限
/// （`varchar(30)`，baseline:1112），按 **char** 计（与 PG 的 `varchar(n)` 口径一致）。
pub const ORDER_NO_MAX_CHARS: usize = 30;

/// 2026-10-06 review 第 3 轮 R3-1 新增：`t_part.note` 的长度上限
/// （`varchar(500)`，baseline:1114），按 **char** 计。
pub const NOTE_MAX_CHARS: usize = 500;

/// 三态日期入参 → repo 层要写的值（`None` = 该列不动 / `Some(None)` = 写 NULL /
/// `Some(Some(d))` = 写日期）。`Err(raw)` = 「给值」这一态给了非法文本。
///
/// 2026-10-06 review 第 1 轮新增（见 `batch_update_order_info` 的 doc）；
/// 第 3 轮 R3-2 把解析调用换成 [`NaiveDate::FromStr`]（见下）。
///
/// - **走 `FromStr` 而非 `parse_from_str("%Y-%m-%d")`**（2026-10-06 R3-2）：本字段
///   改动前的 DTO 类型是 `NaiveDate`，其 serde 反序列化内部走的就是 `FromStr`
///   （chrono `naive/date/mod.rs:2388-2403`）。换成同一个 `FromStr` 后，「合法日期
///   逐位相同」不再需要靠论证 —— 两者在**实现上就是同一段代码**。
///   `parse_from_str` 带显式 format 时是另一条路径（`Item::Literal` 逐字符比），
///   对空白更严格。
/// - **trim 后**再解析**不是新增放宽**：chrono's `FromStr` 里 `Item::Space("")` 的
///   语义是「跳任意量空白」，所以改动前的 serde 路径**本来就接受** `" 2026-07-08 "`。
///   保留 `trim` 只是让「展示用的原文」与「解析用的串」一致，两者对空白行为等价。
/// - 回错时把**截断后的**原文带回，供 message 展示。截断到 32 字符是因为这是
///   用户自由输入（Excel 单元格内容），不能让它在 200 响应体里无界增长。
fn parse_tristate_date(
    raw: &Option<Option<String>>,
) -> Result<Option<Option<chrono::NaiveDate>>, String> {
    match raw {
        None => Ok(None),
        Some(None) => Ok(Some(None)),
        Some(Some(s)) => {
            let t = s.trim();
            match t.parse::<chrono::NaiveDate>() {
                Ok(d) => Ok(Some(Some(d))),
                Err(_) => {
                    let shown: String = t.chars().take(32).collect();
                    Err(shown)
                }
            }
        }
    }
}

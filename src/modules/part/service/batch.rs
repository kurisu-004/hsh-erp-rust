//! part 域批量创建业务逻辑
//!
//! `batch_create_parts_legacy`：批量建零件（per-item savepoint 模型：单条失败
//! 进 `failed[]`，不中断整体），由 `crud.rs::batch_create_parts` 薄包装转发。
//!
//! ## 序列号派发
//! prefix 只在开头取一次（客户缺 prefix → 20308 整批拒，0 行落库），
//! per-item savepoint 循环内每条调一次 `shared::serial::acquire` 派一个号，
//! 排在 SAVEPOINT **之前**（回滚不退 counter，代价是号留空洞；序列号是单调递增的
//! 工单标识、不承载「连续区间」语义，空洞可接受，换来的是号永不重复）。
//!
//! `acquire` 是 `UPDATE ... RETURNING`，对 `t_serial_counter` 该 prefix 行加行级
//! 排他锁并**持有到整个事务 COMMIT** ⇒ 同 prefix 的并发建单在该行上串行排队
//! （跨 prefix 不互相阻塞）。单批 N 上限 200，这个吞吐对本域可接受。

use crate::auth::rbac::{CurrentUser, Role};
use crate::infra::snowflake::SnowflakeIdGenerator;
use crate::modules::com::customer::repo::CustomerRepo;
use crate::modules::part::dto_crud::PartBatchCreateRequest;
use crate::modules::part::repo::NewPartCreate;
use crate::modules::part::repo::PartRepoTrait;
use crate::modules::part::vo::PartDetailOut;
use crate::modules::prod::batch::repo::{NewInitialBatch, PartBatchRepo};
use crate::shared::error::{AppError, code};

use super::{BATCH_CREATE_PARTS_MAX_ITEMS, PartService};

use super::crud::{map_create_error, require_leaf_customer};

impl PartService {
    /// batch_create_parts 的 legacy 实现：与既有签名一致，不支持文件绑定。
    /// 2026-09-16 M2-B：拆出来供 batch_create_parts 复用（保留原 per-item savepoint 模型）。
    /// 2026-09-16 M2-C：从 crud.rs 迁移到本文件（按 docs/conventions.md §2 单文件职责拆分）。
    ///
    /// 2026-10-05：`POST /parts/batch` 不带任何 file binding 时走本路径（handler 按
    /// 本文件的实现），而前端 PDF 批量上传正是这条路径，所以序列号派发与
    /// `unit_price` / `total_price` 透传必须在这里也做一遍，否则真实建单入口仍然
    /// 拿不到序列号。prefix 只取一次、per-item 派一次号的取舍与
    /// `POST /parts` 单件建单同（序列号空洞可接受、
    /// 同 prefix 建单在 `t_serial_counter` 该行的排他锁上串行到 COMMIT）。
    pub(super) async fn batch_create_parts_legacy<R: PartRepoTrait>(
        mut repo: R,
        snowflake: &SnowflakeIdGenerator,
        req: &PartBatchCreateRequest,
        current: &CurrentUser,
    ) -> Result<crate::modules::part::vo::PartBatchCreateOut, AppError> {
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
        let _customer = CustomerRepo::get_by_id(repo.conn_mut(), req.customer_id, false)
            .await?
            .ok_or_else(|| {
                AppError::biz(
                    code::BIZ_CUSTOMER_NOT_FOUND,
                    format!("customer {} 不存在", req.customer_id),
                )
            })?;
        // 与 `create_part` **同口径**（2026-10-17
        // 对齐）：闸门一律是「存在性 → 叶子 → 派号 → 取名」。叶子闸门排在派号之前，
        // 挂了非叶子客户不会白烧一个序列号（号池 4 位一轮回绕，白烧的号会挤掉真正要用的）。
        require_leaf_customer(repo.conn_mut(), req.customer_id).await?;
        // 2026-10-05：prefix 缺失（20308）在写任何行之前整批拒
        let serial_prefix = repo.serial_prefix_for_customer(req.customer_id).await?;
        // 2026-10-17：本批客户三列只取一次（逐 item 查 = N+1，而本批 customer_id 恒定）。
        let cust_paths =
            crate::shared::customer::paths_of(repo.conn_mut(), &[req.customer_id]).await?;
        let mut created = Vec::new();
        let mut failed = Vec::new();
        for (idx, item) in req.items.iter().enumerate() {
            let new_id = snowflake.next_id();
            let serial_no = crate::shared::serial::acquire(repo.conn_mut(), serial_prefix).await?;
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
                serial_no: Some(&serial_no),
                unit_price: item.unit_price,
                total_price: item.total_price,
            };
            use sqlx::AssertSqlSafe;
            let sp_name = format!("batch_item_{idx}");
            sqlx::raw_sql(AssertSqlSafe(format!("SAVEPOINT {sp_name}")))
                .execute(repo.conn_mut())
                .await?;
            match repo.create_part(new).await {
                Ok(_) => {
                    let initial_batch_id = snowflake.next_id();
                    if let Err(e) = PartBatchRepo::create_initial_batch(
                        repo.conn_mut(),
                        NewInitialBatch {
                            id: initial_batch_id,
                            part_id: new_id,
                            quantity: item.quantity,
                            location: None,
                            created_by: Some(current.id),
                        },
                    )
                    .await
                    {
                        sqlx::raw_sql(AssertSqlSafe(format!("ROLLBACK TO SAVEPOINT {sp_name}")))
                            .execute(repo.conn_mut())
                            .await?;
                        let mapped = map_create_error(e);
                        failed.push(crate::modules::part::vo::PartBatchCreateFailure {
                            part_id: None,
                            code: mapped.code(),
                            message: format!("{mapped}"),
                            item_index: idx,
                        });
                        continue;
                    }
                    sqlx::raw_sql(AssertSqlSafe(format!("RELEASE SAVEPOINT {sp_name}")))
                        .execute(repo.conn_mut())
                        .await?;
                    match repo.get_part_detail(new_id).await {
                        Ok(Some(p)) => {
                            let current_batch_id =
                                repo.find_current_inspection_batch_id(p.id).await?;
                            created.push(PartDetailOut::from_with_customer_extra(
                                p,
                                current_batch_id,
                                cust_paths.get(&req.customer_id),
                            ));
                        }
                        _ => {
                            failed.push(crate::modules::part::vo::PartBatchCreateFailure {
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
                        .execute(repo.conn_mut())
                        .await?;
                    let mapped = map_create_error(e);
                    failed.push(crate::modules::part::vo::PartBatchCreateFailure {
                        part_id: None,
                        code: mapped.code(),
                        message: format!("{mapped}"),
                        item_index: idx,
                    });
                }
            }
        }
        Ok(crate::modules::part::vo::PartBatchCreateOut {
            created,
            failed,
            // legacy 路径不绑定文件，无 tmp 需清理
            cleanup_tmp_keys: Vec::new(),
        })
    }
}

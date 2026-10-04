//! part 域批量创建业务逻辑（含直传 COS 文件绑定）
//!
//! 2026-09-16 M2-B 新增 + M2-C 重构：
//! - `batch_create_parts_with_bindings` —— 第一遍并发 head+copy（max 5 并发）→
//!   第二遍单事务 INSERT parts + per-item savepoint + part_file 行。
//! - `batch_create_parts_legacy` —— 兼容老 batch-create（无文件绑定）；薄包装
//!   转 `crud.rs::batch_create_parts` 的旧实现。
//! - `prepare_binding_head_copy` —— 单 item 文件绑定预处理（head + copy +
//!   cas_key 派生）。
//!
//! ## Err 透出契约（M2-C 修）
//! `batch_create_parts_with_bindings` 的返回类型为
//! `Result<(PartBatchCreateOut, Vec<String>), (AppError, Vec<String>)>`：
//! - **Ok** 分支：第二个元素 = 已成功 head/copy 的 tmp_key 列表，handler commit
//!   后无差别 spawn delete 兜底（无论 per-item DB 结果如何；2026-09-16 M2-B
//!   review 第 2 轮 B1 修）。
//! - **Err** 分支：第二个元素 = 同一时刻已成功 head/copy 的 tmp_key 列表
//!   （M2-C 新增；first-pass 收集）。即使整个调用最终失败，已成功 copy 到 CAS
//!   端的 tmp 对象仍需清理 —— handler 走 Err 分支也 spawn delete。
//!
//! 与 `batch_create_parts` 的区别：调用方需额外注入 `cos` / `cfg`（upload_prefix +
//! tmp_prefix）；handler 层走 state 直接拿，service 层把 IO 控制在 pool（不依赖 tx）。

use std::sync::Arc;

use crate::auth::rbac::{CurrentUser, Role};
use crate::infra::cos::CosClient;
use crate::infra::snowflake::SnowflakeIdGenerator;
use crate::modules::com::customer::repo::CustomerRepo;
use crate::modules::part::dto_crud::{FileBindingIn, PartBatchCreateRequest};
use crate::modules::part::repo::NewPartCreate;
use crate::modules::part::repo::PartRepoTrait;
use crate::modules::part::vo::{PartBatchCreateOut, PartDetailOut};
use crate::modules::part_file::policy;
use crate::modules::part_file::repo::{NewPartFile, PartFileRepo};
use crate::modules::prod::batch::repo::{NewInitialBatch, PartBatchRepo};
use crate::shared::error::{AppError, code};

use super::{BATCH_CREATE_PARTS_MAX_ITEMS, PartService};

use super::crud::{lookup_customer_names, map_create_error};

/// 2026-09-16 M2-B 新增：单 item 文件绑定预处理的输出。
///
/// 由 `prepare_binding_head_copy` 在 batch_create 第一遍（并发 head+copy）后产出，
/// 供第二遍 DB 写入直接使用已准备好的 cas_key（**不再做 head/copy**）。
#[derive(Debug, Clone)]
pub(super) struct PreparedBinding {
    /// 临时对象 key（head/copy 完成后等 commit 成功 spawn 异步 delete 兜底）
    pub(super) tmp_key: String,
    /// 正式 CAS 对象 key（含预生成的 owner_id）
    pub(super) cas_key: String,
    /// "DRAWING" / "3D_MODEL"
    pub(super) kind: String,
    /// "PDF" / "STEP" / "STL" / ...
    pub(super) file_type: String,
    /// 64 hex
    pub(super) sha256: String,
    /// 原始 filename
    pub(super) original_filename: String,
    pub(super) file_size: i64,
    pub(super) content_type: String,
}

impl PartService {
    /// 2026-09-16 M2-C：带文件绑定的 batch_create（场景 A 收口），签名
    /// `Result<Out, (AppError, Vec<String>)>` 让 Err 路径也透出已成功
    /// head/copy 的 tmp_keys。
    ///
    /// 第一遍：并发 head+copy 所有文件绑定（max 5 并发）→ 任一失败整体报错。
    /// 第二遍：单事务 INSERT parts + per-item savepoint + part_file 行（每个 part 至多 2 行）。
    /// commit 后 spawn batch delete_object(tmp_keys) 兜底清理（由 handler 统一发起）。
    ///
    /// 与 `batch_create_parts` 的区别：调用方需额外注入 `cos` / `cfg`（upload_prefix +
    /// tmp_prefix）；handler 层走 state 直接拿，service 层把 IO 控制在 pool（不依赖 tx）。
    ///
    /// ## 序列号派发（2026-10-05）
    /// prefix 只在开头取一次（L1 客户缺 prefix → 20308 整批拒，连文件都不 head/copy），
    /// per-item savepoint 循环内每条调一次 `shared::serial::acquire` 派一个号。
    ///
    /// **取舍**：不去做「批量预取 N 个号」省往返——预取会把 counter 一次性推进 N，
    /// 一旦中途 savepoint 回滚就有 N 个号变空洞，且「一个 item 一次派发」的语义被
    /// 打破。代价是事务内 N 次 `UPDATE t_serial_counter` 往返（N 上限 200，常规
    /// PDF 批量 18 条量级，串行几百毫秒内可完成）。
    ///
    /// **已知代价**：个别 item 失败（唯一索引撞号 / part_file 写失败）时该 item 已
    /// 消耗的号不会回收，序列号出现空洞。序列号是单调递增的工单标识、不承载「连续
    /// 区间」语义，空洞可接受（换来的好处是号永不重复）。
    ///
    /// **并发约束**：`acquire` 是 `UPDATE ... RETURNING`，会对 `t_serial_counter`
    /// 该 prefix 行加行级排他锁并**持有到整个事务 COMMIT**。所以同 prefix 的并发
    /// 建单会在这一行上串行排队（跨 prefix 不互相阻塞）。单批 N 上限 200、单次
    /// 持锁时长与 N 成正比，这个吞吐对本域可接受，不做「取一批号后立刻提交」的改法
    /// （那会让号与 INSERT 脱离同一事务，崩溃后留下永久空洞）。
    #[allow(clippy::too_many_arguments)]
    pub async fn batch_create_parts_with_bindings<R: PartRepoTrait>(
        mut repo: R,
        snowflake: &SnowflakeIdGenerator,
        cos: Arc<dyn CosClient>,
        cfg_upload_prefix: &str,
        cfg_tmp_prefix: &str,
        req: &PartBatchCreateRequest,
        current: &CurrentUser,
    ) -> Result<(PartBatchCreateOut, Vec<String>), (AppError, Vec<String>)> {
        current
            .require_any_role(&[Role::Manager, Role::Clerk])
            .map_err(|e| (e, Vec::new()))?;
        if req.items.is_empty() {
            return Err((AppError::validation("items 不能为空"), Vec::new()));
        }
        if req.items.len() > BATCH_CREATE_PARTS_MAX_ITEMS {
            return Err((
                AppError::validation(format!(
                    "items 数量 {} 超过上限 {}",
                    req.items.len(),
                    BATCH_CREATE_PARTS_MAX_ITEMS
                )),
                Vec::new(),
            ));
        }
        let customer_check = CustomerRepo::get_by_id(repo.conn_mut(), req.customer_id, false).await;
        match customer_check {
            Ok(Some(_)) => {}
            Ok(None) => {
                return Err((
                    AppError::biz(
                        code::BIZ_CUSTOMER_NOT_FOUND,
                        format!("customer {} 不存在", req.customer_id),
                    ),
                    Vec::new(),
                ));
            }
            Err(e) => return Err((AppError::from(e), Vec::new())),
        }

        // 2026-10-05：序列号 prefix 只取一次，且取在第一遍 head/copy **之前** ——
        // L1 客户没配 prefix（20308）要 fail-fast 整批拒，此时一个 COS 对象都
        // 没 head/copy 过，handler 的 tmp 清理集合天然为空。
        let serial_prefix = repo
            .serial_prefix_for_customer(req.customer_id)
            .await
            .map_err(|e| (e, Vec::new()))?;

        // ===== 第一遍：预生成 part_id + 收集所有 bindings + 并发 head/copy（max 5 并发） =====
        let preallocated_part_ids: Vec<i64> =
            (0..req.items.len()).map(|_| snowflake.next_id()).collect();

        // 收集所有 (item_index, binding_kind, FileBindingIn, future_owner_id) jobs
        #[derive(Clone)]
        struct Job {
            item_index: usize,
            kind: &'static str,
            binding: FileBindingIn,
            future_owner_id: i64,
        }
        let mut jobs: Vec<Job> = Vec::new();
        for (idx, item) in req.items.iter().enumerate() {
            if let Some(b) = &item.drawing_file {
                jobs.push(Job {
                    item_index: idx,
                    kind: "DRAWING",
                    binding: b.clone(),
                    future_owner_id: preallocated_part_ids[idx],
                });
            }
            if let Some(b) = &item.model3d_file {
                jobs.push(Job {
                    item_index: idx,
                    kind: "3D_MODEL",
                    binding: b.clone(),
                    future_owner_id: preallocated_part_ids[idx],
                });
            }
        }

        // 受控并发 5
        let max_concurrency = 5usize;
        let mut prepared_per_item: Vec<Vec<PreparedBinding>> =
            (0..req.items.len()).map(|_| Vec::new()).collect();
        // 2026-09-16 M2-B review 第 2 轮 B1 修：所有 head/copy 成功的 tmp_key 统一收集到
        // `cleanup_tmp_keys`，不再按「per-item INSERT 是否成功」分流。
        // 理由：INSERT 失败的 item 也已把 tmp 对象 copy 到 CAS key（COS 端实际有该对象），
        // 但 DB 未提交 → 既然 DB 没有 part_file 行指向 cas_key，CAS 对象就成了孤儿。
        // 反之 tmp 对象没 INSERT 记录引用、必须删。两个对象都要清：
        // - tmp：必须在 commit 成功/失败后都删（前者防止下次重传误用旧文件；后者防止孤儿）
        // - cas（INSERT 成功的）：被 part_file 引用，删了会破坏 CAS 不变量 → 不能动
        // service 层不再分「成功 vs 失败」，统一把 head/copy 成功的 tmp_key 全收上来
        // → handler commit 后无差别 spawn 删全部（不依赖 per-item DB 结果）。
        let mut cleanup_tmp_keys: Vec<String> = Vec::new();

        let chunks: Vec<Vec<Job>> = jobs.chunks(max_concurrency).map(|c| c.to_vec()).collect();
        for chunk in chunks {
            // 同一批并发执行 head+copy
            let results = futures_util::future::join_all(chunk.iter().map(|job| {
                let cos = cos.clone();
                let cfg_upload_prefix = cfg_upload_prefix.to_string();
                let cfg_tmp_prefix = cfg_tmp_prefix.to_string();
                async move {
                    prepare_binding_head_copy(
                        cos,
                        &cfg_upload_prefix,
                        &cfg_tmp_prefix,
                        job.future_owner_id,
                        job.kind,
                        &job.binding,
                    )
                    .await
                }
            }))
            .await;
            for (job, res) in chunk.iter().zip(results) {
                match res {
                    Ok(pb) => {
                        cleanup_tmp_keys.push(pb.tmp_key.clone());
                        prepared_per_item[job.item_index].push(pb);
                    }
                    Err(e) => {
                        // 任何 head/copy 失败 → 整体报错；把已成功 copy 的 tmp_keys
                        // 一起 Err 透出（M2-C 修：让 handler 决定 spawn 时机，避免
                        // 重复 spawn）。原 M2-B review 第 1 轮的 service 自 spawn
                        // 方案已移除 —— 现在 service 只负责"收集 keys + 透出"，
                        // handler 统一 spawn（避免 commit 失败但已 spawn 删除的双重
                        // 副作用，以及 Err 路径漏 spawn 的孤儿风险）。
                        return Err((e, cleanup_tmp_keys));
                    }
                }
            }
        }

        // ===== 第二遍：单事务 INSERT parts + per-item savepoint + part_file 行 =====
        let mut created = Vec::new();
        let mut failed = Vec::new();

        for (idx, item) in req.items.iter().enumerate() {
            let new_id = preallocated_part_ids[idx];
            // 2026-10-05：每条 item 派发一个序列号（`shared::serial::acquire` 走
            // `UPDATE t_serial_counter ... RETURNING`，与 prefix 解析分开两次往返）。
            // 放在 SAVEPOINT **之前**：这样本 item 后续失败时 rollback 不会退掉
            // counter，代价是序列号留空洞（见函数 doc 的「已知代价」）。
            let serial_no =
                match crate::shared::serial::acquire(repo.conn_mut(), serial_prefix).await {
                    Ok(sn) => sn,
                    Err(e) => return Err((e, cleanup_tmp_keys)),
                };
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
            // per-item savepoint
            use sqlx::AssertSqlSafe;
            let sp_name = format!("batch_item_{idx}");
            if let Err(e) = sqlx::raw_sql(AssertSqlSafe(format!("SAVEPOINT {sp_name}")))
                .execute(repo.conn_mut())
                .await
            {
                return Err((AppError::from(e), cleanup_tmp_keys));
            }
            match repo.create_part(new).await {
                Ok(_) => {
                    let initial_batch_id = snowflake.next_id();
                    let initial_batch_result = PartBatchRepo::create_initial_batch(
                        repo.conn_mut(),
                        NewInitialBatch {
                            id: initial_batch_id,
                            part_id: new_id,
                            quantity: item.quantity,
                            location: None,
                            created_by: Some(current.id),
                        },
                    )
                    .await;
                    if let Err(e) = initial_batch_result {
                        if let Err(e) =
                            sqlx::raw_sql(AssertSqlSafe(format!("ROLLBACK TO SAVEPOINT {sp_name}")))
                                .execute(repo.conn_mut())
                                .await
                        {
                            return Err((AppError::from(e), cleanup_tmp_keys));
                        }
                        let mapped = map_create_error(e);
                        failed.push(crate::modules::part::vo::PartBatchCreateFailure {
                            part_id: None,
                            code: mapped.code(),
                            message: format!("{mapped}"),
                            item_index: idx,
                        });
                        continue;
                    }
                    {
                        // 把 prepared bindings 插入 t_part_file（成功 part 才绑；savepoint 已释放）
                        let mut part_files_ok = true;
                        for pb in &prepared_per_item[idx] {
                            if let Err(e) = PartFileRepo::create_part_file(
                                repo.conn_mut(),
                                NewPartFile {
                                    id: snowflake.next_id(),
                                    part_id: new_id,
                                    owner_kind: "PART",
                                    kind: &pb.kind,
                                    file_type: &pb.file_type,
                                    object_key: &pb.cas_key,
                                    original_filename: &pb.original_filename,
                                    file_size: pb.file_size,
                                    content_type: &pb.content_type,
                                    upload_status: "READY",
                                    content_sha256: Some(&pb.sha256),
                                    created_by: current.id,
                                },
                            )
                            .await
                            {
                                if let sqlx::Error::Database(db) = &e
                                    && db.code().as_deref() == Some("23505")
                                {
                                    part_files_ok = false;
                                    failed.push(crate::modules::part::vo::PartBatchCreateFailure {
                                        part_id: Some(new_id),
                                        code: code::BIZ_PART_FILE_DUPLICATE,
                                        message: format!(
                                            "drawing_file / model3d_file sha={} 撞唯一索引",
                                            &pb.sha256[..16]
                                        ),
                                        item_index: idx,
                                    });
                                    break;
                                }
                                part_files_ok = false;
                                let mapped = AppError::from(e);
                                failed.push(crate::modules::part::vo::PartBatchCreateFailure {
                                    part_id: Some(new_id),
                                    code: mapped.code(),
                                    message: format!("{mapped}"),
                                    item_index: idx,
                                });
                                break;
                            }
                        }
                        if !part_files_ok {
                            if let Err(e) = sqlx::raw_sql(AssertSqlSafe(format!(
                                "ROLLBACK TO SAVEPOINT {sp_name}"
                            )))
                            .execute(repo.conn_mut())
                            .await
                            {
                                return Err((AppError::from(e), cleanup_tmp_keys));
                            }
                            // savepoint 已回滚，继续下一个 item
                        } else {
                            if let Err(e) =
                                sqlx::raw_sql(AssertSqlSafe(format!("RELEASE SAVEPOINT {sp_name}")))
                                    .execute(repo.conn_mut())
                                    .await
                            {
                                return Err((AppError::from(e), cleanup_tmp_keys));
                            }
                            // 注：2026-09-16 M2-B review 第 2 轮 B1 修 —— 不再在这里 push
                            // `successful_tmp_keys`。cleanup_tmp_keys 已在第一遍 head/copy
                            // 成功后全量收集，handler 统一 spawn 删除（与 per-item
                            // DB 结果无关）。
                            match repo.get_part_detail(new_id).await {
                                Ok(Some(p)) => {
                                    let (cn, l1cn) =
                                        lookup_customer_names(repo.conn_mut(), p.customer_id)
                                            .await
                                            .map_err(|e| (e, cleanup_tmp_keys.clone()))?;
                                    let current_batch_id =
                                        repo.find_current_inspection_batch_id(p.id).await.map_err(
                                            |e| (AppError::from(e), cleanup_tmp_keys.clone()),
                                        )?;
                                    created.push(PartDetailOut::from_with_customer_extra(
                                        p,
                                        current_batch_id,
                                        cn,
                                        l1cn,
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
                    }
                }
                Err(e) => {
                    if let Err(e) =
                        sqlx::raw_sql(AssertSqlSafe(format!("ROLLBACK TO SAVEPOINT {sp_name}")))
                            .execute(repo.conn_mut())
                            .await
                    {
                        return Err((AppError::from(e), cleanup_tmp_keys));
                    }
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
        // 2026-09-16 M2-B review 第 2 轮 B1 修：`cleanup_tmp_keys` 已是 head/copy 阶段
        // 全量收集的所有 tmp_key（与 per-item INSERT 是否成功无关），handler commit 后
        // 无差别 spawn 删除全部（成功 INSERT 的删 tmp 无害、失败未 INSERT 的删 tmp 必须）。
        let out = PartBatchCreateOut {
            created,
            failed,
            cleanup_tmp_keys: cleanup_tmp_keys.clone(),
        };
        Ok((out, cleanup_tmp_keys))
    }

    /// batch_create_parts 的 legacy 实现：与既有签名一致，不支持文件绑定。
    /// 2026-09-16 M2-B：拆出来供 batch_create_parts 复用（保留原 per-item savepoint 模型）。
    /// 2026-09-16 M2-C：从 crud.rs 迁移到本文件（按 docs/conventions.md §2 单文件职责拆分）。
    ///
    /// 2026-10-05：`POST /parts/batch` 不带任何 file binding 时走本路径（handler 按
    /// `has_bindings` 分流），而前端 PDF 批量上传正是这条路径，所以序列号派发与
    /// `unit_price` / `total_price` 透传必须在这里也做一遍，否则真实建单入口仍然
    /// 拿不到序列号。prefix 只取一次、per-item 派一次号的取舍与
    /// [`Self::batch_create_parts_with_bindings`] 同（序列号空洞可接受、
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
        // 与 with_bindings 同口径：prefix 缺失（20308）在写任何行之前整批拒
        let serial_prefix = repo.serial_prefix_for_customer(req.customer_id).await?;
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
                            let (cn, l1cn) =
                                lookup_customer_names(repo.conn_mut(), p.customer_id).await?;
                            let current_batch_id =
                                repo.find_current_inspection_batch_id(p.id).await?;
                            created.push(PartDetailOut::from_with_customer_extra(
                                p,
                                current_batch_id,
                                cn,
                                l1cn,
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

/// 2026-09-16 M2-B 新增：单 item 文件绑定预处理（head + copy + cas_key 派生）。
///
/// 复用 [`PartFileService::bind_uploaded_file`] 的 head/copy 校验语义，但**不**做
/// soft_delete + INSERT —— DB 写入由 batch_create 第二遍在事务内做。
///
/// 错误码：
/// - 40001 VALIDATION_ERROR — 字段校验失败（kind / sha / filename / size / content_type）
/// - 21114 BIZ_PART_FILE_TMP_OBJECT_MISSING — head_object 失败
/// - 21115 BIZ_PART_FILE_SIZE_MISMATCH — head size 与声明 size 不一致
/// - 21104 BIZ_PART_FILE_UPLOAD_FAILED — copy_object 失败
/// - 40000 BIZ_INVALID_VALUE — tmp_key 不在 cfg_tmp_prefix 范围内
#[allow(clippy::too_many_arguments)]
pub(super) async fn prepare_binding_head_copy(
    cos: Arc<dyn CosClient>,
    cfg_upload_prefix: &str,
    cfg_tmp_prefix: &str,
    #[allow(unused_variables)]
    // 2026-09-29 扁平化：CAS key 已不带 owner_id，参数保留以匹配 caller 签名
    future_owner_id: i64,
    kind: &str,
    binding: &FileBindingIn,
) -> Result<PreparedBinding, AppError> {
    use crate::modules::part_file::dto::validate;
    let max_file_size = 300 * 1024 * 1024usize;
    validate::check_kind(kind)?;
    validate::check_sha256(&binding.content_sha256)?;
    validate::check_filename(&binding.original_filename)?;
    validate::check_file_size(binding.file_size, max_file_size)?;
    validate::check_content_type(&binding.content_type, &binding.original_filename)?;

    if !binding.tmp_key.starts_with(cfg_tmp_prefix) {
        return Err(AppError::biz(
            code::BIZ_INVALID_VALUE,
            format!(
                "tmp_key {:?} 不在 cfg_tmp_prefix {:?} 范围内",
                binding.tmp_key, cfg_tmp_prefix
            ),
        ));
    }

    let meta = cos.head_object(&binding.tmp_key).await.map_err(|e| {
        AppError::biz(
            code::BIZ_PART_FILE_TMP_OBJECT_MISSING,
            format!("head_object 失败（tmp_key={:?}）: {e}", binding.tmp_key),
        )
    })?;
    if meta.size != binding.file_size {
        return Err(AppError::biz(
            code::BIZ_PART_FILE_SIZE_MISMATCH,
            format!(
                "tmp_key={:?} 客户端声明 size={} 与 head size={} 不一致",
                binding.tmp_key, binding.file_size, meta.size
            ),
        ));
    }

    let ext = policy::ext_of(&binding.original_filename)
        .ok_or_else(|| AppError::biz(code::BIZ_PART_FILE_BAD_TYPE, "缺少扩展名"))?;
    let file_type = policy::file_type_for_ext(&ext)
        .ok_or_else(|| AppError::biz(code::BIZ_PART_FILE_BAD_TYPE, format!("未知扩展名 {ext}")))?;
    // 2026-09-29 扁平化：模板五段→两段，owner/kind 信息已在 t_part_file DB 行外键索引。
    // 注：此处仍传 cfg_upload_prefix（保留兼容 .env 旧值；迁移 bin 不在本 worktree 范围）。
    let cas_key = crate::util::cos_key::build_cas_key(
        cfg_upload_prefix,
        &binding.content_sha256,
        &binding.original_filename,
    );
    cos.copy_object(&binding.tmp_key, &cas_key)
        .await
        .map_err(|e| {
            AppError::biz(
                code::BIZ_PART_FILE_UPLOAD_FAILED,
                format!(
                    "copy_object 失败（tmp={:?} → cas={cas_key:?}）: {e}",
                    binding.tmp_key
                ),
            )
        })?;

    Ok(PreparedBinding {
        tmp_key: binding.tmp_key.clone(),
        cas_key,
        kind: kind.to_string(),
        file_type: file_type.to_string(),
        sha256: binding.content_sha256.clone(),
        original_filename: binding.original_filename.clone(),
        file_size: binding.file_size,
        content_type: binding.content_type.clone(),
    })
}

// 仅为消除未用导入警告（类型由 impl 块自然带回）
#[allow(unused_imports)]
use crate::modules::part::model::NewPartEvent as _NewPartEventShim;

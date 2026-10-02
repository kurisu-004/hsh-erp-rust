//! prod::batch 的返修闭环：两个写点 + 三条集合读
//!
//! | 端点 | 方法 |
//! |---|---|
//! | `POST /{batch_id}/complete-repair` | `complete_repair` |
//! | `POST /{batch_id}/repair-dispatch` | `repair_dispatch` |
//! | `GET  /repair` | `list_repair_batches` |
//! | `GET  /repairing` | `list_repairing_batches` |
//!
//! 2026-10-01：REPAIRING 降级为 `t_part_batch.is_repairing` 标记列
//! （migration 005/006）后本流的语义：
//!
//! | 维度 | 改造前 | 改造后 |
//! |---|---|---|
//! | 「在返修中」判据 | `status = 'REPAIRING'` | `is_repairing = true` |
//! | `complete_repair` 守卫 | 枚举迁移白名单（源须为 REPAIRING） | `is_repairing == true`（否则 20118） |
//! | `complete_repair` / `repair_dispatch` 的写点 | 写 `status` | 写 `status` **并**清 `is_repairing`（经 `mark_batch_with_status_and_meta`）|
//! | 事件 `from_status` / `to_status` | 含 `'REPAIRING'` 字面量 | 一律写**真实**状态值（返修事实由 `event_type` + `is_repairing` 承载）|
//! | `GET /repairing` 列表 | `statuses = ['REPAIRING']` | `is_repairing = true`（`BatchListFilter::Repairing`）|

use sqlx::PgConnection;

use crate::auth::rbac::{CurrentUser, Role};
use crate::infra::snowflake::SnowflakeIdGenerator;
use crate::modules::part::model::NewPartEvent;
use crate::modules::part::repo::PartRepoTrait;
use crate::modules::part::statemachine::PartStatus;
use crate::modules::prod::batch::dto::{
    CompleteRepairRequest, RepairBatchListQuery, RepairDispatchRequest,
};
use crate::modules::prod::batch::vo::{InspectionBatchListItemOut, InspectionBatchListOut};
use crate::modules::prod::process_chain::repo::ProcessChainRepo;
use crate::modules::shelf::repo::ShelfRepo;
use crate::shared::error::{AppError, code};

use super::BatchService;
use super::guard::{
    InspectionRepairRow, assert_shelf_maps_process, mark_batch_with_status_and_meta,
    require_process_chain, validate_batch_version,
};

impl BatchService {
    // ===== 1.4 返修闭环 =====

    /// `POST /prod/batches/{batch_id}/complete-repair`：完成返修。
    ///
    /// 前置：批次 `is_repairing = true`（确实在返修中）。
    /// 去向由 shelf.zone 决定：PRODUCTION → `IN_PROCESS`（落回生产架、重新
    /// 入池并写目标工序）／INSPECTION → `INSPECTION`（送检区、出池）。
    /// 两条路径都把 `is_repairing` 清回 false（`mark_batch_with_status_and_meta`）。
    pub async fn complete_repair<R: PartRepoTrait>(
        mut repo: R,
        snowflake: &SnowflakeIdGenerator,
        batch_id: i64,
        req: CompleteRepairRequest,
        current: &CurrentUser,
    ) -> Result<crate::modules::part::vo::PartOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk, Role::Inspector])?;
        let batch = repo
            .find_batch_by_id(batch_id)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_PART_BATCH_NOT_FOUND, "batch 不存在"))?;
        // 2026-10-02：batch_id 来自 URL 路径参数（`POST /prod/batches/{batch_id}/…`），
        // part_id 由批次行反查。
        let part_id = batch.part_id;
        let part = repo
            .get_part_inspected(part_id)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_PART_NOT_FOUND, "part 不存在"))?;
        validate_batch_version(batch.id, req.version, batch.version)?;
        // 守卫 1：源状态必须在生产流里。REPAIRING 降级为标记列（migration
        // 005/006）后本端点不再是「状态迁移」，status 恒为 IN_PROCESS ——
        // 这条守卫拦住 PENDING / PROGRAMMING / INSPECTION / READY_TO_SHIP /
        // DELIVERED / OUTSOURCE / 终态 等一切「不在生产中」的批次。
        //
        // ⚠️ `PartStatus::from_str` 保留了 `"REPAIRING" => IN_PROCESS` 的过渡
        // 兼容分支（migration 006 之前的存量行），故它在本守卫下会被判为
        // IN_PROCESS 通过 —— 这是**有意的**：那是真实存在的存量数据形态。
        let from = PartStatus::from_str(&batch.status)
            .ok_or_else(|| AppError::biz(code::BIZ_INVALID_VALUE, "batch.status 非法"))?;
        if from != PartStatus::IN_PROCESS {
            return Err(AppError::biz(
                code::BIZ_PART_REPAIR_NOT_TRIGGERED,
                format!(
                    "complete-repair: batch {} 当前状态 {} 不允许（必须在生产中，即 IN_PROCESS）",
                    batch.id,
                    from.as_str()
                ),
            ));
        }
        // 守卫 2（**精确判定**，2026-10-01）：必须 `is_repairing = true`，即
        // 「确实在返修中」。
        //
        // 为什么必须落到列上、不能只看 status：REPAIRING 降级为标记后，
        // 返修态与正常生产态的 `status` **完全相同**（都是 IN_PROCESS），
        // 只判 status 会让任意在产批次都能调 complete-repair 直接改位置 / 工序
        // —— 相当于绕开「start-repair / scan-inspect FAIL」这两个起修入口
        // 任意搬运在产批次，是本轮改造最需要堵的口子。
        //
        // 错误码沿用 `BIZ_PART_REPAIR_NOT_TRIGGERED`（20118，HTTP 400）：语义
        // 精确对应「返修未触发」=「这个批次不在返修态」，且与守卫 1 同码，
        // 调用方（前端）原本就按 20118 区分「源状态不对」与「参数不对」，无需
        // 改契约。
        if !batch.is_repairing {
            return Err(AppError::biz(
                code::BIZ_PART_REPAIR_NOT_TRIGGERED,
                format!(
                    "complete-repair: batch {} 未处于返修中（is_repairing = false）。\
                     请先 start-repair 或 scan-inspect(pass=false) 起修",
                    batch.id
                ),
            ));
        }
        // shelf 区决定目标状态
        let shelf = ShelfRepo::get_by_id(repo.conn_mut(), req.shelf_id)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_SHELF_NOT_FOUND, "shelf 不存在"))?;
        if !shelf.is_active {
            return Err(AppError::biz(code::BIZ_SHELF_INACTIVE, "shelf 已停用"));
        }
        // 2026-09-30：新增 current_process_id（池归属权威依据）—— 返修完成
        // 回生产架即重新入池（写目标工序）；回送检区则出池（置 NULL）
        let (new_status, new_location, step_id_opt, process_id_opt) = match shelf.zone.as_str() {
            "PRODUCTION" => {
                let np = req.next_process_id.ok_or_else(|| {
                    AppError::biz(code::BIZ_INVALID_VALUE, "PRODUCTION 区需要 next_process_id")
                })?;
                // PR-3：PRODUCTION 区必须已绑定工艺链
                let chain_id = require_process_chain(repo.conn_mut(), part_id).await?;
                assert_shelf_maps_process(repo.conn_mut(), req.shelf_id, np).await?;
                // PR-3：解析 step_id
                let step_id =
                    ProcessChainRepo::resolve_step_id_by_process(repo.conn_mut(), chain_id, np)
                        .await?
                        .ok_or_else(|| {
                            AppError::biz(
                                code::BIZ_PROCESS_CHAIN_STEP_NOT_FOUND,
                                format!(
                                    "chain {} 内找不到 process_id={} 的活跃 step",
                                    chain_id, np
                                ),
                            )
                        })?;
                (
                    "IN_PROCESS",
                    Some("PRODUCTION_SHELF"),
                    Some(step_id),
                    Some(np),
                )
            }
            "INSPECTION" => {
                // INSPECTION 区不带 step（送检区不需要 process 上下文）；
                // 检验完成后 to_process / to_ship 再设 step。
                // 2026-09-30：同理不带 current_process_id（出池 → NULL）
                ("INSPECTION", Some("INSPECTION_SHELF"), None, None)
            }
            other => {
                return Err(AppError::biz(
                    code::BIZ_INVALID_VALUE,
                    format!("complete-repair 不允许 zone={other}"),
                ));
            }
        };
        let n = mark_batch_with_status_and_meta(
            repo.conn_mut(),
            batch.id,
            req.version,
            new_status,
            new_location,
            Some(req.shelf_id),
            step_id_opt,
            process_id_opt,
            current.id,
        )
        .await?;
        if n == 0 {
            return Err(AppError::biz(code::VERSION_CONFLICT, "batch 版本冲突"));
        }
        // 2026-10-01：`from_status` 由 `'REPAIRING'` 改为**真实**源状态
        // `'IN_PROCESS'`。
        //
        // 理由：`t_part_event.from_status` / `to_status` 是 `GET /parts/{id}/events`
        // 时间线上展示的「这一步从哪走到哪」，必须与 `t_part_batch.status` /
        // `t_part.status` 实际取到的值一致。REPAIRING 降级为标记列后，这两列
        // 再也不会是 'REPAIRING'（rollup 输出恒为 IN_PROCESS，见
        // `statemachine::normalize_rollup_status`），继续写 'REPAIRING' 会让
        // 同一批次在时间线与详情页显示两个互相矛盾的状态。
        //
        // 「源批次当时在返修中」这一事实由两处承载：事件类型
        // `REPAIR_COMPLETED` 本身（时间线上「完成返修」）+ `is_repairing` 列已
        // 被本调用清成 false（历史事实则留在历史行与 t_part_event 里）。
        //
        // 顺带说明：PRODUCTION 区去向的目标状态也是 IN_PROCESS，故该分支的事件
        // 形如 `IN_PROCESS → IN_PROCESS`（status 未变、只改了 location/holder/
        // 工序 + 清了标记）。这是**真实**的状态轨迹，不是笔误。
        repo.insert_part_event(NewPartEvent {
            id: snowflake.next_id(),
            part_id,
            event_type: "REPAIR_COMPLETED",
            from_status: Some("IN_PROCESS"),
            to_status: Some(new_status),
            batch_id: Some(batch.id),
            quantity: Some(batch.quantity),
            drawing_code: Some(&part.drawing_no),
            badge_code: None,
            note: req.note.as_deref(),
            created_by: Some(current.id),
        })
        .await?;
        let fresh = repo
            .get_part_inspected(part_id)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_PART_NOT_FOUND, "complete-repair 后查不到"))?;
        Ok(crate::modules::part::vo::PartOut::from(fresh))
    }

    /// `POST /prod/batches/{batch_id}/repair-dispatch`：一步式返修下发。
    ///
    /// 入口：IN_PROCESS / INSPECTION / READY_TO_SHIP / DELIVERED；目标状态由
    /// shelf.zone 决定（PRODUCTION → IN_PROCESS；INSPECTION → INSPECTION）。
    /// 一次调用完成「起修 + 到位」，故 `is_repairing` 在本调用内被清成 false
    /// （批次到达最终位置，不再处于待返修态）。
    pub async fn repair_dispatch<R: PartRepoTrait>(
        mut repo: R,
        snowflake: &SnowflakeIdGenerator,
        batch_id: i64,
        req: RepairDispatchRequest,
        current: &CurrentUser,
    ) -> Result<crate::modules::part::vo::PartOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk, Role::Inspector])?;
        let batch = repo
            .find_batch_by_id(batch_id)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_PART_BATCH_NOT_FOUND, "batch 不存在"))?;
        // 2026-10-02：batch_id 来自 URL 路径参数（`POST /prod/batches/{batch_id}/…`），
        // part_id 由批次行反查。
        let part_id = batch.part_id;
        let part = repo
            .get_part_inspected(part_id)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_PART_NOT_FOUND, "part 不存在"))?;
        validate_batch_version(batch.id, req.version, batch.version)?;
        let from = PartStatus::from_str(&batch.status)
            .ok_or_else(|| AppError::biz(code::BIZ_INVALID_VALUE, "batch.status 非法"))?;
        // 入口白名单（PR-M 2026-08-04：INSPECTION / READY_TO_SHIP / IN_PROCESS / DELIVERED）
        if !matches!(
            from,
            PartStatus::INSPECTION
                | PartStatus::READY_TO_SHIP
                | PartStatus::IN_PROCESS
                | PartStatus::DELIVERED
        ) {
            return Err(AppError::biz(
                code::BIZ_INVALID_TRANSITION,
                format!("repair-dispatch: 起点 {from:?} 不允许"),
            ));
        }
        let shelf = ShelfRepo::get_by_id(repo.conn_mut(), req.shelf_id)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_SHELF_NOT_FOUND, "shelf 不存在"))?;
        if !shelf.is_active {
            return Err(AppError::biz(code::BIZ_SHELF_INACTIVE, "shelf 已停用"));
        }
        // 2026-09-30：新增 current_process_id（池归属权威依据）—— 返修下发
        // 到生产架即入池（写目标工序）；下发到送检区则出池（置 NULL）
        let (new_status, new_location, step_id_opt, process_id_opt) = match shelf.zone.as_str() {
            "PRODUCTION" => {
                let np = req.next_process_id.ok_or_else(|| {
                    AppError::biz(code::BIZ_INVALID_VALUE, "PRODUCTION 区需要 next_process_id")
                })?;
                // PR-3：PRODUCTION 区必须已绑定工艺链
                let chain_id = require_process_chain(repo.conn_mut(), part_id).await?;
                assert_shelf_maps_process(repo.conn_mut(), req.shelf_id, np).await?;
                // PR-3：解析 step_id
                let step_id =
                    ProcessChainRepo::resolve_step_id_by_process(repo.conn_mut(), chain_id, np)
                        .await?
                        .ok_or_else(|| {
                            AppError::biz(
                                code::BIZ_PROCESS_CHAIN_STEP_NOT_FOUND,
                                format!(
                                    "chain {} 内找不到 process_id={} 的活跃 step",
                                    chain_id, np
                                ),
                            )
                        })?;
                (
                    "IN_PROCESS",
                    Some("PRODUCTION_SHELF"),
                    Some(step_id),
                    Some(np),
                )
            }
            "INSPECTION" => ("INSPECTION", Some("INSPECTION_SHELF"), None, None),
            other => {
                return Err(AppError::biz(
                    code::BIZ_INVALID_VALUE,
                    format!("repair-dispatch 不允许 zone={other}"),
                ));
            }
        };
        // 一步式：单条 UPDATE + 两条事件日志（REPAIR_STARTED + REPAIR_COMPLETED），
        // 与 Python `repair_dispatch` 一致。
        //
        // 2026-10-01 事件字段改写（本函数的事件仍按**逻辑两步**记录：起修 → 到位，
        // 只是 DB 写入是「一步式」）：
        // - REPAIR_STARTED：`to_status` 由 `'REPAIRING'` 改为 `'IN_PROCESS'`
        //   —— 「起修」这个逻辑中间态的真实 status 就是 IN_PROCESS（返修仍在生产
        //   中，progress 与原 REPAIRING 同档 2）。
        // - REPAIR_COMPLETED：`from_status` 同理由 `'REPAIRING'` 改为
        //   `'IN_PROCESS'`，与上一条首尾相接，拼出的仍是
        //   `S → IN_PROCESS（起修）→ T（到位）` 这条完整轨迹。
        // 写 'REPAIRING' 的话，时间线上的状态会与批次实际 status 矛盾（理由与
        // `complete_repair` 的同款说明）。
        let n = mark_batch_with_status_and_meta(
            repo.conn_mut(),
            batch.id,
            req.version,
            new_status,
            new_location,
            Some(req.shelf_id),
            step_id_opt,
            process_id_opt,
            current.id,
        )
        .await?;
        if n == 0 {
            return Err(AppError::biz(code::VERSION_CONFLICT, "batch 版本冲突"));
        }
        // 2026-09-16 PR-2 瘦身（migration 027）：t_part_batch 删
        // `has_been_repaired` 列；返修事实由下方两条 t_part_event 事件日志
        // 追溯（REPAIR_STARTED + REPAIR_COMPLETED）。
        // 两条事件
        repo.insert_part_event(NewPartEvent {
            id: snowflake.next_id(),
            part_id,
            event_type: "REPAIR_STARTED",
            from_status: Some(from.as_str()),
            to_status: Some("IN_PROCESS"),
            batch_id: Some(batch.id),
            quantity: Some(batch.quantity),
            drawing_code: Some(&part.drawing_no),
            badge_code: None,
            note: req.reason.as_deref(),
            created_by: Some(current.id),
        })
        .await?;
        repo.insert_part_event(NewPartEvent {
            id: snowflake.next_id(),
            part_id,
            event_type: "REPAIR_COMPLETED",
            from_status: Some("IN_PROCESS"),
            to_status: Some(new_status),
            batch_id: Some(batch.id),
            quantity: Some(batch.quantity),
            drawing_code: Some(&part.drawing_no),
            badge_code: None,
            note: req.note.as_deref(),
            created_by: Some(current.id),
        })
        .await?;
        let fresh = repo
            .get_part_inspected(part_id)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_PART_NOT_FOUND, "repair-dispatch 后查不到"))?;
        Ok(crate::modules::part::vo::PartOut::from(fresh))
    }

    /// `GET /prod/batches/repair`：DELIVERED 批次列表（M+C+I）。
    /// 复用 `RepairBatchListQuery` + repo；status=DELIVERED。
    pub async fn list_repair_batches<R: PartRepoTrait>(
        mut repo: R,
        query: &RepairBatchListQuery,
        current: &CurrentUser,
    ) -> Result<InspectionBatchListOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk, Role::Inspector])?;
        Self::list_batches_matching(
            repo.conn_mut(),
            query,
            BatchListFilter::Statuses(&["DELIVERED"]),
            current,
        )
        .await
    }

    /// `GET /prod/batches/repairing`：返修中批次列表（M+C+I）。
    ///
    /// 2026-10-01：判据由 `status = 'REPAIRING'` 改为 `is_repairing = true`
    /// （REPAIRING 降级为标记列，migration 005/006）。
    ///
    /// ⚠️ 关于是否需要独立的 repo 方法：本函数与 `list_repair_batches` 共用
    /// **同一条** 30 行 SELECT（同一 `InspectionRepairRow` 投影、同一个
    /// `InspectionBatchListItemOut` DTO、同套分页 / 过滤 / 计数），差别**只有
    /// WHERE 里的一个判据**。为此新增 `PartBatchRepo` 方法有两种做法，都更差：
    /// (1) 复制整条 SQL（两份必须手工保持同步的投影，漂移即两端口径不一致）；
    /// (2) 把 SQL 搬进 `prod/batch/repo/queries.rs` 并连带搬迁 `InspectionRepairRow`
    ///   —— 那是与本轮「消费新列」无关的结构搬迁。故本轮用
    ///   [`BatchListFilter`] 把判据显式化（调用点读起来就是「按标记过滤」，
    ///   不再出现 `'REPAIRING'` 字面量），判据在 SQL 层仍是同一个 bind 位。
    pub async fn list_repairing_batches<R: PartRepoTrait>(
        mut repo: R,
        query: &RepairBatchListQuery,
        current: &CurrentUser,
    ) -> Result<InspectionBatchListOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk, Role::Inspector])?;
        Self::list_batches_matching(repo.conn_mut(), query, BatchListFilter::Repairing, current)
            .await
    }

    /// 通用 INSPECTION / DELIVERED / 返修中 等批次列表实现。
    async fn list_batches_matching(
        conn: &mut PgConnection,
        query: &RepairBatchListQuery,
        filter: BatchListFilter<'_>,
        _current: &CurrentUser,
    ) -> Result<InspectionBatchListOut, AppError> {
        let (statuses, by_repairing_flag) = match filter {
            BatchListFilter::Statuses(s) => (s, false),
            BatchListFilter::Repairing => (&[][..], true),
        };
        let limit = query.limit.unwrap_or(200).clamp(1, 500);
        let offset = query.offset.unwrap_or(0).max(0);
        let keyword = query.keyword.as_deref().unwrap_or("");
        // 2026-09-30（review 第 3 轮 M3 补漏）：`next_process_id` /
        // `next_process_name` **继续**从 `current_process_step_id` 经
        // `LEFT JOIN t_process_chain_step s2` 派生，`t_process p2` 的 JOIN 条件
        // 同步挂 `s2.process_id`。
        //
        // 本分支 d8f5788（原 6ecdf2c，migration 004）一度把两列改直读
        // `b.current_process_id`（并删掉 s2 JOIN），**该改法已回退**，理由：
        //
        // 本函数服务 `GET /prod/batches/repair`（`DELIVERED`）与
        // `GET /prod/batches/repairing`（`is_repairing = true`），两个端点
        // 都**不是工序池端点**（工序池端点只认 `status='IN_PROCESS' AND
        // location='PRODUCTION_SHELF'`），与 `current_process_id` 无关。
        //
        // - **DELIVERED**：`can_transition_to` 中进 `READY_TO_SHIP` 的边**只有**
        //   `INSPECTION → READY_TO_SHIP`，进 `DELIVERED` 的边**只有**
        //   `READY_TO_SHIP → DELIVERED`（无 `OUTSOURCE → READY_TO_SHIP`、无
        //   `PENDING → READY_TO_SHIP`）⇒ DELIVERED 批次**必经 INSPECTION**；而
        //   所有进 INSPECTION 的写点都把 `current_process_id` 置 NULL
        //   （`phase1::scan` / `outsource::receive_to_inspection` /
        //   `repair::complete_repair` / `mark_batch_inspected`），其后
        //   `mark_batch_passed_inspection` 与 `mark_batch_delivered` 都**不写该列**
        //   ⇒ 直读会让本端点的 `next_process_id` / `next_process_name`
        //   **结构性恒 null**（用户可见回归，与 M3 修掉的 inspection-batches
        //   回归完全同形）。
        // - **返修中批次**（`is_repairing = true`）：2026-10-01 起
        //   `mark_batch_repairing` **不再翻 status**（保持 IN_PROCESS），
        //   迁移 004「已知局限 (4d)」记录的「残留陈旧 cpid」场景**已不复存在**
        //   —— 返修中的批次要么停在送检架（出池，该列按不变式为 NULL，由
        //   `scan-inspect` 第一步清空）、要么留在原工序池（该列是它**当前**
        //   池归属、不是陈旧值）。但本 VO 仍走 step 派生：判据从「列在结构上
        //   恒 NULL」变成「统一分工」—— 「展示类列表一律走 step 派生」这条
        //   规则不因单个端点判据变化而分叉（否则同一条 SQL 要为两种口径各写
        //   一套投影）。
        //
        // 读取方分工（勿越界）：`current_process_id` 的读取方严格限定为 5 条工序池
        // SQL + `list_pickable_by_work_type` + rollup 派生；**展示类列表一律走
        // step 派生**。完整清单见 `prod/batch/model.rs` 模块 doc。
        //
        // ⚠️ 本函数与 `prod/batch/repo/queries.rs::list_batches_with_part`（服务
        // `GET /prod/batches/inspection`）是**两条独立 SQL**，M3 只回退了后者；
        // 本条当时漏网，本次补齐。
        let rows: Vec<InspectionRepairRow> = sqlx::query_as::<_, InspectionRepairRow>(
            "SELECT b.id AS batch_id, b.part_id, b.batch_no, b.quantity, b.status, b.is_repairing,              b.location, b.version, b.current_process_step_id, b.parent_batch_id,              b.current_holder_id, COALESCE(s.name, w.name, oc.name) AS holder_name,              s2.process_id AS next_process_id, p2.name AS next_process_name,              b.delivery_note_id, dn.delivery_note_no,              p.serial_no, p.drawing_no, p.name, p.order_no, p.planned_delivery_date,              p.is_urgent, p.version AS part_version, p.created_at, p.updated_at,              p.customer_id, c.name AS customer_name, c_l1.name AS l1_customer_name              FROM t_part_batch b JOIN t_part p ON p.id = b.part_id              LEFT JOIN t_customer c ON c.id = p.customer_id              LEFT JOIN t_customer c_l1 ON c_l1.id = c.parent_id AND c_l1.deleted_at IS NULL              LEFT JOIN t_shelf s ON s.id = b.current_holder_id              LEFT JOIN t_worker w ON w.id = b.current_holder_id              LEFT JOIN t_outsource_company oc ON oc.id = b.current_holder_id              LEFT JOIN t_process_chain_step s2 ON s2.id = b.current_process_step_id              LEFT JOIN t_process p2 ON p2.id = s2.process_id              LEFT JOIN t_delivery_note dn ON dn.id = b.delivery_note_id              WHERE b.deleted_at IS NULL AND p.deleted_at IS NULL              AND (CASE WHEN $9::bool THEN b.is_repairing = true ELSE b.status = ANY($1) END)              AND (NOT $9::bool OR b.status NOT IN ('COMPLETED', 'CANCELLED'))              AND ($2 = '' OR p.drawing_no ILIKE '%' || $2 || '%' OR p.name ILIKE '%' || $2 || '%')              AND ($3::bigint IS NULL OR p.customer_id = $3)              AND ($4::text IS NULL OR p.serial_no ILIKE '%' || $4 || '%')              AND ($5::date IS NULL OR p.planned_delivery_date >= $5)              AND ($6::date IS NULL OR p.planned_delivery_date <= $6)              ORDER BY b.id DESC LIMIT $7 OFFSET $8",
        )
        .bind(statuses)
        .bind(keyword)
        .bind(query.customer_id)
        .bind(query.serial_no.as_deref())
        .bind(query.planned_delivery_date_from)
        .bind(query.planned_delivery_date_to)
        .bind(limit)
        .bind(offset)
        // 判据开关：true = 按 `is_repairing` 过滤（此时 $1 绑空数组）
        .bind(by_repairing_flag)
        .fetch_all(&mut *conn)
        .await?;
        let items: Vec<InspectionBatchListItemOut> = rows
            .into_iter()
            .map(|r| InspectionBatchListItemOut {
                batch_id: r.batch_id,
                batch_no: r.batch_no,
                quantity: r.quantity,
                status: r.status,
                // 2026-10-01 review 第 1 轮 M5：REPAIRING 已降级为标记列，
                // `status` 恒为 IN_PROCESS，「返修中」只能由本字段表达
                is_repairing: r.is_repairing,
                location: r.location,
                version: r.version,
                current_process_step_id: r.current_process_step_id,
                parent_batch_id: r.parent_batch_id,
                current_holder_id: r.current_holder_id,
                holder_name: r.holder_name,
                // 2026-09-30（review 第 3 轮 M3 补漏）：派生自 `s2.process_id`
                // （LEFT JOIN t_process_chain_step），**刻意不直读
                // `b.current_process_id`** —— 理由见上方 SQL 注释。字段名保留以
                // 兼容 `InspectionBatchListItemOut` DTO 与前端。
                next_process_id: r.next_process_id,
                next_process_name: r.next_process_name,
                delivery_note_id: r.delivery_note_id,
                delivery_note_no: r.delivery_note_no,
                part_id: r.part_id,
                serial_no: r.serial_no,
                drawing_no: r.drawing_no,
                name: r.name,
                order_no: r.order_no,
                planned_delivery_date: r.planned_delivery_date,
                is_urgent: r.is_urgent,
                part_version: r.part_version,
                created_at: r.created_at,
                updated_at: r.updated_at,
                customer_id: r.customer_id,
                customer_name: r.customer_name,
                l1_customer_name: r.l1_customer_name,
            })
            .collect();
        // 计数（轻量重发一次同条件但不带分页）
        //
        // 2026-10-01：判据与上面那条 SELECT **必须逐字一致**（开关 + 终态排除），
        // 否则 `total` 与 `items` 会各说各话（分页第一页就是空的）。
        // ⚠️ 本条无 LIMIT/OFFSET，判据开关的位次是 **$7**（上面那条是 $9）——
        // 位次不同是两条 SQL 唯一的形态差异，改任一条的 bind 顺序时记得同步。
        let total: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) AS n \
             FROM t_part_batch b JOIN t_part p ON p.id = b.part_id \
             WHERE b.deleted_at IS NULL AND p.deleted_at IS NULL \
             AND (CASE WHEN $7::bool THEN b.is_repairing = true ELSE b.status = ANY($1) END) \
             AND (NOT $7::bool OR b.status NOT IN ('COMPLETED', 'CANCELLED')) \
             AND ($2 = '' OR p.drawing_no ILIKE '%' || $2 || '%' OR p.name ILIKE '%' || $2 || '%') \
             AND ($3::bigint IS NULL OR p.customer_id = $3) \
             AND ($4::text IS NULL OR p.serial_no ILIKE '%' || $4 || '%') \
             AND ($5::date IS NULL OR p.planned_delivery_date >= $5) \
             AND ($6::date IS NULL OR p.planned_delivery_date <= $6)",
        )
        .bind(statuses)
        .bind(keyword)
        .bind(query.customer_id)
        .bind(query.serial_no.as_deref())
        .bind(query.planned_delivery_date_from)
        .bind(query.planned_delivery_date_to)
        .bind(by_repairing_flag)
        .fetch_one(&mut *conn)
        .await?;
        Ok(InspectionBatchListOut {
            items,
            total,
            limit,
            offset,
        })
    }
}

/// 批次列表的判据模式（2026-10-01 新增，替代原 `statuses: &[&str]` 直参）。
///
/// 为什么抽成枚举而不留两个函数 / 复制两条 SQL：两个端点
/// （`repair-batches` / `repairing-batches`）共用**同一条** SELECT + 同一套
/// 分页 / 过滤 / 计数，差别只有 WHERE 里的一个判据。枚举把「按状态过滤」与
/// 「按返修标记过滤」两种判据在**类型层面**分开（`statuses` 在
/// [`BatchListFilter::Repairing`] 分支下必然是空数组，不会出现「既按
/// status 又按标记」的混合语义），同时让调用点不再出现 `'REPAIRING'`
/// 字面量。
#[derive(Debug, Clone, Copy)]
enum BatchListFilter<'a> {
    /// `b.status = ANY(statuses)`（如 `["DELIVERED"]`）。
    Statuses(&'a [&'a str]),
    /// `b.is_repairing = true`（2026-10-01：REPAIRING 降级为标记列后的
    /// 「返修中」判据）。
    ///
    /// 额外排除 `status IN ('COMPLETED','CANCELLED')`：正常路径下终态批次不会
    /// 带 `is_repairing = true`（`complete_repair` / `repair_dispatch` /
    /// `cancel_batch` 都清标记），但 part 级批量取消走的
    /// `status_gate::apply_bulk_batch_status_change_for_part` **只写 status**，
    /// 会留下一批 `CANCELLED + is_repairing=true` 的行。把它们排除掉，
    /// 「待返修列表」就不会列出已作废的批次（否则用户点进去才发现货早没了）。
    /// 该兜底在 bulk 写点补上标记写入后可去掉。
    Repairing,
}

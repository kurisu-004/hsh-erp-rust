//! prod::scan 的扫码接管：`POST /api/v2/prod/scan/take-over`（2026-10-15 新增 S2）
//!
//! 「扫码找件 · 自助恢复」三件套的第二件（S1 = 只读诊断 `GET /scan/locate`，
//! S3 = 外协收回 `POST /scan/outsource-receive`，**2026-10-15 已落地**）。场景：
//! 批次仍挂在**别的**工人名下
//! （`IN_PROCESS + location='WORKER'` + `current_holder_id != 我`），但图纸 / 实物
//! 已经流转到这位工人手里 ⇒ 扫工牌一键接管：holder 换成我、顺手推进到我工种下的
//! 下一道工序，同一事务原子完成。
//!
//! ## 判据顺序（**顺序即错误码优先级契约，不要调整**）
//!
//! | # | 判据 | 失败 |
//! |---|---|---|
//! | 1 | 工牌反查 worker（`get_by_badge_code(..., false)`） | 不存在 `20201` / 停用 `20202` / 无工种 `20206` |
//! | 2 | 批次存在（`find_batch_by_id`）、part 存在（`get_part_inspected`） | `20109` / `20101` |
//! | 3 | 客户端 `version` 与批次 OCC 比对 | `40901` |
//! | 4 | `status='IN_PROCESS'` 且 `location='WORKER'` 且**有持有人** | `20103` |
//! | 5 | 持有人不是我自己 | `20104`「批次已在你手上，无需接管」 |
//! | 6 | 链位置解析出下一道（`pointer_consistent && NEXT` 免填，否则 `next_process_id` 必填） | 缺 `40001` / 链内定位不到 `20702` |
//! | 7 | 下一道 ∈ 我的工种工序映射（`WorkTypeRepo::list_process_ids`） | `20104` |
//! | 8 | 容量守卫 `QueueService::ensure_worker_has_capacity` | `20904` / `20204` |
//! | 9 | 原子 UPDATE（`PartBatchRepo::transfer_to_worker`） | 0 行 `40901` |
//!
//! 与 S1 `locate` 的 `TAKE_OVER` 判定**刻意同源**（写侧会成功的绝不显示 `NONE`、
//! 写侧会拒绝的绝不显示可点按钮）：第 6 / 7 两步与 `locate` 的
//! `take_over_gate` 对位（推导得出时校验「下一道 ∈ 工种」，推导不出时放行到
//! 手填流由写侧兜底）。第 4 步的「有持有人」与 `classify_batch` 的
//! 「holder 为 NULL 不给 TAKE_OVER」也是同一条订正。
//!
//! ## 第 6 步的两条分支（与 worker-scan RETURNED 逐条同源）
//!
//! - **`pointer_consistent && chain_state == "NEXT"`**：链上下一道已定位到，
//!   `next_process_id` 可省略；`position.next_process_id` 为 `None` 说明派生 SQL
//!   内部不自洽，按 worker-scan 同款拒 `20702`。
//! - **其余形态**（无链 / 链已软删 / 指针漂移 / 链内歧义 / 链尾 TAIL）：
//!   `req.next_process_id` 必填（缺 `40001`）；显式值经
//!   `optional_process_chain` + `optional_step_id` 解析 ——
//!   **无链 ⇒ step 可空**（该列允许 NULL）；**有链但链内没有这道工序 ⇒ `20702`**。
//!   链尾（TAIL）同样只能指定**链内**工序：越链的显式值一律 `20702`。
//!
//! ## 写入（第 9 步）为什么不是 `mark_batch_with_status_and_meta`
//!
//! 那个漏斗会**无条件清 `is_repairing`**，且 WHERE 没有「旧持有人」条件 ——
//! 用它做接管会把返修件静默挪出返修流、且并发下可能从已易主的批次上再接管一次。
//! 接管专用的 `PartBatchRepo::transfer_to_worker` 只动
//! `holder / location / process / step / version / updated_*` 六处，**不写 status**、
//! **不碰 is_repairing**（回归：`take_over_keeps_is_repairing_flag`）。
//!
//! 写入后照 worker-scan RETURNED 分支：`PartService::sync_from_batch_change`
//! **只调一次**（part 派生 + 父装配件级联），再写 `TRANSFERRED` 事件留痕。
//! 不做部分数量 —— 接管是整批动作。

use crate::auth::rbac::{CurrentUser, Role};
use crate::infra::snowflake::SnowflakeIdGenerator;
use crate::modules::assembly::service::SyncOutcome;
use crate::modules::part::repo::PartRepoTrait;
use crate::modules::part::service::PartService;
use crate::modules::prod::batch::repo::PartBatchRepo;
use crate::modules::prod::queue::service::QueueService;
use crate::modules::prod::scan::dto::TakeOverRequest;
use crate::modules::prod::scan::vo::TakeOverOut;
use crate::modules::prod::worker::repo::WorkerRepo;
use crate::shared::batch::guards::validate_batch_version;
use crate::shared::error::{AppError, code};
use crate::shared::part_event::{NewPartEvent, insert};

use super::ScanService;

impl ScanService {
    /// `POST /api/v2/prod/scan/take-over`：扫码接管（2026-10-15 新增，S2）。
    ///
    /// 角色守卫在 handler 与 service 双层（2026-10-15 review 订正，与 outsource_receive
    /// 对齐）；事务边界在 handler。判据顺序见本文件模块 doc 的表。
    pub async fn take_over<R: PartRepoTrait>(
        mut repo: R,
        snowflake: &SnowflakeIdGenerator,
        req: TakeOverRequest,
        current: &CurrentUser,
    ) -> Result<TakeOverOut, AppError> {
        // ── 0. 角色守卫（service 入口第一行，2026-10-15 review 补，与 pickup /
        //    outsource_receive 对齐；handler 侧已有同名守卫，行为逐字不变）──────
        current.require_any_role(&[Role::Manager, Role::ShelfAccount])?;
        // ── 1. 工牌 → worker（与 worker-scan 逐条同形，include_deleted=false）──
        let worker = WorkerRepo::get_by_badge_code(repo.conn_mut(), &req.badge_code, false)
            .await?
            .ok_or_else(|| {
                AppError::biz(
                    code::BIZ_WORKER_NOT_FOUND,
                    format!("badge_code {} 未注册", req.badge_code),
                )
            })?;
        if !worker.is_active {
            return Err(AppError::biz(
                code::BIZ_WORKER_INACTIVE,
                format!("worker {} 已停用", worker.id),
            ));
        }
        let work_type_id = worker.work_type_id.ok_or_else(|| {
            AppError::biz(
                code::BIZ_WORKER_NO_WORK_TYPE,
                format!("worker {} 未分配工种", worker.id),
            )
        })?;
        // ── 2. 批次 + part ────────────────────────────────────────────────
        let batch = repo.find_batch_by_id(req.batch_id).await?.ok_or_else(|| {
            AppError::biz(
                code::BIZ_PART_BATCH_NOT_FOUND,
                format!("batch {} 不存在或已软删", req.batch_id),
            )
        })?;
        let part = repo
            .get_part_inspected(batch.part_id)
            .await?
            .ok_or_else(|| {
                AppError::biz(
                    code::BIZ_PART_NOT_FOUND,
                    format!("part {} 不存在或已软删", batch.part_id),
                )
            })?;
        // ── 3. OCC（客户端传值，禁止「读-再-比」的隐式 OCC）───────────────
        validate_batch_version(batch.id, req.version, batch.version)?;
        // ── 4. 状态 / 位置 / 持有人存在性 ─────────────────────────────────
        if batch.status != "IN_PROCESS" || batch.location.as_deref() != Some("WORKER") {
            return Err(AppError::biz(
                code::BIZ_INVALID_TRANSITION,
                format!(
                    "batch {} 当前 status={} location={:?}，不允许接管（要求 IN_PROCESS + WORKER）",
                    batch.id, batch.status, batch.location
                ),
            ));
        }
        // `location='WORKER'` 却没有持有人是**数据异常**（任何写路径都不会产出这种
        // 形态）：接管的前提是「批次挂在别人手上」，无人持有则无对象可接管，
        // 且本端点的出参 `from_worker_id` / 事件备注都以「存在旧持有人」为前提。
        // 与 S1 `locate` 的 `classify_batch` 对位（holder 为 NULL 不给 TAKE_OVER）。
        let from_worker_id = batch.current_holder_id.ok_or_else(|| {
            AppError::biz(
                code::BIZ_INVALID_TRANSITION,
                format!(
                    "batch {} location='WORKER' 但没有持有人，无法接管（数据异常）",
                    batch.id
                ),
            )
        })?;
        // ── 5. 持有人是我自己 ⇒ 没有「接管」这个动作 ──────────────────────
        if from_worker_id == worker.id {
            return Err(AppError::biz(
                code::BIZ_INVALID_VALUE,
                "批次已在你手上，无需接管",
            ));
        }
        // ── 6. 链位置解析（照 worker-scan RETURNED 分支）─────────────────
        let position =
            crate::shared::batch::chain::resolve_chain_position(repo.conn_mut(), part.id, &batch)
                .await?;
        let (next_pid, step_id_opt): (i64, Option<i64>) = if position.is_pointer_consistent(&batch)
            && position.chain_state.as_deref() == Some("NEXT")
        {
            // 顺应工序：链上下一道已定位到，可免填。
            let pid = position.next_process_id.ok_or_else(|| {
                AppError::biz(
                    code::BIZ_PROCESS_CHAIN_STEP_NOT_FOUND,
                    "链位置派生返回 NEXT 但没有下一道工序（SQL 内部不自洽）",
                )
            })?;
            (pid, position.next_step_id)
        } else {
            // 非顺应（无链 / 链已软删 / 指针漂移 / 链内歧义 / 链尾）：必须显式指定
            // 接管后的工序。链尾同样只能指定链内工序（越链 ⇒ 20702，见
            // `optional_step_id` 的 doc）。
            let raw = req.next_process_id.as_deref().ok_or_else(|| {
                AppError::validation(
                    "非顺应工序（无链 / 链已软删 / 指针漂移 / 当前工序不在链内 / 当前已是链尾），\
                     必须在 next_process_id 里显式指定接管后的工序",
                )
            })?;
            let pid = raw
                .parse()
                .map_err(|_| AppError::validation("next_process_id 非法"))?;
            let chain_id =
                crate::shared::batch::optional_process_chain(repo.conn_mut(), part.id).await?;
            let step =
                crate::shared::batch::optional_step_id(repo.conn_mut(), chain_id, pid).await?;
            (pid, step)
        };
        // ── 7. 下一道 ∈ 我的工种工序映射（与 queue/move POOL→WORKER 同源）────
        let process_ids = crate::modules::prod::work_type::repo::WorkTypeRepo::list_process_ids(
            repo.conn_mut(),
            work_type_id,
        )
        .await?;
        if !process_ids.contains(&next_pid) {
            return Err(AppError::biz(
                code::BIZ_INVALID_VALUE,
                format!(
                    "worker {} 的工种不含工序 {next_pid}（接管目标工序）",
                    worker.id
                ),
            ));
        }
        // ── 8. 容量守卫（20904 / 20204；返回值是给 move 填展示字段的，此处丢弃）──
        let _ = QueueService::ensure_worker_has_capacity(repo.conn_mut(), worker.id, work_type_id)
            .await?;
        // ── 9. 原子写：holder worker → worker + 工序 / step 成对推进（OCC）────
        let n = PartBatchRepo::transfer_to_worker(
            repo.conn_mut(),
            batch.id,
            req.version,
            Some(from_worker_id),
            worker.id,
            next_pid,
            step_id_opt,
            current.id,
        )
        .await?;
        if n == 0 {
            return Err(AppError::biz(code::VERSION_CONFLICT, "乐观锁失败"));
        }
        // part 派生 + 父装配件级联（**只调一次**；status 未变，event_id 传 None ——
        // 该批次仍非终态，min-progress 推不出终态，归档分支不可达）。
        let sync = PartService::sync_from_batch_change(&mut repo, part.id, current, None).await?;
        // 事件留痕：接手人 + 目标工序快照 + 「接管自谁」备注。
        // 备注两态：无 body note → `"接管自 worker {旧持有人}"`；有 → 拼 `：{note}`。
        let note = match req.note.as_deref() {
            None => format!("接管自 worker {from_worker_id}"),
            Some(extra) => format!("接管自 worker {from_worker_id}：{extra}"),
        };
        insert(
            repo.conn_mut(),
            NewPartEvent {
                id: snowflake.next_id(),
                part_id: part.id,
                event_type: "TRANSFERRED",
                // 接管不改 status（IN_PROCESS 保持），两列同值只为满足事件行的
                // 「前后状态」列约束。
                from_status: Some("IN_PROCESS"),
                to_status: Some("IN_PROCESS"),
                batch_id: Some(batch.id),
                quantity: Some(batch.quantity),
                drawing_code: Some(&part.drawing_no),
                badge_code: Some(&worker.badge_code),
                // 作业人 = 接管人（`t_worker.id`，不是账号 id）。
                worker_id: Some(worker.id),
                // 事件时刻的工序快照取**接管后**的目标工序：接管动作的本质就是
                // 「这一批从现在起在新工序上开工」，与 RETURNED 的「刚做完哪道」
                // 相反。见 `shared/part_event/mod.rs` 的写入点清单。
                process_id: Some(next_pid),
                note: Some(note.as_str()),
                created_by: Some(current.id),
            },
        )
        .await?;
        Ok(TakeOverOut {
            batch_id: batch.id,
            part_id: part.id,
            from_worker_id,
            to_worker_id: worker.id,
            next_process_id: next_pid,
            next_process_step_id: step_id_opt,
            // 与 UPDATE 的 `version = version + 1` 同值（第 3 步已保证
            // `req.version == batch.version`）。
            version: batch.version + 1,
            synced_assembly_id: match sync {
                SyncOutcome::Changed(aid) => Some(aid),
                SyncOutcome::NoChange => None,
            },
        })
    }
}

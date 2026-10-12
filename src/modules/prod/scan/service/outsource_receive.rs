//! prod::scan 的外协确认收回：`POST /api/v2/prod/scan/outsource-receive`（2026-10-15 S3）
//!
//! 「扫码找件 · 自助恢复」三件套的第三件（S1 = 只读诊断 `GET /scan/locate`，
//! S2 = 扫码接管 `POST /scan/take-over`）。场景：件实际已从外协回厂（工人扫码），
//! 但库里批次仍停在 `OUTSOURCE + OUTSOURCE_COMPANY` 挂在公司名下 ⇒ 扫工牌一步
//! 「① 收回落自动选架 ② 立刻交到扫码工人手上」，**一个事务原子完成**。
//!
//! ## 判据顺序（**顺序即错误码优先级契约，不要调整**）
//!
//! | # | 判据 | 失败 |
//! |---|---|---|
//! | 1 | 工牌反查 worker（`get_by_badge_code(..., false)`） | 不存在 `20201` / 停用 `20202` / 无工种 `20206` |
//! | 2 | 批次存在、part 存在 | `20109` / `20101` |
//! | 3 | 客户端 `version` 与批次 OCC 比对 | `40901` |
//! | 4 | `status='OUTSOURCE'`；`location='OUTSOURCE_COMPANY'` 且有持有人 | `20103` / `20122` |
//! | 5 | 目标工序解析（三态，见下） | `40001` / 零候选 `20104` / `20706` |
//! | 6 | 目标工序 ∈ 扫码工种的活跃工序映射（`WorkTypeRepo::list_process_ids`） | `20104` |
//! | 7 | 容量预检 `QueueService::ensure_worker_has_capacity` | `20904` / `20204` |
//! | 8 | stage①：`receive_from_outsource_inner`（落 `IN_PROCESS + PRODUCTION_SHELF` + 关 shipment + `RECEIVED_FROM_OUTSOURCE`） | `20508` |
//! | 9 | stage②：`ScanService::pick_up`（同事务把批次交到工人手上 + `PICKED_UP`） | 复检容量 `20904` / `20204`、OCC `40901` |
//!
//! ## 第 5 步的三个分支（**2026-10-16 新语义**：目标工序改由工种映射推导）
//!
//! 与 `transfer.rs` 第 6 步同一套业务口径（工人不该在全部工序里挑，该做什么由他的
//! 工种说了算），但**优先级次序刻意不同**：
//!
//! 1. **显式 `next_process_id`** ⇒ 直接用它（**覆盖链推导**，既有语义逐字不变）；
//! 2. **链推导** ⇒ 缺省时仍先按 outsource 域的 `resolve_receive_next_process`
//!    推导（有链且能推出时工种候选**不参与**）——「有链且能推导」这条老路子的行为
//!    与 2026-10-15 逐字相同（回归 `outsource_receive_derives_next_process_from_chain`）；
//! 3. **链推导不可用**（无链 / 链已软删 / 指针漂移 / 链内歧义 / 链尾）⇒ 退回工种候选
//!    `t_work_type_process` 活跃映射（**不与工序链求交**）：恰好 1 条 ⇒ 自动推导
//!    （**不要求前端传**）；>1 条 ⇒ 有链的返 `20706`（保留「请手填」指引）、无链的
//!    返 `40001`；0 条 ⇒ `20104`「工人 X 的工种未映射任何工序，无法收回」。
//!
//! 与 `transfer.rs` 的次序差异有实据：本端点的链推导口径是 outsource 域的
//! `NEXT_PROCESS_LATERAL_SQL`（`sort_order = 当前 + 1`），在稀疏链上与读侧不等价
//! （分叉登记见 `docs/api/scan.md` §8），故**链能推时优先信链**、推不出才用工种兜底。
//!
//! ## 为什么 stage① 复用 outsource 域的 `receive_from_outsource_inner`
//!
//! `/outsource-queue/move` 的回收臂与本源点的「收回落架」是**同一个业务动作**：
//! 选架口径 / 开口 shipment 的关闭 / `RECEIVED_FROM_OUTSOURCE` 事件字面量必须
//! 逐字一致，各写一份必然漂移（且漂移的表现是静默数据分叉）。故内核抽到
//! `outsource::service::move_svc::receive_from_outsource_inner`（`pub(crate)`），
//! 本处只负责「解析下一道 + 两段式编排」。
//!
//! ## 为什么 stage② 复用 `pick_up` 而不是自己翻状态
//!
//! 「交到工人手上」的全部判据（worker active / 工种 / 容量复检 / OCC）与事件写入
//! 已在 `pick_up` 里收口；再写一份会让「取件」这条路径出现第二套实现。本处传
//! `shelf_id: None`（stage① 刚落位，无需也不应引入货架 scope 校验）、
//! `quantity: None`（整批）。
//!
//! ## 零写入保证
//!
//! 判据 1–7 全部**先于任何写**：容量预检排在 stage① 之前，失败时批次保持
//! `OUTSOURCE + OUTSOURCE_COMPANY`、shipment 仍 `OUTSOURCING`、零 part 事件。
//! stage① 与 stage② 同一事务，stage② 失败（容量预检后极罕见）整体回滚
//! ⇒「要么全成、要么全无」。
//!
//! ⚠️ 写入侧**没有**「目标工序 ∈ 工种」的第二道闸门（stage① / `pick_up` 都不校验
//! 工序归属），判据 6 是本端点唯一的工种资格闸门 —— 与 `take-over` / `queue/move`
//! 同款纪律（写侧集中守一次，不让读侧与写侧各判一套）。
//!
//! ## step 指针：`Lenient`（2026-10-16，review 第 1 轮 BLOCKER-1）
//!
//! stage① 传 [`StepResolve::Lenient`]：**有链但链内定位不到目标工序 ⇒ step 落
//! `NULL`，不再 `20702`**。这不是宽容而是本端点新语义的**必然推论** —— 第 5 步的
//! 工种候选集刻意不与工序链求交，而「part 有链 + 链推不出（指针漂移 / 链内歧义 /
//! 链尾 / 稀疏链）」正是候选集唯一会被激活的形态之一；该形态下自动推导出来的工序
//! **必然可能不在链内**（决策 2 的求交禁令的直接后果）。若 stage① 沿用
//! `/outsource-queue/move` 回收臂的 `optional_step_id`，就会出现「新代码制造输入、
//! 新代码原样打回」的自相矛盾。
//!
//! ⚠️ `/outsource-queue/move` 的回收臂传 [`StepResolve::Strict`]，那条端点的行为
//! **一行未变**；`shared::batch::guards::optional_step_id` 的全局语义也**没动**。
//! 落 `NULL` 不丢数据：外协批次按「出池清工序」不变式该列恒 `NULL`，而
//! `mark_batch_with_status_and_meta` 的 `None` → `clear_process_step_id = true`
//! 对已是 `NULL` 的列是 no-op（详见 `StepResolve` 的 doc）。

use sqlx::PgConnection;

use crate::auth::rbac::{CurrentUser, Role};
use crate::infra::snowflake::SnowflakeIdGenerator;
// 2026-10-15：回收臂内核与「下一道」推导自 outsource 域复用（`pub(crate)`）。
// `prod::scan` 是转发型域（整域不适用域隔离护栏），跨域 import 是既定 pattern。
use crate::modules::outsource::service::move_svc::{
    StepResolve, receive_from_outsource_inner, resolve_receive_next_process,
};
use crate::modules::part::repo::PartRepoTrait;
use crate::modules::prod::queue::service::QueueService;
use crate::modules::prod::scan::dto::{OutsourceReceiveRequest, PickUpRequest};
use crate::modules::prod::scan::vo::OutsourceReceiveOut;
use crate::modules::prod::worker::repo::WorkerRepo;
use crate::shared::batch::guards::{optional_process_chain, validate_batch_version};
use crate::shared::error::{AppError, code};

use super::ScanService;

impl ScanService {
    /// `POST /api/v2/prod/scan/outsource-receive`：外协确认收回 + 立刻交到扫码工人。
    ///
    /// 角色守卫在 handler 与 service 双层（Manager + ShelfAccount，与 take-over 对齐）；
    /// 事务边界在 handler。本函数只收 `&mut PgConnection`（stage① 与 stage② 共用
    /// 同一连接 / 事务，`pick_up` 接受 `&mut PgConnection` 作为 `R: PartRepoTrait`）。
    /// 判据顺序见本文件模块 doc 的表。
    pub async fn outsource_receive(
        conn: &mut PgConnection,
        snowflake: &SnowflakeIdGenerator,
        req: OutsourceReceiveRequest,
        current: &CurrentUser,
    ) -> Result<OutsourceReceiveOut, AppError> {
        // ── 0. 角色守卫（service 入口第一行，与 take-over / pickup 对齐；handler 侧已有同名守卫）──
        current.require_any_role(&[Role::Manager, Role::ShelfAccount])?;
        // ── 1. 工牌 → worker（与 worker-scan / take-over 逐条同形）──────────────
        let worker = WorkerRepo::get_by_badge_code(&mut *conn, &req.badge_code, false)
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
        // ── 2. 批次 + part ──────────────────────────────────────────────────────
        let batch = (&mut *conn)
            .find_batch_by_id(req.batch_id)
            .await?
            .ok_or_else(|| {
                AppError::biz(
                    code::BIZ_PART_BATCH_NOT_FOUND,
                    format!("batch {} 不存在或已软删", req.batch_id),
                )
            })?;
        let part = (&mut *conn)
            .get_part_inspected(batch.part_id)
            .await?
            .ok_or_else(|| {
                AppError::biz(
                    code::BIZ_PART_NOT_FOUND,
                    format!("part {} 不存在或已软删", batch.part_id),
                )
            })?;
        // ── 3. OCC（客户端传值，禁止「读-再-比」的隐式 OCC）─────────────────────
        validate_batch_version(batch.id, req.version, batch.version)?;
        // ── 4. 状态 / 位置 / 在途公司（收回前 holder）──────────────────────────
        if batch.status != "OUTSOURCE" {
            return Err(AppError::biz(
                code::BIZ_INVALID_TRANSITION,
                format!(
                    "batch {} 当前 status={}，不允许外协收回（要求 OUTSOURCE）",
                    batch.id, batch.status
                ),
            ));
        }
        if batch.location.as_deref() != Some("OUTSOURCE_COMPANY") {
            return Err(AppError::biz(
                code::BIZ_BATCH_LOCATION_MISMATCH,
                format!(
                    "batch {} 当前 location={:?}，外协收回要求 'OUTSOURCE_COMPANY'",
                    batch.id, batch.location
                ),
            ));
        }
        // 在途公司 = 批次行上的持有人（任何写路径都会把它写成公司 id；
        // NULL 是数据异常，出参 `company_id` 与收回语义都以它为前提，按
        // 「位置对不上」同类拒 `20122`）。
        let company_id = batch.current_holder_id.ok_or_else(|| {
            AppError::biz(
                code::BIZ_BATCH_LOCATION_MISMATCH,
                format!(
                    "batch {} location='OUTSOURCE_COMPANY' 但没有持有人（公司），无法收回（数据异常）",
                    batch.id
                ),
            )
        })?;
        // ── 5. 目标工序解析（三态：显式 / 链推导 / 工种单候选自动推导）────────
        // 工种候选**先查出来**（2026-10-16）：分支 3 的自动推导与判据 6 的工种闸门
        // 共用同一份，与 `transfer` 逐字同源。
        let process_ids = crate::modules::prod::work_type::repo::WorkTypeRepo::list_process_ids(
            &mut *conn,
            work_type_id,
        )
        .await?;
        // 显式值**覆盖链推导**（与 `resolve_receive_next_process` 的 `given` 分支
        // 同义，语义逐字一致）；缺省时按「链推导 → 工种候选」两级回落。
        let next_process = match req.next_process_id.as_deref() {
            Some(raw) => raw
                .parse::<i64>()
                .map_err(|_| AppError::validation("next_process_id 非法"))?,
            None => {
                // 分支 1：链推导**逐字保留既有路径**（含 `20706` 的两段文案）——
                // 有链且能推导出下一道时，工种候选不参与。
                let chain_err =
                    if optional_process_chain(&mut *conn, batch.part_id).await?.is_some() {
                        match resolve_receive_next_process(&mut *conn, batch.id, None).await {
                            Ok(pid) => Some(Ok(pid)),
                            Err(err) => Some(Err(err)),
                        }
                    } else {
                        None
                    };
                match chain_err {
                    Some(Ok(pid)) => pid,
                    // 分支 2/3：链推不出（无链 / 指针漂移 / 链内歧义 / 链尾）⇒ 退回
                    // 工种候选（**不与工序链求交** —— 这种形态下默认工序链已不适用）。
                    other => match process_ids.as_slice() {
                        // 恰好一道 ⇒ 自动推导，**不要求前端传**
                        [only] => *only,
                        // 一条都没有 ⇒ 该工种不可作业
                        [] => {
                            return Err(AppError::biz(
                                code::BIZ_INVALID_VALUE,
                                format!(
                                    "worker {} 的工种未映射任何工序，无法收回",
                                    worker.id
                                ),
                            ));
                        }
                        many => {
                            return Err(match other {
                                // 有链但推不出 + 多候选 ⇒ 保留链推导自己的 `20706`
                                // 文案（「请手填」），那正是前端该看到的指引。
                                Some(Err(err)) => err,
                                // 无链 + 多候选 ⇒ `40001`
                                _ => AppError::validation(format!(
                                    "worker {} 的工种有 {} 道工序，无法从工序链推导收回后的工序，\
                                     必须在 next_process_id 里显式指定",
                                    worker.id,
                                    many.len()
                                )),
                            });
                        }
                    },
                }
            }
        };
        // ── 6. 目标工序 ∈ 扫码工种的活跃工序映射（本端点唯一的工种资格闸门）──────
        if !process_ids.contains(&next_process) {
            return Err(AppError::biz(
                code::BIZ_INVALID_VALUE,
                format!(
                    "worker {} 的工种不含工序 {next_process}（外协收回目标工序）",
                    worker.id
                ),
            ));
        }
        // ── 7. 容量预检（fail-fast，**先于任何写**：失败时零写入、不留半收回顾虑）──
        let _ =
            QueueService::ensure_worker_has_capacity(&mut *conn, worker.id, work_type_id).await?;
        // ── 8. stage①：收回落自动选架（与 move 回收臂同一内核）─────────────────
        // `note: None` —— 本端点 body 没有 note 字段（事件备注由下游事件本身承担）。
        let _shelf_id = receive_from_outsource_inner(
            &mut *conn,
            snowflake,
            &batch,
            &part,
            next_process,
            // 2026-10-16（review 第 1 轮 BLOCKER-1）：本端点的目标工序可由工种
            // 映射推导、且候选集刻意不与工序链求交 ⇒「有链 + 链外工序」是本端点
            // 自己生产出来的合法输入，必须传 `Lenient`（落 `NULL` step），否则
            // stage① 会被 `optional_step_id` 打回 `20702`。`/outsource-queue/move`
            // 的回收臂传 `Strict`，那条端点行为逐字不变。
            StepResolve::Lenient,
            None,
            current,
        )
        .await?;
        // 读回批次拿**真实新 version**（不手算 +1）→ stage②
        let fresh = (&mut *conn)
            .find_batch_by_id(batch.id)
            .await?
            .ok_or_else(|| {
                AppError::biz(
                    code::BIZ_PART_BATCH_NOT_FOUND,
                    format!("收回落架后读回 batch {} 失败", batch.id),
                )
            })?;
        // stage②：把批次从生产架交到扫码工人手上（同事务）。
        // `shelf_id: None` —— stage① 刚落位、不经客户端，不引入 shelf scope 校验；
        // `quantity: None` —— 收回是整批动作。
        let outcome = ScanService::pick_up(
            &mut *conn,
            snowflake,
            batch.id,
            PickUpRequest {
                version: fresh.version,
                worker_id: worker.id,
                shelf_id: None,
                quantity: None,
                note: None,
            },
            current,
        )
        .await?;
        // 写完再读回最终 version（**不手算 +2**：两次 OCC 写各 +1 是内部
        // 事实，但版本推进必须由库回答；与 `move_batch` 的写后读回同款纪律）。
        let final_row = (&mut *conn)
            .find_batch_by_id(batch.id)
            .await?
            .ok_or_else(|| {
                AppError::biz(
                    code::BIZ_PART_BATCH_NOT_FOUND,
                    format!("收回交人后读回 batch {} 失败", batch.id),
                )
            })?;
        Ok(OutsourceReceiveOut {
            batch_id: batch.id,
            part_id: batch.part_id,
            worker_id: worker.id,
            company_id,
            next_process_id: next_process,
            version: final_row.version,
            // 整批收回：数量恒为批次余量（`pick_up` 的返回值，不另查）。
            picked_quantity: outcome.picked_quantity,
        })
    }
}

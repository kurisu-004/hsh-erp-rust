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
//! | 5 | 下一道解析：显式 `next_process_id` 覆盖链推导；缺省时**无链 ⇒ `40001`**，有链 ⇒ `resolve_receive_next_process` 推导（推不出按其两段 `20706` 文案） | `40001` / `20706` |
//! | 6 | 下一道 ∈ 扫码工种的活跃工序映射（`WorkTypeRepo::list_process_ids`） | `20104` |
//! | 7 | 容量预检 `QueueService::ensure_worker_has_capacity` | `20904` / `20204` |
//! | 8 | stage①：`receive_from_outsource_inner`（落 `IN_PROCESS + PRODUCTION_SHELF` + 关 shipment + `RECEIVED_FROM_OUTSOURCE`） | `20508` |
//! | 9 | stage②：`ScanService::pick_up`（同事务把批次交到工人手上 + `PICKED_UP`） | 复检容量 `20904` / `20204`、OCC `40901` |
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
//! ⚠️ 写入侧**没有**「下一道 ∈ 工种」的第二道闸门（stage① / `pick_up` 都不校验
//! 工序归属），判据 6 是本端点唯一的工种资格闸门 —— 与 `take-over` / `queue/move`
//! 同款纪律（写侧集中守一次，不让读侧与写侧各判一套）。

use sqlx::PgConnection;

use crate::auth::rbac::{CurrentUser, Role};
use crate::infra::snowflake::SnowflakeIdGenerator;
// 2026-10-15：回收臂内核与「下一道」推导自 outsource 域复用（`pub(crate)`）。
// `prod::scan` 是转发型域（整域不适用域隔离护栏），跨域 import 是既定 pattern。
use crate::modules::outsource::service::move_svc::{
    receive_from_outsource_inner, resolve_receive_next_process,
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
    /// 角色 / 事务边界都在 handler（Manager + ShelfAccount，与 `worker-scan` /
    /// `take-over` 同款）；本函数只收 `&mut PgConnection`（stage① 与 stage② 共用
    /// 同一连接 / 事务，`pick_up` 接受 `&mut PgConnection` 作为 `R: PartRepoTrait`）。
    /// 判据顺序见本文件模块 doc 的表。
    pub async fn outsource_receive(
        conn: &mut PgConnection,
        snowflake: &SnowflakeIdGenerator,
        req: OutsourceReceiveRequest,
        current: &CurrentUser,
    ) -> Result<OutsourceReceiveOut, AppError> {
        // ── 1. 角色守卫（service 入口第一行，与 take-over / worker-scan 对齐）────
        current.require_any_role(&[Role::Manager, Role::ShelfAccount])?;
        // ── 2. 工牌 → worker（与 worker-scan / take-over 逐条同形）──────────────
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
        // ── 3. 批次 + part ──────────────────────────────────────────────────────
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
        // ── 4. OCC（客户端传值，禁止「读-再-比」的隐式 OCC）─────────────────────
        validate_batch_version(batch.id, req.version, batch.version)?;
        // ── 5. 状态 / 位置 / 在途公司（收回前 holder）──────────────────────────
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
        // ── 6. 下一道解析 ───────────────────────────────────────────────────────
        // 显式值**覆盖链推导**（与 `resolve_receive_next_process` 的 `given` 分支
        // 同义，语义逐字一致）；缺省时才按链走 outsource 域的同一份推导。
        let next_process = match req.next_process_id.as_deref() {
            Some(raw) => raw
                .parse::<i64>()
                .map_err(|_| AppError::validation("next_process_id 非法"))?,
            None => {
                // 无链零件手写工单是常态：收回必须知道落哪道工序，而链推导对无链
                // 无解 ⇒ `40001` 明确要求手填（有链时才复用 `20706` 的两段文案）。
                if optional_process_chain(&mut *conn, batch.part_id)
                    .await?
                    .is_none()
                {
                    return Err(AppError::validation("无链零件收回必须手填 next_process_id"));
                }
                resolve_receive_next_process(&mut *conn, batch.id, None).await?
            }
        };
        // ── 7. 下一道 ∈ 扫码工种的活跃工序映射（本端点唯一的工种资格闸门）──────
        let process_ids = crate::modules::prod::work_type::repo::WorkTypeRepo::list_process_ids(
            &mut *conn,
            work_type_id,
        )
        .await?;
        if !process_ids.contains(&next_process) {
            return Err(AppError::biz(
                code::BIZ_INVALID_VALUE,
                format!(
                    "worker {} 的工种不含工序 {next_process}（外协收回目标工序）",
                    worker.id
                ),
            ));
        }
        // ── 8. 容量预检（fail-fast，**先于任何写**：失败时零写入、不留半收回顾虑）──
        let _ =
            QueueService::ensure_worker_has_capacity(&mut *conn, worker.id, work_type_id).await?;
        // ── 9. stage①：收回落自动选架（与 move 回收臂同一内核）─────────────────
        // `note: None` —— 本端点 body 没有 note 字段（事件备注由下游事件本身承担）。
        let _shelf_id = receive_from_outsource_inner(
            &mut *conn,
            snowflake,
            &batch,
            &part,
            next_process,
            None,
            current,
        )
        .await?;
        // ── 10. 读回批次拿**真实新 version**（不手算 +1）→ stage② ────────────────
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
        // ── 11. 写完再读回最终 version（**不手算 +2**：两次 OCC 写各 +1 是内部
        // 事实，但版本推进必须由库回答；与 `move_batch` 的写后读回同款纪律）──────
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

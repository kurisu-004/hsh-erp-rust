//! assembly 域生命周期 service（2026-09-22 Group D-3 拆自原 service.rs）
//!
//! 承载两类非 CRUD 业务：
//! 1. **状态机推进** —— `start_assembly`（PENDING → IN_PROCESS 守卫）。
//! 2. **生产链路收口** —— `force_complete`（MANAGER 单角色强推装配件 + 全部子件）。
//!
//! 状态机翻转（cancel / soft_delete）已归到 `crud.rs`（属于 CRUD 端点）。
//!
//! 2026-10-13 PR1b：**文件上传 / 文件列出两类业务整体迁出本文件** —— 上传并入
//! `files::part_file` 的 `POST /api/v2/files/objects`（四合一端点），列出端点
//! `GET /assemblies/{id}/files` 直接删除（等价于
//! `GET /files/objects?owner_kind=ASSEMBLY&owner_id=…`）。
//! ⇒ 本域自此**零**文件写入路径；`POST /assemblies`（multipart 建单）收的 PDF 仍只
//! 喂 `lopdf` 数页数，不入库不上 COS。
//!
//! ## 事务边界（2026-09-22 重构对齐 iam 范本）
//! 事务移交 handler：service 仅业务逻辑，所有跨 repo 操作经 `repo: R`
//! （by-value；`R: AssemblyRepoTrait`）参数传入——handler/service 借 `&mut *tx` /
//! `&mut *conn` 喂给 trait（trait 已直接 `impl for &mut PgConnection`）。
//!
//! ## 跨域调用（2026-09-22 D-3 决策）
//! 所有跨域 SQL 通过 `AssemblyRepoTrait` 跨域 helper 收口。
//!
//! 2026-10-13 PR1b：原先经 `AssemblyRepoTrait` 的 3 个 part_file 跨域 helper
//! 删到只剩 `list_part_files_by_owner`（`GET /assemblies/{id}` 详情仍要用，
//! 见 `service/crud.rs`）；另两个（CAS 去重查 / INSERT）随文件上传端点一起下线。
//! 本文件随之不再 import `CosClient` / `NewPartFile` / `hash_bytes`。

use std::collections::HashMap;

use crate::auth::rbac::{CurrentUser, Role};
use crate::infra::snowflake::SnowflakeIdGenerator;
use crate::modules::assembly::model::TAssembly;
use crate::modules::assembly::repo::AssemblyRepoTrait;
use crate::modules::assembly::statemachine::AssemblyStatus;
use crate::modules::assembly::vo::AssemblyOut;
use crate::shared::error::{AppError, code};

use super::AssemblyService;

// =============================================================================
// 兼容旧测试的 ZST 静态 wrapper 实现（2026-09-22 D-3 决策）
// =============================================================================
//
// 既存集成测试以 `AssemblyService::start_assembly(&mut tx, ...)` 形式直调 service。
// trait 注入式新签名要求 caller 写 `AssemblyService.xxx(&mut *tx, ...)`，与旧测试
// 不兼容。本任务"不修改测试代码"，故以下 `_impl` 函数保留旧签名，内部一行委托。

pub(crate) async fn start_assembly_dispatch(
    conn: &mut sqlx::PgConnection,
    assembly_id: i64,
    current: &CurrentUser,
) -> Result<AssemblyOut, AppError> {
    AssemblyService
        .start_assembly_inner(conn, assembly_id, current)
        .await
}

impl AssemblyService {
    // =======================================================================
    // 状态机推进：start_assembly
    // =======================================================================

    /// `POST /assemblies/{id}/start`：PENDING → IN_PROCESS（状态机守卫）。
    ///
    /// 2026-09-14 Phase 3（deferred #4）：独立端点暴露装配体进入加工态。
    /// 权限：Manager / Clerk；状态机 `PENDING → IN_PROCESS` 校验在 service 层。
    pub async fn start_assembly_inner<R: AssemblyRepoTrait>(
        &self,
        mut repo: R,
        assembly_id: i64,
        current: &CurrentUser,
    ) -> Result<AssemblyOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk])?;
        let asm = repo
            .get_by_id(assembly_id, false)
            .await
            .map_err(AppError::from)?
            .ok_or_else(|| AppError::biz(code::BIZ_ASSEMBLY_NOT_FOUND, "assembly 不存在"))?;
        let from = AssemblyStatus::from_str(&asm.status).ok_or_else(|| {
            AppError::biz(
                code::BIZ_INVALID_VALUE,
                format!("未知 assembly status: {}", asm.status),
            )
        })?;
        if !from.can_transition_to(AssemblyStatus::IN_PROCESS) {
            return Err(AppError::biz(
                code::BIZ_INVALID_TRANSITION,
                format!("start 状态机禁止: {} → IN_PROCESS", from.as_str()),
            ));
        }
        let affected = repo
            .update_status_if_not_terminal(
                assembly_id,
                asm.version,
                AssemblyStatus::IN_PROCESS.as_str(),
                current.id,
            )
            .await
            .map_err(AppError::from)?;
        if affected == 0 {
            return Err(AppError::biz(
                code::VERSION_CONFLICT,
                "version 不匹配或已终态",
            ));
        }
        let fresh = repo
            .get_by_id(assembly_id, false)
            .await
            .map_err(AppError::from)?
            .ok_or_else(|| AppError::biz(code::BIZ_ASSEMBLY_NOT_FOUND, "start 后查不到"))?;
        Ok(render_assembly_out(fresh))
    }

    // =======================================================================
    // 强制完成：force_complete（2026-10-11 新增）
    // =======================================================================

    /// `POST /prod/assemblies/{id}/force-complete`：MANAGER 单角色强推装配件 +
    /// 全部子件 + 全部非 CANCELLED 批次为 `COMPLETED`（绕状态机）。
    ///
    /// **守卫顺序**（逐条 early-fail，顺序固定）：
    /// 1. MANAGER 单角色（不下放 Clerk —— 逃生通道）；
    /// 2. 装配件存在且 `deleted_at IS NULL`；
    /// 3. `status` 可被 `AssemblyStatus::from_str` 解析；
    /// 4. 已 COMPLETED → `BIZ_ASSEMBLY_ALREADY_COMPLETED`（409，幂等拒）；
    /// 5. 已 CANCELLED → `BIZ_ASSEMBLY_ALREADY_CANCELLED`（409，终态守护）。
    ///
    /// **三步写（同一事务内，顺序固定）**：
    /// 1. 全部子件的非 CANCELLED 批次 → `COMPLETED`（走
    ///    `shared::batch::status::force_complete_all_batches_for_assembly`，该函数
    ///    **不派生**）；
    /// 2. **全部**子件显式写终态（`AssemblyRepo::force_complete_children`），逐个
    ///    归档清序列号 + 记 `FORCE_COMPLETED` 事件；
    /// 3. 装配件显式写终态 + 清序列号（`force_complete_status` +
    ///    `clear_serial_no_if_terminal`）。
    ///
    /// 第 2/3 步刻意不派生：批次 UPDATE 命中 0 行时派生循环一次都不跑（见
    /// `shared::batch::status` 里那条 TODO），装配件级的语义是「整体判为已交」，
    /// 终态必须显式写，否则端点返回 200 却没有把这一行移出大屏的未交 / 部分已交切片。
    ///
    /// **第 3 步的 0 行不是成功**：`force_complete_status` 带
    /// `status NOT IN ('COMPLETED','CANCELLED')` 守卫，守卫 1~5 与第 3 步之间存在
    /// 并发窗口（并发 `cancel` 抢先）。命中 0 行时本方法按重读值上抛对应错误码，
    /// 让整笔事务回滚 —— 绝不返回「200 但父件没进 COMPLETED」。
    ///
    /// **事务边界移交 handler**（与 assembly 域其余 handler 同形）：本方法收
    /// `&mut PgConnection`，`begin` / `commit` 由 handler 负责。
    ///
    /// 返回 `(装配件重读结果, 被改动的子件 id 列表)`：后者供 handler 在 commit 之后
    /// 逐个广播既有的 `PART_FORCE_COMPLETED`。HTTP 响应体仍是 `AssemblyOut`。
    pub async fn force_complete_inner(
        &self,
        conn: &mut sqlx::PgConnection,
        snowflake: &SnowflakeIdGenerator,
        assembly_id: i64,
        req: crate::modules::assembly::dto::ForceCompleteRequest,
        current: &CurrentUser,
    ) -> Result<(AssemblyOut, Vec<i64>), AppError> {
        use crate::modules::assembly::repo::sql::AssemblyRepo;
        use crate::modules::part::repo::sql::PartRepo;
        use crate::shared::batch::status::{
            force_complete_all_batches_for_assembly, release_part_serial_no,
        };
        // 2026-10-12：`t_part_event` 的写侧自 part 域的事件 INSERT 上提到
        // `shared::part_event::insert`（该表由 7 个模块写、无域归属）。
        use crate::shared::part_event::{NewPartEvent, insert};

        // 1. MANAGER 单角色守卫（不下放 Clerk）。
        current.require_role(Role::Manager)?;
        // 2. 装配件存在性（include_deleted=false ⇒ 软删行一并视为不存在）。
        let asm = AssemblyRepo::get_by_id(&mut *conn, assembly_id, false)
            .await?
            .ok_or_else(|| {
                AppError::biz(
                    code::BIZ_ASSEMBLY_NOT_FOUND,
                    format!("assembly {assembly_id} 不存在"),
                )
            })?;
        // 3. status 解析（脏数据守卫：状态机不认识的值不做任何写入）。
        let from = AssemblyStatus::from_str(&asm.status).ok_or_else(|| {
            AppError::biz(
                code::BIZ_INVALID_VALUE,
                format!("未知 assembly status: {}", asm.status),
            )
        })?;
        // 4. 幂等拒绝：已 COMPLETED → 避免重复强推副作用。
        if from == AssemblyStatus::COMPLETED {
            return Err(AppError::biz(
                code::BIZ_ASSEMBLY_ALREADY_COMPLETED,
                "装配件已 COMPLETED",
            ));
        }
        // 5. 终态守护：已 CANCELLED → 拒（与 COMPLETED 语义对称）。
        if from == AssemblyStatus::CANCELLED {
            return Err(AppError::biz(
                code::BIZ_ASSEMBLY_ALREADY_CANCELLED,
                "装配件已 CANCELLED",
            ));
        }

        // 步骤①：全部子件的非 CANCELLED 批次强推 COMPLETED（不派生）。
        let _affected_batches =
            force_complete_all_batches_for_assembly(conn, assembly_id, current.id).await?;

        // 步骤②：全部子件显式写终态（不是步骤①的受影响集合 —— 批次 0 条的子件
        // 同样要判为已交），再逐个归档清序列号 + 记事件。
        let children = PartRepo::list_by_assembly_id(&mut *conn, assembly_id, false).await?;
        let child_ids: Vec<i64> = children.iter().map(|c| c.id).collect();
        let changed =
            AssemblyRepo::force_complete_children(&mut *conn, &child_ids, current.id).await?;

        // 审计保真度：事件要能事后回答「这个子件是从哪一态被强推的」，故 `from_status`
        // 与 `drawing_code` 都从上面已读出的 `children` 取（零件级同款 `FORCE_COMPLETED`
        // 事件也是这么填的）。`children` 在本函数内不再变更，借用其 `&str` 填事件安全。
        // 注意 `changed` 只含**真的被改**的子件（SQL 排除了已终态的），映射按 id 查；
        // 查不到只可能是 `children` 与 `changed` 取自不同快照，属编程错误，按 None 填
        // 不 panic（事件表不是闸门，不能因审计字段缺失而让整笔强制完成失败）。
        let pre_status: HashMap<i64, (&str, &str)> = children
            .iter()
            .map(|c| (c.id, (c.status.as_str(), c.drawing_no.as_str())))
            .collect();

        let prefixed_note = format!("[FORCE_ASSEMBLY] {}", req.note.unwrap_or_default());
        for part_id in &changed {
            // 两个事件必须各拿一个真实雪花 id：`t_part_event.id` 是 pkey，复用同一个
            // id 会直接 23505 让整个事务回滚。
            // - `serial_event_id` 交给 release 写 `SERIAL_RELEASED` 归档行；
            // - `force_event_id` 给本次强推的 `FORCE_COMPLETED` 行。
            let serial_event_id = snowflake.next_id();
            let force_event_id = snowflake.next_id();
            let (from_status, drawing_code) = match pre_status.get(part_id).copied() {
                Some((s, d)) => (Some(s), Some(d)),
                None => (None, None),
            };
            // 归档 + 清序列号（复用派生链的同一实现，见 status.rs 的 doc）。
            release_part_serial_no(
                conn,
                *part_id,
                "COMPLETED",
                current.id,
                Some(serial_event_id),
            )
            .await?;
            insert(
                &mut *conn,
                NewPartEvent {
                    id: force_event_id,
                    part_id: *part_id,
                    event_type: "FORCE_COMPLETED",
                    from_status,
                    to_status: Some("COMPLETED"),
                    batch_id: None,
                    quantity: None,
                    drawing_code,
                    badge_code: None,
                    // 2026-10-12：管理员强制完成，无作业工人、无批次工序上下文。
                    worker_id: None,
                    process_id: None,
                    note: Some(&prefixed_note),
                    created_by: Some(current.id),
                },
            )
            .await?;
        }

        // 步骤③：装配件自身终态 + 清序列号。
        //
        // ⚠️ 2026-10-11：`force_complete_status` 的 `status NOT IN ('COMPLETED','CANCELLED')`
        // 守卫，在「步骤 2 的守卫读完、到这里之前」被并发 `cancel` 抢先时会命中 0 行。
        // **必须接住 rows_affected** —— 丢弃它会让下面的 `fresh` 重读拿到 CANCELLED 并
        // 当作成功返回 200，而此时子件已被推成 COMPLETED、序列号已释放，端点承诺的
        // 「整体判为已交」只落空了一半（父件 CANCELLED + 子件全 COMPLETED）。归因口径
        // 同 `AssemblyRepo::update_status_if_not_terminal`（0 行 = 版本不匹配 / 终态 /
        // 已软删），按重读值分派错误码。
        let asm_rows =
            AssemblyRepo::force_complete_status(&mut *conn, assembly_id, current.id).await?;
        if asm_rows == 0 {
            // 事务内上抛：handler 侧 `tx` 未 commit 即随 `?` drop、sqlx 回滚，上面步骤
            // ①②/③ 写下的子件 COMPLETED 与序列号释放一并撤销 ⇒ 库值与「本次调用没有
            // 成功」自洽，不会留下「子件已交、父件已取消」的半截状态。handler 的 WS
            // 广播在 `tx.commit()` **之后**，故这条路径不发任何广播。
            let observed = AssemblyRepo::get_by_id(&mut *conn, assembly_id, false).await?;
            tracing::warn!(
                assembly_id,
                "force-complete 步骤③命中 0 行：装配件在守卫读之后被并发改写，整笔已回滚"
            );
            let err = match observed.as_ref().map(|a| a.status.as_str()) {
                None => AppError::biz(
                    code::BIZ_ASSEMBLY_NOT_FOUND,
                    format!("assembly {assembly_id} 在本次写入前已被软删，强制完成整笔回滚"),
                ),
                Some(s) => match AssemblyStatus::from_str(s) {
                    Some(AssemblyStatus::CANCELLED) => AppError::biz(
                        code::BIZ_ASSEMBLY_ALREADY_CANCELLED,
                        format!(
                            "assembly {assembly_id} 在本次写入前已被并发取消，强制完成整笔回滚"
                        ),
                    ),
                    Some(AssemblyStatus::COMPLETED) => AppError::biz(
                        code::BIZ_ASSEMBLY_ALREADY_COMPLETED,
                        format!(
                            "assembly {assembly_id} 在本次写入前已被并发置为 COMPLETED，\
                             强制完成整笔回滚"
                        ),
                    ),
                    // 守卫排除了终态，落到这里只能是脏数据或守卫与状态机不同步。
                    _ => AppError::biz(
                        code::BIZ_INVALID_VALUE,
                        format!("assembly {assembly_id} 强制完成未生效，重读状态为 {s}"),
                    ),
                },
            };
            return Err(err);
        }
        AssemblyRepo::clear_serial_no_if_terminal(&mut *conn, assembly_id, current.id).await?;

        let fresh = AssemblyRepo::get_by_id(&mut *conn, assembly_id, false)
            .await?
            .ok_or_else(|| {
                AppError::biz(code::BIZ_ASSEMBLY_NOT_FOUND, "force-complete 后查不到")
            })?;
        Ok((render_assembly_out(fresh), changed))
    }
}

// ---------- internal helpers ----------

/// `TAssembly` → `AssemblyOut`（start / cancel / 等端点返回）。
///
/// 与 `crud.rs::render_assembly_out` 同形；2026-09-22 D-3 决策：保留两份（不抽 pub fn）
/// 以避免跨子模块 `pub(crate)` 暴露——本子模块只服务 lifecycle 端点。
fn render_assembly_out(asm: TAssembly) -> AssemblyOut {
    AssemblyOut {
        id: asm.id,
        drawing_no: asm.drawing_no,
        name: asm.name,
        applicant_name: asm.applicant_name,
        customer_id: asm.customer_id,
        request_date: asm.request_date,
        planned_delivery_date: asm.planned_delivery_date,
        is_urgent: asm.is_urgent,
        status: asm.status,
        version: asm.version,
        serial_no: asm.serial_no,
        quantity: asm.quantity,
        unit_price: asm.unit_price,
        total_price: asm.total_price,
        order_no: asm.order_no,
        system_delivery_date: asm.system_delivery_date,
        note: asm.note,
        created_at: asm.created_at,
        updated_at: asm.updated_at,
    }
}

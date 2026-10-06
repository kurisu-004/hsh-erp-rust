//! prod::batch 的「下发」子流：待下发列表 + 批量下发 + 自动下发预览。
//!
//! 2026-09-29 新增 + 2026-09-30 重构：
//! - `dispatch_batch` 改为 bulk-only（接受 `Vec<(batch_id, target_process_id)>`，
//!   单条下发即 `targets.length == 1`）
//! - `bulk_dispatch` service 删除（合并入 `dispatch_batch` 循环）
//! - `auto_dispatch` 改为 `auto_dispatch_preview` 只读查询（不开事务）
//!
//! ## 3 个公共方法
//! - [`BatchService::list_pending`] —— 读待下发批次列表（handler `pool.acquire()`）
//! - [`BatchService::dispatch_batch`] —— bulk-only 下发（事务内）：fetch batch → 校验 status ∈ {PENDING, PROGRAMMING} → 解析货架 → UPDATE OCC → 写事件
//! - [`BatchService::auto_dispatch_preview`] —— 只读查询，返回每个 batch 的首道工序 + 首货架
//!
//! ## 2026-10-06：源状态白名单纳入已废弃的 `PROGRAMMING`
//! 三个方法的状态闸门统一为 `IN ('PENDING', 'PROGRAMMING')`。`PROGRAMMING` 的入口
//! 端点已下线（见 `part::statemachine`），但存量行需要在「待下发」页被消化，故与
//! `PENDING` 同链路：同样可列出、同样可批量下发、同样可自动下发。白名单分布在
//! `BatchRepo::list_pending_batches` / `count_pending_batches` /
//! `preview_auto_dispatch` / `update_batch_dispatched` 与本文件 `dispatch_single`，
//! 五处必须同步。
//!
//! ## 事务 + 角色守卫
//! 角色守卫下沉到 service（与 worker_pool `pool_by_process` 等同形）：handler
//! 仅做权限分发；service 入口第一行 `current.require_any_role(...)`。
//!
//! 事务边界在 handler（与 worker_pool 范本一致）：handler `pool.begin()` →
//! service 收 `&mut PgConnection` → handler `commit()`。
//!
//! ## 事务内并发冲突（OCC）
//! dispatch_batch 入口 `shared::batch::get_batch_by_id` 后用 fetched `batch.version` 作
//! `expected_version`；UPDATE 0 行 → `VERSION_CONFLICT 40901`。
//!
//! ## 2026-10-02 `t_shelf_process` SQL 收口
//! 解析货架改调同域 `prod::shelf_process::repo::ShelfProcessRepo::find_first_shelf_for_process`
//! （原为 `BatchRepo::find_first_shelf_for_process` 内联 SQL），两处 inline 保留见
//! `repo.rs::preview_auto_dispatch` 注释。

use sqlx::PgConnection;

use crate::auth::rbac::{CurrentUser, Role};
use crate::infra::snowflake::SnowflakeIdGenerator;
use crate::modules::part::model::NewPartEvent;
use crate::modules::part::repo::PartRepo;
use crate::modules::prod::batch::repo::BatchRepo;
use crate::modules::prod::batch::vo::{
    AutoDispatchItem, AutoDispatchResult, DispatchResult, DispatchSuccessItem, PendingBatchItem,
    PendingBatchListOut,
};
use crate::modules::prod::shelf_process::repo::ShelfProcessRepo;
use crate::shared::error::{AppError, code};

use super::BatchService;
use crate::modules::prod::batch::repo::PendingBatchRow;

impl BatchService {
    pub fn new() -> Self {
        Self
    }

    /// `GET /api/v2/prod/batches/pending` 业务逻辑。
    ///
    /// 角色守卫：Manager + Clerk + Inspector。
    /// 读路径（`pool.acquire()`）：handler 不开事务，service 借 `&mut PgConnection`
    /// 调 `list_pending_batches` + `count_pending_batches` 两条 SQL。
    pub async fn list_pending(
        conn: &mut PgConnection,
        current: &CurrentUser,
        limit: i64,
        offset: i64,
    ) -> Result<PendingBatchListOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk, Role::Inspector])?;

        // 防御：limit / offset 边界
        let limit = limit.clamp(1, 500);
        let offset = offset.max(0);

        let rows = BatchRepo::list_pending_batches(&mut *conn, limit, offset).await?;
        let total = BatchRepo::count_pending_batches(&mut *conn).await?;
        let items = rows.into_iter().map(row_to_item).collect();
        Ok(PendingBatchListOut {
            items,
            total,
            limit,
            offset,
        })
    }

    /// 单 batch 下发核心逻辑（bulk-only：单条下发即 `targets.length == 1`）。
    ///
    /// 2026-09-30 重构：
    /// - 删除原 `bulk_dispatch` service（业务逻辑完全相同）
    /// - `dispatch_batch` 改为接受 `Vec<(batch_id, target_process_id)>`，
    ///   handler 层做 targets 解构后调用（避免新增 `bulk_dispatch` 转发壳）
    /// - 返回 `DispatchResult { succeeded, failed }`（BulkDispatchResult 形态）
    ///
    /// 事务内流程（每条 target 顺序执行，任一硬失败 → service 抛 AppError，
    /// handler tx Drop 自动回滚全部 succeeded 写入）：
    /// 1. 角色守卫：Manager + Clerk
    /// 2. fetch batch → `None` → `BIZ_BATCH_NOT_FOUND` 抛错
    /// 3. 校验 `batch.status ∈ {'PENDING', 'PROGRAMMING'}` → 否则
    ///    `BIZ_BATCH_INVALID_STATUS` 抛错（2026-10-06：纳入已废弃的 `PROGRAMMING`，
    ///    与待下发列表同一白名单）
    /// 4. `find_first_shelf_for_process(target_process_id)` → `None` → `BIZ_SHELF_PROCESS_NOT_FOUND` 抛错
    ///    （2026-10-04：该方法已带 `t_shelf` 的 `deleted_at` / `is_active` / `zone='PRODUCTION'`
    ///    守卫，故 `None` 含「有映射但货架全不可用」，仍复用 20508 不新造码）
    /// 5. `update_batch_dispatched`（OCC）→ 0 行 → `VERSION_CONFLICT` 抛错
    /// 6. `PartRepo::insert_part_event('PLACED_ON_SHELF')`
    ///
    /// 当前实现：保留原 bulk_dispatch 「任一失败 → 全回滚」语义。
    /// `succeeded` / `failed` 数组实际只在全部成功时填 succeeded；失败路径
    /// 由 service 抛 AppError 把 failed 信息透传给 caller。
    pub async fn dispatch_batch(
        conn: &mut PgConnection,
        batch_ids_targets: Vec<(i64, i64)>,
        note: Option<&str>,
        snowflake: &SnowflakeIdGenerator,
        current: &CurrentUser,
    ) -> Result<DispatchResult, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk])?;
        if batch_ids_targets.is_empty() {
            return Err(AppError::validation(
                "dispatch targets 不能为空（至少 1 条）",
            ));
        }

        let mut succeeded = Vec::with_capacity(batch_ids_targets.len());

        for (batch_id, target_process_id) in batch_ids_targets {
            // 任一失败 → 直接抛 AppError；handler 的 Transaction Drop 自动回滚
            // （service 不持有事务，事务由 caller 持有）
            let item = Self::dispatch_single(
                &mut *conn,
                batch_id,
                target_process_id,
                note,
                snowflake,
                current,
            )
            .await?;
            succeeded.push(item);
        }

        Ok(DispatchResult {
            succeeded,
            failed: vec![],
        })
    }

    /// 单条 dispatch 内部 helper（2026-09-30 新增）。
    ///
    /// `dispatch_batch` 在循环内调：成功 → 返回 `DispatchSuccessItem`；
    /// 失败 → 返回 `AppError`（由 `dispatch_batch` 转 `DispatchFailureItem`）。
    async fn dispatch_single(
        conn: &mut PgConnection,
        batch_id: i64,
        target_process_id: i64,
        note: Option<&str>,
        snowflake: &SnowflakeIdGenerator,
        current: &CurrentUser,
    ) -> Result<DispatchSuccessItem, AppError> {
        // 1. 取 batch
        let batch = crate::shared::batch::get_batch_by_id(&mut *conn, batch_id, false)
            .await?
            .ok_or_else(|| {
                AppError::biz(
                    code::BIZ_BATCH_NOT_FOUND,
                    format!("batch {batch_id} 不存在或已软删"),
                )
            })?;

        // 2. 校验 status（2026-10-06：白名单含已废弃的 PROGRAMMING —— 存量 PROGRAMMING
        // 批次与 PENDING 同链路下发，判定必须与 repo 层 list/count/preview 的闸门、
        // 以及 update_batch_dispatched 的 allowed_from 一致）
        if !matches!(batch.status.as_str(), "PENDING" | "PROGRAMMING") {
            return Err(AppError::biz(
                code::BIZ_BATCH_INVALID_STATUS,
                format!(
                    "batch {} 当前 status='{}'，不允许 dispatch（要求 'PENDING' 或 'PROGRAMMING'）",
                    batch.id, batch.status
                ),
            ));
        }

        // 3. 解析货架
        // 2026-10-02 域拆分：原调 `BatchRepo::find_first_shelf_for_process`（本域手写
        // `t_shelf_process` SQL），现改调 SQL 真源
        // `prod::shelf_process::repo::ShelfProcessRepo::find_first_shelf_for_process`
        // （executor 泛型直接接住 `&mut PgConnection`，无需改事务上下文）。
        //
        // 2026-10-04：货源守卫下沉到该方法的 SQL（`JOIN t_shelf` + `deleted_at IS NULL`
        // + `is_active` + `zone='PRODUCTION'`），故此处拿到的 `shelf_id` 一定是可被
        // 报工台取件页取到的生产架。`None` 有两种成因（完全没配映射 / 配了但货架全
        // 不可用），都收敛到既有的 20508，不新造错误码；文案要写全，否则运营会去查
        // 错方向（以为只是漏配映射，实际是货架被停用 / 改成了品检架）。
        let shelf_id =
            ShelfProcessRepo::find_first_shelf_for_process(&mut *conn, target_process_id)
                .await?
                .ok_or_else(|| {
                    AppError::biz(
                        code::BIZ_SHELF_PROCESS_NOT_FOUND,
                        format!(
                            "process {target_process_id} 无可用货架映射（无 active 映射，\
                             或命中的映射其货架均已软删 / 已停用 / 非 PRODUCTION 区）"
                        ),
                    )
                })?;

        // 4. UPDATE OCC（带当前 version）
        // 2026-09-30：透传 target_process_id 作为 current_process_id —— 池归属
        // 的权威依据，缺了它批次会对所有工序池查询隐身（见 repo 同名函数 doc）
        let rows_affected = BatchRepo::update_batch_dispatched(
            &mut *conn,
            batch.id,
            batch.version,
            shelf_id,
            Some(current.id),
            target_process_id,
        )
        .await?;
        if rows_affected == 0 {
            return Err(AppError::biz(
                code::VERSION_CONFLICT,
                format!(
                    "batch {} 版本冲突或状态不在可下发白名单（version={}，caller 未传 version 由 service 隐式 OCC）",
                    batch.id, batch.version
                ),
            ));
        }

        // 5. 写 part_event（PLACED_ON_SHELF）
        let part_id = batch.part_id;
        let quantity = batch.quantity;
        let part = PartRepo::get_by_id(&mut *conn, part_id, false)
            .await?
            .ok_or_else(|| {
                AppError::biz(
                    code::BIZ_BATCH_NOT_FOUND,
                    format!("batch {batch_id} 关联 part {part_id} 不存在"),
                )
            })?;
        let event_id = snowflake.next_id();
        PartRepo::insert_part_event(
            &mut *conn,
            NewPartEvent {
                id: event_id,
                part_id,
                event_type: "PLACED_ON_SHELF",
                // 2026-10-06：源状态透传实际值（白名单含 PROGRAMMING，写死 'PENDING'
                // 会让历史批次的流转事件记错起点）
                from_status: Some(&batch.status),
                to_status: Some("IN_PROCESS"),
                batch_id: Some(batch.id),
                quantity: Some(quantity),
                drawing_code: Some(&part.drawing_no),
                badge_code: None,
                note,
                created_by: Some(current.id),
            },
        )
        .await?;

        Ok(DispatchSuccessItem {
            batch_id: batch.id,
            // 2026-09-30：dispatch 路径仍不解析 step（诚实置 None），
            // current_process_id 才是入池的权威依据 —— 填真实写入值
            current_process_step_id: None,
            current_process_id: Some(target_process_id),
            target_process_id,
            shelf_id,
            version: batch.version + 1,
        })
    }

    /// `POST /api/v2/prod/batches/auto-dispatch` 业务逻辑（只读查询，2026-09-30 重构）。
    ///
    /// 流程（不开事务）：
    /// 1. 角色守卫：Manager + Clerk
    /// 2. 空 batch_ids → `40001 VALIDATION_ERROR`
    /// 3. 调 `preview_auto_dispatch` 单 SQL 拉待下发白名单
    ///    （`PENDING` / `PROGRAMMING`）内 batch 的 preview 元数据（白名单口径见
    ///    文件头 2026-10-06 段）
    /// 4. 对每个 preview 行计算 skip_reason：
    ///    - process_chain_id None → `NO_PROCESS_CHAIN`
    ///    - first_process_id None → `NO_PROCESS_STEP`
    ///    - first_shelf_id None → `NO_SHELF`
    ///    - 全有 → `None`（可下发）
    /// 5. 对不在 preview 结果里的 batch_id → 兜底查 part_id + `skip_reason='NOT_FOUND'`
    /// 6. 返回 `AutoDispatchResult { items }`
    ///
    /// 不发 WS 广播（只读查询，无业务流转）。
    pub async fn auto_dispatch_preview(
        conn: &mut PgConnection,
        current: &CurrentUser,
        batch_ids: Vec<i64>,
    ) -> Result<AutoDispatchResult, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk])?;
        if batch_ids.is_empty() {
            return Err(AppError::validation("auto-dispatch batch_ids 不能为空"));
        }

        // 单 SQL 拉 preview（已包含 batch_id 在待下发白名单 + 未软删 的过滤）
        let previews = BatchRepo::preview_auto_dispatch(&mut *conn, &batch_ids).await?;
        let preview_ids: std::collections::HashSet<i64> =
            previews.iter().map(|p| p.batch_id).collect();

        let mut items: Vec<AutoDispatchItem> = previews
            .into_iter()
            .map(|p| {
                let skip_reason = if p.process_chain_id.is_none() {
                    Some("NO_PROCESS_CHAIN".to_string())
                } else if p.first_process_id.is_none() {
                    Some("NO_PROCESS_STEP".to_string())
                } else if p.first_shelf_id.is_none() {
                    Some("NO_SHELF".to_string())
                } else {
                    None
                };
                // 2026-09-30 review 第 1 轮：Option<i64> 透传（plan §3.2），
                // 不再 `.unwrap_or(0)`；None → JSON `null` 由 vo.rs `serialize_i64_opt` 兜底。
                AutoDispatchItem {
                    batch_id: p.batch_id,
                    part_id: p.part_id,
                    process_chain_id: p.process_chain_id,
                    first_process_id: p.first_process_id,
                    first_process_code: p.first_process_code.unwrap_or_default(),
                    first_process_name: p.first_process_name.unwrap_or_default(),
                    first_shelf_id: p.first_shelf_id,
                    skip_reason,
                }
            })
            .collect();

        // 兜底：不在 preview 结果里的 batch_id（批次 / 其工单已软删 / 不在待下发白名单 /
        // 不存在）→ 单独补一行 + skip_reason='NOT_FOUND'。该路径无法取到 chain/step/shelf，
        // 全部 Option 置 None（→ JSON `null`），对齐上游 OK 路径的 Option 语义。
        // 落空成因不止软删：preview 的 FROM 是 `t_part_batch JOIN t_part`（INNER），
        // 故**工单**软删与批次软删同效。
        for batch_id in &batch_ids {
            if !preview_ids.contains(batch_id) {
                let part_id_opt =
                    BatchRepo::find_part_id_by_batch_id(&mut *conn, *batch_id).await?;
                let part_id = part_id_opt.unwrap_or(0);
                items.push(AutoDispatchItem {
                    batch_id: *batch_id,
                    part_id,
                    process_chain_id: None,
                    first_process_id: None,
                    first_process_code: String::new(),
                    first_process_name: String::new(),
                    first_shelf_id: None,
                    skip_reason: Some("NOT_FOUND".to_string()),
                });
            }
        }

        // 按 batch_ids 入参顺序排序（保持 caller 视角稳定）
        let order: std::collections::HashMap<i64, usize> = batch_ids
            .iter()
            .enumerate()
            .map(|(i, &id)| (id, i))
            .collect();
        items.sort_by_key(|it| order.get(&it.batch_id).copied().unwrap_or(usize::MAX));

        Ok(AutoDispatchResult { items })
    }
}

impl Default for BatchService {
    fn default() -> Self {
        Self::new()
    }
}

// ===== 行 → VO 转换 =====

/// `PendingBatchRow` → `PendingBatchItem`（service → vo 转换边界）。
///
/// `planned_delivery_date` 走 `String`：DB 列 `t_part.planned_delivery_date` 是
/// `NOT NULL DEFAULT CURRENT_DATE`（migration 001），故 row 字段必非空；service
/// 直接 `.to_string()` 即可，无需 NULL 兜底。PENDING 时 `current_process_step_id`
/// 通常 NULL，但 sqlx 把它按非空 `i64` 处理（与 `process_chain_id` 同形），此处
/// 沿用 row 的 i64 值（`0` 即"未设 step"语义；与 dispatch 写入 NULL 的差异
/// 由前端按 status 区分）。
fn row_to_item(r: PendingBatchRow) -> PendingBatchItem {
    PendingBatchItem {
        batch_id: r.pb_id,
        part_id: r.pb_part_id,
        batch_no: r.pb_batch_no,
        quantity: r.pb_quantity,
        serial_no: r.p_serial_no,
        name: r.p_name,
        drawing_no: r.p_drawing_no,
        planned_delivery_date: r.p_planned_delivery_date.to_string(),
        system_delivery_date: r.p_system_delivery_date,
        customer_name: r.c_name,
        parent_customer_name: r.pc_name,
        applicant_name: r.a_name,
        is_urgent: r.p_is_urgent,
        note: r.p_note,
        version: r.pb_version,
        current_process_step_id: r.pb_current_process_step_id.unwrap_or(0),
        process_chain_id: r.p_process_chain_id.unwrap_or(0),
    }
}

// ============================================================================
// 单元测试（2026-09-29 + 2026-09-30 重构）
// ============================================================================
//
// 覆盖以下场景：
// - list_pending_batches: 正常返回 + 排序 + 排除 IN_PROCESS / 已软删
// - dispatch_batch: bulk 成功路径 + 二次 dispatch 40903 + 不存在 batch_id 40404 +
//   并发冲突 40901 + t_shelf_process 多结果取 LIMIT 1 + Inspector 角色守卫
// - auto_dispatch_preview: 无 chain → NO_PROCESS_CHAIN；无 step → NO_PROCESS_STEP；
//   完整链路 → first_process_id / first_shelf_id 透传；NOT_FOUND 兜底

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::rbac::Role;
    use crate::infra::clock::now_naive;
    use chrono::NaiveDate;
    use hsh_erp_test_support::test_pool;

    // ===== helper：构造一个最小可跑的 user / role / customer / process / part / batch =====

    /// 写一个 active `t_user` 行（username + 全名），并拿到 id。bcrypt 哈希现场生成。
    async fn insert_user_with_role(
        pool: &sqlx::PgPool,
        username: &str,
        plain_password: &str,
        role: &str,
    ) -> i64 {
        use crate::auth::password;
        let hash = password::hash(plain_password).expect("bcrypt");
        let snowflake = crate::infra::snowflake::SnowflakeIdGenerator::new(1_577_836_800_000, 7);
        let user_id = snowflake.next_id();
        let role_id = snowflake.next_id();
        let now = now_naive();
        sqlx::query(
            "INSERT INTO t_user (id, username, password_hash, full_name, is_active, \
             refresh_token_version, version, created_at, updated_at) \
             VALUES ($1, $2, $3, $4, true, 0, 0, $5, $5)",
        )
        .bind(user_id)
        .bind(username.to_lowercase())
        .bind(hash)
        .bind(username)
        .bind(now)
        .execute(pool)
        .await
        .expect("insert t_user");
        sqlx::query(
            "INSERT INTO t_user_role (id, user_id, role, version, created_at, updated_at) \
             VALUES ($1, $2, $3, 0, $4, $4)",
        )
        .bind(role_id)
        .bind(user_id)
        .bind(role)
        .bind(now)
        .execute(pool)
        .await
        .expect("insert t_user_role");
        user_id
    }

    /// 写一个 L2 customer（parent_id 留 NULL 表示它本身就是 L1）。
    async fn insert_customer_l2(pool: &sqlx::PgPool, name: &str) -> i64 {
        let snowflake = crate::infra::snowflake::SnowflakeIdGenerator::new(1_577_836_800_000, 7);
        let id = snowflake.next_id();
        let now = now_naive();
        sqlx::query(
            "INSERT INTO t_customer (id, name, version, created_at, updated_at) \
             VALUES ($1, $2, 0, $3, $3)",
        )
        .bind(id)
        .bind(name)
        .bind(now)
        .execute(pool)
        .await
        .expect("insert t_customer L2");
        id
    }

    /// 写一个 INHOUSE 工序（category='INHOUSE'，确保 apply_filter 通用）。
    async fn insert_process(pool: &sqlx::PgPool, code: &str, name: &str) -> i64 {
        let snowflake = crate::infra::snowflake::SnowflakeIdGenerator::new(1_577_836_800_000, 7);
        let id = snowflake.next_id();
        let now = now_naive();
        sqlx::query(
            "INSERT INTO t_process (id, code, name, category, sort_order, requires_approval, \
             version, created_at, updated_at) \
             VALUES ($1, $2, $3, 'INHOUSE', 0, false, 0, $4, $4)",
        )
        .bind(id)
        .bind(code)
        .bind(name)
        .bind(now)
        .execute(pool)
        .await
        .expect("insert t_process");
        id
    }

    /// 写一个 PRODUCTION 货架。
    async fn insert_shelf(pool: &sqlx::PgPool, code: &str, zone: &str) -> i64 {
        let snowflake = crate::infra::snowflake::SnowflakeIdGenerator::new(1_577_836_800_000, 7);
        let id = snowflake.next_id();
        let now = now_naive();
        sqlx::query(
            "INSERT INTO t_shelf (id, code, name, zone, is_active, display_order, version, \
             created_at, updated_at) \
             VALUES ($1, $2, $2, $3, true, 0, 0, $4, $4)",
        )
        .bind(id)
        .bind(code)
        .bind(zone)
        .bind(now)
        .execute(pool)
        .await
        .expect("insert t_shelf");
        id
    }

    /// 写一条 **active** 的 `t_shelf_process` 映射（`deleted_at` 留默认 NULL）。
    async fn link_shelf_to_process(pool: &sqlx::PgPool, shelf_id: i64, process_id: i64) {
        let snowflake = crate::infra::snowflake::SnowflakeIdGenerator::new(1_577_836_800_000, 7);
        let id = snowflake.next_id();
        let now = now_naive();
        sqlx::query(
            "INSERT INTO t_shelf_process (id, shelf_id, process_id, sort_order, version, \
             created_at, updated_at) \
             VALUES ($1, $2, $3, 0, 0, $4, $4)",
        )
        .bind(id)
        .bind(shelf_id)
        .bind(process_id)
        .bind(now)
        .execute(pool)
        .await
        .expect("insert t_shelf_process");
    }

    /// 写一个 `t_part` 行（status='PENDING'，带 planned_delivery_date 与
    /// system_delivery_date）；返回 part_id。
    #[allow(clippy::too_many_arguments)]
    async fn insert_part(
        pool: &sqlx::PgPool,
        name: &str,
        drawing_no: &str,
        customer_id: i64,
        planned: NaiveDate,
        system: Option<NaiveDate>,
        is_urgent: bool,
        process_chain_id: Option<i64>,
    ) -> i64 {
        let snowflake = crate::infra::snowflake::SnowflakeIdGenerator::new(1_577_836_800_000, 7);
        let id = snowflake.next_id();
        let now = now_naive();
        sqlx::query(
            "INSERT INTO t_part (id, serial_no, name, drawing_no, applicant_name, quantity, \
             request_date, planned_delivery_date, system_delivery_date, customer_id, status, \
             is_urgent, order_no, note, unit_price, total_price, version, created_at, updated_at, \
             process_chain_id) \
             VALUES ($1, NULL, $2, $3, '', 1, $4, $5, $6, $7, 'PENDING', $8, NULL, NULL, 0, 0, 0, \
             $9, $9, $10)",
        )
        .bind(id)
        .bind(name)
        .bind(drawing_no)
        .bind(planned)
        .bind(planned)
        .bind(system)
        .bind(customer_id)
        .bind(is_urgent)
        .bind(now)
        .bind(process_chain_id)
        .execute(pool)
        .await
        .expect("insert t_part");
        id
    }

    /// 写一个初始 `t_part_batch` 行（status='PENDING'，location=NULL）。
    async fn insert_part_batch(pool: &sqlx::PgPool, part_id: i64) -> i64 {
        insert_part_batch_with_status(pool, part_id, "PENDING").await
    }

    /// 同 [`insert_part_batch`]，但显式指定 `t_part_batch.status`。
    ///
    /// 2026-10-06 新增：`PROGRAMMING` 已纳入待下发白名单，需要能造出该状态的批次
    /// 来锁住「列得出 + 下发得了」。默认 helper 保持 PENDING，存量调用点零改动。
    async fn insert_part_batch_with_status(pool: &sqlx::PgPool, part_id: i64, status: &str) -> i64 {
        let snowflake = crate::infra::snowflake::SnowflakeIdGenerator::new(1_577_836_800_000, 7);
        let id = snowflake.next_id();
        let now = now_naive();
        sqlx::query(
            "INSERT INTO t_part_batch (id, part_id, batch_no, quantity, status, location, \
             current_holder_id, current_process_step_id, delivery_note_id, parent_batch_id, \
             version, created_at, updated_at) \
             VALUES ($1, $2, 1, 1, $3, NULL, NULL, NULL, NULL, NULL, 0, $4, $4)",
        )
        .bind(id)
        .bind(part_id)
        .bind(status)
        .bind(now)
        .execute(pool)
        .await
        .expect("insert t_part_batch");
        id
    }

    /// 构造一个最小的 `CurrentUser` 用于 service 直调（绕开 JWT 解析）。
    fn make_current(user_id: i64, role: Role) -> CurrentUser {
        CurrentUser {
            id: user_id,
            username: "test".to_string(),
            roles: vec![role],
            shelf_ids: vec![],
            shelf_wildcard: false,
        }
    }

    // ===== 单元测试 =====

    #[tokio::test]
    async fn list_pending_batches_returns_pending_rows_in_correct_order() {
        let pool = test_pool().await;
        // 准备 customer + 3 个 part + 3 个 PENDING batch（不同 system_delivery_date / is_urgent）
        let customer_id = insert_customer_l2(&pool, "ACME").await;
        // Part A：system_delivery_date=今天 + is_urgent=false
        // Part B：system_delivery_date=昨天 + is_urgent=true
        // Part C：system_delivery_date=NULL + is_urgent=false
        let today = chrono::NaiveDate::from_ymd_opt(2026, 9, 29).unwrap();
        let yesterday = chrono::NaiveDate::from_ymd_opt(2026, 9, 28).unwrap();

        let p_a = insert_part(
            &pool,
            "Part-A",
            "DWG-A",
            customer_id,
            today,
            Some(today),
            false,
            None,
        )
        .await;
        let p_b = insert_part(
            &pool,
            "Part-B",
            "DWG-B",
            customer_id,
            today,
            Some(yesterday),
            true,
            None,
        )
        .await;
        let p_c = insert_part(
            &pool,
            "Part-C",
            "DWG-C",
            customer_id,
            today,
            None,
            false,
            None,
        )
        .await;
        let _b_a = insert_part_batch(&pool, p_a).await;
        let _b_b = insert_part_batch(&pool, p_b).await;
        let _b_c = insert_part_batch(&pool, p_c).await;

        // 跑 list_pending
        let mut conn = pool.acquire().await.unwrap();
        let out = BatchService::list_pending(&mut conn, &make_current(1, Role::Manager), 200, 0)
            .await
            .expect("list_pending OK");

        // 总数 = 3
        assert_eq!(out.total, 3);
        assert_eq!(out.items.len(), 3);

        // 排序：system_delivery_date ASC NULLS LAST → 昨天 (B) 在前；NULL (C) 在最后
        // is_urgent DESC 仅在 system_delivery_date 相同时生效
        // 期望顺序：B(昨天+urgent), A(今天+non-urgent), C(NULL)
        let names: Vec<&str> = out.items.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, vec!["Part-B", "Part-A", "Part-C"]);

        // planned_delivery_date 非空（非 1970-01-01 时回原始值）
        for item in &out.items {
            assert!(
                !item.planned_delivery_date.is_empty(),
                "planned_delivery_date 必须非空"
            );
        }
    }

    #[tokio::test]
    async fn list_pending_batches_excludes_in_process_and_deleted() {
        let pool = test_pool().await;
        let customer_id = insert_customer_l2(&pool, "ACME").await;
        let today = chrono::NaiveDate::from_ymd_opt(2026, 9, 29).unwrap();

        // PENDING 1 条
        let p_pending = insert_part(
            &pool,
            "Pending-Part",
            "DWG-P",
            customer_id,
            today,
            Some(today),
            false,
            None,
        )
        .await;
        let _b_pending = insert_part_batch(&pool, p_pending).await;

        // IN_PROCESS 1 条（应被排除）
        let p_in_process = insert_part(
            &pool,
            "InProcess-Part",
            "DWG-IP",
            customer_id,
            today,
            Some(today),
            false,
            None,
        )
        .await;
        let snowflake = crate::infra::snowflake::SnowflakeIdGenerator::new(1_577_836_800_000, 7);
        let b_ip = snowflake.next_id();
        let now = now_naive();
        // 2026-09-30（review L2）：补 `current_process_id` —— 写入不变式第 1 行要求
        // 「进池（IN_PROCESS + PRODUCTION_SHELF）必写目标 process_id」。此前本
        // helper 只写 status/location 而把该列留 NULL，造出的正是本次要消灭的
        // 「在池但无工序」数据形态。本测试只验 list_pending 的软删过滤、不触发池
        // 查询，所以过去不炸；但复用该 helper 测池时会踩坑。故先建一条真实
        // t_process 行（逻辑 FK 虽无物理约束，仍按 service 层存在性校验的约定走）。
        let pool_process_id = insert_process(&pool, "PROC-INPOOL", "在池工序").await;
        sqlx::query(
            "INSERT INTO t_part_batch (id, part_id, batch_no, quantity, status, location, \
             current_holder_id, current_process_id, current_process_step_id, \
             delivery_note_id, parent_batch_id, \
             version, created_at, updated_at) \
             VALUES ($1, $2, 1, 1, 'IN_PROCESS', 'PRODUCTION_SHELF', NULL, $3, NULL, NULL, NULL, \
             0, $4, $4)",
        )
        .bind(b_ip)
        .bind(p_in_process)
        .bind(pool_process_id)
        .bind(now)
        .execute(&pool)
        .await
        .unwrap();
        // 再写一条 PENDING 然后软删（应被排除）
        let p_deleted = insert_part(
            &pool,
            "Deleted-Part",
            "DWG-D",
            customer_id,
            today,
            Some(today),
            false,
            None,
        )
        .await;
        let b_del = snowflake.next_id();
        sqlx::query(
            "INSERT INTO t_part_batch (id, part_id, batch_no, quantity, status, location, \
             current_holder_id, current_process_step_id, delivery_note_id, parent_batch_id, \
             version, created_at, updated_at, deleted_at) \
             VALUES ($1, $2, 1, 1, 'PENDING', NULL, NULL, NULL, NULL, NULL, 0, $3, $3, $3)",
        )
        .bind(b_del)
        .bind(p_deleted)
        .bind(now)
        .execute(&pool)
        .await
        .unwrap();

        let mut conn = pool.acquire().await.unwrap();
        let out = BatchService::list_pending(&mut conn, &make_current(1, Role::Inspector), 200, 0)
            .await
            .expect("list_pending OK");

        assert_eq!(
            out.total, 1,
            "应只返回 1 条 PENDING（其它 status / 已软删被过滤）"
        );
        assert_eq!(out.items[0].name, "Pending-Part");
    }

    /// 2026-10-06：`PROGRAMMING`（已废弃状态的存量数据）与 `PENDING` 同链路，
    /// 必须一并出现在待下发列表里，且 count 与 list 口径一致。
    #[tokio::test]
    async fn list_pending_batches_includes_programming_rows() {
        let pool = test_pool().await;
        let customer_id = insert_customer_l2(&pool, "ACME").await;
        let today = chrono::NaiveDate::from_ymd_opt(2026, 9, 29).unwrap();

        let p_pending = insert_part(
            &pool,
            "Pending-Part",
            "DWG-P",
            customer_id,
            today,
            Some(today),
            false,
            None,
        )
        .await;
        let b_pending = insert_part_batch_with_status(&pool, p_pending, "PENDING").await;

        let p_prog = insert_part(
            &pool,
            "Programming-Part",
            "DWG-PROG",
            customer_id,
            today,
            Some(today),
            false,
            None,
        )
        .await;
        let b_prog = insert_part_batch_with_status(&pool, p_prog, "PROGRAMMING").await;

        // INSPECTION 应继续被排除（闸门是白名单，不是「非 IN_PROCESS 都放行」）
        let p_insp = insert_part(
            &pool,
            "Inspection-Part",
            "DWG-INS",
            customer_id,
            today,
            Some(today),
            false,
            None,
        )
        .await;
        insert_part_batch_with_status(&pool, p_insp, "INSPECTION").await;

        let mut conn = pool.acquire().await.unwrap();
        let out = BatchService::list_pending(&mut conn, &make_current(1, Role::Manager), 200, 0)
            .await
            .expect("list_pending OK");

        assert_eq!(
            out.total, 2,
            "PENDING + PROGRAMMING 都应计入 total（count 与 list 同一白名单）"
        );
        assert_eq!(out.items.len(), 2, "items 与 total 必须同口径");
        let ids: Vec<i64> = out.items.iter().map(|i| i.batch_id).collect();
        assert!(ids.contains(&b_pending), "PENDING 批次应列出: {ids:?}");
        assert!(
            ids.contains(&b_prog),
            "PROGRAMMING 批次应与 PENDING 同链路列出: {ids:?}"
        );
    }

    #[tokio::test]
    async fn dispatch_batch_bulk_success_path() {
        // 2026-09-30 重构：dispatch_batch bulk-only 形态，单条 target 即 1 元素 succeeded
        let pool = test_pool().await;
        let user_id = insert_user_with_role(&pool, "manager1", "password", "MANAGER").await;
        let customer_id = insert_customer_l2(&pool, "ACME").await;
        let today = chrono::NaiveDate::from_ymd_opt(2026, 9, 29).unwrap();

        let process_id = insert_process(&pool, "P-DISP", "ACME").await;
        let shelf_id = insert_shelf(&pool, "SH-DISP", "PRODUCTION").await;
        link_shelf_to_process(&pool, shelf_id, process_id).await;

        let p_id = insert_part(
            &pool,
            "P",
            "DWG",
            customer_id,
            today,
            Some(today),
            false,
            None,
        )
        .await;
        let b_id = insert_part_batch(&pool, p_id).await;

        let mut conn = pool.acquire().await.unwrap();
        let snowflake = crate::infra::snowflake::SnowflakeIdGenerator::new(1_577_836_800_000, 7);
        let r = BatchService::dispatch_batch(
            &mut conn,
            vec![(b_id, process_id)],
            Some("dispatch test"),
            &snowflake,
            &make_current(user_id, Role::Manager),
        )
        .await
        .expect("dispatch OK");
        assert_eq!(r.succeeded.len(), 1);
        assert_eq!(r.failed.len(), 0);
        assert_eq!(r.succeeded[0].batch_id, b_id);
        assert_eq!(r.succeeded[0].target_process_id, process_id);
        assert_eq!(r.succeeded[0].shelf_id, shelf_id);
        assert_eq!(r.succeeded[0].version, 1);
        // 2026-09-30 翻转：原断言只锁 step=NULL（把 bug 编码进了测试）。
        // 真正的池归属权威依据是 current_process_id，必须等于目标工序。
        assert!(
            r.succeeded[0].current_process_step_id.is_none(),
            "dispatch 路径仍不解析 step，step 应为 None（可选的显示用定位信息）"
        );
        assert_eq!(
            r.succeeded[0].current_process_id,
            Some(process_id),
            "下发后 current_process_id 必须等于 target_process_id（入池依据）"
        );

        // DB 验证：batch 应 IN_PROCESS + holder=shelf_id + current_process_id=目标工序
        let row: (String, Option<i64>, Option<i64>) = sqlx::query_as(
            "SELECT status, current_holder_id, current_process_id FROM t_part_batch WHERE id = $1",
        )
        .bind(b_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(row.0, "IN_PROCESS");
        assert_eq!(row.1, Some(shelf_id));
        assert_eq!(row.2, Some(process_id), "DB 层也要写入 current_process_id");

        // event 验证
        let n: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM t_part_event WHERE batch_id = $1 AND event_type = 'PLACED_ON_SHELF'",
        )
        .bind(b_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(n, 1);
    }

    /// 2026-10-06：已废弃的 `PROGRAMMING` 批次必须能正常下发（service 白名单 +
    /// repo 层 `allowed_from` 两道闸门都要放行）。
    ///
    /// 这条同时覆盖 service 层状态白名单与 `apply_batch_status_change` 的 SQL 层
    /// 源状态闸门（`allowed_from`）：后者漏放行会在 UPDATE 阶段被拒（40901 / 0 行）。
    #[tokio::test]
    async fn dispatch_batch_accepts_programming_batch() {
        let pool = test_pool().await;
        let user_id = insert_user_with_role(&pool, "manager1", "password", "MANAGER").await;
        let customer_id = insert_customer_l2(&pool, "ACME").await;
        let today = chrono::NaiveDate::from_ymd_opt(2026, 9, 29).unwrap();

        let process_id = insert_process(&pool, "P-PROG", "PROGRAMMING 源工序").await;
        let shelf_id = insert_shelf(&pool, "SH-PROG", "PRODUCTION").await;
        link_shelf_to_process(&pool, shelf_id, process_id).await;

        let p_id = insert_part(
            &pool,
            "P-PROG",
            "DWG-PROG",
            customer_id,
            today,
            Some(today),
            false,
            None,
        )
        .await;
        let b_id = insert_part_batch_with_status(&pool, p_id, "PROGRAMMING").await;

        let mut conn = pool.acquire().await.unwrap();
        let snowflake = crate::infra::snowflake::SnowflakeIdGenerator::new(1_577_836_800_000, 7);
        let r = BatchService::dispatch_batch(
            &mut conn,
            vec![(b_id, process_id)],
            Some("dispatch programming"),
            &snowflake,
            &make_current(user_id, Role::Manager),
        )
        .await
        .expect("PROGRAMMING 批次应可下发");
        assert_eq!(r.succeeded.len(), 1);
        assert_eq!(r.failed.len(), 0);
        assert_eq!(r.succeeded[0].batch_id, b_id);
        assert_eq!(r.succeeded[0].shelf_id, shelf_id);
        assert_eq!(
            r.succeeded[0].current_process_id,
            Some(process_id),
            "下发后 current_process_id 必须等于 target_process_id（入池依据）"
        );

        // DB 验证：PROGRAMMING → IN_PROCESS，holder / process 与 PENDING 源同形
        let row: (String, Option<i64>, Option<i64>) = sqlx::query_as(
            "SELECT status, current_holder_id, current_process_id FROM t_part_batch WHERE id = $1",
        )
        .bind(b_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(row.0, "IN_PROCESS");
        assert_eq!(row.1, Some(shelf_id));
        assert_eq!(row.2, Some(process_id));

        // 事件：from_status 必须记真实起点（写死 'PENDING' 会把历史流转记错）
        let (from_status, to_status): (Option<String>, Option<String>) = sqlx::query_as(
            "SELECT from_status, to_status FROM t_part_event \
             WHERE batch_id = $1 AND event_type = 'PLACED_ON_SHELF'",
        )
        .bind(b_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(from_status.as_deref(), Some("PROGRAMMING"));
        assert_eq!(to_status.as_deref(), Some("IN_PROCESS"));
    }

    #[tokio::test]
    async fn dispatch_batch_second_call_collects_failure_with_invalid_status() {
        // 2026-09-30 重构：二次 dispatch 走到 failed 数组（bulk 形态）而非直接抛错
        let pool = test_pool().await;
        let user_id = insert_user_with_role(&pool, "manager1", "password", "MANAGER").await;
        let customer_id = insert_customer_l2(&pool, "ACME").await;
        let today = chrono::NaiveDate::from_ymd_opt(2026, 9, 29).unwrap();

        let process_id = insert_process(&pool, "P-DISP2", "ACME").await;
        let shelf_id = insert_shelf(&pool, "SH-DISP2", "PRODUCTION").await;
        link_shelf_to_process(&pool, shelf_id, process_id).await;

        let p_id = insert_part(
            &pool,
            "P",
            "DWG",
            customer_id,
            today,
            Some(today),
            false,
            None,
        )
        .await;
        let b_id = insert_part_batch(&pool, p_id).await;

        let mut conn = pool.acquire().await.unwrap();
        let snowflake = crate::infra::snowflake::SnowflakeIdGenerator::new(1_577_836_800_000, 7);
        let current = make_current(user_id, Role::Manager);

        // 第一次成功
        BatchService::dispatch_batch(
            &mut conn,
            vec![(b_id, process_id)],
            None,
            &snowflake,
            &current,
        )
        .await
        .expect("第 1 次 dispatch OK");

        // 第二次：batch.status='IN_PROCESS' → 40903 → failed
        let _r = BatchService::dispatch_batch(
            &mut conn,
            vec![(b_id, process_id)],
            None,
            &snowflake,
            &current,
        )
        .await
        .expect_err("第 2 次 dispatch 应抛 BIZ_BATCH_INVALID_STATUS");
    }

    #[tokio::test]
    async fn dispatch_batch_nonexistent_batch_id_collects_failure() {
        // 2026-09-30 重构：不存在 batch_id service 抛 BIZ_BATCH_NOT_FOUND
        let pool = test_pool().await;
        let user_id = insert_user_with_role(&pool, "manager1", "password", "MANAGER").await;
        let process_id = insert_process(&pool, "P-NX", "ACME").await;
        let shelf_id = insert_shelf(&pool, "SH-NX", "PRODUCTION").await;
        link_shelf_to_process(&pool, shelf_id, process_id).await;

        let mut conn = pool.acquire().await.unwrap();
        let snowflake = crate::infra::snowflake::SnowflakeIdGenerator::new(1_577_836_800_000, 7);
        let e = BatchService::dispatch_batch(
            &mut conn,
            vec![(999_999_999, process_id)],
            None,
            &snowflake,
            &make_current(user_id, Role::Manager),
        )
        .await
        .expect_err("应抛 BIZ_BATCH_NOT_FOUND");
        assert_eq!(e.code(), code::BIZ_BATCH_NOT_FOUND);
    }

    #[tokio::test]
    async fn dispatch_batch_concurrent_modification_collects_invalid_status_failure() {
        // 2026-09-30 重构：并发前置 mutate 走 failed 而非抛错
        let pool = test_pool().await;
        let user_id = insert_user_with_role(&pool, "manager1", "password", "MANAGER").await;
        let customer_id = insert_customer_l2(&pool, "ACME").await;
        let today = chrono::NaiveDate::from_ymd_opt(2026, 9, 29).unwrap();
        let process_id = insert_process(&pool, "P-VC", "ACME").await;
        let shelf_id = insert_shelf(&pool, "SH-VC", "PRODUCTION").await;
        link_shelf_to_process(&pool, shelf_id, process_id).await;
        let p_id = insert_part(
            &pool,
            "P",
            "DWG",
            customer_id,
            today,
            Some(today),
            false,
            None,
        )
        .await;
        let b_id = insert_part_batch(&pool, p_id).await;

        sqlx::query("UPDATE t_part_batch SET status='IN_PROCESS', version=99 WHERE id = $1")
            .bind(b_id)
            .execute(&pool)
            .await
            .unwrap();

        let mut conn = pool.acquire().await.unwrap();
        let snowflake = crate::infra::snowflake::SnowflakeIdGenerator::new(1_577_836_800_000, 7);
        let _r = BatchService::dispatch_batch(
            &mut conn,
            vec![(b_id, process_id)],
            None,
            &snowflake,
            &make_current(user_id, Role::Manager),
        )
        .await
        .expect_err("应抛 BIZ_BATCH_INVALID_STATUS");
    }

    #[tokio::test]
    async fn dispatch_batch_picks_first_shelf_when_multiple_mappings_exist() {
        let pool = test_pool().await;
        let user_id = insert_user_with_role(&pool, "manager1", "password", "MANAGER").await;
        let customer_id = insert_customer_l2(&pool, "ACME").await;
        let today = chrono::NaiveDate::from_ymd_opt(2026, 9, 29).unwrap();
        let process_id = insert_process(&pool, "P-MULTI", "ACME").await;

        // 三个货架：sort_order 分别是 5 / 1 / 9，应取 sort_order=1 的那个
        let snowflake = crate::infra::snowflake::SnowflakeIdGenerator::new(1_577_836_800_000, 7);
        let now = now_naive();
        let shelf_first = snowflake.next_id();
        let shelf_mid = snowflake.next_id();
        let shelf_last = snowflake.next_id();
        for (shelf_id, sort_order) in [(shelf_first, 5), (shelf_mid, 1), (shelf_last, 9)] {
            sqlx::query(
                "INSERT INTO t_shelf (id, code, name, zone, is_active, display_order, version, \
                 created_at, updated_at) VALUES ($1, $2, $2, 'PRODUCTION', true, 0, 0, $3, $3)",
            )
            .bind(shelf_id)
            .bind(format!("SH-MULTI-{sort_order}"))
            .bind(now)
            .execute(&pool)
            .await
            .unwrap();
            sqlx::query(
                "INSERT INTO t_shelf_process (id, shelf_id, process_id, sort_order, version, \
                 created_at, updated_at) VALUES ($1, $2, $3, $4, 0, $5, $5)",
            )
            .bind(snowflake.next_id())
            .bind(shelf_id)
            .bind(process_id)
            .bind(sort_order)
            .bind(now)
            .execute(&pool)
            .await
            .unwrap();
        }

        let p_id = insert_part(
            &pool,
            "P",
            "DWG",
            customer_id,
            today,
            Some(today),
            false,
            None,
        )
        .await;
        let b_id = insert_part_batch(&pool, p_id).await;

        let mut conn = pool.acquire().await.unwrap();
        let r = BatchService::dispatch_batch(
            &mut conn,
            vec![(b_id, process_id)],
            None,
            &snowflake,
            &make_current(user_id, Role::Manager),
        )
        .await
        .expect("dispatch OK");
        // 期望 sort_order=1 的 shelf_mid
        assert_eq!(
            r.succeeded[0].shelf_id, shelf_mid,
            "应取 sort_order 最小的 shelf"
        );
    }

    #[tokio::test]
    async fn dispatch_batch_collects_failure_when_no_shelf_for_process() {
        // 2026-09-30 重构：无货架映射 → failed 而非抛错
        let pool = test_pool().await;
        let user_id = insert_user_with_role(&pool, "manager1", "password", "MANAGER").await;
        let customer_id = insert_customer_l2(&pool, "ACME").await;
        let today = chrono::NaiveDate::from_ymd_opt(2026, 9, 29).unwrap();
        let process_id_no_shelf = insert_process(&pool, "P-NSH", "ACME").await; // 没建映射
        let p_id = insert_part(
            &pool,
            "P",
            "DWG",
            customer_id,
            today,
            Some(today),
            false,
            None,
        )
        .await;
        let b_id = insert_part_batch(&pool, p_id).await;

        let mut conn = pool.acquire().await.unwrap();
        let snowflake = crate::infra::snowflake::SnowflakeIdGenerator::new(1_577_836_800_000, 7);
        let _r = BatchService::dispatch_batch(
            &mut conn,
            vec![(b_id, process_id_no_shelf)],
            None,
            &snowflake,
            &make_current(user_id, Role::Manager),
        )
        .await
        .expect_err("应抛 BIZ_SHELF_PROCESS_NOT_FOUND");
    }

    #[tokio::test]
    async fn dispatch_batch_rejects_for_inspector_role() {
        let pool = test_pool().await;
        let user_id = insert_user_with_role(&pool, "inspector1", "password", "INSPECTOR").await;
        let customer_id = insert_customer_l2(&pool, "ACME").await;
        let today = chrono::NaiveDate::from_ymd_opt(2026, 9, 29).unwrap();
        let process_id = insert_process(&pool, "P-INS", "ACME").await;
        let shelf_id = insert_shelf(&pool, "SH-INS", "PRODUCTION").await;
        link_shelf_to_process(&pool, shelf_id, process_id).await;
        let p_id = insert_part(
            &pool,
            "P",
            "DWG",
            customer_id,
            today,
            Some(today),
            false,
            None,
        )
        .await;
        let b_id = insert_part_batch(&pool, p_id).await;

        let mut conn = pool.acquire().await.unwrap();
        let snowflake = crate::infra::snowflake::SnowflakeIdGenerator::new(1_577_836_800_000, 7);
        let e = BatchService::dispatch_batch(
            &mut conn,
            vec![(b_id, process_id)],
            None,
            &snowflake,
            &make_current(user_id, Role::Inspector),
        )
        .await
        .expect_err("应 40300");
        assert_eq!(e.code(), code::FORBIDDEN);
    }

    #[tokio::test]
    async fn dispatch_batch_rejects_empty_targets_with_validation_error() {
        let pool = test_pool().await;
        let user_id = insert_user_with_role(&pool, "manager1", "password", "MANAGER").await;

        let mut conn = pool.acquire().await.unwrap();
        let snowflake = crate::infra::snowflake::SnowflakeIdGenerator::new(1_577_836_800_000, 7);
        let e = BatchService::dispatch_batch(
            &mut conn,
            vec![], // empty
            None,
            &snowflake,
            &make_current(user_id, Role::Manager),
        )
        .await
        .expect_err("空 targets 应 422");
        assert_eq!(e.code(), code::VALIDATION_ERROR);
        assert_eq!(
            e.http_status(),
            axum::http::StatusCode::UNPROCESSABLE_ENTITY
        );
    }

    // ===== auto_dispatch_preview 单测（2026-09-30 重构） =====

    #[tokio::test]
    async fn auto_dispatch_preview_no_chain_returns_skip_reason() {
        let pool = test_pool().await;
        let user_id = insert_user_with_role(&pool, "manager1", "password", "MANAGER").await;
        let customer_id = insert_customer_l2(&pool, "ACME").await;
        let today = chrono::NaiveDate::from_ymd_opt(2026, 9, 29).unwrap();

        // part 无 process_chain_id
        let p_a = insert_part(
            &pool,
            "P-A",
            "DWG-A",
            customer_id,
            today,
            Some(today),
            false,
            None,
        )
        .await;
        let b_a = insert_part_batch(&pool, p_a).await;

        let mut conn = pool.acquire().await.unwrap();
        let r = BatchService::auto_dispatch_preview(
            &mut conn,
            &make_current(user_id, Role::Manager),
            vec![b_a],
        )
        .await
        .expect("preview OK");
        assert_eq!(r.items.len(), 1);
        assert_eq!(r.items[0].skip_reason.as_deref(), Some("NO_PROCESS_CHAIN"));
        // 2026-09-30 review 第 1 轮：NO_PROCESS_CHAIN 时 process_chain_id 必为 None
        // （→ JSON `null`）；first_process_id / first_shelf_id 因上游不存在联动 None。
        assert!(r.items[0].process_chain_id.is_none());
        assert!(r.items[0].first_process_id.is_none());
        assert!(r.items[0].first_shelf_id.is_none());
    }

    #[tokio::test]
    async fn auto_dispatch_preview_no_step_returns_skip_reason() {
        let pool = test_pool().await;
        let user_id = insert_user_with_role(&pool, "manager1", "password", "MANAGER").await;
        let customer_id = insert_customer_l2(&pool, "ACME").await;
        let today = chrono::NaiveDate::from_ymd_opt(2026, 9, 29).unwrap();

        // 建 chain（无 step）
        let snowflake = crate::infra::snowflake::SnowflakeIdGenerator::new(1_577_836_800_000, 7);
        let now = now_naive();
        let chain_id = snowflake.next_id();
        sqlx::query(
            "INSERT INTO t_part_process_chain (id, version, created_at, created_by, updated_at, updated_by) \
             VALUES ($1, 0, $2, 1, $2, 1)",
        )
        .bind(chain_id)
        .bind(now)
        .execute(&pool)
        .await
        .unwrap();

        let p_a = insert_part(
            &pool,
            "P-NO-STEP",
            "DWG-NS",
            customer_id,
            today,
            Some(today),
            false,
            Some(chain_id),
        )
        .await;
        let b_a = insert_part_batch(&pool, p_a).await;

        let mut conn = pool.acquire().await.unwrap();
        let r = BatchService::auto_dispatch_preview(
            &mut conn,
            &make_current(user_id, Role::Manager),
            vec![b_a],
        )
        .await
        .expect("preview OK");
        assert_eq!(r.items.len(), 1);
        assert_eq!(r.items[0].skip_reason.as_deref(), Some("NO_PROCESS_STEP"));
        // 2026-09-30 review 第 1 轮：NO_PROCESS_STEP 时 chain 已知但首道工序/货架 None。
        assert!(r.items[0].process_chain_id.is_some());
        assert!(r.items[0].first_process_id.is_none());
        assert!(r.items[0].first_shelf_id.is_none());
    }

    #[tokio::test]
    async fn auto_dispatch_preview_with_chain_and_step_returns_first_process() {
        let pool = test_pool().await;
        let user_id = insert_user_with_role(&pool, "manager1", "password", "MANAGER").await;
        let customer_id = insert_customer_l2(&pool, "ACME").await;
        let today = chrono::NaiveDate::from_ymd_opt(2026, 9, 29).unwrap();

        // 建链 + 2 个 step（sort_order=1 / 2）+ 2 个货架 + 2 个映射
        let snowflake = crate::infra::snowflake::SnowflakeIdGenerator::new(1_577_836_800_000, 7);
        let now = now_naive();
        let chain_id = snowflake.next_id();
        sqlx::query(
            "INSERT INTO t_part_process_chain (id, version, created_at, created_by, updated_at, updated_by) \
             VALUES ($1, 0, $2, 1, $2, 1)",
        )
        .bind(chain_id)
        .bind(now)
        .execute(&pool)
        .await
        .unwrap();

        let process_first = insert_process(&pool, "P-AUTO-1", "FIRST").await;
        let process_second = insert_process(&pool, "P-AUTO-2", "SECOND").await;
        let shelf_first = insert_shelf(&pool, "SH-AUTO-1", "PRODUCTION").await;
        let shelf_second = insert_shelf(&pool, "SH-AUTO-2", "PRODUCTION").await;
        link_shelf_to_process(&pool, shelf_first, process_first).await;
        link_shelf_to_process(&pool, shelf_second, process_second).await;

        for (step_no, process_id) in [(2, process_second), (1, process_first)] {
            sqlx::query(
                "INSERT INTO t_process_chain_step (id, chain_id, sort_order, process_id, estimated_minutes, version, \
                 created_at, created_by, updated_at, updated_by) VALUES ($1, $2, $3, $4, 0, 0, $5, 1, $5, 1)",
            )
            .bind(snowflake.next_id())
            .bind(chain_id)
            .bind(step_no)
            .bind(process_id)
            .bind(now)
            .execute(&pool)
            .await
            .unwrap();
        }

        let p_a = insert_part(
            &pool,
            "P-AUTO",
            "DWG-AUTO",
            customer_id,
            today,
            Some(today),
            false,
            Some(chain_id),
        )
        .await;
        let b_a = insert_part_batch(&pool, p_a).await;

        let mut conn = pool.acquire().await.unwrap();
        let r = BatchService::auto_dispatch_preview(
            &mut conn,
            &make_current(user_id, Role::Manager),
            vec![b_a],
        )
        .await
        .expect("preview OK");
        assert_eq!(r.items.len(), 1);
        assert!(r.items[0].skip_reason.is_none());
        // 2026-09-30 review 第 1 轮：OK 路径三个 ID 必 Some（→ JSON 字符串）。
        assert_eq!(r.items[0].process_chain_id, Some(chain_id));
        assert_eq!(r.items[0].first_process_id, Some(process_first));
        assert_eq!(r.items[0].first_shelf_id, Some(shelf_first));
        assert_eq!(r.items[0].first_process_code, "P-AUTO-1");

        // DB 验证：batch 仍 PENDING（preview 不写库）
        let row: String = sqlx::query_scalar("SELECT status FROM t_part_batch WHERE id = $1")
            .bind(b_a)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(row, "PENDING", "preview 不应改变 batch.status");
    }

    #[tokio::test]
    async fn auto_dispatch_preview_unknown_batch_returns_not_found() {
        let pool = test_pool().await;
        let user_id = insert_user_with_role(&pool, "manager1", "password", "MANAGER").await;

        let mut conn = pool.acquire().await.unwrap();
        let r = BatchService::auto_dispatch_preview(
            &mut conn,
            &make_current(user_id, Role::Manager),
            vec![999_999_999],
        )
        .await
        .expect("preview OK");
        assert_eq!(r.items.len(), 1);
        assert_eq!(r.items[0].skip_reason.as_deref(), Some("NOT_FOUND"));
        // 2026-09-30 review 第 1 轮：NOT_FOUND 时三个 ID 必 None。
        assert!(r.items[0].process_chain_id.is_none());
        assert!(r.items[0].first_process_id.is_none());
        assert!(r.items[0].first_shelf_id.is_none());
    }

    #[tokio::test]
    async fn auto_dispatch_preview_no_shelf_returns_skip_reason() {
        let pool = test_pool().await;
        let user_id = insert_user_with_role(&pool, "manager1", "password", "MANAGER").await;
        let customer_id = insert_customer_l2(&pool, "ACME").await;
        let today = chrono::NaiveDate::from_ymd_opt(2026, 9, 29).unwrap();

        // 建链 + step，但首道工序不映射货架
        let snowflake = crate::infra::snowflake::SnowflakeIdGenerator::new(1_577_836_800_000, 7);
        let now = now_naive();
        let chain_id = snowflake.next_id();
        sqlx::query(
            "INSERT INTO t_part_process_chain (id, version, created_at, created_by, updated_at, updated_by) \
             VALUES ($1, 0, $2, 1, $2, 1)",
        )
        .bind(chain_id)
        .bind(now)
        .execute(&pool)
        .await
        .unwrap();

        let process_first = insert_process(&pool, "P-NSH-AUTO", "FIRST").await;
        sqlx::query(
            "INSERT INTO t_process_chain_step (id, chain_id, sort_order, process_id, estimated_minutes, version, \
             created_at, created_by, updated_at, updated_by) VALUES ($1, $2, 1, $3, 0, 0, $4, 1, $4, 1)",
        )
        .bind(snowflake.next_id())
        .bind(chain_id)
        .bind(process_first)
        .bind(now)
        .execute(&pool)
        .await
        .unwrap();
        // 注意：不调 link_shelf_to_process → 0 货架映射

        let p_a = insert_part(
            &pool,
            "P-NSH-AUTO",
            "DWG-NSH",
            customer_id,
            today,
            Some(today),
            false,
            Some(chain_id),
        )
        .await;
        let b_a = insert_part_batch(&pool, p_a).await;

        let mut conn = pool.acquire().await.unwrap();
        let r = BatchService::auto_dispatch_preview(
            &mut conn,
            &make_current(user_id, Role::Manager),
            vec![b_a],
        )
        .await
        .expect("preview OK");
        assert_eq!(r.items.len(), 1);
        assert_eq!(r.items[0].skip_reason.as_deref(), Some("NO_SHELF"));
        // 2026-09-30 review 第 1 轮：NO_SHELF 时 chain + first_process 已知，shelf None。
        assert!(r.items[0].process_chain_id.is_some());
        assert!(r.items[0].first_process_id.is_some());
        assert!(r.items[0].first_shelf_id.is_none());
    }

    /// 2026-10-06：已废弃的 `PROGRAMMING` 批次必须能被自动下发预览命中。
    ///
    /// `preview_auto_dispatch` 的 WHERE 若退回只收 `status = 'PENDING'`，该批次
    /// 会整条落空 → service 的 `NOT_FOUND` 兜底分支接手（`process_chain_id` /
    /// `first_process_id` / `first_shelf_id` 全 None），前端「自动下发」把它显示成
    /// 「批次不存在或状态不可下发」并跳过。故本条断言整条 preview 元数据链
    /// （chain / 首道工序 / 首货架）都在，且 `skip_reason` 为空。
    #[tokio::test]
    async fn auto_dispatch_preview_includes_programming_batch() {
        let pool = test_pool().await;
        let user_id = insert_user_with_role(&pool, "manager1", "password", "MANAGER").await;
        let customer_id = insert_customer_l2(&pool, "ACME").await;
        let today = chrono::NaiveDate::from_ymd_opt(2026, 9, 29).unwrap();

        // 完整链路：工艺链 + 首道 step + 首货架映射（保证 skip_reason 无从谈起）
        let snowflake = crate::infra::snowflake::SnowflakeIdGenerator::new(1_577_836_800_000, 7);
        let now = now_naive();
        let chain_id = snowflake.next_id();
        sqlx::query(
            "INSERT INTO t_part_process_chain (id, version, created_at, created_by, updated_at, updated_by) \
             VALUES ($1, 0, $2, 1, $2, 1)",
        )
        .bind(chain_id)
        .bind(now)
        .execute(&pool)
        .await
        .unwrap();

        let process_first = insert_process(&pool, "P-PROG-AUTO", "FIRST").await;
        let shelf_first = insert_shelf(&pool, "SH-PROG-AUTO", "PRODUCTION").await;
        link_shelf_to_process(&pool, shelf_first, process_first).await;
        sqlx::query(
            "INSERT INTO t_process_chain_step (id, chain_id, sort_order, process_id, estimated_minutes, version, \
             created_at, created_by, updated_at, updated_by) VALUES ($1, $2, 1, $3, 0, 0, $4, 1, $4, 1)",
        )
        .bind(snowflake.next_id())
        .bind(chain_id)
        .bind(process_first)
        .bind(now)
        .execute(&pool)
        .await
        .unwrap();

        let p_prog = insert_part(
            &pool,
            "P-PROG-AUTO",
            "DWG-PROG-AUTO",
            customer_id,
            today,
            Some(today),
            false,
            Some(chain_id),
        )
        .await;
        let b_prog = insert_part_batch_with_status(&pool, p_prog, "PROGRAMMING").await;

        let mut conn = pool.acquire().await.unwrap();
        let r = BatchService::auto_dispatch_preview(
            &mut conn,
            &make_current(user_id, Role::Manager),
            vec![b_prog],
        )
        .await
        .expect("preview OK");
        // 落进 preview 结果就该只有 1 条；若整条落空，兜底分支会补出 NOT_FOUND 行
        assert_eq!(r.items.len(), 1, "PROGRAMMING 批次应只产出 1 条 preview 项");
        assert_eq!(
            r.items[0].batch_id, b_prog,
            "命中行必须是入参里的那个 PROGRAMMING 批次"
        );
        assert_ne!(
            r.items[0].skip_reason.as_deref(),
            Some("NOT_FOUND"),
            "PROGRAMMING 批次被 preview 的状态闸门漏掉，会走 NOT_FOUND 兜底"
        );
        assert_eq!(r.items[0].skip_reason, None, "链路完整 ⇒ 可自动下发");
        assert_eq!(r.items[0].process_chain_id, Some(chain_id));
        assert_eq!(r.items[0].first_process_id, Some(process_first));
        assert_eq!(r.items[0].first_shelf_id, Some(shelf_first));

        // DB 验证：preview 只读，批次仍是 PROGRAMMING
        let row: String = sqlx::query_scalar("SELECT status FROM t_part_batch WHERE id = $1")
            .bind(b_prog)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(row, "PROGRAMMING", "preview 不应改变 batch.status");
    }
}

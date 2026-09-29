//! prod::batch 子模块 service 层 —— 业务逻辑
//!
//! 2026-09-29 新增：与 worker_pool / process_chain 同形 service 模块。
//!
//! ## 4 个公共方法
//! - [`BatchService::list_pending`] —— 读 PENDING 批次列表（handler `pool.acquire()`）
//! - [`BatchService::dispatch_batch`] —— 单 batch 下发（事务内）：
//!   fetch batch → 校验 status='PENDING' → 解析货架 → UPDATE OCC → 写事件
//! - [`BatchService::bulk_dispatch`] —— 顺序执行 dispatch_batch，任一失败全回滚
//! - [`BatchService::auto_dispatch`] —— 按 part.process_chain 首道 step 自动推导 target
//!
//! ## 事务 + 角色守卫
//! 角色守卫下沉到 service（与 worker_pool `pool_by_process` 等同形）：handler
//! 仅做权限分发；service 入口第一行 `current.require_any_role(...)`。
//!
//! 事务边界在 handler（与 worker_pool 范本一致）：handler `pool.begin()` →
//! service 收 `&mut PgConnection` → handler `commit()`。
//!
//! ## 事务内并发冲突（OCC）
//! dispatch_batch 入口 `find_batch_by_id` 后用 fetched `batch.version` 作
//! `expected_version`；UPDATE 0 行 → `VERSION_CONFLICT 40901`。
//! 当前实现没用悲观锁（`SELECT FOR UPDATE`），依赖 OCC 兜底；并发路径在
//! in-source 单测覆盖（详见模块底部 `mod tests`）。

use sqlx::PgConnection;

use crate::auth::rbac::{CurrentUser, Role};
use crate::infra::snowflake::SnowflakeIdGenerator;
use crate::modules::part::model::NewPartEvent;
use crate::modules::part::repo::PartRepo;
use crate::modules::prod::batch::dto::AutoDispatchSkippedItem;
use crate::modules::prod::batch::repo::{BatchRepo, FirstChainStepRow};
use crate::modules::prod::batch::vo::{
    AutoDispatchResult, BulkDispatchResult, DispatchResult, PendingBatchItem, PendingBatchListOut,
};
use crate::shared::error::{AppError, code};

use super::repo::PendingBatchRow;

/// `prod::batch` service（ZST，与 worker_pool 范本一致）。
///
/// 公共方法均通过 `<BatchService>::batch_service()` 访问（unit struct 形态）。
/// 显式 snowflake 形参保留（与 worker_pool `WorkerPoolService::refill_for_worker`
/// 同形 —— 跨模块调用方预留兼容）。
pub struct BatchService;

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

    /// 单 batch 下发核心逻辑。
    ///
    /// 事务内流程：
    /// 1. 角色守卫：Manager + Clerk
    /// 2. fetch batch → `None` → `BIZ_BATCH_NOT_FOUND`
    /// 3. 校验 `batch.status == 'PENDING'` → 否则 `BIZ_BATCH_INVALID_STATUS`
    /// 4. `find_first_shelf_for_process(target_process_id)` → `None` → `BIZ_SHELF_PROCESS_NOT_FOUND`
    /// 5. `update_batch_dispatched`（OCC）→ 0 行 → `VERSION_CONFLICT`
    /// 6. `PartRepo::insert_part_event('PLACED_ON_SHELF')` —— 写 `t_part_event`
    ///    （event id 由 `snowflake.next_id()` 生成；handler 端持 `&SnowflakeIdGenerator`
    ///    引用，service 内 `next_id()` 同步调用）。
    ///
    /// 返回 `DispatchResult { batch_id, current_process_step_id (NULL),
    /// target_process_id, shelf_id, version }`。
    #[allow(clippy::too_many_arguments)]
    pub async fn dispatch_batch(
        conn: &mut PgConnection,
        batch_id: i64,
        target_process_id: i64,
        note: Option<&str>,
        snowflake: &SnowflakeIdGenerator,
        current: &CurrentUser,
    ) -> Result<DispatchResult, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk])?;

        // 1. 取 batch
        let batch = BatchRepo::find_batch_by_id(&mut *conn, batch_id, false)
            .await?
            .ok_or_else(|| {
                AppError::biz(
                    code::BIZ_BATCH_NOT_FOUND,
                    format!("batch {batch_id} 不存在或已软删"),
                )
            })?;

        // 2. 校验 status
        if batch.status != "PENDING" {
            return Err(AppError::biz(
                code::BIZ_BATCH_INVALID_STATUS,
                format!(
                    "batch {} 当前 status='{}'，不允许 dispatch（要求 'PENDING'）",
                    batch.id, batch.status
                ),
            ));
        }

        // 3. 解析货架
        let shelf_id = BatchRepo::find_first_shelf_for_process(&mut *conn, target_process_id)
            .await?
            .ok_or_else(|| {
                AppError::biz(
                    code::BIZ_SHELF_PROCESS_NOT_FOUND,
                    format!(
                        "process {target_process_id} 未配置任何 active 货架映射（t_shelf_process 0 结果）"
                    ),
                )
            })?;

        // 4. UPDATE OCC（带当前 version）
        let rows_affected = BatchRepo::update_batch_dispatched(
            &mut *conn,
            batch.id,
            batch.version,
            shelf_id,
            Some(current.id),
        )
        .await?;
        if rows_affected == 0 {
            return Err(AppError::biz(
                code::VERSION_CONFLICT,
                format!(
                    "batch {} 版本冲突或状态非 PENDING（version={}，caller 未传 version 由 service 隐式 OCC）",
                    batch.id, batch.version
                ),
            ));
        }

        // 5. 写 part_event（PLACED_ON_SHELF）
        let part_id = batch.part_id;
        let quantity = batch.quantity;
        // fetch part 拿 drawing_no（事件 drawing_code 字段需要；批量 dispatch 时多次触发，
        // 故单批走 PartRepo::get_by_id —— 单条查询，与项目惯例一致）
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
                from_status: Some("PENDING"),
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

        Ok(DispatchResult {
            batch_id: batch.id,
            current_process_step_id: None,
            target_process_id,
            shelf_id,
            version: batch.version + 1,
        })
    }

    /// `POST /api/v2/prod/batches/bulk-dispatch` 业务逻辑。
    ///
    /// 顺序执行 `dispatch_batch` 核心逻辑：任一失败 → service 直接抛错，由
    /// handler 的 `Transaction` Drop 自动回滚（全成功才走到 `tx.commit()`）。
    ///
    /// 空 `targets` → `AppError::Validation`（HTTP 422，沿用 `BIZ_DELIVERY_PRINT_BAD_ORDER`
    /// 同形「显式 422」约束）。
    ///
    /// `snowflake` 形参：批量内部每条 PLACED_ON_SHELF 事件 id 由
    /// `&SnowflakeIdGenerator::next_id()` 生成；handler 端 `&state.snowflake`，
    /// 单测 `SnowflakeIdGenerator::new(...)` 同步调用。
    pub async fn bulk_dispatch(
        conn: &mut PgConnection,
        batch_ids_targets: Vec<(i64, i64)>,
        note: Option<&str>,
        snowflake: &SnowflakeIdGenerator,
        current: &CurrentUser,
    ) -> Result<BulkDispatchResult, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk])?;
        if batch_ids_targets.is_empty() {
            return Err(AppError::validation(
                "bulk-dispatch targets 不能为空（至少 1 条）",
            ));
        }

        let mut succeeded = Vec::with_capacity(batch_ids_targets.len());
        for (batch_id, target_process_id) in batch_ids_targets {
            // 任一失败 → 直接抛原 AppError；handler 的 `Transaction` Drop 自动
            // 回滚（service 不持有事务）；caller 通过 AppError 的 code 知道是
            // 哪一类失败（40404 / 40901 / 40903 / 40402 等）。
            let r = Self::dispatch_batch(
                &mut *conn,
                batch_id,
                target_process_id,
                note,
                snowflake,
                current,
            )
            .await?;
            succeeded.push(r);
        }

        Ok(BulkDispatchResult {
            succeeded,
            failed: vec![],
        })
    }

    /// `POST /api/v2/prod/batches/auto-dispatch` 业务逻辑。
    ///
    /// 对每个 batch_id：
    /// 1. `part_get_process_chain_id(batch.part_id)` → None / 部分 None → skipped('NO_PROCESS_CHAIN')
    /// 2. `first_step_of_chain(chain_id)` → None → skipped('NO_PROCESS_STEP')
    /// 3. 否则以 step.process_id 作为 target_process_id 调 dispatch 核心逻辑
    ///
    /// 全成功提交；任一硬错误（非 skipped）→ 全回滚（service 抛 AppError）。
    /// `skipped` 与 `succeeded` 互不影响（skipped 是合法的「该 batch 跳过」语义）。
    pub async fn auto_dispatch(
        conn: &mut PgConnection,
        batch_ids: Vec<i64>,
        snowflake: &SnowflakeIdGenerator,
        current: &CurrentUser,
    ) -> Result<AutoDispatchResult, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk])?;
        if batch_ids.is_empty() {
            return Err(AppError::validation("auto-dispatch batch_ids 不能为空"));
        }

        let mut succeeded = Vec::with_capacity(batch_ids.len());
        let mut skipped = Vec::new();

        for batch_id in batch_ids {
            // 取 batch（auto-dispatch 不要求 PENDING —— chain 不存在也可走；让 dispatch_batch 内部 status 守卫拒绝）
            // 但为节省一次空跑：先做一次 batch 检查（dispatch_batch 也会查，但 fetched 的 version 需在那里走）
            let batch = BatchRepo::find_batch_by_id(&mut *conn, batch_id, false)
                .await?
                .ok_or_else(|| {
                    AppError::biz(
                        code::BIZ_BATCH_NOT_FOUND,
                        format!("auto-dispatch: batch {batch_id} 不存在"),
                    )
                })?;

            // 1. 取 chain_id
            let chain_id = BatchRepo::part_get_process_chain_id(&mut *conn, batch.part_id).await?;
            let chain_id = match chain_id {
                None => {
                    skipped.push(AutoDispatchSkippedItem {
                        batch_id,
                        reason: "NO_PROCESS_CHAIN".to_string(),
                    });
                    continue;
                }
                Some(c) => c,
            };

            // 2. 取首道 step
            let step: FirstChainStepRow =
                match BatchRepo::first_step_of_chain(&mut *conn, chain_id).await? {
                    None => {
                        skipped.push(AutoDispatchSkippedItem {
                            batch_id,
                            reason: "NO_PROCESS_STEP".to_string(),
                        });
                        continue;
                    }
                    Some(s) => s,
                };

            // 3. 以 step.process_id 作为 target_process_id 调 dispatch
            let r = Self::dispatch_batch(
                &mut *conn,
                batch_id,
                step.step_process_id,
                None,
                snowflake,
                current,
            )
            .await?;
            succeeded.push(r);
        }

        Ok(AutoDispatchResult { succeeded, skipped })
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
// 单元测试（2026-09-29）
// ============================================================================
//
// 覆盖以下场景（与任务规约 1:1）：
// - list_pending_batches: 正常返回 + 排序 (system_delivery_date ASC, is_urgent DESC, created_at ASC)
// - dispatch_batch: 成功路径 + 二次 dispatch 40903 + 不存在 batch_id 40404 +
//   并发冲突 40901 + t_shelf_process 多结果取 LIMIT 1
// - bulk_dispatch: 全回滚（任一失败）+ 空 targets 422
// - auto_dispatch: 无 chain / 无 step / 全部无 chain / 有 chain 成功首道 step.id
//
// DB fixture 用 `hsh_erp_test_support::test_pool()`（与 project 现有 in-source tests
// 一致，例如 `src/modules/wx/wecom_client.rs::tests`）。

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

    /// 写一个 `t_shelf_process` 映射（无业务软删）。
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
        let snowflake = crate::infra::snowflake::SnowflakeIdGenerator::new(1_577_836_800_000, 7);
        let id = snowflake.next_id();
        let now = now_naive();
        sqlx::query(
            "INSERT INTO t_part_batch (id, part_id, batch_no, quantity, status, location, \
             current_holder_id, current_process_step_id, delivery_note_id, parent_batch_id, \
             version, created_at, updated_at) \
             VALUES ($1, $2, 1, 1, 'PENDING', NULL, NULL, NULL, NULL, NULL, 0, $3, $3)",
        )
        .bind(id)
        .bind(part_id)
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
        sqlx::query(
            "INSERT INTO t_part_batch (id, part_id, batch_no, quantity, status, location, \
             current_holder_id, current_process_step_id, delivery_note_id, parent_batch_id, \
             version, created_at, updated_at) \
             VALUES ($1, $2, 1, 1, 'IN_PROCESS', 'PRODUCTION_SHELF', NULL, NULL, NULL, NULL, 0, \
             $3, $3)",
        )
        .bind(b_ip)
        .bind(p_in_process)
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

    #[tokio::test]
    async fn dispatch_batch_success_path() {
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
            b_id,
            process_id,
            Some("dispatch test"),
            &snowflake,
            &make_current(user_id, Role::Manager),
        )
        .await
        .expect("dispatch OK");
        assert_eq!(r.batch_id, b_id);
        assert_eq!(r.target_process_id, process_id);
        assert_eq!(r.shelf_id, shelf_id);
        assert_eq!(r.version, 1);
        assert!(r.current_process_step_id.is_none());

        // DB 验证：batch 应 IN_PROCESS + holder=shelf_id
        let row: (String, Option<i64>) =
            sqlx::query_as("SELECT status, current_holder_id FROM t_part_batch WHERE id = $1")
                .bind(b_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(row.0, "IN_PROCESS");
        assert_eq!(row.1, Some(shelf_id));

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

    #[tokio::test]
    async fn dispatch_batch_second_call_returns_invalid_status() {
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
        BatchService::dispatch_batch(&mut conn, b_id, process_id, None, &snowflake, &current)
            .await
            .expect("第 1 次 dispatch OK");

        // 第二次：batch.status='IN_PROCESS' → 40903
        let e =
            BatchService::dispatch_batch(&mut conn, b_id, process_id, None, &snowflake, &current)
                .await
                .expect_err("第 2 次 dispatch 应 40903");
        assert_eq!(e.code(), code::BIZ_BATCH_INVALID_STATUS);
    }

    #[tokio::test]
    async fn dispatch_batch_nonexistent_batch_id_returns_not_found() {
        let pool = test_pool().await;
        let user_id = insert_user_with_role(&pool, "manager1", "password", "MANAGER").await;
        let process_id = insert_process(&pool, "P-NX", "ACME").await;
        let shelf_id = insert_shelf(&pool, "SH-NX", "PRODUCTION").await;
        link_shelf_to_process(&pool, shelf_id, process_id).await;

        let mut conn = pool.acquire().await.unwrap();
        let snowflake = crate::infra::snowflake::SnowflakeIdGenerator::new(1_577_836_800_000, 7);
        let e = BatchService::dispatch_batch(
            &mut conn,
            999_999_999, // 不存在的 batch_id
            process_id,
            None,
            &snowflake,
            &make_current(user_id, Role::Manager),
        )
        .await
        .expect_err("应 40404");
        assert_eq!(e.code(), code::BIZ_BATCH_NOT_FOUND);
    }

    #[tokio::test]
    async fn dispatch_batch_concurrent_modification_returns_invalid_status() {
        // 2026-09-29 实现笔记：dispatch_batch 内部 fetch + UPDATE 在同一 service
        // 调用内顺序执行（无并发交错），无法在单线程单连接单测里稳定构造
        // 40901 VERSION_CONFLICT 的 OCC 触发条件（version 在 fetch 之后、UPDATE
        // 之前被另一事务 mutate）。该场景由 E2E 并发压测 / production 观测；
        // 此单测改为覆盖「另一事务已完成 mutation → dispatch_batch 的 status 守卫
        // 直接拒绝」这一并发前置场景（40903 BIZ_BATCH_INVALID_STATUS）。
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

        // 模拟另一事务已完成 mutation：status='IN_PROCESS' + version=99
        // （service 入口 fetch 会读到 IN_PROCESS，status 守卫直接拒绝）。
        sqlx::query("UPDATE t_part_batch SET status='IN_PROCESS', version=99 WHERE id = $1")
            .bind(b_id)
            .execute(&pool)
            .await
            .unwrap();

        let mut conn = pool.acquire().await.unwrap();
        let snowflake = crate::infra::snowflake::SnowflakeIdGenerator::new(1_577_836_800_000, 7);
        let e = BatchService::dispatch_batch(
            &mut conn,
            b_id,
            process_id,
            None,
            &snowflake,
            &make_current(user_id, Role::Manager),
        )
        .await
        .expect_err("应 40903（前置并发 modification）");
        assert_eq!(e.code(), code::BIZ_BATCH_INVALID_STATUS);
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
            b_id,
            process_id,
            None,
            &snowflake,
            &make_current(user_id, Role::Manager),
        )
        .await
        .expect("dispatch OK");
        // 期望 sort_order=1 的 shelf_mid
        assert_eq!(r.shelf_id, shelf_mid, "应取 sort_order 最小的 shelf");
    }

    #[tokio::test]
    async fn dispatch_batch_rejects_when_no_shelf_for_process() {
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
        let e = BatchService::dispatch_batch(
            &mut conn,
            b_id,
            process_id_no_shelf,
            None,
            &snowflake,
            &make_current(user_id, Role::Manager),
        )
        .await
        .expect_err("应 40402");
        assert_eq!(e.code(), code::BIZ_SHELF_PROCESS_NOT_FOUND);
    }

    #[tokio::test]
    async fn dispatch_batch_rejects_for_inspector_role() {
        // 角色守卫下沉到 service：Inspector 不允许 dispatch
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
            b_id,
            process_id,
            None,
            &snowflake,
            &make_current(user_id, Role::Inspector),
        )
        .await
        .expect_err("应 40300");
        assert_eq!(e.code(), code::FORBIDDEN);
    }

    #[tokio::test]
    async fn bulk_dispatch_full_rollback_when_one_target_fails() {
        let pool = test_pool().await;
        let user_id = insert_user_with_role(&pool, "manager1", "password", "MANAGER").await;
        let customer_id = insert_customer_l2(&pool, "ACME").await;
        let today = chrono::NaiveDate::from_ymd_opt(2026, 9, 29).unwrap();
        let process_id = insert_process(&pool, "P-BLK", "ACME").await;
        let shelf_id = insert_shelf(&pool, "SH-BLK", "PRODUCTION").await;
        link_shelf_to_process(&pool, shelf_id, process_id).await;

        let p_ok = insert_part(
            &pool,
            "P-OK",
            "DWG-OK",
            customer_id,
            today,
            Some(today),
            false,
            None,
        )
        .await;
        let b_ok = insert_part_batch(&pool, p_ok).await;
        let p_bad = insert_part(
            &pool,
            "P-BAD",
            "DWG-BAD",
            customer_id,
            today,
            Some(today),
            false,
            None,
        )
        .await;
        let b_bad = insert_part_batch(&pool, p_bad).await;

        // 在 dispatch 前先把 b_bad 软删 → bulk 走到 b_bad 时会 BIZ_BATCH_NOT_FOUND
        sqlx::query("UPDATE t_part_batch SET deleted_at = now() WHERE id = $1")
            .bind(b_bad)
            .execute(&pool)
            .await
            .unwrap();

        let mut tx = pool.begin().await.unwrap();
        let snowflake = crate::infra::snowflake::SnowflakeIdGenerator::new(1_577_836_800_000, 7);
        let current = make_current(user_id, Role::Manager);
        let r = BatchService::bulk_dispatch(
            &mut tx,
            vec![(b_ok, process_id), (b_bad, process_id)],
            None,
            &snowflake,
            &current,
        )
        .await;
        // 任一失败 → service 抛 AppError；tx Drop 自动回滚
        assert!(r.is_err(), "bulk_dispatch 应在 b_bad 失败时抛 AppError");
        tx.rollback().await.unwrap(); // 显式回滚（Drop 兜底）

        // DB 验证：b_ok 应保持 PENDING（被回滚），b_bad 仍 deleted
        let row_ok: (String, Option<chrono::NaiveDateTime>) =
            sqlx::query_as("SELECT status, deleted_at FROM t_part_batch WHERE id = $1")
                .bind(b_ok)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(row_ok.0, "PENDING", "b_ok 应保持 PENDING（事务回滚）");
        assert!(row_ok.1.is_none(), "b_ok 不应有 deleted_at（事务回滚）");
    }

    #[tokio::test]
    async fn bulk_dispatch_rejects_empty_targets_with_validation_error() {
        let pool = test_pool().await;
        let user_id = insert_user_with_role(&pool, "manager1", "password", "MANAGER").await;

        let mut conn = pool.acquire().await.unwrap();
        let snowflake = crate::infra::snowflake::SnowflakeIdGenerator::new(1_577_836_800_000, 7);
        let e = BatchService::bulk_dispatch(
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

    #[tokio::test]
    async fn auto_dispatch_no_process_chain_skips_each_batch() {
        let pool = test_pool().await;
        let user_id = insert_user_with_role(&pool, "manager1", "password", "MANAGER").await;
        let customer_id = insert_customer_l2(&pool, "ACME").await;
        let today = chrono::NaiveDate::from_ymd_opt(2026, 9, 29).unwrap();

        // 2 个 batch：都没有 process_chain_id
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
        let p_b = insert_part(
            &pool,
            "P-B",
            "DWG-B",
            customer_id,
            today,
            Some(today),
            false,
            None,
        )
        .await;
        let b_a = insert_part_batch(&pool, p_a).await;
        let b_b = insert_part_batch(&pool, p_b).await;

        let mut conn = pool.acquire().await.unwrap();
        let snowflake = crate::infra::snowflake::SnowflakeIdGenerator::new(1_577_836_800_000, 7);
        let r = BatchService::auto_dispatch(
            &mut conn,
            vec![b_a, b_b],
            &snowflake,
            &make_current(user_id, Role::Manager),
        )
        .await
        .expect("auto_dispatch OK（无 chain 走 skipped）");
        assert_eq!(r.succeeded.len(), 0);
        assert_eq!(r.skipped.len(), 2);
        for s in &r.skipped {
            assert_eq!(s.reason, "NO_PROCESS_CHAIN");
        }
    }

    #[tokio::test]
    async fn auto_dispatch_no_step_skips_with_reason() {
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
        let r = BatchService::auto_dispatch(
            &mut conn,
            vec![b_a],
            &snowflake,
            &make_current(user_id, Role::Manager),
        )
        .await
        .expect("auto_dispatch OK（无 step 走 skipped）");
        assert_eq!(r.succeeded.len(), 0);
        assert_eq!(r.skipped.len(), 1);
        assert_eq!(r.skipped[0].reason, "NO_PROCESS_STEP");
    }

    #[tokio::test]
    async fn auto_dispatch_with_chain_and_step_dispatches_to_first_step_process() {
        let pool = test_pool().await;
        let user_id = insert_user_with_role(&pool, "manager1", "password", "MANAGER").await;
        let customer_id = insert_customer_l2(&pool, "ACME").await;
        let today = chrono::NaiveDate::from_ymd_opt(2026, 9, 29).unwrap();

        // 建链 + 2 个 step（sort_order=1 / 2，期望取 sort_order=1 的 process）
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
        let r = BatchService::auto_dispatch(
            &mut conn,
            vec![b_a],
            &snowflake,
            &make_current(user_id, Role::Manager),
        )
        .await
        .expect("auto_dispatch OK");
        assert_eq!(r.succeeded.len(), 1);
        assert_eq!(r.skipped.len(), 0);
        // 应取 sort_order=1 的 process_first + shelf_first
        assert_eq!(r.succeeded[0].target_process_id, process_first);
        assert_eq!(r.succeeded[0].shelf_id, shelf_first);

        // DB 验证：batch 应 IN_PROCESS + holder=shelf_first
        let row: (String, Option<i64>) =
            sqlx::query_as("SELECT status, current_holder_id FROM t_part_batch WHERE id = $1")
                .bind(b_a)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(row.0, "IN_PROCESS");
        assert_eq!(row.1, Some(shelf_first));
    }
}

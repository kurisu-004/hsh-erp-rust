//! prod::shelf_process 子模块 service —— 货架 ↔ 工序映射读写
//!
//! 2026-10-02 域归属反转：自 `src/modules/shelf/process_mapping/mod.rs` 平移
//! （业务逻辑零 diff，仅把「胖 trait 调用」换成「ZST 静态方法直调」）。
//!
//! ## 依赖方向
//! 迁移前：shelf 域 service 经胖 trait `ShelfRepoTrait` 的 2 个跨域 helper
//! （`proc_check_process_exists` / `proc_list_existing_process_ids`）反向依赖
//! `prod::process::ProcessRepo`；`t_shelf_process` 的 4 个方法也并入 shelf 的胖
//! trait。迁移后：
//! - `t_shelf_process` SQL 只在本文件 + 同目录 `repo.rs` 内
//! - 唯一跨域调用是**读** `shelf::repo::ShelfRepo::get_by_id`（校验 shelf 存在 +
//!   scope），方向由 shelf→prod 翻转为 prod→shelf
//! - process 存在性校验改调**同域** `prod::process::repo::ProcessRepo::list_by_ids`
//!   （零改动直接调），shelf 侧的 2 个反向 helper 已删
//!
//! ## 整组替换语义
//! `set_shelf_processes` 是「整组替换」：先软删该 shelf 的全部旧 mapping，再
//! INSERT 新列表；事务由 caller 保证（handler 层 `state.pool.begin()` + commit）。
//!
//! ## 错误码（数字不动，仅改归属说明；20504~20508 是已发布契约）
//! - 20501 `BIZ_SHELF_NOT_FOUND` —— 数字留在 shelf 段（货架本体）
//! - 20504 `BIZ_SHELF_PROCESS_SHELF_NOT_FOUND`
//! - 20505 `BIZ_SHELF_PROCESS_PROCESS_NOT_FOUND` —— items 里有 process_id 不存在
//! - 20507 `BIZ_SHELF_PROCESS_NOT_MAPPED`（worker_pool move 反向校验复用）
//! - 20508 `BIZ_SHELF_PROCESS_NOT_FOUND`（prod::batch dispatch 解析货架复用）
//! - 20502 `BIZ_SHELF_DUPLICATE_CODE` —— uk_t_shelf_process 撞（理论不该发生，service 已去重）

use sqlx::PgConnection;

use crate::auth::rbac::CurrentUser;
use crate::infra::snowflake::SnowflakeIdGenerator;
use crate::modules::prod::batch::service::guard::validate_shelf_zone;
use crate::modules::prod::process::repo::ProcessRepo;
use crate::modules::shelf::repo::ShelfRepo;
use crate::shared::error::{AppError, code};

use super::dto::SetShelfProcessesItem;
use super::repo::{NewShelfProcessRow, ShelfProcessRepo};
use super::vo::{
    AllShelfProcessMappingItem, AllShelfProcessMappingOut, ShelfProcessMappingItem,
    ShelfProcessMappingOut,
};

// ===========================================================================
// ShelfProcessService
// ===========================================================================

pub struct ShelfProcessService;

impl ShelfProcessService {
    /// 设置指定 shelf 的工序映射 —— **整组替换**语义：
    ///
    /// 1. 校验 shelf 存在 + active + `zone='PRODUCTION'`（`prod::batch::service::guard::validate_shelf_zone`）
    /// 2. 校验 items 内的所有 process_id 存在（`ProcessRepo::list_by_ids`，同域）
    /// 3. 软删该 shelf 的全部旧 mapping（`ShelfProcessRepo::soft_delete_all_for_shelf`）
    /// 4. INSERT 新 mapping（`ShelfProcessRepo::bulk_insert`，按 sort_order）
    ///
    /// 整组事务由 caller 保证（handler 层 `state.pool.begin()` + commit）。
    ///
    /// 错误码：
    /// - 20501 `BIZ_SHELF_NOT_FOUND`
    /// - 20512 `BIZ_SHELF_INACTIVE` —— shelf `is_active=false`（2026-10-04 新增，见下）
    /// - 20104 `BIZ_INVALID_VALUE` —— shelf `zone≠'PRODUCTION'`（2026-10-04 新增）/
    ///   process_id 非整数
    /// - 20505 `BIZ_SHELF_PROCESS_PROCESS_NOT_FOUND` —— items 里有 process_id 不存在
    /// - 20502 `BIZ_SHELF_DUPLICATE_CODE` —— uk_t_shelf_process 撞（理论不该发生，service 已去重）
    ///
    /// ## 2026-10-04 zone 守卫（写侧收紧，`current_holder_id` 写脏缺口的一环）
    /// 原实现只校验「货架存在」，**不校验 zone**，于是品检区（`INSPECTION`）货架可以被
    /// 配成某工序的落料架，再被 `ShelfProcessRepo::find_first_shelf_for_process`（同批
    /// 收紧）选中并写进 `t_part_batch.current_holder_id`；而报工台取件页的取件 SQL 硬限定
    /// `sh.zone = 'PRODUCTION'`，这种批次就永远不会被工人领到，且不报错。
    ///
    /// **为什么「只有 PRODUCTION 区能配工序」是对的**：`t_shelf_process` 的语义是
    /// 「可执行某工序的**在制品**货架」，三条读侧全部是生产流 —— 下发解析货架
    /// （`dispatch_single`）、worker 归还（worker_scan RETURNED）、候选池放回
    /// （`/prod/pool/move`）。品检流走的是显式 `target_inspection_shelf_id` +
    /// `validate_shelf_zone(.., "INSPECTION")`（见 `prod::batch::service::scan` /
    /// `outsource`），**完全不读 `t_shelf_process`**；前端 10 处
    /// `useShelfProcessFilter` 消费 `GET /prod/shelf-processes` 时，货架候选源也一律是
    /// `zone='PRODUCTION'` 过滤后的列表。品检架上的映射行是**既无读侧消费、又能让脏货架
    /// 落进 holder** 的纯负债。
    ///
    /// **收紧的副作用（已知且接受）**：对品检架（或任何非 PRODUCTION 区货架）调本端点
    /// 现在返 `20104`，且**存量**非法映射在下一次 `POST` 整组替换时同样被拒（整组替换
    /// 语义下无法只改其中一条）。这是刻意的 fail-fast：让配置错误在**写侧**暴露，而不是
    /// 继续静默产出漏件批次。存量非法行的排查 SQL（只读）见
    /// `docs/api/production/shelf-process-mapping.md` 的「只读诊断 SQL」一节；**本仓不
    /// 自动修数据**，修复走独立的数据修复单。
    ///
    /// ## 20512 的可达性
    /// 货架 service 的 `deactivate` 等价于 soft-delete（同时 `is_active=false` +
    /// `deleted_at=now()`），故经 API 停用的货架先命中 20501；20512 是防「直接改库 /
    /// 历史数据造成 `is_active=false` 但未软删」的防御位，与 `validate_shelf_zone`
    /// 在其它 5 个调用点的定位一致。
    pub async fn set_shelf_processes(
        &self,
        conn: &mut PgConnection,
        snowflake: &SnowflakeIdGenerator,
        shelf_id: i64,
        items: &[SetShelfProcessesItem],
        current: &CurrentUser,
    ) -> Result<(), AppError> {
        // 1. shelf 存在性 + 软删 + 停用 + zone 守卫（2026-10-04 复用 prod::batch 的
        // `validate_shelf_zone`，与 place_on_shelf / pickup / outsource 等 6 个生产流
        // 端点**同源同码**：20501 → 20512 → 20104，不另造判定）。
        //
        // 依赖方向说明：shelf_process → prod::batch::service::guard 是**同域**横向依赖
        // （guard.rs 是 prod 域的货架校验自由函数层，不是 batch 域私有实现）；换来的是
        // 「判序与错误码只有一份」——本仓已因两份判序（2026-10-02 域拆分前后的内联 SQL）
        // 分叉过一次，不值得再开第二个。
        //
        // ⚠️ 本函数原先自己 `ShelfRepo::get_by_id` 拿 `shelf`（为 20501），改调本守卫后
        // 不再需要该行：守卫内部已按同一 id 查过同一次，`shelf.id == shelf_id`，
        // 故下方 `bulk_insert` 直接用形参 `shelf_id`，**不增加任何 DB 往返**。
        validate_shelf_zone(&mut *conn, shelf_id, "PRODUCTION").await?;

        // 2. 解析 + 校验所有 process_id 存在
        let mut process_ids: Vec<i64> = Vec::with_capacity(items.len());
        for it in items {
            let pid = it.process_id.parse::<i64>().map_err(|_| {
                AppError::biz(code::BIZ_INVALID_VALUE, "process_id 必须为雪花 ID 字符串")
            })?;
            process_ids.push(pid);
        }
        if !process_ids.is_empty() {
            // 一次性批量查 process —— 防 N+1（同域 ProcessRepo::list_by_ids）
            let existing_ids = ProcessRepo::list_by_ids(&mut *conn, &process_ids).await?;
            if existing_ids.len() != process_ids.len() {
                // 找出缺失的 id（用 Vec 差集；批量小，开销可忽略）
                let existing_set: std::collections::HashSet<i64> =
                    existing_ids.iter().map(|p| p.id).collect();
                let missing: Vec<i64> = process_ids
                    .iter()
                    .filter(|p| !existing_set.contains(p))
                    .copied()
                    .collect();
                return Err(AppError::biz(
                    code::BIZ_SHELF_PROCESS_PROCESS_NOT_FOUND,
                    format!("process 不存在: {:?}", missing),
                ));
            }
        }

        // 3. 软删旧 mapping（事务内）
        ShelfProcessRepo::soft_delete_all_for_shelf(&mut *conn, shelf_id).await?;

        // 4. 批量 INSERT 新 mapping（空 items = 清空映射；无行写）
        let new_rows: Vec<NewShelfProcessRow> = items
            .iter()
            .zip(process_ids.iter())
            .map(|(it, &pid)| NewShelfProcessRow {
                shelf_id,
                process_id: pid,
                sort_order: it.sort_order,
            })
            .collect();
        ShelfProcessRepo::bulk_insert(&mut *conn, &new_rows, snowflake, current.id).await?;

        Ok(())
    }

    /// 列出所有 active shelf 的全部 mapping（`GET /prod/shelf-processes`）。
    ///
    /// 2026-10-02 自 `shelf::service::ShelfService::list_all_process_mappings`
    /// 平移（原端点 `GET /shelves/processes` 已硬切到 `/api/v2/prod/shelf-processes`）。
    /// 单条 SQL JOIN（防 N+1）。任意已登录可调。
    pub async fn list_all_mappings(
        &self,
        conn: &mut PgConnection,
        current: &CurrentUser,
    ) -> Result<AllShelfProcessMappingOut, AppError> {
        current.require_any_role(&[
            crate::auth::rbac::Role::Manager,
            crate::auth::rbac::Role::Clerk,
            crate::auth::rbac::Role::CncProgrammer,
            crate::auth::rbac::Role::ShelfAccount,
            crate::auth::rbac::Role::Inspector,
        ])?;

        let rows = ShelfProcessRepo::list_all_active_mappings(&mut *conn).await?;

        // SHELF_ACCOUNT scope：统一走 `can_access_shelf`（Manager / wildcard 已短路
        // 返回 true ⇒ 见全集；其他按 user.shelf_ids 收窄）。
        let items: Vec<AllShelfProcessMappingItem> = rows
            .into_iter()
            .filter(|(sid, _, _, _)| current.can_access_shelf(*sid))
            .map(|(sid, pid, sc, pc)| AllShelfProcessMappingItem {
                shelf_id: sid,
                shelf_code: sc,
                process_id: pid,
                process_code: pc,
            })
            .collect();

        Ok(AllShelfProcessMappingOut { items })
    }

    /// 列出指定 shelf 的所有 active mapping（按 sort_order ASC）。
    pub async fn list_shelf_processes(
        &self,
        conn: &mut PgConnection,
        shelf_id: i64,
        current: &CurrentUser,
    ) -> Result<ShelfProcessMappingOut, AppError> {
        // 权限：与 list_shelves 一致（任意已登录）
        current.require_any_role(&[
            crate::auth::rbac::Role::Manager,
            crate::auth::rbac::Role::Clerk,
            crate::auth::rbac::Role::CncProgrammer,
            crate::auth::rbac::Role::ShelfAccount,
            crate::auth::rbac::Role::Inspector,
        ])?;

        // shelf 存在性 / scope 校验
        let shelf = ShelfRepo::get_by_id(&mut *conn, shelf_id)
            .await?
            .ok_or_else(|| {
                AppError::biz(
                    code::BIZ_SHELF_NOT_FOUND,
                    format!("shelf {shelf_id} 不存在"),
                )
            })?;
        if !current.can_access_shelf(shelf.id) {
            return Err(AppError::biz(
                code::SHELF_MISMATCH,
                format!("无权访问 shelf {shelf_id}"),
            ));
        }

        let rows = ShelfProcessRepo::list_by_shelf(&mut *conn, shelf.id).await?;
        let items = rows
            .into_iter()
            .map(
                |(sid, pid, sort_order, shelf_code, process_code)| ShelfProcessMappingItem {
                    shelf_id: sid,
                    shelf_code,
                    process_id: pid,
                    process_code,
                    sort_order,
                },
            )
            .collect();
        Ok(ShelfProcessMappingOut { items })
    }
}

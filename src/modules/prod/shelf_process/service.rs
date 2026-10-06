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
    /// 1. 校验 shelf 存在（20501）
    /// 2. **`items` 非空时**额外校验 shelf active（20512）+ `zone='PRODUCTION'`（20104）
    ///    （`prod::batch::service::guard::validate_shelf_zone`；`items` 为空 = 清空，见下）
    /// 3. 校验 items 内的所有 process_id 存在（`ProcessRepo::list_by_ids`，同域）
    /// 4. 软删该 shelf 的全部旧 mapping（`ShelfProcessRepo::soft_delete_all_for_shelf`）
    /// 5. INSERT 新 mapping（`ShelfProcessRepo::bulk_insert`，按 sort_order）
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
    /// （`dispatch_single`）、worker 归还（`worker_scan` RETURNED）、候选池放回
    /// （`/prod/pool/move`）。品检流走的是显式 `target_inspection_shelf_id` +
    /// `validate_shelf_zone(.., "INSPECTION")`（见 `prod::batch::service::scan` /
    /// `outsource`），**完全不读 `t_shelf_process`**；前端 10 处
    /// `useShelfProcessFilter` 消费 `GET /prod/shelf-processes` 时，货架候选源也一律是
    /// `zone='PRODUCTION'` 过滤后的列表。品检架上的映射行因此是**无读侧消费**的纯负债：
    /// 三条读侧里只有 `dispatch_single` 会**从映射里选出**货架（故其 SQL 必须自带
    /// 谓词），另两条的 `shelf_id` 来自请求、在写 `current_holder_id` 之前各自独立
    /// 守过货架本身（`worker_scan` 用 `ShelfRepo::get_by_id_zone(.., "PRODUCTION")`
    /// 一步守掉存在/软删/停用/zone 四个谓词；`move_batch` 用
    /// `validate_shelf_zone`）—— 故品检架上的映射行不会被它们选中。
    ///
    /// **收紧的副作用（已知且接受）**：对品检架（或任何非 PRODUCTION 区货架）调本端点
    /// 且 `items` **非空**时现在返 `20104`。这是刻意的 fail-fast：让配置错误在**写侧**
    /// 暴露，而不是继续静默产出漏件批次。存量非法行**本仓不自动修数据**，修复走独立
    /// 的数据修复单（本仓也没有登记这些行的只读诊断 SQL，改前先按上面三个谓词
    /// （`deleted_at IS NULL` / `is_active` / `zone = 'PRODUCTION'`）自行捞行）。
    ///
    /// ## `items: []`（清空）豁免 zone / is_active 守卫
    /// 守卫只对「**要写新 mapping 行**」的请求生效，`items.is_empty()` 时跳过。理由：
    /// 清空只会 `soft_delete_all_for_shelf`，**不新增任何非法映射**，拦它零收益；反过来，
    /// 无条件守卫会让**存量非法映射失去唯一的 API 清理路径** —— 整组替换语义下改不了
    /// 其中一条，而 `ShelfUpdateRequest` 只有 `name` / `location`（`zone` 不可经 API 改），
    /// 也就没有「先把架改成 PRODUCTION 再清映射」这条绕路 —— 结果是「按上面三个谓词
    /// 捞出的非法行，在本仓找不到任何 API 能清掉」，诊断与处置自相矛盾。
    /// 存在性（20501）仍无条件守：货架不存在/已软删时清空也无意义（`get_by_id` 带
    /// `deleted_at IS NULL`，否则会静默「成功」一个对已删货架的空操作）。
    /// 回归见 `tests/production/shelf_process.rs::set_shelf_processes_allows_clearing_inspection_zone_mappings`。
    ///
    /// ## 20512 的可达性
    /// 货架 service 的 `deactivate` 等价于 soft-delete（同时 `is_active=false` +
    /// `deleted_at=now()`），故经 API 停用的货架先命中 20501；20512 是防「直接改库 /
    /// 历史数据造成 `is_active=false` 但未软删」的防御位，与 `validate_shelf_zone`
    /// 在其余生产流调用点的定位一致。
    pub async fn set_shelf_processes(
        &self,
        conn: &mut PgConnection,
        snowflake: &SnowflakeIdGenerator,
        shelf_id: i64,
        items: &[SetShelfProcessesItem],
        current: &CurrentUser,
    ) -> Result<(), AppError> {
        // 1. shelf 存在性 + 软删 + 停用 + zone 守卫（2026-10-04 复用 prod::batch 的
        // `validate_shelf_zone`，与 place_on_shelf / pickup / outsource 等生产流
        // 端点**同源同码**：20501 → 20512 → 20104，不另造判定）。
        //
        // ⚠️ 守卫**只对「要写新 mapping 行」的请求生效**，
        // `items` 为空（清空）时跳过。存在性（20501）仍在下方无条件守。
        //
        //   为什么不拦清空：清空只 `soft_delete_all_for_shelf`，不新增任何非法映射，
        //   拦它零收益；无条件守卫会让存量非法映射**失去唯一的 API 清理路径**（整组
        //   替换改不了其中一条，且 `ShelfUpdateRequest` 无 `zone` 字段、换不了区），
        //   于是「按那三个谓词捞出的非法行在本仓清不掉」，诊断与处置自相矛盾。
        //
        //   为什么仍守存在性：货架不存在/已软删时清空也无意义，放行会变成对已删货架的
        //   静默空操作（`ShelfRepo::get_by_id` 带 `deleted_at IS NULL`）。
        //
        //   回归：`tests/production/shelf_process.rs::set_shelf_processes_allows_clearing_inspection_zone_mappings`
        //   （品检架 + `items: []` → 200 且旧映射被清掉）。
        //
        // 依赖方向说明：shelf_process → prod::batch::service::guard 是**同域**横向依赖
        // （guard.rs 是 prod 域的货架校验自由函数层，不是 batch 域私有实现）；换来的是
        // 「判序与错误码只有一份」——本仓已因两份判序（2026-10-02 域拆分前后的内联 SQL）
        // 分叉过一次，不值得再开第二个。
        //
        // ⚠️ 两个分支各自恰好 1 次 DB 往返：非空分支由守卫内部的 `ShelfRepo::get_by_id`
        // 承担，空分支由下面的显式 `get_by_id` 承担。守卫内部查的就是同一 id
        // （`shelf.id == shelf_id`），故下方 `bulk_insert` 直接用形参 `shelf_id`。
        if !items.is_empty() {
            validate_shelf_zone(&mut *conn, shelf_id, "PRODUCTION").await?;
        } else if ShelfRepo::get_by_id(&mut *conn, shelf_id).await?.is_none() {
            // 文案与 `validate_shelf_zone` 的 20501 分支**逐字一致**（刻意）：同一个
            // 「货架不存在」事实不该因 `items` 是否为空而给运营两种说法。改一处记得改另一处。
            //
            // 2026-10-04 review 第 2 轮 N4（技术债登记，**非缺陷**）：全仓这句 20501 文案
            // 共 3 处 —— `prod::batch::service::guard::validate_shelf_zone`、本分支这一处、
            // 以及本文件 `list_shelf_processes` 的既有那一处（3 处字面值已用 shasum 逐字
            // 核对一致）。抽出共享断言（如 `guard::assert_shelf_exists`）供三处复用属**后续
            // 重构项**，本轮刻意不做：它要动 `validate_shelf_zone` 全部调用点共用的共享层，
            // 收益（几行去重）远小于把既有生产流端点的守卫一次性卷入改动的风险。
            return Err(AppError::biz(
                code::BIZ_SHELF_NOT_FOUND,
                format!("shelf {shelf_id} 不存在"),
            ));
        }

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

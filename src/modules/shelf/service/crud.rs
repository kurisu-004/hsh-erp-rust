//! shelf 域 CRUD service
//!
//! 列表 / 详情 / 创建 / 更新 / 软删（deactivate）—— 共 5 个端点。
//!
//! ## 业务约束（service 层 enforce）
//! - `zone` ∈ {PRODUCTION, INSPECTION}
//! - `code` 业务唯一键，update 不允许改
//! - `deactivate` 等价于 soft-delete：同时 `is_active = false` + `deleted_at = now()`
//! - `deactivate` 前查 `t_part_batch.current_holder_id = shelf_id` 且
//!   `status IN ('IN_PROCESS','INSPECTION')` 引用，>0 ⇒ 20503 拒
//!   （2026-10-01：REPAIRING 降级为 `is_repairing` 标记后，返修批次 status 即
//!   IN_PROCESS，仍被本守卫覆盖）
//!
//! ## picker 端点
//! 见同级 `service::picker`（for-return / for-inspection）。
//!
//! ## 2026-10-02 域拆分
//! 工序映射端点（`set_shelf_processes` / `list_shelf_processes` /
//! `list_all_process_mappings`）搬到 `src/modules/prod/shelf_process/`；本文件的
//! `to_shelf_out` 随之去掉第 2 参数 `account_count`（`ShelfOut.account_count` 出参
//! 取消，账号绑定真源在 iam 域）。
//!
//! ## 事务边界（2026-09-22 重构对齐 iam 范本）
//! 事务移交 handler（与 20 个 handler 文件现状对齐）：service 仅业务逻辑，所有
//! 跨 repo 操作经 `repo: R`（by-value；`R: ShelfRepoTrait`）参数传入——handler/service
//! 借 `&mut *tx` / `&mut *conn` 喂给 `ShelfRepoTrait`（trait 已直接
//! `impl for &mut PgConnection`）。service 不知事务——handler `pool.begin()` +
//! `tx.commit()` 包外。
//!
//! `ShelfService` 是 unit struct（无字段依赖，iam 范本 §6）；方法签名
//! `<R: ShelfRepoTrait>(&self, mut repo: R, ...)`，生产 `R = &mut PgConnection`。

use crate::auth::rbac::{CurrentUser, Role};
use crate::shared::error::{AppError, code};

use super::super::dto::*;
use super::super::model::TShelf;
use super::super::repo::ShelfRepoTrait;
use super::super::vo::*;
use super::{DEFAULT_LIMIT, MAX_LIMIT, ZONE_INSPECTION, ZONE_PRODUCTION};

fn shelf_not_found() -> AppError {
    AppError::biz(code::BIZ_SHELF_NOT_FOUND, "货架不存在")
}

fn version_conflict() -> AppError {
    AppError::biz(code::VERSION_CONFLICT, "数据已被他人修改，请刷新后重试")
}

/// 把 `TShelf` 转 `ShelfOut`。
///
/// 2026-10-10：新增 `capacity`（直接取 `t_shelf` 列）+ `current_load`（**读时聚合**，
/// 不在 `TShelf` 里）。聚合口径由 [`crate::shared::shelf::load::LOAD_AGGREGATE_SQL`]
/// 单点承担；`capacity = None` 表示该货架行**不在** batch 查询的结果里（理论上不会
/// 发生 —— id 来自同一张表），落 `None` 而非 panic。
///
/// ## 为什么不再保持「纯 `TShelf → ShelfOut`」
///
/// 2026-10-10 之前本函数确实是纯函数（`account_count` 出参取消后只剩字段搬运）。
/// 加 `current_load` 之后它需要一次额外查询，于是调用方改为「先查行、再查负载、
/// 再映射」三步。**不**把聚合 JOIN 进货架列表 SQL 的理由见
/// [`crate::shared::shelf::load::loads_by_shelf_ids`] 的 doc（会让 shared 层接管
/// shelf 域的 `QueryBuilder` 筛选语义）。
fn to_shelf_out(s: TShelf, load: (Option<i32>, i64)) -> ShelfOut {
    let (capacity, current_load) = load;
    ShelfOut {
        id: s.id,
        code: s.code,
        name: s.name,
        zone: s.zone,
        location: s.location,
        is_active: s.is_active,
        display_order: s.display_order,
        // `TShelf.capacity` 是存储列、`loads_by_shelf_ids` 那份是同一列的第二次
        // 读取；取前者（零额外往返语义），后者只为算 `current_load`。
        capacity: s.capacity.or(capacity),
        current_load,
        version: s.version,
        created_at: s.created_at,
        updated_at: s.updated_at,
    }
}

/// 校验 `zone` ∈ {PRODUCTION, INSPECTION}；返回规范化大写字符串。
fn check_zone(s: &str) -> Result<String, AppError> {
    let upper = s.trim().to_uppercase();
    match upper.as_str() {
        ZONE_PRODUCTION | ZONE_INSPECTION => Ok(upper),
        _ => Err(AppError::biz(
            code::BIZ_INVALID_VALUE,
            "zone 必须是 PRODUCTION 或 INSPECTION",
        )),
    }
}

pub struct ShelfService;

impl ShelfService {
    // =======================================================================
    // 列表 / 详情
    // =======================================================================

    pub async fn list_shelves<R: ShelfRepoTrait>(
        &self,
        mut repo: R,
        query: &ShelfListQuery,
        current: &CurrentUser,
    ) -> Result<ShelfListOut, AppError> {
        current.require_any_role(&[
            Role::Manager,
            Role::Clerk,
            Role::CncProgrammer,
            Role::ShelfAccount,
            Role::Inspector,
        ])?;

        let limit = query.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
        let offset = query.offset.unwrap_or(0).max(0);
        let code_like = query
            .code_like
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let zone = query
            .zone
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());

        let items = repo
            .list_with_filters(code_like, zone, query.is_active, limit, offset)
            .await?;
        let total = repo
            .count_with_filters(code_like, zone, query.is_active)
            .await?;
        // 2026-10-10：`capacity` / `current_load` 出参 —— 一次批量聚合（零 N+1），
        // 口径见 `shared::shelf::load::LOAD_AGGREGATE_SQL`
        let loads = repo
            .load_by_ids(&items.iter().map(|s| s.id).collect::<Vec<_>>())
            .await?;
        let zero = (None, 0);
        let out_items = items
            .into_iter()
            .map(|s| {
                let load = loads.get(&s.id).copied().unwrap_or(zero);
                to_shelf_out(s, load)
            })
            .collect();

        Ok(ShelfListOut {
            items: out_items,
            total,
            limit,
            offset,
        })
    }

    pub async fn get_shelf<R: ShelfRepoTrait>(
        &self,
        mut repo: R,
        id: i64,
        current: &CurrentUser,
    ) -> Result<ShelfOut, AppError> {
        current.require_any_role(&[
            Role::Manager,
            Role::Clerk,
            Role::CncProgrammer,
            Role::ShelfAccount,
            Role::Inspector,
        ])?;

        let s = repo.get_by_id(id).await?.ok_or_else(shelf_not_found)?;

        if !current.can_access_shelf(s.id) {
            return Err(AppError::biz(
                code::SHELF_MISMATCH,
                format!("无权访问 shelf {}", s.id),
            ));
        }

        // 2026-10-10：`capacity` / `current_load` 出参。单条详情也要出这两个字段 ——
        // 前端货架管理页的编辑弹窗靠 `GET /shelves/{id}` 回显上限（列表页只给
        // 摘要，不回显全部可编辑字段）
        let load = repo
            .load_by_ids(&[s.id])
            .await?
            .get(&s.id)
            .copied()
            .unwrap_or((None, 0));

        Ok(to_shelf_out(s, load))
    }

    // =======================================================================
    // 创建 / 更新 / 软删（deactivate）
    // =======================================================================

    pub async fn create_shelf<R: ShelfRepoTrait>(
        &self,
        mut repo: R,
        snowflake: &crate::infra::snowflake::SnowflakeIdGenerator,
        req: &ShelfCreateRequest,
        current: &CurrentUser,
    ) -> Result<ShelfOut, AppError> {
        current.require_role(Role::Manager)?;

        let code = req.code.trim();
        if code.is_empty() {
            return Err(AppError::biz(code::BIZ_INVALID_VALUE, "code 不能为空"));
        }
        let name = req.name.trim();
        if name.is_empty() {
            return Err(AppError::biz(code::BIZ_INVALID_VALUE, "name 不能为空"));
        }
        let zone = check_zone(&req.zone)?;
        let location = req
            .location
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let display_order = req.display_order.unwrap_or(0);
        // 2026-10-10：`capacity` 缺省 = 不限（落 NULL）。**刻意不校验 `> 0`** ——
        // 「`NULL` 或 `<= 0` = 不限」是选架排序的口径（不限架恒排最后），在这里
        // 报 20104 会让同一个值在 create 走不通、在 update 却走得通（后者按同一
        // 口径接受），两个端点的容错集合分叉。零 / 负值与 NULL 语义完全相同，
        // 没有理由区别对待。
        let capacity = req.capacity;

        let id = snowflake.next_id();
        let s = repo
            .create(
                id,
                code,
                name,
                &zone,
                location,
                display_order,
                current.id,
                capacity,
            )
            .await
            .map_err(
                |e| match e.as_database_error().and_then(|d| d.code()).as_deref() {
                    // uk_t_shelf_code：活跃行唯一
                    Some("23505") => AppError::biz(
                        code::BIZ_SHELF_DUPLICATE_CODE,
                        format!("code '{code}' 已被占用"),
                    ),
                    _ => AppError::from(e),
                },
            )?;

        Ok(to_shelf_out(s, (capacity, 0)))
    }

    pub async fn update_shelf<R: ShelfRepoTrait>(
        &self,
        mut repo: R,
        id: i64,
        req: &ShelfUpdateRequest,
        current: &CurrentUser,
    ) -> Result<ShelfOut, AppError> {
        current.require_role(Role::Manager)?;

        let current_shelf = repo.get_by_id(id).await?.ok_or_else(shelf_not_found)?;

        // name: Some("") ⇒ 显式拒；None ⇒ 不修改
        let name_update: Option<&str> = match req.name.as_deref() {
            Some(s) => {
                let t = s.trim();
                if t.is_empty() {
                    return Err(AppError::biz(code::BIZ_INVALID_VALUE, "name 不能为空"));
                }
                Some(t)
            }
            None => None,
        };

        // location 三态：None ⇒ 不改；Some(None) ⇒ 清空；Some(Some(v)) ⇒ 改
        let new_loc_owned: Option<String>;
        let loc_update: Option<Option<&str>> = match &req.location {
            None => None,
            Some(None) => Some(None),
            Some(Some(s)) => {
                let trimmed = s.trim();
                new_loc_owned = Some(trimmed.to_string());
                Some(Some(new_loc_owned.as_deref().unwrap()))
            }
        };

        let affected = repo
            .update(
                id,
                current_shelf.version,
                name_update,
                loc_update,
                req.display_order,
                current.id,
                // 2026-10-10：三态透传（`None` 不改 / `Some(None)` 清空 /
                // `Some(Some(v))` 改值）。**`<= 0` 照样接受**（= 不限），理由同
                // `create_shelf` 的注释。
                req.capacity,
            )
            .await
            .map_err(
                |e| match e.as_database_error().and_then(|d| d.code()).as_deref() {
                    // zone CHECK（理论 service 已 catch）
                    Some("23514") => AppError::biz(
                        code::BIZ_INVALID_VALUE,
                        "zone 必须是 PRODUCTION 或 INSPECTION",
                    ),
                    _ => AppError::from(e),
                },
            )?;
        if affected == 0 {
            return Err(version_conflict());
        }

        // 回读最新行
        self.get_shelf(repo, id, current).await
    }

    /// 软删 + 停用（`is_active = false` 同时 `deleted_at = now()`）。
    /// 软删前查 `t_part_batch.current_holder_id = shelf_id` 且
    /// `status IN ('IN_PROCESS','INSPECTION')` 引用，>0 ⇒ 20503 拒。
    pub async fn soft_delete_shelf<R: ShelfRepoTrait>(
        &self,
        mut repo: R,
        id: i64,
        current: &CurrentUser,
    ) -> Result<(), AppError> {
        current.require_role(Role::Manager)?;

        let shelf = repo.get_by_id(id).await?.ok_or_else(shelf_not_found)?;

        let in_use = repo.count_in_use_parts(id).await?;
        if in_use > 0 {
            return Err(AppError::biz(
                code::BIZ_SHELF_IN_USE,
                format!(
                    "货架 {} (id={}) 仍被 {in_use} 个 IN_PROCESS/INSPECTION 零件引用，无法软删",
                    shelf.code, shelf.id
                ),
            ));
        }

        let affected = repo.soft_delete(id, shelf.version, current.id).await?;
        if affected == 0 {
            return Err(version_conflict());
        }
        Ok(())
    }
}

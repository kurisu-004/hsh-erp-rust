//! prod::scan 报工台只读聚合端点的用例层（角色守卫 + 分页 / 诊断判定 + 调 [`repo`]）
//!
//! 与 `repo.rs` 的分工同 `prod::queue::board`：`repo` 只管 SQL 与行投影，
//! service 只管「谁能看到什么」（角色白名单 + 货架 scope 收窄）与分页信封；
//! 2026-10-15 新增的 `locate`（扫码找件 · 诊断）在此之上多做一层**纯函数式的
//! action 判定**（决策表与写侧守卫的对照见 `docs/api/scan.md`）。

use sqlx::PgConnection;

use crate::auth::rbac::{CurrentUser, Role};
use crate::modules::prod::scan::dto::{HeldQuery, LocateQuery, PickableQuery};
use crate::modules::prod::scan::listing::repo::{LocateBatchRow, ScanListingRepo};
use crate::modules::prod::scan::vo::{
    HeldListOut, LocateAction, LocateBatchItem, LocateDisabledReason, LocateOut, ScanChainState,
    ScanListOut,
};
use crate::shared::error::{AppError, code};

/// 两条端点共用的角色白名单。
const SCAN_LIST_ROLES: &[Role] = &[
    Role::Manager,
    Role::Clerk,
    Role::Inspector,
    Role::ShelfAccount,
];

/// 分页缺省与上限（两条端点一致）。
const DEFAULT_LIMIT: i64 = 50;
const MAX_LIMIT: i64 = 200;

/// `pickable` 的货架 scope 谓词入参。
///
/// 语义**逐条**对齐 `auth::rbac::CurrentUser::can_access_shelf`：
/// ```text
/// shelf_wildcard || shelf_ids.contains(&shelf_id) || has_role(Role::Manager)
/// ```
/// 即「谓词对某货架恒真」的两类账号（wildcard / Manager）返回 `None`（SQL 侧不加
/// 任何谓词），其余账号返回 scope 数组走 `sh.id = ANY($n)`。
///
/// ⚠️ `CurrentUser.shelf_ids` 是 `Vec<i64>`（JSON 层才是 string 序列）。
/// ⚠️ 空 scope 返回 `Some(vec![])` 而**不是** `None`：`ANY('{}')` 对任何货架都
/// 假，与 `can_access_shelf` 对任何货架都返 false 同形。若把空数组误判成
/// 「无限制」，未绑架的 SHELF_ACCOUNT 会看到全厂。
/// 空 scope 在生产里的真实成因：`iam::service::session::resolve_roles_and_scope`
/// 会把绑到「已停用 / 已软删 / 不存在」货架的 `scope_id` 过滤掉（登录时求值），
/// 于是这类账号登录后 `shelf_ids == []` 且 `shelf_wildcard == false`。
///
/// ⚠️ **与 `shared::shelf::select::shelf_scope_for` 的分歧是刻意的，不要合并**
/// （那个函数的 doc 里已写明同一件事）：写侧那条多一条「非货架账号不限」的分支，
/// 因为 `shelf_ids` 只对 `Role::ShelfAccount` 填 —— 一个从未被授予货架范围的
/// `Inspector`，`shelf_ids` 恒为 `[]`，照本函数收窄的话它的选架 scope 恒空、
/// 送检一律 `40301`，而送检恰恰是品检员的主业。
///
/// 读侧不受影响：本函数服务的是「工人能取哪些件」，把无货架范围的 Clerk /
/// Inspector 收窄成空列表在那个语境下无害（他本来就不该替工人取件）。
///
/// ⚠️ **Clerk / Inspector 的行为后果**：本端点的角色白名单含 Clerk / Inspector，
/// 而这两类角色按惯例不配 `t_user_role` 的 SHELF_ACCOUNT 行 ⇒ `shelf_ids` 为空
/// 且 `shelf_wildcard = false` ⇒ 收口后**返回空列表**。写侧 `worker-scan` 的
/// `require_any_role(&[Manager, ShelfAccount])` 只放行 Manager/ShelfAccount，故对
/// 这两类账号不存在「列表给出但提交被拒」的落差。若业务上要放开，唯一经产品
/// API 可达的办法是给它们**逐架**配 `scope_id` 的 SHELF_ACCOUNT 行
/// （`POST /iam/users/{id}/roles`）。
fn pickable_shelf_scope(current: &CurrentUser) -> Option<Vec<i64>> {
    if current.shelf_wildcard || current.has_role(Role::Manager) {
        None
    } else {
        Some(current.shelf_ids.clone())
    }
}

/// 报工台只读聚合 service（ZST）。
pub struct ScanListingService;

impl ScanListingService {
    /// `GET /api/v2/prod/scan/pickable`：可领取件（生产架上、绑了该工种的工序）。
    ///
    /// 只读端点：handler 走 `pool.acquire()` 不开事务。
    pub async fn list_pickable(
        conn: &mut PgConnection,
        work_type_id: i64,
        query: &PickableQuery,
        current: &CurrentUser,
    ) -> Result<ScanListOut, AppError> {
        current.require_any_role(SCAN_LIST_ROLES)?;
        let limit = query.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
        let offset = query.offset.unwrap_or(0).max(0);
        let shelf_scope = pickable_shelf_scope(current);
        let items =
            ScanListingRepo::fetch_pickable(conn, work_type_id, limit, offset, shelf_scope.clone())
                .await?;
        let total = ScanListingRepo::count_pickable(conn, work_type_id, shelf_scope).await?;
        Ok(ScanListOut {
            items,
            total,
            limit,
            offset,
        })
    }

    /// `GET /api/v2/prod/scan/held`：工人当前持有件。
    ///
    /// 放回 / 送检页的**唯一**数据源，故出参比 `pickable` 多承担两层语义：
    /// 批次的工序链位置（`chain_state` 三值 + `chain_next_process_*`）、以及
    /// **容量**（`current_held` / `max_held_batches`，2026-10-14 新增，让前端能
    /// 直接显示「已拿 2/5」并在触顶前置灰取件按钮）。货架 scope 收窄不适用
    /// （行已在工人手上，不是架上的候选池）。
    pub async fn list_held(
        conn: &mut PgConnection,
        worker_id: i64,
        query: &HeldQuery,
        current: &CurrentUser,
    ) -> Result<HeldListOut, AppError> {
        current.require_any_role(SCAN_LIST_ROLES)?;
        let limit = query.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
        let offset = query.offset.unwrap_or(0).max(0);
        let items = ScanListingRepo::fetch_held(conn, worker_id, limit, offset).await?;
        // ⚠️ `total` 与 `current_held` **绑成同一个局部变量**，让「两者恒等」在代码
        // 里自证：它们是同一次 COUNT 的同一个值（都是「该工人当前持有的
        // IN_PROCESS+WORKER+未软删 批次数」）。冗余暴露 `current_held` 是为了让
        // 前端不必推断 `total` 在本端点恰好等于容量计数（见
        // `vo::listing::HeldListOut` 的字段口径 ①）。
        // ⚠️ **不要**为 `current_held` 再查一次 —— 那既多一条 SQL，又让两个字段
        // 在并发下可能出现「total 与 current_held 差 1」的诡异现象。
        let held_count = ScanListingRepo::count_held(conn, worker_id).await?;
        let max_held = ScanListingRepo::fetch_max_held_batches(conn, worker_id).await?;
        Ok(HeldListOut {
            items,
            total: held_count,
            limit,
            offset,
            current_held: held_count,
            max_held_batches: max_held,
        })
    }

    /// `GET /api/v2/prod/scan/locate?serial_no=&worker_id=`（2026-10-15 新增）。
    ///
    /// 扫图纸序列号 → 该 part **全部未软删批次**（含终态）的位置事实；传了
    /// `worker_id` 时再对每批给出「该工人能做什么」的 `action` / 不能做的
    /// `disabled_reason`。**纯读**：handler 走 `pool.acquire()` 不开事务、不发 WS。
    ///
    /// ## 命中与错误
    /// - `serial_no.trim()` 为空 / `t_part` 查不到（**不回退 `t_assembly`**）→
    ///   `20101 BIZ_PART_NOT_FOUND`（HTTP 404），文案提示装配件要扫子件；
    /// - `worker_id` 传了但查不到（含已软删）→ `20201 BIZ_WORKER_NOT_FOUND`（404）；
    ///   存在但 `is_active=false` → `20202 BIZ_WORKER_INACTIVE`（400）—— 与写路径
    ///   `prod::scan::service::pick_up` 的 worker 校验**逐条同形**：诊断端点不能
    ///   诊断出一个「点了就会失败」的按钮；
    /// - 未传 `worker_id` → `action` / `disabled_reason` 均为 `null`（纯位置事实）。
    ///
    /// ## SQL 条数（≤ 4，与批次数无关）
    /// part 1 + worker/工种上限 1（仅传了 worker_id）+ held 计数 1（复用 `/held`
    /// 的 `count_held`）+ 批次取行 1。登记在 `docs/api/scan.md` §5。
    ///
    /// ## action 判定（决策表见 `docs/api/scan.md`，判定函数见下方自由函数）
    /// 判定顺序与写侧「先资格后容量」同序：**状态类原因 → 工种未绑定 → 工序 ∉
    /// 工种 → 容量**。`MINE`（件已在本工人手上）**不受任何闸门影响** —— 容量触顶、
    /// 工种未绑定都不改变「这是我手上的件」这个事实。
    pub async fn locate(
        conn: &mut PgConnection,
        query: &LocateQuery,
        current: &CurrentUser,
    ) -> Result<LocateOut, AppError> {
        current.require_any_role(SCAN_LIST_ROLES)?;
        // ⚠️ `trim()` 后为空按**未命中**处理（不是 400）：扫到空白码时前端要的是
        // 「这个码没东西」，与 `prod::inspection` 的扫码口径一致。
        let serial_no = query.serial_no.trim();
        if serial_no.is_empty() {
            return Err(AppError::biz(
                code::BIZ_PART_NOT_FOUND,
                "serial_no 为空，无法定位",
            ));
        }
        let part = ScanListingRepo::fetch_locate_part(conn, serial_no)
            .await?
            .ok_or_else(|| {
                AppError::biz(
                    code::BIZ_PART_NOT_FOUND,
                    format!("未找到序列号 {serial_no} 的工单；如为装配件，请扫子件序列号"),
                )
            })?;
        // 工人上下文只在传了 worker_id 时构建：未传 ⇒ 纯位置事实，action 恒 null。
        let worker_ctx = match query.worker_id {
            None => None,
            Some(worker_id) => {
                let row = ScanListingRepo::fetch_locate_worker(conn, worker_id)
                    .await?
                    .ok_or_else(|| {
                        AppError::biz(
                            code::BIZ_WORKER_NOT_FOUND,
                            format!("worker {worker_id} 不存在"),
                        )
                    })?;
                if !row.is_active {
                    return Err(AppError::biz(
                        code::BIZ_WORKER_INACTIVE,
                        format!("worker {worker_id} 已停用"),
                    ));
                }
                // 容量计数：**复用 `/held` 的同一条 `count_held`**，判据与写侧
                // `QueueService::ensure_worker_has_capacity` 的 held 完全一致
                // （`IN_PROCESS + WORKER + 未软删`）。
                let held = ScanListingRepo::count_held(conn, worker_id).await?;
                Some(LocateWorkerCtx {
                    worker_id,
                    work_type_id: row.work_type_id,
                    max_held_batches: row.max_held_batches,
                    held,
                })
            }
        };
        // 工种 id 只用于两个 EXISTS 投影；未传 worker_id / 工人未绑工种时绑 NULL
        // （`wtp.work_type_id = NULL` 恒假 ⇒ 投影 false，判定不会消费它们）。
        let work_type_id = worker_ctx.as_ref().and_then(|ctx| ctx.work_type_id);
        let rows = ScanListingRepo::fetch_locate_batches(conn, part.id, work_type_id).await?;
        let batches = rows
            .into_iter()
            .map(|row| into_locate_item(row, worker_ctx.as_ref()))
            .collect();
        Ok(LocateOut {
            part: part.into_part_brief(),
            batches,
        })
    }
}

/// `locate` 的工人上下文（一次 `fetch_locate_worker` + 一次 `count_held` 的产物）。
struct LocateWorkerCtx {
    /// 用于判 `IN_PROCESS + WORKER` 批次是不是「我持有的」。
    worker_id: i64,
    /// `NULL` = 未绑工种（除 `MINE` 外一切动作落 `WORK_TYPE_NOT_SET`）。
    work_type_id: Option<i64>,
    /// `NULL` = 工种未配上限（写侧 `20904`，读侧落 `WORK_TYPE_MAX_HELD_NOT_SET`）。
    max_held_batches: Option<i32>,
    /// 该工人当前持有批次数（与写侧容量守卫同一谓词）。
    held: i64,
}

/// 状态 / 位置组合 → 候选动作（不含任何闸门）。
///
/// 与决策表逐行对应；**不在表内的组合一律落 `Disabled(Unknown)`**（保守出口：
/// 宁可不给按钮，也不给一个写侧不认的按钮）。
enum LocateClass {
    /// `PENDING`（未下发；写侧 = S2-B 的链首落位流，无链则两列 NULL）。
    Pending,
    /// `IN_PROCESS + PRODUCTION_SHELF`（架上可领，写侧 pick-up 的 IN_PROCESS 分支）。
    Shelf,
    /// `IN_PROCESS + WORKER` 且持有人是该工人。
    Mine,
    /// `IN_PROCESS + WORKER` 且持有人是**非空**的别的工人（S2 接手；holder 为 NULL
    /// 的脏形态落 `Disabled(Unknown)`，见 `classify_batch`）。
    TakeOver,
    /// `OUTSOURCE + OUTSOURCE_COMPANY`（S3 外协收回）。
    Outsource,
    /// 状态类原因（写侧没有对应按钮，只有指引文案）。
    Disabled(LocateDisabledReason),
}

/// 分类（`worker_id` 仅用于 `Mine` / `TakeOver` 的分叉）。
fn classify_batch(row: &LocateBatchRow, worker_id: i64) -> LocateClass {
    match row.status.as_str() {
        "PENDING" => LocateClass::Pending,
        "IN_PROCESS" => match row.location.as_deref() {
            Some("PRODUCTION_SHELF") => LocateClass::Shelf,
            Some("WORKER") if row.current_holder_id == Some(worker_id) => LocateClass::Mine,
            // 2026-10-15 S2 订正：`location='WORKER'` 但 holder 为 NULL 是**数据异常**
            // （任何写路径都不会产出这种形态），写侧 `take_over` 对同形态拒 `20103`
            // （「挂在别人手上」是接管的前提，从 Nobody 手上无法接管）⇒ 读侧不给
            // `TAKE_OVER`（否则是「写侧会拒绝的却显示可点按钮」）。落保守出口
            // `UNKNOWN`：出现即说明数据需要人工对账。
            Some("WORKER") if row.current_holder_id.is_none() => {
                LocateClass::Disabled(LocateDisabledReason::Unknown)
            }
            Some("WORKER") => LocateClass::TakeOver,
            _ => LocateClass::Disabled(LocateDisabledReason::Unknown),
        },
        "OUTSOURCE" => match row.location.as_deref() {
            Some("OUTSOURCE_COMPANY") => LocateClass::Outsource,
            _ => LocateClass::Disabled(LocateDisabledReason::Unknown),
        },
        "PROGRAMMING" => LocateClass::Disabled(LocateDisabledReason::Programming),
        "INSPECTION" => LocateClass::Disabled(LocateDisabledReason::Inspection),
        "READY_TO_SHIP" | "DELIVERED" | "COMPLETED" | "CANCELLED" => {
            LocateClass::Disabled(LocateDisabledReason::Terminal)
        }
        _ => LocateClass::Disabled(LocateDisabledReason::Unknown),
    }
}

/// 链状态原始文本 → 枚举（未知取值降级 `NONE`，与 held 侧同款保守方向）。
fn chain_state_of(row: &LocateBatchRow) -> ScanChainState {
    row.chain_state
        .as_deref()
        .map_or(ScanChainState::None, ScanChainState::from_db_text)
}

/// 容量闸门：与写侧 `QueueService::ensure_worker_has_capacity` **同一判据、同一
/// 顺序** —— 工种未配 `max_held_batches`（`NULL` = 未配置）先于「持有数触顶」。
fn capacity_gate(ctx: &LocateWorkerCtx) -> Option<LocateDisabledReason> {
    match ctx.max_held_batches {
        None => Some(LocateDisabledReason::WorkTypeMaxHeldNotSet),
        Some(max) if ctx.held >= i64::from(max) => {
            Some(LocateDisabledReason::WorkerHoldLimitExceeded)
        }
        Some(_) => None,
    }
}

/// `PENDING` 的闸门：链首判定（写侧 S2 的对位）→ 容量。
///
/// 「链不可解析 / 链内无活跃 step ⇒ `PROCESS_CHAIN_STEP_NOT_FOUND`（写侧 `20702`）」
/// 读 `has_usable_chain_head` —— 它与写侧 PENDING 落位用的
/// `ProcessChainRepo::first_step_in_chain` **逐谓词对齐**（链行未软删 + 至少一个
/// 未软删 step）。
///
/// ⚠️ **2026-10-15 S2-B 订正**：此前读 `has_process_chain`
/// （`HAS_PROCESS_CHAIN_EXPR` 投影），该表达式**不带链行软删闸门** ⇒
/// 「链行已软删但 step 仍活跃」的脏数据形态读侧给 `PICK_UP`，而写侧
/// `first_step_in_chain` 返 `None`（`20702`）必拒 —— 读放行、写必拒的分叉。
/// 链首存在后再判它是否命中工种的活跃工序映射（对位写侧 S2 的 `20104`）。
///
/// ⚠️ **残余失败面（登记在 `docs/api/scan.md` §8）**：本闸门与 S2-B 写侧
/// （`resolve_pending_chain_head`）已逐谓词对齐，PENDING 形态**不再有**「读侧放行、
/// 写侧必拒」的确定性组合；仅剩两类非确定形态：① 诊断与执行之间数据被并发改动；
/// ② 「链问题 + 容量问题」并存时本函数先报链类 reason、写侧按既有优先级先报容量类
/// 错误（两侧都拒，只是 reason 不同）。
fn pending_gate(row: &LocateBatchRow, ctx: &LocateWorkerCtx) -> Option<LocateDisabledReason> {
    if row.has_chain {
        if !row.has_usable_chain_head {
            return Some(LocateDisabledReason::ProcessChainStepNotFound);
        }
        if !row.chain_head_in_work_type {
            return Some(LocateDisabledReason::ProcessNotInWorkType);
        }
    }
    capacity_gate(ctx)
}

/// `TAKE_OVER` 的闸门：仅当**推导得出下一道**（`chain_state == NEXT` 且指针与
/// 当前工序一致）才校验「下一道 ∈ 我工种映射」（对位写侧 S2 派生分支的 `20104`）；
/// 推导不出（无链 / 指针漂移 / 链内歧义）**仍给 `TAKE_OVER`** —— 前端弹手填下一道，
/// 残余失败组合登记在 `docs/api/scan.md` §8。最后过容量。
fn take_over_gate(row: &LocateBatchRow, ctx: &LocateWorkerCtx) -> Option<LocateDisabledReason> {
    if chain_state_of(row) == ScanChainState::Next
        && row.pointer_consistent.unwrap_or(false)
        && !row.next_process_in_work_type
    {
        return Some(LocateDisabledReason::ProcessNotInWorkType);
    }
    capacity_gate(ctx)
}

/// 外协收回的闸门：**有链且 `NEXT`** ⇒ 校验「下一道 ∈ 我工种映射」（对位 S3 写侧
/// `POST /scan/outsource-receive` 的 `20104`）；**其余形态（无链 / 有链但推不出
/// `NEXT`）一律放行** —— 写侧对这两种形态走「前端手填 `next_process_id`」的流
/// （无链时写侧缺省必拒 `40001`、有链推不出按其 `20706` 文案拒），前端弹手填即可。
///
/// ⚠️ **2026-10-15 S3 订正（假阻挡）**：此前「有链但推不出 NEXT」返回
/// `PROCESS_CHAIN_REQUIRED`（`NONE`），但写侧 `receive_from_outsource_inner` 的
/// `next_process_id` 显式分支**原样放行**、其 `20706` 文案本身就写「请手填」——
/// 即会点成功的操作被读侧藏掉了按钮。该 reason 变体已随之删除。
///
/// `next_process_in_work_type` 投影的谓词是「**链内推导出的下一道** ∈ 工种映射」，
/// 只在 `NEXT` 形态下被消费。最后过容量。
fn outsource_gate(row: &LocateBatchRow, ctx: &LocateWorkerCtx) -> Option<LocateDisabledReason> {
    if row.has_chain
        && chain_state_of(row) == ScanChainState::Next
        && !row.next_process_in_work_type
    {
        return Some(LocateDisabledReason::ProcessNotInWorkType);
    }
    capacity_gate(ctx)
}

/// 单批次的 action 判定（纯函数：只读行事实与工人上下文）。
///
/// 判定优先级（与写侧「先资格后容量」同序）：
/// 1. 未传 `worker_id` ⇒ `(None, None)`（纯位置事实）；
/// 2. `Mine` ⇒ `MINE` 且无 reason（**不受任何闸门影响**）；
/// 3. 状态类原因（`PROGRAMMING` / `INSPECTION` / `TERMINAL` / `UNKNOWN`）⇒ 原文；
/// 4. 工人未绑工种 ⇒ `WORK_TYPE_NOT_SET`（**先于**工序 / 容量闸门）；
/// 5. 各形态的工序闸门 → 容量闸门（见上三个 `*_gate`）。
fn decide_action(
    row: &LocateBatchRow,
    ctx: Option<&LocateWorkerCtx>,
) -> (Option<LocateAction>, Option<LocateDisabledReason>) {
    let Some(ctx) = ctx else {
        return (None, None);
    };
    match classify_batch(row, ctx.worker_id) {
        LocateClass::Mine => (Some(LocateAction::Mine), None),
        LocateClass::Disabled(reason) => (None, Some(reason)),
        class => {
            // 状态类已短路，剩下的 4 个都是「写侧有对应动作」的形态。
            if ctx.work_type_id.is_none() {
                return (None, Some(LocateDisabledReason::WorkTypeNotSet));
            }
            let (action, gate) = match class {
                LocateClass::Pending => (LocateAction::PickUp, pending_gate(row, ctx)),
                LocateClass::Shelf => (LocateAction::PickUp, capacity_gate(ctx)),
                LocateClass::TakeOver => (LocateAction::TakeOver, take_over_gate(row, ctx)),
                LocateClass::Outsource => {
                    (LocateAction::ReceiveFromOutsource, outsource_gate(row, ctx))
                }
                LocateClass::Mine | LocateClass::Disabled(_) => {
                    unreachable!("Mine / Disabled 已在上层分支短路")
                }
            };
            match gate {
                Some(reason) => (None, Some(reason)),
                None => (Some(action), None),
            }
        }
    }
}

/// 行事实 + action 判定 → 出参行（穷尽 struct 字面量：加字段时这里编译失败）。
fn into_locate_item(row: LocateBatchRow, ctx: Option<&LocateWorkerCtx>) -> LocateBatchItem {
    let (action, disabled_reason) = decide_action(&row, ctx);
    // 先算完所有派生值（`chain_state_of` 借用 row），再逐字段 move。
    let chain_state = chain_state_of(&row);
    LocateBatchItem {
        batch_id: row.batch_id,
        batch_no: row.batch_no,
        quantity: row.quantity,
        status: row.status,
        version: row.version,
        location: row.location,
        holder_display: row.holder_display,
        current_process_name: row.current_process_name,
        chain_state,
        next_process_id: row.next_process_id,
        next_process_name: row.next_process_name,
        updated_at: row.updated_at,
        company_name: row.company_name,
        sent_at: row.sent_at,
        action,
        disabled_reason,
    }
}

#[cfg(test)]
mod tests {
    //! `pickable_shelf_scope` 与 `shared::shelf::select::shelf_scope_for` 的
    //! **分歧**必须被钉住 —— 它们长得几乎一样（前半段逐条相同），而合并是错的。
    //!
    //! 两者的差异只有一条：Clerk / Inspector（无 SHELF_ACCOUNT 角色行 ⇒
    //! `shelf_ids` 恒空）在本函数下得到 `Some(vec![])`（看不见任何架），
    //! 在选架函数下得到 `None`（不限）。写侧必须不限，否则品检员一律送不了检。
    use super::*;

    /// 造一个带指定角色 / 货架范围的 `CurrentUser`。
    fn mk(roles: Vec<Role>, wildcard: bool, ids: Vec<i64>) -> CurrentUser {
        CurrentUser {
            id: 1,
            username: "u".into(),
            roles,
            shelf_wildcard: wildcard,
            shelf_ids: ids,
            permissions: Vec::new(),
        }
    }

    #[test]
    fn clerk_and_inspector_get_empty_scope_on_read_side() {
        for role in [Role::Clerk, Role::Inspector] {
            assert_eq!(
                pickable_shelf_scope(&mk(vec![role], false, vec![])),
                Some(vec![]),
                "{role:?} 在读侧必须收窄成空 scope（看不见任何架）"
            );
        }
    }

    #[test]
    fn shelf_account_scope_is_passed_through_verbatim() {
        assert_eq!(
            pickable_shelf_scope(&mk(vec![Role::ShelfAccount], false, vec![7, 9])),
            Some(vec![7, 9])
        );
    }

    #[test]
    fn manager_and_wildcard_are_unrestricted() {
        assert_eq!(
            pickable_shelf_scope(&mk(vec![Role::Manager], false, vec![])),
            None
        );
        // wildcard 且带受限 id：仍是不限（wildcard 优先）
        assert_eq!(
            pickable_shelf_scope(&mk(vec![Role::ShelfAccount], true, vec![7])),
            None
        );
    }

    /// 与写侧的分歧：同一个 Clerk，在选架层**不受**收窄。
    #[test]
    fn diverges_from_write_side_shelf_scope_on_purpose() {
        use crate::shared::shelf::select::shelf_scope_for;
        for role in [Role::Clerk, Role::Inspector] {
            let user = mk(vec![role], false, vec![]);
            assert_eq!(pickable_shelf_scope(&user), Some(vec![]), "读侧收窄");
            assert_eq!(
                shelf_scope_for(&user),
                None,
                "写侧不限 —— 这条分歧是刻意的（见 pickable_shelf_scope 的 doc），\
                 把它改成与读侧一致会让品检员一律送不了检"
            );
        }
    }
}

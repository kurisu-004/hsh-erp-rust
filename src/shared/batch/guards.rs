//! 跨域批次守卫自由函数：状态机守卫 / OCC / 货架校验 / 批次状态写入口薄包装。
//!
//! 2026-10-08 自 `prod::batch::service::guard` 上移到 shared 层：判序（存在 →
//! 停用 → zone）与文案是全仓一份的公共语义，任何碰批次状态的域都要用，留在
//! batch 域等于让所有域反向依赖它。
//!
//! 这里是 part → prod 依赖的反向证明：这层守卫只经 `shared::batch::status` 写
//! `t_part_batch.status`，不引用 part 域任何 service / vo。
//!
//! 2026-10-04 起本文件不再只服务 `prod::batch`：`prod::shelf_process`（建映射时的 zone
//! 守卫）与 `prod::queue`（WORKER→POOL 放回时的货架守卫）两个**同域**跨模块
//! caller 也调 [`validate_shelf_zone`]。新增 caller 时**必须**调本函数而不要复制判序 ——
//! 判序（20501 存在 → 20512 停用 → 20104 zone）与文案只有一份，是 2026-10-04 那次
//! 「`current_holder_id` 写脏」修复的核心：3 个写点各写各的守卫时，其中一个漏了
//! `t_shelf` 侧谓词就足以让批次落到品检架上并从此静默漏件。
//!
//! 2026-10-04 review 第 1 轮 I1：**不要**给 `prod::batch::service::worker_scan` 的
//! RETURNED 分支再加一道货架守卫。它在 `worker_scan_event` 的第 1 步（`event_type`
//! 分支**之前**）已经用 `ShelfRepo::get_by_id_zone(req.shelf_id, "PRODUCTION")` 一步
//! 守掉存在 / 软删 / 停用 / zone 四个谓词（`shelf::repo::sql` 的 WHERE 是
//! `id=$1 AND zone=$2 AND is_active=true AND deleted_at IS NULL`），而写进
//! `current_holder_id` 的正是同一个 `req.shelf_id`。
//!
//! `current_holder_id`（= 货架）的写点全仓恰好 3 个，本批全部覆盖、无遗留：
//! ① `dispatch_single`（`update_batch_dispatched`，货架由
//! `find_first_shelf_for_process` 从映射里**选出** ⇒ 谓词下沉到该方法的 SQL）；
//! ② `move_batch` WORKER→POOL（`prod::queue` 的 move 端点，货架来自请求 ⇒
//! `validate_shelf_zone`）；③ `worker_scan` RETURNED（货架来自请求 ⇒ 已有的
//! `get_by_id_zone`）。改任一处都请先回到这条清单核对。

use sqlx::PgConnection;

use crate::modules::part::statemachine::PartStatus;
use crate::shared::batch::status::{apply_batch_status_change, StatusChange};
use crate::modules::shelf::repo::ShelfRepo;
use crate::shared::error::{AppError, code};

#[derive(sqlx::FromRow)]
pub struct InspectionRepairRow {
    pub batch_id: i64,
    pub part_id: i64,
    pub batch_no: i32,
    pub quantity: i32,
    pub status: String,
    /// 2026-10-01 review 第 1 轮 M5 新增（migration 005）：返修标记随列表投出
    pub is_repairing: bool,
    pub location: Option<String>,
    pub version: i32,
    pub current_process_step_id: Option<i64>,
    pub parent_batch_id: Option<i64>,
    pub current_holder_id: Option<i64>,
    pub holder_name: Option<String>,
    pub next_process_id: Option<i64>,
    pub next_process_name: Option<String>,
    pub delivery_note_id: Option<i64>,
    pub delivery_note_no: Option<String>,
    pub serial_no: Option<String>,
    pub drawing_no: String,
    pub name: String,
    pub order_no: Option<String>,
    pub planned_delivery_date: chrono::NaiveDate,
    pub is_urgent: bool,
    pub part_version: i32,
    pub created_at: chrono::NaiveDateTime,
    pub updated_at: chrono::NaiveDateTime,
    pub customer_id: i64,
    pub customer_name: Option<String>,
    pub l1_customer_name: Option<String>,
}

/// 状态机迁移守卫 + 错误码映射（在 phase1.rs 内复用）：
/// - 起点状态非法 → 20103 `BIZ_INVALID_TRANSITION`
/// - 起点已是终态 → 20115 `BIZ_PART_ALREADY_CANCELLED`（仅 cancel 路径）
#[inline]
pub fn ensure_transition(
    from: PartStatus,
    to: PartStatus,
    ctx: &str,
) -> Result<(), AppError> {
    if from == PartStatus::CANCELLED {
        return Err(AppError::biz(
            code::BIZ_PART_ALREADY_CANCELLED,
            format!("{ctx}: 工单已 CANCELLED"),
        ));
    }
    if !from.can_transition_to(to) {
        return Err(AppError::biz(
            code::BIZ_INVALID_TRANSITION,
            format!("{ctx}: {} → {} 不允许", from.as_str(), to.as_str()),
        ));
    }
    Ok(())
}

/// 锚定 `version` 的 caller 侧乐观锁守卫（OCC）。
///
/// 2026-10-02：原 `validate_batch_ownership` 还兼做「batch 属于 part」断言，
/// 该断言随 `part_id` 路径参数退场（part_id 改由批次行反查）而恒真，已删 ——
/// 批次 id 全局唯一即锚点，不存在「跨 part 批次」这一场景。函数随之更名为
/// `validate_batch_version` 并去掉两个 part 形参。
#[inline]
pub fn validate_batch_version(
    batch_id: i64,
    expected_version: i32,
    actual_version: i32,
) -> Result<(), AppError> {
    if batch_version_mismatch(batch_id, expected_version, actual_version) {
        return Err(AppError::biz(
            code::VERSION_CONFLICT,
            format!("batch {batch_id} 版本冲突（期望 {expected_version}，实际 {actual_version}）"),
        ));
    }
    Ok(())
}

#[inline]
pub fn batch_version_mismatch(_batch_id: i64, expected: i32, actual: i32) -> bool {
    expected != actual
}

/// 校验 shelf 存在 + active + zone 一致。
pub async fn validate_shelf_zone(
    conn: &mut PgConnection,
    shelf_id: i64,
    expected_zone: &str,
) -> Result<(), AppError> {
    let shelf = ShelfRepo::get_by_id(&mut *conn, shelf_id)
        .await?
        .ok_or_else(|| {
            AppError::biz(
                code::BIZ_SHELF_NOT_FOUND,
                format!("shelf {shelf_id} 不存在"),
            )
        })?;
    if !shelf.is_active {
        return Err(AppError::biz(
            code::BIZ_SHELF_INACTIVE,
            format!("shelf {} (id={}) 已停用", shelf.code, shelf.id),
        ));
    }
    if shelf.zone != expected_zone {
        return Err(AppError::biz(
            code::BIZ_INVALID_VALUE,
            format!(
                "shelf {} (id={}) zone={} 不等于 {expected_zone}",
                shelf.code, shelf.id, shelf.zone
            ),
        ));
    }
    Ok(())
}

/// 校验 shelf ↔ process 映射（`_assert_shelf_maps_process`）：必须存在
/// `t_shelf_process` 映射行，否则 20507 `BIZ_SHELF_PROCESS_NOT_MAPPED`。
pub async fn assert_shelf_maps_process(
    conn: &mut PgConnection,
    shelf_id: i64,
    process_id: i64,
) -> Result<(), AppError> {
    let exists: Option<(i64,)> = sqlx::query_as(
        "SELECT id FROM t_shelf_process WHERE shelf_id = $1 AND process_id = $2 \
         AND deleted_at IS NULL LIMIT 1",
    )
    .bind(shelf_id)
    .bind(process_id)
    .fetch_optional(&mut *conn)
    .await?;
    if exists.is_none() {
        return Err(AppError::biz(
            code::BIZ_SHELF_PROCESS_NOT_MAPPED,
            format!("shelf {shelf_id} 未映射 process {process_id}"),
        ));
    }
    Ok(())
}
/// 2026-09-16 PR-3 批次 step 化：
/// - 删 `placed_at` 列写入（COALESCE(placed_at, now()) 已无意义）
/// - `next_process_id: Option<i64>` → `current_process_step_id: Option<i64>`
///   （写入 t_part_batch.current_process_step_id 新列）
/// - `new_next_process_id` 参数改名为 `new_current_process_step_id`
///   （DTO / worker / frontend 仍传 process_id，由 caller 在调本函数前
///   经 `ProcessChainRepo::resolve_step_id_by_process` 解析）
///
/// 2026-09-30 新增第 8 个 bind 参数 `new_current_process_id: Option<i64>`，
/// 写入 `t_part_batch.current_process_id`。两者语义**必须分清**：
/// - `new_current_process_id` —— **池归属的权威依据**（逻辑 FK → t_process.id）。
///   写入不变式：进池（`status='IN_PROCESS'` + `location='PRODUCTION_SHELF'`）
///   写目标 `process_id`；出池（转 PENDING / INSPECTION / INSPECTION_SHELF）
///   传 `None`；池内移动不经过本函数，故无「不动」分支。
/// - `new_current_process_step_id` —— **可选的显示用定位信息**（逻辑 FK →
///   t_process_chain_step.id），仅当工单已绑定工序链时才写，允许 NULL。
///   NULL 不影响入池（旧设计的死状态已由 `current_process_id` 打破）。
///   ⚠️ 措辞（2026-09-30 review 第 3 轮附带发现）：该列**只在首次定位工序时写、
///   之后不再推进**（worker-scan RETURNED / 送检都不写），故**不是**「当前走到
///   第几步」的进度指针。
///
/// 写入不变式第 4 行「非生产流 → NULL」的**唯一例外**（2026-09-30 review 第 1 轮
/// M1 补记，勿按表机械核对后误判为 bug）：
///
/// `outsource::send_to_outsource`（`status='OUTSOURCE'` +
/// `location='OUTSOURCE_COMPANY'`）传 `Some(req.process_id)` 而非 NULL。三条理由：
///
/// 1. 外协加工的就是这道工序，rollup 派生 `t_part.next_process_id` 需要它；
/// 2. 与本次改动**前**的行为一致（旧代码写 `Some(step_id)`，rollup 再翻成
///    process_id），不写才是行为变更；
/// 3. `status='OUTSOURCE'` + `location='OUTSOURCE_COMPANY'` 使其**不可能**被任何
///    工序池查询命中（4 条池 SQL 与 `list_pickable_by_work_type` 均硬限定
///    `status='IN_PROCESS'` 叠加 `location='PRODUCTION_SHELF'`）。
///
/// 初始批次 / 子批次仍为严格 NULL（见 `prod/batch/repo/queries.rs::create_initial_batch`
/// 与 `part/repo/sql/part_sql.rs::insert_child_for_assembly`，两者都不写该列）。
///
/// 2026-10-01：改为 `apply_batch_status_change` 的薄包装
/// （全仓唯一的 `t_part_batch.status` 写入口）。
///
/// 语义映射（保持与改造前逐条等价）：
/// - WHERE 的 `status NOT IN ('CANCELLED','COMPLETED')` 换成写入口的
///   **正向** `allowed_from` 白名单（= 全状态减两个终态）。之所以改成正向：
///   写入口的白名单是「本次允许的源状态」，反向排除无法直接表达，
///   而正向表多写 9 个状态值换来的是「新增状态时若忘了加进白名单会被拒」
///   ——fail-safe 方向正确。
/// - 其余 3 个可选列的 `None` 在原语义里**同样是「写 NULL」**（SQL 是
///   `location = $4, current_holder_id = $5, current_process_step_id = $6`），
///   而写入口的 `None` 是「保持原值」。故本包装函数对 4 列一律
///   `clear_*: 形参.is_none()`，把「传 None = 清 NULL」这一**既有约定**如实
///   翻译过去。
///
///   ⚠️ 2026-10-01 review 第 1 轮 M2：上一轮（本改造的首版）只给
///   `clear_process_id` 做了翻译，并在注释里断言「全部 8 个调用点传的
///   location / holder / step 均非 None 或与保持原值等价」——**该断言是错的**，
///   实际有 5 个调用点靠 `None` 表达「清空」，被误改成「保持原值」后：
///   - `shelf::recall_to_pending`（`None,None,None,None`）：
///     PENDING 批次仍留着 `location='PRODUCTION_SHELF'` + `current_holder_id`
///     + 陈旧 step，UI 上「待投产」的工单显示还压在生产架上；
///   - `outsource::receive_from_outsource_to_inspection`（外协收回 → INSPECTION）、
///     `scan::scan_inspect`（第一步）、`repair::complete_repair` /
///     `repair::repair_dispatch` 的 INSPECTION 分支：
///     `current_process_step_id` 不再清，而展示用的 `next_process_id` 正是由它
///     经 `t_process_chain_step` JOIN 派生 → 出池批次显示上一道工序。
///
/// ## step 列的 10 个调用点里，**10 个都可能传 `None`**（2026-10-03 review 第 2 轮订正）
/// 逐点核对后的现状（改任一处 step 形参都要连带复核本表）：
///
/// | 调用点 | step 形参 | 何时为 `None` ⇒ 走 clear 分支 |
/// |---|---|---|
/// | `shelf::place_on_shelf` | `optional_step_id(..)` | 无链 |
/// | `programming::release_from_programming` | `optional_step_id(..)` | 无链 |
/// | `outsource::send_to_outsource` | `optional_step_id(..)` | 无链 |
/// | `outsource::receive_from_outsource` | `optional_step_id(..)` | 无链 |
/// | `repair::complete_repair` | `step_id_opt` | 无链（PRODUCTION 分支）/ 恒 `None`（INSPECTION 分支） |
/// | `repair::repair_dispatch` | `step_id_opt` | 无链（PRODUCTION 分支）/ 恒 `None`（INSPECTION 分支） |
/// | `shelf::recall_to_pending` | 字面 `None` | 恒 `None` |
/// | `outsource::receive_from_outsource_to_inspection` | 字面 `None` | 恒 `None` |
/// | `scan::scan_inspect`（第一步） | 字面 `None` | 恒 `None` |
/// | `pickup`（work_type pick-up） | `batch.current_process_step_id`（读回） | 批次行该列本来就是 NULL（无链零件）⇒ 传 `None`，与 clear 等价（目标列已 NULL，无副作用） |
///
/// 即 **6 处生产流端点的 step 列在「无链」时为 `None` 而走 clear 分支**（这 6 处是
/// 2026-10-03 起工序链从「必须」放宽为 `optional_process_chain` / `optional_step_id`
/// 的直接后果，链可选项化后「无链」从 20706 拒收变成放行 + 落
/// NULL），另 3 处恒传 `None`（纯出池路径），只有 work_type pick-up 是把**读回值**
/// 原样传下去 —— 有链时保持定位信息不丢，无链时恰好等价于清 NULL。
///
/// 改任何一处的 step 形参，都要连带复核上表：把该传 `Some(..)` 的地方改成 `None`
/// 会静默清掉定位信息，把该传 `None` 的地方改成 `Some(..)` 会留下陈旧 step。
#[allow(clippy::too_many_arguments)]
pub async fn mark_batch_with_status_and_meta(
    conn: &mut PgConnection,
    batch_id: i64,
    expected_version: i32,
    new_status: &str,
    new_location: Option<&str>,
    new_holder_id: Option<i64>,
    new_current_process_step_id: Option<i64>,
    new_current_process_id: Option<i64>,
    updated_by: i64,
) -> Result<u64, AppError> {
    apply_batch_status_change(
        conn,
        StatusChange {
            batch_id,
            new_status,
            new_location,
            new_holder_id,
            new_process_id: new_current_process_id,
            new_process_step_id: new_current_process_step_id,
            // 2026-10-01：本漏斗 = 「批次离开返修态」的清零点。
            //
            // 规则依据：设 `is_repairing=true` 的入口只有 2 个 ——
            // `mark_batch_repairing`（start-repair，只置标记不改 status）与
            // `mark_batch_status_only`（scan-inspect FAIL，形参
            // `is_repairing = Some(true)`），二者都**不走**本漏斗的返修分支；
            // 本漏斗的 10 个调用点（place_on_shelf / recall_to_pending /
            // release_from_programming / outsource 收发 ×3 / complete_repair /
            // repair_dispatch / scan-inspect 第一步 / work_type pick-up）全部表示
            // 「批次回到了正常生产流 / 送检 / 完成返修」，此刻必须清标记，否则返修态
            // 永远挂着。其中 `complete_repair` 与 `repair_dispatch` 正是最关键的两个
            // 清除点（前者=返修完成，后者=一步式起修并直接到位）。
            is_repairing: Some(false),
            expected_version: Some(expected_version),
            // 终态不可流转：COMPLETED / CANCELLED 之外的全部状态。
            //
            // 末尾的 "REPAIRING" 是**过渡期源状态别名**：REPAIRING 已从
            // PartStatus 删除，但 migration 006 之前的存量行、以及
            // 直接写库的历史数据仍可能是该值。把它列进白名单是
            // `PartStatus::from_str` 里 `"REPAIRING" => IN_PROCESS`
            // 兼容分支在 SQL 层的对应物（两侧容忍度必须一致，否则会出现
            // 「service 层放行、SQL 守卫拒掉」的诡异 409）。
            allowed_from: &[
                "PENDING",
                "PROGRAMMING",
                "IN_PROCESS",
                "INSPECTION",
                "READY_TO_SHIP",
                "DELIVERED",
                "OUTSOURCE",
                "REPAIRING",
            ],
            updated_by,
            // 2026-10-01 review 第 1 轮 M2：本包装函数沿用「形参 None = 写 NULL」
            // 的**既有约定**（改造前 SQL 是 4 列直写），故 4 列一律
            // `clear_* = 形参.is_none()`，把旧语义如实翻译进写入口的
            // 「None = 保持原值」三态模型。理由与受影响调用点清单见本函数 doc。
            clear_location: new_location.is_none(),
            clear_holder_id: new_holder_id.is_none(),
            clear_process_id: new_current_process_id.is_none(),
            clear_process_step_id: new_current_process_step_id.is_none(),
            // 本漏斗的目标状态没有一个是终态（COMPLETED 走
            // `mark_batch_completed`、CANCELLED 走 `mark_batch_status_only`），
            // 故永远不会触发 step 4 的终态序列号归档。
            event_id: None,
        },
    )
    .await
    .map(|_| 1u64)
}

/// mark_batch 的轻量版本（不写 location/holder/process；用于状态机迁移但保持原 holder 的场景，如 CANCELLED）。
///
/// 2026-10-01：改为 `apply_batch_status_change` 的薄包装。
///
/// 3 个调用点传入的目标状态是 `READY_TO_SHIP`（scan-inspect pass）/
/// `IN_PROCESS`（scan-inspect FAIL，**同时置 `is_repairing=true`**）/
/// `CANCELLED`（cancel-batch）。返修标记由形参 `is_repairing` 显式传入，
/// 本函数不做任何别名翻译 —— 上一轮为兼容老调用点留的
/// `resolve_status_alias("REPAIRING")` 过渡分支已在本轮删除（REPAIRING 已从
/// `PartStatus` 删除，DB 层也不再产生该 status，别名只会让读代码的人以为它
/// 还是合法状态）。
///
/// `allowed_from` 由目标状态反查（`status_guard_for_target`）——原实现
/// 「无源状态守卫」，本实现补上；等价性由该函数的 doc 逐目标状态论证。
pub async fn mark_batch_status_only(
    conn: &mut PgConnection,
    batch_id: i64,
    expected_version: i32,
    new_status: &str,
    is_repairing: Option<bool>,
    updated_by: i64,
    event_id: Option<i64>,
) -> Result<u64, AppError> {
    apply_batch_status_change(
        conn,
        StatusChange {
            batch_id,
            new_status,
            new_location: None,
            new_holder_id: None,
            new_process_id: None,
            new_process_step_id: None,
            is_repairing,
            expected_version: Some(expected_version),
            allowed_from: status_guard_for_target(new_status),
            updated_by,
            clear_location: false,
            clear_holder_id: false,
            clear_process_id: false,
            clear_process_step_id: false,
            // 2026-10-01 review 第 1 轮 M4：只有 `new_status='CANCELLED'` 会让
            // part 新进终态（cancel-batch），caller 需在那种情况下传雪花 id；
            // `READY_TO_SHIP` / `IN_PROCESS` 两个目标传 `None` 即可。
            event_id,
        },
    )
    .await
    .map(|_| 1u64)
}

/// 2026-10-01：由**目标**状态反查 `mark_batch_status_only` 的源状态白名单。
///
/// 等价性论证（原实现 WHERE 无 status 守卫，调用点各自在 service 层守）：
/// - `READY_TO_SHIP`：唯一调用点是 `scan_inspect` 的 pass 分支，源恒为上一步
///   刚写下的 `INSPECTION`（同一函数前半段写死）→ 白名单 `["INSPECTION"]` 等价。
/// - `IN_PROCESS`：唯一调用点是 `scan_inspect` 的
///   FAIL 分支，源恒为 `INSPECTION` → 等价。
/// - `CANCELLED`：调用点 `batch_ops::cancel_batch` 在 service 层已守
///   `from != COMPLETED && from != CANCELLED`，故白名单取
///   「全状态减两个终态」→ 等价。
pub fn status_guard_for_target(target: &str) -> &'static [&'static str] {
    match target {
        "READY_TO_SHIP" | "IN_PROCESS" => &["INSPECTION"],
        _ => &[
            "PENDING",
            "PROGRAMMING",
            "IN_PROCESS",
            "INSPECTION",
            "READY_TO_SHIP",
            "DELIVERED",
            "OUTSOURCE",
        ],
    }
}

/// 读 part 的 `process_chain_id`（part 不存在 / 已软删 → `BIZ_PART_NOT_FOUND`）。
#[inline]
async fn read_part_chain_id(
    conn: &mut PgConnection,
    part_id: i64,
) -> Result<Option<i64>, AppError> {
    let row: Option<(Option<i64>,)> =
        sqlx::query_as("SELECT process_chain_id FROM t_part WHERE id = $1 AND deleted_at IS NULL")
            .bind(part_id)
            .fetch_optional(&mut *conn)
            .await?;
    Ok(row
        .ok_or_else(|| AppError::biz(code::BIZ_PART_NOT_FOUND, format!("part {part_id} 不存在")))?
        .0)
}

/// 2026-10-03 新增：工序链**可选**版守卫（`optional_process_chain` 的 part 侧）。
///
/// ## 为什么要放松
/// 生产库里绝大多数零件没有 `t_part.process_chain_id`（`queue` 的候选池 SQL 早在
/// 2026-09-30 就因为「INNER JOIN t_process_chain_step 导致批次隐身」改成按
/// `t_part_batch.current_process_id` 普通过滤）。若进生产流仍强制「先有链」，
/// 读侧（候选池 / 外协可发送列表）能列出来的批次，写侧却发不出去 —— 读侧口径才是
/// 仓库的既定权威依据。
///
/// ## 三种返回
/// - part 不存在 / 已软删 → `20101 BIZ_PART_NOT_FOUND`
/// - `process_chain_id IS NULL` → `Ok(None)`，caller 放行、`current_process_step_id` 落 NULL
/// - 已绑链 → `Ok(Some(chain_id))`
///
/// **链行自身已软删的情形本函数不判**：读的就是 `t_part.process_chain_id` 一个列，
/// 链软删后该列仍是旧 id，caller 的 step 解析在链内找不到活跃 step 时才以
/// `20702` 拒收（见 [`optional_step_id`]）。
///
/// 恢复「必须有链」（`20706 BIZ_PROCESS_CHAIN_REQUIRED`）的严格变体时，在本函数
/// 之上加一层 `ok_or_else` 即可，读链逻辑不必重新发明。
pub async fn optional_process_chain(
    conn: &mut PgConnection,
    part_id: i64,
) -> Result<Option<i64>, AppError> {
    read_part_chain_id(conn, part_id).await
}

/// 2026-10-03 新增：工序链**可选**版守卫（step 侧），与 [`optional_process_chain`] 配对使用。
///
/// ## 两种返回必须分清
/// - `chain_id = None`（压根没链）→ `Ok(None)`，**放行**。写 NULL 的先例见
///   `dispatch` 路径（`worker-scan` RETURNED 不写 step 是既有行为），
///   `current_process_step_id` 早已被官方降级为「可选的显示用定位信息」
///   （写入不变式见本文件 [`mark_batch_with_status_and_meta`]）。
/// - `chain_id = Some(_)` 但链内找不到该 process 的活跃 step → `20702
///   BIZ_PROCESS_CHAIN_STEP_NOT_FOUND`，**继续拒**。这是真数据错误：链是有的，
///   却没把正在加工的工序登记进链内（例如链在批次发出之后才被改写）。跟着
///   「没链就放行」一起吞掉的话，批次会带着一个链内不存在的工序静默入池，
///   之后每一步的 step 定位全部漂移，且没有任何报错可查。
pub async fn optional_step_id(
    conn: &mut PgConnection,
    chain_id: Option<i64>,
    process_id: i64,
) -> Result<Option<i64>, AppError> {
    let Some(chain_id) = chain_id else {
        return Ok(None);
    };
    let step_id =
        crate::modules::prod::process_chain::repo::ProcessChainRepo::resolve_step_id_by_process(
            conn, chain_id, process_id,
        )
        .await?
        .ok_or_else(|| {
            AppError::biz(
                code::BIZ_PROCESS_CHAIN_STEP_NOT_FOUND,
                format!("chain {chain_id} 内找不到 process_id={process_id} 的活跃 step"),
            )
        })?;
    Ok(Some(step_id))
}

// ===== unit tests for state-machine helpers =====

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infra::clock::now_naive;
    use crate::infra::snowflake::SnowflakeIdGenerator;
    use crate::modules::part::statemachine::PartStatus;
    use hsh_erp_test_support::test_pool;

    /// 2026-10-03 新增：写一个 `t_part` 行（`process_chain_id` 留空由调用方决定）。
    ///
    /// `t_part.customer_id` 是 NOT NULL，故先造一个根 L1 客户（无 parent）。
    async fn insert_part(pool: &sqlx::PgPool, name: &str) -> i64 {
        let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 9);
        let now = now_naive();
        let customer_id = snowflake.next_id();
        sqlx::query(
            "INSERT INTO t_customer (id, name, version, created_at, updated_at) \
             VALUES ($1, $2, 0, $3, $3)",
        )
        .bind(customer_id)
        .bind(format!("Co-{name}"))
        .bind(now)
        .execute(pool)
        .await
        .expect("insert t_customer");
        let id = snowflake.next_id();
        sqlx::query(
            "INSERT INTO t_part (id, name, drawing_no, applicant_name, quantity, unit_price, \
             total_price, request_date, planned_delivery_date, customer_id, status, version, \
             created_at, updated_at) \
             VALUES ($1, $2, $3, 'Tester', 1, 1.00, 1.00, CURRENT_DATE, CURRENT_DATE, $4, \
                     'PENDING', 0, $5, $5)",
        )
        .bind(id)
        .bind(name)
        .bind(format!("DWG-{name}"))
        .bind(customer_id)
        .bind(now)
        .execute(pool)
        .await
        .expect("insert t_part");
        id
    }

    /// 2026-10-03 新增：`optional_process_chain` 的三条分支。
    #[tokio::test]
    async fn optional_process_chain_none_when_part_has_no_chain() {
        let pool = test_pool().await;
        let conn = &mut *pool.acquire().await.expect("acquire");
        let part_id = insert_part(&pool, "OPCH-NONE").await;
        assert_eq!(optional_process_chain(conn, part_id).await.unwrap(), None);
    }

    #[tokio::test]
    async fn optional_process_chain_some_when_part_bound_to_chain() {
        let pool = test_pool().await;
        let conn = &mut *pool.acquire().await.expect("acquire");
        let part_id = insert_part(&pool, "OPCH-SOME").await;
        // 换 instance 取 id：同毫秒内重复 `SnowflakeIdGenerator::new(..)` 会撞 pkey
        let chain_id = SnowflakeIdGenerator::new(1_577_836_800_000, 11).next_id();
        sqlx::query(
            "INSERT INTO t_part_process_chain (id, name, version, created_at, created_by, \
             updated_at, updated_by) VALUES ($1, 'chain-opch', 0, now(), 0, now(), 0)",
        )
        .bind(chain_id)
        .execute(&pool)
        .await
        .expect("insert t_part_process_chain");
        sqlx::query("UPDATE t_part SET process_chain_id = $1 WHERE id = $2")
            .bind(chain_id)
            .bind(part_id)
            .execute(&pool)
            .await
            .expect("bind part to chain");
        assert_eq!(
            optional_process_chain(conn, part_id).await.unwrap(),
            Some(chain_id)
        );
    }

    /// part 不存在 ⇒ 仍 `20101 BIZ_PART_NOT_FOUND`（放松的是「有没有链」，不是
    /// 「part 存不存在」）。
    #[tokio::test]
    async fn optional_process_chain_missing_part_is_part_not_found() {
        let pool = test_pool().await;
        let conn = &mut *pool.acquire().await.expect("acquire");
        let err = optional_process_chain(conn, 9_000_000_000_000_009_999)
            .await
            .expect_err("不存在的 part 必须拒");
        assert_eq!(err.code(), code::BIZ_PART_NOT_FOUND);
    }

    /// 2026-10-03 新增：`optional_step_id` 的两条分支。
    #[tokio::test]
    async fn optional_step_id_none_without_chain_but_rejects_unlisted_process() {
        let pool = test_pool().await;
        let conn = &mut *pool.acquire().await.expect("acquire");
        let part_id = insert_part(&pool, "OPST").await;
        let chain_id = SnowflakeIdGenerator::new(1_577_836_800_000, 11).next_id();
        sqlx::query(
            "INSERT INTO t_part_process_chain (id, name, version, created_at, created_by, \
             updated_at, updated_by) VALUES ($1, 'chain-opst', 0, now(), 0, now(), 0)",
        )
        .bind(chain_id)
        .execute(&pool)
        .await
        .expect("insert t_part_process_chain");

        // ① 无链 → 放行（NULL）
        assert_eq!(optional_step_id(conn, None, 4_242).await.unwrap(), None);
        // ② 有链但链内没有该 process → 20702，不放行
        let err = optional_step_id(conn, Some(chain_id), 4_242)
            .await
            .expect_err("链内没有该工序必须拒");
        assert_eq!(err.code(), code::BIZ_PROCESS_CHAIN_STEP_NOT_FOUND);
        let _ = part_id;
    }

    #[test]
    fn ensure_transition_allows_known() {
        // 已知的合法迁移应通过
        ensure_transition(PartStatus::PENDING, PartStatus::IN_PROCESS, "test").unwrap();
        ensure_transition(PartStatus::IN_PROCESS, PartStatus::PENDING, "test").unwrap();
        ensure_transition(PartStatus::PROGRAMMING, PartStatus::PENDING, "test").unwrap();
        ensure_transition(PartStatus::PROGRAMMING, PartStatus::IN_PROCESS, "test").unwrap();
        ensure_transition(PartStatus::OUTSOURCE, PartStatus::IN_PROCESS, "test").unwrap();
        // 2026-10-03：send-to-outsource 的两个源状态
        ensure_transition(PartStatus::PENDING, PartStatus::OUTSOURCE, "test").unwrap();
        ensure_transition(PartStatus::IN_PROCESS, PartStatus::OUTSOURCE, "test").unwrap();
    }

    #[test]
    fn ensure_transition_rejects_unknown() {
        // 非法迁移
        let r = ensure_transition(PartStatus::DELIVERED, PartStatus::READY_TO_SHIP, "test");
        assert!(r.is_err());
        let r = ensure_transition(PartStatus::COMPLETED, PartStatus::CANCELLED, "test");
        assert!(r.is_err());
    }

    #[test]
    fn ensure_transition_rejects_already_cancelled() {
        // CANCELLED 是终态；任何迁移拒绝
        let r = ensure_transition(PartStatus::CANCELLED, PartStatus::PENDING, "test");
        assert!(r.is_err());
        let code = r.unwrap_err().code();
        assert_eq!(code, code::BIZ_PART_ALREADY_CANCELLED);
    }

    #[test]
    fn batch_version_mismatch_detects_correctly() {
        assert!(batch_version_mismatch(1, 0, 1));
        assert!(!batch_version_mismatch(1, 1, 1));
    }
}

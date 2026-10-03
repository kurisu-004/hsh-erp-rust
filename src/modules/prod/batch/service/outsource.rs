//! prod::batch 的外协流转三端点
//!
//! - `POST /api/v2/prod/batches/{batch_id}/send-to-outsource` —— `PENDING` /
//!   `IN_PROCESS+PRODUCTION_SHELF` → `OUTSOURCE`
//! - `POST /api/v2/prod/batches/{batch_id}/receive-from-outsource` —— 外协收回 →
//!   `IN_PROCESS` + 生产架
//! - `POST /api/v2/prod/batches/{batch_id}/receive-from-outsource-to-inspection`
//!   —— 外协收回 → `INSPECTION` + 送检架
//!
//! ## 2026-10-03：本文件补齐的 3 项能力
//!
//! 1. **DIRECT 免审批直发**（原为 501 stub）：按 `(part, company, process)` 复用
//!    活跃 APPROVED 报价；没有则自动建一条 `price = 0` 的占位报价。
//! 2. **部分发送 / 部分接收**：入参 `quantity`，`0 < q < 批次量` 时先拆批、只流转
//!    子批次，源批次留在原处（量减少 `q`）。拆批统一走
//!    `PartBatchRepo::_split_batch_inner`（OCC + 数量守卫都在里面）。
//! 3. **补齐缺失守卫**：`process.category` 必须 `OUTSOURCE`、公司必须映射该工序、
//!    `direct` 与 `quote_id` 必须恰给一个、`requires_approval=true` 的工序不许
//!    `direct=true` 直发（2026-10-03 review 第 1 轮）。
//!
//! 部分接收的**记账口径**（有意为之，勿"顺手修"）：shipment 记的是**发出时**的全量。
//! 部分回收只拆批次，源批次余量继续挂着那张 `OUTSOURCING` shipment；
//! `received_at` / `status='RECEIVED'` 只在**整批**回收时才落。故对账列表里
//! `shipment.quantity` 与批次当前余量可能不相等 —— 对账要回答的是「发出去多少、
//! 单价多少」。

use sqlx::PgConnection;

use crate::auth::rbac::{CurrentUser, Role};
use crate::infra::snowflake::SnowflakeIdGenerator;
use crate::modules::outsource::repo::OutsourceRepoTrait;
use crate::modules::part::model::NewPartEvent;
use crate::modules::part::repo::PartRepoTrait;
use crate::modules::part::statemachine::PartStatus;
use crate::modules::prod::batch::dto::{
    ReceiveFromOutsourceRequest, ReceiveFromOutsourceToInspectionRequest, SendToOutsourceRequest,
};
use crate::modules::prod::batch::repo::PartBatchRepo;
use crate::shared::error::{AppError, code};

use super::BatchService;
use super::guard::{
    assert_shelf_maps_process, ensure_transition, mark_batch_with_status_and_meta,
    optional_process_chain, optional_step_id, validate_batch_version, validate_shelf_zone,
};

/// 2026-10-03：DIRECT 占位报价的 `note` 固定文案 —— 让对账页一眼看出
/// 「这行单价 0 不是漏填，是免审批直发自动建的占位报价」。
const DIRECT_PLACEHOLDER_NOTE: &str = "DIRECT 直发自动创建（免审批，单价待对账补录）";

/// 解析本次流转的数量语义（部分收发共用）。
///
/// 返回值：
/// - `Ok(None)` = 整批（`quantity` 缺省，或恰好等于批次量 —— 后者不拆批）
/// - `Ok(Some(q))` = 部分量，已守 `0 < q < batch_quantity`
/// - `Err` = 数量非法（`q <= 0` / `q > 批次量`）→ 400 `BIZ_INVALID_VALUE`
///
/// 2026-10-03 新增。守卫的必要性：`quantity` 缺省即整批，若不做「显式部分量」的
/// 归一化，调用方传了部分量却因字段名不匹配被 serde 静默忽略，界面上选 5 件、
/// 实际整批发出。
#[inline]
fn resolve_partial_quantity(
    quantity: Option<i32>,
    batch_quantity: i32,
    ctx: &str,
) -> Result<Option<i32>, AppError> {
    let Some(q) = quantity else {
        return Ok(None);
    };
    if q <= 0 {
        return Err(AppError::biz(
            code::BIZ_INVALID_VALUE,
            format!("{ctx}: quantity {q} 必须 > 0"),
        ));
    }
    if q > batch_quantity {
        return Err(AppError::biz(
            code::BIZ_INVALID_VALUE,
            format!("{ctx}: quantity {q} 超过批次量 {batch_quantity}"),
        ));
    }
    if q == batch_quantity {
        return Ok(None);
    }
    Ok(Some(q))
}

/// 找 `(part, company, process)` 三元组上的活跃 APPROVED 报价 id（多条取 id 最大者）。
///
/// 活跃口径复用 `OutsourceRepoTrait::quote_list_active_by_part_process`（`SUBMITTED` /
/// `APPROVED` 未软删），再按公司过滤 + 只取 APPROVED。
async fn find_approved_quote_id(
    conn: &mut PgConnection,
    part_id: i64,
    company_id: i64,
    process_id: i64,
) -> Result<Option<i64>, sqlx::Error> {
    let rows = {
        let mut orepo = &mut *conn;
        orepo
            .quote_list_active_by_part_process(part_id, process_id, 0)
            .await?
    };
    Ok(rows
        .into_iter()
        .filter(|q| q.outsource_company_id == company_id && q.status == "APPROVED")
        .max_by_key(|q| q.id)
        .map(|q| q.id))
}

/// 2026-10-03：DIRECT（免审批直发）的价来源解析，返回本次 shipment 引用的
/// `quote_id`。
///
/// 1. **复用**：按 `(part_id, company_id, process_id)` 找活跃（`SUBMITTED` /
///    `APPROVED`）报价中的 APPROVED 条目（不区分 `is_direct`，故 DIRECT 占位报价
///    本身也会被复用），多条时取 id 最大者（最新审批）。
/// 2. **自动建**：没有则 INSERT 一条 `price = 0` / `status='APPROVED'` /
///    `is_direct = true` 的占位报价（`submitted_at` / `reviewed_at` = now），
///    复用下方主流程的「APPROVED 校验 + 写 SENT 事件」路径。
///
/// ## 幂等与两条互补的 partial 唯一索引
///
/// `t_outsource_quote` 上与「APPROVED 活跃行」相关的唯一索引有两条，谓词互斥：
///
/// | 索引 | 键 | 谓词 | 拦的是谁 |
/// |---|---|---|---|
/// | `uq_t_outsource_quote_approved_part_process`（baseline） | `(part_id, process_id)` | `deleted_at IS NULL AND status='APPROVED' AND is_direct = false` | 审批报价：每 (零件, 工序) 最多一条 |
/// | `uq_t_outsource_quote_direct_part_company_process`（migration 008） | `(part_id, outsource_company_id, process_id)` | `deleted_at IS NULL AND status='APPROVED' AND is_direct = true` | DIRECT 占位报价：每 (零件, 公司, 工序) 最多一条 |
///
/// 两条都必须有：审批报价**故意**排除 `is_direct = true`（这正是 `is_direct` 列
/// 存在的意义 —— 免审批直发不该占用审批报价的唯一键），代价是单靠它兜不住 DIRECT
/// 行。migration 008 补上后半条，于是下面的 `ON CONFLICT DO NOTHING` + 回查
/// 才真正成立：并发下第二个 INSERT 命中该索引 → 0 行 → 回查取第一条的 id 当
/// `quote_id`，同一 tuple 恒定只留一条 0 元占位报价。
async fn resolve_direct_quote_id(
    conn: &mut PgConnection,
    snowflake: &SnowflakeIdGenerator,
    part_id: i64,
    company_id: i64,
    process_id: i64,
    current: &CurrentUser,
) -> Result<i64, AppError> {
    if let Some(qid) = find_approved_quote_id(conn, part_id, company_id, process_id).await? {
        return Ok(qid);
    }
    let quote_id = snowflake.next_id();
    let inserted = sqlx::query(
        "INSERT INTO t_outsource_quote \
             (id, part_id, outsource_company_id, process_id, price, note, status, \
              submitted_at, reviewed_at, is_direct, created_by, updated_at, updated_by) \
         VALUES ($1, $2, $3, $4, 0, $5, 'APPROVED', now(), now(), true, $6, now(), $6) \
         ON CONFLICT DO NOTHING",
    )
    .bind(quote_id)
    .bind(part_id)
    .bind(company_id)
    .bind(process_id)
    .bind(DIRECT_PLACEHOLDER_NOTE)
    .bind(current.id)
    .execute(&mut *conn)
    .await?;
    if inserted.rows_affected() == 1 {
        return Ok(quote_id);
    }
    // 命中 `uq_t_outsource_quote_direct_part_company_process`（并发窗口内同一
    // tuple 已被别的请求建过占位报价）→ 回查复用它，不留第二条等价记录
    match find_approved_quote_id(conn, part_id, company_id, process_id).await? {
        Some(qid) => Ok(qid),
        None => Err(AppError::biz(
            code::BIZ_OUTSOURCE_QUOTE_NOT_APPROVED,
            format!(
                "DIRECT 直发：占位报价被唯一约束拒绝，且回查不到 \
                 (part={part_id}, company={company_id}, process={process_id}) 的 APPROVED 报价"
            ),
        )),
    }
}

/// 整批回收外协货时把该批次的**开口** shipment 标 `RECEIVED` + 写 quote event
/// `RECEIVED`（`receive_from_outsource` 调用；`receive_from_outsource_to_inspection`
/// 仍是自带的同一段内联 SQL，本次不抽以免动到另一个端点的行为）。
///
/// 2026-10-03 抽成自由函数：部分接收必须**跳过**这一段（子批次没有 shipment，
/// 源批次那张还要继续开口），「跳不跳」用一个显式 if 表达比把整段复制两份更好读。
///
/// 无开口 shipment 时静默跳过（外协厂直接入库的批次没有发货记录，不算错误）。
async fn close_open_outsource_shipment(
    conn: &mut PgConnection,
    snowflake: &SnowflakeIdGenerator,
    batch_id: i64,
    note: Option<&str>,
    current: &CurrentUser,
) -> Result<(), AppError> {
    let shipment_row: Option<(i64, i64, i32)> = sqlx::query_as(
        "SELECT id, quote_id, version FROM t_outsource_shipment \
             WHERE batch_id = $1 AND deleted_at IS NULL AND status = 'OUTSOURCING'",
    )
    .bind(batch_id)
    .fetch_optional(&mut *conn)
    .await?;
    let Some((sid, qid, sver)) = shipment_row else {
        return Ok(());
    };
    let marked = sqlx::query(
        "UPDATE t_outsource_shipment SET status = 'RECEIVED', received_at = now(), \
             version = version + 1, updated_at = now(), updated_by = $2 \
         WHERE id = $1 AND version = $3 AND deleted_at IS NULL",
    )
    .bind(sid)
    .bind(current.id)
    .bind(sver)
    .execute(&mut *conn)
    .await?;
    if marked.rows_affected() == 0 {
        return Err(AppError::biz(
            code::VERSION_CONFLICT,
            format!("shipment {sid} 版本冲突"),
        ));
    }
    // quote_id 可空（旧 shipment 兼容，列本身 NOT NULL 但历史行可能是 0）；非 0 时写 RECEIVED 事件
    if qid != 0 {
        sqlx::query(
            "INSERT INTO t_outsource_quote_event \
                 (id, quote_id, event_type, from_status, to_status, note, created_by) \
                 VALUES ($1, $2, 'RECEIVED', 'APPROVED', 'APPROVED', $3, $4)",
        )
        .bind(snowflake.next_id())
        .bind(qid)
        .bind(note)
        .bind(current.id)
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

impl BatchService {
    /// `POST /prod/batches/{batch_id}/send-to-outsource`：PENDING / IN_PROCESS+PRODUCTION_SHELF → OUTSOURCE。
    ///
    /// Phase 2（2026-09-13）扩展：
    /// - 同事务 INSERT t_outsource_shipment（status=OUTSOURCING，quantity=本次发送量）
    /// - 若 `quote_id` 提供：校验 APPROVED 状态，写 `t_outsource_quote_event` `SENT`
    ///
    /// 2026-10-03 扩展（`direct` 由 501 stub 变为可用 + 部分发送 + 补守卫）：
    /// - **DIRECT**：`direct=true` 时复用 `(part, company, process)` 的活跃 APPROVED
    ///   报价，没有则自动建 `price=0` 占位报价；与 `quote_id` 互斥，两者都不给 → 400
    /// - **部分发送**：`quantity ∈ (0, 批次量)` 时先拆出子批次，只把子批次发出
    /// - **守卫**：process 类别必须 `OUTSOURCE`；公司必须映射该工序；
    ///   **`requires_approval=true` 的工序不许 `direct=true`**（2026-10-03 review
    ///   第 1 轮补：此前该规则只在读侧 SQL 生效，写侧无任何强制）；
    ///   **`quote_id` 不接受 DIRECT 占位报价**（2026-10-03 review 第 2 轮补：
    ///   占位报价同样是 `status='APPROVED'`，能绕开上面那条守卫）
    pub async fn send_to_outsource<R: PartRepoTrait>(
        mut repo: R,
        snowflake: &SnowflakeIdGenerator,
        batch_id: i64,
        req: SendToOutsourceRequest,
        current: &CurrentUser,
    ) -> Result<crate::modules::part::vo::PartOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk, Role::Inspector])?;
        // ---- 价来源守卫：APPROVAL（quote_id）与 DIRECT（direct=true）互斥且必居其一 ----
        //
        // 2026-10-03 新增。守卫的必要性：没有价来源时 shipment 的 `unit_price` 只能
        // 落 0，而对账页看到「单价 0」无从判断是漏填还是 DIRECT 免审批直发。
        let direct = req.direct.unwrap_or(false);
        if direct && req.quote_id.is_some() {
            return Err(AppError::biz(
                code::BIZ_INVALID_VALUE,
                "send-to-outsource: direct=true 与 quote_id 互斥（DIRECT 免审批直发；\
                 APPROVAL 模式请只传 quote_id）",
            ));
        }
        if !direct && req.quote_id.is_none() {
            return Err(AppError::biz(
                code::BIZ_INVALID_VALUE,
                "send-to-outsource: 必须给价来源——direct=true（免审批直发）或 \
                 quote_id（APPROVED 报价）",
            ));
        }
        let batch = repo
            .find_batch_by_id(batch_id)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_PART_BATCH_NOT_FOUND, "batch 不存在"))?;
        // 2026-10-02：batch_id 来自 URL 路径参数（`POST /prod/batches/{batch_id}/…`），
        // part_id 由批次行反查。
        let part_id = batch.part_id;
        let part = repo
            .get_part_inspected(part_id)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_PART_NOT_FOUND, "part 不存在"))?;
        validate_batch_version(batch.id, req.version, batch.version)?;
        let from = PartStatus::from_str(&batch.status)
            .ok_or_else(|| AppError::biz(code::BIZ_INVALID_VALUE, "batch.status 非法"))?;
        ensure_transition(from, PartStatus::OUTSOURCE, "send-to-outsource")?;
        // service 守：IN_PROCESS 时必须有 location=PRODUCTION_SHELF
        if from == PartStatus::IN_PROCESS && batch.location.as_deref() != Some("PRODUCTION_SHELF") {
            return Err(AppError::biz(
                code::BIZ_INVALID_TRANSITION,
                "send-to-outsource: IN_PROCESS 批次必须在 PRODUCTION_SHELF 上",
            ));
        }
        // 2026-10-03：部分发送数量解析（缺省 / 等于批次量 = 整批）
        let partial_qty =
            resolve_partial_quantity(req.quantity, batch.quantity, "send-to-outsource")?;
        // 2026-10-03：工序链改为可选（无链的旧零件也能发外协，见 guard.rs）
        let chain_id = optional_process_chain(repo.conn_mut(), part_id).await?;
        // 校验 outsource 公司存在 + 启用
        let company_row: Option<(bool,)> = sqlx::query_as(
            "SELECT is_active FROM t_outsource_company WHERE id = $1 AND deleted_at IS NULL",
        )
        .bind(req.outsource_company_id)
        .fetch_optional(repo.conn_mut())
        .await?;
        let is_active = company_row.ok_or_else(|| {
            AppError::biz(
                code::BIZ_OUTSOURCE_COMPANY_NOT_FOUND,
                format!("outsource_company {} 不存在", req.outsource_company_id),
            )
        })?;
        if !is_active.0 {
            return Err(AppError::biz(
                code::BIZ_OUTSOURCE_COMPANY_IN_USE,
                format!("outsource_company {} 已停用", req.outsource_company_id),
            ));
        }
        // 2026-10-03 新增守卫：外协工序必须是 OUTSOURCE 类别。守卫的必要性：只校验
        // 「process 存在」的话，把货派给一道**非外协**工序（例如内部装配工序）也会
        // 通过，批次随后被标成 OUTSOURCE + holder 写外协公司，库内会出现
        // 「这道工序由不需要它的公司加工」的脏关系。
        // `process_get_category` 只查未软删行，故「不存在」仍由上面的
        // `BIZ_PROCESS_NOT_FOUND` 先行拦掉。
        let category = {
            let mut orepo = &mut *repo.conn_mut();
            orepo.process_get_category(req.process_id).await?
        }
        .ok_or_else(|| {
            AppError::biz(
                code::BIZ_PROCESS_NOT_FOUND,
                format!("process {} 不存在", req.process_id),
            )
        })?;
        if category != "OUTSOURCE" {
            return Err(AppError::biz(
                code::BIZ_INVALID_VALUE,
                format!(
                    "send-to-outsource: process {} 的 category={category}，外协派发必须 \
                     走 OUTSOURCE 类别的工序",
                    req.process_id
                ),
            ));
        }
        // 2026-10-03 review 第 1 轮：写侧补「需审批的工序不许直发」守卫。
        //
        // 守卫的必要性：`requires_approval` 此前**只在读侧生效**（`GET
        // /outsource-sendable` 与 `/outsource-pool` 的判定 SQL 会把「需审批但无审批
        // 报价」的批次藏起来），写侧零校验 ⇒ 绕过 UI 直接调本端点传 `direct=true`
        // 就能对「先审批再发」这道业务规则下该走报价的工序直发，系统里没有任何一处
        // 强制。两侧同时守才闭环：读侧决定「看不看得见」，写侧决定「发不发得成」。
        //
        // 放在 category 校验之后：那里已经用同一个 `process_get_category` 确认了
        // 工序存在（`BIZ_PROCESS_NOT_FOUND` 先行），这里「不存在」的情形不可能出现，
        // 用 `unwrap_or(true)` 取**保守默认**（宁可拒，不放行）而不是 `unwrap()`。
        let requires_approval = {
            let mut orepo = &mut *repo.conn_mut();
            orepo.process_get_requires_approval(req.process_id).await?
        }
        .unwrap_or(true);
        if direct && requires_approval {
            return Err(AppError::biz(
                code::BIZ_INVALID_VALUE,
                format!(
                    "send-to-outsource: 外协工序 {} requires_approval=true，该工序需要 \
                     报价审批，请先走审批（传 quote_id）再发货，不能 direct 直发",
                    req.process_id
                ),
            ));
        }
        // 2026-10-03 新增守卫：公司必须**活跃映射**该工序
        // （`t_outsource_company_process` 存在未软删行）。与「公司有没有这项能力」
        // 是两件事：`t_outsource_company.is_active` 只说公司在册。
        let mapped_company_ids = {
            let mut orepo = &mut *repo.conn_mut();
            orepo
                .junction_list_company_ids_by_process(req.process_id)
                .await?
        };
        if !mapped_company_ids.contains(&req.outsource_company_id) {
            return Err(AppError::biz(
                code::BIZ_INVALID_VALUE,
                format!(
                    "send-to-outsource: outsource_company {} 未映射外协工序 {}",
                    req.outsource_company_id, req.process_id
                ),
            ));
        }
        // PR-3：解析 step_id（chain 内 process_id → step_id）写入 OUTSOURCE 批次。
        // 2026-10-03：无链 → 落 NULL（显示用定位信息，非必填）；有链但链内没有该
        // 工序 → 20702 拒收。
        let step_id = optional_step_id(repo.conn_mut(), chain_id, req.process_id).await?;
        // 价来源解析：DIRECT 自动取（复用 / 建占位），APPROVAL 用调用方给的
        // quote_id。两条路径汇合到同一段 APPROVED 校验 + 写 SENT 事件。
        let quote_id: i64 = match (direct, req.quote_id) {
            (true, None) => {
                resolve_direct_quote_id(
                    repo.conn_mut(),
                    snowflake,
                    part_id,
                    req.outsource_company_id,
                    req.process_id,
                    current,
                )
                .await?
            }
            (false, Some(qid)) => qid,
            // 上面的两条守卫已把 (true, Some) 与 (false, None) 拒掉；这里仍给一条
            // 400 而不是 unwrap panic —— 守卫若将来被调整，表现为可诊断的拒绝。
            _ => {
                return Err(AppError::biz(
                    code::BIZ_INVALID_VALUE,
                    "send-to-outsource: direct 与 quote_id 必须恰给一个",
                ));
            }
        };
        let quote_row: Option<(String, rust_decimal::Decimal, i64, i64, i64, bool)> =
            sqlx::query_as(
                "SELECT status, price, part_id, outsource_company_id, process_id, is_direct \
                 FROM t_outsource_quote \
                 WHERE id = $1 AND deleted_at IS NULL",
            )
            .bind(quote_id)
            .fetch_optional(repo.conn_mut())
            .await?;
        let (status, price, q_part_id, q_company_id, q_process_id, q_is_direct) = quote_row
            .ok_or_else(|| {
                AppError::biz(
                    code::BIZ_OUTSOURCE_QUOTE_NOT_FOUND,
                    format!("quote {quote_id} 不存在"),
                )
            })?;
        if status != "APPROVED" {
            return Err(AppError::biz(
                code::BIZ_OUTSOURCE_QUOTE_NOT_APPROVED,
                format!("quote {quote_id} 当前 {status}，非 APPROVED 不可发送"),
            ));
        }
        // 2026-10-03 review 第 2 轮：APPROVAL（`quote_id`）路径拒 `is_direct=true`。
        //
        // 不变式：「需审批的工序只能凭真审批价发货」有两条入口，两条都要守 ——
        // `direct=true` 由上面那道 `requires_approval` 守卫拦，`quote_id` 由本守卫
        // 拦。只守状态与三元组不够：占位报价是 `resolve_direct_quote_id` 自动建的
        // `price=0 / status='APPROVED' / is_direct=true` 行，恰好满足 APPROVAL 分支的
        // 既有校验条件（`status='APPROVED'` + (part, company, process) 三元组一致），
        // 不看 `is_direct` 就与真审批报价无法区分。
        //
        // 占位报价在库里已经存在（守卫上线前建的，或建完之后该工序的
        // `requires_approval` 由 false 被 `PATCH /prod/processes/{id}` 翻成 true），
        // 所以这道守卫必须落在端点里，不能靠清数据。
        //
        // 判据用 `!direct` 而不是「凡 `is_direct=true` 就拒」：DIRECT 路径的
        // `find_approved_quote_id` 复用占位报价是**既有正确行为**（免审批直发本就
        // 没有审批价），而该路径在需审批工序上已被上面那道守卫整体拦掉，不会走到
        // 这里。两条路径的价来源判定必须分开。
        //
        // 错误码取 `BIZ_OUTSOURCE_QUOTE_NOT_APPROVED`（与紧邻的 status 守卫同码）：
        // 两者都是「这不是可用的审批价来源」，只是原因不同（没批 / 是占位价）；
        // 21302 留给「与请求参数不匹配」那条。
        if !direct && q_is_direct {
            return Err(AppError::biz(
                code::BIZ_OUTSOURCE_QUOTE_NOT_APPROVED,
                format!(
                    "quote {quote_id} 是免审批直发的占位价（is_direct=true、price=0），\
                     不能作为审批价来源；请改传该 (part, company, process) 经审批的报价"
                ),
            ));
        }
        if q_part_id != part_id
            || q_company_id != req.outsource_company_id
            || q_process_id != req.process_id
        {
            return Err(AppError::biz(
                code::BIZ_OUTSOURCE_QUOTE_INVALID_TRANSITION,
                format!("quote {quote_id} 与 send_to_outsource 参数不一致（part/company/process）"),
            ));
        }
        // ---- 2026-10-03 部分发送：拆出子批次，只把子批次流转出去 ----
        //
        // OCC 锚分两段，别混：源批次的锚是 `req.version`（前端列表行里的那个
        // version），由 `_split_batch_inner` 内部守卫；子批次的 `version` 被它写死
        // 为 0，所以必须**读回子批次行**取真实 version 再喂给 status_gate ——
        // 拿 `req.version` 去撞子批次会恒 409。
        //
        // 调用方要知道的副作用（2026-10-03 补注）：`_split_batch_inner` 对源批次
        // 的 UPDATE 带 `version = version + 1`，故**拆批成功后源批次的 version
        // 已经 +1**。同一次请求内不可能再对源批次做第二次流转（它已不在本次的
        // 流转对象里），但**跨请求的二次部分发送必须先刷新列表**拿新 version，
        // 否则恒 409 —— 这是设计意图（OCC 挡住「基于旧读数继续拆」），不是缺陷。
        let (target_batch_id, target_version, send_qty) = match partial_qty {
            Some(q) => {
                let child = PartBatchRepo::_split_batch_inner(
                    repo.conn_mut(),
                    snowflake.next_id(),
                    batch.id,
                    req.version,
                    part_id,
                    q,
                    &batch.status,
                    current.id,
                )
                .await
                .map_err(|e| match e {
                    // 0 行 = 源批次 version 已变 / 余量不足（OCC + 数量守卫）
                    sqlx::Error::RowNotFound => AppError::biz(
                        code::VERSION_CONFLICT,
                        format!("batch {} 版本冲突或余量不足（部分发送 {q}）", batch.id),
                    ),
                    other => AppError::from(other),
                })?;
                let child_row = repo.find_batch_by_id(child).await?.ok_or_else(|| {
                    AppError::biz(
                        code::BIZ_PART_BATCH_NOT_FOUND,
                        format!("部分发送拆批后查不到新批次 {child}"),
                    )
                })?;
                (child, child_row.version, q)
            }
            // 整批：流转对象与 OCC 锚都还是调用方给的那一批
            None => (batch.id, req.version, batch.quantity),
        };
        mark_batch_with_status_and_meta(
            repo.conn_mut(),
            target_batch_id,
            target_version,
            "OUTSOURCE",
            Some("OUTSOURCE_COMPANY"),
            Some(req.outsource_company_id),
            // 2026-10-03：无链时为 None ⇒ status_gate 的 clear 分支写 NULL
            step_id,
            // 2026-09-30：记录批次所属工序（外协加工的就是这道工序），
            // 收回时按 next_process_id 重新入池即可
            Some(req.process_id),
            current.id,
        )
        .await?;
        // Phase 2：同事务 INSERT t_outsource_shipment（status=OUTSOURCING）
        //   quantity = 本次发送量（整批 = batch.quantity；部分发送 = 拆批量 q）
        //   unit_price = quote.price；DIRECT 无可用报价时为自动建的 0 元占位报价
        let shipment_id = snowflake.next_id();
        let inserted_shipment = sqlx::query(
            "INSERT INTO t_outsource_shipment \
                 (id, quote_id, part_id, batch_id, outsource_company_id, process_id, \
                  quantity, unit_price, status, sent_at, created_by, updated_by) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, 'OUTSOURCING', now(), $9, $9) \
                 ON CONFLICT DO NOTHING",
        )
        .bind(shipment_id)
        .bind(quote_id)
        .bind(part_id)
        .bind(target_batch_id)
        .bind(req.outsource_company_id)
        .bind(req.process_id)
        .bind(send_qty)
        .bind(price)
        .bind(current.id)
        .execute(repo.conn_mut())
        .await?;
        // 若唯一索引 uq_t_outsource_shipment_open_batch 撞了（同一批次已有开口 shipment），
        // 0 行；回滚思路：拒 + BIZ_OUTSOURCE_SHIPMENT_INVALID_TRANSITION
        if inserted_shipment.rows_affected() == 0 {
            return Err(AppError::biz(
                code::BIZ_OUTSOURCE_SHIPMENT_INVALID_TRANSITION,
                format!("batch {target_batch_id} 已有开口 shipment；不可重复发送"),
            ));
        }
        // quote 存在（APPROVAL 显式传 / DIRECT 复用或自动建）→ 写 SENT 事件
        // （仅审计，不改 quote.status）
        sqlx::query(
            "INSERT INTO t_outsource_quote_event \
                 (id, quote_id, event_type, from_status, to_status, note, created_by) \
                 VALUES ($1, $2, 'SENT', 'APPROVED', 'APPROVED', $3, $4)",
        )
        .bind(snowflake.next_id())
        .bind(quote_id)
        .bind(req.note.as_deref())
        .bind(current.id)
        .execute(repo.conn_mut())
        .await?;
        repo.insert_part_event(NewPartEvent {
            id: snowflake.next_id(),
            part_id,
            event_type: "SENT_TO_OUTSOURCE",
            from_status: Some(from.as_str()),
            to_status: Some("OUTSOURCE"),
            // 部分发送时指向**真正发出去的那个子批次**
            batch_id: Some(target_batch_id),
            quantity: Some(send_qty),
            drawing_code: Some(&part.drawing_no),
            badge_code: None,
            note: req.note.as_deref(),
            created_by: Some(current.id),
        })
        .await?;
        let fresh = repo
            .get_part_inspected(part_id)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_PART_NOT_FOUND, "send-to-outsource 后查不到"))?;
        Ok(crate::modules::part::vo::PartOut::from(fresh))
    }

    /// `POST /prod/batches/{batch_id}/receive-from-outsource`：OUTSOURCE → IN_PROCESS（PRODUCTION_SHELF）。
    ///
    /// Phase 2（2026-09-13）扩展：同事务把批次开口 shipment 标 RECEIVED + 写 RECEIVED 事件。
    ///
    /// 2026-09-16 PR-3 批次 step 化：req.next_process_id 解析为 step_id 写入
    /// current_process_step_id（2026-10-03 起链本身可选，见 guard.rs）。
    ///
    /// 2026-10-03：入参从 `PlaceOnShelfRequest` 换成 `ReceiveFromOutsourceRequest`（多
    /// `quantity`），并支持**部分接收**：拆批后只回收子批次，源批次保留余量、继续持有
    /// 那张开口 shipment（`status` 仍 `OUTSOURCING`、`received_at` 仍 NULL）。
    pub async fn receive_from_outsource<R: PartRepoTrait>(
        mut repo: R,
        snowflake: &SnowflakeIdGenerator,
        batch_id: i64,
        req: ReceiveFromOutsourceRequest,
        current: &CurrentUser,
    ) -> Result<crate::modules::part::vo::PartOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk, Role::Inspector])?;
        let batch = repo
            .find_batch_by_id(batch_id)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_PART_BATCH_NOT_FOUND, "batch 不存在"))?;
        // 2026-10-02：batch_id 来自 URL 路径参数（`POST /prod/batches/{batch_id}/…`），
        // part_id 由批次行反查。
        let part_id = batch.part_id;
        let part = repo
            .get_part_inspected(part_id)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_PART_NOT_FOUND, "part 不存在"))?;
        validate_batch_version(batch.id, req.version, batch.version)?;
        let from = PartStatus::from_str(&batch.status)
            .ok_or_else(|| AppError::biz(code::BIZ_INVALID_VALUE, "batch.status 非法"))?;
        ensure_transition(from, PartStatus::IN_PROCESS, "receive-from-outsource")?;
        if from != PartStatus::OUTSOURCE {
            return Err(AppError::biz(
                code::BIZ_INVALID_TRANSITION,
                "receive-from-outsource: 源状态必须是 OUTSOURCE",
            ));
        }
        // 2026-10-03：部分接收数量解析（缺省 / 等于批次量 = 整批）
        let partial_qty =
            resolve_partial_quantity(req.quantity, batch.quantity, "receive-from-outsource")?;
        // 2026-10-03：工序链可选（无链的旧零件也能收回，见 guard.rs）
        let chain_id = optional_process_chain(repo.conn_mut(), part_id).await?;
        validate_shelf_zone(repo.conn_mut(), req.shelf_id, "PRODUCTION").await?;
        assert_shelf_maps_process(repo.conn_mut(), req.shelf_id, req.next_process_id).await?;
        // PR-3：解析 step_id（无链 → NULL；有链但链内无该工序 → 20702）
        let step_id = optional_step_id(repo.conn_mut(), chain_id, req.next_process_id).await?;
        // ---- 2026-10-03 部分接收：拆出子批次，只回收子批次 ----
        //
        // 新子批次继承源批次的 `OUTSOURCE` 状态与 `OUTSOURCE_COMPANY` holder
        // （`_split_batch_inner` 走 SELECT 继承），随后的 status_gate 写会把
        // 它的 location / holder / process / step 全换成生产架 + 新工序 ——
        // 即「出池清 location + holder」的三态由 status_gate 的 `clear_*` 语义兜住
        // （本调用点 4 列都传 `Some(..)`，故走的是「写值」而非 clear 分支）。
        // 源批次**不动** status / location / holder：它还在外协厂里。
        //
        // OCC 锚分两段（与 send 侧同款）：源批次用请求里的 `version`（由
        // `_split_batch_inner` 内部守卫），子批次必须用**读回行**的 version ——
        // 它被写死为 0，拿 `req.version` 去撞会恒 409。副作用同 send 侧：
        // 拆批成功后**源批次 version 已 +1**，二次部分接收必须先刷新列表。
        let (target_batch_id, target_version) = match partial_qty {
            Some(q) => {
                let child = PartBatchRepo::_split_batch_inner(
                    repo.conn_mut(),
                    snowflake.next_id(),
                    batch.id,
                    req.version,
                    part_id,
                    q,
                    &batch.status,
                    current.id,
                )
                .await
                .map_err(|e| match e {
                    sqlx::Error::RowNotFound => AppError::biz(
                        code::VERSION_CONFLICT,
                        format!("batch {} 版本冲突或余量不足（部分接收 {q}）", batch.id),
                    ),
                    other => AppError::from(other),
                })?;
                let child_row = repo.find_batch_by_id(child).await?.ok_or_else(|| {
                    AppError::biz(
                        code::BIZ_PART_BATCH_NOT_FOUND,
                        format!("部分接收拆批后查不到新批次 {child}"),
                    )
                })?;
                (child, child_row.version)
            }
            None => (batch.id, req.version),
        };
        mark_batch_with_status_and_meta(
            repo.conn_mut(),
            target_batch_id,
            target_version,
            "IN_PROCESS",
            Some("PRODUCTION_SHELF"),
            Some(req.shelf_id),
            // 2026-10-03：无链时为 None ⇒ status_gate 的 clear 分支写 NULL
            step_id,
            // 2026-09-30：进池 → current_process_id 写目标工序
            Some(req.next_process_id),
            current.id,
        )
        .await?;
        // 2026-10-03：**部分接收不关 shipment**。
        //
        // 口径见文件头：shipment 记的是发出时的全量，部分回收只拆批次，源批次余量
        // 继续挂着这张开口 shipment；`received_at` / `status='RECEIVED'` 与
        // quote event RECEIVED 都只在整批回收时落。
        if partial_qty.is_none() {
            self::close_open_outsource_shipment(
                repo.conn_mut(),
                snowflake,
                batch.id,
                req.note.as_deref(),
                current,
            )
            .await?;
        }
        repo.insert_part_event(NewPartEvent {
            id: snowflake.next_id(),
            part_id,
            event_type: "RECEIVED_FROM_OUTSOURCE",
            from_status: Some("OUTSOURCE"),
            to_status: Some("IN_PROCESS"),
            // 部分接收时指向真正回到生产架的那个子批次
            batch_id: Some(target_batch_id),
            quantity: Some(partial_qty.unwrap_or(batch.quantity)),
            drawing_code: Some(&part.drawing_no),
            badge_code: None,
            note: req.note.as_deref(),
            created_by: Some(current.id),
        })
        .await?;
        let fresh = repo
            .get_part_inspected(part_id)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_PART_NOT_FOUND, "receive 后查不到"))?;
        Ok(crate::modules::part::vo::PartOut::from(fresh))
    }

    /// `POST /prod/batches/{batch_id}/receive-from-outsource-to-inspection`：OUTSOURCE → INSPECTION。
    ///
    /// Phase 2（2026-09-13）扩展：同事务把批次对应开口 shipment 标 RECEIVED（与
    /// `receive_from_outsource` 同样的"整批接收"语义）。
    pub async fn receive_from_outsource_to_inspection<R: PartRepoTrait>(
        mut repo: R,
        snowflake: &SnowflakeIdGenerator,
        batch_id: i64,
        req: ReceiveFromOutsourceToInspectionRequest,
        current: &CurrentUser,
    ) -> Result<crate::modules::part::vo::PartOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk, Role::Inspector])?;
        let batch = repo
            .find_batch_by_id(batch_id)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_PART_BATCH_NOT_FOUND, "batch 不存在"))?;
        // 2026-10-02：batch_id 来自 URL 路径参数（`POST /prod/batches/{batch_id}/…`），
        // part_id 由批次行反查。
        let part_id = batch.part_id;
        let part = repo
            .get_part_inspected(part_id)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_PART_NOT_FOUND, "part 不存在"))?;
        validate_batch_version(batch.id, req.version, batch.version)?;
        let from = PartStatus::from_str(&batch.status)
            .ok_or_else(|| AppError::biz(code::BIZ_INVALID_VALUE, "batch.status 非法"))?;
        ensure_transition(
            from,
            PartStatus::INSPECTION,
            "receive-from-outsource-to-inspection",
        )?;
        if from != PartStatus::OUTSOURCE {
            return Err(AppError::biz(
                code::BIZ_INVALID_TRANSITION,
                "receive-from-outsource-to-inspection: 源状态必须是 OUTSOURCE",
            ));
        }
        validate_shelf_zone(repo.conn_mut(), req.shelf_id, "INSPECTION").await?;
        let n = mark_batch_with_status_and_meta(
            repo.conn_mut(),
            batch.id,
            req.version,
            "INSPECTION",
            Some("INSPECTION_SHELF"),
            Some(req.shelf_id),
            None,
            // 2026-09-30：出池（转 INSPECTION）→ current_process_id 置 NULL
            None,
            current.id,
        )
        .await?;
        if n == 0 {
            return Err(AppError::biz(code::VERSION_CONFLICT, "batch 版本冲突"));
        }
        // Phase 2：标 shipment RECEIVED + 写 quote event RECEIVED
        let shipment_row: Option<(i64, i64, i32)> = sqlx::query_as(
            "SELECT id, quote_id, version FROM t_outsource_shipment \
                 WHERE batch_id = $1 AND deleted_at IS NULL AND status = 'OUTSOURCING'",
        )
        .bind(batch.id)
        .fetch_optional(repo.conn_mut())
        .await?;
        if let Some((sid, qid, sver)) = shipment_row {
            let marked = sqlx::query(
                "UPDATE t_outsource_shipment SET status = 'RECEIVED', received_at = now(), \
                     version = version + 1, updated_at = now(), updated_by = $2 \
                     WHERE id = $1 AND version = $3 AND deleted_at IS NULL",
            )
            .bind(sid)
            .bind(current.id)
            .bind(sver)
            .execute(repo.conn_mut())
            .await?;
            if marked.rows_affected() == 0 {
                return Err(AppError::biz(
                    code::VERSION_CONFLICT,
                    format!("shipment {sid} 版本冲突"),
                ));
            }
            if qid != 0 {
                sqlx::query(
                    "INSERT INTO t_outsource_quote_event \
                         (id, quote_id, event_type, from_status, to_status, note, created_by) \
                         VALUES ($1, $2, 'RECEIVED', 'APPROVED', 'APPROVED', $3, $4)",
                )
                .bind(snowflake.next_id())
                .bind(qid)
                .bind(req.note.as_deref())
                .bind(current.id)
                .execute(repo.conn_mut())
                .await?;
            }
        }
        repo.insert_part_event(NewPartEvent {
            id: snowflake.next_id(),
            part_id,
            // 列宽硬约束（2026-10-03 核对）：`t_part_event.event_type` 是
            // `varchar(30)`，字面量超 30 字符 → PG 22001 使**整个事务**回滚。本字面量
            // 22 字符、语义为「外协收回 → 直接进品检」。
            // WS 事件名 `PART_RECEIVED_FROM_OUTSOURCE_INSPECTED` 不在本 payload 内、
            // 不受该列宽约束，逐字不变。
            event_type: "RECEIVED_TO_INSPECTION",
            from_status: Some("OUTSOURCE"),
            to_status: Some("INSPECTION"),
            batch_id: Some(batch.id),
            quantity: Some(batch.quantity),
            drawing_code: Some(&part.drawing_no),
            badge_code: None,
            note: req.note.as_deref(),
            created_by: Some(current.id),
        })
        .await?;
        let fresh = repo
            .get_part_inspected(part_id)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_PART_NOT_FOUND, "receive 后查不到"))?;
        Ok(crate::modules::part::vo::PartOut::from(fresh))
    }
}

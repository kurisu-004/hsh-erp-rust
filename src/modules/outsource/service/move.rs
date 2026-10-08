//! outsource 域 service —— `POST /api/v2/outsource-queue/move` 三合一移动端点
//! （2026-10-09 新增）
//!
//! 三个单边端点（`prod::batch` 的外协收发）的全部守卫与写入合并到本文件，
//! 收成**一个带 `from` / `to` 的移动端点**：
//!
//! | `from` | `to` | 目标状态 | `current_process_id` | `current_process_step_id` |
//! |---|---|---|---|---|
//! | `PRODUCTION_SHELF` | `OUTSOURCE_COMPANY` | `OUTSOURCE` | **不变**（仍指外协工序） | 按同一工序重解析 |
//! | `OUTSOURCE_COMPANY` | `PRODUCTION_SHELF` | `IN_PROCESS` | **推进**到 `next_process_id` | 按新工序重解析 |
//! | `OUTSOURCE_COMPANY` | `INSPECTION_SHELF` | `INSPECTION` | **置 NULL**（出池） | **置 NULL**（出池） |
//!
//! 三处对旧行为的**有意收窄**，理由都在注释里：
//! - **不带 `quantity`**（整批语义，部分收发先走共用拆批端点
//!   `POST /api/v2/batches/split`）；
//! - **不带 `process_id`**（外协工序 = 批次当前所属工序，由后端自推）；
//! - **不再返回 `PartOut`**（part 级 VO 对批次级看板无用，返回批次级
//!   [`OutsourceMoveResult`]）。
//!
//! 第四处是**形态**收窄（不是入参）：发送的起点恒为「在生产架上的批次」，故
//! `location IS NULL` 的 `PENDING` 批次（还没上架）不再能直接发外协，要先
//! `place-on-shelf`。这与看板候选卡一致（那类行的 `shelf_id` 序列化成空串，VO 的 doc
//! 逐字写了「会被写端点拒收」），旧单边端点允许直接从 `PENDING` 发是绕过看板的旁路。
//!
//! ## 依赖形态
//! 形参收 `&mut PgConnection`（不收胖 trait）—— 与 `prod::queue::QueueService` / 本域
//! `board::OutsourceQueueService` 同款：写路径要同时经批次、零件、工序、报价、公司
//! 映射、货架六组既有 repo 入口取数，逐个套一层 `R: XxxRepoTrait` 只会把同一句
//! `&mut *conn` 藏进多层泛型里。批次与零件的读写仍经 part 域 trait
//! （`find_batch_by_id` / `get_part_inspected` / `insert_part_event`），工序 / 报价 /
//! 公司映射经本域 trait —— 两者的默认实现都直接落在 `&mut PgConnection` 上。
//!
//! ## 事务边界
//! handler `pool.begin()` → 本 service → `tx.commit()`；**WS 广播在 commit 之后**
//! （handler 负责）。本 service 只在同一连接上读写，对事务的存在无知。

use sqlx::{AssertSqlSafe, PgConnection};

use crate::auth::rbac::{CurrentUser, Role};
use crate::infra::snowflake::SnowflakeIdGenerator;
use crate::modules::outsource::dto::{OutsourceLocation, OutsourceMoveRequest};
use crate::modules::outsource::repo::OutsourceRepoTrait;
use crate::modules::outsource::repo::sql::NEXT_PROCESS_LATERAL_SQL;
use crate::modules::outsource::vo::OutsourceMoveResult;
use crate::modules::part::model::{NewPartEvent, TPartInspected};
use crate::modules::part::repo::PartRepoTrait;
use crate::modules::part::statemachine::PartStatus;
use crate::shared::batch::TPartBatch;
use crate::shared::batch::guards::{
    assert_shelf_maps_process, ensure_transition, mark_batch_with_status_and_meta,
    optional_process_chain, optional_step_id, validate_batch_version, validate_shelf_zone,
};
use crate::shared::error::{AppError, code};

/// DIRECT（免审批直发）自动创建的占位报价 `note` 固定文案 —— 让对账页一眼看出
/// 「这行单价 0 不是漏填，是免审批直发自动建的占位报价」。
const DIRECT_PLACEHOLDER_NOTE: &str = "DIRECT 直发自动创建（免审批，单价待对账补录）";

/// part 事件的三个审计字面量 —— **与 WS 事件名是两件事，不要一起改**。
///
/// `t_part_event.event_type` 是 `varchar(30)`，字面量超 30 字符会让 PG 返 22001 并把
/// **整个事务**回滚（三个字面量分别 19 / 26 / 22 字符，均在限内）。
///
/// 它们记录的是**发生了什么业务事实**（哪个方向、去了哪），前端在工单时间线上按它
/// 分组；WS 事件 `OUTSOURCE_MOVE_DONE` 记的是**传输层的一次移动完成**（payload 带
/// from/to 让前端自己推断方向）。合并 WS 事件时这三个字面量逐字保留，否则历史时间线
/// 的分组口径会在新写入的行上断裂。
const EVENT_SENT_TO_OUTSOURCE: &str = "SENT_TO_OUTSOURCE";
const EVENT_RECEIVED_FROM_OUTSOURCE: &str = "RECEIVED_FROM_OUTSOURCE";
const EVENT_RECEIVED_TO_INSPECTION: &str = "RECEIVED_TO_INSPECTION";

/// 推导「下一道工序」的 SQL（`to.kind = PRODUCTION_SHELF` 且省略 `next_process_id`
/// 时跑一次）。
///
/// `LEFT JOIN LATERAL` 片段取自 `repo/sql.rs::NEXT_PROCESS_LATERAL_SQL`（**与看板
/// 在途卡片的 `receive_next_process_id` 同一份口径**，理由见该常量 doc）。
///
/// 额外的 `LEFT JOIN t_process_chain_step cur` 只为投影诊断列 `anchor_chain_id`，不
/// 参与推导（推导完全发生在 `nx` 片段内部）。两列一起取是为了让 `20706` 的文案能区分
/// 「压根没链」与「有链但推不出」两种成因 —— 只看 `next_process_id = 0` 无法区分，
/// 而两种成因的处置完全不同（前者要去制定工序链，后者运营自己知道该进哪道工序、
/// 手填即可）。
const SQL_DERIVE_NEXT_PROCESS: &str = "SELECT COALESCE(nx.next_process_id, 0) AS next_process_id, \
     COALESCE(p.process_chain_id, cur.chain_id) AS anchor_chain_id \
     FROM t_part_batch pb \
     JOIN t_part p ON p.id = pb.part_id AND p.deleted_at IS NULL \
     LEFT JOIN t_process_chain_step cur \
       ON cur.id = pb.current_process_step_id AND cur.deleted_at IS NULL \
     {nx} \
     WHERE pb.id = $1 AND pb.deleted_at IS NULL";

/// `SQL_DERIVE_NEXT_PROCESS` 的行（推导值 + 诊断值）。
#[derive(Debug, Clone, sqlx::FromRow)]
struct DerivedNextProcessRow {
    /// `COALESCE(nx.next_process_id, 0)` —— **0 = 推不出**（0 兜底口径，非 nullable）。
    next_process_id: i64,
    /// `COALESCE(p.process_chain_id, cur.chain_id)` —— `None` = 压根没链。
    anchor_chain_id: Option<i64>,
}

/// 三合一移动端点 service（unit struct；handler 借 `&mut *conn` 传入，与
/// `board::OutsourceQueueService` / `prod::queue::QueueService` 同形）。
pub struct OutsourceMoveService;

impl Default for OutsourceMoveService {
    fn default() -> Self {
        Self
    }
}

impl OutsourceMoveService {
    pub fn new() -> Self {
        Self
    }

    /// `POST /api/v2/outsource-queue/move` 业务逻辑。
    ///
    /// ## 守卫顺序（逐条带错误码，顺序本身是契约）
    ///
    /// 1. **角色**（Manager + Clerk + Inspector，沿用被取代的三个旧端点口径）→
    ///    `40301`；service 入口第一行。
    /// 2. **`from.kind == to.kind`** → `40001 VALIDATION_ERROR`（照
    ///    `prod::queue::service::queue::move_batch` 的同款判定）。**在查批次之前判**：
    ///    同 kind 移动是**请求本身的形状错误**，与哪一批货无关 —— 先查库会让「批次不
    ///    存在」掩盖「你把同一个位置同时当成起点和终点」，调用方拿到错误码会去查批次，
    ///    而真正要改的是 body。
    /// 3. **批次存在**（软删视为不存在）→ `20109 BIZ_PART_BATCH_NOT_FOUND`。
    /// 4. **OCC**：`validate_batch_version` → `40901 VERSION_CONFLICT`。
    /// 5. **状态机**：`ensure_transition(from_status, target_status)` →
    ///    `20103 BIZ_INVALID_TRANSITION`；两个回收方向额外显式要求源为 `OUTSOURCE`
    ///    → `20103`（白名单里 `PENDING → IN_PROCESS` / `PENDING → INSPECTION` 也是
    ///    合法边，但「从 PENDING 收回」不是看板语义，不靠白名单隐式挡）。
    /// 6. **`from` 锚点**：`from` 必须等于批次真实 `(location, current_holder_id)` →
    ///    `20122 BIZ_BATCH_LOCATION_MISMATCH`；另有一条额外不变量（源为 `IN_PROCESS`
    ///    时 location 必须是 `PRODUCTION_SHELF`）→ `20103`，见
    ///    [`assert_from_matches_batch`]。
    /// 7. **`to` 侧分方向校验**：见三个臂各自注释，逐条不丢旧守卫。
    ///
    /// ## 写入口
    /// 唯一写 `t_part_batch.status` 的入口是 [`mark_batch_with_status_and_meta`]
    /// （`shared::batch::status` 的薄包装）—— `cargo test --lib` 的
    /// `write_guard_tests::no_outside_file_writes_batch_status` 扫全 `src/**/*.rs`，
    /// 本文件不写任何 `UPDATE t_part_batch SET status …`。该入口的 0 行（版本已变 /
    /// 源状态不在白名单 / 批次已软删）在其内部即转 `40901`，故本函数**不需要**再判
    /// 返回行数（旧端点里那条 `if n == 0` 是恒假分支）。
    #[allow(clippy::too_many_lines)]
    pub async fn move_batch(
        conn: &mut PgConnection,
        snowflake: &SnowflakeIdGenerator,
        req: OutsourceMoveRequest,
        current: &CurrentUser,
    ) -> Result<OutsourceMoveResult, AppError> {
        // ① 角色守卫（service 入口第一行）
        current.require_any_role(&[Role::Manager, Role::Clerk, Role::Inspector])?;

        let from_kind = location_kind(&req.from);
        let to_kind = location_kind(&req.to);

        // ② 同 kind 移动 → 请求形状错误（在查批次之前判，见方法 doc）
        if from_kind == to_kind {
            return Err(AppError::validation(format!(
                "outsource-queue/move 同 kind 移动非法（from={from_kind} to={to_kind}）；应跨 kind 移动"
            )));
        }

        // ③ 批次存在
        let batch = (&mut *conn)
            .find_batch_by_id(req.batch_id)
            .await?
            .ok_or_else(|| {
                AppError::biz(
                    code::BIZ_PART_BATCH_NOT_FOUND,
                    format!("batch {} 不存在或已软删", req.batch_id),
                )
            })?;
        let part_id = batch.part_id;

        // ④ OCC
        validate_batch_version(batch.id, req.version, batch.version)?;

        // ⑤ 状态机
        let from_status = PartStatus::from_str(&batch.status).ok_or_else(|| {
            AppError::biz(
                code::BIZ_INVALID_VALUE,
                format!("batch.status 非法: {}", batch.status),
            )
        })?;
        match to_kind {
            "OUTSOURCE_COMPANY" => {
                // 发送：PENDING / IN_PROCESS 都可发（白名单两条边）
                ensure_transition(from_status, PartStatus::OUTSOURCE, "outsource-queue/move")?;
            }
            _ => {
                let target = if to_kind == "PRODUCTION_SHELF" {
                    PartStatus::IN_PROCESS
                } else {
                    PartStatus::INSPECTION
                };
                ensure_transition(from_status, target, "outsource-queue/move")?;
                // 显式钉住「回收的源只能是 OUTSOURCE」：白名单里
                // `PENDING → IN_PROCESS` / `PENDING → INSPECTION` 也是合法边，而那两条
                // 边属于「建档 / 待编程」流的语义，与外协看板无关
                if from_status != PartStatus::OUTSOURCE {
                    return Err(AppError::biz(
                        code::BIZ_INVALID_TRANSITION,
                        format!(
                            "outsource-queue/move: 回收方向的源状态必须是 OUTSOURCE，当前 {}",
                            from_status.as_str()
                        ),
                    ));
                }
            }
        }

        // ⑥ from 锚点校验
        assert_from_matches_batch(&batch, &req.from, from_status)?;

        // `quote_id` / `direct` 只服务发送方向。放在守卫 ⑦ 之前：它是**请求形状**的判定
        // （不是发送方向的业务规则），与下面两个臂里的价来源判定分开守。
        if to_kind != "OUTSOURCE_COMPANY" && (req.quote_id.is_some() || req.direct.is_some()) {
            return Err(AppError::biz(
                code::BIZ_INVALID_VALUE,
                format!(
                    "outsource-queue/move: quote_id / direct 只对发送方向（to.kind=\
                     OUTSOURCE_COMPANY）有意义，本次 to.kind={to_kind}"
                ),
            ));
        }

        // 零件行（part 域 trait）：part 事件要记 drawing_code，且 part 不存在时派生层
        // 无从回填 —— 早于任何写。
        let part = (&mut *conn)
            .get_part_inspected(part_id)
            .await?
            .ok_or_else(|| {
                AppError::biz(
                    code::BIZ_PART_NOT_FOUND,
                    format!("batch {} 关联 part {part_id} 不存在", batch.id),
                )
            })?;

        // 三臂分派产出的出参增量
        let mut shipment_id: Option<i64> = None;
        let mut new_process_id: Option<i64> = None;
        let new_holder_id: i64;

        match (&req.from, &req.to) {
            // ── ① 生产架 → 外协公司（发送）───────────────────────────────
            (
                OutsourceLocation::ProductionShelf { .. },
                OutsourceLocation::OutsourceCompany { company_id },
            ) => {
                // 外协加工的工序 = 批次当前所属工序（DTO doc 的「为什么不带 process_id」）
                let process_id = batch.current_process_id.ok_or_else(|| {
                    AppError::biz(
                        code::BIZ_INVALID_VALUE,
                        format!(
                            "outsource-queue/move: batch {} 的 current_process_id 为空，\
                             无法判定外协工序",
                            batch.id
                        ),
                    )
                })?;
                let (quote_id, price) = resolve_send_quote(
                    conn,
                    snowflake,
                    part_id,
                    *company_id,
                    process_id,
                    &req,
                    current,
                )
                .await?;
                let chain_id = optional_process_chain(conn, part_id).await?;
                let step_id = optional_step_id(conn, chain_id, process_id).await?;
                shipment_id = Some(
                    write_send(
                        conn,
                        snowflake,
                        &batch,
                        &part,
                        *company_id,
                        process_id,
                        step_id,
                        quote_id,
                        price,
                        req.note.as_deref(),
                        current,
                    )
                    .await?,
                );
                new_holder_id = *company_id;
            }

            // ── ② 外协公司 → 生产架（回收生产，工序推进）─────────────────
            (
                OutsourceLocation::OutsourceCompany { .. },
                OutsourceLocation::ProductionShelf {
                    shelf_id,
                    next_process_id,
                },
            ) => {
                // 货架守卫：存在（20501）/ 启用（20512）/ zone=PRODUCTION（20104）
                validate_shelf_zone(conn, *shelf_id, "PRODUCTION").await?;
                let next_process =
                    resolve_receive_next_process(conn, batch.id, *next_process_id).await?;
                // 货架必须映射该工序（20507）：否则批次落到一个「不做这道工序」的架上，
                // 取件 SQL 硬限定 `zone='PRODUCTION'` 但工序池按 process 查，批次会静默
                // 失联
                assert_shelf_maps_process(conn, *shelf_id, next_process).await?;
                let chain_id = optional_process_chain(conn, part_id).await?;
                let step_id = optional_step_id(conn, chain_id, next_process).await?;
                mark_batch_with_status_and_meta(
                    conn,
                    batch.id,
                    batch.version,
                    "IN_PROCESS",
                    Some("PRODUCTION_SHELF"),
                    Some(*shelf_id),
                    step_id,
                    Some(next_process),
                    current.id,
                )
                .await?;
                close_open_outsource_shipment(
                    conn,
                    snowflake,
                    batch.id,
                    req.note.as_deref(),
                    current,
                )
                .await?;
                write_part_event(
                    conn,
                    snowflake,
                    &batch,
                    &part,
                    EVENT_RECEIVED_FROM_OUTSOURCE,
                    "OUTSOURCE",
                    "IN_PROCESS",
                    req.note.as_deref(),
                    current,
                )
                .await?;
                new_holder_id = *shelf_id;
                new_process_id = Some(next_process);
            }

            // ── ③ 外协公司 → 品检架（回收直送品检，出池清工序列）─────────
            (
                OutsourceLocation::OutsourceCompany { .. },
                OutsourceLocation::InspectionShelf { shelf_id },
            ) => {
                validate_shelf_zone(conn, *shelf_id, "INSPECTION").await?;
                mark_batch_with_status_and_meta(
                    conn,
                    batch.id,
                    batch.version,
                    "INSPECTION",
                    Some("INSPECTION_SHELF"),
                    Some(*shelf_id),
                    // 出池（转 INSPECTION）→ step 与 process 两列都按出池不变式清 NULL
                    // （形参 `None` ⇒ 写入口的 `clear_*` 分支，调用点清单见
                    // `shared::batch::guards::mark_batch_with_status_and_meta`）
                    None,
                    None,
                    current.id,
                )
                .await?;
                close_open_outsource_shipment(
                    conn,
                    snowflake,
                    batch.id,
                    req.note.as_deref(),
                    current,
                )
                .await?;
                write_part_event(
                    conn,
                    snowflake,
                    &batch,
                    &part,
                    EVENT_RECEIVED_TO_INSPECTION,
                    "OUTSOURCE",
                    "INSPECTION",
                    req.note.as_deref(),
                    current,
                )
                .await?;
                new_holder_id = *shelf_id;
            }

            // `from` 侧只能是生产架或外协公司（品检架不是合法起点，守卫 ⑥ 已拒），
            // 同 kind 已在守卫 ② 拦掉；这条分支让编译器的穷尽性检查通过
            #[allow(unreachable_patterns)]
            _ => unreachable!("同 kind 移动与非法起点已在守卫 ② / ⑥ 拦截"),
        }

        // 写后读回真实 version（**不在 Rust 里算 `batch.version + 1`**），理由见
        // `vo/queue.rs::OutsourceMoveResult::version`
        let fresh = (&mut *conn)
            .find_batch_by_id(batch.id)
            .await?
            .ok_or_else(|| {
                AppError::biz(
                    code::BIZ_PART_BATCH_NOT_FOUND,
                    format!("move 后读回 batch {} 失败", batch.id),
                )
            })?;

        Ok(OutsourceMoveResult {
            batch_id: batch.id,
            part_id,
            from_kind: from_kind.to_string(),
            to_kind: to_kind.to_string(),
            new_holder_id,
            // `new_location` 恒等于 `to.kind`：DTO 的 `kind` 字面与 `t_part_batch.location`
            // 取值逐字对齐是本设计的前提（见 `dto.rs::OutsourceLocation` 的 doc）
            new_location: to_kind.to_string(),
            version: fresh.version,
            shipment_id,
            new_process_id,
        })
    }
}

/// `location` 的 `kind` 字面（= `t_part_batch.location` 取值，DTO doc 的对齐前提）。
fn location_kind(loc: &OutsourceLocation) -> &'static str {
    match loc {
        OutsourceLocation::ProductionShelf { .. } => "PRODUCTION_SHELF",
        OutsourceLocation::OutsourceCompany { .. } => "OUTSOURCE_COMPANY",
        OutsourceLocation::InspectionShelf { .. } => "INSPECTION_SHELF",
    }
}

/// 守卫 ⑥：`from` 必须等于批次真实 `(location, current_holder_id)`，否则 `20122`。
///
/// ## 为什么额外保留一条 `20103` 的 `IN_PROCESS` 不变量
/// 发送方向的源是 `IN_PROCESS` 批次，按写入不变式（进池 = `IN_PROCESS` +
/// `location='PRODUCTION_SHELF'`）它**必须**在生产架上。`from.kind=PRODUCTION_SHELF`
/// 这一条已经能拒绝「不在生产架上的 `IN_PROCESS` 批次」，但那时它拿到的是 `20122`
/// 「货架对不上」—— 对一个本来就不该出现在看板候选列里的批次说「货架对不上」，指向性
/// 差（调用方会去核对 shelf_id，而真正的原因是这批货的位置本身不合规）。故先判状态
/// 语义（`20103`，沿用旧端点的错误码与文案），再判 holder 匹配（`20122`）。
///
/// 这条守卫历史上**曾经不可达**：状态机白名单里一度没有 `IN_PROCESS → OUTSOURCE`，
/// `ensure_transition` 一定先把 `IN_PROCESS` 源拒掉，于是「可发送一览的行（按写入不变式
/// 几乎全是 `IN_PROCESS` 源）发一单就被 20103 拒」，端到端实测下外协发送 100% 不可用。
/// 状态机补边后它才真正承担 location 不变式 —— 别因为「现在 `from` 守卫已经能拒」就把
/// 它删掉。
///
/// ## `from.kind = INSPECTION_SHELF` 一律拒
/// 批次离开品检架只有一条路（品检通过 / 打回，走 `prod::batch` 的 `to-ship` /
/// `scan-inspect`），与外协看板无关。给 `20122` 而不是 `40001` 是为了与「from 与真实
/// 位置不符」归为同一类：都是「你指的起点不对」。
fn assert_from_matches_batch(
    batch: &TPartBatch,
    from: &OutsourceLocation,
    from_status: PartStatus,
) -> Result<(), AppError> {
    let mismatch = |what: String| {
        AppError::biz(
            code::BIZ_BATCH_LOCATION_MISMATCH,
            format!("batch {} {what}", batch.id),
        )
    };
    match from {
        OutsourceLocation::ProductionShelf { shelf_id, .. } => {
            if from_status == PartStatus::IN_PROCESS
                && batch.location.as_deref() != Some("PRODUCTION_SHELF")
            {
                return Err(AppError::biz(
                    code::BIZ_INVALID_TRANSITION,
                    format!(
                        "outsource-queue/move: IN_PROCESS 批次必须在 PRODUCTION_SHELF 上，\
                         当前 location={:?}",
                        batch.location
                    ),
                ));
            }
            if batch.location.as_deref() != Some("PRODUCTION_SHELF") {
                return Err(mismatch(format!(
                    "当前 location={:?}，from.kind=PRODUCTION_SHELF 期望 'PRODUCTION_SHELF'",
                    batch.location
                )));
            }
            if batch.current_holder_id != Some(*shelf_id) {
                return Err(mismatch(format!(
                    "current_holder_id={:?} 与 from.shelf_id={shelf_id} 不一致",
                    batch.current_holder_id
                )));
            }
            Ok(())
        }
        OutsourceLocation::OutsourceCompany { company_id } => {
            if batch.location.as_deref() != Some("OUTSOURCE_COMPANY") {
                return Err(mismatch(format!(
                    "当前 location={:?}，from.kind=OUTSOURCE_COMPANY 期望 'OUTSOURCE_COMPANY'",
                    batch.location
                )));
            }
            if batch.current_holder_id != Some(*company_id) {
                return Err(mismatch(format!(
                    "current_holder_id={:?} 与 from.company_id={company_id} 不一致",
                    batch.current_holder_id
                )));
            }
            Ok(())
        }
        OutsourceLocation::InspectionShelf { .. } => Err(mismatch(
            "from.kind=INSPECTION_SHELF 不是合法起点（品检架上的批次不走外协看板）".to_string(),
        )),
    }
}

/// 回收方向的目标工序：调用方给了就用，没给就按工序链推导，推不出返 `20706`。
///
/// **文案按「推不出的成因」分两段**（三合一端点里 `next_process_id` 只存在于回收方向
/// —— 旧两个端点中「发送侧」压根不吃这个字段，故两种成因都落在回收这一步）：
/// - 压根没有锚链（零件未绑工艺链 / 批次无 step 指针可回退）⇒「请先制定工序链」；
/// - 有锚链但推不出下一 step（链内当前工序缺失、指针漂移、已是最后一步）⇒「无法推导
///   下一道工序，请手填」—— 这类批次运营自己知道该进哪道工序，手填即可。
async fn resolve_receive_next_process(
    conn: &mut PgConnection,
    batch_id: i64,
    given: Option<i64>,
) -> Result<i64, AppError> {
    if let Some(pid) = given {
        return Ok(pid);
    }
    let sql = SQL_DERIVE_NEXT_PROCESS.replace("{nx}", NEXT_PROCESS_LATERAL_SQL);
    let row: DerivedNextProcessRow = sqlx::query_as(AssertSqlSafe(sql))
        .bind(batch_id)
        .fetch_optional(&mut *conn)
        .await?
        .ok_or_else(|| {
            // 批次刚校验过存在（守卫 ③），故这一支只在「批次所属 part 被并发软删」时可达
            AppError::biz(
                code::BIZ_PART_BATCH_NOT_FOUND,
                format!("推导下一道工序时查不到 batch {batch_id}"),
            )
        })?;
    if row.next_process_id != 0 {
        return Ok(row.next_process_id);
    }
    Err(if row.anchor_chain_id.is_some() {
        AppError::biz(
            code::BIZ_PROCESS_CHAIN_REQUIRED,
            format!(
                "batch {batch_id}：工序链缺失或指针漂移，无法推导下一道工序，请手填 \
                 to.next_process_id（或补全该零件的工序链）"
            ),
        )
    } else {
        AppError::biz(
            code::BIZ_PROCESS_CHAIN_REQUIRED,
            format!(
                "batch {batch_id}：该零件尚未制定工序链，无法推导外协收回后的下一道工序；\
                 请先制定工序链，或手填 to.next_process_id"
            ),
        )
    })
}

/// 发送方向的全部守卫 + 价来源解析，返回本次 shipment 引用的 `(quote_id, 单价)`。
///
/// 逐条沿用旧 `send_to_outsource` 的守卫，顺序即错误码归因的优先级：
/// 1. **公司存在**（未软删）→ `21201`；
/// 2. **公司启用** → `21205`；
/// 3. **工序类别是 OUTSOURCE** → `20104`（工序不存在 → `20801`）；
/// 4. **公司映射该工序**（`t_outsource_company_process` 有未软删行）→ `20104`；
/// 5. **`direct` 与 `quote_id` 恰给一个** → `20104`；
/// 6. **`requires_approval` 的工序不许 `direct=true`** → `20104`；
/// 7. 价来源解析：DIRECT 复用 `(part, company, process)` 的活跃 APPROVED 报价，没有就
///    自动建 `price=0` 占位报价；APPROVAL 用调用方给的 `quote_id`；
/// 8. **报价存在** → `21301` / **状态 APPROVED** → `21307` /
///    **(part, company, process) 三元组一致** → `21302` /
///    **APPROVAL 路径拒 DIRECT 占位价** → `21307`。
///
/// ## 第 3 / 4 条为什么不能省
/// 只校验「工序存在」的话，把货派给一道**非外协**工序（例如内部装配工序）也会通过，
/// 批次随后被标成 `OUTSOURCE` + holder 写外协公司，库里会出现「这道工序由不需要它的
/// 公司加工」的脏关系；而 `t_outsource_company.is_active` 只说公司在册，与「公司有没有
/// 这项能力」是两件事。
///
/// ## 第 6 条为什么不能省（读侧 ≠ 写侧）
/// `requires_approval` 在读侧候选卡里生效（「需审批但无审批报价的行不出候选」），写侧
/// 零校验时绕过 UI 直接调本端点传 `direct=true`，就能对「先审批再发」这道业务规则下该
/// 走报价的工序直发，系统里没有任何一处强制。两侧同时守才闭环：读侧决定「看不看得见」，
/// 写侧决定「发不发得成」。
///
/// ## 第 8 条末项的判据是 `!direct` 而不是「凡 `is_direct=true` 就拒」
/// DIRECT 路径的价来源判定复用占位报价是**既有正确行为**（免审批直发本就没有审批价），
/// 而该路径在需审批工序上已被第 6 条整体拦掉，不会走到末项。两条路径的价来源判定必须
/// 分开。
#[allow(clippy::too_many_arguments)]
async fn resolve_send_quote(
    conn: &mut PgConnection,
    snowflake: &SnowflakeIdGenerator,
    part_id: i64,
    company_id: i64,
    process_id: i64,
    req: &OutsourceMoveRequest,
    current: &CurrentUser,
) -> Result<(i64, rust_decimal::Decimal), AppError> {
    let direct = req.direct.unwrap_or(false);

    let company_active: Option<bool> = sqlx::query_scalar(
        "SELECT is_active FROM t_outsource_company WHERE id = $1 AND deleted_at IS NULL",
    )
    .bind(company_id)
    .fetch_optional(&mut *conn)
    .await?;
    let Some(is_active) = company_active else {
        return Err(AppError::biz(
            code::BIZ_OUTSOURCE_COMPANY_NOT_FOUND,
            format!("outsource_company {company_id} 不存在"),
        ));
    };
    if !is_active {
        return Err(AppError::biz(
            code::BIZ_OUTSOURCE_COMPANY_IN_USE,
            format!("outsource_company {company_id} 已停用"),
        ));
    }

    let category = (&mut *conn)
        .process_get_category(process_id)
        .await?
        .ok_or_else(|| {
            AppError::biz(
                code::BIZ_PROCESS_NOT_FOUND,
                format!("process {process_id} 不存在"),
            )
        })?;
    if category != "OUTSOURCE" {
        return Err(AppError::biz(
            code::BIZ_INVALID_VALUE,
            format!(
                "outsource-queue/move: process {process_id} 的 category={category}，\
                 外协派发必须走 OUTSOURCE 类别的工序"
            ),
        ));
    }

    let mapped = (&mut *conn)
        .junction_list_company_ids_by_process(process_id)
        .await?;
    if !mapped.contains(&company_id) {
        return Err(AppError::biz(
            code::BIZ_INVALID_VALUE,
            format!(
                "outsource-queue/move: outsource_company {company_id} 未映射外协工序 {process_id}"
            ),
        ));
    }

    if direct && req.quote_id.is_some() {
        return Err(AppError::biz(
            code::BIZ_INVALID_VALUE,
            "outsource-queue/move: direct=true 与 quote_id 互斥（DIRECT 免审批直发；\
             APPROVAL 模式请只传 quote_id）",
        ));
    }
    if !direct && req.quote_id.is_none() {
        return Err(AppError::biz(
            code::BIZ_INVALID_VALUE,
            "outsource-queue/move: 必须给价来源——direct=true（免审批直发）或 \
             quote_id（APPROVED 报价）",
        ));
    }

    // `unwrap_or(true)` = 保守默认（宁可拒，不放行）：上一道守卫已确认工序存在，这里
    // `None` 只可能来自并发软删
    let requires_approval = (&mut *conn)
        .process_get_requires_approval(process_id)
        .await?
        .unwrap_or(true);
    if direct && requires_approval {
        return Err(AppError::biz(
            code::BIZ_INVALID_VALUE,
            format!(
                "outsource-queue/move: 外协工序 {process_id} requires_approval=true，\
                 该工序需要报价审批，请先走审批（传 quote_id）再发货，不能 direct 直发"
            ),
        ));
    }

    let quote_id: i64 = match (direct, req.quote_id) {
        (true, None) => {
            resolve_direct_quote_id(conn, snowflake, part_id, company_id, process_id, current)
                .await?
        }
        (false, Some(qid)) => qid,
        // 上面两条守卫已把 (true, Some) 与 (false, None) 拒掉；仍给一条可诊断的拒绝
        // 而不是 unwrap panic —— 守卫若将来被调整，表现为 400 而不是进程崩
        _ => {
            return Err(AppError::biz(
                code::BIZ_INVALID_VALUE,
                "outsource-queue/move: direct 与 quote_id 必须恰给一个",
            ));
        }
    };

    let (status, price, q_part_id, q_company_id, q_process_id, q_is_direct) =
        fetch_quote(conn, quote_id).await?;
    if status != "APPROVED" {
        return Err(AppError::biz(
            code::BIZ_OUTSOURCE_QUOTE_NOT_APPROVED,
            format!("quote {quote_id} 当前 {status}，非 APPROVED 不可发送"),
        ));
    }
    if q_part_id != part_id || q_company_id != company_id || q_process_id != process_id {
        return Err(AppError::biz(
            code::BIZ_OUTSOURCE_QUOTE_INVALID_TRANSITION,
            format!("quote {quote_id} 与本次 move 参数不一致（part/company/process）"),
        ));
    }
    // 占位报价在库里已经存在（守卫上线前建的，或建完之后该工序的 `requires_approval`
    // 由 false 被 `PATCH /prod/processes/{id}` 翻成 true），所以这道守卫必须落在端点里，
    // 不能靠清数据。排在三元组校验之后：两条互不依赖，同一请求可同时命中，先判三元组
    // 时归因才准（21302 直指参数不一致，是调用方唯一改得动的那一条）
    if !direct && q_is_direct {
        return Err(AppError::biz(
            code::BIZ_OUTSOURCE_QUOTE_NOT_APPROVED,
            format!(
                "quote {quote_id} 是免审批直发的占位价（is_direct=true、price=0），\
                 不能作为审批价来源；请改传该 (part, company, process) 经审批的报价；\
                 若该工序 requires_approval=false，请改用 direct=true"
            ),
        ));
    }
    Ok((quote_id, price))
}

/// 取一行报价的 `(status, price, part_id, company_id, process_id, is_direct)`。
///
/// 走裸 `sqlx::query_as` 而非 `OutsourceRepoTrait::quote_get_by_id`：本端点要的是 6 列
/// 元组（含 `is_direct` —— 占位价判定的依据），而 trait 的报价读按「(part, process)
/// 活跃列表」组织，与这里「按主键取、用于发送前的最后校验」不同形。
async fn fetch_quote(
    conn: &mut PgConnection,
    quote_id: i64,
) -> Result<(String, rust_decimal::Decimal, i64, i64, i64, bool), AppError> {
    let row: Option<(String, rust_decimal::Decimal, i64, i64, i64, bool)> = sqlx::query_as(
        "SELECT status, price, part_id, outsource_company_id, process_id, is_direct \
         FROM t_outsource_quote \
         WHERE id = $1 AND deleted_at IS NULL",
    )
    .bind(quote_id)
    .fetch_optional(&mut *conn)
    .await?;
    row.ok_or_else(|| {
        AppError::biz(
            code::BIZ_OUTSOURCE_QUOTE_NOT_FOUND,
            format!("quote {quote_id} 不存在"),
        )
    })
}

/// 找 `(part, company, process)` 三元组上的活跃 APPROVED 报价 id（多条取 id 最大者
/// = 最新审批）。
///
/// 活跃口径复用 `OutsourceRepoTrait::quote_list_active_by_part_process`（`SUBMITTED` /
/// `APPROVED` 未软删），再按公司过滤 + 只取 APPROVED。
async fn find_approved_quote_id(
    conn: &mut PgConnection,
    part_id: i64,
    company_id: i64,
    process_id: i64,
) -> Result<Option<i64>, AppError> {
    let rows = (&mut *conn)
        .quote_list_active_by_part_process(part_id, process_id, 0)
        .await?;
    Ok(rows
        .into_iter()
        .filter(|q| q.outsource_company_id == company_id && q.status == "APPROVED")
        .max_by_key(|q| q.id)
        .map(|q| q.id))
}

/// DIRECT（免审批直发）的价来源解析：复用 `(part, company, process)` 的活跃 APPROVED
/// 报价，没有则 INSERT 一条 `price = 0` / `status='APPROVED'` / `is_direct = true` 的
/// 占位报价。
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
/// 两条都必须有：审批报价**故意**排除 `is_direct = true`（这正是 `is_direct` 列存在的
/// 意义 —— 免审批直发不该占用审批报价的唯一键），代价是单靠它兜不住 DIRECT 行。
/// migration 008 补上后半条，于是下面的 `ON CONFLICT DO NOTHING` + 回查才真正成立：
/// 并发下第二个 INSERT 命中该索引 → 0 行 → 回查取第一条的 id 当 `quote_id`，同一 tuple
/// 恒定只留一条 0 元占位报价。
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
    // 命中 DIRECT 部分的 partial 唯一索引（并发窗口内同一 tuple 已被别的请求建过占位
    // 报价）→ 回查复用它，不留第二条等价记录
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

/// 发送方向的全部写入，返回新建的 shipment id。
///
/// 顺序照旧端点：批次状态 → shipment → 报价事件 `SENT` → part 事件
/// `SENT_TO_OUTSOURCE`（同事务，任一失败整体回滚）。
#[allow(clippy::too_many_arguments)]
async fn write_send(
    conn: &mut PgConnection,
    snowflake: &SnowflakeIdGenerator,
    batch: &TPartBatch,
    part: &TPartInspected,
    company_id: i64,
    process_id: i64,
    step_id: Option<i64>,
    quote_id: i64,
    price: rust_decimal::Decimal,
    note: Option<&str>,
    current: &CurrentUser,
) -> Result<i64, AppError> {
    mark_batch_with_status_and_meta(
        conn,
        batch.id,
        batch.version,
        "OUTSOURCE",
        Some("OUTSOURCE_COMPANY"),
        Some(company_id),
        // 无链时为 `None` ⇒ 写入口的 clear 分支写 NULL（step 是可选的链内位置指针）
        step_id,
        // **写外协工序本身**（不是置 NULL）：外协加工的就是这道工序，rollup 派生
        // `t_part.next_process_id` 需要它；且 `status='OUTSOURCE'` +
        // `location='OUTSOURCE_COMPANY'` 使它不可能被任何工序池查询命中（4 条池 SQL 与
        // `list_pickable_by_work_type` 都硬限定 `IN_PROCESS` + `PRODUCTION_SHELF`）——
        // 登记见 `shared::batch::model::TPartBatch::current_process_id` 的「写入不变式
        // 第 4 行的唯一例外」
        Some(process_id),
        current.id,
    )
    .await?;

    // 新建**开口** shipment。`uq_t_outsource_shipment_open_batch` 保证一个批次最多一张
    // 开口单，撞了（0 行）⇒ 这批货已经发过一次，拒收而不是静默复用旧单
    let shipment_id = snowflake.next_id();
    let inserted = sqlx::query(
        "INSERT INTO t_outsource_shipment \
             (id, quote_id, part_id, batch_id, outsource_company_id, process_id, \
              quantity, unit_price, status, sent_at, created_by, updated_by) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, 'OUTSOURCING', now(), $9, $9) \
         ON CONFLICT DO NOTHING",
    )
    .bind(shipment_id)
    .bind(quote_id)
    .bind(batch.part_id)
    .bind(batch.id)
    .bind(company_id)
    .bind(process_id)
    // **整批发送**：`quantity` = 批次当前余量。move 端点没有部分发送语义，这与 shipment
    // 的记账口径一致 —— 对账要回答的是「发出去多少、单价多少」
    .bind(batch.quantity)
    .bind(price)
    .bind(current.id)
    .execute(&mut *conn)
    .await?;
    if inserted.rows_affected() == 0 {
        return Err(AppError::biz(
            code::BIZ_OUTSOURCE_SHIPMENT_INVALID_TRANSITION,
            format!("batch {} 已有开口 shipment；不可重复发送", batch.id),
        ));
    }

    // 报价存在（APPROVAL 显式传 / DIRECT 复用或自动建）→ 写 SENT 事件（仅审计，不改
    // quote.status）
    sqlx::query(
        "INSERT INTO t_outsource_quote_event \
             (id, quote_id, event_type, from_status, to_status, note, created_by) \
         VALUES ($1, $2, 'SENT', 'APPROVED', 'APPROVED', $3, $4)",
    )
    .bind(snowflake.next_id())
    .bind(quote_id)
    .bind(note)
    .bind(current.id)
    .execute(&mut *conn)
    .await?;

    write_part_event(
        conn,
        snowflake,
        batch,
        part,
        EVENT_SENT_TO_OUTSOURCE,
        &batch.status,
        "OUTSOURCE",
        note,
        current,
    )
    .await?;
    Ok(shipment_id)
}

/// 回收时把该批次的**开口** shipment 标 `RECEIVED` + 写 quote event `RECEIVED`。
///
/// 两个回收方向共用（旧代码里「回收生产」走独立 helper、「直送品检」内联了同一段
/// SQL）。无开口 shipment 时静默跳过（外协厂直接入库的批次没有发货记录，不算错误）。
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
    // quote_id 可空（旧 shipment 兼容，列本身 NOT NULL 但历史行可能是 0）；非 0 时写
    // RECEIVED 事件
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

/// 写 `t_part_event`（批次流转的审计事实；三个方向各用自己的字面量）。
async fn write_part_event(
    conn: &mut PgConnection,
    snowflake: &SnowflakeIdGenerator,
    batch: &TPartBatch,
    part: &TPartInspected,
    event_type: &str,
    from_status: &str,
    to_status: &str,
    note: Option<&str>,
    current: &CurrentUser,
) -> Result<(), AppError> {
    (&mut *conn)
        .insert_part_event(NewPartEvent {
            id: snowflake.next_id(),
            part_id: batch.part_id,
            event_type,
            from_status: Some(from_status),
            to_status: Some(to_status),
            batch_id: Some(batch.id),
            // 整批语义：数量恒为批次当前余量
            quantity: Some(batch.quantity),
            drawing_code: Some(&part.drawing_no),
            badge_code: None,
            note,
            created_by: Some(current.id),
        })
        .await?;
    Ok(())
}

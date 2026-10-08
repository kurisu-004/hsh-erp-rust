//! `POST /api/v2/com/delivery/note/scan` —— 扫码入单（**唯一**入单入口）
//!
//! ## 请求体（客户端**不传批次 version**）
//! ```rust
//! pub struct ScanEntryRequest {
//!     pub serial_no: String,          // 扫码串：定位 L1 → find-or-create 草稿
//!     pub note_version: Option<i32>,  // draft 非 null 时必填（送货单 OCC）
//!     pub entries: Vec<ScanEntry>,
//! }
//! pub struct ScanEntry {
//!     pub node_kind: String,   // "ASSEMBLY" | "PART"
//!     pub node_id: i64,        // JSON string
//!     pub sets: Option<i32>,      // node_kind=ASSEMBLY 时必填（套数）
//!     pub quantity: Option<i32>,  // node_kind=PART 时必填（件数）
//! }
//! ```
//!
//! 批次 version **不收**：分配在服务端事务内完成，读到的就是最新 —— 让客户端回传
//! 一个可能已过期的版本只会制造假的 OCC 冲突。
//!
//! ## 处理流程（Step 编号即代码分段编号）
//! ```text
//! 1. 角色守卫 Manager / Clerk / Inspector
//! 2. 打开 tx
//! 3. scan_find_or_create_draft(l1_id) —— 判定键单键 (customer_id, DRAFT)
//! 4. 命中既有单：obj.version != note_version ⇒ 40901 VERSION_CONFLICT
//!    （23505 撞唯一索引时已在 find-or-create 内部重查一次，兜并发扫码）
//! 5. 解析 entries → 得到 targets（零件 + 装配件的全部子件；装配件按
//!    `sets × (part.quantity / assembly.quantity)` 展开，且 `sets <= entry_max_sets`）
//! 6. 逐零件分类：可入单（`READY_TO_SHIP` 且未占用）/ 已占用（21406，请求级拒绝）/
//!    状态未过检（只按 part 收集明细，不拒绝 —— 见「状态闸门的作用域」）
//! 7. 每个 target 跑一次 DP（子件之间不耦合）+ 拆批 + 挂单（同事务）：任一 part
//!    凑不出 ⇒ 21405，message 附该 part 状态未过检的批次明细；此时尚未发生任何
//!    写操作 ⇒ 整请求零写入
//! 8. note.version++ → commit → WS 广播 DELIVERY_NOTE_SCAN_ADD
//! 9. 返回 DeliveryNoteDetailOut（含拆批后的完整行项，前端可就地替换草稿卡）
//! ```
//!
//! ## 校验闸门
//! | 检查 | 作用域 | 错误码 |
//! |---|---|---|
//! | 批次 status ≠ `READY_TO_SHIP`（含 `INSPECTION`） | **本次分配实际需要的量**（见下节） | 21405 |
//! | 批次已挂在别的 `DRAFT`/`SUBMITTED`/`PICKED_UP`/`ARCHIVED` 单上 | 请求级 | 21406 |
//! | 零件的 L1 客户 ≠ 单据 L1 客户 | 请求级 | 21407 |
//! | DP 不可行（凑不出 / 差额 > 0 且无可拆批次） | 请求级 | 21405 |
//! | `sets` > `entry_max_sets` | 请求级 | 21405 |
//!
//! ### 状态闸门的作用域：判「本次要的量」，不是「该零件的全部活跃批次」
//!
//! 2026-10-09 改。同一个零件同时有 `READY_TO_SHIP` 批次与非 `READY_TO_SHIP`
//! 批次时，只要**可入单量**够，本次入单正常成功 —— 非 READY 批次只是不参与分配，
//! 不是「顺带拒绝」的理由。原先把它做成请求级闸门，会让「旁边趴着 8 件
//! `IN_PROCESS`、8 件 `READY_TO_SHIP` 明明能入单」这种场景整单 21405。
//!
//! ⇒ `not_ready` 从请求级闸门降级为**分配失败时的诊断明细**：判定依据只有一条
//! —— DP 能否凑出该 part 本次的 target；失败时才把这些明细附进 21405 的 message。
//!
//! ### ⚠️ `INSPECTION` 必须显式分支，绝不走兜底沉默
//! 入单口径从 `{INSPECTION, READY_TO_SHIP}` 收窄为 `{READY_TO_SHIP}` 后，
//! `INSPECTION` 批次会掉进分类循环的兜底臂 —— 那段注释自称「剩下的合法状态只有
//! `READY_TO_SHIP`（已收进 attachable）」，即它声称自己不可达。若真让 `INSPECTION`
//! 掉进去：`all_attachable_empty = true` ⇒ 返回 `AlreadyPresent` ⇒ 前端弹「已在
//! XX 上」，**但它根本没被挂上去**。这是静默说谎。
//!
//! ⇒ 本文件在分类循环里给 `INSPECTION` 显式分支，收集到**按 part 分组**的
//! `not_ready` 明细；当该 part 的 DP 分配失败时，这些明细连同 part_id /
//! serial_no / batch_no / status 一起进 21405 的 message。

use std::collections::HashMap;

use super::batch_allocation::allocate;
use super::scan_tree::l1_of;
use super::shippable_sets::{SetsBatchRow, SetsChild, shippable_sets};
use crate::auth::rbac::{CurrentUser, Role};
use crate::infra::clock::now_naive;
use crate::modules::assembly::model::TAssembly;
use crate::modules::assembly::repo::AssemblyRepo;
use crate::modules::com::delivery_note::repo::DeliveryNoteRepoTrait;
use crate::modules::com::delivery_note::repo::scan_tree::DeliveryScanRepo;
use crate::modules::com::delivery_note::vo::DeliveryNoteDetailOut;
use crate::modules::part::model::TPart;
use crate::modules::part::repo::PartRepo;
use crate::modules::prod::batch::repo::PartBatchRepo;
use crate::shared::batch::TPartBatch;
use crate::shared::error::{AppError, code};

use super::DeliveryNoteService;
use super::inner::get_with_parts;

/// `entries[].node_kind` 的取值白名单。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NodeKind {
    Assembly,
    Part,
}

impl NodeKind {
    fn parse(raw: &str) -> Result<Self, AppError> {
        match raw {
            "ASSEMBLY" => Ok(Self::Assembly),
            "PART" => Ok(Self::Part),
            other => Err(AppError::validation(format!(
                "node_kind 必须是 ASSEMBLY 或 PART，收到 {other:?}"
            ))),
        }
    }
}

/// 入单唯一允许的批次状态。
const STATUS_READY_TO_SHIP: &str = "READY_TO_SHIP";

/// 一批「已被某张送货单占着」的批次明细（报 21406 时附在 message 里）。
///
/// `reason` 区分两种占用：
/// - `"活跃单"`：`DRAFT` / `SUBMITTED` —— 货还在单上，换单要先把货撤下来；
/// - `"已领取/已归档单"`：`PICKED_UP` / `ARCHIVED` —— 货已经随该单送出，
///   **不可再次入单**（若放行会把已送出的货再挂一张新单，账实不符）。
struct OccupiedDetail {
    part_id: i64,
    batch_no: i32,
    on_note_id: i64,
    reason: &'static str,
}

/// 一批「状态不是 READY_TO_SHIP」的批次明细（报 21405 时附在 message 里）。
///
/// 2026-10-09 起按 `part_id` 分组收集（`HashMap<i64, Vec<NotReadyDetail>>`）：它不再
/// 是请求级闸门，只在**该 part 的 DP 分配失败**时用来解释「为什么凑不出」—— 分组
/// 让「某个 part 够、另一个 part 不够」能各说各的。
struct NotReadyDetail {
    part_id: i64,
    serial_no: String,
    batch_no: i32,
    status: String,
}

/// 一个 part 的 DP 分配失败（`allocate` 的错误原样留存，供汇总成一条 21405）。
struct AllocFailure {
    part_id: i64,
    /// 该 part 本次要的件数。
    target: i32,
    error: AppError,
}

/// 取 `AppError` 的 message 原文（拼汇总文案时用；非业务错误退回 `Display`）。
fn error_message(e: &AppError) -> String {
    match e {
        AppError::Biz { message, .. } | AppError::BizWithFailures { message, .. } => {
            message.clone()
        }
        other => other.to_string(),
    }
}

/// 把「本次分配的失败 part」汇总成一条 21405。
///
/// 2026-10-09 定。三条规则：
/// - **基底是 DP 自己的 message**（如「可入单件数不足：需要 8 件，候选批次合计 4 件」）
///   —— 根因是「货不够」时不要硬塞「READY_TO_SHIP」字样，那是在说错话；
/// - 该失败 part 在 `not_ready` 里有明细时，在基底后追加
///   `；入单只允许 READY_TO_SHIP，以下批次不可用：…` + 明细；
/// - 多个 part 同时失败时逐个列出（`part_id` + 需要件数 + DP 文案），按 `part_id`
///   升序输出（`targets` 迭代序不定，不排序会让同一请求的 message 抖动）。
fn alloc_failure_error(
    failures: &[AllocFailure],
    not_ready_by_part: &HashMap<i64, Vec<NotReadyDetail>>,
) -> AppError {
    let mut sorted: Vec<&AllocFailure> = failures.iter().collect();
    sorted.sort_by_key(|f| f.part_id);
    let multiple = sorted.len() > 1;
    let segments: Vec<String> = sorted
        .iter()
        .map(|f| {
            let dp_msg = error_message(&f.error);
            let head = if multiple {
                format!("part {}（需 {} 件）：{dp_msg}", f.part_id, f.target)
            } else {
                dp_msg
            };
            match not_ready_by_part.get(&f.part_id) {
                Some(details) if !details.is_empty() => {
                    let detail = details
                        .iter()
                        .map(|d| {
                            format!(
                                "part {}（{}）批次 {} status={}",
                                d.part_id, d.serial_no, d.batch_no, d.status
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("；");
                    format!("{head}；入单只允许 READY_TO_SHIP，以下批次不可用：{detail}")
                }
                _ => head,
            }
        })
        .collect();
    AppError::biz(code::BIZ_DELIVERY_NOTE_PART_NOT_READY, segments.join("；"))
}

impl DeliveryNoteService {
    /// `POST /api/v2/com/delivery/note/scan` —— 扫码入单。
    ///
    /// 见模块 doc 的完整流程与闸门表。事务边界：handler `pool.begin()` → 这里 →
    /// handler `commit()`；本方法不 commit、不发广播。
    pub async fn scan_entry<R: DeliveryNoteRepoTrait>(
        &self,
        mut repo: R,
        req: crate::modules::com::delivery_note::dto::ScanEntryRequest,
        current: &CurrentUser,
    ) -> Result<DeliveryNoteDetailOut, AppError> {
        // ===== Step 1: 角色守卫 =====
        current.require_any_role(&[Role::Manager, Role::Clerk, Role::Inspector])?;

        // ===== Step 2: 解析入参 =====
        let serial_no = req.serial_no.trim();
        if serial_no.is_empty() {
            return Err(AppError::validation("serial_no must not be empty"));
        }
        if req.entries.is_empty() {
            return Err(AppError::validation("entries must not be empty"));
        }

        // ===== Step 3: find-or-create 草稿（判定键单键） =====
        //
        // 先解析 serial_no 拿到锚点客户（L1 由它上推）—— 建单必须发生在拿到 L1 之后，
        // 否则「扫了个不存在的码」也会凭空建出一张草稿。
        let anchor_customer_id =
            match DeliveryScanRepo::find_part_by_serial(&mut *repo.conn_mut(), serial_no).await? {
                Some(p) => p.customer_id,
                None => {
                    let asm =
                        DeliveryScanRepo::find_assembly_by_serial(&mut *repo.conn_mut(), serial_no)
                            .await?
                            .ok_or_else(|| {
                                AppError::biz(
                                    code::BIZ_DELIVERY_SCAN_UNKNOWN_CODE,
                                    format!("序列号 {serial_no} 未找到对应零件或装配件"),
                                )
                            })?;
                    asm.customer_id
                }
            };
        // 锚点客户 = 扫码命中的零件 / 装配件所属客户；L1 由它上推一级。
        let l1_id = l1_of(anchor_customer_id, &mut *repo.conn_mut()).await?;

        let mut note = self
            .scan_find_or_create_draft(&mut *repo.conn_mut(), l1_id, current)
            .await?;

        // ===== Step 4: OCC =====
        // `note_version` 在「命中既有单」时必填；草稿刚建出来时前端拿不到 version，
        // 传 None 也接受（刚建的单 version 必为 0，无并发可冲突）。
        if let Some(v) = req.note_version
            && v != note.version
        {
            return Err(super::inner::note_version_conflict(
                note.id,
                note.version,
                v,
            ));
        }

        // ===== Step 5: 解析 entries → 每个零件的 target 件数 =====
        let targets = self
            .resolve_entry_targets(&mut *repo.conn_mut(), &req.entries, note.customer_id)
            .await?;

        // ===== Step 6: 逐零件分类（可入单 / 已占用 / 未过检） =====
        let part_ids: Vec<i64> = targets.keys().copied().collect();
        // 2026-10-08 review 第 1 轮 m9：删掉 `filter(part_ids.contains(..))` —— repo 的
        // SQL 已写死 `WHERE part_id = ANY($1) AND deleted_at IS NULL`，返回集恒是入参
        // 的子集，内存再过一遍既是 O(n·m) 的无用功，也让人误以为 repo 没过滤。
        let all_batches =
            PartBatchRepo::list_active_by_part_ids(&mut *repo.conn_mut(), &part_ids).await?;
        let eligible =
            DeliveryScanRepo::list_entryable_batches_by_part_ids(&mut *repo.conn_mut(), &part_ids)
                .await?;

        // 批次自身信息（serial_no / part 名）用于错误明细，懒加载。
        let part_rows = PartRepo::list_by_ids(&mut *repo.conn_mut(), &part_ids, false).await?;
        let part_map: HashMap<i64, TPart> = part_rows.into_iter().map(|p| (p.id, p)).collect();

        let mut eligible_by_part: HashMap<i64, Vec<TPartBatch>> = HashMap::new();
        for b in eligible {
            eligible_by_part.entry(b.part_id).or_default().push(b);
        }

        // 21406 收集：批次挂在别的 `DRAFT` / `SUBMITTED` 单上。
        let mut occupied: Vec<OccupiedDetail> = Vec::new();
        let mut note_ids_involved: Vec<i64> = Vec::new();
        // 21405 诊断明细收集：`INSPECTION` 等非 READY_TO_SHIP 批次（**显式分支，
        // 绝不走兜底沉默**），**按 part 分组**。
        //
        // 2026-10-09 改：原先它是请求级闸门（分类循环结束即整单 21405），作用域被
        // 放大成「该零件的全部活跃批次」—— 零件只要沾一个非 READY 批次，即便有足量
        // READY_TO_SHIP 批次可用也整单失败。现降级为「分配失败时的诊断明细」：判定
        // 权交给 Step 7 的 DP（能不能凑出本次 target），失败时才把这些明细附进
        // 21405 的 message。分类的三条分支判定本身（占用方 != 本单 / 占用方已软删
        // 视为未占用 / 未占用）一字未动。
        let mut not_ready_by_part: HashMap<i64, Vec<NotReadyDetail>> = HashMap::new();
        for b in &all_batches {
            if let Some(other_id) = b.delivery_note_id
                && other_id != note.id
            {
                note_ids_involved.push(other_id);
            }
        }
        // 批量取占用方单据（去重）。三态：
        //   * `DRAFT` / `SUBMITTED` ⇒ **活跃占用**（货还在单上，换单要先撤货）；
        //   * `PICKED_UP` / `ARCHIVED` ⇒ **已送出占用**（同样 21406，但 message 要说清
        //     与「货还在单上」不同 —— 换单救不回来）；
        //   * 不在结果里 ⇒ 占用方已软删 / 查不到（`note_list_by_ids` 带
        //     `include_deleted=false`）⇒ 视为未占用，与扫码树 JOIN 的
        //     `dn.deleted_at IS NULL` 同口径。
        note_ids_involved.sort_unstable();
        note_ids_involved.dedup();
        let mut occupied_note_ids: HashMap<i64, &'static str> = HashMap::new();
        if !note_ids_involved.is_empty() {
            for n in repo.note_list_by_ids(&note_ids_involved, false).await? {
                let reason = match n.status.as_str() {
                    "DRAFT" | "SUBMITTED" => "活跃单，货还在这张单上",
                    _ => "已领取/已归档单，货已随该单送出，不可再次入单",
                };
                occupied_note_ids.insert(n.id, reason);
            }
        }
        for b in &all_batches {
            match b.delivery_note_id {
                Some(other_id) if other_id != note.id => {
                    if let Some(reason) = occupied_note_ids.get(&other_id) {
                        occupied.push(OccupiedDetail {
                            part_id: b.part_id,
                            batch_no: b.batch_no,
                            on_note_id: other_id,
                            reason,
                        });
                    } else if b.status != STATUS_READY_TO_SHIP {
                        // 占用方已软删 ⇒ 视为未占用；此时按状态闸门判。
                        not_ready_by_part
                            .entry(b.part_id)
                            .or_default()
                            .push(NotReadyDetail {
                                part_id: b.part_id,
                                serial_no: part_map
                                    .get(&b.part_id)
                                    .and_then(|p| p.serial_no.clone())
                                    .unwrap_or_default(),
                                batch_no: b.batch_no,
                                status: b.status.clone(),
                            });
                    }
                }
                _ => {
                    // 未被占用 ⇒ 判状态。READY_TO_SHIP 由 repo 的
                    // `list_entryable_batches_by_part_ids` 保证进了 `eligible`，
                    // 其余一律收集到 not_ready（Step 7 分配失败时才用它们报错）。
                    if b.status != STATUS_READY_TO_SHIP {
                        not_ready_by_part
                            .entry(b.part_id)
                            .or_default()
                            .push(NotReadyDetail {
                                part_id: b.part_id,
                                serial_no: part_map
                                    .get(&b.part_id)
                                    .and_then(|p| p.serial_no.clone())
                                    .unwrap_or_default(),
                                batch_no: b.batch_no,
                                status: b.status.clone(),
                            });
                    }
                }
            }
        }

        if !occupied.is_empty() {
            let detail = occupied
                .iter()
                .map(|d| {
                    format!(
                        "part {} 批次 {} 已在送货单 {}（{}）",
                        d.part_id, d.batch_no, d.on_note_id, d.reason
                    )
                })
                .collect::<Vec<_>>()
                .join("；");
            return Err(AppError::biz(
                code::BIZ_DELIVERY_NOTE_PART_ALREADY_ASSIGNED,
                format!("以下批次已被其它送货单占用：{detail}"),
            ));
        }
        // ===== Step 7: DP 分配 + 拆批 + 挂单（同一事务） =====
        let now = now_naive();
        let mut attached: Vec<(i64, i32)> = Vec::new();
        let mut split_calls: Vec<SplitCall> = Vec::new();
        // 2026-10-09 改：DP 失败**收集**而不是 `?` 直接冒泡 —— 一次把全部 part 都
        // 试完，失败明细（含 Step 6 收的状态明细）汇总成一条 21405 报出去，别的 part
        // 缺货时用户能一次看全，不必逐个试。
        let mut alloc_failures: Vec<AllocFailure> = Vec::new();
        for (part_id, target) in &targets {
            let cands = eligible_by_part.get(part_id).cloned().unwrap_or_default();
            // DP 要求候选按 (quantity ASC, batch_no ASC) 排序；repo 已保证，
            // 这里再排一次是为了不把「排序契约」只写在 repo 的注释里。
            let mut cands = cands;
            cands.sort_by(|a, b| {
                a.quantity
                    .cmp(&b.quantity)
                    .then(a.batch_no.cmp(&b.batch_no))
            });
            match allocate(*target, &cands) {
                Ok(plan) => {
                    attached.extend(plan.iter().copied());
                    for (batch_id, qty) in plan {
                        let src = cands
                            .iter()
                            .find(|b| b.id == batch_id)
                            .expect("DP 只可能返回候选内的 batch_id");
                        if qty < src.quantity {
                            split_calls.push(SplitCall {
                                source_id: src.id,
                                qty,
                            });
                        }
                    }
                }
                Err(e) => alloc_failures.push(AllocFailure {
                    part_id: *part_id,
                    target: *target,
                    error: e,
                }),
            }
        }
        // 状态闸门在此生效：只有 DP 凑不出本次要的量才拒绝，且拒绝发生在任何
        // `split_batch` / 挂单之前 ⇒ 整请求原子失败（不部分挂单）。
        if !alloc_failures.is_empty() {
            return Err(alloc_failure_error(&alloc_failures, &not_ready_by_part));
        }
        // 兜底：`targets` 非空、每个 target > 0（入参闸门已保证）⇒ DP 全成功必有
        // 分配，走不到这；留着防将来改动把某个分支挪到 DP 之后。
        if attached.is_empty() {
            return Err(AppError::biz(
                code::BIZ_DELIVERY_NOTE_PART_NOT_READY,
                "没有可入单的批次（请先完成品检让批次到 READY_TO_SHIP）",
            ));
        }

        // 拆批：差额新建独立批次行并挂单；原批次保持不动（不挂单、不改 status）。
        let mut final_batches: Vec<(i64, i32)> = Vec::new();
        for c in &split_calls {
            let src =
                crate::shared::batch::get_batch_by_id(&mut *repo.conn_mut(), c.source_id, false)
                    .await?
                    .ok_or_else(|| {
                        AppError::biz(
                            code::BIZ_PART_BATCH_NOT_FOUND,
                            format!("batch {} 不存在或已删除", c.source_id),
                        )
                    })?;
            let new_id = self.snowflake.next_id();
            PartBatchRepo::split_batch(
                &mut *repo.conn_mut(),
                new_id,
                src.id,
                src.version,
                src.part_id,
                c.qty,
                &src.status,
                src.location.as_deref(),
                src.current_holder_id,
                src.current_process_step_id,
                now,
                Some(current.id),
                Some(current.id),
            )
            .await?;
            final_batches.push((new_id, c.qty));
        }
        // 整批入单的批次直接挂。
        for (batch_id, qty) in &attached {
            let already_split = split_calls.iter().any(|c| c.source_id == *batch_id);
            if !already_split {
                final_batches.push((*batch_id, *qty));
            }
        }

        for (batch_id, qty) in &final_batches {
            let b = crate::shared::batch::get_batch_by_id(&mut *repo.conn_mut(), *batch_id, false)
                .await?
                .ok_or_else(|| {
                    AppError::biz(
                        code::BIZ_PART_BATCH_NOT_FOUND,
                        format!("batch {batch_id} 不存在或已删除"),
                    )
                })?;
            let affected = PartBatchRepo::attach_to_note(
                &mut *repo.conn_mut(),
                b.id,
                b.version,
                note.id,
                now,
                Some(current.id),
            )
            .await?;
            if affected == 0 {
                return Err(AppError::biz(
                    code::VERSION_CONFLICT,
                    format!("batch {batch_id} version conflict during scan attach（数量 {qty}）"),
                ));
            }
        }

        // ===== Step 8: 送货单 version++ =====
        note.version += 1;
        note.updated_at = now;
        note.updated_by = Some(current.id);
        let affected = repo.note_update(&note).await?;
        if affected == 0 {
            return Err(AppError::biz(
                code::VERSION_CONFLICT,
                "concurrent modification detected",
            ));
        }

        // ===== Step 9: 返回完整详情（含拆批后的行项，前端可替换草稿卡） =====
        get_with_parts(&mut *repo.conn_mut(), note.id).await
    }

    /// 把 `entries[]` 解析成「part_id → 本次该零件要入单的总件数」。
    ///
    /// 装配件条目会展开成「每个子件 × sets 套的用量」；同一 part 被多个条目命中时
    /// **件数相加**（例如前端同时送「装配件 A 的 2 套」与「A 的某个子件 3 件」，
    /// 该子件本次共入 2×per_set + 3 件）。
    async fn resolve_entry_targets(
        &self,
        conn: &mut sqlx::PgConnection,
        entries: &[crate::modules::com::delivery_note::dto::ScanEntry],
        note_customer_id: i64,
    ) -> Result<HashMap<i64, i32>, AppError> {
        let mut out: HashMap<i64, i32> = HashMap::new();
        for e in entries {
            match NodeKind::parse(&e.node_kind)? {
                NodeKind::Part => {
                    let quantity = e.quantity.ok_or_else(|| {
                        AppError::validation(format!(
                            "node_kind=PART 的条目（node_id={}）必须带 quantity",
                            e.node_id
                        ))
                    })?;
                    if quantity <= 0 {
                        return Err(AppError::validation(format!(
                            "quantity must be positive, got {quantity}"
                        )));
                    }
                    // 散件树里扫子件条码也归到这里：扫到的是子件就按子件算件数。
                    let part = PartRepo::get_by_id(&mut *conn, e.node_id, false)
                        .await?
                        .ok_or_else(|| {
                            AppError::biz(
                                code::BIZ_PART_NOT_FOUND,
                                format!("part {} not found", e.node_id),
                            )
                        })?;
                    check_l1(&mut *conn, &part, note_customer_id).await?;
                    *out.entry(part.id).or_insert(0) += quantity;
                }
                NodeKind::Assembly => {
                    let sets = e.sets.ok_or_else(|| {
                        AppError::validation(format!(
                            "node_kind=ASSEMBLY 的条目（node_id={}）必须带 sets",
                            e.node_id
                        ))
                    })?;
                    if sets <= 0 {
                        return Err(AppError::validation(format!(
                            "sets must be positive, got {sets}"
                        )));
                    }
                    let asm = AssemblyRepo::get_by_id(&mut *conn, e.node_id, false)
                        .await?
                        .ok_or_else(|| {
                            AppError::biz(
                                code::BIZ_ASSEMBLY_NOT_FOUND,
                                format!("assembly {} not found", e.node_id),
                            )
                        })?;
                    let children = PartRepo::list_children(&mut *conn, e.node_id, false).await?;
                    if children.is_empty() {
                        return Err(AppError::biz(
                            code::BIZ_DELIVERY_NOTE_PART_NOT_READY,
                            format!("装配件 {} 没有活跃子件，无法按套入单", asm.id),
                        ));
                    }
                    // 可组套数：把该装配件全部子件的「可入单」批次喂进与扫码树
                    // `entry_max_sets` 同一个公式 ⇒ 前端看到的上限与这里的上限同源。
                    let cap = entry_max_sets(&mut *conn, &asm, &children).await?;
                    if sets > cap {
                        return Err(AppError::biz(
                            code::BIZ_DELIVERY_NOTE_PART_NOT_READY,
                            format!(
                                "装配件 {} 要送 {sets} 套，但当前最多只能组 {cap} 套",
                                asm.id
                            ),
                        ));
                    }
                    // 每个子件：target = sets × (part.quantity / assembly.quantity)
                    //（整数除法向零截断，与扫码树 `per_set_parts` 同口径）。
                    for c in &children {
                        check_l1(&mut *conn, c, note_customer_id).await?;
                        if asm.quantity == 0 {
                            return Err(AppError::biz(
                                code::BIZ_DELIVERY_NOTE_PART_NOT_READY,
                                format!("装配件 {} 的总套数为 0，无法按套入单", asm.id),
                            ));
                        }
                        let per_set = c.quantity / asm.quantity;
                        if per_set == 0 {
                            return Err(AppError::biz(
                                code::BIZ_DELIVERY_NOTE_PART_NOT_READY,
                                format!(
                                    "装配件 {} 的子件 {} 每套用量为 0（整单 {} 件 / 总 {} 套）",
                                    asm.id, c.id, c.quantity, asm.quantity
                                ),
                            ));
                        }
                        *out.entry(c.id).or_insert(0) += sets * per_set;
                    }
                }
            }
        }
        Ok(out)
    }
}

/// 一次拆批调用的参数（收集后统一执行，让「先算完全部分配、再写库」的边界可见）。
struct SplitCall {
    source_id: i64,
    /// 从源批次拆出的件数（小于源批次数量 ⇒ 需拆批）。
    qty: i32,
}

/// L1 一致性闸门：零件所属客户的 L1 必须等于单据 L1（21407）。
///
/// 「零件的 L1」与「单据的 L1」是两个独立查询的结果（零件表存的是 L2 客户），所以
/// 这里现查一次 `t_customer` 而不是信任入参。
async fn check_l1(
    conn: &mut sqlx::PgConnection,
    part: &TPart,
    note_customer_id: i64,
) -> Result<(), AppError> {
    let l1 = l1_of(part.customer_id, &mut *conn).await?;
    if l1 != note_customer_id {
        return Err(AppError::biz(
            code::BIZ_DELIVERY_NOTE_PARTS_MULTIPLE_CUSTOMERS,
            format!(
                "part {} 所属 L1 客户 {} != 送货单 L1 客户 {}",
                part.id, l1, note_customer_id
            ),
        ));
    }
    Ok(())
}

/// 装配件的「可组套数」上限：把全部子件的**可入单**批次喂进与扫码树
/// `entry_max_sets` 同一个 `shippable_sets` 公式。
async fn entry_max_sets(
    conn: &mut sqlx::PgConnection,
    asm: &TAssembly,
    children: &[TPart],
) -> Result<i32, AppError> {
    let part_ids: Vec<i64> = children.iter().map(|c| c.id).collect();
    let entryable =
        DeliveryScanRepo::list_entryable_batches_by_part_ids(&mut *conn, &part_ids).await?;
    let rows: Vec<SetsBatchRow> = entryable
        .iter()
        .map(|b| SetsBatchRow {
            part_id: b.part_id,
            batch_quantity: b.quantity,
            batch_status: b.status.clone(),
        })
        .collect();
    let asm_quantity = HashMap::from([(asm.id, asm.quantity)]);
    let children_by_asm = HashMap::from([(
        asm.id,
        children
            .iter()
            .map(|c| SetsChild {
                part_id: c.id,
                part_quantity: c.quantity,
            })
            .collect::<Vec<_>>(),
    )]);
    Ok(shippable_sets(&rows, &asm_quantity, &children_by_asm)
        .get(&asm.id)
        .copied()
        .unwrap_or(0))
}

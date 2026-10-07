//! 送货单扫码三层树 service（`GET /api/v2/com/delivery/note/scan/{serial_no}`）
//!
//! ## 角色守卫
//! Manager / Clerk / Inspector（与送货单其余端点同一组）。`ShelfAccount` 不放行 ——
//! 它只该扫码核销批次，不该开送货单。
//!
//! ## 事务边界
//! 纯读端点不开事务（handler `pool.acquire()` 借 `&mut PgConnection`）。**不发** WS
//! 广播，也**绝不建单** —— 建单发生在 `POST /scan`。
//!
//! ## 命中顺序：先 `t_part` 再 `t_assembly`
//! 与 `prod::inspection` 逐字一致（子件码与父件码不同形，两表各自对活跃行有唯一
//! 索引），固定顺序少一个需要解释的特例。两表皆未命中 ⇒ `20101 BIZ_PART_NOT_FOUND`
//! （HTTP 404），与 part 域扫码共用同一码。

use std::collections::HashMap;

use sqlx::PgConnection;

use crate::auth::rbac::{CurrentUser, Role};
use crate::modules::com::customer::model::TCustomer;
use crate::modules::com::customer::repo::CustomerRepo;
use crate::modules::com::delivery_note::repo::DeliveryNoteRepoTrait;
use crate::modules::com::delivery_note::repo::scan_tree::{
    DeliveryScanRepo, ScanAssemblyRow, ScanBatchRow, ScanPartRow,
};
use crate::modules::com::delivery_note::service::shippable_sets::{
    SetsBatchRow, SetsChild, shippable_sets,
};
use crate::modules::com::delivery_note::vo::{
    DeliveryScanAssemblyOut, DeliveryScanBatchOut, DeliveryScanDraftOut, DeliveryScanPartOut,
    DeliveryScanPerSetPartOut, DeliveryScanTreeOut,
};
use crate::shared::error::{AppError, code};

/// 本端点允许的角色：Manager / Clerk / Inspector。
const READ_ROLES: &[Role] = &[Role::Manager, Role::Clerk, Role::Inspector];

/// `hit_kind` 出参的取值白名单（只在 service 内构造，出参仍是 `String`）。
#[derive(Debug, Clone, Copy)]
enum HitKind {
    /// 扫到的是装配件条码（`t_assembly` 命中）。
    Assembly,
    /// 扫到的是独立件或装配件子件的条码（`t_part` 命中）。
    Part,
}

impl HitKind {
    fn as_str(&self) -> &'static str {
        match self {
            HitKind::Assembly => "ASSEMBLY",
            HitKind::Part => "PART",
        }
    }
}

/// 命中结果（SQL 阶段的产物，尚未挂批次 / 未算闸门）。
struct HitTree {
    kind: HitKind,
    /// 被扫中的那个零件 id。扫装配件条码时为 `None`（装配件没有零件身份）。
    scanned_part_id: Option<i64>,
    assembly: Option<ScanAssemblyRow>,
    /// 顶层零件节点（装配件树 = 全部子件；独立件树 = `[被扫中的那个]`）。
    parts: Vec<ScanPartRow>,
}

/// 送货单扫码树 service 方法组（挂在 `DeliveryNoteService` 上，故本文件只放
/// `impl` 块与私有 helper）。
impl super::DeliveryNoteService {
    /// `GET /api/v2/com/delivery/note/scan/{serial_no}` 业务逻辑。
    ///
    /// 流程：角色守卫 → 序列号 trim + 空值兜底 → 命中（part → assembly 回退）→
    /// 定位装配件节点与零件层 → **一条** SQL 取全部批次（不过滤 status）→ **一条**
    /// SQL 取全部「可入单」批次（算两个 `entry_max_*`）→ 内存分组挂树 → 取该 L1 的
    /// 既有 DRAFT（不建单）。
    pub async fn scan_tree<R: DeliveryNoteRepoTrait>(
        &self,
        mut repo: R,
        current: &CurrentUser,
        serial_no: &str,
    ) -> Result<DeliveryScanTreeOut, AppError> {
        current.require_any_role(READ_ROLES)?;

        // 扫码枪偶发尾随空白 / 空格；空串等价于「没扫到东西」，按未命中收口。
        let serial_no = serial_no.trim();
        if serial_no.is_empty() {
            // 空串直接插值进 message 会渲染成「序列号  未找到…」（双空格），用
            // 占位符让日志与前端 toast 可读。
            return Err(not_found("(空)"));
        }

        let hit = match DeliveryScanRepo::find_part_by_serial(&mut *repo.conn_mut(), serial_no)
            .await?
        {
            Some(p) => {
                // 扫到零件：父装配件活跃 → 整棵装配件树；父装配件已软删（取不到）
                // → 退化成独立件树（assembly = null），不返回孤儿树。
                match p.assembly_id {
                    Some(asm_id) => {
                        match DeliveryScanRepo::find_assembly_by_id(&mut *repo.conn_mut(), asm_id)
                            .await?
                        {
                            Some(asm) => HitTree {
                                kind: HitKind::Part,
                                scanned_part_id: Some(p.id),
                                parts: DeliveryScanRepo::list_parts_by_assembly(
                                    &mut *repo.conn_mut(),
                                    asm_id,
                                )
                                .await?,
                                assembly: Some(asm),
                            },
                            None => HitTree {
                                kind: HitKind::Part,
                                scanned_part_id: Some(p.id),
                                parts: vec![p],
                                assembly: None,
                            },
                        }
                    }
                    None => HitTree {
                        kind: HitKind::Part,
                        scanned_part_id: Some(p.id),
                        parts: vec![p],
                        assembly: None,
                    },
                }
            }
            // part 未命中 → 回退查装配件条码。
            None => {
                let asm =
                    DeliveryScanRepo::find_assembly_by_serial(&mut *repo.conn_mut(), serial_no)
                        .await?
                        .ok_or_else(|| not_found(serial_no))?;
                HitTree {
                    kind: HitKind::Assembly,
                    // 装配件码没有零件身份 → 全部批次 is_scanned = false
                    scanned_part_id: None,
                    parts: DeliveryScanRepo::list_parts_by_assembly(&mut *repo.conn_mut(), asm.id)
                        .await?,
                    assembly: Some(asm),
                }
            }
        };

        let part_ids: Vec<i64> = hit.parts.iter().map(|p| p.id).collect();

        // 批次层：一条 SQL 覆盖整棵树（无 N+1），空零件列表时 repo 直接返空。
        let batch_rows =
            DeliveryScanRepo::list_batches_by_part_ids(&mut *repo.conn_mut(), &part_ids).await?;

        // 「可入单」批次层：另一条 SQL，供 entry_max_quantity / entry_max_sets 取数。
        let entryable =
            DeliveryScanRepo::list_entryable_batches_by_part_ids(&mut *repo.conn_mut(), &part_ids)
                .await?;

        // 装配件的 entry_max_sets：把该装配件全部子件的**可入单**批次行喂进与详情
        // VO 共用的 `shippable_sets` 公式（同一份实现 ⇒ 同一数字）。
        //
        // `min` 的定义域是「该装配件的**全部**子件」（缺子件 ⇒ 0 套，凑不齐整套不能
        // 发），本树已把全部子件查回来，直接复用，不发第二条 SQL。与详情 VO 的
        // `PartRepo::list_children(..., include_deleted=false)` 同口径。
        let asm_sets: Option<i32> = hit.assembly.as_ref().map(|asm| {
            let asm_quantity = HashMap::from([(asm.id, asm.quantity)]);
            let children_by_asm = HashMap::from([(
                asm.id,
                hit.parts
                    .iter()
                    .map(|p| SetsChild {
                        part_id: p.id,
                        part_quantity: p.quantity,
                    })
                    .collect::<Vec<_>>(),
            )]);
            // 「可入单」的批次行（不是「单上批次」）：entryable 已由 repo 按
            // `READY_TO_SHIP` + 未占用 + 未软删过滤。
            let rows: Vec<SetsBatchRow> = entryable
                .iter()
                .map(|b| SetsBatchRow {
                    part_id: b.part_id,
                    batch_quantity: b.quantity,
                    batch_status: b.status.clone(),
                })
                .collect();
            shippable_sets(&rows, &asm_quantity, &children_by_asm)
                .get(&asm.id)
                .copied()
                .unwrap_or(0)
        });

        // 分组：批次按 part_id；可入单量按 part_id 求和。
        let mut batches_by_part: HashMap<i64, Vec<DeliveryScanBatchOut>> = HashMap::new();
        for row in batch_rows {
            // is_scanned 的唯一来源：t_part_batch 无序列号列，只能内存比对
            // 「批次所属零件 == 被扫中的那个零件」。
            let is_scanned = hit.scanned_part_id == Some(row.part_id);
            batches_by_part
                .entry(row.part_id)
                .or_default()
                .push(batch_to_out(row, is_scanned));
        }
        let mut entry_max_by_part: HashMap<i64, i32> = HashMap::new();
        for b in &entryable {
            *entry_max_by_part.entry(b.part_id).or_insert(0) += b.quantity;
        }

        let children = hit
            .parts
            .iter()
            .map(|p| {
                let batches = batches_by_part.remove(&p.id).unwrap_or_default();
                part_to_out(
                    p,
                    batches,
                    entry_max_by_part.get(&p.id).copied().unwrap_or(0),
                )
            })
            .collect();

        // draft：该 L1 名下现有的 DRAFT（建单判定键单键）。**本端点不建单**。
        let draft = resolve_draft(&mut repo, &hit).await?;

        Ok(DeliveryScanTreeOut {
            hit_kind: hit.kind.as_str().to_string(),
            scanned_serial_no: serial_no.to_string(),
            draft,
            assembly: hit
                .assembly
                .as_ref()
                .map(|a| assembly_to_out(a, asm_sets.unwrap_or(0), &hit.parts)),
            children,
        })
    }
}

/// 「序列号两表皆未命中」的唯一错误出口。
///
/// 复用 part 域的 `20101 BIZ_PART_NOT_FOUND`（HTTP 404）而不是另开错误码：语义完全
/// 相同（扫到的东西不存在），前端按同一个 code 弹「未找到」即可。
fn not_found(serial_no: &str) -> AppError {
    AppError::biz(
        code::BIZ_PART_NOT_FOUND,
        format!("序列号 {serial_no} 未找到对应零件或装配件"),
    )
}

/// 装配件行 → VO（挂上 `entry_max_sets` 与 `per_set_parts`）。
fn assembly_to_out(
    r: &ScanAssemblyRow,
    entry_max_sets: i32,
    parts: &[ScanPartRow],
) -> DeliveryScanAssemblyOut {
    // per_set_quantity = part.quantity / assembly.quantity（整数除法向零截断）。
    // `assembly.quantity == 0` 时不参与（避免除零；此时 entry_max_sets 恒 0，
    // 前端「送 0 套」是唯一可能的选择）。
    let per_set_parts = if r.quantity == 0 {
        Vec::new()
    } else {
        parts
            .iter()
            .map(|p| DeliveryScanPerSetPartOut {
                part_id: p.id,
                per_set_quantity: p.quantity / r.quantity,
            })
            .collect()
    };
    DeliveryScanAssemblyOut {
        id: r.id,
        serial_no: r.serial_no.clone(),
        name: r.name.clone(),
        drawing_no: r.drawing_no.clone(),
        status: r.status.clone(),
        quantity: r.quantity,
        is_urgent: r.is_urgent,
        system_delivery_date: r.system_delivery_date,
        customer_name: r.customer_name.clone(),
        customer_id: r.customer_id,
        entry_max_sets,
        per_set_parts,
    }
}

/// 零件行 → VO（挂上该零件的批次列表与可入单量）。
fn part_to_out(
    r: &ScanPartRow,
    children: Vec<DeliveryScanBatchOut>,
    entry_max_quantity: i32,
) -> DeliveryScanPartOut {
    DeliveryScanPartOut {
        id: r.id,
        serial_no: r.serial_no.clone(),
        name: r.name.clone(),
        drawing_no: r.drawing_no.clone(),
        status: r.status.clone(),
        quantity: r.quantity,
        is_urgent: r.is_urgent,
        system_delivery_date: r.system_delivery_date,
        customer_name: r.customer_name.clone(),
        // 零件级 version 仅展示：批次写操作的 OCC 锚是 DeliveryScanBatchOut::version
        version: r.version,
        customer_id: r.customer_id,
        entry_max_quantity,
        children,
    }
}

/// 批次行 → VO（`version` 取 `t_part_batch.version`，`is_scanned` 由调用方给出）。
fn batch_to_out(r: ScanBatchRow, is_scanned: bool) -> DeliveryScanBatchOut {
    DeliveryScanBatchOut {
        id: r.id,
        batch_no: r.batch_no,
        quantity: r.quantity,
        status: r.status,
        // ★ 批次版本（t_part_batch.version），不是零件版本
        version: r.version,
        is_repairing: r.is_repairing,
        location: r.location,
        current_holder_display: r.current_holder_display,
        // INSPECTION / DELIVERED 批次恒为 None：出池已清 current_process_id
        process_name: r.process_name,
        is_scanned,
        occupied_by_note_no: r.occupied_by_note_no,
    }
}

/// 取该 L1 名下现有的 DRAFT（无则 `null`；**绝不建单**）。
///
/// L1 的推导口径与 `POST /scan` 完全一致：装配件树用 `t_assembly.customer_id`
/// 上推一级，独立件树用 `t_part.customer_id` 上推一级（`parent_id.unwrap_or(id)`）。
async fn resolve_draft<R: DeliveryNoteRepoTrait>(
    repo: &mut R,
    hit: &HitTree,
) -> Result<Option<DeliveryScanDraftOut>, AppError> {
    let anchor = match hit.assembly.as_ref() {
        Some(a) => a.customer_id,
        None => match hit.parts.first() {
            Some(p) => p.customer_id,
            None => return Ok(None),
        },
    };
    let l1_id = l1_of(anchor, &mut *repo.conn_mut()).await?;
    let Some(note) = repo.note_find_open_draft_by_l1(l1_id).await? else {
        return Ok(None);
    };
    Ok(Some(DeliveryScanDraftOut {
        note_id: note.id,
        note_no: note.delivery_note_no,
        version: note.version,
        status: note.status,
    }))
}

/// 客户 id → 其 L1 id（自身即 L1 时返回自身）。客户不存在时报 20103。
pub(crate) async fn l1_of(customer_id: i64, conn: &mut PgConnection) -> Result<i64, AppError> {
    let c: TCustomer = CustomerRepo::get_by_id(conn, customer_id, false)
        .await?
        .ok_or_else(|| {
            AppError::biz(
                code::BIZ_CUSTOMER_NOT_FOUND,
                format!("customer {customer_id} not found"),
            )
        })?;
    Ok(c.parent_id.unwrap_or(c.id))
}

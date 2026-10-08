//! DeliveryNoteService 列表 / 详情 / 编辑 / 移除。
//!
//! 2026-10-08：`create_draft`（手动建单）与 `add_parts` 两个方法随入单入口收敛删除
//! —— `POST /` 与 `POST /{id}/add-parts` 下线后，草稿只可能由扫码入口创建
//! （`POST /scan` 内部的 `scan_find_or_create_draft`）。
//!
//! ## 2026-09-22 D-5 + review 第 1 轮修正（service by-value trait）
//! - 所有方法签名从 `pub async fn xxx(conn: &mut PgConnection, snowflake: &SnowflakeIdGenerator, ...)`
//!   改成 `pub async fn xxx<R: DeliveryNoteRepoTrait>(&self, mut repo: R, ...)`（iam 严格范本）。
//! - 跨域 ZST 静态调用走 `&mut *repo.conn_mut()`；私有 helper
//!   （`build_note_outs` / `get_with_parts`）收 `&mut PgConnection`，
//!   caller 喂 `&mut *repo.conn_mut()`。
//! - 原 `sqlx::query!(...)` 直调走 `&mut *repo.conn_mut()` 替换 `&mut *conn`。

use std::collections::HashMap;

use crate::auth::rbac::{CurrentUser, Role};
use crate::infra::clock::now_naive;
use crate::modules::com::customer::repo::CustomerRepo;
use crate::modules::com::delivery_note::repo::DeliveryNoteRepoTrait;
use crate::modules::part::model::TPart;
use crate::modules::part::repo::PartRepo;
use crate::modules::prod::batch::repo::PartBatchRepo;
use crate::shared::error::{AppError, code};

use super::super::dto::DeliveryNoteUpdateRequest;
use super::super::repo::SortDir;
use super::super::vo::{
    DeliveryNoteDetailOut, DeliveryNoteListOut, DeliveryNoteOut, delivery_seq_i32,
};
use super::inner::{build_note_outs, get_with_parts, note_not_found, note_version_conflict};
use super::shippable_sets::note_shippable_sets;

use super::DeliveryNoteService;

const STATUS_DRAFT: &str = "DRAFT";
const STATUS_SUBMITTED: &str = "SUBMITTED";

impl DeliveryNoteService {
    // ---------- list ----------

    #[allow(clippy::too_many_arguments)]
    pub async fn list_with_filters<R: DeliveryNoteRepoTrait>(
        &self,
        mut repo: R,
        statuses: &[&str],
        customer_id: Option<i64>,
        keyword: Option<&str>,
        sort_by: super::super::model::DeliveryNoteSortKey,
        sort_dir: SortDir,
        limit: i64,
        offset: i64,
        current: &CurrentUser,
    ) -> Result<DeliveryNoteListOut, AppError> {
        current.require_any_role(&[
            Role::Manager,
            Role::Clerk,
            Role::Inspector,
            Role::CncProgrammer,
        ])?;

        let rows = repo
            .note_list_with_filters(
                statuses,
                customer_id,
                keyword,
                sort_by,
                sort_dir,
                limit,
                offset,
            )
            .await?;
        let total = repo
            .note_count_with_filters(statuses, customer_id, keyword)
            .await?;

        let items = build_note_outs(&mut *repo.conn_mut(), &rows).await?;
        Ok(DeliveryNoteListOut {
            items,
            total,
            limit,
            offset,
        })
    }

    // ---------- get_with_parts ----------

    pub async fn get_with_parts<R: DeliveryNoteRepoTrait>(
        &self,
        mut repo: R,
        note_id: i64,
    ) -> Result<DeliveryNoteDetailOut, AppError> {
        get_with_parts(&mut *repo.conn_mut(), note_id).await
    }

    // ---------- get_many_with_parts (PR3 batch-detail) ----------

    /// 批查 N 个送货单详情（PR3 batch-detail 专用）。固定 6 次 Postgres 往返：
    /// 1) `DeliveryNoteRepo::list_by_ids` 头
    /// 2) `PartBatchRepo::list_with_part_by_delivery_note_ids` 批次+工单
    /// 3) `CustomerRepo::list_by_ids` (leaf) L2
    /// 4) `CustomerRepo::list_by_ids` (parent) L1
    /// 5) `AssemblyRepo::list_by_ids` 装配件
    /// 6) `build_note_outs(&heads)` head → DeliveryNoteOut（内部已批 driver / group）
    ///
    /// 输出按入参 `ids` 顺序排列；缺失 id 静默跳过；入参应已 dedupe（caller 责任）。
    #[allow(clippy::too_many_lines)]
    pub async fn get_many_with_parts<R: DeliveryNoteRepoTrait>(
        &self,
        mut repo: R,
        ids: &[i64],
    ) -> Result<Vec<DeliveryNoteDetailOut>, AppError> {
        use crate::modules::assembly::model::TAssembly;
        use crate::modules::assembly::repo::AssemblyRepo;
        use crate::modules::com::customer::model::TCustomer;
        use std::collections::HashSet;

        use super::super::vo::DeliveryNoteLineItem;

        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let heads = repo.note_list_by_ids(ids, false).await?;
        if heads.is_empty() {
            return Ok(Vec::new());
        }
        let head_ids: Vec<i64> = heads.iter().map(|n| n.id).collect();

        let rows =
            PartBatchRepo::list_with_part_by_delivery_note_ids(&mut *repo.conn_mut(), &head_ids)
                .await?;

        let leaf_ids: HashSet<i64> = rows.iter().map(|(_b, p)| p.customer_id).collect();
        let leaf_list = CustomerRepo::list_by_ids(
            &mut *repo.conn_mut(),
            &leaf_ids.iter().copied().collect::<Vec<_>>(),
            false,
        )
        .await?;
        let leaf_map: HashMap<i64, TCustomer> = leaf_list.into_iter().map(|c| (c.id, c)).collect();

        let parent_ids: HashSet<i64> = leaf_map.values().filter_map(|c| c.parent_id).collect();
        let parent_list = if parent_ids.is_empty() {
            Vec::new()
        } else {
            CustomerRepo::list_by_ids(
                &mut *repo.conn_mut(),
                &parent_ids.iter().copied().collect::<Vec<_>>(),
                false,
            )
            .await?
        };
        let parent_map: HashMap<i64, TCustomer> =
            parent_list.into_iter().map(|c| (c.id, c)).collect();

        let asm_ids: Vec<i64> = rows
            .iter()
            .filter_map(|(_b, p)| p.assembly_id)
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        let mut assembly_map: HashMap<i64, TAssembly> = HashMap::new();
        if !asm_ids.is_empty() {
            let asms = AssemblyRepo::list_by_ids(&mut *repo.conn_mut(), &asm_ids, false).await?;
            for a in asms {
                assembly_map.insert(a.id, a);
            }
        }

        // 2026-10-04 新增：套数公式的 `min` 定义域是「该装配件的**全部**子件」，
        // 本单没批次的子件按 0 参与（否则会算出凑不齐的套数 → 错标签）。批量详情
        // 是 N 单 × M 装配件，不能逐个 `list_children`，故 1 条 SQL 批量取。
        // 与 `inner.rs::get_with_parts` 同 `include_deleted=false` 口径 ⇒ 批量详情
        // 与单张详情给同一装配件的套数同源同值。
        let children_by_asm: HashMap<i64, Vec<TPart>> = if assembly_map.is_empty() {
            HashMap::new()
        } else {
            // 2026-10-04 review 第 3 轮（INFO-5）：按 `assembly_map` 的 key 取子件，
            // 不用 `asm_ids`。差集 = 被 `include_deleted=false` 判为软删的装配件，
            // 它的子件查回来也用不上（`asm_quantity` 里没有该 key，`note_shippable_sets`
            // 直接跳过），对应行上 `shippable_sets` 取 `None` 与现状一致，纯省一批
            // 无谓往返。
            let keys: Vec<i64> = assembly_map.keys().copied().collect();
            let mut m: HashMap<i64, Vec<TPart>> = HashMap::with_capacity(keys.len());
            for c in
                PartRepo::list_children_by_assemblies(&mut *repo.conn_mut(), &keys, false).await?
            {
                if let Some(aid) = c.assembly_id {
                    m.entry(aid).or_default().push(c);
                }
            }
            m
        };
        let asm_quantity: HashMap<i64, i32> = assembly_map
            .iter()
            .map(|(id, a)| (*id, a.quantity))
            .collect();

        let head_outs = build_note_outs(&mut *repo.conn_mut(), &heads).await?;
        let head_out_map: HashMap<i64, DeliveryNoteOut> =
            head_outs.into_iter().map(|h| (h.id, h)).collect();

        // 按 b.delivery_note_id 分桶
        let mut by_note: HashMap<
            i64,
            Vec<(
                crate::shared::batch::TPartBatch,
                crate::modules::part::model::TPart,
            )>,
        > = HashMap::new();
        for r in rows {
            if let Some(nid) = r.0.delivery_note_id {
                by_note.entry(nid).or_default().push(r);
            }
        }

        // 按入参 ids 顺序装配
        let mut out = Vec::with_capacity(heads.len());
        for nid in &head_ids {
            let Some(head) = head_out_map.get(nid) else {
                continue;
            };
            let items_rows = by_note.remove(nid).unwrap_or_default();
            // 2026-10-04 新增：循环前先在本单行集上聚合一次可出货套数。
            let sets_map = note_shippable_sets(&items_rows, &asm_quantity, &children_by_asm);
            let mut items: Vec<DeliveryNoteLineItem> = Vec::with_capacity(items_rows.len());
            for (b, p) in items_rows {
                let leaf = leaf_map.get(&p.customer_id);
                let parent = leaf
                    .and_then(|l| l.parent_id)
                    .and_then(|pid| parent_map.get(&pid));
                let leaf_name = leaf.map(|c| c.name.clone());
                let parent_name = parent.map(|c| c.name.clone()).or_else(|| leaf_name.clone());
                let path = match (&parent_name, &leaf_name) {
                    (Some(p), Some(l)) if p != l => Some(format!("{p} / {l}")),
                    _ => leaf_name.clone(),
                };
                let asm = p.assembly_id.and_then(|id| assembly_map.get(&id));
                let batch_label = match &p.serial_no {
                    Some(s) => format!("{s}B{:02}", b.batch_no),
                    None => format!("批次{}", b.batch_no),
                };
                items.push(DeliveryNoteLineItem {
                    id: b.id,
                    part_id: p.id,
                    batch_no: b.batch_no,
                    // 加入本单的次序（bigint → i32 的收窄口径见 `delivery_seq_i32`；批量详情
                    // 逐单分桶保持 SQL 返回序，故桶内也是这个次序）
                    delivery_seq: delivery_seq_i32(b.delivery_seq),
                    batch_label,
                    serial_no: p.serial_no.clone().unwrap_or_default(),
                    drawing_no: p.drawing_no.clone(),
                    name: p.name.clone(),
                    quantity: b.quantity,
                    status: b.status.clone(),
                    applicant_name: Some(p.applicant_name.clone()).filter(|s| !s.is_empty()),
                    request_date: Some(p.request_date),
                    planned_delivery_date: Some(p.planned_delivery_date),
                    system_delivery_date: p.system_delivery_date,
                    order_no: p.order_no.clone(),
                    note: p.note.clone(),
                    customer_name: leaf_name,
                    parent_customer_name: parent_name,
                    customer_path: path,
                    // 与 `leaf_map` 的查表 key 同值，不额外查库
                    customer_id: p.customer_id,
                    assembly_id: asm.map(|a| a.id),
                    assembly_serial_no: asm.and_then(|a| a.serial_no.clone()),
                    assembly_drawing_no: asm.map(|a| a.drawing_no.clone()),
                    assembly_name: asm.map(|a| a.name.clone()),
                    assembly_order_no: asm.and_then(|a| a.order_no.clone()),
                    // 2026-10-04 新增：装配件工单总套数 + 本单可出货套数（散件 None）
                    assembly_quantity: asm.map(|a| a.quantity),
                    shippable_sets: p.assembly_id.and_then(|id| sets_map.get(&id).copied()),
                });
            }
            out.push(DeliveryNoteDetailOut {
                head: head.clone(),
                line_items: items,
            });
        }
        Ok(out)
    }

    // ---------- update (partial) ----------

    pub async fn update<R: DeliveryNoteRepoTrait>(
        &self,
        mut repo: R,
        note_id: i64,
        req: DeliveryNoteUpdateRequest,
        current: &CurrentUser,
    ) -> Result<DeliveryNoteOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk, Role::Inspector])?;

        let mut obj = repo
            .note_get_by_id(note_id, false)
            .await?
            .ok_or_else(|| note_not_found(note_id))?;

        if obj.version != req.version {
            return Err(note_version_conflict(note_id, obj.version, req.version));
        }
        if obj.status != STATUS_DRAFT && obj.status != STATUS_SUBMITTED {
            return Err(AppError::biz(
                code::BIZ_DELIVERY_NOTE_INVALID_TRANSITION,
                format!(
                    "cannot update {st} note; only DRAFT/SUBMITTED is editable",
                    st = obj.status
                ),
            ));
        }

        let now = now_naive();
        let mut changed = false;
        if let Some(d) = req.delivery_date
            && Some(d) != obj.delivery_date
        {
            obj.delivery_date = Some(d);
            changed = true;
        }
        if let Some(ref n) = req.note {
            // trim 后空字符串存 NULL，否则存 trim 结果
            let next: Option<String> = if n.trim().is_empty() {
                None
            } else {
                Some(n.trim().to_string())
            };
            if next != obj.note {
                obj.note = next;
                changed = true;
            }
        }
        if changed {
            obj.version += 1;
            obj.updated_at = now;
            obj.updated_by = Some(current.id);
            let affected = repo.note_update(&obj).await?;
            if affected == 0 {
                return Err(AppError::biz(
                    code::VERSION_CONFLICT,
                    "concurrent modification detected",
                ));
            }
            // 立即 reload 让 updated_at 拿到 server 值
            obj = repo
                .note_get_by_id(note_id, false)
                .await?
                .ok_or_else(|| note_not_found(note_id))?;
        }
        let out = build_note_outs(&mut *repo.conn_mut(), std::slice::from_ref(&obj)).await?;
        Ok(out.into_iter().next().unwrap())
    }

    // ---------- remove_batches ----------

    pub async fn remove_batches<R: DeliveryNoteRepoTrait>(
        &self,
        mut repo: R,
        note_id: i64,
        batch_ids: &[i64],
        version: i32,
        current: &CurrentUser,
    ) -> Result<DeliveryNoteDetailOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk, Role::Inspector])?;

        let obj = repo
            .note_get_by_id(note_id, false)
            .await?
            .ok_or_else(|| note_not_found(note_id))?;
        if obj.version != version {
            return Err(note_version_conflict(note_id, obj.version, version));
        }
        if obj.status != STATUS_DRAFT {
            return Err(AppError::biz(
                code::BIZ_DELIVERY_NOTE_PARTS_LOCKED,
                format!(
                    "送货单已提交（{}），不能移除批次；如需调整请先撤回。",
                    obj.status
                ),
            ));
        }

        if batch_ids.is_empty() {
            return get_with_parts(&mut *repo.conn_mut(), note_id).await;
        }

        let now = now_naive();
        // 清空确实属于本单的 batch.delivery_note_id（version 校验 + 仅限本单）
        // 2026-10-10 新增：同一条 UPDATE 清 delivery_seq，维持
        // 「delivery_seq IS NULL ⟺ delivery_note_id IS NULL」。不清的话这个批次
        // 日后挂到另一张单上会带着上一张单的旧序号，详情排序直接错位。
        for bid in batch_ids {
            let _ = sqlx::query!(
                r#"
                UPDATE t_part_batch
                SET delivery_note_id = NULL,
                    delivery_seq     = NULL,
                    version          = version + 1,
                    updated_at       = $2,
                    updated_by       = $3
                WHERE id = $1 AND delivery_note_id = $4 AND deleted_at IS NULL
                "#,
                bid,
                now,
                Some(current.id),
                note_id,
            )
            .execute(&mut *repo.conn_mut())
            .await?;
        }

        get_with_parts(&mut *repo.conn_mut(), note_id).await
    }
}

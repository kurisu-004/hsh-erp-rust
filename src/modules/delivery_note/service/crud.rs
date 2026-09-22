//! DeliveryNoteService 列表 / 草稿 / 详情 / 编辑 / 添加 / 移除。
//!
//! ## 2026-09-22 D-5 + review 第 1 轮修正（service by-value trait）
//! - 所有方法签名从 `pub async fn xxx(conn: &mut PgConnection, snowflake: &SnowflakeIdGenerator, ...)`
//!   改成 `pub async fn xxx<R: DeliveryNoteRepoTrait>(&self, mut repo: R, ...)`（iam 严格范本）。
//! - 跨域 ZST 静态调用走 `&mut *repo.conn_mut()`；私有 helper（`add_parts_inner` /
//!   `build_note_outs` / `get_with_parts` / `write_event`）收 `&mut PgConnection`，
//!   caller 喂 `&mut *repo.conn_mut()`。
//! - 原 `sqlx::query!(...)` 直调走 `&mut *repo.conn_mut()` 替换 `&mut *conn`。

use std::collections::HashMap;

use crate::auth::rbac::{CurrentUser, Role};
use crate::infra::clock::now_naive;
use crate::infra::serial::next_delivery_note_no;
use crate::modules::com::customer::repo::CustomerRepo;
use crate::modules::delivery_note::repo::DeliveryNoteRepoTrait;
use crate::modules::part::batch::repo::PartBatchRepo;
use crate::shared::error::{AppError, code};

use super::super::dto::{
    DeliveryNoteAddItem, DeliveryNoteCreateRequest, DeliveryNoteDetailOut, DeliveryNoteListOut,
    DeliveryNoteOut, DeliveryNoteUpdateRequest,
};
use super::super::model::{DeliveryNote, DeliveryNoteEventType};
use super::super::repo::SortDir;
use super::inner::{
    add_parts_inner, build_note_outs, get_with_parts, note_not_found, note_version_conflict,
    write_event,
};

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

    pub async fn list_for_pickup<R: DeliveryNoteRepoTrait>(
        &self,
        mut repo: R,
        customer_id: Option<i64>,
        current: &CurrentUser,
    ) -> Result<Vec<DeliveryNoteOut>, AppError> {
        // 司机扫码台用：任意已登录账号 + service 层校验 driver work_type
        // 这里不做角色硬限；具体 worker 校验在 pickup/pickup_scan 里。
        let _ = current;

        let rows = repo.note_list_for_pickup(customer_id).await?;
        build_note_outs(&mut *repo.conn_mut(), &rows).await
    }

    // ---------- create_draft ----------

    pub async fn create_draft<R: DeliveryNoteRepoTrait>(
        &self,
        mut repo: R,
        req: DeliveryNoteCreateRequest,
        current: &CurrentUser,
    ) -> Result<DeliveryNoteDetailOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk, Role::Inspector])?;

        // 1. 校验 L1 存在且是 L1（parent_id IS NULL）
        let l1 = CustomerRepo::get_by_id(&mut *repo.conn_mut(), req.customer_id, false)
            .await?
            .ok_or_else(|| super::inner::customer_not_found(req.customer_id))?;
        if l1.parent_id.is_some() {
            return Err(AppError::biz(
                code::BIZ_DELIVERY_NOTE_PARTS_MULTIPLE_CUSTOMERS,
                format!(
                    "customer {l1_id} 不是一级客户（parent_id 必须为 NULL）；送货单必须挂在一级客户下",
                    l1_id = req.customer_id
                ),
            ));
        }

        // 2. 发放单号
        let delivery_note_no =
            next_delivery_note_no(&mut *repo.conn_mut(), req.customer_id).await?;

        // 3. 写入草稿
        let now = now_naive();
        let note = DeliveryNote {
            id: self.snowflake.next_id(),
            delivery_note_no,
            customer_id: req.customer_id,
            status: STATUS_DRAFT.to_string(),
            submitted_at: None,
            picked_up_at: None,
            submitted_by: None,
            picked_up_by: None,
            driver_worker_id: None,
            note: req.note,
            delivery_date: Some(req.delivery_date.unwrap_or_else(|| now.date())),
            version: 0,
            created_at: now,
            created_by: Some(current.id),
            updated_at: now,
            updated_by: Some(current.id),
            deleted_at: None,
            delivery_group_id: None,
            leaf_customer_id: None,
        };
        repo.note_create(&note).await?;

        // 4. CREATED 事件
        write_event(
            &mut *repo.conn_mut(),
            &self.snowflake,
            note.id,
            DeliveryNoteEventType::Created,
            None,
            Some(STATUS_DRAFT.to_string()),
            Some(format!("create draft for customer {}", l1.name)),
            Some(current.id),
        )
        .await?;

        // 5. 原子带入首批零件（如果给了 items）
        if !req.items.is_empty() {
            add_parts_inner(
                &mut *repo.conn_mut(),
                &self.snowflake,
                note.id,
                &req.items,
                note.version,
                current,
            )
            .await?;
        }

        get_with_parts(&mut *repo.conn_mut(), note.id).await
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

        use super::super::dto::DeliveryNoteLineItem;

        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let heads = repo.note_list_by_ids(ids, false).await?;
        if heads.is_empty() {
            return Ok(Vec::new());
        }
        let head_ids: Vec<i64> = heads.iter().map(|n| n.id).collect();

        let rows = PartBatchRepo::list_with_part_by_delivery_note_ids(
            &mut *repo.conn_mut(),
            &head_ids,
        )
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

        let head_outs = build_note_outs(&mut *repo.conn_mut(), &heads).await?;
        let head_out_map: HashMap<i64, DeliveryNoteOut> =
            head_outs.into_iter().map(|h| (h.id, h)).collect();

        // 按 b.delivery_note_id 分桶
        let mut by_note: HashMap<
            i64,
            Vec<(
                crate::modules::part::batch::model::TPartBatch,
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
                    batch_label,
                    serial_no: p.serial_no.clone().unwrap_or_default(),
                    drawing_no: p.drawing_no.clone(),
                    name: p.name.clone(),
                    quantity: b.quantity,
                    is_urgent: false,
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
                    is_scanned: false,
                    scanned: false,
                    assembly_id: asm.map(|a| a.id),
                    assembly_serial_no: asm.and_then(|a| a.serial_no.clone()),
                    assembly_drawing_no: asm.map(|a| a.drawing_no.clone()),
                    assembly_name: asm.map(|a| a.name.clone()),
                    assembly_order_no: asm.and_then(|a| a.order_no.clone()),
                });
            }
            out.push(DeliveryNoteDetailOut {
                head: head.clone(),
                line_items: items,
                scanned_serials: vec![],
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

    // ---------- add_parts ----------

    pub async fn add_parts<R: DeliveryNoteRepoTrait>(
        &self,
        mut repo: R,
        note_id: i64,
        items: &[DeliveryNoteAddItem],
        version: i32,
        current: &CurrentUser,
    ) -> Result<DeliveryNoteDetailOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk, Role::Inspector])?;
        add_parts_inner(
            &mut *repo.conn_mut(),
            &self.snowflake,
            note_id,
            items,
            version,
            current,
        )
        .await?;
        get_with_parts(&mut *repo.conn_mut(), note_id).await
    }

    // ---------- remove_parts ----------

    pub async fn remove_parts<R: DeliveryNoteRepoTrait>(
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
                    "送货单已提交（{}），不能移除零件；如需调整请先撤回。",
                    obj.status
                ),
            ));
        }

        if batch_ids.is_empty() {
            return get_with_parts(&mut *repo.conn_mut(), note_id).await;
        }

        let now = now_naive();
        // 清空确实属于本单的 batch.delivery_note_id（version 校验 + 仅限本单）
        for bid in batch_ids {
            let _ = sqlx::query!(
                r#"
                UPDATE t_part_batch
                SET delivery_note_id = NULL,
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
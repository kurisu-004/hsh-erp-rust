//! part 域单件 CRUD 业务逻辑（2026-09-16 M2-C 拆分）
//!
//! 本文件承载单件 CRUD + 列表 + 上传 + 历史 / 位置树等查询；批量创建相关逻辑
//! （含直传 COS 文件绑定 `batch_create_parts_with_bindings` / legacy 路径 /
//! `prepare_binding_head_copy`）已迁出到 `service/batch.rs`，避免单文件超 1000 行。
//!
//! ## 范围（本文件）
//! - `create_part` / `list_parts` / `list_inspection_batches` / `get_part` /
//!   `get_part_by_serial` / `get_part_batches_by_serial` / `update_part` /
//!   `soft_delete_part` / `upload_part_file` / `upload_drawing` / `upload_3d_model`
//! - `batch_create_parts` —— 薄包装，转 `service/batch.rs::batch_create_parts_legacy`
//!
//! ## helpers（pub(super)，供 batch.rs 复用）
//! - `map_create_error` —— sqlx 错误码 → 业务错误码
//! - `expand_customer_id` —— L1+L2 客户 id 展开
//! - `lookup_customer_names` —— 取客户名 + L1 名
//!
//! 2026-09-22 D-6 重构：方法签名 `<R: PartRepoTrait>`（by-value；trait 已直接
//! `impl for &mut PgConnection`）。生产 `R = &mut PgConnection`，handler/service
//! 借 `&mut *tx` / `repo.conn_mut()` 即可喂给 trait 与跨域 ZST 调用。Inline sqlx 查询
//! 与 ZST 跨域调用走 `repo.conn_mut()`（同一 `PgConnection` 借位，trait 与
//! `CustomerRepo` / `ProcessChainRepo` / `PartBatchRepo` 等同时持有）。

use sqlx::PgConnection;
use std::sync::Arc;

use chrono::NaiveDate;
use rust_decimal::Decimal;

use crate::auth::rbac::{CurrentUser, Role};
use crate::infra::snowflake::SnowflakeIdGenerator;
use crate::modules::assembly::model::TAssembly;
use crate::modules::assembly::repo::sql::{AssemblyListFilters, AssemblyRepo};
use crate::modules::assembly::service::AssemblyService;
use crate::modules::com::customer::repo::CustomerRepo;
use crate::modules::part::batch::repo::{NewInitialBatch, PartBatchRepo};
use crate::modules::part::dto::InspectionBatchListQuery;
use crate::modules::part::model::NewPartEvent;
use crate::modules::part::repo::PartRepoTrait;
use crate::modules::part::repo::{NewPartCreate, PartListFilters, PartUpdate};
use crate::modules::part::vo::{
    InspectionBatchListItemOut, InspectionBatchListOut, PartBatchScanOut, PartDetailOut,
    PartListItem, PartListOut, PartScanContextOut, PartScanInfoOut,
};
use crate::modules::part_file::model::TPartFile;
use crate::modules::part_file::policy; // 2026-09-11 新增：kind → 扩展名 / content_type 白名单
use crate::modules::part_file::repo::{NewPartFile, PartFileRepo, hash_bytes};
use crate::modules::prod::process_chain::repo::ProcessChainRepo;
use crate::shared::error::{AppError, code};
use crate::state::AppState;

use super::super::dto_crud::{
    PartBatchCreateRequest, PartCreateRequest, PartListQuery, PartUpdateRequest,
};
use super::PartService;
use super::list_enrichment::enrich_part_list_with_location_and_holder;

/// 2026-09-28 新增：`list_parts` 行类型分支内部枚举（DTO → service 内部映射）。
///
/// 解析矩阵见 `PartListQuery` 字段 doc 注释：
/// - `Part`：仅 `t_part WHERE assembly_id IS NULL`（PART 模式 / `include_assemblies=false` 兼容路径）
/// - `Assembly`：仅 `t_assembly` 投影为 `PartListItem` 形态
/// - `All`：`t_part` UNION `t_assembly`，按统一 sort_key 内存合并
#[derive(Debug, Clone, Copy)]
enum RowMode {
    Part,
    Assembly,
    All,
}

/// 扫码快捷品检上下文内部 FromRow 结构。
///
/// 仅本 crate 可见：`get_part_batches_by_serial` 用 `sqlx::query_as!` 接收
/// `t_part` 的窄字段（id + 8 列），避免读 28 列 `TPart`。由
/// `PartScanInfoOut::from` 转 DTO，转换实现位于 `src/modules/part/dto.rs`
/// （与 DTO 同处，便于维护）。
#[derive(sqlx::FromRow)]
pub(crate) struct TPartScanRow {
    pub(crate) id: i64,
    pub(crate) drawing_no: String,
    pub(crate) name: String,
    pub(crate) quantity: i32,
    pub(crate) customer_id: i64,
    pub(crate) system_delivery_date: Option<NaiveDate>,
    pub(crate) is_urgent: bool,
    pub(crate) order_no: Option<String>,
    pub(crate) note: Option<String>,
}

impl PartService {
    pub async fn create_part<R: PartRepoTrait>(
        mut repo: R,
        snowflake: &SnowflakeIdGenerator,
        req: &PartCreateRequest,
        current: &CurrentUser,
    ) -> Result<PartDetailOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk])?;
        if req.name.trim().is_empty()
            || req.drawing_no.trim().is_empty()
            || req.applicant_name.trim().is_empty()
        {
            return Err(AppError::validation(
                "name / drawing_no / applicant_name 均不可为空",
            ));
        }
        if req.quantity <= 0 {
            return Err(AppError::validation("quantity 必须 > 0"));
        }
        let _customer = CustomerRepo::get_by_id(repo.conn_mut(), req.customer_id, false)
            .await?
            .ok_or_else(|| {
                AppError::biz(
                    code::BIZ_CUSTOMER_NOT_FOUND,
                    format!("customer {} 不存在", req.customer_id),
                )
            })?;
        let new_id = snowflake.next_id();
        let new = NewPartCreate {
            id: new_id,
            name: req.name.trim(),
            drawing_no: req.drawing_no.trim(),
            applicant_name: req.applicant_name.trim(),
            quantity: req.quantity,
            request_date: req.request_date,
            planned_delivery_date: req.planned_delivery_date,
            is_urgent: req.is_urgent,
            customer_id: req.customer_id,
            assembly_id: req.assembly_id,
            order_no: req.order_no.as_deref(),
            system_delivery_date: req.system_delivery_date,
            note: req.note.as_deref(),
            created_by: current.id,
        };
        if let Err(e) = repo.create_part(new).await {
            return Err(map_create_error(e));
        }
        // 2026-09-11 part/assembly/batch 重构方案 §4.1 (PR-B1)：同事务插入初始
        // t_part_batch（batch_no=1 / status='PENDING' / location=NULL），让新建工单
        // 即可走 to_inspection / to_ship / pickup 等 batch-锚定流转。
        let initial_batch_id = snowflake.next_id();
        PartBatchRepo::create_initial_batch(
            repo.conn_mut(),
            NewInitialBatch {
                id: initial_batch_id,
                part_id: new_id,
                quantity: req.quantity,
                location: None,
                created_by: Some(current.id),
            },
        )
        .await?;
        let part = repo
            .get_part_detail(new_id)
            .await?
            .ok_or_else(|| AppError::biz(code::BIZ_PART_NOT_FOUND, "新建 part 查不到"))?;
        let (cn, l1cn) = lookup_customer_names(repo.conn_mut(), part.customer_id).await?;
        let current_batch_id = repo.find_current_inspection_batch_id(part.id).await?;
        Ok(PartDetailOut::from_with_customer_extra(
            part,
            current_batch_id,
            cn,
            l1cn,
        ))
    }

    pub async fn batch_create_parts<R: PartRepoTrait>(
        repo: R,
        snowflake: &SnowflakeIdGenerator,
        req: &PartBatchCreateRequest,
        current: &CurrentUser,
    ) -> Result<crate::modules::part::vo::PartBatchCreateOut, AppError> {
        // 2026-09-16 M2-B + M2-C：薄包装转 legacy 实现（不绑定文件）。
        // 文件绑定走 `batch_create_parts_with_bindings`（handler 层显式选，
        // 实现已迁出到 `service/batch.rs`）。
        Self::batch_create_parts_legacy(repo, snowflake, req, current).await
    }

    /// 2026-09-25 新增（D-08 api-drift-fix）：按 part 反查所属装配体。
    ///
    /// 行为：
    /// - 权限：4 角色全开放（与 `get_part` 一致）
    /// - `part` 不存在 → `40400 NOT_FOUND`
    /// - `part.assembly_id IS NULL` → 返回 `Ok(None)`（无父装配体）
    /// - 否则委托 `AssemblyService::get_assembly` 拿 AssemblyDetail
    ///
    /// 实现要点：因 `part` service 通过 `&mut PgConnection` 持连接，调用
    /// `AssemblyService::get_assembly` 时直接传 `&mut conn`（不重新开事务）。
    /// 跨域读 → `AssemblyRepoTrait` 的 `fetch_part_assembly_id` helper 不在
    /// PartRepoTrait 上，所以这里走 `repo.conn_mut()` 直接 inline SQL。
    pub async fn get_assembly_by_part<R: PartRepoTrait>(
        mut repo: R,
        part_id: i64,
        current: &CurrentUser,
    ) -> Result<Option<crate::modules::assembly::vo::AssemblyDetail>, AppError> {
        current.require_any_role(&[
            Role::Manager,
            Role::Clerk,
            Role::Inspector,
            Role::CncProgrammer,
        ])?;

        // 1. 校验 part 存在 + 拿 assembly_id（单条 SQL，避免拿全行 TPart）
        let part_row: Option<(Option<i64>,)> =
            sqlx::query_as("SELECT assembly_id FROM t_part WHERE id = $1 AND deleted_at IS NULL")
                .bind(part_id)
                .fetch_optional(repo.conn_mut())
                .await?;
        let part_row = part_row.ok_or_else(|| {
            AppError::biz(
                code::BIZ_PART_NOT_FOUND,
                format!("part {part_id} 不存在或已删除"),
            )
        })?;
        // 2. assembly_id IS NULL → 返回 None（无父装配体）
        let asm_id = match part_row.0 {
            Some(id) => id,
            None => return Ok(None),
        };
        // 3. 委托 assembly service 拿详情（含 children + files）
        //    这里直接复用 `AssemblyService::get_assembly` 的 trait 注入形式，
        //    传 `repo.conn_mut()`（同一连接，避免重复借用）。
        let conn = repo.conn_mut();
        let detail = AssemblyService::get_assembly(conn, asm_id, current).await?;
        Ok(Some(detail))
    }

    pub async fn list_parts<R: PartRepoTrait>(
        repo: R,
        query: &PartListQuery,
        current: &CurrentUser,
    ) -> Result<PartListOut, AppError> {
        current.require_any_role(&[
            Role::Manager,
            Role::Clerk,
            Role::Inspector,
            Role::CncProgrammer,
        ])?;
        let limit = query.limit.unwrap_or(50).clamp(1, 200);
        let offset = query.offset.unwrap_or(0).max(0);

        // 2026-09-28 新增：行类型筛选矩阵 normalize。
        // 矩阵详见 `PartListQuery` 字段 doc 注释：
        // | row_type    | include_assemblies | 模式              |
        // |-------------|--------------------|-------------------|
        // | "PART"      | 任意               | Part              |
        // | "ASSEMBLY"  | 任意               | Assembly          |
        // | absent      | false              | Part（兼容旧 caller）|
        // | absent      | true / absent      | All               |
        // | 其它非空    | 任意               | 40001 VALIDATION_ERROR |
        let mode = match (query.row_type.as_deref(), query.include_assemblies.unwrap_or(true)) {
            (Some("PART"), _) => RowMode::Part,
            (Some("ASSEMBLY"), _) => RowMode::Assembly,
            (None, false) => RowMode::Part,
            (None, true) => RowMode::All,
            _ => {
                return Err(AppError::validation(
                    "row_type 非法，必须是 PART / ASSEMBLY 或省略",
                ));
            }
        };

        match mode {
            RowMode::Part => {
                // PART-only 分支：含 total 计数；与历史 PART-only 行为一致
                // （兼容旧 caller：`/parts/pending-programming` 等内部端点
                // 走 `include_assemblies=false` 也会进此分支）。
                Self::list_parts_part_only_with_total(repo, query, limit, offset, current).await
            }
            RowMode::Assembly => {
                Self::list_parts_assembly_only(repo, query, limit, offset, current).await
            }
            RowMode::All => {
                Self::list_parts_all_merged(repo, query, limit, offset, current).await
            }
        }
    }

    /// PART-only / 「include_assemblies=false」分支。
    ///
    /// 走既有 `PartRepo::list_with_filters`（带 `part_only=true` 守卫）+ batch
    /// enrichment + customer_name / l1_customer_name 注入。**保留全部既有
    /// 行为**（最小破坏面），仅在最后把 `row_type="PART"` 显式写到所有 item。
    async fn list_parts_part_only_with_total<R: PartRepoTrait>(
        mut repo: R,
        query: &PartListQuery,
        limit: i64,
        offset: i64,
        _current: &CurrentUser,
    ) -> Result<PartListOut, AppError> {
        let (sort_by, sort_dir, customer_ids, statuses_owned, locations_owned, holder_ids_owned) =
            Self::parse_list_filters(query, &mut repo).await?;

        let filters = PartListFilters {
            customer_ids: &customer_ids,
            status: query.status.as_deref(),
            statuses: &statuses_owned,
            is_urgent: query.is_urgent,
            keyword: query.keyword.as_deref(),
            locations: &locations_owned,
            holder_ids: &holder_ids_owned,
            // 2026-09-28 新增：PART-only 模式强制打开装配体子件守卫
            part_only: true,
            sort_by: &sort_by,
            sort_dir: &sort_dir,
            limit,
            offset,
            include_deleted: false,
        };
        let rows = repo.list_with_filters(&filters).await?;
        let total = repo.count_with_filters(&filters).await?;

        // batch enrichment（min-progress 活跃批次 → location / holder_name）
        let part_ids: Vec<i64> = rows.iter().map(|p| p.id).collect();
        let batch_enrichment =
            enrich_part_list_with_location_and_holder(&mut repo, &part_ids).await?;

        let mut items = Vec::with_capacity(rows.len());
        for p in rows {
            let (cn, l1cn) = lookup_customer_names(repo.conn_mut(), p.customer_id).await?;
            let (loc, holder) = batch_enrichment.get(&p.id).cloned().unwrap_or((None, None));
            // 2026-09-27 review 第 1 修复：`From<TPart>` 派生基础字段；4 个派生字段
            // 由 service 注入（保持旧行为）。
            let mut item: PartListItem = p.into();
            item.customer_name = cn;
            item.l1_customer_name = l1cn;
            item.location = loc;
            item.holder_name = holder;
            items.push(item);
        }
        Ok(PartListOut {
            items,
            total,
            limit,
            offset,
        })
    }

    /// ASSEMBLY-only 分支：直接复用 `AssemblyRepo::list_with_filters` /
    /// `count_with_filters`（零修改 `AssemblyService::list_assemblies`），再
    /// 把每行 `TAssembly` 投影为 `PartListItem` 形态：
    /// - `row_type="ASSEMBLY"`
    /// - `has_children=child_count.unwrap_or(0) > 0`
    /// - `child_count`：从 `t_part WHERE assembly_id = ANY($1) AND deleted_at IS NULL`
    ///   的 GROUP BY 一次性拿到（≤200 ids / 1 extra query）
    /// - `location / holder_name / process_chain_id / batch_* / assembly_id` 全 None
    /// - `applicant_name` 保留 `Option<String>`（TAssembly 允许空）
    /// - `unit_price / total_price` 保留 `Option<Decimal>` → 转 `Option<String>` 时 None 不兜 0
    async fn list_parts_assembly_only<R: PartRepoTrait>(
        mut repo: R,
        query: &PartListQuery,
        limit: i64,
        offset: i64,
        _current: &CurrentUser,
    ) -> Result<PartListOut, AppError> {
        let (sort_by, sort_dir, customer_ids, _statuses, _locations, _holder_ids) =
            Self::parse_list_filters_for_assembly(query, &mut repo).await?;

        let assembly_filters = AssemblyListFilters {
            customer_ids: &customer_ids,
            status: query.status.as_deref(),
            statuses: &[], // assembly 列表不走 status 复合（与旧 list_assemblies 一致）
            is_urgent: query.is_urgent,
            keyword: query.keyword.as_deref(),
            sort_by: Some(&sort_by),
            sort_dir: Some(&sort_dir),
            limit,
            offset,
            include_deleted: false,
        };
        let asm_rows: Vec<TAssembly> =
            AssemblyRepo::list_with_filters(repo.conn_mut(), &assembly_filters).await?;
        let total =
            AssemblyRepo::count_with_filters(repo.conn_mut(), &assembly_filters).await?;

        // 一次性拉子件计数（GROUP BY assembly_id）
        let asm_ids: Vec<i64> = asm_rows.iter().map(|a| a.id).collect();
        let child_counts = Self::fetch_child_counts(repo.conn_mut(), &asm_ids).await?;

        // 批量拉 customer name（防 N+1）
        let unique_customer_ids: Vec<i64> = asm_rows
            .iter()
            .map(|a| a.customer_id)
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        let mut items = Vec::with_capacity(asm_rows.len());
        for a in asm_rows {
            let (cn, l1cn) = lookup_customer_names(repo.conn_mut(), a.customer_id).await?;
            let cc = child_counts.get(&a.id).copied().unwrap_or(0);
            let mut item: PartListItem = Self::project_assembly_to_part_list_item(a);
            item.customer_name = cn;
            item.l1_customer_name = l1cn;
            item.child_count = Some(cc);
            item.has_children = cc > 0;
            items.push(item);
        }
        let _ = unique_customer_ids; // 仅防御未使用警告（实际由 lookup_customer_names 内查）
        Ok(PartListOut {
            items,
            total,
            limit,
            offset,
        })
    }

    /// ALL 分支：零件 UNION 装配件，按统一 sort_key 内存合并 + 切片。
    ///
    /// 数据流：
    /// 1. 两次 list：part 段 (`part_only=true`) + assembly 段，**都**
    ///    用 `limit = (limit + offset).min(200)`（max page width；上限与单段
    ///    list 端点一致）
    /// 2. 两次 count 相加 → total
    /// 3. sort 键取 `PartListQuery.sort_by`（t_part / t_assembly 共有列交集）：
    ///    CREATED_AT / UPDATED_AT / PLANNED_DELIVERY_DATE / REQUEST_DATE /
    ///    DRAWING_NO / NAME；`SERIAL_NO` 仅 t_part 独有 → ALL 模式降级为
    ///    `CREATED_AT`（在文档中标注）
    /// 4. 内存 merge sort by unified sort_key + secondary id DESC（稳定）
    /// 5. 切 `[offset, offset+limit)`
    ///
    /// 性能：单次 list 上限 200，单页合并 ≤ 400 行；sort 用标准库 TimSort，无
    /// N+1 SQL；子件计数派生仅在装配件段发生（≤ 200 ids / 1 extra query）。
    async fn list_parts_all_merged<R: PartRepoTrait>(
        mut repo: R,
        query: &PartListQuery,
        limit: i64,
        offset: i64,
        _current: &CurrentUser,
    ) -> Result<PartListOut, AppError> {
        let (sort_by_raw, sort_dir, customer_ids, statuses_owned, locations_owned, holder_ids_owned) =
            Self::parse_list_filters(query, &mut repo).await?;
        // ALL 模式 sort 键：t_part / t_assembly 共有列交集。SERIAL_NO 不在
        // t_assembly 上 → 降级 CREATED_AT（见 crud.md 行类型合并规则备注）。
        let sort_by: &'static str = match sort_by_raw.as_str() {
            "CREATED_AT" | "UPDATED_AT" | "PLANNED_DELIVERY_DATE" | "REQUEST_DATE"
            | "DRAWING_NO" | "NAME" => match sort_by_raw.as_str() {
                "CREATED_AT" => "CREATED_AT",
                "UPDATED_AT" => "UPDATED_AT",
                "PLANNED_DELIVERY_DATE" => "PLANNED_DELIVERY_DATE",
                "REQUEST_DATE" => "REQUEST_DATE",
                "DRAWING_NO" => "DRAWING_NO",
                "NAME" => "NAME",
                _ => unreachable!(),
            },
            _ => "CREATED_AT", // SERIAL_NO / 其它 → 降级
        };

        // 段宽：单段最多拉 limit + offset 行（cap 200，与单段 list 端点对齐）
        let segment_limit = (limit + offset).clamp(1, 200);

        // ---------- part 段 ----------
        let part_filters = PartListFilters {
            customer_ids: &customer_ids,
            status: query.status.as_deref(),
            statuses: &statuses_owned,
            is_urgent: query.is_urgent,
            keyword: query.keyword.as_deref(),
            locations: &locations_owned,
            holder_ids: &holder_ids_owned,
            // 2026-09-28 新增：ALL 模式 part 段强制打开装配体子件守卫（装配件走
            // t_assembly 段，不通过 t_part 段出现）。
            part_only: true,
            sort_by, // SERIAL_NO 已降级
            sort_dir: &sort_dir,
            limit: segment_limit,
            offset: 0,
            include_deleted: false,
        };
        let part_rows = repo.list_with_filters(&part_filters).await?;
        let part_total = repo.count_with_filters(&part_filters).await?;

        // ---------- assembly 段 ----------
        let assembly_filters = AssemblyListFilters {
            customer_ids: &customer_ids,
            status: query.status.as_deref(),
            statuses: &statuses_owned, // ALL 模式把 status 也应用到装配件（与零件同语义）
            is_urgent: query.is_urgent,
            keyword: query.keyword.as_deref(),
            sort_by: Some(sort_by),
            sort_dir: Some(&sort_dir),
            limit: segment_limit,
            offset: 0,
            include_deleted: false,
        };
        let asm_rows: Vec<TAssembly> =
            AssemblyRepo::list_with_filters(repo.conn_mut(), &assembly_filters).await?;
        let asm_total =
            AssemblyRepo::count_with_filters(repo.conn_mut(), &assembly_filters).await?;

        // ---------- enrichment ----------
        // PART 段：batch location / holder_name 派生（既有逻辑）
        let part_ids: Vec<i64> = part_rows.iter().map(|p| p.id).collect();
        let batch_enrichment =
            enrich_part_list_with_location_and_holder(&mut repo, &part_ids).await?;

        // ASSEMBLY 段：子件计数派生（一次性 GROUP BY）
        let asm_ids: Vec<i64> = asm_rows.iter().map(|a| a.id).collect();
        let child_counts = Self::fetch_child_counts(repo.conn_mut(), &asm_ids).await?;

        // ---------- 内存 merge ----------
        // 统一 key 抽提 + 二级 id DESC；切到 [offset, offset+limit)。
        let mut all_items: Vec<PartListItem> = Vec::with_capacity(part_rows.len() + asm_rows.len());

        // PART 行
        for p in part_rows {
            let (cn, l1cn) = lookup_customer_names(repo.conn_mut(), p.customer_id).await?;
            let (loc, holder) = batch_enrichment.get(&p.id).cloned().unwrap_or((None, None));
            let mut item: PartListItem = p.into();
            item.customer_name = cn;
            item.l1_customer_name = l1cn;
            item.location = loc;
            item.holder_name = holder;
            all_items.push(item);
        }
        // ASSEMBLY 行
        for a in asm_rows {
            let (cn, l1cn) = lookup_customer_names(repo.conn_mut(), a.customer_id).await?;
            let cc = child_counts.get(&a.id).copied().unwrap_or(0);
            let mut item: PartListItem = Self::project_assembly_to_part_list_item(a);
            item.customer_name = cn;
            item.l1_customer_name = l1cn;
            item.child_count = Some(cc);
            item.has_children = cc > 0;
            all_items.push(item);
        }

        // 排序（统一 sort_by 已经是 repo 层 ORDER BY；ALL 段内还要在内存里
        // 跨段 merge）。用标准库 unstable_sort_by 取二级 id 保证稳定。
        let asc = sort_dir.eq_ignore_ascii_case("ASC");
        all_items.sort_by(|a, b| {
            let ord = match sort_by {
                    "UPDATED_AT" => {
                        let (x, y) = (a.updated_at, b.updated_at);
                        x.cmp(&y)
                    }
                    "PLANNED_DELIVERY_DATE" => a
                        .planned_delivery_date
                        .cmp(&b.planned_delivery_date),
                    "REQUEST_DATE" => a.request_date.cmp(&b.request_date),
                    "DRAWING_NO" => a.drawing_no.cmp(&b.drawing_no),
                    "NAME" => a.name.cmp(&b.name),
                    _ => a.created_at.cmp(&b.created_at), // CREATED_AT / 降级
                };
            // 统一 sort 方向
            let primary = if asc { ord } else { ord.reverse() };
            // 二级：id DESC（稳定，与单段 repo 层排序保持一致）
            primary.then_with(|| b.id.cmp(&a.id))
        });

        // 切片到 [offset, offset+limit)
        let total = part_total + asm_total;
        let items: Vec<PartListItem> = all_items
            .into_iter()
            .skip(offset as usize)
            .take(limit as usize)
            .collect();
        Ok(PartListOut {
            items,
            total,
            limit,
            offset,
        })
    }

    /// 解析 list 过滤参数为内部变量。供 PART / ALL 分支复用。
    /// 返回 `(sort_by, sort_dir, customer_ids, statuses, locations, holder_ids)`，
    /// 全部 String / Vec 在 helper 内持有，借给下游 `PartListFilters` 时取 ref。
    async fn parse_list_filters<R: PartRepoTrait>(
        query: &PartListQuery,
        repo: &mut R,
    ) -> Result<
        (
            String,
            String,
            Vec<i64>,
            Vec<String>,
            Vec<String>,
            Vec<i64>,
        ),
        AppError,
    > {
        let sort_by = [
            "CREATED_AT",
            "UPDATED_AT",
            "PLANNED_DELIVERY_DATE",
            "REQUEST_DATE",
            "SERIAL_NO",
            "DRAWING_NO",
            "NAME",
        ]
        .iter()
        .find(|&&s| Some(s) == query.sort_by.as_deref())
        .copied()
        .unwrap_or("CREATED_AT")
        .to_string();
        let sort_dir = if query
            .sort_dir
            .as_deref()
            .map(|s| s.eq_ignore_ascii_case("ASC"))
            .unwrap_or(false)
        {
            "ASC"
        } else {
            "DESC"
        }
        .to_string();

        let customer_ids: Vec<i64> = if let Some(cid) = query.customer_id {
            expand_customer_id(repo.conn_mut(), cid).await?
        } else {
            Vec::new()
        };
        let statuses: Vec<String> = query
            .statuses
            .as_deref()
            .map(|s| {
                s.split(',')
                    .filter(|x| !x.is_empty())
                    .map(|s| s.to_string())
                    .collect()
            })
            .unwrap_or_default();
        let locations: Vec<String> = query
            .locations
            .as_deref()
            .map(|s| {
                s.split(',')
                    .filter(|x| !x.is_empty())
                    .map(|s| s.trim().to_string())
                    .collect()
            })
            .unwrap_or_default();
        let holder_ids: Vec<i64> = match query.holder_ids.as_deref() {
            Some(s) if !s.is_empty() => s
                .split(',')
                .filter(|x| !x.is_empty())
                .map(|x| {
                    x.trim().parse::<i64>().map_err(|_| {
                        AppError::validation(format!("holder_ids 含非法雪花 ID: {x}"))
                    })
                })
                .collect::<Result<Vec<_>, _>>()?,
            _ => Vec::new(),
        };
        Ok((sort_by, sort_dir, customer_ids, statuses, locations, holder_ids))
    }

    /// ASSEMBLY-only 分支专用解析（不走 status 复合）。
    async fn parse_list_filters_for_assembly<R: PartRepoTrait>(
        query: &PartListQuery,
        repo: &mut R,
    ) -> Result<
        (
            String,
            String,
            Vec<i64>,
            Vec<String>,
            Vec<String>,
            Vec<i64>,
        ),
        AppError,
    > {
        let (sort_by, sort_dir, customer_ids, _statuses, _locations, _holder_ids) =
            Self::parse_list_filters(query, repo).await?;
        // assembly 域不消费 holder_ids / locations（t_assembly 没有 batch 派
        // 生字段）；status 单值透传，复合由上游 list_assemblies 自管（这里
        // 不传复合 statuses 数组）。
        Ok((
            sort_by,
            sort_dir,
            customer_ids,
            Vec::new(),
            Vec::new(),
            Vec::new(),
        ))
    }

    /// 一次性 GROUP BY 拿一组 assembly_id 的子件计数。
    /// 空 ids → 返回空 HashMap（不发起 SQL）。
    async fn fetch_child_counts(
        conn: &mut PgConnection,
        asm_ids: &[i64],
    ) -> Result<std::collections::HashMap<i64, i64>, AppError> {
        use std::collections::HashMap;
        let mut out: HashMap<i64, i64> = HashMap::new();
        if asm_ids.is_empty() {
            return Ok(out);
        }
        let rows: Vec<(i64, i64)> = sqlx::query_as(
            "SELECT assembly_id, COUNT(*) \
             FROM t_part WHERE assembly_id = ANY($1) AND deleted_at IS NULL \
             GROUP BY assembly_id",
        )
        .bind(asm_ids)
        .fetch_all(&mut *conn)
        .await?;
        for (aid, cnt) in rows {
            out.insert(aid, cnt);
        }
        Ok(out)
    }

    /// `TAssembly` → `PartListItem` 投影（ASSEMBLY-only / ALL 模式装配件段）。
    ///
    /// 字段映射：
    /// - 基础 8 列：`id` / `name` / `drawing_no` / `applicant_name` / `quantity` /
    ///   `request_date` / `planned_delivery_date` / `customer_id` 直接转
    /// - `serial_no`：t_assembly 上有 → 走
    /// - `assembly_id`：装配件行无父 → `None`（前端识别顶层装配件）
    /// - `status`：t_assembly.status 直接搬
    /// - `is_urgent`：直接搬
    /// - `order_no` / `system_delivery_date` / `note` / `version`：直接搬
    /// - `unit_price` / `total_price`：`TAssembly` 上是 `Option<Decimal>`（前端
    ///   装配录入可空），落到 `PartListItem` 用 `unwrap_or(Decimal::ZERO)` 兜底
    ///   —— 与 `t_part` NUMERIC NOT NULL DEFAULT 0 + frontend Zod schema
    ///   `unit_price: z.string()`（non-nullable）三方契约一致；plan §1.3
    ///   曾设想把 `PartListItem` 也改成 `Option<Decimal>`，经核实前端 Zod
    ///   同样是非空字符串，保持现状是最佳选择
    /// - `created_at` / `created_by` / `updated_at` / `updated_by` / `deleted_at` /
    ///   `process_chain_id`：直接搬（`process_chain_id` 在 t_assembly 不存在，
    ///   给 None）
    /// - `customer_name` / `l1_customer_name` / `location` / `holder_name`：由
    ///   caller 注入；本方法给 None 默认
    /// - `row_type` = `Some("ASSEMBLY")`
    /// - `has_children` / `child_count`：由 caller 按 fetch_child_counts 注入
    fn project_assembly_to_part_list_item(a: TAssembly) -> PartListItem {
        // 2026-09-28 修复：docstring 与实际类型声明一致。
        // - `TAssembly.unit_price` / `total_price`：`Option<Decimal>`（前端装配录入可空）
        // - `PartListItem.unit_price` / `total_price`：`Decimal`（non-nullable；NUMERIC(12,2) /
        //   NUMERIC(14,2) NOT NULL DEFAULT 0）
        // 此处用 `unwrap_or(Decimal::ZERO)` 兜底——与 `t_part` DB DEFAULT 0 语义对齐，
        // 也与 frontend Zod schema `unit_price: z.string()`（non-nullable）契约一致。
        //
        // 备注：本行类型合并 plan §1.3 曾设想改 `PartListItem` 为 `Option<Decimal>`，
        // 经核实前端 Zod 同样是 non-nullable string，保持现状是与两端契约一致的最佳选择。
        let unit_price: Option<Decimal> = a.unit_price;
        let total_price: Option<Decimal> = a.total_price;
        PartListItem {
            id: a.id,
            serial_no: a.serial_no,
            name: a.name,
            drawing_no: a.drawing_no,
            applicant_name: a.applicant_name.unwrap_or_default(),
            quantity: a.quantity,
            request_date: a.request_date,
            planned_delivery_date: a.planned_delivery_date,
            customer_id: a.customer_id,
            assembly_id: None,
            status: a.status,
            is_urgent: a.is_urgent,
            order_no: a.order_no,
            system_delivery_date: a.system_delivery_date,
            note: a.note,
            unit_price: unit_price.unwrap_or(Decimal::ZERO), // PartListItem.unit_price 非 Option；沿用 part 同款 DB 默认 0
            total_price: total_price.unwrap_or(Decimal::ZERO),
            version: a.version,
            created_at: a.created_at,
            created_by: a.created_by,
            updated_at: a.updated_at,
            updated_by: a.updated_by,
            deleted_at: a.deleted_at,
            process_chain_id: None, // t_assembly 不持 process_chain_id
            customer_name: None,
            l1_customer_name: None,
            location: None,
            holder_name: None,
            row_type: Some("ASSEMBLY".to_string()),
            has_children: false,           // 由 caller 用 child_count 覆盖
            child_count: None,             // 由 caller 用 fetch_child_counts 覆盖
        }
    }

    /// `GET /parts/inspection-batches` 列表：对齐 Python v1
    /// `PartService.list_inspection_batches`，返回 status=INSPECTION 全部活跃批次
    /// （含工单 / holder / process / delivery_note / customer 名称，一次 JOIN 解析，
    /// 服务层无 N+1）。
    ///
    /// 权限：Manager + Inspector（对齐 v1）。
    /// 限流：`limit ∈ [1, 200]`，默认 200；`offset` 默认 0。
    /// customer_id：单值 → `expand_customer_id` 展开为 L1+L2 ids（与 `list_parts` 同逻辑）。
    /// keyword / serial_no：service 层拼 `%...%` 加通配符；为防 SQL 注入风险，
    /// 拒绝 `%` / `_` / `\\` 等通配符特殊字符（含任一 → VALIDATION_ERROR 40001）。
    pub async fn list_inspection_batches<R: PartRepoTrait>(
        mut repo: R,
        query: &InspectionBatchListQuery,
        current: &CurrentUser,
    ) -> Result<InspectionBatchListOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Inspector])?;

        let limit = query.limit.unwrap_or(200).clamp(1, 200);
        let offset = query.offset.unwrap_or(0).max(0);

        // customer_id 展开：单值 → [L1, 所有 L2]；None → 不传（走全客户）
        let customer_ids_owned: Vec<i64>;
        let customer_ids: &[i64] = if let Some(cid) = query.customer_id {
            customer_ids_owned = expand_customer_id(repo.conn_mut(), cid).await?;
            &customer_ids_owned
        } else {
            &[]
        };

        // keyword：service 层拼 `%...%` + 拒绝 SQL 通配符特殊字符（% _ \）
        let keyword_owned: Option<String> = match query.keyword.as_deref() {
            Some(kw) => {
                if kw.contains(['%', '_', '\\']) {
                    return Err(AppError::validation("keyword 不能包含通配符 % _ \\"));
                }
                Some(format!("%{kw}%"))
            }
            None => None,
        };
        let keyword: Option<&str> = keyword_owned.as_deref();

        // serial_no：同上
        let serial_no_owned: Option<String> = match query.serial_no.as_deref() {
            Some(sn) => {
                if sn.contains(['%', '_', '\\']) {
                    return Err(AppError::validation("serial_no 不能包含通配符 % _ \\"));
                }
                Some(format!("%{sn}%"))
            }
            None => None,
        };
        let serial_no: Option<&str> = serial_no_owned.as_deref();

        let statuses: &[&str] = &["INSPECTION"];

        let rows = PartBatchRepo::list_batches_with_part(
            repo.conn_mut(),
            statuses,
            customer_ids,
            keyword,
            serial_no,
            query.planned_delivery_date_from,
            query.planned_delivery_date_to,
            limit,
            offset,
        )
        .await?;

        let total = PartBatchRepo::count_batches_with_part(
            repo.conn_mut(),
            statuses,
            customer_ids,
            keyword,
            serial_no,
            query.planned_delivery_date_from,
            query.planned_delivery_date_to,
        )
        .await?;

        Ok(InspectionBatchListOut {
            items: rows
                .into_iter()
                .map(InspectionBatchListItemOut::from)
                .collect(),
            total,
            limit,
            offset,
        })
    }

    pub async fn get_part<R: PartRepoTrait>(
        mut repo: R,
        part_id: i64,
        current: &CurrentUser,
    ) -> Result<PartDetailOut, AppError> {
        current.require_any_role(&[
            Role::Manager,
            Role::Clerk,
            Role::Inspector,
            Role::CncProgrammer,
        ])?;
        let part = repo.get_part_detail(part_id).await?.ok_or_else(|| {
            AppError::biz(
                code::BIZ_PART_NOT_FOUND,
                format!("part {part_id} 不存在或已删除"),
            )
        })?;
        let (cn, l1cn) = lookup_customer_names(repo.conn_mut(), part.customer_id).await?;
        let current_batch_id = repo.find_current_inspection_batch_id(part.id).await?;
        Ok(PartDetailOut::from_with_customer_extra(
            part,
            current_batch_id,
            cn,
            l1cn,
        ))
    }

    pub async fn get_part_by_serial<R: PartRepoTrait>(
        mut repo: R,
        serial_no: &str,
        current: &CurrentUser,
    ) -> Result<PartDetailOut, AppError> {
        current.require_any_role(&[
            Role::Manager,
            Role::Clerk,
            Role::Inspector,
            Role::CncProgrammer,
        ])?;
        let p = repo.get_by_serial(serial_no, false).await?.ok_or_else(|| {
            AppError::biz(
                code::BIZ_PART_NOT_FOUND,
                format!("serial_no {serial_no} 不存在"),
            )
        })?;
        Self::get_part(repo, p.id, current).await
    }

    /// 扫码快捷品检上下文：通过 serial 查工单窄字段 + 全部活跃批次（含 holder 名称）。
    ///
    /// 权限与 `get_part_by_serial` 一致（Manager / Clerk / Inspector / CncProgrammer）。
    /// 用于前端扫码弹窗，让用户直接看到批次（id + quantity + status + holder +
    /// version）并据此拼出 `POST /parts/{part_id}/to-ship` 的 `{ batch_id, version }`
    /// 入参。
    pub async fn get_part_batches_by_serial<R: PartRepoTrait>(
        mut repo: R,
        serial_no: &str,
        current: &CurrentUser,
    ) -> Result<PartScanContextOut, AppError> {
        current.require_any_role(&[
            Role::Manager,
            Role::Clerk,
            Role::Inspector,
            Role::CncProgrammer,
        ])?;

        // ① 查工单窄字段（仅 8 列 + id，避免读 28 列）
        let part = sqlx::query_as!(
            TPartScanRow,
            r#"
            SELECT id, drawing_no, name, quantity, customer_id,
                   system_delivery_date, is_urgent, order_no, note
            FROM t_part
            WHERE serial_no = $1 AND deleted_at IS NULL
            "#,
            serial_no,
        )
        .fetch_optional(repo.conn_mut())
        .await?
        .ok_or_else(|| {
            AppError::biz(
                code::BIZ_PART_NOT_FOUND,
                format!("serial_no {serial_no} 不存在"),
            )
        })?;

        // ② 查全部活跃批次（含 holder 名称）
        let batches =
            PartBatchRepo::list_active_by_part_id_with_holder(repo.conn_mut(), part.id).await?;

        // ③ 拼 DTO
        Ok(PartScanContextOut {
            part: PartScanInfoOut::from(part),
            batches: batches.into_iter().map(PartBatchScanOut::from).collect(),
        })
    }

    pub async fn update_part<R: PartRepoTrait>(
        mut repo: R,
        part_id: i64,
        req: &PartUpdateRequest,
        current: &CurrentUser,
    ) -> Result<PartDetailOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk])?;
        let n = repo
            .update_part(
                part_id,
                req.version,
                PartUpdate {
                    name: req.name.as_deref(),
                    drawing_no: req.drawing_no.as_deref(),
                    applicant_name: req.applicant_name.as_deref(),
                    quantity: req.quantity,
                    order_no: req.order_no.as_deref(),
                    system_delivery_date: req.system_delivery_date,
                    planned_delivery_date: req.planned_delivery_date,
                    note: req.note.as_deref(),
                    is_urgent: req.is_urgent,
                    unit_price: req.unit_price,
                    total_price: req.total_price,
                    updated_by: current.id,
                },
            )
            .await?;
        if n == 0 {
            return Err(AppError::biz(
                code::VERSION_CONFLICT,
                format!("part {part_id} 版本冲突或已删除"),
            ));
        }
        Self::get_part(repo, part_id, current).await
    }

    pub async fn soft_delete_part<R: PartRepoTrait>(
        mut repo: R,
        snowflake: &SnowflakeIdGenerator,
        part_id: i64,
        expected_version: i32,
        current: &CurrentUser,
    ) -> Result<(), AppError> {
        current.require_role(Role::Manager)?;
        // 2026-09-16 PR-2 瘦身（migration 027）：t_part.delivery_note_id 列已删；
        // 「已挂送货单禁删」守卫移出 PartRepo::soft_delete_part UPDATE，
        // 改在 service 层用 PartBatchRepo::has_active_batch_on_delivery_note
        // 预检（批次级真相源）。
        if repo.part_batch_has_active_on_delivery_note(part_id).await? {
            return Err(AppError::biz(
                code::BIZ_DELIVERY_NOTE_LOCKED_PART,
                format!("part {part_id} 存在活跃批次已挂送货单，禁 soft-delete"),
            ));
        }
        let n = repo
            .soft_delete_part(part_id, expected_version, current.id)
            .await?;
        match n {
            1 => {
                // 2026-09-16 FK 翻转（migration 026）级联：part 有工艺链时同事务
                // 软删链 + steps 并 unlink（顺序：steps → chain → unlink）。
                // unlink 必须清掉已软删 part 的 process_chain_id，让出
                // uq_t_part_process_chain 部分唯一索引槽位。
                let chain_id = repo
                    .get_by_id(part_id, true)
                    .await?
                    .and_then(|p| p.process_chain_id);
                if let Some(chain_id) = chain_id {
                    ProcessChainRepo::soft_delete_all_steps_for_chain(repo.conn_mut(), chain_id)
                        .await?;
                    ProcessChainRepo::soft_delete_chain(repo.conn_mut(), chain_id, current.id)
                        .await?;
                    ProcessChainRepo::unlink_part_from_chain(repo.conn_mut(), chain_id, current.id)
                        .await?;
                }
                repo.insert_part_event(NewPartEvent {
                    id: snowflake.next_id(),
                    part_id,
                    event_type: "SOFT_DELETED",
                    from_status: None,
                    to_status: None,
                    batch_id: None,
                    quantity: None,
                    drawing_code: None,
                    badge_code: None,
                    note: Some("manager soft-delete"),
                    created_by: Some(current.id),
                })
                .await?;
                Ok(())
            }
            _ => {
                // soft_delete SQL 0 行可能由 4 类原因触发，分支映射到不同错误码：
                // 1) part_id 不存在                  → 20101 BIZ_PART_NOT_FOUND (404)
                // 2) 已软删                          → 20101 BIZ_PART_NOT_FOUND (404, "已软删")
                // 3) version 不匹配                  → 40901 VERSION_CONFLICT (409)
                // 4) 终态 (DELIVERED/COMPLETED)       → 20119 BIZ_PART_NOT_DELETABLE (409)
                // 注：21420 BIZ_DELIVERY_NOTE_LOCKED_PART 已在上方预检拦截（service 层
                // 调用 PartBatchRepo::has_active_batch_on_delivery_note），不会进入
                // 此 match。
                let p = repo.get_by_id(part_id, true).await?;
                match p {
                    None => Err(AppError::biz(
                        code::BIZ_PART_NOT_FOUND,
                        format!("part {part_id} 不存在"),
                    )),
                    Some(p) if p.deleted_at.is_some() => Err(AppError::biz(
                        code::BIZ_PART_NOT_FOUND,
                        format!("part {part_id} 已软删"),
                    )),
                    Some(p) if p.version != expected_version => Err(AppError::biz(
                        code::VERSION_CONFLICT,
                        format!(
                            "part {part_id} 版本冲突（期望 {expected_version}，实际 {}）",
                            p.version
                        ),
                    )),
                    Some(p) if matches!(p.status.as_str(), "DELIVERED" | "COMPLETED") => {
                        Err(AppError::biz(
                            code::BIZ_PART_NOT_DELETABLE,
                            format!("part {part_id} 状态 {} 终态禁删", p.status),
                        ))
                    }
                    Some(p) => {
                        // 兜底：理论上 soft_delete SQL 已包含 `deleted_at IS NULL`
                        // 守卫，此分支不可达。映射成 50000 让上游看到错误模式。
                        Err(AppError::internal(format!(
                            "soft_delete_part 兜底：part {part_id} status={} 触发未识别条件",
                            p.status
                        )))
                    }
                }
            }
        }
    }

    /// 通用 part 文件上传（2026-09-11 重构）：
    /// DRAWING / 3D_MODEL 等 kind 共用同一段上传 + 落库逻辑。
    /// 调用方（`upload_drawing` / `upload_3d_model`）只负责决定 kind 和 file_type。
    #[allow(clippy::too_many_arguments)]
    pub async fn upload_part_file<R: PartRepoTrait>(
        mut repo: R,
        snowflake: &SnowflakeIdGenerator,
        state: &Arc<AppState>,
        part_id: i64,
        bytes: &[u8],
        original_filename: &str,
        content_type: &str,
        kind: &str,
        file_type: &str,
        current: &CurrentUser,
    ) -> Result<TPartFile, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk])?;
        if bytes.is_empty() {
            // 空字节不是「过大」而是「无效输入」：用 VALIDATION_ERROR (40001)
            // 而不是 BIZ_PART_FILE_TOO_LARGE (21103)
            return Err(AppError::validation(format!("{file_type} 字节为空")));
        }
        // 2026-09-11 修改：改用 CosConfig.max_file_size 配置（默认 300MB），
        // 不再写死 50MB。`.env` 改 COS_MAX_FILE_SIZE 即生效。
        let max = state.config.cos.max_file_size;
        if bytes.len() > max {
            return Err(AppError::biz(
                code::BIZ_PART_FILE_TOO_LARGE,
                format!("{file_type} > {max} bytes（{}MB）", max / 1024 / 1024),
            ));
        }
        // 2026-09-11 新增：kind → 扩展名 / content_type 白名单校验
        let ext = policy::ext_of(original_filename)
            .ok_or_else(|| AppError::biz(code::BIZ_PART_FILE_BAD_TYPE, "缺少扩展名"))?;
        let allowed = policy::allowed_exts(kind);
        if !allowed.contains(&ext.as_str()) {
            return Err(AppError::biz(
                code::BIZ_PART_FILE_BAD_TYPE,
                format!("扩展名 {ext} 不在 kind={kind} 白名单（{allowed:?}）"),
            ));
        }
        let ct_ok = policy::expected_content_types_for_ext(&ext);
        if !ct_ok.contains(&content_type) {
            return Err(AppError::biz(
                code::BIZ_PART_FILE_BAD_TYPE,
                format!("content_type {content_type} 与扩展名 {ext} 不一致"),
            ));
        }
        // 上传前 part 必须存在
        if repo.get_part_detail(part_id).await?.is_none() {
            return Err(AppError::biz(
                code::BIZ_PART_FILE_OWNER_NOT_FOUND,
                format!("part {part_id} 不存在"),
            ));
        }

        let sha = hash_bytes(bytes);
        let new_file_id = snowflake.next_id();
        // 2026-09-11 改为 Python 同款 CAS key：`{prefix}{kind}/{id}/{KIND}/{sha16}_{safe_name}`
        let real_key = crate::util::cos_key::build_cas_key(
            &state.config.cos.upload_prefix,
            "part",
            part_id,
            kind,
            &sha,
            original_filename,
        );
        state
            .cos
            .put_object(&real_key, bytes.to_vec(), content_type)
            .await
            .map_err(|e| {
                AppError::biz(
                    code::BIZ_PART_FILE_UPLOAD_FAILED,
                    format!("COS 上传失败: {e}"),
                )
            })?;
        PartFileRepo::create_part_file(
            repo.conn_mut(),
            NewPartFile {
                id: new_file_id,
                part_id,
                owner_kind: "PART",
                kind,
                file_type,
                object_key: &real_key,
                original_filename,
                file_size: bytes.len() as i64,
                content_type,
                upload_status: "READY",
                content_sha256: Some(&sha),
                created_by: current.id,
            },
        )
        .await
        .map_err(|e| {
            if let sqlx::Error::Database(db) = &e
                && db.code().as_deref() == Some("23505")
            {
                return AppError::biz(code::BIZ_PART_FILE_DUPLICATE, "相同文件已存在");
            }
            AppError::from(e)
        })?;
        let pf = PartFileRepo::get_by_part_kind(repo.conn_mut(), part_id, kind)
            .await?
            .ok_or_else(|| AppError::internal("刚 INSERT 的 file 查不到"))?;
        Ok(pf)
    }

    /// 上传 part 图纸 PDF（multipart 处理上传到 COS + INSERT t_part_file）。
    /// 2026-09-11 修改：改为对 `upload_part_file` 的薄包装。
    #[allow(clippy::too_many_arguments)]
    pub async fn upload_drawing<R: PartRepoTrait>(
        repo: R,
        snowflake: &SnowflakeIdGenerator,
        state: &Arc<AppState>,
        part_id: i64,
        bytes: &[u8],
        original_filename: &str,
        content_type: &str,
        current: &CurrentUser,
    ) -> Result<TPartFile, AppError> {
        Self::upload_part_file(
            repo,
            snowflake,
            state,
            part_id,
            bytes,
            original_filename,
            content_type,
            "DRAWING",
            "PDF",
            current,
        )
        .await
    }

    /// 上传 part 3D 模型（STEP / STP / IGES / IGS / STL / OBJ / 3MF）。
    /// 2026-09-11 新增：与 Python `POST /api/v1/parts/{id}/3d-models` 对齐。
    /// file_type 由扩展名推导（`policy::file_type_for_ext`）。
    #[allow(clippy::too_many_arguments)]
    pub async fn upload_3d_model<R: PartRepoTrait>(
        repo: R,
        snowflake: &SnowflakeIdGenerator,
        state: &Arc<AppState>,
        part_id: i64,
        bytes: &[u8],
        original_filename: &str,
        content_type: &str,
        current: &CurrentUser,
    ) -> Result<TPartFile, AppError> {
        let ext = policy::ext_of(original_filename)
            .ok_or_else(|| AppError::biz(code::BIZ_PART_FILE_BAD_TYPE, "缺少扩展名"))?;
        let file_type = policy::file_type_for_ext(&ext).ok_or_else(|| {
            AppError::biz(
                code::BIZ_PART_FILE_BAD_TYPE,
                format!("未知 3D 模型扩展名: {ext}"),
            )
        })?;
        Self::upload_part_file(
            repo,
            snowflake,
            state,
            part_id,
            bytes,
            original_filename,
            content_type,
            "3D_MODEL",
            file_type,
            current,
        )
        .await
    }
}

// ===== helpers =====

/// `create_part` 的 sqlx 错误码映射：唯一索引冲突（`23505`） → 业务语义
/// `BIZ_PART_NOT_FOUND`（serial_no 已被使用；可能是软删旧件占号导致
/// `uk_t_part_serial_no` 触发。当前 INSERT 路径 serial_no 写 NULL，partial
/// unique 不生效；此分支为预留，等 serial_no 变成可写时启用）。
///
/// `pub(super)`：暴露给 `lifecycle.rs`（如需要）。
pub(super) fn map_create_error(e: sqlx::Error) -> AppError {
    if let sqlx::Error::Database(db) = &e
        && db.code().as_deref() == Some("23505")
    {
        return AppError::biz(
            code::BIZ_PART_NOT_FOUND,
            "serial_no 已被使用（可能软删旧件占号）",
        );
    }
    AppError::from(e)
}

/// 展开 `customer_id` 为 `[id]`（含自身 + 子节点）。
///
/// 语义：
/// - L1 客户（无 parent_id）→ 自身 + 全部 L2 子节点 ids
/// - L2 客户（有 parent_id）→ 自身 + 同 L1 下所有兄弟 L2 ids
pub(super) async fn expand_customer_id(
    conn: &mut PgConnection,
    cid: i64,
) -> Result<Vec<i64>, AppError> {
    let row: Option<(i64, Option<i64>)> =
        sqlx::query_as("SELECT id, parent_id FROM t_customer WHERE id = $1 AND deleted_at IS NULL")
            .bind(cid)
            .fetch_optional(&mut *conn)
            .await?;
    let (_id, parent_id) = row.ok_or_else(|| {
        AppError::biz(
            code::BIZ_CUSTOMER_NOT_FOUND,
            format!("customer {cid} 不存在"),
        )
    })?;
    if let Some(p) = parent_id {
        let mut rows: Vec<i64> = sqlx::query_scalar(
            "SELECT id FROM t_customer WHERE parent_id = $1 AND deleted_at IS NULL",
        )
        .bind(p)
        .fetch_all(&mut *conn)
        .await?;
        if !rows.contains(&cid) {
            rows.push(cid);
        }
        Ok(rows)
    } else {
        sqlx::query_scalar(
            "SELECT id FROM t_customer WHERE (parent_id = $1 OR id = $1) AND deleted_at IS NULL",
        )
        .bind(cid)
        .fetch_all(&mut *conn)
        .await
        .map_err(Into::into)
    }
}

/// 取客户名 + L1 名（用于 `PartDetailOut` / `PartListItem` 冗余字段）。
///
/// 返回 `(Some(name), Some(l1_name))`：当自身为 L1 时 l1_name 与 name 同；
/// 当 customer 不存在 → `(None, None)`（service 层可容忍）。
pub(super) async fn lookup_customer_names(
    conn: &mut PgConnection,
    customer_id: i64,
) -> Result<(Option<String>, Option<String>), AppError> {
    let row: Option<(String, Option<i64>)> = sqlx::query_as(
        "SELECT name, parent_id FROM t_customer WHERE id = $1 AND deleted_at IS NULL",
    )
    .bind(customer_id)
    .fetch_optional(&mut *conn)
    .await?;
    let (name, parent_id) = match row {
        Some(r) => r,
        None => return Ok((None, None)),
    };
    let l1_name = match parent_id {
        Some(pid) => {
            sqlx::query_scalar::<_, String>(
                "SELECT name FROM t_customer WHERE id = $1 AND deleted_at IS NULL",
            )
            .bind(pid)
            .fetch_optional(&mut *conn)
            .await?
        }
        None => Some(name.clone()),
    };
    Ok((Some(name), l1_name))
}

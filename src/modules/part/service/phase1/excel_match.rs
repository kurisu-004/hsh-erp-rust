//! `POST /parts/match-by-excel-items`：采购订单 Excel 明细行 → 候选零件。
//!
//! 2026-10-06 新增（从 `events.rs` 迁出）。本文件承载「一次请求 → 一份候选表」
//! 的全部逻辑，分三段：
//!
//! 1. [`ExcelMatchIndex::ingest_parts`] / `ingest_assemblies` / `ingest_children`
//!    —— 纯内存分桶，无 IO。
//! 2. [`resolve_match_tier`] —— **分档决策纯函数**，只读索引、不碰 DB，
//!    可用 `MockPartRepoTrait` + 直接喂索引做单测（验收项「档位选择可单测」）。
//! 3. [`PartService::match_by_excel_items`] —— 编排：固定 4 条查询 + 逐行决策。
//!
//! ## 查询数硬约束：恒 ≤ 4 条，与请求行数无关
//!
//! | # | 查询 | 触发条件 | repo 方法 |
//! |---|---|---|---|
//! | 1 | `t_part` 图号命中 ∪ 名称命中 | 判据非空 | `list_match_parts_by_keys` |
//! | 2 | `t_assembly` 图号命中 ∪ 名称命中 | 判据非空 | `list_match_assemblies_by_keys` |
//! | 3 | 命中装配件的全部**有效子件** | 装配件命中非空 | `list_children_by_assemblies` |
//! | 4 | 候选零件所属装配件的名称映射 | 候选带 `assembly_id` | `list_assembly_names_by_ids` |
//!
//! 旧实现**逐行**发 SQL（每行 2 条），采购订单 500 行 = 1000 次数据库往返，
//! 不可接受。
//!
//! ## 分档规则（单行单档位，先命中先占）
//!
//! `PART_CODE > ASSEMBLY_CODE > PART_NAME > ASSEMBLY_NAME > NONE`。能按图号唯一定位
//! 就不该退到名称去冒险匹配（跨客户同名极容易误配）。
//!
//! **装配件本身绝不作为候选**：它不在 `t_part`，`batch-update-order-info` 打不到它；
//! 装配件命中时返回它的全部有效子件。装配件命中但 0 个有效子件时 `match_type` 仍报
//! 装配件档、`parts: []` 并追加 warning，**不静默降级到下一档**（降级会按名称误配）。

use std::collections::{HashMap, HashSet};

use crate::auth::rbac::{CurrentUser, Role};
use crate::modules::part::dto_crud::{MatchByExcelItem, MatchByExcelItemsRequest};
use crate::modules::part::model::TPart;
use crate::modules::part::repo::{AssemblyMatchRow, PartRepoTrait};
use crate::modules::part::vo::{ExcelMatchType, MatchByExcelItemResult, PartMatchInfoOut};
use crate::shared::error::AppError;

use super::super::PartService;

/// 2026-10-06 新增：单请求行数上限。
///
/// 采购订单 Excel 实际规模在数百行；2000 行足够覆盖异常大表，同时把「一次请求
/// 打爆连接池」的口子堵住。
pub const MATCH_MAX_ITEMS: usize = 2000;

/// 2026-10-06 新增：单行候选上限。
///
/// 名称兜底档跨客户同名极容易命中几十上百条，全量返回会让对话框不可用、也会把
/// 用户推向「闭眼全选」。超限时截断并在 `warnings` 里写明总数，请人工确认。
pub const MATCH_CANDIDATE_CAP: usize = 20;

/// 一行 Excel 的分档决策结果（[`resolve_match_tier`] 的输出）。
///
/// 只带 **part_id** 而非整行数据：整行留在 [`ExcelMatchIndex`] 里，决策函数
/// 因此不必克隆 `TPart`，单测断言 id 序列也最直观。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TierDecision {
    pub match_type: ExcelMatchType,
    /// 候选 part id（已按档位排序 + 去重 + 截断到 `MATCH_CANDIDATE_CAP`）。
    pub part_ids: Vec<i64>,
    /// 档位异常提示（无异常时为空）。
    pub warnings: Vec<String>,
}

impl TierDecision {
    fn none() -> Self {
        Self {
            match_type: ExcelMatchType::None,
            part_ids: Vec::new(),
            warnings: Vec::new(),
        }
    }
}

/// 匹配索引：整请求一次捞回的行，按判据分桶后供 [`resolve_match_tier`] 查询。
///
/// 2026-10-06 新增。桶的 key 是**去空白后**的判据值（与 [`normalize_key`] 同口径），
/// 故决策函数不需要再次 trim。
#[derive(Debug, Default)]
pub struct ExcelMatchIndex {
    /// 图号 → 命中零件（`id DESC`，最近建的排前；沿用旧实现口径）。
    parts_by_code: HashMap<String, Vec<usize>>,
    /// 名称 → 命中零件（`id ASC`）。
    parts_by_name: HashMap<String, Vec<usize>>,
    /// 图号 → 命中装配件（`id ASC`）。
    assemblies_by_code: HashMap<String, Vec<usize>>,
    /// 名称 → 命中装配件（`id ASC`）。
    assemblies_by_name: HashMap<String, Vec<usize>>,
    /// 装配件 id → 其全部有效子件（`id ASC`）。
    children_by_assembly: HashMap<i64, Vec<usize>>,
    /// 装配件 id → 名称（候选零件的所属装配件展示名）。
    assembly_names: HashMap<i64, String>,
    /// 第 1 条查询返回的零件行；各桶存的是它的下标（避免按判据重复克隆整行）。
    parts: Vec<TPart>,
    /// part id → 在 `parts` 里的下标（候选回查用）。
    part_idx_by_id: HashMap<i64, usize>,
    /// 第 2 条查询返回的装配件窄投影。
    assemblies: Vec<AssemblyMatchRow>,
}

impl ExcelMatchIndex {
    /// 分桶第 1 条查询的 `t_part` 行。
    ///
    /// 同一行会同时进「图号桶」与「名称桶」（两个判据都命中时），故两个桶各自
    /// 按档位要求的顺序排：图号桶 `id DESC`、名称桶 `id ASC`。SQL 侧统一
    /// `ORDER BY id ASC`，图号桶在这里做一次**稳定**逆序。
    fn ingest_parts(&mut self, rows: Vec<TPart>) {
        for row in rows {
            let idx = self.parts.len();
            self.part_idx_by_id.insert(row.id, idx);
            self.parts.push(row);
            let p = &self.parts[idx];
            let drawing_no = normalize_key(p.drawing_no.as_str());
            let name = normalize_key(p.name.as_str());
            // 图号档：`id DESC`（最近建的排前）
            let bucket = self.parts_by_code.entry(drawing_no).or_default();
            bucket.push(idx);
            bucket.sort_by(|a, b| self.parts[*b].id.cmp(&self.parts[*a].id));
            // 名称档：`id ASC`（桶内按插入序即可，插入序即 id ASC）
            let bucket = self.parts_by_name.entry(name).or_default();
            bucket.push(idx);
            bucket.sort_by_key(|i| self.parts[*i].id);
        }
    }

    /// 分桶第 2 条查询的 `t_assembly` 窄投影。
    fn ingest_assemblies(&mut self, rows: Vec<AssemblyMatchRow>) {
        for row in rows {
            let idx = self.assemblies.len();
            self.assemblies.push(row);
            let a = &self.assemblies[idx];
            let bucket = self
                .assemblies_by_code
                .entry(normalize_key(a.drawing_no.as_str()))
                .or_default();
            bucket.push(idx);
            bucket.sort_by_key(|i| self.assemblies[*i].id);
            let bucket = self
                .assemblies_by_name
                .entry(normalize_key(a.name.as_str()))
                .or_default();
            bucket.push(idx);
            bucket.sort_by_key(|i| self.assemblies[*i].id);
        }
    }

    /// 分桶第 3 条查询的「装配件 → 有效子件」映射。
    fn ingest_children(&mut self, rows: Vec<TPart>) {
        for row in rows {
            let Some(assembly_id) = row.assembly_id else {
                continue;
            };
            let idx = self.parts.len();
            self.part_idx_by_id.insert(row.id, idx);
            self.parts.push(row);
            self.children_by_assembly
                .entry(assembly_id)
                .or_default()
                .push(idx);
        }
    }

    /// 候选 part id → 该 part 的行（组装响应 + 收集装配件名时用）。
    ///
    /// 走 `part_idx_by_id` 哈希而不是线性扫 `parts`：候选总数可达
    /// `行数 × 20`（2000 行 = 4 万），线性扫会退化成 O(行数² × 20)。
    fn part_by_id(&self, part_id: i64) -> Option<&TPart> {
        self.part_idx_by_id
            .get(&part_id)
            .and_then(|i| self.parts.get(*i))
    }
}

/// 判据规范化：去首尾空白；空白/缺省 ⇒ `None`（该判据视为不存在）。
fn normalize_key(raw: &str) -> String {
    raw.trim().to_string()
}

/// `Option<String>` 判据 → 规范化键（`None` / 空串 / 全空白 ⇒ `None`）。
fn normalize_opt_key(raw: Option<&str>) -> Option<String> {
    let t = raw?.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

/// 2026-10-06 新增：收集整请求的 distinct 判据（图号 / 名称各一份，**保序去重**）。
///
/// 保序是为了让同一份请求的 SQL 绑定参数稳定，便于复现；用 `HashSet` 去重。
fn collect_criteria(items: &[MatchByExcelItem]) -> (Vec<String>, Vec<String>) {
    let mut drawing_seen: HashSet<String> = HashSet::new();
    let mut name_seen: HashSet<String> = HashSet::new();
    let mut drawing_nos: Vec<String> = Vec::new();
    let mut names: Vec<String> = Vec::new();
    for it in items {
        if let Some(k) = normalize_opt_key(it.drawing_no.as_deref())
            && drawing_seen.insert(k.clone())
        {
            drawing_nos.push(k);
        }
        if let Some(k) = normalize_opt_key(it.name.as_deref())
            && name_seen.insert(k.clone())
        {
            names.push(k);
        }
    }
    (drawing_nos, names)
}

/// **分档决策纯函数**：一行 Excel + 索引 ⇒ 档位 / 候选 / 警告。零 IO。
///
/// 2026-10-06 新增（验收项：档位选择必须可单测）。优先级
/// `PART_CODE > ASSEMBLY_CODE > PART_NAME > ASSEMBLY_NAME > NONE`，取**第一个**
/// 产出 ≥1 候选的档位，该档的**全部**命中都作为候选。
///
/// 判据为空串 / 缺省时直接跳档（`drawing_no` / `name` 都缺 ⇒ `NONE`）。
pub fn resolve_match_tier(item: &MatchByExcelItem, index: &ExcelMatchIndex) -> TierDecision {
    let code = normalize_opt_key(item.drawing_no.as_deref());
    let name = normalize_opt_key(item.name.as_deref());

    if let Some(c) = code.as_deref()
        && let Some(hits) = index.parts_by_code.get(c)
        && !hits.is_empty()
    {
        return decision_from_parts(ExcelMatchType::PartCode, hits, index);
    }
    if let Some(c) = code.as_deref()
        && let Some(hits) = index.assemblies_by_code.get(c)
        && !hits.is_empty()
    {
        return decision_from_assemblies(ExcelMatchType::AssemblyCode, hits, index);
    }
    if let Some(n) = name.as_deref()
        && let Some(hits) = index.parts_by_name.get(n)
        && !hits.is_empty()
    {
        return decision_from_parts(ExcelMatchType::PartName, hits, index);
    }
    if let Some(n) = name.as_deref()
        && let Some(hits) = index.assemblies_by_name.get(n)
        && !hits.is_empty()
    {
        return decision_from_assemblies(ExcelMatchType::AssemblyName, hits, index);
    }
    TierDecision::none()
}

/// 零件档（`PART_CODE` / `PART_NAME`）的候选组装 + cap 截断。
fn decision_from_parts(
    match_type: ExcelMatchType,
    hits: &[usize],
    index: &ExcelMatchIndex,
) -> TierDecision {
    let ids = collect_part_ids(hits, index);
    let (part_ids, warnings) = cap_candidates(match_type, ids);
    TierDecision {
        match_type,
        part_ids,
        warnings,
    }
}

/// 装配件档（`ASSEMBLY_CODE` / `ASSEMBLY_NAME`）的候选组装。
///
/// 候选 = **被命中装配件的全部有效子件**（装配件本身绝不入候选）。
/// 命中但 0 个有效子件时仍返回该档 + 空候选 + 中文 warning（不降级下一档）。
fn decision_from_assemblies(
    match_type: ExcelMatchType,
    hits: &[usize],
    index: &ExcelMatchIndex,
) -> TierDecision {
    let mut ids: Vec<i64> = Vec::new();
    let mut seen: HashSet<i64> = HashSet::new();
    let mut warnings: Vec<String> = Vec::new();
    for a_idx in hits {
        let Some(a) = index.assemblies.get(*a_idx) else {
            continue;
        };
        let children = index.children_by_assembly.get(&a.id);
        match children {
            Some(child_idxs) if !child_idxs.is_empty() => {
                for c_idx in child_idxs {
                    if let Some(c) = index.parts.get(*c_idx)
                        && seen.insert(c.id)
                    {
                        ids.push(c.id);
                    }
                }
            }
            _ => warnings.push(format!(
                "装配件 {}（图号 {}）无有效子件，无需回填",
                a.name, a.drawing_no
            )),
        }
    }
    let (part_ids, cap_warnings) = cap_candidates(match_type, ids);
    warnings.extend(cap_warnings);
    TierDecision {
        match_type,
        part_ids,
        warnings,
    }
}

/// 命中下标列表 → 去重后的 part id 列表（保持传入顺序）。
fn collect_part_ids(hits: &[usize], index: &ExcelMatchIndex) -> Vec<i64> {
    let mut seen: HashSet<i64> = HashSet::new();
    let mut ids = Vec::with_capacity(hits.len());
    for idx in hits {
        if let Some(p) = index.parts.get(*idx)
            && seen.insert(p.id)
        {
            ids.push(p.id);
        }
    }
    ids
}

/// 候选 cap：超 `MATCH_CANDIDATE_CAP` 则截断 + 追加中文说明。
///
/// warning 文本必须点明判据（`按名称匹配` vs `按图号匹配`）：名称档是**兜底**匹配，
/// 用户看到 20 条候选时要知道「这是按名称撞出来的、可能撞错人」。
fn cap_candidates(match_type: ExcelMatchType, mut ids: Vec<i64>) -> (Vec<i64>, Vec<String>) {
    if ids.len() <= MATCH_CANDIDATE_CAP {
        return (ids, Vec::new());
    }
    let total = ids.len();
    let warnings = vec![format!(
        "{}命中 {total} 条，已截断至前 {MATCH_CANDIDATE_CAP} 条，请人工确认",
        match_type.label_zh()
    )];
    ids.truncate(MATCH_CANDIDATE_CAP);
    (ids, warnings)
}

impl PartService {
    /// `POST /parts/match-by-excel-items`：采购订单 Excel 明细行 → 候选零件。
    ///
    /// 2026-10-06 重做。响应数组**恒等于**请求 `items` 长度、顺序一致（前端按
    /// `row_no` 关联，顺序仅供人读）；未匹配的行也必须出现（`match_type: NONE`
    /// + `parts: []`），不得省略 —— 省略会让前端按 `row_no` 建 Map 时丢行。
    ///
    /// 查询数恒 ≤ 4 条（与 `items.len()` 无关），逐条对应本函数内的注释编号。
    pub async fn match_by_excel_items<R: PartRepoTrait>(
        mut repo: R,
        req: &MatchByExcelItemsRequest,
        current: &CurrentUser,
    ) -> Result<Vec<MatchByExcelItemResult>, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk])?;
        if req.items.is_empty() {
            return Err(AppError::validation("items 不能为空"));
        }
        if req.items.len() > MATCH_MAX_ITEMS {
            return Err(AppError::validation(format!(
                "items 最多 {MATCH_MAX_ITEMS} 行，当前 {} 行",
                req.items.len()
            )));
        }

        let mut index = ExcelMatchIndex::default();
        let (drawing_nos, names) = collect_criteria(&req.items);
        let drawing_refs: Vec<&str> = drawing_nos.iter().map(String::as_str).collect();
        let name_refs: Vec<&str> = names.iter().map(String::as_str).collect();

        // ① t_part：`drawing_no = ANY($1) OR name = ANY($2)`（软删闸门在 SQL 内）
        index.ingest_parts(
            repo.list_match_parts_by_keys(&drawing_refs, &name_refs)
                .await?,
        );

        // ② t_assembly：同一判据查装配件（装配件命中后取其子件作候选）
        index.ingest_assemblies(
            repo.list_match_assemblies_by_keys(&drawing_refs, &name_refs)
                .await?,
        );

        // ③ 命中装配件的全部有效子件（一条；装配件为空则 repo 侧短路不发 SQL）
        let mut assembly_ids: Vec<i64> = index
            .assemblies
            .iter()
            .map(|a| a.id)
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        assembly_ids.sort_unstable();
        index.ingest_children(
            repo.list_children_by_assemblies(&assembly_ids, false)
                .await?,
        );

        // 分档决策（纯函数，逐行）
        let decisions: Vec<TierDecision> = req
            .items
            .iter()
            .map(|it| resolve_match_tier(it, &index))
            .collect();

        // ④ 候选零件所属装配件的名称映射（一条）
        let mut name_ids: Vec<i64> = decisions
            .iter()
            .flat_map(|d| d.part_ids.iter())
            .filter_map(|pid| index.part_by_id(*pid).and_then(|p| p.assembly_id))
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        name_ids.sort_unstable();
        for row in repo.list_assembly_names_by_ids(&name_ids).await? {
            index.assembly_names.insert(row.id, row.name);
        }

        Ok(req
            .items
            .iter()
            .zip(decisions)
            .map(|(item, d)| build_result(item, &d, &index))
            .collect())
    }
}

/// 分档决策 + 索引 → 响应行（纯函数）。
fn build_result(
    item: &MatchByExcelItem,
    decision: &TierDecision,
    index: &ExcelMatchIndex,
) -> MatchByExcelItemResult {
    let parts = decision
        .part_ids
        .iter()
        .filter_map(|pid| index.part_by_id(*pid))
        .map(|p| PartMatchInfoOut {
            part_id: p.id,
            version: p.version,
            drawing_no: p.drawing_no.clone(),
            name: p.name.clone(),
            order_no: p.order_no.clone(),
            system_delivery_date: p.system_delivery_date,
            assembly_id: p.assembly_id,
            assembly_name: p
                .assembly_id
                .and_then(|aid| index.assembly_names.get(&aid).cloned()),
        })
        .collect();
    MatchByExcelItemResult {
        row_no: item.row_no,
        match_type: decision.match_type,
        parts,
        warnings: decision.warnings.clone(),
    }
}

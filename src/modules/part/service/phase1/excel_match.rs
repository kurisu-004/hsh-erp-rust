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

#[cfg(test)]
mod tests {
    //! 分档决策纯函数单测（2026-10-06 新增）。
    //!
    //! 覆盖 4 组契约（`resolve_match_tier` 是零 IO 纯函数，直接喂索引即可）：
    //! 1. **五档全组合 + 优先级** —— `PART_CODE > ASSEMBLY_CODE > PART_NAME >
    //!    ASSEMBLY_NAME > NONE`。重点是**跨档位**的抢占：同一行同时命中「零件图号」
    //!    与「装配件名称」必须落 `PART_CODE`，同时命中「装配件图号」与「零件名称」
    //!    必须落 `ASSEMBLY_CODE`，同一个 `name` 同时是零件名和装配件名必须落
    //!    `PART_NAME`。单档位命中不构成优先级证明。
    //! 2. **装配件 0 子件不降级** —— 仍报装配件档 + 空候选 + 中文 warning。
    //!    降级会按名称误配，故这是硬行为。
    //! 3. **判据缺省 / 空串 / 全空白** —— 视为该判据不存在（跳档）。
    //! 4. **候选 cap 20** —— 超限截断 + warning 文案含总数与判据名；恰好 20 不截断。
    //!
    //! 组装响应（`build_result`：`part_id` 序列化成 string、`assembly_name` 回填、
    //! 长度守恒）与「整请求 ≤ 4 条查询」的编排由 `MockPartRepoTrait` 用例守；
    //! 端到端（真库 SQL / 软删闸门 / 三态写库）由集成测试
    //! `tests/part/purchase_order_import.rs` 守。
    //!
    //! mockall strict mode：被调用的方法必须 `.expect_*()`，没调用的不必 expect。

    use mockall::predicate::*;

    use super::*;
    use crate::auth::rbac::CurrentUser;
    use crate::modules::part::repo::MockPartRepoTrait;

    // ===== 行构造 helper =====

    /// 造一行 `TPart`（只填匹配链路读到的列，其余给中性值）。
    fn sample_part(id: i64, drawing_no: &str, name: &str, assembly_id: Option<i64>) -> TPart {
        TPart {
            id,
            serial_no: None,
            name: name.to_string(),
            drawing_no: drawing_no.to_string(),
            applicant_name: "甲".to_string(),
            quantity: 1,
            request_date: chrono::NaiveDate::from_ymd_opt(2026, 10, 6).unwrap(),
            planned_delivery_date: chrono::NaiveDate::from_ymd_opt(2026, 10, 6).unwrap(),
            customer_id: 900_000_000_000_000_010,
            assembly_id,
            status: "PENDING".to_string(),
            is_urgent: false,
            next_process_id: None,
            order_no: None,
            system_delivery_date: None,
            note: None,
            unit_price: rust_decimal::Decimal::ZERO,
            total_price: rust_decimal::Decimal::ZERO,
            version: 3,
            created_at: chrono::NaiveDateTime::UNIX_EPOCH,
            created_by: Some(1),
            updated_at: chrono::NaiveDateTime::UNIX_EPOCH,
            updated_by: Some(1),
            deleted_at: None,
            process_chain_id: None,
        }
    }

    /// 造一行 `t_assembly` 窄投影。
    fn sample_assembly(id: i64, drawing_no: &str, name: &str) -> AssemblyMatchRow {
        AssemblyMatchRow {
            id,
            drawing_no: drawing_no.to_string(),
            name: name.to_string(),
        }
    }

    /// 造一行 Excel（`row_no` 固定 1，本组断言只看档位与候选）。
    fn item(drawing_no: Option<&str>, name: Option<&str>) -> MatchByExcelItem {
        MatchByExcelItem {
            row_no: 1,
            line_no: None,
            drawing_no: drawing_no.map(str::to_string),
            name: name.map(str::to_string),
        }
    }

    /// **四档齐全**的索引：同时存在零件图号 / 装配件图号 / 零件名 / 装配件名
    /// 四个判据的命中，用来验证跨档位抢占。
    ///
    /// 布局（id 段刻意不连续，见下方各用例注释）：
    /// - 零件：`id=1 / drawing_no=CODE-A / name=零件甲`（图号+名称双命中）
    /// - 零件：`id=2 / drawing_no=D-2 / name=零件乙`
    /// - 零件：`id=3 / drawing_no=DUP-NAME / name=同名件`（与装配件同名 ⇒ 验 PART_NAME 优先）
    /// - 装配件：`id=501 / drawing_no=ASM-CODE / name=装配件甲`，子件 `id=201 / 202`
    /// - 装配件：`id=502 / drawing_no=D-5 / name=装配件乙`，子件 `id=203`
    /// - 装配件：`id=503 / drawing_no=EMPTY-CODE / name=空件名`，**0 子件**
    fn four_tier_index() -> ExcelMatchIndex {
        let mut idx = ExcelMatchIndex::default();
        idx.ingest_parts(vec![
            sample_part(1, "CODE-A", "零件甲", None),
            sample_part(2, "D-2", "零件乙", None),
            sample_part(3, "DUP-NAME", "同名件", None),
        ]);
        idx.ingest_assemblies(vec![
            sample_assembly(501, "ASM-CODE", "装配件甲"),
            sample_assembly(502, "D-5", "装配件乙"),
            sample_assembly(503, "EMPTY-CODE", "空件名"),
        ]);
        idx.ingest_children(vec![
            sample_part(201, "C-1", "子件一", Some(501)),
            sample_part(202, "C-2", "子件二", Some(501)),
            sample_part(203, "C-3", "子件三", Some(502)),
        ]);
        idx
    }

    // ===== 组 1：五档全组合 + 优先级 =====

    #[test]
    fn tier_part_code_wins_when_only_code_matches() {
        let idx = four_tier_index();
        let d = resolve_match_tier(&item(Some("CODE-A"), Some("绝不命中的名称")), &idx);
        assert_eq!(d.match_type, ExcelMatchType::PartCode);
        assert_eq!(d.part_ids, vec![1]);
        assert!(d.warnings.is_empty(), "单候选不应有 warning: {d:?}");
    }

    #[test]
    fn tier_part_code_outranks_assembly_name() {
        // 行内 drawing_no 命中零件、name 命中装配件 ⇒ 必须 PART_CODE（跨档位抢占）。
        let idx = four_tier_index();
        let d = resolve_match_tier(&item(Some("CODE-A"), Some("装配件乙")), &idx);
        assert_eq!(
            d.match_type,
            ExcelMatchType::PartCode,
            "零件图号命中必须压过装配件名称命中: {d:?}"
        );
        assert_eq!(d.part_ids, vec![1]);
    }

    #[test]
    fn tier_assembly_code_outranks_part_name() {
        // 图号命中装配件、名称命中零件 ⇒ 必须 ASSEMBLY_CODE。
        let idx = four_tier_index();
        let d = resolve_match_tier(&item(Some("ASM-CODE"), Some("零件乙")), &idx);
        assert_eq!(
            d.match_type,
            ExcelMatchType::AssemblyCode,
            "装配件图号命中必须压过零件名称命中: {d:?}"
        );
        assert_eq!(d.part_ids, vec![201, 202], "候选应是该装配件的全部子件");
    }

    #[test]
    fn tier_part_name_outranks_assembly_name() {
        // 同名：零件 id=3 与装配件 id=502 的 name 都是…… 这里让「零件名」与
        // 「装配件名」字面相同才能构成冲突，故用 name 同时等于两者的构造：
        //   零件 id=3 name=同名件；另造装配件 id=504 name=同名件。
        let mut idx = four_tier_index();
        idx.ingest_assemblies(vec![sample_assembly(504, "D-7", "同名件")]);
        idx.ingest_children(vec![sample_part(204, "C-4", "子件四", Some(504))]);
        let d = resolve_match_tier(&item(Some("NO-SUCH-CODE"), Some("同名件")), &idx);
        assert_eq!(
            d.match_type,
            ExcelMatchType::PartName,
            "零件名命中必须压过装配件名命中: {d:?}"
        );
        assert_eq!(d.part_ids, vec![3]);
    }

    #[test]
    fn tier_assembly_name_returns_children() {
        let idx = four_tier_index();
        let d = resolve_match_tier(&item(Some("NO-SUCH-CODE"), Some("装配件乙")), &idx);
        assert_eq!(d.match_type, ExcelMatchType::AssemblyName);
        assert_eq!(d.part_ids, vec![203], "候选应是子件而非装配件本身");
        assert!(
            !d.part_ids.contains(&502),
            "装配件本身绝不入候选（batch-update 打不到它）"
        );
    }

    #[test]
    fn tier_none_when_all_criteria_miss() {
        let idx = four_tier_index();
        let d = resolve_match_tier(&item(Some("NO-SUCH"), Some("也不存在")), &idx);
        assert_eq!(d.match_type, ExcelMatchType::None);
        assert!(d.part_ids.is_empty(), "NONE 档不得有候选: {d:?}");
        assert!(
            d.warnings.is_empty(),
            "「没匹配上」不是异常，不该 warning: {d:?}"
        );
    }

    #[test]
    fn tier_part_code_orders_by_id_desc() {
        // 同一图号 3 行 ⇒ 候选按 id DESC（最近建的排前，沿用旧实现口径）。
        let mut idx = ExcelMatchIndex::default();
        idx.ingest_parts(vec![
            sample_part(10, "SAME", "同名", None),
            sample_part(30, "SAME", "同名", None),
            sample_part(20, "SAME", "同名", None),
        ]);
        let d = resolve_match_tier(&item(Some("SAME"), None), &idx);
        assert_eq!(d.match_type, ExcelMatchType::PartCode);
        assert_eq!(d.part_ids, vec![30, 20, 10], "图号档应 id DESC");
    }

    #[test]
    fn tier_part_name_orders_by_id_asc() {
        // 名称档排序口径与图号档相反（id ASC）。
        let mut idx = ExcelMatchIndex::default();
        idx.ingest_parts(vec![
            sample_part(10, "D-10", "同名件", None),
            sample_part(30, "D-30", "同名件", None),
            sample_part(20, "D-20", "同名件", None),
        ]);
        let d = resolve_match_tier(&item(None, Some("同名件")), &idx);
        assert_eq!(d.match_type, ExcelMatchType::PartName);
        assert_eq!(d.part_ids, vec![10, 20, 30], "名称档应 id ASC");
    }

    // ===== 组 2：装配件 0 子件不降级 =====

    #[test]
    fn tier_assembly_code_without_children_keeps_tier_and_warns() {
        let mut idx = four_tier_index();
        // 让 name 也能命中一个零件名：若实现降级到下一档，这里会错报 PART_NAME。
        idx.ingest_parts(vec![sample_part(9, "D-9", "空件名", None)]);
        let d = resolve_match_tier(&item(Some("EMPTY-CODE"), Some("空件名")), &idx);
        assert_eq!(
            d.match_type,
            ExcelMatchType::AssemblyCode,
            "装配件命中但 0 子件时不得降级到 PART_NAME: {d:?}"
        );
        assert!(d.part_ids.is_empty(), "0 子件 ⇒ 空候选: {d:?}");
        assert_eq!(d.warnings.len(), 1, "应有 1 条 warning: {d:?}");
        let w = &d.warnings[0];
        assert!(
            w.contains("空件名") && w.contains("EMPTY-CODE") && w.contains("无有效子件"),
            "warning 应点明装配件名 / 图号 / 无需回填，实际 {w:?}"
        );
    }

    #[test]
    fn tier_assembly_name_without_children_keeps_tier_and_warns() {
        // 装配件 id=503（name=空件名）在 `four_tier_index` 里 0 子件。
        // ASSEMBLY_NAME 是最后一档，「不降级」在这里表现为：**不得**报成 NONE ——
        // 报 NONE 等于告诉用户「这张 Excel 行没东西可填」，而真实情况是「装配体里
        // 没有有效子件、需要先补子件」，两者的处置动作完全不同。
        let idx = four_tier_index();
        let d = resolve_match_tier(&item(None, Some("空件名")), &idx);
        assert_eq!(
            d.match_type,
            ExcelMatchType::AssemblyName,
            "0 子件仍应报装配件名档: {d:?}"
        );
        assert!(d.part_ids.is_empty(), "0 子件 ⇒ 空候选: {d:?}");
        assert_eq!(d.warnings.len(), 1, "{d:?}");
        assert!(
            d.warnings[0].contains("无有效子件"),
            "warning 应说明无有效子件: {d:?}"
        );
    }

    #[test]
    fn tier_assembly_mixed_children_and_empty_reports_both() {
        // 两个装配件同图号命中：一个有子件、一个没有 ⇒ 候选取有子件那个，
        // 同时保留「无有效子件」warning。
        let mut idx = ExcelMatchIndex::default();
        idx.ingest_assemblies(vec![
            sample_assembly(501, "MULTI", "多子件装配"),
            sample_assembly(502, "MULTI", "空子件装配"),
        ]);
        idx.ingest_children(vec![sample_part(201, "C-1", "子件一", Some(501))]);
        let d = resolve_match_tier(&item(Some("MULTI"), None), &idx);
        assert_eq!(d.match_type, ExcelMatchType::AssemblyCode);
        assert_eq!(d.part_ids, vec![201]);
        assert_eq!(
            d.warnings.len(),
            1,
            "只该为空子件那个装配件报 warning: {d:?}"
        );
        assert!(d.warnings[0].contains("空子件装配"), "实际 {d:?}");
    }

    // ===== 组 3：判据缺省 / 空串 / 全空白 =====

    #[test]
    fn tier_blank_criteria_are_treated_as_absent() {
        let idx = four_tier_index();

        // 双判据都缺省
        let d = resolve_match_tier(&item(None, None), &idx);
        assert_eq!(d.match_type, ExcelMatchType::None, "{d:?}");

        // 双判据都是空串
        let d = resolve_match_tier(&item(Some(""), Some("")), &idx);
        assert_eq!(
            d.match_type,
            ExcelMatchType::None,
            "空串应视为不存在: {d:?}"
        );

        // drawing_no 全空白 → 该判据不存在，只剩 name 判据
        let d = resolve_match_tier(&item(Some("   "), Some("零件乙")), &idx);
        assert_eq!(
            d.match_type,
            ExcelMatchType::PartName,
            "全空白 drawing_no 应跳档: {d:?}"
        );
        assert_eq!(d.part_ids, vec![2]);

        // name 全空白 → 只剩 drawing_no 判据
        let d = resolve_match_tier(&item(Some("CODE-A"), Some("  ")), &idx);
        assert_eq!(
            d.match_type,
            ExcelMatchType::PartCode,
            "全空白 name 应跳档: {d:?}"
        );

        // 空串 + 装配件名 ⇒ 落到 ASSEMBLY_NAME（而不是 NONE）
        let d = resolve_match_tier(&item(Some(""), Some("装配件乙")), &idx);
        assert_eq!(d.match_type, ExcelMatchType::AssemblyName, "{d:?}");
    }

    #[test]
    fn tier_criteria_are_trimmed_before_lookup() {
        let idx = four_tier_index();
        let d = resolve_match_tier(&item(Some("  CODE-A  "), Some("\t零件乙\n")), &idx);
        assert_eq!(
            d.match_type,
            ExcelMatchType::PartCode,
            "判据两端空白应被 trim 后匹配（Excel 单元格常带空格）: {d:?}"
        );
        assert_eq!(d.part_ids, vec![1]);
    }

    // ===== 组 4：候选 cap 20 =====

    #[test]
    fn tier_cap_truncates_part_name_hits_and_warns() {
        let mut idx = ExcelMatchIndex::default();
        let rows: Vec<TPart> = (1..=25)
            .map(|i| sample_part(i, &format!("D-{i}"), "爆款件", None))
            .collect();
        idx.ingest_parts(rows);
        let d = resolve_match_tier(&item(None, Some("爆款件")), &idx);
        assert_eq!(d.match_type, ExcelMatchType::PartName);
        assert_eq!(
            d.part_ids.len(),
            MATCH_CANDIDATE_CAP,
            "25 条命中应截断到 cap=20"
        );
        assert_eq!(d.part_ids[0], 1, "名称档截断保留 id ASC 的前 20 条");
        assert_eq!(d.part_ids[19], 20);
        assert_eq!(d.warnings.len(), 1, "截断应有 1 条 warning: {d:?}");
        let w = &d.warnings[0];
        assert!(
            w.contains("按名称匹配") && w.contains("25") && w.contains("20"),
            "warning 必须写明判据（名称是兜底匹配）+ 原总数 + 截断数，实际 {w:?}"
        );
    }

    #[test]
    fn tier_cap_does_not_warn_at_exactly_cap() {
        let mut idx = ExcelMatchIndex::default();
        let rows: Vec<TPart> = (1..=MATCH_CANDIDATE_CAP as i64)
            .map(|i| sample_part(i, &format!("D-{i}"), "刚好二十件", None))
            .collect();
        idx.ingest_parts(rows);
        let d = resolve_match_tier(&item(None, Some("刚好二十件")), &idx);
        assert_eq!(d.part_ids.len(), MATCH_CANDIDATE_CAP);
        assert!(d.warnings.is_empty(), "恰好等于 cap 不该报截断: {d:?}");
    }

    #[test]
    fn tier_cap_truncates_assembly_children_and_keeps_empty_warning() {
        let mut idx = ExcelMatchIndex::default();
        idx.ingest_assemblies(vec![
            sample_assembly(501, "BIG", "大装配"),
            sample_assembly(502, "BIG", "空装配"),
        ]);
        let rows: Vec<TPart> = (1..=23)
            .map(|i| sample_part(i, &format!("C-{i}"), "子件", Some(501)))
            .collect();
        idx.ingest_children(rows);
        let d = resolve_match_tier(&item(Some("BIG"), None), &idx);
        assert_eq!(d.match_type, ExcelMatchType::AssemblyCode);
        assert_eq!(d.part_ids.len(), MATCH_CANDIDATE_CAP, "子件也应受 cap 约束");
        assert_eq!(
            d.warnings.len(),
            2,
            "应有「截断」+「空装配」两条 warning: {d:?}"
        );
        assert!(d.warnings.iter().any(|w| w.contains("已截断")), "{d:?}");
        assert!(d.warnings.iter().any(|w| w.contains("无有效子件")), "{d:?}");
    }

    #[test]
    fn tier_dedupes_same_part_reaching_index_twice() {
        // 同一 part 既被判据 ① 捞回（图号命中）、又作为装配件子件被 ③ 捞回：
        // 两档各自去重，候选不得出现重复 id。
        let mut idx = ExcelMatchIndex::default();
        idx.ingest_parts(vec![sample_part(201, "C-1", "子件一", Some(501))]);
        idx.ingest_assemblies(vec![sample_assembly(501, "ASM", "装配")]);
        idx.ingest_children(vec![sample_part(201, "C-1", "子件一", Some(501))]);
        let d = resolve_match_tier(&item(Some("ASM"), None), &idx);
        assert_eq!(d.match_type, ExcelMatchType::AssemblyCode);
        assert_eq!(d.part_ids, vec![201]);
        let mut uniq = d.part_ids.clone();
        uniq.sort_unstable();
        uniq.dedup();
        assert_eq!(uniq.len(), d.part_ids.len(), "候选 id 必须唯一: {d:?}");
    }

    // ===== 组 5（mockall）：编排 + 线格式 =====

    /// MANAGER 测试用户（id=1）。
    fn current_manager() -> CurrentUser {
        CurrentUser {
            id: 1,
            username: "test-mgr".to_string(),
            roles: vec![Role::Manager],
            shelf_ids: vec![],
            shelf_wildcard: false,
        }
    }

    /// INSPECTOR 测试用户（id=2）：两端点都只放行 Manager / Clerk。
    fn current_inspector() -> CurrentUser {
        CurrentUser {
            id: 2,
            username: "test-insp".to_string(),
            roles: vec![Role::Inspector],
            shelf_ids: vec![],
            shelf_wildcard: false,
        }
    }

    /// 两行请求 + 一次 match 调用（3 命中行 + 1 空装配件 ⇒ 期望 4 条查询）。
    ///
    /// 顺带钉死两条线格式契约：雪花 id 序列化成 **JSON string**（不是数字）、
    /// 响应数组长度恒等于请求 items 长度。
    #[tokio::test]
    async fn match_by_excel_items_orchestrates_four_queries_and_keeps_row_shape() {
        let mut mock = MockPartRepoTrait::new();
        // ① 零件：id=1 图号命中「PO-001」；id=2 是被命中装配件的子件（它自己图号不命中，
        //    由 ③ 带回来）
        mock.expect_list_match_parts_by_keys()
            .returning(|_codes, _names| Ok(vec![sample_part(1, "PO-001", "法兰盘", None)]));
        // ② 装配件：图号「PO-ASM」命中，子件 id=2
        mock.expect_list_match_assemblies_by_keys()
            .returning(|_codes, _names| Ok(vec![sample_assembly(900, "PO-ASM", "法兰装配体")]));
        // ③ 子件
        mock.expect_list_children_by_assemblies()
            .withf(|ids, inc| ids.contains(&900) && !*inc)
            .returning(|_ids, _inc| Ok(vec![sample_part(2, "C-1", "子件一", Some(900))]));
        // ④ 候选所属装配件名称
        mock.expect_list_assembly_names_by_ids()
            .returning(|_ids| Ok(vec![sample_assembly(900, "PO-ASM", "法兰装配体")]));

        let req = MatchByExcelItemsRequest {
            items: vec![
                item(Some("PO-001"), None),
                item(Some("PO-ASM"), None),
                item(Some("PO-NONE"), None),
                item(Some("PO-EMPTY"), None),
            ],
        };
        let out = PartService::match_by_excel_items(mock, &req, &current_manager())
            .await
            .expect("match 应成功");

        assert_eq!(out.len(), 4, "响应长度必须恒等于请求 items 长度");
        assert_eq!(
            out.iter().map(|r| r.row_no).collect::<Vec<_>>(),
            vec![1, 1, 1, 1],
            "row_no 应原样回显（各 item 都构造为 row_no=1）"
        );
        assert_eq!(out[0].match_type, ExcelMatchType::PartCode);
        assert_eq!(out[1].match_type, ExcelMatchType::AssemblyCode);
        assert_eq!(out[2].match_type, ExcelMatchType::None);
        assert_eq!(out[3].match_type, ExcelMatchType::None);
        assert_eq!(out[1].parts[0].assembly_name.as_deref(), Some("法兰装配体"));
        assert_eq!(
            out[1].parts[0].version, 3,
            "version 必须回传（前端提交时的 OCC 依据，缺了整个更新请求会 400）"
        );
        // 线格式：`serialize_i64` / `serialize_i64_opt` ⇒ 字符串
        let j = serde_json::to_value(&out).expect("序列化响应");
        assert!(
            j[0]["parts"][0]["part_id"].is_string(),
            "part_id 必须是 JSON string: {j}"
        );
        assert!(
            j[0]["parts"][0]["assembly_id"].is_null(),
            "无所属装配件时 assembly_id 应为 null: {j}"
        );
        assert_eq!(
            j[0]["match_type"], "PART_CODE",
            "枚举线格式必须是 PART_CODE 大写下划线: {j}"
        );
        assert!(
            j[0]["warnings"].is_array(),
            "warnings 无异常时也必须是数组（不是 null）: {j}"
        );
    }

    /// 角色闸门：INSPECTOR 两个端点都拿 40300，且**一条 SQL 都不该发**。
    #[tokio::test]
    async fn match_by_excel_items_rejects_inspector_before_any_query() {
        let mock = MockPartRepoTrait::new(); // strict mode：未 expect ⇒ 被调用即 panic
        let req = MatchByExcelItemsRequest {
            items: vec![item(Some("PO-001"), None)],
        };
        let err = PartService::match_by_excel_items(mock, &req, &current_inspector())
            .await
            .expect_err("INSPECTOR 应被拒");
        assert_eq!(err.code(), crate::shared::error::code::FORBIDDEN);
    }

    /// 入参闸门：空 items / 超 2000 行 → 40001，且一条 SQL 都不发。
    #[tokio::test]
    async fn match_by_excel_items_rejects_bad_item_counts_before_any_query() {
        let req_empty = MatchByExcelItemsRequest { items: vec![] };
        let err = PartService::match_by_excel_items(
            MockPartRepoTrait::new(),
            &req_empty,
            &current_manager(),
        )
        .await
        .expect_err("空 items 应被拒");
        assert_eq!(err.code(), crate::shared::error::code::VALIDATION_ERROR);

        let too_many = MatchByExcelItemsRequest {
            items: (0..=MATCH_MAX_ITEMS)
                .map(|_| item(Some("PO-001"), None))
                .collect(),
        };
        assert_eq!(too_many.items.len(), MATCH_MAX_ITEMS + 1);
        let err = PartService::match_by_excel_items(
            MockPartRepoTrait::new(),
            &too_many,
            &current_manager(),
        )
        .await
        .expect_err("超限 items 应被拒");
        assert_eq!(err.code(), crate::shared::error::code::VALIDATION_ERROR);
    }
}

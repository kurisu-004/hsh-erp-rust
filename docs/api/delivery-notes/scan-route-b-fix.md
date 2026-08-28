# 路线 B 修复计划：`POST /api/v2/delivery-notes/scan`（后端）

> **状态**：规划稿（2026-08-26，待评审；2026-08-27 字段精简 + 错误码重写）。
> **关联**：[`drafts.md`](./drafts.md#post-apiv2delivery-notesscan--p3-扫码建单) 是当前端点的实现规范；本文档描述把它从"原子失败"改造成"软成功 + 候选批次清单"的具体改动。
> **配套前端文档**：[`/Users/ren/Code/frontend/docs/03-modules/scan-route-b-fix.md`](../../../../../frontend/docs/03-modules/scan-route-b-fix.md)

---

## Context

当前 `POST /api/v2/delivery-notes/scan` 是"原子入单"思路：

| 用户场景 | 当前实现路径 | 响应 |
|---|---|---|
| ① 散件 + 全部已送检 | 200 OK | `outcome=ADDED`, `added_batches=[…]` |
| ② 散件 + 全部未送检 | **21405 错误** | `data.message` 文本，无结构化明细 |
| ③ 装配件 + 全部子件已送检 | 200 OK | `outcome=ADDED`, `added_batches=[各子件]` |
| ④ 装配件 + 部分子件未送检 | **21418 错误** | `data.failures[]` 含 `part_id/batch_id/status/...` |

**问题**：场景 ②、④ 用户期望看到"候选批次清单"，前端可以驱动"挑一个去送检"；当前却要弹错误 toast → 弹窗驱动一键送检 → 关闭弹窗 → 重新扫码，工作流断裂。

**路线 B 目标**：把 ②、④ 从错误路径改成 200 路径下的新 outcome 变体（`CANDIDATES_AVAILABLE` / `PARTIAL_ADDED`），把失败明细结构化成 `unresolved_targets[]` 返回。

### batch 状态 5 类分组（2026-08-27 新增）

按业务语义把 `t_part_batch.status`（10 值枚举，详见 `src/modules/part/statemachine.rs`）分成 3 组 + 2 个分组边界条件：

| 组 | 状态 | 行为 |
|---|---|---|
| **A 直接入单** | `READY_TO_SHIP`, `INSPECTION` | 满足 `delivery_note_id IS NULL` → 进 `added_batches` |
| **B 候选→一键送检** | `PENDING`, `PROGRAMMING`, `IN_PROCESS`（非工人持有）, `REPAIRING` | 进 `unresolved_targets[i].available_batches[]`；前端调 `to-inspection` 自动送检，再 re-scan |
| **C 直接报错** | `DELIVERED`, `OUTSOURCE`, `IN_PROCESS`（工人持有）, `COMPLETED`, `CANCELLED` | **短路报错**，新错误码 `21421 BIZ_DELIVERY_BATCH_STATE_INVALID`（保留旧 `21405` 不动） |

**分组边界条件**：

- `IN_PROCESS` 是否"被工人持有"——以 `t_part_batch.current_holder_id IS NOT NULL` 为主（前端无需关心具体 holder 类型）
- B 组送检需通过状态机白名单：当前 `REPAIRING → INSPECTION` 不在白名单，**本次需新增**（详见 B.5）

---

## 一、目标响应形状（与前端共同契约）

```jsonc
{
  "outcome": "ADDED" | "ALREADY_PRESENT" | "CANDIDATES_AVAILABLE" | "PARTIAL_ADDED",
  "resolved": {
    "kind": "PART" | "ASSEMBLY",
    "id": "string(i64)",                    // kind=PART → part.id；kind=ASSEMBLY → assembly.id
    "serial_no": "string",
    "drawing_no": "string",
    "name": "string"
    // ❌ 已删除：assembly_id（scan 不扫子件）、child_count（驱动决策不需要）
  },
  "note": { /* ScanDeliveryNoteSummaryDto，不变 */ },

  "added_batches":     [AddedBatchDto],                   // 场景 ①、③、④-已挂载部分
  "unresolved_targets": [UnresolvedTargetDto] | null      // 场景 ②（单元素）、④（多元素）
}
```

### 4 场景 → outcome 映射

| 场景 | outcome | added_batches | unresolved_targets |
|---|---|---|---|
| ① 散件全送检（A 组） | `ADDED` | ✓ | — |
| ② 散件仅 B 组 | `CANDIDATES_AVAILABLE` | `[]` | ✓（1 个元素，含该零件 B 组全部批次） |
| ③ 装配件全 A 组 | `ADDED` | ✓ | — |
| ④ 装配件部分 B 组 | `PARTIAL_ADDED` | ✓（A 组已挂载的） | ✓（B 组子件 + 各 B 组批次） |

**任一 target 含 C 组状态 → 硬错误 `21421`**，不进入任何 200 outcome。

### 错误码迁移

| 错误码 | 现有语义 | 路线 B 后的语义 |
|---|---|---|
| `21405 BIZ_DELIVERY_NOTE_PART_NOT_READY` | 散件扫描失败 / 装配件部分失败 | **保留**：scan 路径不再触发；改为新增 `21421 BIZ_DELIVERY_BATCH_STATE_INVALID`（C 组短路专用）：scan 路径的"批次状态不允许扫描"（C 组） |
| `21418 BIZ_DELIVERY_ASSEMBLY_PARTS_NOT_READY` | 装配件整套失败 | **保留**：scan 路径不再触发；仍由 `add_parts` 等端点使用：scan 不再触发，保留为 `add_parts` 兜底 |

> ⚠️ **破坏性变更**：原 `21405` 字符串作为前端 i18n key / 错误码 switch 散落在多处，**保留旧码，新增 21421**——`add_parts` / `inspect` 等路径可能仍引用旧码，本次同步迁移到新码。

---

## 二、改动清单

### B.1 DTO 改造（`src/modules/delivery_note/dto.rs`）

#### ① `ScanOutcomeDto`（`dto.rs:463-468`）—— 扩展枚举

```rust
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ScanOutcomeDto {
    /// A 组覆盖所有 target，本次成功挂载 ≥1 个。
    Added,
    /// A 组覆盖所有 target，但都已在本单（幂等）。
    AlreadyPresent,
    /// 散件扫描：仅 B 组 → unresolved_targets 单元素。
    CandidatesAvailable,
    /// 装配件扫描：A 组部分挂载，B 组入 unresolved_targets。
    PartialAdded,
}
```

#### ② 新增 `BatchStatusDto` 强类型枚举

```rust
/// `t_part_batch.status` 强类型投影，避免前端 magic string 比较。
/// 序列化沿用 DB 列值（SCREAMING_SNAKE_CASE）。
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum BatchStatusDto {
    Pending,
    Programming,
    InProcess,
    Inspection,
    ReadyToShip,
    Delivered,
    Repairing,
    Outsource,
    Completed,
    Cancelled,
}

impl BatchStatusDto {
    pub fn from_db(s: &str) -> Option<Self> {
        Some(match s {
            "PENDING" => Self::Pending,
            "PROGRAMMING" => Self::Programming,
            "IN_PROCESS" => Self::InProcess,
            "INSPECTION" => Self::Inspection,
            "READY_TO_SHIP" => Self::ReadyToShip,
            "DELIVERED" => Self::Delivered,
            "REPAIRING" => Self::Repairing,
            "OUTSOURCE" => Self::Outsource,
            "COMPLETED" => Self::Completed,
            "CANCELLED" => Self::Cancelled,
            _ => return None,
        })
    }
}
```

#### ③ 精简后的 `ScanDeliveryOut` + 4 个嵌套 DTO（约 `dto.rs:541-579` 全段重写）

```rust
#[derive(Debug, Clone, Serialize)]
pub struct ScanDeliveryOut {
    pub outcome: ScanOutcomeDto,
    pub resolved: ResolvedEntityDto,
    pub note: ScanDeliveryNoteSummaryDto,

    /// 场景 ①、③、④-已挂载部分；其余场景为 `[]`
    pub added_batches: Vec<AddedBatchDto>,

    /// 场景 ②（单元素）、④（多元素）；其余场景为 `null`
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unresolved_targets: Option<Vec<UnresolvedTargetDto>>,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ResolvedKindDto {
    Part,
    Assembly,
}

#[derive(Debug, Clone, Serialize)]
pub struct ResolvedEntityDto {
    pub kind: ResolvedKindDto,
    #[serde(serialize_with = "crate::shared::types::serialize_i64")]
    pub id: i64,
    pub serial_no: String,
    pub drawing_no: String,
    pub name: String,
}

/// 已挂载批次（`added_batches[]`）；跨子件场景 part_id/serial_no 必填。
#[derive(Debug, Clone, Serialize)]
pub struct AddedBatchDto {
    #[serde(serialize_with = "crate::shared::types::serialize_i64")]
    pub batch_id: i64,
    #[serde(serialize_with = "crate::shared::types::serialize_i64")]
    pub part_id: i64,
    pub serial_no: String,
    pub quantity: i32,
}

/// 未就绪 part + 其 B 组候选批次（`unresolved_targets[]`）。
/// 散件场景：单元素；装配件场景：每个未就绪子件一个元素。
#[derive(Debug, Clone, Serialize)]
pub struct UnresolvedTargetDto {
    #[serde(serialize_with = "crate::shared::types::serialize_i64")]
    pub part_id: i64,
    pub serial_no: String,
    pub drawing_no: String,
    pub name: String,
    pub available_batches: Vec<AvailableBatchDto>,
}

/// B 组候选批次（`unresolved_targets[i].available_batches[]`）。
/// part 级信息（serial_no/drawing_no/name）在 `UnresolvedTargetDto` 外层，不重复。
#[derive(Debug, Clone, Serialize)]
pub struct AvailableBatchDto {
    #[serde(serialize_with = "crate::shared::types::serialize_i64")]
    pub batch_id: i64,
    pub quantity: i32,
    pub status: BatchStatusDto,
}
```

**要点**：

- `AddedBatchDto` 与 `AvailableBatchDto` 是两个独立 DTO——前者跨子件必须保留 `part_id`/`serial_no`；后者单 part 维度，part 级信息从外层 `UnresolvedTargetDto` 推得
- 删除的字段：`ResolvedEntityDto.assembly_id`、`ResolvedEntityDto.child_count`、`AvailableBatchDto.part_id/serial_no/drawing_no/name/location`（location 不参与驱动决策）
- `recent_items` 保留（卡片首屏延迟优化场景独立，与本次重构正交）

### B.2 service 改造（`src/modules/delivery_note/service/scan.rs:99-593`）

#### `scan_add` 流程重构

保留 step 1（trim + resolve_scan_kind）、step 2（anchor + classify）、step 3（find-or-create draft）不变；重构 step 4-7，按 5 类分组判定。

```rust
pub async fn scan_add(
    conn: &mut PgConnection,
    snowflake: &SnowflakeIdGenerator,
    code: &str,
    current: &CurrentUser,
) -> Result<ScanDeliveryOut, AppError> {
    current.require_any_role(&[Role::Manager, Role::Clerk, Role::Inspector])?;

    // ===== Step 1: 解析（不变）=====
    let trimmed = code.trim().to_string();
    let trimmed_len = trimmed.chars().count();
    if trimmed_len == 0 || trimmed_len > 64 {
        return Err(AppError::biz(code::BIZ_INVALID_VALUE,
            format!("code length must be 1..=64 chars (trimmed), got {trimmed_len}")));
    }

    let part_opt = PartRepo::get_by_serial(&mut *conn, &trimmed, false).await?;
    let asm_opt = AssemblyRepo::get_by_serial(&mut *conn, &trimmed, false).await?;
    let kind = resolve_scan_kind(part_opt.as_ref(), asm_opt.as_ref());
    if matches!(kind, ScanKind::Unknown) {
        return Err(AppError::biz(code::BIZ_DELIVERY_SCAN_UNKNOWN_CODE,
            format!("scan code '{trimmed}' does not match any part or assembly")));
    }

    // ===== Step 2: 锚点 + L1 + 分类（不变）=====
    // ... 加载 targets, anchor_customer_id, scope（沿用现有 code）...

    // ===== Step 3: find-or-create 草稿（不变）=====
    let note = Self::scan_find_or_create_draft(conn, snowflake, l1_id, scope, current).await?;

    // ===== Step 4: 评估每个 target，按 5 类分组 =====
    struct TargetEvaluation {
        part: TPart,
        attachable: Vec<TPartBatch>,          // A 组：READY_TO_SHIP / INSPECTION 且未挂别单
        inspectable: Vec<TPartBatch>,         // B 组：PENDING/PROGRAMMING/IN_PROCESS(非持有)/REPAIRING
        conflict: Vec<TPartBatch>,            // 已挂别的 active 单
    }

    let mut evaluations: Vec<TargetEvaluation> = {
        let target_part_ids: Vec<i64> = targets.iter().map(|p| p.id).collect();
        let all_batches = PartBatchRepo::list_active_by_part_ids(&mut *conn, &target_part_ids).await?;
        let mut by_part: HashMap<i64, Vec<TPartBatch>> = HashMap::new();
        for b in all_batches { by_part.entry(b.part_id).or_default().push(b); }

        // === 短路：C 组任意一条命中 → 立即报 21421 ===
        for b in by_part.values().flatten() {
            if let Some(reason) = classify_invalid_state(b) {
                return Err(AppError::biz(code::BIZ_DELIVERY_BATCH_STATE_INVALID,
                    format!("batch {} is in invalid state: {reason}", b.id)));
            }
        }

        targets.into_iter().map(|p| {
            let batches = by_part.remove(&p.id).unwrap_or_default();
            let mut attachable = Vec::new();
            let mut inspectable = Vec::new();
            let mut conflict = Vec::new();
            for b in batches {
                if b.delivery_note_id.is_some() && b.delivery_note_id != Some(note.id) {
                    conflict.push(b);
                } else if b.delivery_note_id.is_none() && is_attachable_state(&b.status) {
                    attachable.push(b);
                } else if b.delivery_note_id.is_none() && is_inspectable_state(&b) {
                    inspectable.push(b);
                }
                // 注：delivery_note_id == Some(note.id) 的批次视为已挂本单，跳过
            }
            TargetEvaluation { part: p, attachable, inspectable, conflict }
        }).collect()
    };

    // ===== Step 5: 判定 outcome =====
    // 所有 target 都只有 conflict_batches → 仍报 21406 硬错误（无可选 → 软化没意义）
    let all_conflict = evaluations.iter().all(|e|
        e.attachable.is_empty() && e.inspectable.is_empty() && !e.conflict.is_empty());
    if all_conflict {
        return Err(AppError::biz(code::BIZ_DELIVERY_NOTE_PART_ALREADY_ASSIGNED,
            "all target batches are attached to other active notes".to_string()));
    }

    let is_assembly = matches!(kind, ScanKind::Assembly | ScanKind::PartOfAssembly(_));
    let any_inspectable = evaluations.iter().any(|e| !e.inspectable.is_empty());

    let outcome = match (is_assembly, any_inspectable) {
        (false, true)  => ScanOutcomeDto::CandidatesAvailable,
        (true,  true)  => ScanOutcomeDto::PartialAdded,
        (_,     false) => {
            if evaluations.iter().all(|e| e.attachable.is_empty()) {
                ScanOutcomeDto::AlreadyPresent
            } else {
                ScanOutcomeDto::Added
            }
        }
    };

    // ===== Step 6: attach（仅 outcome=Added / PartialAdded 走）=====
    let mut added_batches: Vec<AddedBatchDto> = Vec::new();
    if matches!(outcome, ScanOutcomeDto::Added | ScanOutcomeDto::PartialAdded) {
        for e in &evaluations {
            for b in &e.attachable {
                PartBatchRepo::attach_to_note(&mut *conn, b.id, note.id, current.user_id).await?;
                added_batches.push(AddedBatchDto {
                    batch_id: b.id,
                    part_id: b.part_id,
                    serial_no: e.part.serial_no.clone().unwrap_or_default(),
                    quantity: b.quantity,
                });
            }
        }
    }

    // ===== Step 7: 构建 unresolved_targets =====
    let unresolved_targets = match outcome {
        ScanOutcomeDto::CandidatesAvailable => {
            // 散件 → 单元素
            let e = evaluations.into_iter().next()
                .expect("CandidatesAvailable 必有 1 个 target");
            Some(vec![build_unresolved_target(e)])
        }
        ScanOutcomeDto::PartialAdded => {
            // 装配件 → 仅 B 组子件入列表
            Some(evaluations.into_iter()
                .filter(|e| !e.inspectable.is_empty())
                .map(build_unresolved_target)
                .collect())
        }
        _ => None,
    };

    Ok(ScanDeliveryOut {
        outcome,
        resolved,
        note,
        added_batches,
        unresolved_targets,
    })
}

// ===== 辅助函数 =====

/// A 组：可直接 attach 入单。
fn is_attachable_state(status: &str) -> bool {
    matches!(status, "READY_TO_SHIP" | "INSPECTION")
}

/// B 组：可送检。`IN_PROCESS` 需未被工人持有（`current_holder_id IS NULL`）。
fn is_inspectable_state(b: &TPartBatch) -> bool {
    match b.status.as_str() {
        "PENDING" | "PROGRAMMING" | "REPAIRING" => true,
        "IN_PROCESS" => b.current_holder_id.is_none(),
        _ => false,
    }
}

/// C 组：直接报错的非法状态。`IN_PROCESS` 被工人持有归此类。
fn classify_invalid_state(b: &TPartBatch) -> Option<&'static str> {
    match b.status.as_str() {
        "DELIVERED" => Some("DELIVERED"),
        "OUTSOURCE" => Some("OUTSOURCE"),
        "COMPLETED" => Some("COMPLETED"),
        "CANCELLED" => Some("CANCELLED"),
        "IN_PROCESS" if b.current_holder_id.is_some() => Some("IN_PROCESS_HELD_BY_WORKER"),
        _ => None,
    }
}

fn build_unresolved_target(e: TargetEvaluation) -> UnresolvedTargetDto {
    UnresolvedTargetDto {
        part_id: e.part.id,
        serial_no: e.part.serial_no.clone().unwrap_or_default(),
        drawing_no: e.part.drawing_no.clone(),
        name: e.part.name.clone(),
        available_batches: e.inspectable.into_iter().map(|b| AvailableBatchDto {
            batch_id: b.id,
            quantity: b.quantity,
            status: BatchStatusDto::from_db(&b.status).unwrap_or(BatchStatusDto::Pending),
        }).collect(),
    }
}
```

#### 关键设计决策

1. **保留 21406（`BIZ_DELIVERY_NOTE_PART_ALREADY_ASSIGNED`）作为硬错误**：所有 A/B 组都空 + 全部 conflict 时仍报错（无可选 → 软化没意义，前端 ElMessage 兜底）。
2. **C 组短路在 step 4 循环最前面**：扫描到第一条 C 组批次就返回，避免做无用功。
3. **C 组短路与"批次数 N+1 加载"冲突**：当前 `list_active_by_part_ids` 是一次加载所有 target 的全部批次；C 组短路遍历的是已加载的 `by_part.values().flatten()`，不额外查 DB——OK。
4. **`serial_no` 字段加载**：现有 `AddedBatchDto` 的 `serial_no` 来自 `part.serial_no`（通过 part_id 关联），不是 `batch` 上的字段；本计划保留此约定（`added_batches` 内 `serial_no` 仍来自 part）。
5. **scan 路径幂等性**：用户决定的两段式工作流（先 `to-inspection` 再 re-scan）要求 scan 端点**幂等**——重试同 serial_no 不应产生新草稿、不应重复挂载。`scan_find_or_create_draft` 现有实现按 `(l1_id, scope)` 找草稿，命中即复用，符合幂等要求（验证测试见 B.3）。

### B.3 单元测试更新（`src/modules/delivery_note/service/scan.rs:674-809`）

- `scan_resolve_tests`（5 个）**不动**
- `classify_tests`（4 个）**不动**
- **新增** `outcome_tests`（10 个）+ **新增** `classify_5groups_tests`（4 个）：

```rust
#[cfg(test)]
mod classify_5groups_tests {
    use super::*;

    #[test]
    fn is_attachable_includes_ready_and_inspection() {
        assert!(is_attachable_state("READY_TO_SHIP"));
        assert!(is_attachable_state("INSPECTION"));
        assert!(!is_attachable_state("PENDING"));
        assert!(!is_attachable_state("DELIVERED"));
    }

    #[test]
    fn is_inspectable_includes_pending_programming_repairing() {
        let mut b = fixture_batch("PENDING", None);
        assert!(is_inspectable_state(&b));
        b.status = "PROGRAMMING".into();
        assert!(is_inspectable_state(&b));
        b.status = "REPAIRING".into();
        assert!(is_inspectable_state(&b));
    }

    #[test]
    fn is_inspectable_in_process_requires_no_holder() {
        let mut b = fixture_batch("IN_PROCESS", None);
        assert!(is_inspectable_state(&b));           // 无 holder → B 组
        b.current_holder_id = Some(42);
        assert!(!is_inspectable_state(&b));           // 有 holder → 非 B 组（被 C 组捕获）
    }

    #[test]
    fn classify_invalid_catches_held_in_process() {
        let mut b = fixture_batch("IN_PROCESS", Some(42));
        assert_eq!(classify_invalid_state(&b), Some("IN_PROCESS_HELD_BY_WORKER"));
        b.current_holder_id = None;
        assert_eq!(classify_invalid_state(&b), None);
    }
}

#[cfg(test)]
mod outcome_tests {
    use super::*;

    #[test] fn outcome_added_when_all_attachable() { /* ... */ }
    #[test] fn outcome_already_present_when_all_on_note() { /* ... */ }
    #[test] fn outcome_candidates_available_when_standalone_only_inspectable() { /* ... */ }
    #[test] fn outcome_partial_added_when_assembly_mixed() { /* ... */ }

    // 新增：错误码迁移相关
    #[test] fn c_group_state_returns_21421_immediately() { /* ... */ }
    #[test] fn c_group_short_circuits_before_outcome_judgement() { /* ... */ }
    #[test] fn all_conflict_still_returns_21406_hard_error() { /* ... */ }

    // 新增：幂等性
    #[test] fn scan_same_code_twice_returns_already_present() { /* ... */ }
    #[test] fn scan_after_to_inspection_promotes_b_to_a() { /* ... */ }
}
```

### B.4 handler 调整（`src/modules/delivery_note/handler.rs:392-430`）

WS 广播 payload 增加 outcome 表达力（前端大屏 WS 监听可能依赖）：

```rust
state.ws_hub.broadcast(crate::infra::ws_hub::WsEvent::DashboardEvent {
    kind: "DELIVERY_NOTE_SCAN_ADD".to_string(),
    payload: serde_json::json!({
        "delivery_note_id": out.note.id,
        "delivery_note_no": out.note.delivery_note_no,
        "added_count": out.added_batches.len(),
        "unresolved_count": out.unresolved_targets.as_ref().map(|v| v.len()).unwrap_or(0),
        "line_count": out.note.line_count,
        "resolved_kind": out.resolved.kind,
        "outcome": match out.outcome {
            ScanOutcomeDto::Added => "ADDED",
            ScanOutcomeDto::AlreadyPresent => "ALREADY_PRESENT",
            ScanOutcomeDto::CandidatesAvailable => "CANDIDATES_AVAILABLE",
            ScanOutcomeDto::PartialAdded => "PARTIAL_ADDED",
        },
    }),
});
```

### B.5 状态机白名单补充（`src/modules/part/statemachine.rs`）

为支持 B 组 `REPAIRING → INSPECTION` 送检：

```rust
// PartStatus::can_transition_to 内追加：
| (REPAIRING, INSPECTION)            // 返修完成 → 重新送检
```

并同步更新 `round_trip_all_statuses` 测试覆盖 + `allowed_transitions_*` 测试。

> ⚠️ 业务语义确认：返修完成后是否真的需要"重新送检"？还是走其它路径（如直接恢复 `READY_TO_SHIP`）？如选后者，则 `REPAIRING` 实际归入 C 组。**需要业务方确认后再落白名单**。

### B.6 集成测试（`tests/delivery_note_scan_integration.rs`）

**新增** 7 个端到端场景：

```rust
#[tokio::test]
async fn scan_standalone_part_with_inspection_returns_added() { /* INSPECTION → A 组 */ }
#[tokio::test]
async fn scan_standalone_part_with_ready_returns_added() { /* READY_TO_SHIP → A 组 */ }
#[tokio::test]
async fn scan_standalone_part_with_only_pending_returns_candidates() { /* PENDING → B 组 */ }
#[tokio::test]
async fn scan_assembly_with_all_ready_returns_added() { /* 子件全 A 组 */ }
#[tokio::test]
async fn scan_assembly_with_partial_ready_returns_partial_added() { /* A+B 混合 */ }
#[tokio::test]
async fn scan_with_delivered_batch_returns_21421() { /* C 组短路 */ }
#[tokio::test]
async fn scan_twice_same_code_is_idempotent() { /* 两段式 re-scan 验证 */ }
```

每个测试用 `create_test_part_with_batch(status, holder_id)` 等 helper 覆盖各状态。

### B.7 错误码清理（`src/shared/error.rs`）

- **新增** `21421 BIZ_DELIVERY_BATCH_STATE_INVALID`（scan 路径 C 组短路专用）
- 全仓 `grep` 旧引用并替换：`add_parts` / `inspect` / `deliver` 等路径可能仍引用
- `21418 BIZ_DELIVERY_ASSEMBLY_PARTS_NOT_READY`：**保留**，scan 不再触发；不标 `#[deprecated]` 以免破坏 `add_parts` 等其它调用点编译警告
- 不从 `code` 模块删除任何常量（保持外部接口稳定；旧 `21405` / `21418` 继续由 `add_parts` 等触发）

### B.8 API 文档同步（`docs/api/delivery-notes/drafts.md:30-95`）

更新字段表：
- `resolved` 字段精简（删 `assembly_id`/`child_count`）
- `available_batches` + `blocked_targets` → `unresolved_targets`（单一形态）
- `AvailableBatchDto` 字段精简（删 `part_id`/`serial_no`/`drawing_no`/`name`/`location`）
- 错误码段：保留 `21405` / `21418`（标 "scan 不再触发"）；保留 `21417` / `21406`；新增 `21421`（C 组短路）

---

## 三、关键文件清单（后端）

| 文件 | 改动 |
|---|---|
| `src/modules/delivery_note/dto.rs:446-579` | 新增 2 个 outcome 变体；新增 `BatchStatusDto` 强类型枚举；精简 DTO 5 个（`ScanDeliveryOut` / `ResolvedEntityDto` / `AddedBatchDto` / `UnresolvedTargetDto` / `AvailableBatchDto`）；删除 `assembly_id` / `child_count` / `location` 等冗余字段 |
| `src/modules/delivery_note/service/scan.rs:99-593` | 重构 `scan_add` 步骤 4-7；按 5 类分组（A/B/C）判定；新增 `is_attachable_state` / `is_inspectable_state` / `classify_invalid_state` / `build_unresolved_target` |
| `src/modules/delivery_note/service/scan.rs:674-809` | 新增 `classify_5groups_tests`（4 个）+ `outcome_tests`（10 个） |
| `src/modules/delivery_note/handler.rs:392-430` | WS 广播 payload 增加 `outcome` / `unresolved_count` |
| `src/modules/part/statemachine.rs` | `can_transition_to` 加 `REPAIRING → INSPECTION`；测试同步 |
| `src/shared/error.rs` | **新增** `BIZ_DELIVERY_BATCH_STATE_INVALID = 21421`；保留 `21405` / `21418` 不变 |
| `tests/delivery_note_scan_integration.rs` | **新增** 7 个场景测试（含 C 组短路 + 幂等性） |
| `docs/api/delivery-notes/drafts.md:30-95` | 同步字段表 + 错误码 |

---

## 四、验证方法

```bash
# 静态
cargo check
cargo clippy --all-targets

# 单元（解析/分类/新 outcome）
cargo test --lib delivery_note::service::scan

# 集成（7 个场景端到端）
cargo test --test delivery_note_scan_integration

# 重新生成 SQLx 元数据
./scripts/sqlx_prepare.sh

# 离线构建
SQLX_OFFLINE=true cargo build --release
```

### 手工冒烟（起 dev server）

| 场景 | 输入状态 | 预期 outcome | 预期行为 |
|---|---|---|---|
| ① | 散件 READY_TO_SHIP | `ADDED` | 200，body 有 `added_batches`，`unresolved_targets` 字段不存在 |
| ② | 散件 INSPECTION | `ADDED` | 200，body 有 `added_batches`（INSPECTION 也算 A 组） |
| ③ | 散件 PENDING | `CANDIDATES_AVAILABLE` | 200，`unresolved_targets[0].available_batches[]` 含该零件 B 组全部 |
| ④ | 装配件子件 A+B 混合 | `PARTIAL_ADDED` | 200，`added_batches[]`（A 组）+ `unresolved_targets[]`（B 组子件） |
| ⑤ | 任一批次 DELIVERED | — | 21421 `BIZ_DELIVERY_BATCH_STATE_INVALID` |
| ⑥ | IN_PROCESS 工人持有 | — | 21421，reason=`IN_PROCESS_HELD_BY_WORKER` |
| ⑦ | REPAIRING（白名单未补前） | — | to-inspection 端点会拒；先补 `statemachine.rs` 再回归 |

### 回归

- `21417 SCAN_UNKNOWN_CODE` 仍走 `ElMessage.error`（前端兜底）
- `21406 PART_ALREADY_ASSIGNED` 仍报硬错误
- WS `DELIVERY_NOTE_SCAN_ADD` 事件仍发，payload 含 `outcome` / `added_count` / `unresolved_count`
- scan 端点幂等：相同 serial_no 连续两次扫描，第二次返回 `ALREADY_PRESENT`（不创建新草稿）
//! admin 域 DTO：`POST /api/v2/admin/recompute-rollup` 的入参 / 出参
//!
//! 2026-10-01 新增。全部 i64 字段按仓内统一约定以 **JSON 字符串** 进出
//! （`shared::types::serialize_i64` / `deserialize_i64_vec_opt`），防 JS 精度截断。

use serde::{Deserialize, Serialize};

use crate::shared::types::{
    deserialize_i64_opt, deserialize_i64_vec_opt, serialize_i64, serialize_i64_opt,
};

/// `POST /api/v2/admin/recompute-rollup` 请求体（**可省略**）。
///
/// 省略 body / 传 `{}` = **全量对账**（`scope = "ALL"`）。三个字段互相独立：
/// - 只给 `part_ids` → 只重算这些 part 的 batch → part 派生（父装配件由
///   status_gate 内部自动级联）；
/// - 只给 `assembly_ids` → 只重算这些装配件的 part → assembly 聚合，**part 段
///   完全跳过**（不给 `part_ids` 绝不等于「全量重算 part」）；
/// - 两个都给 → 两段都做（先 part 后 assembly，顺序保证父件读到的是**已修正**的
///   子件状态，而不是修正前的旧值）。
#[derive(Debug, Default, Clone, Deserialize)]
pub struct RecomputeRollupRequest {
    /// 指定要重算的 `t_part.id` 列表（字符串数组）。`None` / 空 = 不限定 part。
    #[serde(default, deserialize_with = "deserialize_i64_vec_opt")]
    pub part_ids: Option<Vec<i64>>,
    /// 指定要重算的 `t_assembly.id` 列表（字符串数组）。`None` / 空 = 不限定装配件。
    #[serde(default, deserialize_with = "deserialize_i64_vec_opt")]
    pub assembly_ids: Option<Vec<i64>>,
    /// 「不限 id」时最多处理多少行（默认 1000，上限 10000）。
    ///
    /// 为什么要上限：全量对账是**全表扫描 + 每行派生写**，不限量的话一个请求就能
    /// 把整张 `t_part` / `t_assembly` 锁在长事务里，与线上业务抢行锁。给了上限
    /// 之后，运维用 `after_id` 游标重复调用若干轮即可把全表扫完
    /// （每轮都是幂等的），而单次请求的影响面有界。
    #[serde(default)]
    pub limit: Option<i64>,
    /// 续扫游标（字符串 id）：**只**处理 `id > after_id` 的行。
    ///
    /// 缺省 = 从最小 id 开始（首轮）。后续轮次把上一轮响应里的
    /// `next_after_id` 原样回传即可。
    ///
    /// 2026-10-01 review 第 1 轮 M7 新增：原实现只有 `ORDER BY id LIMIT n+1`、
    /// 没有游标也没有 offset —— `truncated = true` 时运维再调一次仍然从最小的
    /// `limit` 行开始扫，**永远收敛不了**，一个兜底端点在生产数据量下兜不住底。
    #[serde(default, deserialize_with = "deserialize_i64_opt")]
    pub after_id: Option<i64>,
}

/// 一次状态变化（`before → after`）的明细。
///
/// 只列**真的变了**的行；没变的行不进 `changes`（否则全量对账会返回几十万条
/// 噪音，前端也没法看）。
#[derive(Debug, Clone, Serialize)]
pub struct StatusChangeEntry {
    /// `PART` / `ASSEMBLY` —— 区分哪一层的派生缓存被修正了。
    pub level: &'static str,
    #[serde(serialize_with = "serialize_i64")]
    pub id: i64,
    pub from: String,
    pub to: String,
}

/// 对账报告（放在标准 `R<T>` 信封的 `data` 里）。
#[derive(Debug, Clone, Default, Serialize)]
pub struct RecomputeRollupReport {
    /// `ALL` / `PART_IDS` / `ASSEMBLY_IDS` / `PART_IDS+ASSEMBLY_IDS`，便于前端 /
    /// 日志区分「这次是全量还是定点」。
    pub scope: String,
    /// 本次实际检查的 part 数（= 被派生算法读过的 part 数）。
    pub parts_examined: u64,
    /// `t_part.status` 真变了的 part 数。
    pub parts_changed: u64,
    /// `t_part.next_process_id` 被修正的 part 数（status 没变、只有工序指针漂移
    /// 的情况也计入 —— 它同样是派生缓存，同样会导致「派工到错误工序」）。
    pub parts_next_process_id_fixed: u64,
    pub assemblies_examined: u64,
    pub assemblies_changed: u64,
    /// 命中 `limit` 上限、**还有行没扫到**。`true` 时运维应把
    /// [`Self::next_after_id`] 原样回传为请求体的 `after_id` 再调一次
    /// （窗口按 `id` 升序推进，直到 `truncated = false` 即扫完整表）。
    pub truncated: bool,
    /// 续扫游标：本轮**已处理**的最大 id。仅在 `truncated = true` 时有意义
    /// （`false` 时已是末轮，置 `null`）。
    #[serde(serialize_with = "serialize_i64_opt")]
    pub next_after_id: Option<i64>,
    /// 逐条 before → after。
    pub changes: Vec<StatusChangeEntry>,
}

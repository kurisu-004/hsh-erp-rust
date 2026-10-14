//! `serial` 域入参 / 出参（**2026-10-19 新建**）
//!
//! 本域只服务一个端点（`POST /api/v2/serials/release-terminal`），所以 dto 只有
//! 「一套入参 + 一套出参」，没有分页 / 列表 DTO。
//!
//! ## 出参为什么要「计数 + 明细」两段
//!
//! 这个端点的交互形态是**破坏性批量维护**：它会一次性清掉全库所有终态工单占着的
//! 序列号。前端必然做成「先预览、再二次确认」两步（入参 `dry_run` 复用同一套出参），
//! 而确认框要展示的是「**一共会动多少行**」—— 只有明细列表的话，用户要么看不到总数
//! （被 LIMIT 截断时更会以为「就这些」），要么被迫渲染上万行把对话框撑爆。
//! ⇒ `released_total` 是**真实总数、不受明细上限影响**，与 `affected` 解耦，两者
//! 刻意分开报就是为了让「计数与明细」永远不会互相冒充。

use serde::{Deserialize, Serialize};

/// `POST /api/v2/serials/release-terminal` 入参。
///
/// `dry_run` 用 `#[serde(default)]` ⇒ **整个 body 传空对象 `{}` 时等价于
/// `{"dry_run": false}`**，也就是**真跑**。这不是随手写的：破坏性操作宁可
/// 「显式 `dry_run: false`」也不该靠「缺字段 = true」保护，但反过来 `dry_run`
/// 缺省为 false 也意味着**误调一次就是全量清理**。前端接入时必须先发
/// `{"dry_run": true}` 预览、用户确认后再发 `{"dry_run": false}`；
/// 该风险已登记在 `docs/api/serial.md` §8 的已知偏差登记。
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ReleaseTerminalRequest {
    /// `true` = 只读预览（**零写入**、不开事务、不广播 WS）；
    /// `false` = 真跑（开事务、逐行释放、commit 后广播一条汇总 WS）。
    #[serde(default)]
    pub dry_run: bool,
}

/// 一行「本端点会动到」的工单明细。
///
/// ⚠️ **`id` 是 `String` 不是 `i64`**：雪花 ID 超出 JS `Number.MAX_SAFE_INTEGER`
/// （前端一律按字符串处理，见 `modules::assembly::handler::force_complete_assembly`
/// 响应里 `assembly_id` 同样字符串化）。这里 `i64` 只在**内部** struct
/// `repo::TerminalSerialRow` 里流转，出参前 `.to_string()`。
#[derive(Debug, Clone, Serialize)]
pub struct ReleasedSerialItem {
    /// 行所属表：`"ASSEMBLY"`（装配件）/ `"PART"`（零件 / 子件）。
    pub kind: &'static str,
    /// 雪花 id，**序列化成字符串**（见本 struct 的 doc）。
    pub id: String,
    /// 被释放掉的原序列号。
    ///
    /// 恒为 `Some`：`repo` 的守卫带 `serial_no IS NOT NULL`，明细列表根本取不到空值。
    pub serial_no: String,
    /// 图纸号。
    ///
    /// ⚠️ 这里是 `String` 不是 `Option<String>`：`t_assembly.drawing_no` 与
    /// `t_part.drawing_no` 在 baseline 里**都是 `NOT NULL`**（且没有任何后续 migration
    /// 去掉该约束），实测两列恒有值 ⇒ 用 `Option` 只会让消费侧多写一个恒不触发的
    /// `None` 分支。
    pub drawing_no: String,
    /// 释放时该行的状态，必为 `COMPLETED` 或 `CANCELLED`（守卫写死）。
    pub status: String,
}

/// `POST /api/v2/serials/release-terminal` 出参。
///
/// `dry_run = true` 与 `dry_run = false` 返回**同一个结构**（前端一个对话框组件
/// 即可两用，不必按模式分两套渲染）。
#[derive(Debug, Clone, Serialize)]
pub struct ReleaseTerminalOut {
    /// 与入参 `dry_run` 原样回显，供前端确认「这次到底是预览还是实跑」。
    pub dry_run: bool,
    /// 本次清空的**装配件**行数。
    pub assembly_count: i64,
    /// 本次清空的**零件 / 子件**行数。
    pub part_count: i64,
    /// `assembly_count + part_count`；**真实总数，不受 [`RELEASE_DETAIL_LIMIT`] 截断**。
    pub released_total: i64,
    /// `released_total > RELEASE_DETAIL_LIMIT` ⇒ `affected` 是不完整列表。
    ///
    /// 前端**必须**据此提示「仅显示前 N 条」，否则用户会把 200 条当成全部。
    pub truncated: bool,
    /// 明细（最多 [`RELEASE_DETAIL_LIMIT`] 行）：**装配件在前、子件在后**，
    /// 各自按 `id` 升序。
    pub affected: Vec<ReleasedSerialItem>,
}

/// 明细行数上限（**2026-10-19 新增**）。
///
/// **为什么要有上限**：本端点扫的是**全库**两张大表。生产上「已完成但没释放」的
/// 残留序列号在长期不清理的现场轻易上千行 —— 明细全量返回会让响应体膨胀到几百 KB、
/// 让前端确认对话框渲染几千行 DOM，**两端同时失控**，而用户真正需要的决策信息
/// （一共动多少行）由 `released_total` 已经完整给出。
///
/// **为什么不影响正确性**：`released_total` / `assembly_count` / `part_count`
/// 三个计数**取自真实候选集合，不经过本上限**，只有 `affected` 明细被截断，并由
/// `truncated` 标志显式告知。⚠️ **只有 `affected` 受本上限约束，写入不受**：
/// `dry_run` 与 apply 共用的是**候选判据口径**（同一谓词常量、apply 侧还是同一个
/// 锁内候选集），**不是**明细条数上限 —— apply 的候选 SELECT 刻意传 `limit = None`
/// （`service.rs` apply 分支），5000 行残留会实打实清 5000 行。给写入套上限会变成
/// 「报 200、实清 5000」的错配，比不限流严重得多。
/// 集成测试 `truncated_flag_and_limit` 钉的就是这个形态：种 201 行 ⇒
/// `affected.len() == 200` 而 `released_total == 201`。
pub const RELEASE_DETAIL_LIMIT: usize = 200;

/// `ReleasedSerialItem.kind` 的两个字面量（**唯一真源**，别在别处重写字符串）。
///
/// ⚠️ **必须是大写**，这是**全仓判别位的统一惯例**，不是本域的偏好：
/// - `com::union_list::repo::sql` 投影 `'PART'::text AS row_type` /
///   `'ASSEMBLY'::text AS row_type`（union 列表的 `row_type` 判别位）；
/// - `com::delivery_note::vo::scan_tree` 的 `hit_kind: "ASSEMBLY" | "PART"`；
/// - `prod::inspection::service` 扫码命中的 `hit_kind = "PART"` / `"ASSEMBLY"`；
/// - `shared::batch::status_machine` 的 `PartStatus` 一律 `#[serde(rename = "大写")]`。
///
/// 本域初版曾取小写（`"assembly"` / `"part"`）、与上述惯例分叉，已订正。判别位两侧
/// （后端常量 / 前端 schema）大小写必须逐字相同 —— 消费侧普遍对判别位写 `.strict()`
/// 守门 schema，大小写对不上时**每一行明细都过不了 Zod**、整个响应被拒（不是降级，
/// 是功能不存在），且这类失败发生在契约层、不产生任何红色报错。
pub const KIND_ASSEMBLY: &str = "ASSEMBLY";
/// `ReleasedSerialItem.kind` 的两个字面量（**唯一真源**，别在别处重写字符串）；
/// 大写惯例的依据见 [`KIND_ASSEMBLY`] 的 doc。
pub const KIND_PART: &str = "PART";

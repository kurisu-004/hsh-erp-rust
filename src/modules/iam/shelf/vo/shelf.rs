//! 货架子模块主端点响应 VO

use chrono::NaiveDateTime;
use serde::Serialize;

use crate::shared::types::serialize_i64;

/// 货架详情出参。
///
/// 2026-09-22 PR4：迁移到 vo/，仅 Serialize。
/// 2026-10-02 域拆分：原 `account_count` 字段删除 —— 货架域对账号的唯一耦合就是它
/// （喂 `t_user_role WHERE scope_type='shelf'` 的 GROUP BY 计数），而绑定真源本来
/// 就在 iam 域（`t_user_role`）。用户决定舍弃该字段、前端不再显示，故连同
/// `ShelfRepo::count_accounts_by_shelf` 一并移除，**本任务零 iam 模块改动**。
#[derive(Debug, Clone, Serialize)]
pub struct ShelfOut {
    #[serde(serialize_with = "serialize_i64")]
    pub id: i64,
    pub code: String,
    pub name: String,
    pub zone: String,
    pub location: Option<String>,
    pub is_active: bool,
    pub display_order: i32,
    /// 负载上限（件数）。`None` 或 `<= 0` = 不限（`t_shelf.capacity` 的原样透出，
    /// 故前端能原样回显「不限」这个状态，不必自己猜 `0` 与 `null` 的区别）。
    ///
    /// **裸 `Option<i32>` Serialize** ⇒ JSON 里是 `number | null`。刻意**不**套
    /// `serialize_i64`：那个 helper 把 id 序列化成**字符串**是为了防 JS
    /// `Number.MAX_SAFE_INTEGER` 精度截断（19 位雪花 ID），而 `capacity` 是
    /// 件数上限（i32 量级），发字符串会让前端的表单控件拿到 `"200"` 而不是 `200`。
    pub capacity: Option<i32>,
    /// 在架件数（**不是**批次数）：`SUM(t_part_batch.quantity)`，口径见
    /// [`crate::shared::shelf::load::LOAD_AGGREGATE_SQL`]。裸 `i64` ⇒ JSON number，
    /// 与本 VO 既有 `ShelfListOut.total` 同向。
    ///
    /// 它**不是存储列**，每次读时聚合。
    pub current_load: i64,
    pub version: i32,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
}

// ⚠️ **`load_ratio` 刻意不进本 VO**（2026-10-10）
//
// 选架内部有 `load_ratio`（`current_load / capacity`，「不限」时为 `None`），但
// 出参**只**给两个整数列，比例由前端自己算。三条理由：
//
// 1. **浮点进 JSON 对前端不友好**：`Option<f64>` 序列化出 `0.12345678901234` 这类
//    值，前端要拿它做百分比进度条必须自己再定精度与四舍五入位数；给整数让它做
//    `Math.round(load / capacity * 100)` 反而更可控。
// 2. **`null` 的语义在 wire 上是冗余的**：`capacity === null || capacity <= 0`
//    已经能完整表达「不限」，多一个 `load_ratio: null` 只是把同一个事实说两遍，而
//    两遍口径一旦漂移（比如后端把 `<= 0` 当成 0 而不是 `null`）就会出现
//    「`capacity=0` 但 `load_ratio=null`」这种自相矛盾的响应。
// 3. **写死的比例在 `current_load` 之后立刻过时**：同一个响应体里的两个数字是
//    同一时刻的快照，但前端若把 `load_ratio` 缓存下来而 `current_load` 每次刷新，
//    两者会以不同频率变化 —— 让前端按当次的两个整数现算，永远自洽。

/// 货架列表出参（分页）。字段顺序对齐 Python `schema/shelf.py::ShelfListOut`，
/// 前端翻页需要 limit/offset 回显，故不复用 `shared::response::Page<T>`。
///
/// 2026-09-22 PR4：迁移到 vo/。
#[derive(Debug, Clone, Serialize)]
pub struct ShelfListOut {
    pub items: Vec<ShelfOut>,
    pub total: i64,
    pub limit: i64,
    pub offset: i64,
}

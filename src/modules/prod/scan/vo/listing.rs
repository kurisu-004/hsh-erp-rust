//! prod::scan 报工台**列表行**出参 VO（`GET /scan/pickable` / `GET /scan/held` 共用）
//!
//! 2026-10-10 自 `part::vo::PartListItem`（39 字段）收敛而来。收敛判据是「报工台
//! 三页真正渲染 / 真正拿它发写请求的字段」，而不是「`t_part` 有这一列」。
//!
//! ## 为什么能收敛：行单位是**批次**，不是工单
//!
//! 两条端点的取行 SQL 都从 `t_part_batch` 起（`JOIN t_part p ON p.id = pb.part_id`），
//! 行的锚点是 `batch_id` / `batch_version`。而 `PartListItem` 是 7 个域共用的
//! part 级行 VO（assembly / com::union_list / outsource / part / wx …），它带的那
//! 20 余个字段在这两条端点上恒为占位值：`applicant_name` 写死空串、`customer_id`
//! 写死 `0`、`status` 写死 `"IN_PROCESS"`、`version`（part 级 OCC）写死 `0`、
//! 四个审计字段写死 epoch、`unit_price` / `total_price` 写死 `"0"`、
//! `location` / `holder_name` 恒 null。报工台一个都不读，让它们继续留在 wire 上
//! 只是让 Zod 守门 schema 逐个声明 20 个不会变的常量。
//!
//! ## 逐字段的消费证据
//!
//! | 字段 | 消费方（前端） |
//! |---|---|
//! | `id` / `serial_no` / `name` / `drawing_no` | 三页列表卡标题、`v-for` key |
//! | `quantity` | 卡片数量、worker-scan 入参 |
//! | `is_urgent` | 卡片「加急」tag + `useScanPartsSort` 第一排序键 |
//! | `system_delivery_date` | `DeliveryDateChip` 日期位 + `useScanPartsSort` 硬优先级 |
//! | `planned_delivery_date` | `useScanPartsSort` 第 3 排序键（系统交期为空时的回落） |
//! | `has_process_chain` | `chainAccent.ts` 卡片左边框 |
//! | `chain_state` / `chain_next_process_id` / `chain_next_process_name` / `chain_current_process_name` | 放回页的放回分流（`enterReturnFlow`） |
//! | `batch_id` / `batch_version` | pick-up 的路径参数 + OCC 锚；worker-scan 的消歧入参 |
//! | `location` | `BatchPickerDialog.holderText` 用**键存在性**判断是否渲染 holder 行 |
//!
//! ⚠️ `location` 的值恒为 `null`（这两条端点不做 batch enrichment），保留它只为
//! 保住那个「键在不在」的判据 —— 删键会让报工台卡片静默少掉「未知位置」这一行，
//! 且仓内没有测试能提前发现（前端 fixture 自己显式带上了这个键）。**不要给它加
//! `skip_serializing_if`**。
//!
//! ## 被砍掉的字段（前端同步删 schema 键）
//!
//! `applicant_name` / `request_date` / `customer_id` / `assembly_id` / `status` /
//! `order_no` / `note` / `unit_price` / `total_price` / `version` / `created_at` /
//! `created_by` / `updated_at` / `updated_by` / `deleted_at` / `customer_name` /
//! `l1_customer_name` / `holder_name` / `row_type` / `has_children` / `child_count` /
//! `has_cnc_program` —— 全部恒为占位值且报工台零消费。
//!
//! ⚠️ 其中 **`request_date` 需要前端一并删掉那条 `'1970-01-01' → null` 的字段级
//! transform**（`scanPartRowSchema`）：键不再下发后 Zod 会因 `undefined` 抛错，
//! 整份信封 parse 失败。`planned_delivery_date` 的同名 transform 保留无害
//! （取行 SQL 自 2026-10-04 起投影真实值，transform 只对恰好等于哨兵的串生效）。
//!
//! ## 序列化约定
//!
//! 雪花 id 一律 `serialize_i64` / `serialize_i64_opt` → JSON **string**
//! （19 位 id 在 JS `Number` 下会丢精度）；`quantity` / `batch_version` 是裸数字。
//! 分页信封 `ScanListOut` 的三个计数字段同样是裸 i64 → JSON number。

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

use crate::shared::types::{serialize_i64, serialize_i64_opt};

/// 报工台放回页的「工序链可否免填下一道工序」三值判据。
///
/// 2026-10-10 自 `part::vo::ChainState` 搬来并更名：唯一有值的填充路径
/// （`GET /parts/by-worker`）已迁进本域，而 `part::vo::ChainState` 现在只剩
/// 「`PartListItem` 恒为 `NONE`」这一处用法。两份类型**刻意并存**（详见类型 doc）。
///
/// 序列化形态是大写字符串（`rename_all = "UPPERCASE"` ⇒ `"NONE"` / `"NEXT"` /
/// `"TAIL"`），与本仓「无 DB ENUM、Rust enum 校验」的约定一致。
///
/// 三值**互斥**而不是两个 bool：两个 bool 会出现「可免填 + 是链尾」这类自相矛盾的
/// 组合，前端必须自己排优先级，而排错的后果是静默把工件投到错误工序。
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "UPPERCASE")]
pub enum ScanChainState {
    /// 无链 / 链已软删 / 锚链解析失败 / **当前工序不在链内（位置指针漂移）**
    /// ⇒ 前端弹工序选择框，让用户手填下一道工序。
    #[default]
    None,
    /// 当前工序在链内且**有下一道** ⇒ 前端免填，确认后直接放回。
    Next,
    /// 当前工序是链内**最后一道** ⇒ 前端提示「加工完成后请送检」。
    Tail,
}

impl ScanChainState {
    /// DB 文本（取行 SQL 里 `COALESCE(nx.chain_state, 'NONE')` 的结果）→ 枚举。
    ///
    /// 未知取值降级成 [`ScanChainState::None`] 并 `warn!`。判错的代价不对称：
    /// `NEXT` / `TAIL` 会让写侧**免填**下一道工序（投错工序静默发生），
    /// `NONE` 只是多一次人工选择，故一律往保守方向降。
    pub fn from_db_text(raw: &str) -> Self {
        match raw {
            "NEXT" => Self::Next,
            "TAIL" => Self::Tail,
            "NONE" => Self::None,
            other => {
                tracing::warn!("chain_state 出现未知取值 {other:?}，降级为 NONE");
                Self::None
            }
        }
    }
}

/// 报工台列表行（`/scan/pickable` 与 `/scan/held` 共用，17 字段）。
///
/// ## 哪些字段只有 `held` 侧有值
///
/// `pickable` 侧的行是「架上还没被领走的批次」，它的 `chain_state` / `chain_*`
/// 四字段在取行 SQL 里显式投影成 `NULL::<type>`，转换时取保守默认
/// （`NONE` / `"0"` / `null` / `null`）—— 语义是「让用户手填下一道工序」，与
/// 「不知道」同向。`held` 侧才真正沿
/// `shared::batch::chain::CHAIN_POSITION_LATERAL_SQL` 解析链并填值。
///
/// `batch_id` / `batch_version` / `has_process_chain` / `process_chain_id` 四条
/// 两条端点都填。
#[derive(Debug, Clone, Serialize)]
pub struct ScanListItem {
    /// `t_part.id`（`serialize_i64` → JSON string）。列表卡 `v-for` key。
    #[serde(serialize_with = "serialize_i64")]
    pub id: i64,
    /// `varchar(15)` 可空：手工工单没序列号 ⇒ JSON `null`（**不是空串**）。
    pub serial_no: Option<String>,
    pub name: String,
    pub drawing_no: String,
    /// 取自**批次**（`pb.quantity`），不是 `p.quantity`。
    pub quantity: i32,
    pub is_urgent: bool,
    /// `t_part.planned_delivery_date`（**真实值**，非占位）。
    pub planned_delivery_date: NaiveDate,
    /// `t_part.system_delivery_date` 是可空列（`date` 无 NOT NULL）。
    /// 取件列表恒为 null（无系统交期），放回 / 送检列表才可能有值。
    pub system_delivery_date: Option<NaiveDate>,
    /// `t_part.process_chain_id`：`held` 侧投影真实值，`pickable` 侧恒 null。
    /// 只回答「这个件有没有链」，**不**回答「链上的下一步是谁」——后者读
    /// `chain_state` 四件套。
    #[serde(serialize_with = "serialize_i64_opt")]
    pub process_chain_id: Option<i64>,
    /// 工单已绑工序链**且**批次当前工序能在链内定位 —— 卡片绿色左边框的判据
    /// （常量与理由见 `shared::batch::chain::HAS_PROCESS_CHAIN_EXPR`）。
    pub has_process_chain: bool,
    /// 链位置三值判据。**仅 `held` 填**；`pickable` 侧恒 `NONE`。
    #[serde(default)]
    pub chain_state: ScanChainState,
    /// 下一道工序 id。非可空 + `"0"` 兜底（沿用本仓 `COALESCE(..., 0)` +
    /// `serialize_i64` 的既有口径）：JSON 里恒出现，`"0"` = 无下一道，
    /// 消费侧见到 `"0"` 必须短路。**仅 `held` 填**。
    #[serde(serialize_with = "serialize_i64")]
    pub chain_next_process_id: i64,
    /// 下一道工序名（`t_process.name`）。`NEXT` 时为下一道工序名，
    /// `NONE` / `TAIL` 恒 null。⚠️ `NEXT` 时也可能为 null —— 下一道工序本身
    /// 被软删（取名走 LEFT JOIN + `deleted_at IS NULL`，id 仍有值）。
    /// **消费侧按本字段判空，不要按 `chain_state` 推断。仅 `held` 填。**
    #[serde(default)]
    pub chain_next_process_name: Option<String>,
    /// 当前工序名，链尾提示里点名「该去送检的是哪一道」用。解析不出时 null。
    /// **仅 `held` 填。**
    #[serde(default)]
    pub chain_current_process_name: Option<String>,
    /// 批次雪花 id（`serialize_i64_opt` → JSON string）。两条端点都填：
    /// 取件页用它发 `POST /scan/batches/{batch_id}/pick-up` 的路径参数 + OCC 锚，
    /// 放回 / 送检页用它做 worker-scan 的批次锚（多批次消歧）。
    #[serde(serialize_with = "serialize_i64_opt")]
    pub batch_id: Option<i64>,
    /// 批次乐观锁版本号（`t_part_batch.version`），作 pick-up 的 `version` 入参。
    /// 两条端点都填。
    #[serde(default)]
    pub batch_version: Option<i32>,
    /// 批次位置。**值恒为 `null`** —— 这两条端点不做 batch enrichment。
    /// 保留字段只为保住 `BatchPickerDialog.holderText` 的「键存在性」判据，
    /// 详见模块 doc。**不要加 `skip_serializing_if`。**
    #[serde(default)]
    pub location: Option<String>,
}

/// 报工台两条 list 端点的分页信封。
///
/// `total` / `limit` / `offset` 是裸 i64 ⇒ JSON **number**（与
/// `inspection` 域的 string 计数方向相反）。
///
/// ⚠️ `limit` 缺省 50、clamp 上限 200：不显式传 `limit=200` 的调用方会**静默截断**
/// （报工台三页与 HeldPartsBadge 抽屉都显式传 200）。
#[derive(Debug, Clone, Serialize)]
pub struct ScanListOut {
    pub items: Vec<ScanListItem>,
    pub total: i64,
    pub limit: i64,
    pub offset: i64,
}

#[cfg(test)]
mod tests {
    //! `ScanChainState` 的 DB 文本 → 枚举映射守卫（含序列化字面量）。
    //!
    //! 这条映射的唯一调用点是取行 SQL 里 `COALESCE(nx.chain_state, 'NONE')` 的
    //! 结果，而 SQL 的 `CASE` 只能产出 `NONE` / `NEXT` / `TAIL` 三个字面量。
    //! 一旦两边字面量漂移（改了 SQL 分支名却没改 `from_db_text`），未知取值会
    //! 静默降级成 `NONE` —— 症状是「放回页突然让工人手填工序」，离根因很远。
    //! 故把映射与序列化形态一并锁在单测里。
    use super::{ScanChainState, ScanListItem};
    use chrono::NaiveDate;

    #[test]
    fn from_db_text_maps_all_three_values() {
        assert_eq!(ScanChainState::from_db_text("NEXT"), ScanChainState::Next);
        assert_eq!(ScanChainState::from_db_text("TAIL"), ScanChainState::Tail);
        assert_eq!(ScanChainState::from_db_text("NONE"), ScanChainState::None);
    }

    #[test]
    fn from_db_text_unknown_falls_back_to_none() {
        // 未知取值必须降级成 `NONE`（保守方向：多一次人工选择，而不是免填投错工序）
        for raw in ["WAT", "", "next", "Next", "TAIL "] {
            assert_eq!(
                ScanChainState::from_db_text(raw),
                ScanChainState::None,
                "未知取值 {raw:?} 必须降级为 NONE"
            );
        }
    }

    #[test]
    fn default_is_none() {
        // `#[serde(default)]` 依赖 `Default = None`（取行 SQL 投影 NULL 的端点靠它降级）
        assert_eq!(ScanChainState::default(), ScanChainState::None);
    }

    #[test]
    fn serializes_as_uppercase_strings() {
        // 出参契约的 wire 字面量：前端按 `"NEXT"` / `"TAIL"` / `"NONE"` 判三态
        for (state, want) in [
            (ScanChainState::None, "\"NONE\""),
            (ScanChainState::Next, "\"NEXT\""),
            (ScanChainState::Tail, "\"TAIL\""),
        ] {
            let json = serde_json::to_string(&state).expect("serialize ScanChainState");
            assert_eq!(json, want);
        }
    }

    #[test]
    fn deserializes_from_uppercase_strings() {
        assert_eq!(
            serde_json::from_str::<ScanChainState>("\"NEXT\"").expect("deserialize"),
            ScanChainState::Next
        );
        // 小写字面量必须拒收（`rename_all = "UPPERCASE"` 的大小写是契约的一部分）
        assert!(serde_json::from_str::<ScanChainState>("\"next\"").is_err());
    }

    /// 字段集护栏：后端**加字段**必须同步前端 schema，**删字段**必须先在这里改。
    ///
    /// 这条测试钉的是「键集合逐字相等」而不是各键取值 —— 取值由集成测试守
    /// （`tests/production/scan_listing.rs`）。它防的是最沉默的一种漂移：
    /// 后端悄悄加一个字段，前端 Zod 直接 strip，报工台三页照常渲染、没有任何
    /// 报错，而那个字段永远没人消费。
    #[test]
    fn scan_list_item_keys_are_pinned() {
        let item = ScanListItem {
            id: 1,
            serial_no: None,
            name: "n".into(),
            drawing_no: "d".into(),
            quantity: 1,
            is_urgent: false,
            planned_delivery_date: NaiveDate::from_ymd_opt(2026, 1, 1).unwrap(),
            system_delivery_date: None,
            process_chain_id: None,
            has_process_chain: false,
            chain_state: ScanChainState::None,
            chain_next_process_id: 0,
            chain_next_process_name: None,
            chain_current_process_name: None,
            batch_id: None,
            batch_version: None,
            location: None,
        };
        let json = serde_json::to_value(&item).expect("serialize ScanListItem");
        let mut keys: Vec<&str> = json
            .as_object()
            .expect("ScanListItem 序列化成对象")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "batch_id",
                "batch_version",
                "chain_current_process_name",
                "chain_next_process_id",
                "chain_next_process_name",
                "chain_state",
                "drawing_no",
                "has_process_chain",
                "id",
                "is_urgent",
                "location",
                "name",
                "planned_delivery_date",
                "process_chain_id",
                "quantity",
                "serial_no",
                "system_delivery_date",
            ],
            "ScanListItem 的字段集变了：加字段要同步前端 scanPartRowSchema，\
             删字段要先确认前端无消费方并改这条断言"
        );
        // 雪花 id 走 JSON string，19 位 id 在 JS Number 下会丢精度
        assert_eq!(json["id"], serde_json::json!("1"));
        assert_eq!(json["batch_id"], serde_json::Value::Null);
        assert_eq!(json["chain_next_process_id"], serde_json::json!("0"));
        assert_eq!(json["quantity"], serde_json::json!(1));
        assert_eq!(json["batch_version"], serde_json::Value::Null);
        assert_eq!(json["location"], serde_json::Value::Null);
    }
}

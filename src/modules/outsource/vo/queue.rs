//! outsource 域外协看板两个只读端点的出参（2026-10-09 新增）
//!
//! - `GET /api/v2/outsource-queue/snapshot` —— 工序序列板
//! - `GET /api/v2/outsource-queue/processes/{process_id}` —— 单工序板（左列候选 +
//!   右列公司列，公司列内联在途批次）
//!
//! 取代 `GET /outsource-pool/{counts,state,{process_id}}` 三条旧读（硬切无 alias）：
//! 旧路径下打开一道工序的板要发 1（工序详情）+ M（每家公司一次 state）= M + 1 个
//! HTTP 请求；在途批次的卡片字段本来就是齐的，内联进公司列后恒定 1 个请求。
//!
//! ## 字段取舍：按前端实际消费收敛
//!
//! 候选卡（`OutsourceQueueCandidate`）相对被取代的 `OutsourcePoolCandidate`
//! （22 字段 → 本文件 25 字段）做了 3 删 1 拆 5 加：
//!
//! | 动作 | 字段 | 原因 |
//! |---|---|---|
//! | 删 | `status_label` | 恒为 `"sendable"`，与 `can_send` 重复 |
//! | 删 | `source_status` | 候选谓词已保证只有 `PENDING` / `IN_PROCESS`+`PRODUCTION_SHELF` 两态，**两态都可发**，不能用于任何判定 |
//! | 删 | `batch_quantity` | 与 `quantity` 同值重复（行 = 批次） |
//! | 拆 | `customer_path` → `customer_name` + `parent_customer_name` | 前端自己拼 L1 / L2；SQL 层本来就已 SELECT 了这两列，是旧 VO 层用 `service::join_customer_path` 合并掉的 |
//! | 加 | `system_delivery_date` | 卡片 body 第 3 行的**唯一日期**；缺了前端卡片恒显「—」 |
//! | 加 | `has_cnc_program` | 卡片角标；与 `prod::queue` 候选卡的 EXISTS 逐字同款 |
//! | 加 | `applicant_name` | 卡片申请人名；必须走 `LEFT JOIN LATERAL`（理由见 `board/repo.rs` 的在途 SQL 注释与 `repo/sql.rs::SENDABLE_INNER_X_SQL`） |
//! | 加 | `note` | 卡片备注（`t_part.note`） |
//! | 加 | `shelf_id` | **承重字段**，见下 |
//!
//! `shelf_id` 为什么是承重字段：批次移动的入参 `from.shelf_id` 必须**等于批次真实
//! 所在货架**（`t_part_batch.current_holder_id`），填错被移动写端点按
//! `20122 BIZ_BATCH_LOCATION_MISMATCH` 拒收。候选池跨货架，前端不能用「用户当前
//! 激活货架」凑（激活货架对 MANAGER / CLERK / INSPECTOR 恒为空）。类型是非可空的
//! `String`：`PENDING` 且未上架的批次 `current_holder_id` 为 NULL，此处序列化成
//! **空串**而不是 `null`（`null` 会让前端的必填字符串校验炸在整页渲染上）；这类行
//! 本来就发不出去（不在生产架上），传空串给 `from.shelf_id` 会被写端点拒收。
//!
//! ## 雪花 ID 序列化口径
//!
//! **本文件全部出参的 i64 都在装配处 `.to_string()`**（不 derive 序列化助手）——
//! 照 `prod::queue::vo::board` 与 `dashboard` 域的做法：聚合出参一次序列化几十上百行，
//! 每个字段挂 serde 属性等于每行多走一层 `serialize_with` 间接调用。唯一的例外是
//! `company_options` 里的 `OutsourceCompanyOption`（与 `GET /outsource-sendable` 共用
//! 同一个类型，故保留 `serialize_i64`），它每次请求至多十几行。

use serde::Serialize;

use super::sendable::OutsourceCompanyOption;

// ===========================================================================
//  GET /api/v2/outsource-queue/snapshot
// ===========================================================================

/// `GET /api/v2/outsource-queue/snapshot` 顶层出参。
///
/// **不含 `total`**（可由 `sendable_total + in_flight_total` 相加得到）—— 与
/// `prod::queue` 的 `QueueBoardSnapshot` 对齐：两个 total 都在，前端再给一个
/// 只会多一个必须与二者对得上的数字。
#[derive(Debug, Clone, Serialize)]
pub struct OutsourceQueueSnapshot {
    /// 只含 `sendable_count + in_flight_count > 0` 的工序，按 `process_id ASC`
    /// 稳定排序（前端 tab 序不随数据量抖动）。
    pub processes: Vec<OutsourceQueueProcess>,
    /// 候选侧（可发）批次数总和。
    pub sendable_total: i64,
    /// 在途侧（在外协公司）批次数总和。
    pub in_flight_total: i64,
    /// RFC3339 带 `+08:00` 偏移（`infra::clock::now_shanghai_iso()`）。
    pub ts: String,
}

/// 序列板上的一道外协工序。
#[derive(Debug, Clone, Serialize)]
pub struct OutsourceQueueProcess {
    /// `t_part_batch.current_process_id`。**装配处 `.to_string()`**，故类型是
    /// `String` 而不是 i64 + 序列化助手（理由见文件头）。
    pub process_id: String,
    pub process_code: String,
    pub process_name: String,
    /// `t_process.color`，9 字符 `#RRGGBBAA`（含 alpha），未设置时 `null`。
    ///
    /// **刻意不收窄成正则 / 长度校验**：前端直接把它喂给 CSS `border-left-color`，
    /// 后端做格式收窄只会制造「DB 里有值但接口给 null」的静默降级，而坏颜色在前端
    /// 的表现是那一条边框不显示，不会造成数据错误。
    pub color: Option<String>,
    /// `t_process.category`。候选侧谓词 `pr.category = 'OUTSOURCE'` 保证绝大多数
    /// 行恒为 `OUTSOURCE`，但仍取 DB 真值而不是写死字面量 —— 万一某批次停在
    /// 外协公司而 `current_process_id` 指向一道厂内工序（异常数据），写死会让前端把
    /// 一道 INHOUSE 工序误标成外协工序。工序已软删导致查不到元数据时按在途侧语义
    /// 兜底为 `"OUTSOURCE"`。
    pub category: String,
    /// 该工序的可发候选批次数。
    pub sendable_count: i64,
    /// 该工序在外协公司的批次数。
    pub in_flight_count: i64,
}

// ===========================================================================
//  GET /api/v2/outsource-queue/processes/{process_id}
// ===========================================================================

/// `GET /api/v2/outsource-queue/processes/{process_id}` 顶层出参。
#[derive(Debug, Clone, Serialize)]
pub struct OutsourceQueueProcessDetail {
    /// 工序元数据（**无 `category`**：单工序详情不展示类别，照
    /// `prod::queue::vo::board::QueueProcessMeta`）。
    pub process: OutsourceQueueProcessMeta,
    /// 右列：该工序映射的全部**活跃**外协公司（`held_count = 0` 的空列也在内 ——
    /// 前端要渲染空公司列当拖拽目标）。
    pub companies: Vec<OutsourceQueueCompany>,
    /// 左列：该工序的可发候选批次。
    pub items: Vec<OutsourceQueueCandidate>,
    /// `items.len()`（**服务层从 Vec 算**，不分页，与 `items` 恒等）。
    pub total: i64,
    pub ts: String,
}

/// 工序元数据（`t_process` 单行）。
#[derive(Debug, Clone, Serialize)]
pub struct OutsourceQueueProcessMeta {
    pub process_id: String,
    pub process_code: String,
    pub process_name: String,
    pub color: Option<String>,
}

/// 看板右列的一列（一家外协公司 + 它在该工序的在外协批次）。
#[derive(Debug, Clone, Serialize)]
pub struct OutsourceQueueCompany {
    pub company_id: String,
    pub name: String,
    /// **`== held_batches.len()`**（服务层保证，单测
    /// `held_count_matches_held_batches_len` 钉住）。
    ///
    /// 之所以要求恒等：前端按 `held_count` 渲染列头徽标、按 `held_batches` 渲染
    /// 卡片，两者不一致时表现为「徽标写 3、列里只有 2 张卡」，运营无法判断是漏件还是
    /// 显示 bug，而这类不一致是**静默**的。批次数由在途批次一次取齐后在内存里
    /// `HashMap` 分组得到，**不依赖 SQL 的 `COUNT`**（那条 SQL 与明细 SQL 的谓词
    /// 一旦漂移就会分叉）。
    pub held_count: i64,
    /// 该公司手上的在外协批次。**内联**在此，不再需要逐公司拉一次 `/state`
    /// （那是本设计要消灭的 N+1）。
    pub held_batches: Vec<OutsourceQueueHeldBatch>,
}

/// 看板左列的一个候选批次卡片（25 字段，取舍见文件头表格）。
#[derive(Debug, Clone, Serialize)]
pub struct OutsourceQueueCandidate {
    /// `t_part_batch.version`（批次级 OCC 锚）。前端调移动写端点时**原样回传**；
    /// 漏传 / 过期返 `40901`。
    pub version: i32,
    /// `"APPROVAL"`（该外协工序 `requires_approval = true`，本行已命中一条已批准
    /// 报价）/ `"DIRECT"`（`requires_approval = false`，免审批直发）。
    pub send_mode: String,
    pub batch_id: String,
    pub part_id: String,
    pub batch_no: i32,
    /// 批次当前余量（`t_part_batch.quantity`）。行 = 批次，故与旧 VO 的
    /// `batch_quantity` 同值 —— 后者已删。
    pub quantity: i32,
    pub part_serial_no: Option<String>,
    pub part_drawing_no: Option<String>,
    pub part_name: Option<String>,
    pub planned_delivery_date: Option<String>,
    /// `t_part.system_delivery_date`（`YYYY-MM-DD`）—— 卡片 body 第 3 行的唯一日期。
    pub system_delivery_date: Option<String>,
    pub is_urgent: bool,
    /// L2 叶子客户名（`t_customer`，`p.customer_id`）。
    pub customer_name: Option<String>,
    /// L1 一级集团名（`t_customer`，`c.parent_id`）。
    pub parent_customer_name: Option<String>,
    /// `t_applicant.name`（非 FK 字符串匹配；`LEFT JOIN LATERAL` 防同名扇行）。
    pub applicant_name: Option<String>,
    /// `t_part.note`（DB 无 batch 级 remark 字段）。
    pub note: Option<String>,
    /// 批次所在货架 code。`PENDING` 且未上架的批次没有 holder，故为 `null`。
    pub shelf_code: Option<String>,
    /// **承重字段**，见文件头。`PENDING` 未上架批次为 `""`。
    pub shelf_id: String,
    /// APPROVAL 有值（取报价的公司）/ DIRECT `null`。
    pub outsource_company_id: Option<String>,
    pub outsource_company_name: Option<String>,
    /// 命中的 APPROVED 报价 id。APPROVAL 有值 / DIRECT `null`。
    pub quote_id: Option<String>,
    /// DIRECT 列出该公司工序映射的全部活跃公司；APPROVAL 恒为 `[]`。
    /// DIRECT 且该工序未映射任何活跃公司时为 `[]` —— **该行仍返回**
    /// （`can_send = false` 把它置灰），不要在 SQL 里滤掉。
    pub company_options: Vec<OutsourceCompanyOption>,
    /// APPROVAL 报价的 Decimal 字符串 / DIRECT `null`。
    pub price: Option<String>,
    /// `send_mode == "APPROVAL" || !company_options.is_empty()`。
    /// 服务端算好给前端，避免每个视图各写一遍判定。
    pub can_send: bool,
    /// 有 G_CODE 程序（`EXISTS (SELECT 1 FROM t_part_file …)`），与 `prod::queue`
    /// 候选卡的同名 EXISTS 逐字一致。
    pub has_cnc_program: bool,
}

/// 看板右列里的一家公司在途批次卡片。
///
/// 相对被取代的 `OutsourceHeldBatchItem`（21 字段）**只加** `has_cnc_program`
/// （卡片角标），其余逐字不变 —— 在途卡片的字段集本来就是齐的（它比候选卡片多
/// `sent_at` / `price` / 下一道工序三个字段），本轮不动。
#[derive(Debug, Clone, Serialize)]
pub struct OutsourceQueueHeldBatch {
    pub batch_id: String,
    pub part_id: String,
    pub batch_no: i32,
    /// **当前余量**（`t_part_batch.quantity`），不是 `shipment.quantity` ——
    /// 前端拿它做「部分接收」输入框的 max 值。
    pub quantity: i32,
    pub serial_no: Option<String>,
    pub drawing_no: String,
    pub name: String,
    pub system_delivery_date: Option<chrono::NaiveDate>,
    pub planned_delivery_date: Option<chrono::NaiveDate>,
    pub is_urgent: bool,
    /// L2 叶子客户名。
    pub customer_name: Option<String>,
    /// L1 一级集团名。
    pub parent_customer_name: Option<String>,
    pub applicant_name: Option<String>,
    /// 恒为 `"OUTSOURCE_COMPANY"`（在途谓词保证）。
    pub location: String,
    pub note: Option<String>,
    /// `t_part_batch.version` —— 前端拿它当移动写端点的 `version`（OCC 锚）。
    pub version: i32,
    /// 有 G_CODE 程序（与候选卡片同一 EXISTS）。
    pub has_cnc_program: bool,
    /// `t_outsource_shipment.sent_at`。
    ///
    /// 正常流**恒有值**：`uq_t_outsource_shipment_open_batch` 保证一个批次最多一张
    /// 开口 shipment，而移动发送方向在同一事务里 INSERT 它。之所以仍声明成可空，
    /// 是因为驱动 SQL 按契约用 `LEFT JOIN`（防御历史脏数据 / 手工改库），不想给一个
    /// 可能不存在的值编一个假的替身（`price` 同理）。
    pub sent_at: Option<chrono::NaiveDateTime>,
    /// `t_outsource_shipment.unit_price` 的 Decimal 字符串 / `null`。
    ///
    /// **刻意不用空串兜底** —— 空串会被前端当成「0 元 / 格式错误的数」渲染，比 `null`
    /// 难排查。
    pub price: Option<String>,
    /// 回收后批次的下一道工序 id；**推不出时为 `"0"`**（0 兜底口径，非 nullable）。
    /// 前端据此判断要不要弹对话框让用户手填工序（`chain_resolvable = false`）。
    pub receive_next_process_id: String,
    pub receive_next_process_name: Option<String>,
    /// `receive_next_process_id != "0"`。
    ///
    /// 业务含义：`true` ⇒ 工序链已知，前端可免填 `to.next_process_id`；`false` ⇒
    /// 工序链缺失或指针漂移，前端必须让用户手填（否则写端点返 `20702`）。
    pub chain_resolvable: bool,
}

#[cfg(test)]
mod tests {
    //! 序列化口径守卫。
    //!
    //! 这些不变量一旦破掉，前端 Zod 的 `z.string()` 守门会以
    //! 「`expected string, received number`」的形式炸在整页渲染上，而症状离根因
    //! 很远，故锁在单测里。
    use super::*;

    fn held_batch() -> OutsourceQueueHeldBatch {
        OutsourceQueueHeldBatch {
            batch_id: 9_000_000_000_000_000_501i64.to_string(),
            part_id: 9_000_000_000_000_000_502i64.to_string(),
            batch_no: 1,
            quantity: 5,
            serial_no: None,
            drawing_no: "DWG-1".into(),
            name: "NAME-1".into(),
            system_delivery_date: None,
            planned_delivery_date: None,
            is_urgent: false,
            customer_name: None,
            parent_customer_name: None,
            applicant_name: None,
            location: "OUTSOURCE_COMPANY".into(),
            note: None,
            version: 3,
            has_cnc_program: false,
            sent_at: None,
            price: Some("12.50".into()),
            receive_next_process_id: "0".into(),
            receive_next_process_name: None,
            chain_resolvable: false,
        }
    }

    /// 在途卡片的雪花字段全是 JSON 字符串（装配处已 `.to_string()`）。
    #[test]
    fn held_batch_snowflake_ids_are_strings() {
        let v = serde_json::to_value(held_batch()).unwrap();
        assert!(v["batch_id"].is_string(), "{v}");
        assert!(v["part_id"].is_string(), "{v}");
    }

    /// 无下一 step ⇒ `receive_next_process_id` 是字符串 `"0"`（不是数字 0、不是 null）。
    #[test]
    fn receive_next_process_id_zero_serializes_as_string() {
        let v = serde_json::to_value(held_batch()).unwrap();
        assert_eq!(v["receive_next_process_id"], serde_json::json!("0"));
    }

    /// 有下一 step ⇒ 十进制字符串。
    #[test]
    fn receive_next_process_id_serializes_as_string() {
        let mut row = held_batch();
        row.receive_next_process_id = 9_000_000_000_000_000_503i64.to_string();
        row.chain_resolvable = true;
        let v = serde_json::to_value(row).unwrap();
        assert_eq!(
            v["receive_next_process_id"],
            serde_json::json!("9000000000000000503")
        );
        assert_eq!(v["chain_resolvable"], serde_json::json!(true));
    }

    /// 板聚合 VO 的工序 id 在装配处已 `.to_string()`，序列化后仍是字符串。
    #[test]
    fn snapshot_process_id_is_string() {
        let v = serde_json::to_value(OutsourceQueueProcess {
            process_id: 9_000_000_000_000_000_501i64.to_string(),
            process_code: "P1".into(),
            process_name: "外协一".into(),
            color: None,
            category: "OUTSOURCE".into(),
            sendable_count: 3,
            in_flight_count: 1,
        })
        .unwrap();
        assert!(v["process_id"].is_string(), "{v}");
        assert_eq!(v["category"], serde_json::json!("OUTSOURCE"));
    }
}

//! part 域：工种维度只读端点
//!
//! - `list_by_work_type` —— `GET /parts/by-work-type/{work_type_id}`
//! - `list_pickable_by_work_type` —— `GET /parts/pickable-by-work-type/{work_type_id}`
//! - `list_by_worker` —— `GET /parts/by-worker/{worker_id}`
//!
//! 2026-10-02：手动 `pick_up`（B 方案兜底）随批次用例迁往
//! `crate::modules::prod::batch::service::pickup`，三条 list 端点留在 part 域。

use chrono::{NaiveDate, NaiveDateTime};
use rust_decimal::Decimal;

use crate::auth::rbac::{CurrentUser, Role};
use crate::modules::part::repo::PartRepoTrait;
use crate::modules::part::vo::{ChainState, PartListItem, PartListOut};
use crate::shared::error::AppError;

use super::super::PartService;
use crate::modules::part::dto_crud::{ByWorkTypeQuery, ByWorkerQuery};

/// 1970-01-01：未投影的 `date` 类占位值。
///
/// ⚠️ `planned_delivery_date` 自 2026-10-04 起**投影真实值**（见
/// [`WorkTypeListRow`]），本常量只剩 `request_date` 一个消费方；而 `request_date`
/// 三条端点都没投影，恒为占位。前端没有消费 `request_date`，故保持占位不动。
const PLACEHOLDER_DATE: NaiveDate = NaiveDate::from_ymd_opt(1970, 1, 1).unwrap();

/// Unix epoch：未投影的 `timestamp` 类占位值。
const PLACEHOLDER_TS: NaiveDateTime = NaiveDateTime::from_timestamp_opt(0, 0).unwrap();

// ===========================================================================
//  共享取行投影（2026-10-04 新增）
// ===========================================================================

/// 三条工种 / 工人维度 list 端点共用的取行投影。
///
/// ## 为什么不是 `TPart` 字面量
/// 2026-10-04 之前三处各手抄一份 20 字段的 `TPart { ... }` 字面量再交给
/// `PartListItem::from`。两个后果：
/// 1. **占位值伪装成业务值** —— 取行 SQL 只投影 `p.id` / `p.serial_no` /
///    `p.drawing_no` 三列，其余全靠字面量填，于是 `name` 填成图号、
///    `is_urgent` 填 `false`、`system_delivery_date` 填 `None`、
///    `planned_delivery_date` 填 1970-01-01。报工台三页的「加急」tag 与交期 chip
///    因此永不渲染，而 `ORDER BY p.is_urgent DESC, p.planned_delivery_date ASC`
///    排的却是 **DB 真实列** ⇒ 列表已按加急排好、工件上看不出任何标记。
/// 2. **漏字段不报错** —— `TPart` 30+ 字段，加字段时三份字面量要手改三处。
///
/// 本 struct 把「SQL 投影了什么」收敛成**一处**事实：字段名即 SQL 别名
/// （`#[derive(sqlx::FromRow)]` 按列名匹配），三处取行 SQL 必须逐字投影每一个字段。
/// ⚠️ 这是**运行期**校验（`sqlx::query_as` 而非 `query_as!` 宏，宏才有编译期校验）：
/// 少投影一列 → 运行时报 missing column；**多投影一列 → 静默忽略**（不报错、不取值）。
/// 端点不消费的字段在 SQL 里显式投影成 `NULL::<type> AS <字段名>`，把「本端点不填」
/// 写进 SQL 而不是靠 struct 缺省 —— 这样「不填」与「漏填」在 SQL 文本上就长得不一样。
///
/// ## 字段填充口径
/// - part 侧 `id` / `serial_no` / `name` / `drawing_no` / `is_urgent` /
///   `planned_delivery_date` / `system_delivery_date`：三条端点都投影**真实值**。
/// - `quantity`：取自**批次**（`b.quantity`）而非 part。
/// - `process_chain_id`：仅 `by-worker` 投影真实值；另两条投影 `NULL`（口径见
///   `vo/part.rs` 字段 doc）。
/// - `batch_id` / `batch_version`：仅 `pickable-by-work-type` / `by-worker` 填
///   （本 VO 的行单位是批次；`by-work-type` 投影 `NULL`）。
/// - 链四字段：仅 `by-worker` 填（链位置是**批次级**事实，part 级投影无从推导）。
///
/// ## 刻意没有的字段
/// `next_process_id` **不在本 struct 里，也不在 `PartListItem` 里** —— 列表响应
/// 从不暴露该字段（2026-09-27 用户决策范围 C，`vo/part.rs` 的
/// `From<TPart> for PartListItem` 因此也不复制它）。旧代码在 `TPart` 字面量里写
/// `next_process_id: None` 只是给一个不会被读的字段赋值。现在这条不变量由
/// **类型系统 + 序列化守卫**双重保证：struct 与 `PartListItem` 都没有该字段
/// （新增即编译失败），且 `tests/part/pickable_by_work_type.rs` 断言响应 JSON
/// 不含 `next_process_id` 键。
#[derive(Debug, sqlx::FromRow)]
struct WorkTypeListRow {
    // ---- part 侧（三条端点均投影真实值）----
    id: i64,
    serial_no: Option<String>,
    name: String,
    drawing_no: String,
    is_urgent: bool,
    planned_delivery_date: NaiveDate,
    /// `t_part.system_delivery_date` 是**可空**列（`date` 无 NOT NULL）。
    system_delivery_date: Option<NaiveDate>,
    // ---- 批次侧 ----
    /// 取自 `b.quantity`（批次数量），不是 `p.quantity`。
    quantity: i32,
    process_chain_id: Option<i64>,
    batch_id: Option<i64>,
    batch_version: Option<i32>,
    // ---- 工序链派生（仅 by-worker 填）----
    /// 取行 SQL 侧已 `COALESCE(..., 'NONE')`，故 `by-worker` 恒为 `Some`；
    /// 另两条端点投影 `NULL::text` ⇒ `None`。
    chain_state: Option<String>,
    /// 取行 SQL 侧已 `COALESCE(..., 0)`；另两条端点投影 `NULL::bigint`。
    chain_next_process_id: Option<i64>,
    chain_next_process_name: Option<String>,
    chain_current_process_name: Option<String>,
}

impl WorkTypeListRow {
    /// 投影行 → `PartListItem`。
    ///
    /// 这是全仓**唯一**构造这三端点出参的地方，用**穷尽 struct 字面量**而非
    /// `From<TPart>`：`PartListItem` 加字段时这里编译失败（`From` 派生路径同样会
    /// 失败，但字面量让「哪些字段是投影、哪些是占位」一眼可辨）。
    ///
    /// 仍为占位值的字段（前端均无消费方，2026-10-04 有意不动）：`applicant_name` /
    /// `request_date` / `customer_id` / `status` / `order_no` / `note` /
    /// `unit_price` / `total_price` / `version` / `created_at` / `created_by` /
    /// `updated_at` / `updated_by` / `deleted_at` / `assembly_id` /
    /// `customer_name` / `l1_customer_name` / `location` / `holder_name` /
    /// `delivered_quantity`。口径与改造前逐字一致（`From<TPart>` 也是这么填的）。
    fn into_list_item(self) -> PartListItem {
        PartListItem {
            id: self.id,
            serial_no: self.serial_no,
            // ⚠️ 2026-10-04 起是 `t_part.name` 真实值；改造前是 `drawing_no` 的副本。
            name: self.name,
            drawing_no: self.drawing_no,
            is_urgent: self.is_urgent,
            planned_delivery_date: self.planned_delivery_date,
            system_delivery_date: self.system_delivery_date,
            quantity: self.quantity,
            process_chain_id: self.process_chain_id,
            batch_id: self.batch_id,
            batch_version: self.batch_version,
            // 未投影链字段的两条端点（`None`）取保守默认 `NONE` / `"0"`：
            // `NONE` 语义是「让用户手填下一道工序」，与「不知道」同向。
            chain_state: self
                .chain_state
                .as_deref()
                .map_or(ChainState::None, ChainState::from_db_text),
            chain_next_process_id: self.chain_next_process_id.unwrap_or(0),
            chain_next_process_name: self.chain_next_process_name,
            chain_current_process_name: self.chain_current_process_name,
            // ---- 以下为占位值（前端无消费方，见方法 doc）----
            applicant_name: String::new(),
            request_date: PLACEHOLDER_DATE,
            customer_id: 0,
            status: "IN_PROCESS".to_string(),
            order_no: None,
            note: None,
            unit_price: Decimal::ZERO,
            total_price: Decimal::ZERO,
            // ⚠️ 本 VO 的 `version` 是 **part 级**（`t_part.version`），而三条端点的
            // 取行 SQL 都没投影 `p.version` —— 恒 0 是**有意的占位**，不是漏取值。
            // 批次乐观锁版本走 `PartListItem::batch_version`（取自 `b.version`）；
            // 下一个读者请勿把本字段当批次版本用。
            version: 0,
            created_at: PLACEHOLDER_TS,
            created_by: None,
            updated_at: PLACEHOLDER_TS,
            updated_by: None,
            deleted_at: None,
            assembly_id: None,
            customer_name: None,
            l1_customer_name: None,
            location: None,
            holder_name: None,
            row_type: Some("PART".to_string()),
            has_children: false,
            child_count: None,
            has_cnc_program: false,
            delivered_quantity: None,
        }
    }
}

/// `pickable-by-work-type` 的货架 scope 谓词入参（2026-10-04 新增）。
///
/// 语义**逐条**对齐 `auth::rbac::CurrentUser::can_access_shelf`：
/// ```text
/// shelf_wildcard || shelf_ids.contains(&shelf_id) || has_role(Role::Manager)
/// ```
/// 即「谓词对某货架恒真」的两类账号（wildcard / Manager）返回 `None`（SQL 侧不加
/// 任何谓词），其余账号返回 scope 数组走 `sh.id = ANY($n)`。
///
/// ⚠️ `CurrentUser.shelf_ids` 是 `Vec<i64>`（JSON 层才是 string 序列）。
/// ⚠️ 空 scope 返回 `Some(vec![])` 而**不是** `None`：`ANY('{}')` 对任何货架都
/// 假，与 `can_access_shelf` 对任何货架都返 false 同形。若把空数组误判成
/// 「无限制」，未绑架的 SHELF_ACCOUNT 会看到全厂。
/// 空 scope 在生产里的真实成因：`iam::service::session::resolve_roles_and_scope`
/// 会把绑到「已停用 / 已软删 / 不存在」货架的 `scope_id` 过滤掉（登录时求值），
/// 于是这类账号登录后 `shelf_ids == []` 且 `shelf_wildcard == false`。
///
/// ⚠️ **Clerk / Inspector 的行为变更**：本端点的角色白名单含 Clerk / Inspector，
/// 而这两类角色按惯例不配 `t_user_role` 的 SHELF_ACCOUNT 行 ⇒ `shelf_ids` 为空
/// 且 `shelf_wildcard = false` ⇒ 收口后**返回空列表**。这是「与
/// `can_access_shelf` 对齐」的必然结果（写侧 `worker-scan` 的
/// `require_any_role(&[Manager, ShelfAccount])` 只放行 Manager/ShelfAccount，故
/// 对这两类账号不存在「列表给出但提交被拒」的落差），但对读侧是行为变更。
/// ⚠️ 若业务上要放开，唯一经产品 API 可达的办法是给它们**逐架**配
/// `scope_id` 的 SHELF_ACCOUNT 行（`POST /iam/users/{id}/roles`）：wildcard
/// （`scope_id IS NULL`）被 `iam::service::account::validate_role_scope` 硬拒，
/// 属只有 fixture / 直插 SQL 能造出的状态。
/// 契约见 `docs/api/parts/lifecycle.md` 的
/// `GET /api/v2/parts/pickable-by-work-type/{work_type_id}` 节。
fn pickable_shelf_scope(current: &CurrentUser) -> Option<Vec<i64>> {
    if current.shelf_wildcard || current.has_role(Role::Manager) {
        None
    } else {
        Some(current.shelf_ids.clone())
    }
}

impl PartService {
    // ===== Phase 2 (2026-09-13) — 领取链路 (B 方案：手动 pick-up 兜底) =====

    /// `GET /parts/by-work-type/{work_type_id}`：可领件（按工种过滤）。
    ///
    /// 实现：worker.work_type_id = $1 → t_part_batch.current_holder_id = worker.id，
    /// 且 batch.location='WORKER'。简化：直接按 worker 反查（每工种有多个 worker）。
    pub async fn list_by_work_type<R: PartRepoTrait>(
        mut repo: R,
        work_type_id: i64,
        query: &ByWorkTypeQuery,
        current: &CurrentUser,
    ) -> Result<PartListOut, AppError> {
        current.require_any_role(&[
            Role::Manager,
            Role::Clerk,
            Role::Inspector,
            Role::ShelfAccount,
        ])?;
        let limit = query.limit.unwrap_or(50).clamp(1, 200);
        let offset = query.offset.unwrap_or(0).max(0);
        // 直接列出该工种所有 worker 当前持有的件（IN_PROCESS + location=WORKER）
        //
        // ⚠️ `p.serial_no` 按 `Option<String>` 收（2026-10-03 review 第 1 轮
        // Major-2 修）：`t_part.serial_no` 是 nullable（手工工单无序列号，见
        // baseline `serial_no character varying(15)` 无 NOT NULL），原先按
        // `String` 解码 → 遇到任一 `serial_no IS NULL` 的 part 就整页 500
        // （`unexpected null; try decoding as an Option`）。手工工单是常态，
        // 故这是真会触发的路径。同一缺陷的第三处见 `list_pickable_by_work_type`
        // （2026-10-03 已修）与本文件 `list_by_worker`（同批已修）。
        //
        // 2026-10-04 补投影 part 侧 4 个真实列（`name` / `is_urgent` /
        // `system_delivery_date` / `planned_delivery_date`）：此前 `name` 填的是
        // 图号副本、`is_urgent` 恒 false、两个交期是占位值。本端点前端无消费方
        // （`pickable-by-work-type` 才是报工台的列表源），补投影只为三个端点口径
        // 一致，避免下次又各自漂移。
        //
        // 2026-10-04 删两列死投影（`b.id AS bid` / `w.name AS worker_name`）：原
        // 代码取出来只 `let _ = bid;` / `let _ = worker_name;` 丢弃，从未进过出参。
        // `FromRow` 按列名匹配，**多余列会被静默忽略**（少投影才报 missing column），
        // 所以删这两列是「少取两列 + 列集与 [`WorkTypeListRow`] 字段集一一对应」的
        // 整洁性取舍，不是正确性修复 —— 保留它们同样能跑通。
        // 端点不填的字段（批次锚点 / 链派生）则显式投影成 `NULL::<type> AS <字段名>`：
        // 把「本端点不填」写进 SQL，而不是靠 struct 缺省或 `#[sqlx(default)]`
        // （后者会**静默**取默认值，正是本次要消灭的那类「填了但没人知道」的口径）。
        let rows: Vec<WorkTypeListRow> = sqlx::query_as(
            "SELECT p.id, p.serial_no, p.name, p.drawing_no, p.is_urgent, \
                    p.system_delivery_date, p.planned_delivery_date, b.quantity, \
                    NULL::bigint AS batch_id, NULL::integer AS batch_version, \
                    NULL::bigint AS process_chain_id, NULL::text AS chain_state, \
                    NULL::bigint AS chain_next_process_id, \
                    NULL::text AS chain_next_process_name, \
                    NULL::text AS chain_current_process_name \
             FROM t_part_batch b \
             JOIN t_part p ON p.id = b.part_id \
             JOIN t_worker w ON w.id = b.current_holder_id \
             WHERE b.deleted_at IS NULL AND p.deleted_at IS NULL \
               AND w.deleted_at IS NULL AND w.is_active = true \
               AND w.work_type_id = $1 \
               AND b.status = 'IN_PROCESS' AND b.location = 'WORKER' \
             ORDER BY b.id DESC LIMIT $2 OFFSET $3",
        )
        .bind(work_type_id)
        .bind(limit)
        .bind(offset)
        .fetch_all(repo.conn_mut())
        .await?;
        let items: Vec<PartListItem> = rows
            .into_iter()
            .map(WorkTypeListRow::into_list_item)
            .collect();
        let total: i64 = sqlx::query_scalar(
            "SELECT COUNT(*)::bigint FROM t_part_batch b \
             JOIN t_worker w ON w.id = b.current_holder_id \
             WHERE b.deleted_at IS NULL AND w.deleted_at IS NULL \
               AND w.is_active = true AND w.work_type_id = $1 \
               AND b.status = 'IN_PROCESS' AND b.location = 'WORKER'",
        )
        .bind(work_type_id)
        .fetch_one(repo.conn_mut())
        .await?;
        Ok(PartListOut {
            items,
            total,
            limit,
            offset,
        })
    }

    /// `GET /parts/pickable-by-work-type/{work_type_id}`：可领取件（货架上、绑了对应工序）。
    pub async fn list_pickable_by_work_type<R: PartRepoTrait>(
        mut repo: R,
        work_type_id: i64,
        query: &ByWorkTypeQuery,
        current: &CurrentUser,
    ) -> Result<PartListOut, AppError> {
        current.require_any_role(&[
            Role::Manager,
            Role::Clerk,
            Role::Inspector,
            Role::ShelfAccount,
        ])?;
        let limit = query.limit.unwrap_or(50).clamp(1, 200);
        let offset = query.offset.unwrap_or(0).max(0);
        let shelf_filter = query.shelf_id;
        // 2026-10-04 scope 收口：客户端可控的 `?shelf_id=` 此前是本端点**唯一**的
        // 货架输入，不与用户 scope 求交、不传时谓词恒真 ⇒ 绑了架 A 的 SHELF_ACCOUNT
        // 能看到全厂所有 PRODUCTION 架上该工种可领的批次（信息泄露）。现按
        // `pickable_shelf_scope` 追加 `$5` 谓词，两条 SQL 完全同形。
        // `?shelf_id=` 参数本身保留不动：收口后它的语义从「不传即全给」变成
        // 「收口后的进一步收窄」，只能更严不能更松。
        let shelf_scope = pickable_shelf_scope(current);
        // 列：t_part_batch WHERE location=PRODUCTION_SHELF AND batch.current_process_id IN (工种→工序映射)
        //
        // 2026-10-03 补投影 `b.id` / `b.version`：本端点的行本来就是「批次行」，
        // 而出参 VO 只有 part 级字段，扫码台「领料」拿不到批次 id 就发不出写请求。
        // 两者填进 `PartListItem::batch_id` / `batch_version`（仅本端点填，
        // 其它复用该 VO 的端点恒 null，见 vo/part.rs 字段 doc）。
        //
        // 2026-09-30 修复（migration 004）：原写法是
        // `JOIN t_process_chain_step s ON s.id = b.current_process_step_id
        //  JOIN t_work_type_process wtp ON wtp.process_id = s.process_id` ——
        // 与此前 worker_pool 池查询同款的 INNER JOIN 盲区：batch 的
        // current_process_step_id 为 NULL（新下发批次的常态，无工序链工单恒为
        // NULL）时匹配不到任何 step 行，批次会从「可领取」列表里**整条消失**。
        // 改直读 b.current_process_id（工序归属的权威列）后该盲区消失。
        //
        // 2026-10-02 修：JOIN 条件补 `wtp.deleted_at IS NULL` —— 工种↔工序映射走
        // 「整组替换」（软删旧行 + 插新行），不过滤则已取消勾选的工序仍会把批次
        // 匹配进本工种的可领取列表。下方 COUNT 同步用**同一 `wtp` 谓词**，否则
        // `total` 与 `items` 对不上（两处的 `t_part` 侧不对称见 COUNT 处注释）。
        // ⚠️ `p.serial_no` 按 `Option<String>` 收（2026-10-03 修）：`t_part.serial_no`
        // 是 nullable（手工工单无序列号），原先按 `String` 解码 → 遇到任一
        // `serial_no IS NULL` 的 part 就整页 500（`unexpected null; try decoding as
        // an Option`）。手工工单是常态，故这是真会触发的路径。
        //
        // 2026-10-04 补 `sh.deleted_at IS NULL`（**行为变更**）：`t_shelf` 的软删
        // 守卫此前缺失，而 pick-up 写侧 `validate_shelf_zone` 走
        // `ShelfRepo::get_by_id`（带 `deleted_at IS NULL`）会拒软删架 ⇒ 现状是
        // 「列表给出但提交必被拒」。补上后两边一致：软删架上的批次不再出现在
        // 结果里。取行与 COUNT 同步补。
        //
        // 2026-10-04 补投影 part 侧 4 个真实列（`name` / `is_urgent` /
        // `system_delivery_date` / `planned_delivery_date`），并给
        // `t_part_process_chain` 无关的链四列投影 `NULL`（本端点不填，口径见
        // [`WorkTypeListRow`] 字段 doc）—— `FromRow` 按列名匹配，列集必须与
        // struct 字段集逐字对齐。
        //
        // ⚠️ `$5` 是 scope 数组、排在 `$3`/`$4`（limit/offset）**之前**出现：PG 的
        // `$n` 只是占位名、不要求按序出现，故把新参数追加在 bind 列表末尾即可把
        // `LIMIT`/`OFFSET` 的 diff 压到零（下方 COUNT 无 `$3`/`$4`，它的编号为何要
        // 独立连续，见该处注释）。
        let rows: Vec<WorkTypeListRow> = sqlx::query_as(
            "SELECT p.id, p.serial_no, p.name, p.drawing_no, p.is_urgent, \
                    p.system_delivery_date, p.planned_delivery_date, b.quantity, \
                    b.id AS batch_id, b.version AS batch_version, \
                    NULL::bigint AS process_chain_id, NULL::text AS chain_state, \
                    NULL::bigint AS chain_next_process_id, \
                    NULL::text AS chain_next_process_name, \
                    NULL::text AS chain_current_process_name \
             FROM t_part_batch b \
             JOIN t_part p ON p.id = b.part_id \
             JOIN t_work_type_process wtp ON wtp.process_id = b.current_process_id \
                AND wtp.deleted_at IS NULL \
             JOIN t_shelf sh ON sh.id = b.current_holder_id AND sh.deleted_at IS NULL \
             WHERE b.deleted_at IS NULL AND p.deleted_at IS NULL \
               AND b.status = 'IN_PROCESS' AND b.location = 'PRODUCTION_SHELF' \
               AND sh.is_active = true AND sh.zone = 'PRODUCTION' \
               AND wtp.work_type_id = $1 \
               AND ($2::bigint IS NULL OR b.current_holder_id = $2) \
               AND ($5::bigint[] IS NULL OR sh.id = ANY($5)) \
             ORDER BY p.is_urgent DESC, p.planned_delivery_date ASC, b.id ASC \
             LIMIT $3 OFFSET $4",
        )
        .bind(work_type_id)
        .bind(shelf_filter)
        .bind(limit)
        .bind(offset)
        .bind(shelf_scope.clone())
        .fetch_all(repo.conn_mut())
        .await?;
        let items: Vec<PartListItem> = rows
            .into_iter()
            .map(WorkTypeListRow::into_list_item)
            .collect();
        let total: i64 = sqlx::query_scalar(
            // 2026-10-02 订正：与取行查询同 `wtp` 谓词（含 `wtp.deleted_at IS NULL`），
            // 但**不等于同 WHERE** —— 取行查询额外 `JOIN t_part p` 且带
            // `p.deleted_at IS NULL`，本 COUNT 不 join `t_part`。故软删 part 的 active
            // batch 会计入 `total` 而不计入 `items`，软删 part 下该工种的可领批次分页
            // 总数偏大。是否补 join 属 `total` 语义决策，未在本处改动。
            //
            // 2026-10-04：scope 谓词与 `sh.deleted_at IS NULL` 两处**必须**与取行
            // 同形，否则 `total` 与 `items` 在收窄后对不上（漏任一处都会让「返回
            // 空列表但 total 仍是全厂数」或反之）。
            // ⚠️ 本 COUNT 的 scope 参数编号是 `$3` 而**不是** `$5`：PG 扩展协议要求
            // Parse 消息声明的参数类型个数 **等于** SQL 里被引用的参数个数（个数 = 被
            // 引用的最大 `$n`）。sqlx 按 `.bind()` 个数声明类型，所以本 COUNT 若沿用
            // 取行的 `$5` 编号，就得额外 bind 两个没人引用的 `$3`/`$4`，Parse 期直接被
            // PG 拒（`bind message supplies 5 parameters, but prepared statement ...
            // requires 3`）。未被引用的 `$n` 连「参数」都算不上，更不会触发类型推断
            // 报错 —— 那是另一种失败（被引用但类型推不出，且发生在 Bind/EXECUTE 期）。
            // 故 COUNT 按自身 bind 顺序连续编号，谓词语义与取行**逐字相同**。
            "SELECT COUNT(*)::bigint FROM t_part_batch b \
             JOIN t_work_type_process wtp ON wtp.process_id = b.current_process_id \
                AND wtp.deleted_at IS NULL \
             JOIN t_shelf sh ON sh.id = b.current_holder_id AND sh.deleted_at IS NULL \
             WHERE b.deleted_at IS NULL \
               AND b.status = 'IN_PROCESS' AND b.location = 'PRODUCTION_SHELF' \
               AND sh.is_active = true AND sh.zone = 'PRODUCTION' \
               AND wtp.work_type_id = $1 \
               AND ($2::bigint IS NULL OR b.current_holder_id = $2) \
               AND ($3::bigint[] IS NULL OR sh.id = ANY($3))",
        )
        .bind(work_type_id)
        .bind(shelf_filter)
        .bind(shelf_scope)
        .fetch_one(repo.conn_mut())
        .await?;
        Ok(PartListOut {
            items,
            total,
            limit,
            offset,
        })
    }

    /// `GET /parts/by-worker/{worker_id}`：工人当前持有件。
    ///
    /// 2026-10-04 起本端点是报工台「放回」页的**唯一数据源**，故出参比同族两个
    /// 列表端点多承担一层语义：批次的工序链位置（`chain_state` 三值 +
    /// `chain_next_process_*`）与批次锚点（`batch_id` / `batch_version`）。
    pub async fn list_by_worker<R: PartRepoTrait>(
        mut repo: R,
        worker_id: i64,
        query: &ByWorkerQuery,
        current: &CurrentUser,
    ) -> Result<PartListOut, AppError> {
        current.require_any_role(&[
            Role::Manager,
            Role::Clerk,
            Role::Inspector,
            Role::ShelfAccount,
        ])?;
        let limit = query.limit.unwrap_or(50).clamp(1, 200);
        let offset = query.offset.unwrap_or(0).max(0);
        // ⚠️ `p.serial_no` 按 `Option<String>` 收（2026-10-03 review 第 1 轮
        // Major-2 修）：同 `list_by_work_type` / `list_pickable_by_work_type`，
        // `t_part.serial_no` nullable，原先按 `String` 解码会让
        // `serial_no IS NULL` 的手工工单把整页打成 500。这是本文件同款写法的
        // 最后一处。
        //
        // 2026-10-04 补投影（`b.id` / `b.version` / `p.process_chain_id` +
        // part 侧 4 个真实列 + 4 个链派生列）：本端点的行本来就是「批次行」，
        // 而出参 VO 只有 part 级字段 —— 报工台放回时既定位不到批次（发不出写请求），
        // 也判定不了「这批是不是链尾 / 下一道是哪道」，更看不到工单名 / 加急 /
        // 交期（此前 `name` 填的是图号副本、`is_urgent` 恒 false、两个交期是占位
        // 值）。批次锚点填 `PartListItem::batch_id` / `batch_version`，链派生填
        // `chain_state` / `chain_next_process_id` / `chain_next_process_name` /
        // `chain_current_process_name`（填充口径见 `vo/part.rs` 字段 doc；本端点是
        // 链四字段的唯一填充路径）。
        //
        // `LEFT JOIN LATERAL` 派生的三值判据，**两步定位**（本端点自有纪律：锚链两步
        // 定位 / 派生列显式别名 / 末尾 `ORDER BY ... LIMIT 1` 收口 / 链内歧义显式
        // 落 `NONE`；「下一道」的定义见下）：
        // 1. **锚链** = `COALESCE(p.process_chain_id, cur.chain_id)`，`cur` =
        //    `b.current_process_step_id` 指向的 step，只用于回退取链 id（该 JOIN
        //    无行 ⇒ 锚链解析失败 ⇒ 落 `NONE`）；中间 JOIN `t_part_process_chain`
        //    是为了让「锚链已软删」同样落 `NONE`。
        // 2. **当前 step 在锚链内的位置**：`cur2.process_id = b.current_process_id`；
        //    再取锚链内 **`sort_order` 大于它且最小**的那一个未软删 step。
        //    `cur2` 由 JOIN LATERAL 定位并带出 `hit_count`（链内命中数），
        //    命中 >1 视作歧义落 `NONE`（见下）。`cur2` 是 **inner** `JOIN
        //    LATERAL`：定位不到时整个派生子查询无行，故下面的 `CASE` 里没有
        //    「定位不到」这一分支（该路径由最外层 `COALESCE(..., 'NONE')` 兜底）。
        //
        // ⚠️ **第 2 步必须按 `current_process_id` 在锚链内重新定位，绝对不能拿
        // `b.current_process_step_id` 的 `sort_order` 直接当位置** —— step 指针与
        // 「当前工序在链内的位置」是两个独立事实，而 worker-scan 的 RETURNED 分支
        // 只写 `current_process_id = next_process_id`、**不推进**
        // `current_process_step_id`（已知缺口，见 `docs/api/parts/inspection.md`
        // worker-scan 节）。于是多工序链的批次在第 2 次放回时 step 指针仍停在
        // **首次定位**那一步：按 `sort_order` 推进会把**当前工序自己**当成下一道
        // 返回（如指针停在 A 的 step 而 `current_process_id = B` ⇒ 返回 B），
        // 而 `chain_state` 仍在说「可免填」⇒ 写侧照单全收，静默错值比拒收更难
        // 发现。同一批次第 N 次放回都只能靠 `current_process_id` 定位。
        //
        // ⚠️ **锚链内同一 `process_id` 允许重复，读侧必须自己识别歧义**：
        // `t_process_chain_step` 只有 `uq_chain_step_chain_order (chain_id,
        // sort_order) WHERE deleted_at IS NULL` 一个唯一约束，**没有**
        // `(chain_id, process_id)` 唯一约束；写侧 `prod::process_chain::service::
        // upsert_chain` 也只校验链内 `sort_order` 互不重复，不校验 `process_id`
        // 重复 ⇒ 重复工序的链后端照收（前端工序链编辑页连续「添加工序」且不改
        // 工序即是一条）。此时 `cur2` 会扇出多行：一行派生 `NEXT → 当前工序自己`
        // （如链 `[(A,10),(A,20),(B,30)]` 而 `current_process_id = A`），另一行
        // 派生 `TAIL`，让 `LIMIT 1` 静默取其一就是拿「绝不能把当前工序自己当成
        // 下一道」这条安全承诺去赌 PG 的行序。故 `cur2` 侧用
        // `(count(*) OVER ())` 带出命中数，`hit_count > 1` 时**显式落 `NONE`**
        // （与「未知一律往保守方向降」一致），并同时门控 `nsp` / `cp` 两个派生
        // 侧：歧义时不产出任何派生值，维持 `NONE` ⇒ 下一道 id 为 `"0"`、两个名字
        // 均为 `null` 的不变量。
        //
        // ⚠️ **「下一道」按 `sort_order > 当前 ORDER BY ASC LIMIT 1` 取，不按
        // `= 当前 + 1`**：与写侧的「链内下一步」正典
        // `prod::process_chain::repo::query::next_step_in_chain`（`sort_order > $2
        // ORDER BY sort_order ASC LIMIT 1`）逐条同形，读侧不会替写侧产生分歧。
        // 而 `sort_order` 的**密度不由读侧决定**：写侧只保证链内 `sort_order`
        // 互不重复（`upsert_chain` 校验 + `uq_chain_step_chain_order` 兜底），
        // 稠密 0-based（前端 `usePartProcessDesign` 保存时拍平成 `0,1,2…`）与
        // 稀疏 `10/20/30` 两种密度都能落库且都受支持。⚠️
        // `docs/api/production/process-chain.md` 记的稀疏口径与真实写路径不符（漂移
        // 登记见 `docs/api/inconsistencies.md` §9.4），别拿它当密度依据。
        // `+ 1` 只在稠密下正确、在稀疏下会把
        // 「还有两道工序」误判成链尾，`>` 对两种密度都成立 ⇒ 读侧只能用 `>`。
        //
        // 4 个派生列都显式 `AS chain_*` 别名，与外层 `COALESCE(nx.*)` 逐字对应，
        // 避免内外层列名不一致时读错位。
        //
        // 外层 LATERAL 末尾 `ORDER BY cur.id ASC LIMIT 1` 收口：不为消歧（`cur` /
        // `pc` 都按主键定位，本就至多一行），而是把「至多一行」这条不变量写进
        // SQL —— 不收口则一旦上游改动放宽了任一 JOIN，一行批次就会扇成多行、
        // 破坏 VO 层「`items.len()` 等于持有批次数」的不变量。排序键 `cur.id` 在
        // 任何假设的扇行里都是同一个常量、打不破平局，故这个 `ORDER BY` 只表达
        // 行数上界，**不买确定性**。
        //
        // `p.process_chain_id` / `b.current_process_id` /
        // `b.current_process_step_id` 全部是可空列：列本身可空时 `query_as` 返回的
        // `O` 仍须是 `Option<T>`（外层 `Result<Option<O>>` 那层 `Option` 只表示
        // 「有没有行」）。`chain_state` 的 `COALESCE(..., 'NONE')` 在最外层兜底：
        // 无链批次的 `current_process_step_id` 按写入不变式恒为 NULL ⇒ `cur` 无行
        // ⇒ LATERAL 无行 ⇒ 四个派生列全 NULL，此时必须仍给出 `NONE` / `0`。
        let rows: Vec<WorkTypeListRow> = sqlx::query_as(
            "SELECT p.id, p.serial_no, p.name, p.drawing_no, p.is_urgent, \
                    p.system_delivery_date, p.planned_delivery_date, b.quantity, \
                    b.id AS batch_id, b.version AS batch_version, p.process_chain_id, \
                    COALESCE(nx.chain_state, 'NONE') AS chain_state, \
                    COALESCE(nx.chain_next_process_id, 0) AS chain_next_process_id, \
                    nx.chain_next_process_name AS chain_next_process_name, \
                    nx.chain_current_process_name AS chain_current_process_name \
             FROM t_part_batch b \
             JOIN t_part p ON p.id = b.part_id \
             LEFT JOIN LATERAL ( \
               SELECT \
                  CASE \
                    WHEN cur2.hit_count > 1 THEN 'NONE' \
                    WHEN nsp.id IS NULL THEN 'TAIL' \
                    ELSE 'NEXT' \
                  END AS chain_state, \
                  nsp.process_id AS chain_next_process_id, \
                  np.name AS chain_next_process_name, \
                  cp.name AS chain_current_process_name \
               FROM t_process_chain_step cur \
               JOIN t_part_process_chain pc \
                 ON pc.id = COALESCE(p.process_chain_id, cur.chain_id) \
                AND pc.deleted_at IS NULL \
               JOIN LATERAL ( \
                 SELECT cur2b.id AS id, cur2b.process_id AS process_id, \
                        cur2b.sort_order AS sort_order, \
                        (count(*) OVER ()) AS hit_count \
                 FROM t_process_chain_step cur2b \
                 WHERE cur2b.chain_id = pc.id \
                   AND cur2b.process_id = b.current_process_id \
                   AND cur2b.deleted_at IS NULL \
                 ORDER BY cur2b.sort_order ASC, cur2b.id ASC \
                 LIMIT 1 \
               ) cur2 ON TRUE \
               LEFT JOIN LATERAL ( \
                 SELECT nxt.id AS id, nxt.process_id AS process_id \
                 FROM t_process_chain_step nxt \
                 WHERE cur2.hit_count = 1 \
                   AND nxt.chain_id = pc.id \
                   AND nxt.sort_order > cur2.sort_order \
                   AND nxt.deleted_at IS NULL \
                 ORDER BY nxt.sort_order ASC \
                 LIMIT 1 \
               ) nsp ON TRUE \
               LEFT JOIN t_process np \
                 ON np.id = nsp.process_id AND np.deleted_at IS NULL \
               LEFT JOIN t_process cp \
                 ON cp.id = cur2.process_id AND cp.deleted_at IS NULL \
                AND cur2.hit_count = 1 \
               WHERE cur.id = b.current_process_step_id AND cur.deleted_at IS NULL \
               ORDER BY cur.id ASC \
               LIMIT 1 \
             ) nx ON TRUE \
             WHERE b.deleted_at IS NULL AND p.deleted_at IS NULL \
               AND b.status = 'IN_PROCESS' AND b.location = 'WORKER' \
               AND b.current_holder_id = $1 \
             ORDER BY b.id DESC LIMIT $2 OFFSET $3",
        )
        .bind(worker_id)
        .bind(limit)
        .bind(offset)
        .fetch_all(repo.conn_mut())
        .await?;
        let items: Vec<PartListItem> = rows
            .into_iter()
            .map(WorkTypeListRow::into_list_item)
            .collect();
        let total: i64 = sqlx::query_scalar(
            // 2026-10-04 不动本 COUNT：链派生只影响 items 的字段取值，不改变行的
            // 增删口径（`t_part` 侧与 items 的不对称是既有语义决策，见
            // `list_pickable_by_work_type` 的 COUNT 处注释）。
            "SELECT COUNT(*)::bigint FROM t_part_batch b \
             WHERE b.deleted_at IS NULL \
               AND b.status = 'IN_PROCESS' AND b.location = 'WORKER' \
               AND b.current_holder_id = $1",
        )
        .bind(worker_id)
        .fetch_one(repo.conn_mut())
        .await?;
        Ok(PartListOut {
            items,
            total,
            limit,
            offset,
        })
    }
}

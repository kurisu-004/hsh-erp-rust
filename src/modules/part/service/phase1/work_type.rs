//! part 域：工种维度只读端点
//!
//! - `list_by_work_type` —— `GET /parts/by-work-type/{work_type_id}`
//! - `list_pickable_by_work_type` —— `GET /parts/pickable-by-work-type/{work_type_id}`
//! - `list_by_worker` —— `GET /parts/by-worker/{worker_id}`
//!
//! 2026-10-02：手动 `pick_up`（B 方案兜底）随批次用例迁往
//! `crate::modules::prod::batch::service::pickup`，三条 list 端点留在 part 域。

use crate::auth::rbac::{CurrentUser, Role};
use crate::modules::part::repo::PartRepoTrait;
use crate::modules::part::vo::{ChainState, PartListItem, PartListOut};
use crate::shared::error::AppError;

use super::super::PartService;
use crate::modules::part::dto_crud::{ByWorkTypeQuery, ByWorkerQuery};

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
        let rows: Vec<(i64, Option<String>, String, i32, i64, Option<String>)> = sqlx::query_as(
            "SELECT p.id, p.serial_no, p.drawing_no, b.quantity, b.id AS bid, w.name AS worker_name \
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
            .map(|(id, serial, drawing, qty, bid, worker_name)| {
                // 2026-09-27 review 第 1 轮修复：PartListItem 改显式列字段，
                // 通过 `From<TPart>` 派生基础字段（next_process_id 自动不复制）。
                let p = crate::modules::part::model::TPart {
                    id,
                    serial_no: serial,
                    name: drawing.clone(),
                    drawing_no: drawing,
                    applicant_name: String::new(),
                    quantity: qty,
                    request_date: chrono::NaiveDate::from_ymd_opt(1970, 1, 1).unwrap(),
                    planned_delivery_date: chrono::NaiveDate::from_ymd_opt(1970, 1, 1).unwrap(),
                    customer_id: 0,
                    assembly_id: None,
                    status: "IN_PROCESS".to_string(),
                    is_urgent: false,
                    next_process_id: None,
                    order_no: None,
                    system_delivery_date: None,
                    note: None,
                    unit_price: rust_decimal::Decimal::ZERO,
                    total_price: rust_decimal::Decimal::ZERO,
                    version: 0,
                    created_at: chrono::NaiveDateTime::from_timestamp_opt(0, 0).unwrap(),
                    created_by: None,
                    updated_at: chrono::NaiveDateTime::from_timestamp_opt(0, 0).unwrap(),
                    updated_by: None,
                    deleted_at: None,
                    process_chain_id: None,
                };
                // 附加 worker_name（轻量：DTO 上没字段，仅放 batch_id 展示）
                let _ = bid;
                let _ = worker_name;
                PartListItem::from(p)
            })
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
        let rows: Vec<(i64, Option<String>, String, i32, Option<i64>, i64, i32)> = sqlx::query_as(
            "SELECT p.id, p.serial_no, p.drawing_no, b.quantity, b.current_process_id, \
                    b.id, b.version \
             FROM t_part_batch b \
             JOIN t_part p ON p.id = b.part_id \
             JOIN t_work_type_process wtp ON wtp.process_id = b.current_process_id \
                AND wtp.deleted_at IS NULL \
             JOIN t_shelf sh ON sh.id = b.current_holder_id \
             WHERE b.deleted_at IS NULL AND p.deleted_at IS NULL \
               AND b.status = 'IN_PROCESS' AND b.location = 'PRODUCTION_SHELF' \
               AND sh.is_active = true AND sh.zone = 'PRODUCTION' \
               AND wtp.work_type_id = $1 \
               AND ($2::bigint IS NULL OR b.current_holder_id = $2) \
             ORDER BY p.is_urgent DESC, p.planned_delivery_date ASC, b.id ASC \
             LIMIT $3 OFFSET $4",
        )
        .bind(work_type_id)
        .bind(shelf_filter)
        .bind(limit)
        .bind(offset)
        .fetch_all(repo.conn_mut())
        .await?;
        let items: Vec<PartListItem> = rows
            .into_iter()
            .map(|(id, serial, drawing, qty, _np, batch_id, batch_version)| {
                // 2026-09-27 review 第 1 轮修复：PartListItem 改显式列字段，
                // 通过 `From<TPart>` 派生基础字段（next_process_id 自动不复制）。
                let p = crate::modules::part::model::TPart {
                    id,
                    serial_no: serial,
                    name: drawing.clone(),
                    drawing_no: drawing,
                    applicant_name: String::new(),
                    quantity: qty,
                    request_date: chrono::NaiveDate::from_ymd_opt(1970, 1, 1).unwrap(),
                    planned_delivery_date: chrono::NaiveDate::from_ymd_opt(1970, 1, 1).unwrap(),
                    customer_id: 0,
                    assembly_id: None,
                    status: "IN_PROCESS".to_string(),
                    is_urgent: false,
                    next_process_id: None,
                    order_no: None,
                    system_delivery_date: None,
                    note: None,
                    unit_price: rust_decimal::Decimal::ZERO,
                    total_price: rust_decimal::Decimal::ZERO,
                    // ⚠️ 本 VO 的 `version` 是 **part 级**（`t_part.version`），
                    // 而取行 SQL 压根没投影 `p.version`（只投影了 p.id /
                    // p.serial_no / p.drawing_no）—— 恒 0 是**有意的占位**，
                    // 不是漏取值。批次乐观锁版本走 2026-10-03 新增的
                    // `PartListItem::batch_version`（取自 `b.version`）；
                    // 下一个读者请勿把本字段当批次版本用。
                    version: 0,
                    created_at: chrono::NaiveDateTime::from_timestamp_opt(0, 0).unwrap(),
                    created_by: None,
                    updated_at: chrono::NaiveDateTime::from_timestamp_opt(0, 0).unwrap(),
                    updated_by: None,
                    deleted_at: None,
                    process_chain_id: None,
                };
                let mut item = PartListItem::from(p);
                // 批次锚点：本端点是全仓唯一填这两字段的路径（出参契约见
                // vo/part.rs::PartListItem::batch_id 的字段 doc）。
                item.batch_id = Some(batch_id);
                item.batch_version = Some(batch_version);
                item
            })
            .collect();
        let total: i64 = sqlx::query_scalar(
            // 2026-10-02 订正：与取行查询同 `wtp` 谓词（含 `wtp.deleted_at IS NULL`），
            // 但**不等于同 WHERE** —— 取行查询额外 `JOIN t_part p` 且带
            // `p.deleted_at IS NULL`，本 COUNT 不 join `t_part`。故软删 part 的 active
            // batch 会计入 `total` 而不计入 `items`，软删 part 下该工种的可领批次分页
            // 总数偏大。是否补 join 属 `total` 语义决策，未在本处改动。
            "SELECT COUNT(*)::bigint FROM t_part_batch b \
             JOIN t_work_type_process wtp ON wtp.process_id = b.current_process_id \
                AND wtp.deleted_at IS NULL \
             JOIN t_shelf sh ON sh.id = b.current_holder_id \
             WHERE b.deleted_at IS NULL \
               AND b.status = 'IN_PROCESS' AND b.location = 'PRODUCTION_SHELF' \
               AND sh.is_active = true AND sh.zone = 'PRODUCTION' \
               AND wtp.work_type_id = $1 \
               AND ($2::bigint IS NULL OR b.current_holder_id = $2)",
        )
        .bind(work_type_id)
        .bind(shelf_filter)
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
        // 4 个链派生列）：本端点的行本来就是「批次行」，而出参 VO 只有 part 级
        // 字段 —— 报工台放回时既定位不到批次（发不出写请求），也判定不了「这批
        // 是不是链尾 / 下一道是哪道」。批次锚点填 `PartListItem::batch_id` /
        // `batch_version`，链派生填 `chain_state` / `chain_next_process_id` /
        // `chain_next_process_name` / `chain_current_process_name`（填充口径见
        // `vo/part.rs` 字段 doc；本端点是链四字段的唯一填充路径）。
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
        let rows: Vec<(
            i64,            // p.id
            Option<String>, // p.serial_no
            String,         // p.drawing_no
            i32,            // b.quantity
            i64,            // b.id
            i32,            // b.version
            Option<i64>,    // p.process_chain_id
            String,         // chain_state
            i64,            // chain_next_process_id
            Option<String>, // chain_next_process_name
            Option<String>, // chain_current_process_name
        )> = sqlx::query_as(
            "SELECT p.id, p.serial_no, p.drawing_no, b.quantity, \
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
            .map(
                |(
                    id,
                    serial,
                    drawing,
                    qty,
                    batch_id,
                    batch_version,
                    process_chain_id,
                    chain_state,
                    chain_next_process_id,
                    chain_next_process_name,
                    chain_current_process_name,
                )| {
                    // 2026-09-27 review 第 1 轮修复：PartListItem 改显式列字段，
                    // 通过 `From<TPart>` 派生基础字段（next_process_id 自动不复制）。
                    let p = crate::modules::part::model::TPart {
                        id,
                        serial_no: serial,
                        name: drawing.clone(),
                        drawing_no: drawing,
                        applicant_name: String::new(),
                        quantity: qty,
                        request_date: chrono::NaiveDate::from_ymd_opt(1970, 1, 1).unwrap(),
                        planned_delivery_date: chrono::NaiveDate::from_ymd_opt(1970, 1, 1).unwrap(),
                        customer_id: 0,
                        assembly_id: None,
                        status: "IN_PROCESS".to_string(),
                        is_urgent: false,
                        next_process_id: None,
                        order_no: None,
                        system_delivery_date: None,
                        note: None,
                        unit_price: rust_decimal::Decimal::ZERO,
                        total_price: rust_decimal::Decimal::ZERO,
                        version: 0,
                        created_at: chrono::NaiveDateTime::from_timestamp_opt(0, 0).unwrap(),
                        created_by: None,
                        updated_at: chrono::NaiveDateTime::from_timestamp_opt(0, 0).unwrap(),
                        updated_by: None,
                        deleted_at: None,
                        // 2026-10-04：取行 SQL 投影的真实值。
                        process_chain_id,
                    };
                    let mut item = PartListItem::from(p);
                    // 批次锚点：与 `list_pickable_by_work_type` 同款覆写（出参契约见
                    // vo/part.rs::PartListItem::batch_id 的字段 doc）。
                    item.batch_id = Some(batch_id);
                    item.batch_version = Some(batch_version);
                    // 链派生：本端点是链四字段的唯一填充路径。
                    item.chain_state = ChainState::from_db_text(&chain_state);
                    item.chain_next_process_id = chain_next_process_id;
                    item.chain_next_process_name = chain_next_process_name;
                    item.chain_current_process_name = chain_current_process_name;
                    item
                },
            )
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

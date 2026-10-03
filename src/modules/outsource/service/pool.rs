//! outsource 域 service — `GET /outsource-pool/*` 子模块（2026-10-03 新增）
//!
//! 看板三件套（形态照抄 `prod::pool`）：
//! - `pool_counts` —— `GET /outsource-pool/counts`，跨所有货架按外协工序聚合
//!   「可发 / 在途」双徽标；
//! - `pool_by_process` —— `GET /outsource-pool/{process_id}`，一个 tab 的全部
//!   内容（左列候选 + 右列公司）；
//! - `pool_state` —— `GET /outsource-pool/state`，某公司在某工序在外协的全部批次。
//!
//! ## 为什么需要独立域（不扩 `/outsource-sendable`）
//! 既有 `GET /outsource-sendable` 与 `GET /outsource-shipments/in-flight` 都
//! **不接受 `process_id` 且分页**。看板要求「每个外协工序一个 tab、tab 内不分页
//! 拿全」，扩这两个端点会同时破坏前端已上线的分页契约，故另起独立顶层前缀。
//!
//! ## 候选侧与 `/outsource-sendable` 同源
//! `items` / `sendable_count` 走 `sendable_list_by_process` /
//! `pool_group_sendable_counts`，二者与 `sendable_list` / `sendable_count` 共享
//! `repo/sql.rs::SENDABLE_INNER_X_SQL`（谓词唯一落点）。故
//! 「看板 tab 内行」与「sendable 一览按 `next_process_id` 过滤的行」逐字段一致
//! —— 由 `tests/outsource/pool.rs` 的对照用例守住。
//!
//! ## 事务边界
//! 三个端点都是纯读：handler `pool.acquire()` **不开事务**，service 借
//! `&mut *conn` 跑查询。WS 广播由写侧（`prod::batch` 的 send / receive）在
//! commit 后发出，本域不发。
//!
//! ## 防 N+1
//! - 候选侧 `company_options` 在 SQL 内一次 `array_agg` 拿完（沿用既有写法）。
//! - `companies[].held_count` 与 `state.items` 全部在各自一条 SQL 内 JOIN /
//!   LATERAL 解析完毕，service 层**不循环查**。
//! - `pool_counts` 的工序元数据**一次 `process_map_short(&all_ids)` 取齐**
//!   （不按工序逐个查），查询条数与工序数无关。

use std::collections::HashMap;

use crate::auth::rbac::{CurrentUser, Role};
use crate::modules::outsource::dto::OutsourcePoolStateQuery;
use crate::modules::outsource::repo::OutsourceRepoTrait;
use crate::modules::outsource::vo::{
    OutsourceCompanyOption, OutsourceHeldBatchItem, OutsourcePoolCandidate,
    OutsourcePoolCompanyOut, OutsourcePoolCountsOut, OutsourcePoolDetailOut,
    OutsourcePoolProcessCount, OutsourcePoolStateOut,
};
use crate::shared::error::{AppError, code};

use super::{OutsourceService, join_customer_path};

/// 把 SQL `array_agg(json_build_object(...))` 的结果解成 `Vec<OutsourceCompanyOption>`。
///
/// 与 `service/sendable.rs::decode_company_options` 同一口径（畸形降级为空数组
/// 而不是让整页 500）。两处各留一份：前者 private、后者同模块；把私有 helper
/// 提到 `service/mod.rs` 的收益（去 6 行重复）小于「外协域多一个 pub(crate)
/// 符号」的长期成本。
fn decode_company_options(raw: serde_json::Value) -> Vec<OutsourceCompanyOption> {
    serde_json::from_value(raw).unwrap_or_default()
}

/// 该候选行能否发送：`APPROVAL` 恒可发（报价已批），`DIRECT` 需至少一个候选公司。
///
/// 提成独立函数是为了能单测 —— 这条判定同时驱动「卡片是否置灰」和「拖到公司列
/// 后能否真发出去」，两处口径漂移的代价是用户点了没反应。
fn can_send(send_mode: &str, company_options: &[OutsourceCompanyOption]) -> bool {
    send_mode == "APPROVAL" || !company_options.is_empty()
}

/// 取工序元数据 `(code, name)`；工序不存在 / 已软删 → `20801 BIZ_PROCESS_NOT_FOUND`。
///
/// 口径照 `/prod/pool/{process_id}`（同一形态的模板端点）：拿不到元数据就没法
/// 拼 tab 标题，静默给空串会让前端渲染出一个无名 tab，故直接 404。
async fn process_meta<R: OutsourceRepoTrait>(
    repo: &mut R,
    process_id: i64,
) -> Result<(String, String), AppError> {
    let rows = repo.process_map_short(&[process_id]).await?;
    // `process_map_short` 返 `(id, code, name)`；入参只 1 个 id，取到的 id 必然相等。
    let (_, code, name) = rows.into_iter().next().ok_or_else(|| {
        AppError::biz(
            code::BIZ_PROCESS_NOT_FOUND,
            format!("process {process_id} 不存在"),
        )
    })?;
    Ok((code, name))
}

impl OutsourceService {
    /// `GET /outsource-pool/counts`（2026-10-03 新增）
    ///
    /// 角色守卫照抄 `/api/v2/prod/pool/counts`：Manager + Clerk + Inspector
    /// （admin 视角但不止 Manager）。
    ///
    /// 流程（4 条查询，与工序数无关）：
    /// 1. 候选侧 `GROUP BY next_process_id` 计数（与 `pool_by_process` 的 items
    ///    行粒度逐行一致）；
    /// 2. 在途侧 `GROUP BY current_process_id` 计数；
    /// 3. 两张计数表求并集，只留 `sendable + in_flight > 0` 的工序，
    ///    按 `process_id ASC` 排；
    /// 4. 一次 `process_map_short(&all_ids)` 取齐工序元数据。
    pub async fn pool_counts<R: OutsourceRepoTrait>(
        &self,
        mut repo: R,
        current: &CurrentUser,
    ) -> Result<OutsourcePoolCountsOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk, Role::Inspector])?;

        let sendable = repo.pool_group_sendable_counts().await?;
        let in_flight = repo.pool_group_in_flight_counts().await?;

        // 并集：候选侧或在途侧任一非零都要出 tab。
        let mut by_process: HashMap<i64, (i64, i64)> =
            sendable.into_iter().map(|(pid, n)| (pid, (n, 0))).collect();
        for (pid, n) in in_flight {
            by_process
                .entry(pid)
                .and_modify(|(_, i)| *i = n)
                .or_insert((0, n));
        }

        // 只留非零组合 + 按 process_id ASC（前端 tab 序稳定，不随数据量抖动）。
        let mut merged: Vec<(i64, i64, i64)> = by_process
            .into_iter()
            .filter(|(_, (s, i))| s + i > 0)
            .map(|(pid, (s, i))| (pid, s, i))
            .collect();
        merged.sort_by_key(|(pid, _, _)| *pid);

        // 元数据一次取齐（防 N+1）：前端的 tab 标题要 code + name。
        let all_ids: Vec<i64> = merged.iter().map(|(pid, _, _)| *pid).collect();
        let meta: HashMap<i64, (String, String)> = repo
            .process_map_short(&all_ids)
            .await?
            .into_iter()
            .map(|(id, code, name)| (id, (code, name)))
            .collect();

        let mut sendable_total = 0i64;
        let mut in_flight_total = 0i64;
        let mut counts = Vec::with_capacity(merged.len());
        for (pid, sendable_count, in_flight_count) in merged {
            sendable_total += sendable_count;
            in_flight_total += in_flight_count;
            // 防御：候选侧 INNER JOIN `t_process` 已排除软删工序；在途侧没有该
            // JOIN，故可能落到「工序已软删」—— 退化为空 code + 显式占位名
            // （口径同 `prod::worker_pool::pool_counts_all_shelves`）。
            let (process_code, process_name) = meta
                .get(&pid)
                .cloned()
                .unwrap_or_else(|| (String::new(), format!("(deleted#{pid})")));
            counts.push(OutsourcePoolProcessCount {
                process_id: pid,
                process_code,
                process_name,
                sendable_count,
                in_flight_count,
            });
        }

        Ok(OutsourcePoolCountsOut {
            counts,
            sendable_total,
            in_flight_total,
            total: sendable_total + in_flight_total,
        })
    }

    /// `GET /outsource-pool/{process_id}`（2026-10-03 新增）
    ///
    /// 角色守卫同 `pool_counts`。流程（3 条查询，与行数无关）：
    /// 1. process 元数据（不存在 → `20801`）；
    /// 2. `companies`：该工序映射的全部活跃外协公司 + `held_count`（无在途批次的
    ///    公司也在列，`held_count = 0`）；
    /// 3. `items`：候选批次（与 `/outsource-sendable` 同源 SQL，按
    ///    `next_process_id = $1` 过滤，不分页）。
    pub async fn pool_by_process<R: OutsourceRepoTrait>(
        &self,
        mut repo: R,
        process_id: i64,
        current: &CurrentUser,
    ) -> Result<OutsourcePoolDetailOut, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk, Role::Inspector])?;

        let (process_code, process_name) = process_meta(&mut repo, process_id).await?;

        let companies: Vec<OutsourcePoolCompanyOut> = repo
            .pool_list_companies_with_held(process_id)
            .await?
            .into_iter()
            .map(|r| OutsourcePoolCompanyOut {
                company_id: r.company_id,
                name: r.name,
                held_count: r.held_count,
            })
            .collect();

        let items: Vec<OutsourcePoolCandidate> = repo
            .sendable_list_by_process(process_id)
            .await?
            .into_iter()
            .map(|r| {
                let company_options = decode_company_options(r.company_options);
                // 命中 APPROVED 报价 → APPROVAL（quote_id / company / price 三件套
                // 来自报价）；未命中 → DIRECT。判定与 `/outsource-sendable` 同源。
                let send_mode = if r.quote_id.is_some() {
                    "APPROVAL"
                } else {
                    "DIRECT"
                };
                OutsourcePoolCandidate {
                    version: r.batch_version,
                    send_mode: send_mode.to_string(),
                    source_status: r.source_status,
                    part_id: r.part_id,
                    part_serial_no: r.part_serial_no,
                    part_drawing_no: r.part_drawing_no,
                    part_name: r.part_name,
                    quantity: r.batch_quantity,
                    batch_id: r.batch_id,
                    batch_no: r.batch_no,
                    batch_quantity: r.batch_quantity,
                    planned_delivery_date: r.planned_delivery_date,
                    is_urgent: r.is_urgent,
                    customer_path: join_customer_path(
                        r.parent_customer_name.as_deref(),
                        r.customer_name.as_deref(),
                    ),
                    shelf_code: Some(r.shelf_code),
                    outsource_company_id: r.outsource_company_id,
                    outsource_company_name: r.outsource_company_name,
                    quote_id: r.quote_id,
                    can_send: can_send(send_mode, &company_options),
                    company_options,
                    price: r.price,
                    status_label: "sendable".to_string(),
                }
            })
            .collect();
        let total = items.len() as i64;

        Ok(OutsourcePoolDetailOut {
            process_id,
            process_code,
            process_name,
            companies,
            total,
            items,
        })
    }

    /// `GET /outsource-pool/state?outsource_company_id=&process_id=`（2026-10-03 新增）
    ///
    /// **无 role guard**（口径同 `/api/v2/prod/pool/state`：已登录即可读，
    /// 看板上 admin 监控与业务自查共用）。
    ///
    /// 流程（2 条查询）：
    /// 1. 公司名（不存在 → `None`，**不拒绝**，见文档「端点契约要点」）；
    /// 2. 一条 list SQL 拿完该公司在该工序在外协的全部批次（含 shipment 字段
    ///    与派生的下一道工序）。
    ///
    /// 本端点**不校验工序存在性**（VO 里没有工序元数据位可承载 404 语义），
    /// 工序 id 无对应在途批次时返回空列表 + `current_held = 0`。
    pub async fn pool_state<R: OutsourceRepoTrait>(
        &self,
        mut repo: R,
        query: &OutsourcePoolStateQuery,
    ) -> Result<OutsourcePoolStateOut, AppError> {
        let company_name = repo
            .company_get_by_id(query.outsource_company_id, false)
            .await?
            .map(|c| c.name);

        let items: Vec<OutsourceHeldBatchItem> = repo
            .pool_list_held(query.outsource_company_id, query.process_id)
            .await?
            .into_iter()
            .map(|r| {
                // 下一道工序派生全在 SQL（LEFT JOIN LATERAL）；这里只做 0 兜底与
                // `chain_resolvable` 判定。0 兜底口径沿 PendingBatchItemOut：
                // 后端 i64 + serialize_i64 ⇒ JSON 非 nullable 的字符串 "0"。
                let receive_next_process_id = r.receive_next_process_id;
                OutsourceHeldBatchItem {
                    batch_id: r.batch_id,
                    part_id: r.part_id,
                    batch_no: r.batch_no,
                    quantity: r.quantity,
                    serial_no: r.serial_no,
                    drawing_no: r.drawing_no,
                    name: r.name,
                    system_delivery_date: r.system_delivery_date,
                    planned_delivery_date: r.planned_delivery_date,
                    is_urgent: r.is_urgent,
                    customer_name: r.customer_name,
                    parent_customer_name: r.parent_customer_name,
                    applicant_name: r.applicant_name,
                    location: r.batch_location,
                    note: r.note,
                    version: r.batch_version,
                    // `LEFT JOIN` 的诚实映射：正常流恒有开口 shipment
                    // （`uq_t_outsource_shipment_open_batch` + send 端点同事务
                    // INSERT），故这两项实际总非空；不编造空串替身。
                    sent_at: r.sent_at,
                    price: r.price,
                    receive_next_process_id,
                    receive_next_process_name: r.receive_next_process_name,
                    chain_resolvable: receive_next_process_id != 0,
                }
            })
            .collect();
        let current_held = items.len() as i64;

        Ok(OutsourcePoolStateOut {
            outsource_company_id: query.outsource_company_id,
            outsource_company_name: company_name,
            process_id: query.process_id,
            current_held,
            items,
        })
    }
}

#[cfg(test)]
mod tests {
    //! 2026-10-03 新增：`can_send` 判定的纯函数回归。
    //!
    //! 这条判定同时驱动「卡片是否置灰」和「拖到公司列后能否真发出去」，
    //! 两处口径漂移的代价是用户点了没反应，故锁住。
    use super::*;

    fn option(id: i64) -> OutsourceCompanyOption {
        OutsourceCompanyOption {
            id,
            name: "Co".into(),
        }
    }

    /// APPROVAL 恒可发 —— 即便 `company_options` 为空（SQL 侧 APPROVAL 短路成 `[]`）。
    #[test]
    fn can_send_true_for_approval_even_without_options() {
        assert!(can_send("APPROVAL", &[]));
    }

    /// DIRECT 且无候选公司 → 不可发（前端把卡片置灰）。
    #[test]
    fn can_send_false_for_direct_without_options() {
        assert!(!can_send("DIRECT", &[]));
    }

    /// DIRECT 且有候选公司 → 可发。
    #[test]
    fn can_send_true_for_direct_with_options() {
        assert!(can_send("DIRECT", &[option(9_000_000_000_000_000_601)]));
    }
}

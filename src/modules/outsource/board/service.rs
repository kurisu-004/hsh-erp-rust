//! outsource 外协看板装配 service（2026-10-09 新增）
//!
//! service 层零 SQL：只做角色守卫 + 内存分组 + VO 装配。SQL 全在
//! [`super::repo::OutsourceQueueRepo`]。
//!
//! ## 为什么派生字段一律在 service 算
//! - `total = items.len()`：明细与计数必须恒等，SQL 再数一次就是给「两条谓词漂移」
//!   留一个机会。
//! - `companies[].held_count = held_batches.len()`：在途批次是一次查询后在内存里
//!   分组的，分组行数就是权威值；SQL 侧那条 `COUNT` 已随看板内联一并删掉。
//! - `chain_resolvable = receive_next_process_id != 0`：0 兜底口径的翻译，前端据此
//!   决定要不要弹「手填下一道工序」对话框。
//! - `can_send`：与 `GET /outsource-sendable` 共用同一判定（同一个函数，见
//!   `crate::modules::outsource::service::sendable::can_send_of`）。

use std::collections::{BTreeMap, HashMap};

use sqlx::PgConnection;

use crate::auth::rbac::{CurrentUser, Role};
use crate::infra::clock::now_shanghai_iso;
use crate::modules::outsource::service::sendable::{
    can_send_of, decode_company_options, send_mode_of,
};
use crate::modules::outsource::vo::{
    OutsourceQueueCandidate, OutsourceQueueCompany, OutsourceQueueHeldBatch, OutsourceQueueProcess,
    OutsourceQueueProcessDetail, OutsourceQueueProcessMeta, OutsourceQueueSnapshot,
};
use crate::shared::error::AppError;

use super::repo::{CandidateRow, CompanyRow, HeldBatchRow, OutsourceQueueRepo, ProcessMetaRow};

/// 看板装配 service（unit struct；service 不持 repo / pool —— handler 借
/// `&mut *conn` 传入，与 `prod::queue::board::service` 同形）。
pub struct OutsourceQueueService;

impl Default for OutsourceQueueService {
    fn default() -> Self {
        Self
    }
}

impl OutsourceQueueService {
    pub fn new() -> Self {
        Self
    }

    /// 工序序列板。角色守卫：Manager + Clerk + Inspector（沿用被取代的
    /// `GET /outsource-pool/counts` 口径 —— admin 视角但不止 Manager）。
    pub async fn build_snapshot(
        conn: &mut PgConnection,
        current: &CurrentUser,
    ) -> Result<OutsourceQueueSnapshot, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk, Role::Inspector])?;
        let (sendable, in_flight, meta) = OutsourceQueueRepo::snapshot(&mut *conn).await?;

        // 两侧计数求并集（任一非零都出 tab）。BTreeMap 保证 `process_id ASC` 稳定序。
        let mut merged: BTreeMap<i64, (i64, i64)> = sendable
            .into_iter()
            .map(|c| (c.process_id, (c.count, 0)))
            .collect();
        for c in in_flight {
            merged
                .entry(c.process_id)
                .and_modify(|(_, i)| *i = c.count)
                .or_insert((0, c.count));
        }

        // 一次 ANY 取回的元数据转成 map，`remove` 消费保证同一行不被取两次。
        let mut meta_by_id: HashMap<i64, ProcessMetaRow> =
            meta.into_iter().map(|m| (m.id, m)).collect();

        let mut sendable_total = 0i64;
        let mut in_flight_total = 0i64;
        let mut processes = Vec::with_capacity(merged.len());
        for (process_id, (sendable_count, in_flight_count)) in merged {
            sendable_total += sendable_count;
            in_flight_total += in_flight_count;
            processes.push(to_process(
                process_id,
                meta_by_id.remove(&process_id),
                sendable_count,
                in_flight_count,
            ));
        }

        Ok(OutsourceQueueSnapshot {
            processes,
            sendable_total,
            in_flight_total,
            ts: now_shanghai_iso(),
        })
    }

    /// 单工序看板。角色守卫同 [`Self::build_snapshot`]。
    pub async fn build_process_detail(
        conn: &mut PgConnection,
        current: &CurrentUser,
        process_id: i64,
    ) -> Result<OutsourceQueueProcessDetail, AppError> {
        current.require_any_role(&[Role::Manager, Role::Clerk, Role::Inspector])?;
        let data = OutsourceQueueRepo::process_detail(&mut *conn, process_id).await?;

        // 在途批次按公司分组（一次查询结果的服务端分组，不是第二次查询）
        let companies = to_companies(data.companies, data.held);

        let items: Vec<OutsourceQueueCandidate> =
            data.items.into_iter().map(to_candidate).collect();
        let total = items.len() as i64;

        Ok(OutsourceQueueProcessDetail {
            process: OutsourceQueueProcessMeta {
                process_id: data.process.id.to_string(),
                process_code: data.process.code,
                process_name: data.process.name,
                color: data.process.color,
            },
            companies,
            items,
            total,
            ts: now_shanghai_iso(),
        })
    }
}

/// 工序序列板的一行（`meta = None` = 工序已软删 / 元数据没查到）。
///
/// **软删工序仍要出 tab**，否则运营看不到「这批货压在谁的名下」。兜底口径：
/// `code` 空串 + `name` 显式占位 `(deleted#{id})` + `category` 按在途语义兜底为
/// `OUTSOURCE` —— 候选侧不可能命中软删工序（它 INNER JOIN 了 `t_process`），只有
/// 在途侧会。与 `prod::queue::board::service::build_snapshot` 同款。
fn to_process(
    process_id: i64,
    meta: Option<ProcessMetaRow>,
    sendable_count: i64,
    in_flight_count: i64,
) -> OutsourceQueueProcess {
    let (process_code, process_name, color, category) = match meta {
        Some(m) => (m.code, m.name, m.color, m.category),
        None => (
            String::new(),
            format!("(deleted#{process_id})"),
            None,
            "OUTSOURCE".to_string(),
        ),
    };
    OutsourceQueueProcess {
        // i64 主键在**装配处** `.to_string()`（照 prod::queue 看板 VO）。
        process_id: process_id.to_string(),
        process_code,
        process_name,
        color,
        category,
        sendable_count,
        in_flight_count,
    }
}

fn to_candidate(r: CandidateRow) -> OutsourceQueueCandidate {
    let company_options = decode_company_options(r.company_options);
    let send_mode = send_mode_of(r.requires_approval, r.quote_id.is_some());
    OutsourceQueueCandidate {
        version: r.batch_version,
        send_mode: send_mode.to_string(),
        batch_id: r.batch_id.to_string(),
        part_id: r.part_id.to_string(),
        batch_no: r.batch_no,
        quantity: r.batch_quantity,
        part_serial_no: r.part_serial_no,
        part_drawing_no: r.part_drawing_no,
        part_name: r.part_name,
        planned_delivery_date: r.planned_delivery_date,
        system_delivery_date: r.system_delivery_date,
        is_urgent: r.is_urgent,
        customer_name: r.customer_name,
        parent_customer_name: r.parent_customer_name,
        applicant_name: r.applicant_name,
        note: r.note,
        shelf_code: r.shelf_code,
        // `current_holder_id` 为 NULL（PENDING 未上架批次）时序列化成空串而不是
        // `null` —— `null` 会让前端的必填字符串校验炸在整页渲染上，而候选 VO 里
        // 这个字段是移动写端点 `from.shelf_id` 的数据源。
        shelf_id: r.shelf_id.map(|v| v.to_string()).unwrap_or_default(),
        outsource_company_id: r.outsource_company_id.map(|v| v.to_string()),
        outsource_company_name: r.outsource_company_name,
        quote_id: r.quote_id.map(|v| v.to_string()),
        can_send: can_send_of(send_mode, &company_options),
        company_options,
        price: r.price,
        has_cnc_program: r.has_cnc_program,
    }
}

fn to_held_batch(r: HeldBatchRow) -> OutsourceQueueHeldBatch {
    OutsourceQueueHeldBatch {
        batch_id: r.batch_id.to_string(),
        part_id: r.part_id.to_string(),
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
        has_cnc_program: r.has_cnc_program,
        // `LEFT JOIN` 的诚实映射：正常流恒有开口 shipment（unique 约束 + 发送方向同
        // 事务 INSERT），故这两项实际总非空；不编造空串替身。
        sent_at: r.sent_at,
        price: r.price,
        receive_next_process_id: r.receive_next_process_id.to_string(),
        receive_next_process_name: r.receive_next_process_name,
        chain_resolvable: r.receive_next_process_id != 0,
    }
}

/// 公司列装配（供 `held_count_matches_held_batches_len` 单测直接调用）。
///
/// 提成独立纯函数是为了让不变量在**生产代码路径上**被断言，而不是在测试里重写一遍
/// 装配逻辑（那样测试只验证自己）。
pub(super) fn to_companies(
    companies: Vec<CompanyRow>,
    held: Vec<HeldBatchRow>,
) -> Vec<OutsourceQueueCompany> {
    // 在途批次按公司分组：**不依赖 SQL 的 ORDER BY 保证同一公司的行连续** —— 那条
    // ORDER BY 排的是 `pb.current_holder_id ASC, pb.id ASC`，与分组键同序只是巧合，
    // 一旦有人改 ORDER BY 就会静默错位。HashMap 显式分组是唯一与排序无关的写法
    // （照 `prod::queue::board::service` 的持有批次分组）。
    let mut held_by_company: HashMap<i64, Vec<OutsourceQueueHeldBatch>> = HashMap::new();
    for row in held {
        held_by_company
            .entry(row.company_id)
            .or_default()
            .push(to_held_batch(row));
    }
    companies
        .into_iter()
        .map(|c| {
            let held_batches = held_by_company.remove(&c.company_id).unwrap_or_default();
            OutsourceQueueCompany {
                company_id: c.company_id.to_string(),
                name: c.name,
                held_count: held_batches.len() as i64,
                held_batches,
            }
        })
        .collect()
}

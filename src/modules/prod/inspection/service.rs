//! prod::inspection 子模块 service 层 —— 业务逻辑
//!
//! 2026-10-05 新增：单个方法 [`InspectionScanService::scan`]。
//!
//! 2026-10-07 新增：待品检队列读 [`InspectionQueueService::list_queue`]（自
//! `prod::batch::service::list` 迁入，规范化口径与 SQL 逐字未变），与扫码读同域。
//!
//! ## 角色守卫
//! 下沉到 service 第一行（沿 `prod::batch` 的 `TO_XXX_ROLES` 范本），
//! `current.require_any_role(READ_ROLES)`；handler 仅做
//! 参数提取，不重复校验。
//!
//! 白名单**只有 2 个角色**（与 `GET /api/v2/prod/inspection/queue` 及
//! `to-ship` / `to-inspection` / `to-process` 三个写端点同一组）：扫码树是
//! 品检动作的前置上下文，放行 `Clerk` / `CncProgrammer` / `ShelfAccount` 会
//! 让它们看到本该看不到的批次明细。队列读的白名单逐字相同（迁前就是同一组，
//! 常量 [`READ_ROLES`] 现由两个 service 共用，改白名单只需改这一处）。
//!
//! ## 事务边界
//! 读端点不开事务（handler `pool.acquire()` 借 `&mut PgConnection`）。纯读，
//! **不发** WS 广播。
//!
//! ## 命中顺序：先 `t_part` 再 `t_assembly`
//! 序列号在**两张表都有值域**（子件 `{asm}-{i:02d}` 与父件 `{prefix}{4 位}`），
//! 两表各自对活跃行有唯一索引。故口径是：
//!
//! 1. 先查 `t_part.serial_no` —— 命中即 `hit_kind = "PART"`
//! 2. 未命中再查 `t_assembly.serial_no` —— 命中即 `hit_kind = "ASSEMBLY"`
//! 3. 都未命中 → `20101 BIZ_PART_NOT_FOUND`（HTTP 404）
//!
//! ⚠️ 顺序不可颠倒：`t_part` 在前意味着「子件码」永远不会被误判成装配件码；
//! 反过来先查 `t_assembly` 也安全（子件码与父件码不同形），但固定「先 part
//! 后 assembly」与全仓扫码入口（`part` 域 `get_by_serial`）的取数方向一致，
//! 少一个需要解释的特例。

use std::collections::HashMap;

use sqlx::PgConnection;

use crate::auth::rbac::{CurrentUser, Role};
use crate::modules::prod::inspection::dto::InspectionQueueQuery;
use crate::modules::prod::inspection::model::{ScanAssemblyRow, ScanBatchRow, ScanPartRow};
use crate::modules::prod::inspection::repo::{
    InspectionQueueFilters, InspectionQueueRepo, InspectionScanRepo,
};
use crate::modules::prod::inspection::vo::{
    InspectionQueueItemOut, InspectionQueueListOut, ScanAssemblyOut, ScanBatchOut, ScanPartOut,
    ScanTreeOut,
};
use crate::shared::customer::expand_customer_id;
use crate::shared::error::{AppError, code};

/// 本域两个读端点（`GET /scan/{serial_no}` / `GET /queue`）允许的角色：Manager 或 Inspector。
///
/// 与 `prod::batch::handler::transition` 的 `TO_XXX_ROLES` 同一组 —— 扫码树与
/// 队列里的批次就是那三个写端点的操作对象，能看就必须能操作。
const READ_ROLES: &[Role] = &[Role::Manager, Role::Inspector];

/// `hit_kind` 出参的取值白名单。
///
/// 只在 service 内构造，出参仍是 `String`（契约逐字要求，前端 Zod 用
/// `z.enum` 校验）；把两个字面量收进枚举是为了让拼错成为编译错误。
enum HitKind {
    /// 扫到的是装配件条码（`t_assembly` 命中）。
    Assembly,
    /// 扫到的是独立件或装配件子件的条码（`t_part` 命中）。
    Part,
}

impl HitKind {
    fn as_str(&self) -> &'static str {
        match self {
            HitKind::Assembly => "ASSEMBLY",
            HitKind::Part => "PART",
        }
    }
}

/// 命中结果（SQL 阶段的产物，尚未挂批次）。
struct HitTree {
    kind: HitKind,
    /// 被扫中的那个零件 id。扫装配件条码时为 `None`（装配件没有零件身份）。
    scanned_part_id: Option<i64>,
    assembly: Option<ScanAssemblyRow>,
    /// 顶层零件节点（装配件树 = 全部子件；独立件树 = `[被扫中的那个]`）。
    parts: Vec<ScanPartRow>,
}

/// `prod::inspection` service（ZST，与 `prod::programming` 范本一致）。
pub struct InspectionScanService;

impl InspectionScanService {
    /// `GET /api/v2/prod/inspection/scan/{serial_no}` 业务逻辑。
    ///
    /// 流程：角色守卫 → 序列号 trim + 空值兜底 → 命中（part → assembly 回退）→
    /// 定位装配件节点与零件层 → **一条** SQL 取全部批次 → 内存分组挂树。
    pub async fn scan(
        conn: &mut PgConnection,
        current: &CurrentUser,
        serial_no: &str,
    ) -> Result<ScanTreeOut, AppError> {
        current.require_any_role(READ_ROLES)?;

        // 扫码枪偶发尾随空白 / 空格；空串等价于「没扫到东西」，按未命中收口。
        let serial_no = serial_no.trim();
        if serial_no.is_empty() {
            // 空串直接插值进 message 会渲染成「序列号  未找到…」（双空格），用
            // 占位符让日志与前端 toast 可读。
            return Err(not_found("(空)"));
        }

        let hit = match InspectionScanRepo::find_part_by_serial(&mut *conn, serial_no).await? {
            Some(p) => {
                // 扫到零件：父装配件活跃 → 整棵装配件树；父装配件已软删（取不
                // 到）→ 退化成独立件树（assembly = null），不返回孤儿树。
                match p.assembly_id {
                    Some(asm_id) => {
                        match InspectionScanRepo::find_assembly_by_id(&mut *conn, asm_id).await? {
                            Some(asm) => HitTree {
                                kind: HitKind::Part,
                                scanned_part_id: Some(p.id),
                                parts: InspectionScanRepo::list_parts_by_assembly(
                                    &mut *conn, asm_id,
                                )
                                .await?,
                                assembly: Some(asm),
                            },
                            None => HitTree {
                                kind: HitKind::Part,
                                scanned_part_id: Some(p.id),
                                parts: vec![p],
                                assembly: None,
                            },
                        }
                    }
                    None => HitTree {
                        kind: HitKind::Part,
                        scanned_part_id: Some(p.id),
                        parts: vec![p],
                        assembly: None,
                    },
                }
            }
            // part 未命中 → 回退查装配件条码。
            None => {
                let asm = InspectionScanRepo::find_assembly_by_serial(&mut *conn, serial_no)
                    .await?
                    .ok_or_else(|| not_found(serial_no))?;
                HitTree {
                    kind: HitKind::Assembly,
                    // 装配件码没有零件身份 → 全部批次 is_scanned = false
                    scanned_part_id: None,
                    parts: InspectionScanRepo::list_parts_by_assembly(&mut *conn, asm.id).await?,
                    assembly: Some(asm),
                }
            }
        };

        // 批次层：一条 SQL 覆盖整棵树（无 N+1），空零件列表时 repo 直接返空。
        let part_ids: Vec<i64> = hit.parts.iter().map(|p| p.id).collect();
        let batch_rows =
            InspectionScanRepo::list_batches_by_part_ids(&mut *conn, &part_ids).await?;

        // 按 part_id 内存分组；SQL 已按 (part_id, batch_no, id) 排好序，
        // 分组后每个零件的批次序与 SQL 序一致，前端无需二次排序。
        let mut batches_by_part: HashMap<i64, Vec<ScanBatchOut>> = HashMap::new();
        for row in batch_rows {
            // is_scanned 的唯一来源：t_part_batch 无序列号列，只能内存比对
            // 「批次所属零件 == 被扫中的那个零件」。
            let is_scanned = hit.scanned_part_id == Some(row.part_id);
            batches_by_part
                .entry(row.part_id)
                .or_default()
                .push(batch_to_out(row, is_scanned));
        }

        let children = hit
            .parts
            .into_iter()
            .map(|p| {
                let batches = batches_by_part.remove(&p.id).unwrap_or_default();
                part_to_out(p, batches)
            })
            .collect();

        Ok(ScanTreeOut {
            hit_kind: hit.kind.as_str().to_string(),
            scanned_serial_no: serial_no.to_string(),
            assembly: hit.assembly.map(assembly_to_out),
            children,
        })
    }
}

/// 「序列号两表皆未命中」的唯一错误出口。
///
/// 复用 part 域的 `20101 BIZ_PART_NOT_FOUND`（HTTP 404）而不是另开错误码：
/// 语义完全相同（扫到的东西不存在），前端按同一个 code 弹「未找到」即可。
/// 入参是**已 trim** 的序列号；纯空白串那条路径传占位符 `"(空)"`（见 [`Self::scan`]）。
fn not_found(serial_no: &str) -> AppError {
    AppError::biz(
        code::BIZ_PART_NOT_FOUND,
        format!("序列号 {serial_no} 未找到对应零件或装配件"),
    )
}

/// 装配件行 → VO（逐字段直传，无兜底）。
fn assembly_to_out(r: ScanAssemblyRow) -> ScanAssemblyOut {
    ScanAssemblyOut {
        id: r.id,
        serial_no: r.serial_no,
        name: r.name,
        drawing_no: r.drawing_no,
        status: r.status,
        quantity: r.quantity,
        is_urgent: r.is_urgent,
        system_delivery_date: r.system_delivery_date,
        customer_name: r.customer_name,
    }
}

/// 零件行 → VO（挂上该零件的批次列表）。
fn part_to_out(r: ScanPartRow, children: Vec<ScanBatchOut>) -> ScanPartOut {
    ScanPartOut {
        id: r.id,
        serial_no: r.serial_no,
        name: r.name,
        drawing_no: r.drawing_no,
        status: r.status,
        quantity: r.quantity,
        is_urgent: r.is_urgent,
        system_delivery_date: r.system_delivery_date,
        customer_name: r.customer_name,
        // 零件级 version 仅展示：批次写操作的 OCC 锚是 ScanBatchOut::version
        version: r.version,
        children,
    }
}

/// 批次行 → VO（`version` 取 `t_part_batch.version`，`is_scanned` 由调用方给出）。
fn batch_to_out(r: ScanBatchRow, is_scanned: bool) -> ScanBatchOut {
    ScanBatchOut {
        id: r.id,
        batch_no: r.batch_no,
        quantity: r.quantity,
        status: r.status,
        // ★ 批次版本（t_part_batch.version），不是零件版本
        version: r.version,
        is_repairing: r.is_repairing,
        location: r.location,
        current_holder_display: r.current_holder_display,
        // INSPECTION / DELIVERED 批次恒为 None：出池已清 current_process_id
        process_name: r.process_name,
        is_scanned,
    }
}

// ===========================================================================
//  待品检队列读（`GET /api/v2/prod/inspection/queue`）
//  2026-10-07 自 `prod::batch::service::list` 迁入，规范化口径逐字未改
// ===========================================================================

/// 排序列白名单（`sort_by` → ORDER BY 列名）。
///
/// 映射放在 service 层而不是 repo：`order_col` 会被拼进 SQL 文本，只有经过这张
/// 映射表的 `sort_by` 才能到达 repo —— 外部输入不可能直接成为 SQL 片段。
///
/// 与前端表头 7 列一一对应。
fn resolve_order_col(sort_by: Option<&str>) -> &'static str {
    match sort_by {
        Some("SERIAL_NO") => "p.serial_no",
        Some("DRAWING_NO") => "p.drawing_no",
        Some("NAME") => "p.name",
        Some("BATCH_NO") => "pb.batch_no",
        Some("QUANTITY") => "pb.quantity",
        Some("CUSTOMER_NAME") => "c.name",
        // SYSTEM_DELIVERY_DATE + 缺省 + 非法值统一退化到系统交期
        // （待品检页默认按交期近优先排）。
        _ => "p.system_delivery_date",
    }
}

/// 排序方向：仅 `DESC`（忽略大小写）被接受，其余（含缺省）→ `ASC`。
fn resolve_order_dir(sort_dir: Option<&str>) -> &'static str {
    match sort_dir {
        Some(d) if d.eq_ignore_ascii_case("DESC") => "DESC",
        _ => "ASC",
    }
}

/// 文本筛选参数 → ILIKE pattern：拒绝 `%` / `_` / `\` 后拼 `%...%`。
///
/// 拒绝通配符是**语义**约束，不是注入防护：注入面由 repo 侧 `push_bind` 参数化保证。
/// 拒它的理由是 `%…%` 会被 PG 当通配符放大 —— 表头筛选框只输一个 `%` 就能把整张
/// 表捞出来，1 次请求退化成全表 ILIKE 扫描。空白串视为「不筛选」（筛选框清空态
/// 传空串比传缺省更常见）。
fn to_ilike_pat(field: &str, raw: Option<&str>) -> Result<Option<String>, AppError> {
    let Some(v) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    if v.contains(['%', '_', '\\']) {
        return Err(AppError::validation(format!(
            "{field} 不能包含通配符 % _ \\"
        )));
    }
    Ok(Some(format!("%{v}%")))
}

/// 队列读默认分页大小（200，clamp 区间 `[1, 200]`）。
const DEFAULT_QUEUE_LIMIT: i64 = 200;

/// `prod::inspection` 待品检队列读的 service（ZST，与本域扫码读范本一致）。
pub struct InspectionQueueService;

impl InspectionQueueService {
    /// `GET /api/v2/prod/inspection/queue` 待品检队列列表。
    ///
    /// 返回 `status='INSPECTION'` 的全部活跃批次，出参严格对齐前端待品检页的 7 个
    /// 数据列 + 操作列锚点。
    ///
    /// 流程：角色守卫 → limit/offset 规范化 → `customer_id` 展开为 L1+L2 ids →
    /// 3 个表头文本筛选 trim + 拒通配符 + 拼 `%…%` → 排序白名单映射 →
    /// `list_inspection_queue` + `count_inspection_queue` 两条 SQL（同 WHERE 拼装器）。
    ///
    /// 排序非法值**不报错**：非法 `sort_by` 退化为系统交期、非法 `sort_dir` 退化为
    /// ASC（前端切表头不会拿到 5xx）。
    pub async fn list_queue(
        conn: &mut PgConnection,
        query: &InspectionQueueQuery,
        current: &CurrentUser,
    ) -> Result<InspectionQueueListOut, AppError> {
        current.require_any_role(READ_ROLES)?;

        let limit = query.limit.unwrap_or(DEFAULT_QUEUE_LIMIT).clamp(1, 200);
        let offset = query.offset.unwrap_or(0).max(0);

        // customer_id 展开：单值 → [L1, 所有 L2]；None → 空切片（不过滤）
        let customer_ids_owned: Vec<i64>;
        let customer_ids: &[i64] = if let Some(cid) = query.customer_id {
            customer_ids_owned = expand_customer_id(&mut *conn, cid).await?;
            &customer_ids_owned
        } else {
            &[]
        };

        let drawing_no_pat = to_ilike_pat("drawing_no", query.drawing_no.as_deref())?;
        let name_pat = to_ilike_pat("name", query.name.as_deref())?;
        let serial_no_pat = to_ilike_pat("serial_no", query.serial_no.as_deref())?;

        let filters = InspectionQueueFilters {
            customer_ids,
            drawing_no_pat: drawing_no_pat.as_deref(),
            name_pat: name_pat.as_deref(),
            serial_no_pat: serial_no_pat.as_deref(),
            date_from: query.system_delivery_date_from,
            date_to: query.system_delivery_date_to,
            order_col: resolve_order_col(query.sort_by.as_deref()),
            order_dir: resolve_order_dir(query.sort_dir.as_deref()),
            limit,
            offset,
        };

        let rows = InspectionQueueRepo::list_inspection_queue(&mut *conn, &filters).await?;
        let total = InspectionQueueRepo::count_inspection_queue(&mut *conn, &filters).await?;

        Ok(InspectionQueueListOut {
            items: rows.into_iter().map(InspectionQueueItemOut::from).collect(),
            total,
            limit,
            offset,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{resolve_order_col, resolve_order_dir, to_ilike_pat};

    #[test]
    fn order_col_whitelist_maps_and_degrades() {
        assert_eq!(resolve_order_col(Some("SERIAL_NO")), "p.serial_no");
        assert_eq!(resolve_order_col(Some("DRAWING_NO")), "p.drawing_no");
        assert_eq!(resolve_order_col(Some("NAME")), "p.name");
        assert_eq!(resolve_order_col(Some("BATCH_NO")), "pb.batch_no");
        assert_eq!(resolve_order_col(Some("QUANTITY")), "pb.quantity");
        assert_eq!(
            resolve_order_col(Some("SYSTEM_DELIVERY_DATE")),
            "p.system_delivery_date"
        );
        assert_eq!(resolve_order_col(Some("CUSTOMER_NAME")), "c.name");
        // 缺省 / 非法值 → 系统交期（绝不 500）
        assert_eq!(resolve_order_col(None), "p.system_delivery_date");
        assert_eq!(
            resolve_order_col(Some("p.serial_no; DROP TABLE t_part_batch")),
            "p.system_delivery_date"
        );
    }

    #[test]
    fn order_dir_only_accepts_desc() {
        assert_eq!(resolve_order_dir(Some("DESC")), "DESC");
        assert_eq!(resolve_order_dir(Some("desc")), "DESC");
        assert_eq!(resolve_order_dir(Some("ASC")), "ASC");
        assert_eq!(
            resolve_order_dir(Some("ASC;DROP TABLE t_part_batch")),
            "ASC"
        );
        assert_eq!(resolve_order_dir(None), "ASC");
    }

    #[test]
    fn ilike_pat_rejects_wildcards_and_blanks_out_empty() {
        assert_eq!(
            to_ilike_pat("name", Some("ABC")).unwrap().as_deref(),
            Some("%ABC%")
        );
        assert_eq!(to_ilike_pat("name", Some("  ")).unwrap(), None);
        assert_eq!(to_ilike_pat("name", None).unwrap(), None);
        for bad in ["A%B", "A_B", "A\\B"] {
            assert!(
                to_ilike_pat("name", Some(bad)).is_err(),
                "含通配符应被拒：{bad}"
            );
        }
    }
}

//! prod::process_design 子模块 service 层 —— 业务逻辑
//!
//! 2026-10-05 新增：单个方法 [`ProcessDesignService::list_parts`]。
//!
//! ## 角色守卫
//! 下沉到 service 第一行（沿 `prod::programming::ProgrammingService` 范本），
//! `current.require_any_role(...)`；handler 仅做参数提取，不重复校验。
//!
//! ⚠️ `Role::CncProgrammer` **必须**在白名单内：定工序发生在生产链前端，与「待编程」
//! 页同属 CNC 侧人机界面的一部分，漏掉该角色会直接 40300。
//!
//! ## 事务边界
//! 读端点不开事务（handler `pool.acquire()` 借 `&mut PgConnection`），与
//! `prod::programming` 的 pending 端点一致。

use sqlx::PgConnection;

use crate::auth::rbac::{CurrentUser, Role};
use crate::modules::prod::process_design::repo::{
    ProcessDesignFilters, ProcessDesignRepo, ProcessDesignRow,
};
use crate::modules::prod::process_design::vo::{
    ProcessDesignPartItemOut, ProcessDesignPartListOut,
};
use crate::shared::error::AppError;

use super::dto::ProcessDesignListQuery;

/// 默认分页大小（2026-10-05 新增）。
///
/// ⚠️ 依据**不是**「与前端现调口径一致」：前端 `usePartProcessDesign.ts::loadParts`
/// 调的是 `listParts({ status: 'PENDING' })`，**不传 `limit`**，真正的前端口径是 part
/// 域旧端点 `part/service/crud.rs` 的 `unwrap_or(50).clamp(1, 200)`
/// （**缺省 50 / 封顶 200**）。本端点缺省 200、上限 {@link MAX_LIMIT} `clamp(1, 500)`
/// （500 与 `prod::programming` 同值），比旧端点**更宽**：前端切端点后**首屏行数从 50
/// 涨到 200** —— 这是预期契约后果（待制定工序的零件常超 50 条），不是 bug，量大时
/// 前端按 `offset` 翻页。
const DEFAULT_LIMIT: i64 = 200;

/// 分页上限（service 层 clamp）。500 与 `prod::programming` 同值。
const MAX_LIMIT: i64 = 500;

/// `prod::process_design` service（ZST，与 `prod::programming` 范本一致）。
pub struct ProcessDesignService;

impl ProcessDesignService {
    /// `GET /api/v2/prod/process-design/parts` 业务逻辑。
    ///
    /// 流程：角色守卫 → limit/offset clamp → repo `list` + `count` 两条 SQL →
    /// row → vo 投影。
    ///
    /// ⚠️ 结果集**含装配件子件**（不按 `assembly_id IS NULL` 过滤），见
    /// [`super`] 模块 doc 的警示段。
    pub async fn list_parts(
        conn: &mut PgConnection,
        current: &CurrentUser,
        q: ProcessDesignListQuery,
    ) -> Result<ProcessDesignPartListOut, AppError> {
        current.require_any_role(&[
            Role::Manager,
            Role::Clerk,
            Role::Inspector,
            Role::CncProgrammer,
        ])?;

        // 防御：limit / offset 边界（limit=0 → 1；offset 负数 → 0）
        let limit = q.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
        let offset = q.offset.unwrap_or(0).max(0);

        let f = ProcessDesignFilters {
            sort_dir: q.sort_dir,
            limit,
            offset,
        };

        let rows = ProcessDesignRepo::list(&mut *conn, &f).await?;
        let total = ProcessDesignRepo::count(&mut *conn, &f).await?;
        let items = rows.into_iter().map(row_to_item).collect();

        Ok(ProcessDesignPartListOut {
            items,
            total,
            limit,
            offset,
        })
    }
}

/// row → vo 投影（逐字段直传，无兜底 —— 7 个字段的 DB 列都是原样透出）。
fn row_to_item(r: ProcessDesignRow) -> ProcessDesignPartItemOut {
    ProcessDesignPartItemOut {
        id: r.id,
        version: r.version,
        serial_no: r.serial_no,
        name: r.name,
        drawing_no: r.drawing_no,
        // null = 尚未制定工序（前端据此显示「待制定」）
        process_chain_id: r.process_chain_id,
        // null = 独立零件；非 null = 装配件子件（本端点照常返回，不做过滤）
        assembly_id: r.assembly_id,
    }
}

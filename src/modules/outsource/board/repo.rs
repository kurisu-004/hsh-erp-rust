//! outsource 外协看板聚合 SQL（2026-10-09 新增）
//!
//! 取代 `GET /outsource-pool/{process_id}` + 逐公司 `GET /outsource-pool/state`
//! 的 N+1 组合：旧路径下打开一道工序的板要发 1（工序详情）+ M（每家公司一次
//! state）= M + 1 个请求，M = 公司数；新路径恒定 1 个。
//!
//! ## 两条铁律
//!
//! 1. **SQL 条数固定，与工序数 / 公司数 / 批次数无关。** 逐公司循环查是本文件要
//!    消灭的东西（旧 `/state` 时代前端要发 M 个请求）。每个方法的 doc 写明「固定
//!    N 条」，条数由 `super::mod::sql_count_guard_tests` 钉死。
//! 2. **候选侧谓词只有 `repo/sql.rs::SENDABLE_INNER_X_SQL` 一个落点** —— 看板左列
//!    必须与「可发送」判定同口径，任一改动漏改一处就会让 tab 徽标与 tab 内行数对不上。
//!    本文件只换**投影**（`sendable_dedup_sql` 的两个投影形参 + 外层列清单），
//!    **JOIN 与 WHERE 一行都不重写**。
//!
//! ## 为什么不用 `query!` 宏
//! 照 `prod::queue::board::repo` 的做法：复杂聚合 SQL 字段多、迭代频繁，不进
//! `.sqlx/` 离线缓存（改一个字段要重跑 `sqlx_prepare.sh` 提交一批 hash 文件）。
//! 本文件全部走运行时 `sqlx::query_as` + `#[derive(FromRow)]`，与 outsource 域既有
//! repo 形态一致。
//!
//! ## 时间口径
//! 本文件无日期窗口（候选谓词的交期只参与排序、不参与过滤），故无需从 service 层
//! 绑时间形参。一旦将来加窗口，必须走形参而不是写 SQL 的 `CURRENT_DATE` —— DB 会话
//! 时区与本仓统一的 Asia/Shanghai 是两个时钟。

use std::collections::BTreeMap;

use sqlx::{AssertSqlSafe, PgConnection};

use crate::modules::outsource::repo::sql::{
    NEXT_PROCESS_LATERAL_SQL, SENDABLE_DEDUP_PROJECTION_COUNT, SENDABLE_DEDUP_PROJECTION_FULL,
    SENDABLE_PROJECTION_COUNT, SENDABLE_PROJECTION_FULL, sendable_dedup_sql,
};
use crate::shared::error::{AppError, code};

// ---------------------------------------------------------------------------
// SQL 常量
// ---------------------------------------------------------------------------

/// `snapshot` SQL 1：候选侧按工序分组计数。
///
/// 谓词来自 `repo/sql.rs::SENDABLE_INNER_X_SQL` 的 `x → d` 收敛层（同一份谓词 →
/// 与 [`SQL_CANDIDATES_BY_PROCESS`] 的行粒度**逐行一致**，故
/// `snapshot.processes[].sendable_count == detail.items.len()` 恒成立）。
/// `GROUP BY` 不产 0 行组 ⇒ 只返有候选的工序。
const SQL_SENDABLE_COUNT_BY_PROCESS: &str = "SELECT d.current_process_id AS process_id, \
     COUNT(*)::bigint AS count \
     FROM ( {dedup} ) d \
     GROUP BY d.current_process_id \
     ORDER BY d.current_process_id ASC";

/// `snapshot` SQL 2：在途侧按工序分组计数。
///
/// `current_holder_id IS NOT NULL` 是**承重谓词**，不是可选的防御：`t_part_batch.
/// current_holder_id` 没有 NOT NULL 约束，而另两处落点（[`SQL_HELD_BY_PROCESS`] 与
/// `to_companies` 的 HashMap 分组）都以「holder = 公司 id」为分组键。在途两条 SQL 的
/// 谓词必须逐字同形，否则 tab 徽标会数到一批 detail 取不出来的行（`COUNT(*)` 会计，
/// WHERE 过滤不会）。
const SQL_IN_FLIGHT_COUNT_BY_PROCESS: &str = "SELECT pb.current_process_id AS process_id, \
     COUNT(*)::bigint AS count \
     FROM t_part_batch pb \
     WHERE pb.status = 'OUTSOURCE' \
       AND pb.location = 'OUTSOURCE_COMPANY' \
       AND pb.deleted_at IS NULL \
       AND pb.current_process_id IS NOT NULL \
       AND pb.current_holder_id IS NOT NULL \
     GROUP BY pb.current_process_id \
     ORDER BY pb.current_process_id ASC";

/// `snapshot` SQL 3：工序元数据（一次 `id = ANY($1)` 批量查，零 N+1）。
///
/// `color` 可空（新列，历史行为 NULL）。
///
/// **为什么是本文件自己的一条查询而不是复用 `OutsourceRepoTrait::process_map_short`**：
/// `process_map_short` 返回 `(id, code, name)` 三元组，看板要的第四列是 `color`；扩成
/// 四元组要改它的返回值类型与全部解构点（`service/quote.rs::quote_out_many`），回归面
/// 从看板一条端点扩到整个报价域的出参装配，换来的只是省掉一句 `SELECT`。新开一条同形
/// 查询的成本是几行 SQL，收益是报价域一行不动。
const SQL_PROCESS_META_BY_IDS: &str = "SELECT id, code, name, color, category \
     FROM t_process \
     WHERE id = ANY($1::bigint[]) \
       AND deleted_at IS NULL \
     ORDER BY id ASC";

/// `process_detail` SQL 1：工序元数据（单行）。
const SQL_PROCESS_META_ONE: &str = "SELECT id, code, name, color, category \
     FROM t_process \
     WHERE id = $1 \
       AND deleted_at IS NULL";

/// `process_detail` SQL 2：该工序映射的全部**活跃**外协公司（看板右列的白名单）。
///
/// **不再 `LEFT JOIN t_part_batch` 做 `COUNT`** —— `held_count` 由服务层拿在途批次的
/// 内存分组行数算（`vo::OutsourceQueueCompany.held_count` 的 doc），少一个数字就少
/// 一处「两条 SQL 谓词漂移」的可能。
///
/// `t_outsource_company_process` 上有 partial unique
/// `(outsource_company_id, process_id) WHERE deleted_at IS NULL`，正常每公司只有一条
/// 映射行；用 `MIN()` 包一层是为了不依赖这条唯一性推断（历史脏数据下 GROUP BY 仍只出
/// 一组）。停用 / 软删的公司不出现。
const SQL_COMPANIES_BY_PROCESS: &str = "SELECT c.id AS company_id, c.name \
     FROM t_outsource_company_process cp \
     JOIN t_outsource_company c \
       ON c.id = cp.outsource_company_id \
      AND c.is_active AND c.deleted_at IS NULL \
     WHERE cp.process_id = $1 AND cp.deleted_at IS NULL \
     GROUP BY c.id, c.name \
     ORDER BY MIN(cp.sort_order) ASC, c.id ASC";

/// `process_detail` SQL 3：该工序**全部**在外协批次（跨公司，一次取齐）。
///
/// 这就是消灭 N+1 的那条查询：**没有任何公司谓词**，10 家公司与 2 家公司发的是同一条
/// SQL，只是回更多行。SQL 逐字取自按公司取在途批次的查询，唯一改动是
/// 去掉 `pb.current_holder_id = $1`（并把 `$2` 提成 `$1`）后把
/// `pb.current_holder_id` 投影成 `company_id` 供服务层分组，再补一列 `has_cnc_program`。
///
/// **`AND pb.current_holder_id IS NOT NULL` 是承重谓词**：`current_holder_id` 没有 DB
/// 约束（`bigint` 可空），而本查询按它分组、解码目标是 `i64`（`HeldBatchRow`）——
/// 漏掉它时 sqlx 会把 SQL NULL 解不进 `i64` 而报 `error decoding column`，整个
/// `process_detail` 返 500。加上它既让解码目标保持非可空，也让在途两侧的 SQL 谓词与
/// [`SQL_IN_FLIGHT_COUNT_BY_PROCESS`] 逐字同形。
///
/// **谓词同形不等于「徽标 == 列内卡片数」**：本查询**不带公司谓词**（一次取齐全部
/// 公司的在途批次），而公司列只渲染 [`SQL_COMPANIES_BY_PROCESS`] 那份「活跃 + 已映射」
/// 白名单 ⇒ holder 指向已停用 / 已解映射公司的批次**照旧计入** `snapshot` 的
/// `in_flight`，却在 `to_companies` 的 `HashMap::remove` 处取不到对应列而被整组丢弃。
/// 症状是「tab 徽标 3、列里 0 张卡」（批次发出后停用公司或解映射即可复现）。所以两侧
/// 的差**只可能**来自这一处丢弃，不是谓词漂移。偏差登记见 `docs/api/outsource.md`
/// §8.4（该表同时登记了正确的修法方向：在写入侧禁止停用公司持有在途批次）。
///
/// `LEFT JOIN LATERAL` 派生 `receive_next_process_*`，两步定位与「为什么必须共用同一份
/// 片段」的全部论证见 `repo/sql.rs::NEXT_PROCESS_LATERAL_SQL`（2026-10-09 从本文件
/// 抽成共用常量：移动写端点省略 `to.next_process_id` 时的推导用同一份口径）。
/// 本查询用 `{nx}` 占位符取回该片段，故这条常量是运行时拼装的（走 `AssertSqlSafe`，
/// 拼进去的是编译期常量、无注入面）。
///
/// **`t_applicant` 走 `LEFT JOIN LATERAL (… ORDER BY ap.id ASC LIMIT 1)` 而不是直接
/// JOIN**：`t_part.applicant_name` 是字符串非 FK，而 `t_applicant` 的唯一索引是
/// `(name, customer_id)`，**name 单独不唯一** —— 同名申请人跨客户存在时直接 JOIN 会把
/// 一行批次扇出成多行，破坏下面那个 `held_count == held_batches.len()` 不变量
/// （`batch_id` 重复 + 计数虚高）。投影只有 `ap.name` 一个列，而
/// `ap.name = p.applicant_name` 由 WHERE 保证恒等 ⇒ **取哪一行取值都一样**，
/// `ORDER BY ap.id ASC` 的作用只是给这条 LATERAL 一个确定的行（配合 `LIMIT 1`），零
/// 语义内容；不要把它当成「挑了某个申请人」。
const SQL_HELD_BY_PROCESS: &str = "SELECT pb.id AS batch_id, pb.current_holder_id AS company_id, \
     pb.part_id, pb.batch_no, pb.quantity, \
     p.serial_no, p.drawing_no, p.name, \
     p.system_delivery_date, p.planned_delivery_date, p.is_urgent, \
     c2.name AS customer_name, c1.name AS parent_customer_name, \
     a.name AS applicant_name, \
     pb.location AS batch_location, p.note, \
     pb.version AS batch_version, \
     s.sent_at, s.unit_price::text AS price, \
     EXISTS (SELECT 1 FROM t_part_file pf \
             WHERE pf.part_id = pb.part_id \
               AND pf.kind = 'G_CODE' \
               AND pf.deleted_at IS NULL) AS has_cnc_program, \
     COALESCE(nx.next_process_id, 0) AS receive_next_process_id, \
     nx.next_process_name AS receive_next_process_name \
     FROM t_part_batch pb \
     JOIN t_part p ON p.id = pb.part_id AND p.deleted_at IS NULL \
     LEFT JOIN t_customer c2 ON c2.id = p.customer_id AND c2.deleted_at IS NULL \
     LEFT JOIN t_customer c1 ON c1.id = c2.parent_id AND c1.deleted_at IS NULL \
     LEFT JOIN LATERAL ( \
       SELECT ap.name \
       FROM t_applicant ap \
       WHERE ap.name = p.applicant_name AND ap.deleted_at IS NULL \
       ORDER BY ap.id ASC \
       LIMIT 1 \
     ) a ON TRUE \
     LEFT JOIN t_outsource_shipment s \
       ON s.batch_id = pb.id AND s.status = 'OUTSOURCING' AND s.deleted_at IS NULL \
     {nx} \
     WHERE pb.status = 'OUTSOURCE' \
       AND pb.location = 'OUTSOURCE_COMPANY' \
       AND pb.current_process_id = $1 \
       AND pb.current_holder_id IS NOT NULL \
       AND pb.deleted_at IS NULL \
     ORDER BY pb.current_holder_id ASC, pb.id ASC";

/// `process_detail` SQL 4：该工序的候选卡。
///
/// 分层与候选侧 SQL 的既有形态同构（内层 `x` → `DISTINCT ON` 收敛层 `d` → 外层过滤
/// 与展示序），收敛层的 `DISTINCT ON (batch_id, current_process_id)` 与排序键来自
/// `repo/sql.rs` 的既有常量（`SENDABLE_INNER_X_SQL` / `SENDABLE_DISTINCT_D_SQL` /
/// `sendable_dedup_sql`），只有投影不同。
///
/// 展示序沿用 `repo/sql.rs::SENDABLE_DISPLAY_ORDER` 的前四项（加急优先 → 交期近的
/// 优先 → 同 part → 同批次号），**不再按 `current_process_id` 收尾**：本查询已按
/// `current_process_id = $1` 过滤，该列组内恒定，排序项恒等。
///
/// **不分页**：admin 看板视角，一个 tab 要一次拿全（与 `prod::queue` 的单工序板同
/// 取舍）。
const SQL_CANDIDATES_BY_PROCESS: &str = "SELECT d.batch_version, d.batch_id, d.batch_no, \
     d.batch_quantity, d.shelf_id, \
     d.part_id, d.part_serial_no, d.part_drawing_no, d.part_name, \
     d.planned_delivery_date, d.system_delivery_date, d.is_urgent, \
     d.customer_name, d.parent_customer_name, d.applicant_name, d.note, \
     d.shelf_code, d.requires_approval, d.quote_id, d.price, \
     d.outsource_company_id, d.outsource_company_name, \
     d.has_cnc_program, d.has_process_chain, d.company_options \
     FROM ( {dedup} ) d \
     WHERE d.current_process_id = $1 \
     ORDER BY d.is_urgent DESC, \
              d.planned_delivery_date ASC NULLS LAST, \
              d.part_id ASC, d.batch_no ASC";

// ---------------------------------------------------------------------------
// 行精简（repo ↔ service 边界）
// ---------------------------------------------------------------------------

/// 工序计数行（候选侧 / 在途侧共用一个形状）。
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ProcessCountRow {
    pub process_id: i64,
    pub count: i64,
}

/// 工序元数据行。
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ProcessMetaRow {
    pub id: i64,
    pub code: String,
    pub name: String,
    pub color: Option<String>,
    /// `t_process.category`（DB CHECK 非空）。
    pub category: String,
}

/// 看板右列的一列（公司白名单行；`held_count` 不在这里，见 [`SQL_COMPANIES_BY_PROCESS`]）。
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct CompanyRow {
    pub company_id: i64,
    pub name: String,
}

/// 在途批次行（`SQL_HELD_BY_PROCESS`；服务层按 `company_id` 分组）。
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct HeldBatchRow {
    /// `t_part_batch.current_holder_id`（在途谓词下即外协公司 id）。分组键。
    ///
    /// **解码目标恒非可空**：`t_part_batch.current_holder_id` 本身没有 NOT NULL 约束，
    /// 异常行（`location='OUTSOURCE_COMPANY'` 却没 holder）由
    /// [`SQL_HELD_BY_PROCESS`] 的 `current_holder_id IS NOT NULL` 在 SQL 层剔除 ——
    /// 该谓词与 [`SQL_IN_FLIGHT_COUNT_BY_PROCESS`] 逐字同形，故这批行在 tab 徽标与
    /// detail 里同时消失（而不是只有一边少算）。
    pub company_id: i64,
    pub batch_id: i64,
    pub part_id: i64,
    pub batch_no: i32,
    /// **当前余量**（`t_part_batch.quantity`，不是 shipment.quantity）——
    /// 前端拿它做「部分接收」输入框的 max 值。
    pub quantity: i32,
    pub serial_no: Option<String>,
    pub drawing_no: String,
    pub name: String,
    pub system_delivery_date: Option<chrono::NaiveDate>,
    pub planned_delivery_date: Option<chrono::NaiveDate>,
    pub is_urgent: bool,
    pub customer_name: Option<String>,
    pub parent_customer_name: Option<String>,
    pub applicant_name: Option<String>,
    /// `t_part_batch.location`，恒为 `"OUTSOURCE_COMPANY"`。
    pub batch_location: String,
    pub note: Option<String>,
    pub batch_version: i32,
    pub has_cnc_program: bool,
    pub sent_at: Option<chrono::NaiveDateTime>,
    /// `t_outsource_shipment.unit_price::text`（Decimal 字符串）。
    pub price: Option<String>,
    /// 下一道工序 id；`COALESCE(..., 0)` ⇒ 无下一 step 时为 **0**（0 兜底口径：
    /// JSON 里非 nullable，语义为字符串 `"0"` = 未设）。
    pub receive_next_process_id: i64,
    pub receive_next_process_name: Option<String>,
}

/// 候选卡行（`SQL_CANDIDATES_BY_PROCESS`）。
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct CandidateRow {
    pub batch_version: i32,
    pub batch_id: i64,
    pub batch_no: i32,
    pub batch_quantity: i32,
    pub shelf_id: Option<i64>,
    pub part_id: i64,
    pub part_serial_no: Option<String>,
    pub part_drawing_no: Option<String>,
    pub part_name: Option<String>,
    pub planned_delivery_date: Option<String>,
    pub system_delivery_date: Option<String>,
    pub is_urgent: bool,
    pub customer_name: Option<String>,
    pub parent_customer_name: Option<String>,
    pub applicant_name: Option<String>,
    pub note: Option<String>,
    pub shelf_code: Option<String>,
    pub requires_approval: bool,
    pub quote_id: Option<i64>,
    pub price: Option<String>,
    pub outsource_company_id: Option<i64>,
    pub outsource_company_name: Option<String>,
    pub has_cnc_program: bool,
    /// 工单已绑工序链且批次当前工序能在链内定位（判据见
    /// `shared::batch::chain::HAS_PROCESS_CHAIN_EXPR`，经 `repo/sql.rs` 的
    /// `sendable_dedup_sql` 填进内层投影）。卡片绿色左边框的判据。
    pub has_process_chain: bool,
    /// SQL `to_jsonb(array_agg(json_build_object(...)))` 的结果（单个 JSONB 值，
    /// 不是 `json[]` —— 后者 sqlx 解不进 `serde_json::Value`）。APPROVAL 行恒为
    /// `[]`（SQL 侧 CASE 短路）。
    pub company_options: serde_json::Value,
}

/// `process_detail` 四条 SQL 的 repo ↔ service 边界返回类型。
pub struct QueueProcessData {
    pub process: ProcessMetaRow,
    pub companies: Vec<CompanyRow>,
    /// 该工序**全部**在外协批次（跨公司），服务层按 `company_id` 分组。
    pub held: Vec<HeldBatchRow>,
    pub items: Vec<CandidateRow>,
}

// ---------------------------------------------------------------------------
// repo
// ---------------------------------------------------------------------------

/// 看板聚合 SQL 入口（ZST）。
pub struct OutsourceQueueRepo;

impl OutsourceQueueRepo {
    /// 工序序列板数据。**固定 3 条 SQL（与工序数无关）**：
    ///
    /// 1. 候选侧按工序 `GROUP BY` 计数；
    /// 2. 在途侧按工序 `GROUP BY` 计数；
    /// 3. 工序元数据（`id = ANY($1)` 一次批量，零 N+1）。
    ///
    /// 只返 `sendable + in_flight > 0` 的工序（两侧计数求并集，非零才出 tab）——
    /// 并集与筛选在服务层做，因为第 3 条的 id 集合就是并集的结果。
    pub async fn snapshot(
        conn: &mut PgConnection,
    ) -> Result<
        (
            Vec<ProcessCountRow>,
            Vec<ProcessCountRow>,
            Vec<ProcessMetaRow>,
        ),
        AppError,
    > {
        // 1. 候选侧（谓词真源 = repo/sql.rs 的 SENDABLE_INNER_X_SQL）
        let dedup = sendable_dedup_sql(SENDABLE_PROJECTION_COUNT, SENDABLE_DEDUP_PROJECTION_COUNT);
        let sql = SQL_SENDABLE_COUNT_BY_PROCESS.replace("{dedup}", &dedup);
        let sendable = sqlx::query_as::<_, ProcessCountRow>(AssertSqlSafe(sql))
            .fetch_all(&mut *conn)
            .await?;

        // 2. 在途侧
        let in_flight = sqlx::query_as::<_, ProcessCountRow>(SQL_IN_FLIGHT_COUNT_BY_PROCESS)
            .fetch_all(&mut *conn)
            .await?;

        // 3. 工序元数据（并集后的 id 集合一次 ANY 取齐 —— 零 N+1）
        let all_ids: BTreeMap<i64, ()> = sendable
            .iter()
            .chain(in_flight.iter())
            .map(|c| (c.process_id, ()))
            .collect();
        let meta: Vec<ProcessMetaRow> = if all_ids.is_empty() {
            Vec::new()
        } else {
            let ids: Vec<i64> = all_ids.keys().copied().collect();
            sqlx::query_as::<_, ProcessMetaRow>(SQL_PROCESS_META_BY_IDS)
                .bind(&ids)
                .fetch_all(&mut *conn)
                .await?
        };

        Ok((sendable, in_flight, meta))
    }

    /// 单工序看板数据。**固定 4 条 SQL，与公司数 / 批次数无关**：
    ///
    /// 1. 工序元数据（单行）；
    /// 2. 该工序映射的全部活跃外协公司（`SQL_COMPANIES_BY_PROCESS`，白名单）；
    /// 3. **该工序全部在外协批次一次取齐**（`SQL_HELD_BY_PROCESS`，**无公司谓词** ——
    ///    这是消灭 N+1 的关键：10 家公司与 2 家公司发的是同一条 SQL）；
    /// 4. 该工序候选卡。
    ///
    /// 工序不存在 / 已软删 → `20801 BIZ_PROCESS_NOT_FOUND`（HTTP 404）。
    pub async fn process_detail(
        conn: &mut PgConnection,
        process_id: i64,
    ) -> Result<QueueProcessData, AppError> {
        // 1. 工序元数据
        let process = sqlx::query_as::<_, ProcessMetaRow>(SQL_PROCESS_META_ONE)
            .bind(process_id)
            .fetch_optional(&mut *conn)
            .await?
            .ok_or_else(|| {
                AppError::biz(
                    code::BIZ_PROCESS_NOT_FOUND,
                    format!("process {process_id} 不存在"),
                )
            })?;

        // 2. 公司列白名单（无在途批次的公司也在列）
        let companies = sqlx::query_as::<_, CompanyRow>(SQL_COMPANIES_BY_PROCESS)
            .bind(process_id)
            .fetch_all(&mut *conn)
            .await?;

        // 3. 该工序全部在外协批次（一次查询，无公司谓词）
        let held_sql = SQL_HELD_BY_PROCESS.replace("{nx}", NEXT_PROCESS_LATERAL_SQL);
        let held = sqlx::query_as::<_, HeldBatchRow>(AssertSqlSafe(held_sql))
            .bind(process_id)
            .fetch_all(&mut *conn)
            .await?;

        // 4. 候选卡（不分页：看板一个 tab 一次拿全）
        let dedup = sendable_dedup_sql(SENDABLE_PROJECTION_FULL, SENDABLE_DEDUP_PROJECTION_FULL);
        let cand_sql = SQL_CANDIDATES_BY_PROCESS.replace("{dedup}", &dedup);
        let items = sqlx::query_as::<_, CandidateRow>(AssertSqlSafe(cand_sql))
            .bind(process_id)
            .fetch_all(&mut *conn)
            .await?;

        Ok(QueueProcessData {
            process,
            companies,
            held,
            items,
        })
    }
}

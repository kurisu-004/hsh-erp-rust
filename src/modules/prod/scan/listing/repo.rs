//! prod::scan 报工台两条只读聚合端点的取行 SQL + 行投影
//!
//! 2026-10-10 自 `part::service::phase1::work_type` 逐字搬来（只改返回类型与
//! 参数形态）。
//!
//! ## 两条端点
//! - `pickable` —— `GET /api/v2/prod/scan/pickable?work_type_id=&limit=&offset=`
//! - `held` —— `GET /api/v2/prod/scan/held?worker_id=&limit=&offset=`
//!
//! ## 为什么这个目录必须零跨域依赖
//!
//! 本文件是纯只读聚合：取行 SQL 只碰 `t_part` / `t_part_batch` /
//! `t_work_type_process` / `t_shelf` / `t_process` / `t_process_chain` /
//! `t_process_chain_step`，一处他域的 service / repo 都不 import。守护这条规则的
//! 是 [`super`] 模块 doc 里提到的域隔离护栏（`listing/mod.rs` 的 `mod tests`）。
//!
//! 这也是它**不能**借 `PartRepoTrait` 的原因 —— 那是一个 part 域的胖 trait，
//! import 它就等于把 part 域拖进来。本模块直接收 `&mut PgConnection`。

use chrono::NaiveDate;
use sqlx::PgConnection;

use crate::modules::prod::scan::vo::{ScanChainState, ScanListItem};

/// 报工台两条 list 端点共用的取行投影。
///
/// ## 为什么不是 `TPart` 字面量
/// 2026-10-04 之前两处各手抄一份 20 字段的 `TPart { ... }` 字面量再交给
/// `PartListItem::from`。两个后果：
/// 1. **占位值伪装成业务值** —— 取行 SQL 只投影 `p.id` / `p.serial_no` /
///    `p.drawing_no` 三列，其余全靠字面量填，于是 `name` 填成图号、
///    `is_urgent` 填 `false`、`system_delivery_date` 填 `None`、
///    `planned_delivery_date` 填 1970-01-01。报工台三页的「加急」tag 与交期 chip
///    因此永不渲染，而 `ORDER BY p.is_urgent DESC, p.planned_delivery_date ASC`
///    排的却是 **DB 真实列** ⇒ 列表已按加急排好、工件上看不出任何标记。
/// 2. **漏字段不报错** —— `TPart` 30+ 字段，加字段时两份字面量要手改两处。
///
/// 本 struct 把「SQL 投影了什么」收敛成**一处**事实：字段名即 SQL 别名
/// （`#[derive(sqlx::FromRow)]` 按列名匹配），两条取行 SQL 必须逐字投影每一个字段。
/// ⚠️ 这是**运行期**校验（`sqlx::query_as` 而非 `query_as!` 宏，宏才有编译期校验）：
/// 少投影一列 → 运行时报 missing column；**多投影一列 → 静默忽略**（不报错、不取值）。
/// 端点不消费的字段在 SQL 里显式投影成 `NULL::<type> AS <字段名>`，把「本端点不填」
/// 写进 SQL 而不是靠 struct 缺省 —— 这样「不填」与「漏填」在 SQL 文本上就长得不一样。
///
/// ## 字段填充口径
/// - part 侧 `id` / `serial_no` / `name` / `drawing_no` / `is_urgent` /
///   `planned_delivery_date` / `system_delivery_date`：两条端点都投影**真实值**。
/// - `quantity`：取自**批次**（`pb.quantity`）而非 part。
/// - `process_chain_id`：仅 `held` 投影真实值；`pickable` 投影 `NULL`。
/// - 链四字段：仅 `held` 填（链位置是**批次级**事实，part 级投影无从推导）；
///   `pickable` 侧投影 `NULL` ⇒ 转换时取保守默认 `NONE` / `"0"` / `null` / `null`。
#[derive(Debug, sqlx::FromRow)]
pub struct WorkTypeListRow {
    // ---- part 侧（两条端点均投影真实值）----
    id: i64,
    serial_no: Option<String>,
    name: String,
    drawing_no: String,
    is_urgent: bool,
    planned_delivery_date: NaiveDate,
    /// `t_part.system_delivery_date` 是**可空**列（`date` 无 NOT NULL）。
    system_delivery_date: Option<NaiveDate>,
    // ---- 批次侧 ----
    /// 取自 `pb.quantity`（批次数量），不是 `p.quantity`。
    quantity: i32,
    process_chain_id: Option<i64>,
    batch_id: Option<i64>,
    batch_version: Option<i32>,
    // ---- 工序链派生（仅 held 填）----
    /// 取行 SQL 侧已 `COALESCE(..., 'NONE')`，故 `held` 恒为 `Some`；
    /// `pickable` 侧投影 `NULL::text` ⇒ `None`。
    chain_state: Option<String>,
    /// 取行 SQL 侧已 `COALESCE(..., 0)`；`pickable` 侧投影 `NULL::bigint`。
    chain_next_process_id: Option<i64>,
    chain_next_process_name: Option<String>,
    chain_current_process_name: Option<String>,
    // ---- 工序链存在性派生（两条端点都填）----
    /// `shared::batch::chain::HAS_PROCESS_CHAIN_EXPR` 的结果（判据 = 工单已绑链
    /// 且批次当前工序能在链内定位）。字段名即 SQL 别名（`FromRow` 按列名匹配）。
    ///
    /// **按 `Option<bool>` 收**：`pickable` 侧那条 SELECT 按本文件的约定把
    /// 「本端点不填」投影成 `NULL::boolean`，而 `NULL` 解不进非可空的 `bool`
    /// （sqlx 报 `unexpected null; try decoding as an Option` ⇒ 整页 500）。
    has_process_chain: Option<bool>,
}

impl WorkTypeListRow {
    /// 投影行 → [`ScanListItem`]。
    ///
    /// 这是全仓**唯一**构造这两条端点出参的地方，用**穷尽 struct 字面量**：
    /// `ScanListItem` 加字段时这里编译失败，且字面量让「哪些字段是投影、哪些是
    /// 占位」一眼可辨。
    fn into_scan_list_item(self) -> ScanListItem {
        ScanListItem {
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
            // 未投影链字段的 `pickable` 端点（`None`）取保守默认 `NONE` / `"0"`：
            // `NONE` 语义是「让用户手填下一道工序」，与「不知道」同向。
            chain_state: self
                .chain_state
                .as_deref()
                .map_or(ScanChainState::None, ScanChainState::from_db_text),
            chain_next_process_id: self.chain_next_process_id.unwrap_or(0),
            chain_next_process_name: self.chain_next_process_name,
            chain_current_process_name: self.chain_current_process_name,
            // 链条上「这批货当前工序能不能在链内定位」（卡片绿色边框）。
            // `pickable` 投影 `NULL::boolean` ⇒ 取保守默认 false（「没绑链」）。
            has_process_chain: self.has_process_chain.unwrap_or(false),
            // 这两条端点不做 batch enrichment，位置恒 null。字段保留只为保住
            // 前端 `BatchPickerDialog.holderText` 的「键存在性」判据，理由见
            // `ScanListItem::location` 的 doc。
            location: None,
        }
    }
}

/// 仓库外壳（ZST，与 `prod::queue::board` 的 `QueueBoardRepo` 范本一致）。
///
/// 两条端点各 2 条 SQL（取行 + COUNT），恒定条数、不随工人数或批次数增长。
pub struct ScanListingRepo;

impl ScanListingRepo {
    /// `GET /scan/pickable` 的取行。
    ///
    /// 列：t_part_batch WHERE location=PRODUCTION_SHELF AND
    /// batch.current_process_id IN (工种→工序映射)。
    ///
    /// 2026-09-30 修复（migration 004）：原写法是
    /// `JOIN t_process_chain_step s ON s.id = b.current_process_step_id
    ///  JOIN t_work_type_process wtp ON wtp.process_id = s.process_id` ——
    /// 与此前 queue 池查询同款的 INNER JOIN 盲区：batch 的
    /// current_process_step_id 为 NULL（新下发批次的常态，无工序链工单恒为
    /// NULL）时匹配不到任何 step 行，批次会从「可领取」列表里**整条消失**。
    /// 改直读 `pb.current_process_id`（工序归属的权威列）后该盲区消失。
    ///
    /// `wtp.deleted_at IS NULL` —— 工种↔工序映射走「整组替换」（软删旧行 +
    /// 插新行），不过滤则已取消勾选的工序仍会把批次匹配进本工种的可领取列表。
    /// COUNT 同步用**同一 `wtp` 谓词**，否则 `total` 与 `items` 对不上。
    ///
    /// ⚠️ `p.serial_no` 按 `Option<String>` 收：`t_part.serial_no` 是 nullable
    /// （手工工单无序列号），按 `String` 解码 → 遇到任一 `serial_no IS NULL` 的
    /// part 就整页 500。手工工单是常态，故这是真会触发的路径。
    ///
    /// `sh.deleted_at IS NULL` —— `t_shelf` 的软删守卫；补上后与写侧
    /// `validate_shelf_zone` 走 `ShelfRepo::get_by_id`（带同款守卫）一致：
    /// 软删架上的批次不再出现在结果里。取行与 COUNT 同步补。
    ///
    /// ⚠️ `$2` 是账号的货架 scope 数组、排在 `$3`/`$4`（limit/offset）**之前**
    /// 出现：PG 的 `$n` 只是占位名、不要求按序出现，故把 scope 参数追加在 bind
    /// 列表第二位即可把 `LIMIT`/`OFFSET` 的编号留在尾部（下方 COUNT 无
    /// `$3`/`$4`，它的编号为何要独立连续，见该处注释）。
    ///
    /// ⚠️ **注入面为 0**：`format!` 只填 `HAS_PROCESS_CHAIN_EXPR` 这一个编译期
    /// 常量，其余三个入参一律走 bind，故 `AssertSqlSafe` 包裹安全。
    /// `t_process_chain_step cs` 走 **LEFT JOIN** —— 指针为 NULL 的批次
    /// （无链工单的常态）必须照样出现在可领列表里。
    pub(crate) async fn fetch_pickable(
        conn: &mut PgConnection,
        work_type_id: i64,
        limit: i64,
        offset: i64,
        shelf_scope: Option<Vec<i64>>,
    ) -> Result<Vec<ScanListItem>, sqlx::Error> {
        let sql = format!(
            "SELECT p.id, p.serial_no, p.name, p.drawing_no, p.is_urgent, \
                    p.system_delivery_date, p.planned_delivery_date, pb.quantity, \
                    pb.id AS batch_id, pb.version AS batch_version, \
                    NULL::bigint AS process_chain_id, NULL::text AS chain_state, \
                    NULL::bigint AS chain_next_process_id, \
                    NULL::text AS chain_next_process_name, \
                    NULL::text AS chain_current_process_name, \
                    {} AS has_process_chain \
             FROM t_part_batch pb \
             JOIN t_part p ON p.id = pb.part_id \
             JOIN t_work_type_process wtp ON wtp.process_id = pb.current_process_id \
                AND wtp.deleted_at IS NULL \
             JOIN t_shelf sh ON sh.id = pb.current_holder_id AND sh.deleted_at IS NULL \
             LEFT JOIN t_process_chain_step cs \
               ON cs.id = pb.current_process_step_id AND cs.deleted_at IS NULL \
             WHERE pb.deleted_at IS NULL AND p.deleted_at IS NULL \
               AND pb.status = 'IN_PROCESS' AND pb.location = 'PRODUCTION_SHELF' \
               AND sh.is_active = true AND sh.zone = 'PRODUCTION' \
               AND wtp.work_type_id = $1 \
               AND ($2::bigint[] IS NULL OR sh.id = ANY($2)) \
             ORDER BY p.is_urgent DESC, p.planned_delivery_date ASC, pb.id ASC \
             LIMIT $3 OFFSET $4",
            crate::shared::batch::chain::HAS_PROCESS_CHAIN_EXPR
        );
        let rows: Vec<WorkTypeListRow> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
            .bind(work_type_id)
            .bind(shelf_scope)
            .bind(limit)
            .bind(offset)
            .fetch_all(conn)
            .await?;
        Ok(rows
            .into_iter()
            .map(WorkTypeListRow::into_scan_list_item)
            .collect())
    }

    /// `GET /scan/pickable` 的 COUNT。
    ///
    /// 与取行查询同 `wtp` 谓词（含 `wtp.deleted_at IS NULL`），但**不等于同
    /// WHERE** —— 取行查询额外 `JOIN t_part p` 且带 `p.deleted_at IS NULL`，
    /// 本 COUNT 不 join `t_part`。故软删 part 的 active batch 会计入 `total`
    /// 而不计入 `items`，软删 part 下该工种的可领批次分页总数偏大。是否补 join
    /// 属 `total` 语义决策，未在本处改动。
    ///
    /// scope 谓词与 `sh.deleted_at IS NULL` 两处**必须**与取行同形，否则
    /// `total` 与 `items` 在收窄后对不上（漏任一处都会让「返回空列表但 total
    /// 仍是全厂数」或反之）。
    ///
    /// ⚠️ 本 COUNT 的 scope 参数编号是 `$2` 而**不是** `$4`：PG 扩展协议要求
    /// Parse 消息声明的参数类型个数 **等于** SQL 里被引用的参数个数（个数 =
    /// 被引用的最大 `$n`）。sqlx 按 `.bind()` 个数声明类型，所以本 COUNT 若沿用
    /// 取行的 `$4` 编号，就得额外 bind 两个没人引用的参数，Parse 期直接被 PG 拒
    /// （`bind message supplies 4 parameters, but prepared statement ...
    /// requires 2`）。未被引用的 `$n` 连「参数」都算不上，更不会触发类型推断
    /// 报错 —— 那是另一种失败（被引用但类型推不出，且发生在 Bind/EXECUTE 期）。
    /// 故 COUNT 按自身 bind 顺序连续编号，谓词语义与取行**逐字相同**。
    pub(crate) async fn count_pickable(
        conn: &mut PgConnection,
        work_type_id: i64,
        shelf_scope: Option<Vec<i64>>,
    ) -> Result<i64, sqlx::Error> {
        sqlx::query_scalar(
            "SELECT COUNT(*)::bigint FROM t_part_batch b \
             JOIN t_work_type_process wtp ON wtp.process_id = b.current_process_id \
                AND wtp.deleted_at IS NULL \
             JOIN t_shelf sh ON sh.id = b.current_holder_id AND sh.deleted_at IS NULL \
             WHERE b.deleted_at IS NULL \
               AND b.status = 'IN_PROCESS' AND b.location = 'PRODUCTION_SHELF' \
               AND sh.is_active = true AND sh.zone = 'PRODUCTION' \
               AND wtp.work_type_id = $1 \
               AND ($2::bigint[] IS NULL OR sh.id = ANY($2))",
        )
        .bind(work_type_id)
        .bind(shelf_scope)
        .fetch_one(conn)
        .await
    }

    /// `GET /scan/held` 的取行。
    ///
    /// ⚠️ `p.serial_no` 按 `Option<String>` 收：同上，手工工单（`serial_no IS
    /// NULL`）会把整页打成 500。
    ///
    /// 取行投影含批次锚点（`batch_id` / `batch_version`）、`p.process_chain_id`
    /// 与 4 个链派生列 —— 本端点是报工台放回 / 送检页的唯一数据源，既要定位到
    /// 批次（发得出写请求），也要判「这批是不是链尾 / 下一道是哪道」。
    ///
    /// `LEFT JOIN LATERAL` 派生的三值判据，**两步定位**（纪律与理由见
    /// `shared::batch::chain::CHAIN_POSITION_LATERAL_SQL` 的模块 doc —— 那是
    /// 读写共用的唯一真源，本端点是它的第一个消费方）：
    /// 1. **锚链** = `COALESCE(p.process_chain_id, cur.chain_id)`，`cur` =
    ///    `pb.current_process_step_id` 指向的 step，只用于回退取链 id（该 JOIN
    ///    无行 ⇒ 锚链解析失败 ⇒ 落 `NONE`）；中间 JOIN `t_part_process_chain`
    ///    是为了让「锚链已软删」同样落 `NONE`。
    /// 2. **当前 step 在锚链内的位置**：片段按 `pb.current_process_id` 在锚链内
    ///    **重新定位**（`cur2` inner `JOIN LATERAL`），再取锚链内 `sort_order`
    ///    大于它且最小的那个未软删 step。命中 >1 视作歧义落 `NONE`。`cur2` 是
    ///    inner `JOIN LATERAL`：定位不到时整个派生子查询无行，故片段里的
    ///    `CASE` 没有「定位不到」这一分支（该路径由本处
    ///    `COALESCE(nx.chain_state, 'NONE')` 兜底）。
    ///
    /// ⚠️ **第 2 步绝对不能拿 `pb.current_process_step_id` 的 `sort_order` 直接当
    /// 位置** —— step 指针与「当前工序在链内的位置」是两个独立事实，而 worker-scan
    /// 的 RETURNED 分支在**非顺应工序**时只写 `current_process_id`、step 指针留在
    /// 原处（`ChainPosition::is_pointer_consistent` 为 false 时前端必须显式指定
    /// 下一道工序）。于是指针漂移的批次在放回时按 `sort_order` 推进会把**当前
    /// 工序自己**当成下一道返回，而 `chain_state` 仍在说「可免填」⇒ 写侧照单
    /// 全收，静默错值比拒收更难发现。同一批次第 N 次放回都只能靠
    /// `current_process_id` 定位。
    ///
    /// ⚠️ **链内同一 `process_id` 允许重复，读侧必须自己识别歧义**：片段的
    /// `cur2` 侧用 `(count(*) OVER ())` 带出命中数，`hit_count > 1` 时**显式落
    /// `NONE`**（与「未知一律往保守方向降」一致），并同时门控 `current_step_id` /
    /// `current_sort_order` / `nsp` 三个派生侧：歧义时不产出任何派生值，维持
    /// `NONE` ⇒ 下一道 id 为 `"0"`、两个名字均为 `null` 的不变量。
    ///
    /// ⚠️ **「下一道」按 `sort_order > 当前 ORDER BY ASC LIMIT 1` 取，不按
    /// `= 当前 + 1`**：与写侧正典
    /// `prod::process_chain::repo::query::next_step_in_chain` 逐条同形，读侧不会
    /// 替写侧产生分歧。而 `sort_order` 的**密度不由读侧决定**：写侧只保证链内
    /// `sort_order` 互不重复，稠密 0-based 与稀疏 `10/20/30` 两种密度都能落库且
    /// 都受支持。别拿任何文档当密度依据 —— 写路径才是权威。`+ 1` 只在稠密下正确、
    /// 在稀疏下会把「还有两道工序」误判成链尾，`>` 对两种密度都成立。
    ///
    /// 4 个投影列都显式 `AS chain_*` 别名，与 [`WorkTypeListRow`] 的字段名逐字
    /// 对应，避免内外层列名不一致时读错位。
    ///
    /// ⚠️ **两个工序名改由外层 LEFT JOIN `t_process` 取**：片段只导出 id
    /// （`nx.next_process_id` / `nx.current_step_id`），名字在外层查。两个 JOIN
    /// 谓词与「片段内取名」写法逐条等价 ——
    ///   - 「下一道」侧 `np.id = nx.next_process_id AND np.deleted_at IS NULL`；
    ///   - 「当前工序」侧 `cp.id = pb.current_process_id AND cp.deleted_at IS NULL
    ///     AND nx.current_step_id IS NOT NULL`，其中 `hit_count = 1` 的门控换成
    ///     外层等价物 `current_step_id IS NOT NULL`（零命中时 `cur2` 那块 inner
    ///     LATERAL 整块无行；≥2 命中时片段显式落 `NONE` 且 `current_step_id`
    ///     保持 NULL，只有恰好 1 命中才会带出它）。
    ///
    /// `t_process.id` 是主键，两条 LEFT JOIN 都不改变行数。
    ///
    /// ⚠️ **`cp` 的软删闸门不可丢**：`np` 是 LEFT JOIN、未命中时整行 NULL，
    /// 若谓词写 `np.deleted_at IS NULL` 则该子句恒为真（命中时它也是 join 条件
    /// 的一部分），等于把 `cp` 的 `deleted_at IS NULL` 删掉 —— 批次的
    /// `current_process_id` 指向已软删工序时 `chain_current_process_name` 会从
    /// `null` 变成该软删工序的名字。
    ///
    /// ⚠️ **别名契约**：片段引用 `p`（`t_part`）与 `pb`（`t_part_batch`）两个别名，
    /// 故本 SELECT 的批次别名是 `pb`。
    ///
    /// 外层 LATERAL 末尾 `ORDER BY cur.id ASC LIMIT 1` 收口（片段内）：不为消歧
    /// （`cur` / `pc` 都按主键定位，本就至多一行），而是把「至多一行」这条不变量
    /// 写进 SQL —— 不收口则一旦上游改动放宽了任一 JOIN，一行批次就会扇成多行、
    /// 破坏 VO 层「`items.len()` 等于持有批次数」的不变量。排序键 `cur.id` 在任何
    /// 假设的扇行里都是同一个常量、打不破平局，故这个 `ORDER BY` 只表达行数上界，
    /// **不买确定性**。
    ///
    /// `p.process_chain_id` / `pb.current_process_id` /
    /// `pb.current_process_step_id` 全部是可空列：列本身可空时 `query_as` 返回的
    /// `O` 仍须是 `Option<T>`（外层 `Result<Option<O>>` 那层 `Option` 只表示
    /// 「有没有行」）。`chain_state` 的 `COALESCE(..., 'NONE')` 在最外层兜底：
    /// 无链批次的 `current_process_step_id` 按写入不变式恒为 NULL ⇒ `cur` 无行
    /// ⇒ LATERAL 无行 ⇒ 派生列全 NULL，此时必须仍给出 `NONE` / `0`。
    ///
    /// ⚠️ **注入面为 0**：`format!` 只填 `HAS_PROCESS_CHAIN_EXPR` 与
    /// `CHAIN_POSITION_LATERAL_SQL` 两个编译期常量，`worker_id` / `limit` /
    /// `offset` 一律走 bind，故 `AssertSqlSafe` 安全。`t_process_chain_step cs`
    /// 走 **LEFT JOIN**（无 step 的批次必须照样出现在放回列表里）。
    pub(crate) async fn fetch_held(
        conn: &mut PgConnection,
        worker_id: i64,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<ScanListItem>, sqlx::Error> {
        let sql = format!(
            "SELECT p.id, p.serial_no, p.name, p.drawing_no, p.is_urgent, \
                    p.system_delivery_date, p.planned_delivery_date, pb.quantity, \
                    pb.id AS batch_id, pb.version AS batch_version, p.process_chain_id, \
                    COALESCE(nx.chain_state, 'NONE') AS chain_state, \
                    COALESCE(nx.next_process_id, 0) AS chain_next_process_id, \
                    np.name AS chain_next_process_name, \
                    cp.name AS chain_current_process_name, \
                    {} AS has_process_chain \
             FROM t_part_batch pb \
             JOIN t_part p ON p.id = pb.part_id \
             LEFT JOIN LATERAL ( {} ) nx ON TRUE \
             LEFT JOIN t_process np \
               ON np.id = nx.next_process_id AND np.deleted_at IS NULL \
             LEFT JOIN t_process cp \
               ON cp.id = pb.current_process_id AND cp.deleted_at IS NULL \
              AND nx.current_step_id IS NOT NULL \
             LEFT JOIN t_process_chain_step cs \
               ON cs.id = pb.current_process_step_id AND cs.deleted_at IS NULL \
             WHERE pb.deleted_at IS NULL AND p.deleted_at IS NULL \
               AND pb.status = 'IN_PROCESS' AND pb.location = 'WORKER' \
               AND pb.current_holder_id = $1 \
             ORDER BY pb.id DESC LIMIT $2 OFFSET $3",
            crate::shared::batch::chain::HAS_PROCESS_CHAIN_EXPR,
            crate::shared::batch::chain::CHAIN_POSITION_LATERAL_SQL
        );
        let rows: Vec<WorkTypeListRow> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
            .bind(worker_id)
            .bind(limit)
            .bind(offset)
            .fetch_all(conn)
            .await?;
        Ok(rows
            .into_iter()
            .map(WorkTypeListRow::into_scan_list_item)
            .collect())
    }

    /// `GET /scan/held` 的 COUNT。链派生只影响 items 的字段取值，不改变行的
    /// 增删口径（`t_part` 侧与 items 的不对称是既有语义决策，见
    /// [`Self::count_pickable`] 的注释）。
    pub(crate) async fn count_held(
        conn: &mut PgConnection,
        worker_id: i64,
    ) -> Result<i64, sqlx::Error> {
        sqlx::query_scalar(
            "SELECT COUNT(*)::bigint FROM t_part_batch b \
             WHERE b.deleted_at IS NULL \
               AND b.status = 'IN_PROCESS' AND b.location = 'WORKER' \
               AND b.current_holder_id = $1",
        )
        .bind(worker_id)
        .fetch_one(conn)
        .await
    }
}

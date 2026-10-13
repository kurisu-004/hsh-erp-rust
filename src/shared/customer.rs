//! 跨域客户树 helper（2026-10-07 新增；2026-10-17 扩成全仓唯一的客户树设施）
//!
//! 本模块只放「客户树（**任意深度**，2026-10-17 起）遍历」这一件事——多个域的列表
//! 端点都要把 Query 里的单个 `customer_id` 展开成一组 ids 才能进 SQL 的
//! `= ANY($n)`，口径必须**全仓一份**，否则同一页面的不同入口会给出不同的行集。
//!
//! 五个公开函数，**互不派生**（各自可独立调用，调用方按需要取）：
//! - [`subtree_ids`]：向下展开成「自身 + 全部后代」的纯子树（筛选用）
//! - [`root_id_of`]：向上走到根（`parent_id IS NULL`），派发序列号 / 派生根属性用
//! - [`has_children`]：有没有未软删的直接子节点（软删闸门用）
//! - [`paths_of`]：一次往返批量产出 `(root_name, own_name, customer_path)`（出参用）
//! - [`require_leaf_customer`]：业务单据「只能挂叶子」的闸门（2026-10-17 新增，
//!   前一轮各域各写一份，现收敛到本函数）
//!
//! 全部只读 `t_customer`，不带任何权限 / 分页口径，故可放在 shared 而非某个域的
//! service：调用方各自用自己的连接句柄传入，本模块自身不开事务（**事务边界在
//! handler**，见 `CLAUDE.md` 领域结构一节）。
//!
//! ## 为什么不做成 repo
//! 各域的客户表访问面差别很大（customer 域是全量 CRUD，`part` 域只在筛选时
//! 读一列 id），为「读两列」单独造一个跨域 trait 只会把域隔离变成一层空壳。
//! 共享的粒度取在**函数**而不是**仓储**上：谁需要谁拿连接句柄来调。
//!
//! ## 2026-10-17 的语义变更（原 `expand_customer_id` → `subtree_ids`）
//! 客户层级从**固定两层**（L1 集团 + L2 叶子）放开为**任意深度**（对齐部门架构：
//! 集团 / 分厂 / 车间 / 班组…）。旧 `expand_customer_id` 的两条硬编码语义因此作废：
//! - 旧「L1 → 自身 + 全部 L2」：形状与新的「根 → 自身 + 全部后代」一致，仅深度从
//!   2 层变成任意层；
//! - 旧「L2 → 自身 + **同 L1 下所有兄弟 L2**」：这是两层时代的「向上找根再全展开」
//!   历史包袱，会把**别人的兄弟节点**混进筛选结果（新口径下「选某节点 = 该节点 +
//!   其子树」，兄弟节点不在其中）。2026-10-17 起一律按**纯子树**展开。

use std::collections::HashMap;

use sqlx::{PgConnection, QueryBuilder};

use crate::shared::error::{AppError, code};

/// 2026-10-17 新增：客户树向上遍历的深度上限。
///
/// 正常业务路径**不可能**触顶（`create` 只接受已存在的 parent、`update` 拒改
/// `parent_id`，每加一层都要有人手工建节点），它是给「历史数据 / 手工改库造出环」
/// 兜底的**硬刹车**：`parent_id` 成环时没有根可找，无界递归会一直跑到语句超时，
/// 而超时在现场表现为「接口卡住几十秒后 500」，比一个明确的 400 难查得多。
/// 取 32 层：真实组织架构（集团/分厂/车间/班组）离它很远，误触顶只能是脏数据。
pub const MAX_CUSTOMER_DEPTH: i32 = 32;

/// `customer_path` 各段之间的分隔符（2026-10-17 新增）。**前后各一个空格**是产品
/// 锁定的字面量，勿改成 `/` 或窄空格。
const PATH_SEP: &str = " / ";

/// 单个客户在树中的三个名字（2026-10-17 新增）。
///
/// - `root_name`：该节点所属**根**（L1 集团）的名字 —— 对应出参 `l1_customer_name`
/// - `own_name`：本节点自己的名字 —— 对应出参 `customer_name`
/// - `path`：**第 2 层到叶子**用 `" / "` 拼接的路径，对应出参 `customer_path`
///   （**不含**根节点自己的名字；本节点即根时 = `own_name`）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CustomerPath {
    pub root_name: String,
    pub own_name: String,
    pub path: String,
}

/// 向下展开 `cid` 的**纯子树**：`[cid]` + 其任意深度的全部后代 id。
///
/// 语义（2026-10-17 起）：选中某节点 = 该节点**与其子树**，不再含任何兄弟节点
/// （旧两层实现的「L2 → 同 L1 下所有兄弟 L2」已作废，见模块 doc）。
///
/// 递归 CTE（`WITH RECURSIVE`）一次往返拿齐，**不限深度**。防环用 `UNION`（去重）
/// 而非 `UNION ALL`：环形脏数据下 `UNION` 收敛到不动点后终止，`UNION ALL` 会无限
/// 展开直到语句超时。正常数据下两者结果一致（id 天然唯一），故为安全性付出的一次
/// 哈希去重是划算的。
///
/// 客户不存在（含软删）→ `20102 BIZ_CUSTOMER_NOT_FOUND`：筛选条件指向一个已经
/// 消失的客户时，让调用方拿到空集去查表会把「客户没录进来」伪装成「该客户没有
/// 工单」，前端只能显示一个空列表页，无法提示用户。
///
/// 返回的 ids **已去重**（CTE 用 `UNION`）、**不排序**，由调用方按自己的 SQL 语义
/// 消费（`= ANY($n)` 对重复项与顺序均不敏感）。
pub async fn subtree_ids(conn: &mut PgConnection, cid: i64) -> Result<Vec<i64>, AppError> {
    let mut qb: QueryBuilder<sqlx::Postgres> =
        QueryBuilder::new("WITH RECURSIVE subtree AS (SELECT id FROM t_customer WHERE id = ");
    qb.push_bind(cid);
    qb.push(
        " AND deleted_at IS NULL UNION SELECT c.id FROM t_customer c \
         INNER JOIN subtree s ON c.parent_id = s.id WHERE c.deleted_at IS NULL) \
         SELECT id FROM subtree",
    );
    let ids: Vec<i64> = qb.build_query_scalar().fetch_all(&mut *conn).await?;
    if ids.is_empty() {
        return Err(AppError::biz(
            code::BIZ_CUSTOMER_NOT_FOUND,
            format!("customer {cid} 不存在"),
        ));
    }
    Ok(ids)
}

/// 沿 `parent_id` 链向上走到根（`parent_id IS NULL` 的那一行），返回根 id。
///
/// 2026-10-17 新增。派发序列号取根的 `serial_prefix`、判定「是否 L1」等都依赖它。
/// 旧的 `COALESCE(parent_id, id)` 子查询**只折一层**，在三级及以上的树上会把中间层
/// 当根（中间层的 `serial_prefix` 恒为 NULL ⇒ 派号必然失败）。
///
/// 防环两道闸（`parent_id` 成环时没有根可找，无界递归会跑到语句超时）：
/// - `seen` 路径数组：已访问过的 id 不再上跳（环在第二步就被切断）；
/// - `depth < MAX_CUSTOMER_DEPTH`：兜住「seen 没覆盖到的异常形态」（如链尾指向一个
///   自身 `parent_id` 自指的脏行）。
///
/// 两种失败：
/// - 客户不存在 / 已软删（`chain` 为空）→ `20102 BIZ_CUSTOMER_NOT_FOUND`
/// - 客户在但**解析不出根**（成环 / 链尾断在**不存在**的父行 / 根行已软删 / 超过
///   深度上限）→ `20104 BIZ_INVALID_VALUE`。刻意**不**复用 20102：那不是「客户没录进来」，
///   而是库里的树已经坏了，报 20102 会把排查方向指到「用户没建客户」。
///
/// ## 软删祖先：**可以穿过**（2026-10-17 与 [`paths_of`] / `shared::serial::
/// prefix_for_customer` 统一口径）
/// 递归步**不**带 `p.deleted_at IS NULL` —— 中途祖先即使已软删也照样穿过，只有
/// **根行**必须未软删（外层的 `AND deleted_at IS NULL` 判据）。三处向上递归必须同口径：
/// 同一个客户若在「取根」和「取路径」上给出不同答案，派号与展示就会互相矛盾。
pub async fn root_id_of(conn: &mut PgConnection, cid: i64) -> Result<i64, AppError> {
    // CTE 里必须带出 `deleted_at`：外层要在 CTE 结果上判「根行未软删」，
    // CTE 不投影这一列的话 PG 直接报 `column "deleted_at" does not exist`。
    let mut qb: QueryBuilder<sqlx::Postgres> = QueryBuilder::new(
        "WITH RECURSIVE up AS (SELECT id, parent_id, deleted_at, 0 AS depth, \
         ARRAY[id] AS seen FROM t_customer WHERE id = ",
    );
    qb.push_bind(cid);
    qb.push(
        " AND deleted_at IS NULL \
         UNION ALL \
         SELECT p.id, p.parent_id, p.deleted_at, u.depth + 1, u.seen || p.id FROM up u \
         INNER JOIN t_customer p ON p.id = u.parent_id \
         WHERE NOT (p.id = ANY(u.seen)) AND u.depth < ",
    );
    qb.push_bind(MAX_CUSTOMER_DEPTH);
    // 无 GROUP BY 的 SELECT 恒返 1 行，故 `chain_len` 恒拿得到；`root_id` 可为 NULL
    // （链里没有 `parent_id IS NULL` 的行 = 解析不出根）。
    qb.push(
        ") SELECT (SELECT id FROM up WHERE parent_id IS NULL AND deleted_at IS NULL \
         ORDER BY depth ASC LIMIT 1) AS root_id, (SELECT count(*) FROM up) AS chain_len",
    );
    let row: Option<(Option<i64>, i64)> = qb.build_query_as().fetch_optional(&mut *conn).await?;
    let (root_id, chain_len) = row.ok_or_else(|| {
        AppError::biz(
            code::BIZ_CUSTOMER_NOT_FOUND,
            format!("customer {cid} 不存在"),
        )
    })?;
    if chain_len == 0 {
        return Err(AppError::biz(
            code::BIZ_CUSTOMER_NOT_FOUND,
            format!("customer {cid} 不存在或已软删"),
        ));
    }
    root_id.ok_or_else(|| {
        AppError::biz(
            code::BIZ_INVALID_VALUE,
            format!(
                "customer {cid} 的 parent_id 链解析不出根（成环 / 父行不存在或根行已软删 / \
                 超过最大深度 {MAX_CUSTOMER_DEPTH}），客户树数据异常"
            ),
        )
    })
}

/// 是否存在**直接**子节点（任意未软删）。
///
/// 2026-10-17 新增。判「客户还能不能软删」只需看直接子节点：树里任何非根节点都
/// 必有一个直接父，故「有任意深度的后代」等价于「有直接子节点」，而直接子节点
/// 一次 `EXISTS` 就够，不必展开子树。
///
/// 不查「自己是不是根」：一个没有子节点的根客户是可以软删的（只要 part/assembly
/// 引用为 0）。
pub async fn has_children(conn: &mut PgConnection, cid: i64) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM t_customer WHERE parent_id = $1 AND deleted_at IS NULL)",
    )
    .bind(cid)
    .fetch_one(&mut *conn)
    .await
}

/// 一次往返批量产出每个 id 的 `(root_name, own_name, customer_path)`。
///
/// 2026-10-17 新增（`customer_path` 出参的数据来源）。向上递归 CTE：每个目标 id
/// 展开成一条到根的链，`array_agg(name ORDER BY depth DESC)` 收集成
/// **根 → … → 自身** 的名字数组，Rust 侧再按口径拼路径。
///
/// **为什么要拼在 Rust 侧而不是 SQL 侧**：路径口径（「去掉根、其余用 ` / ` 拼」）是
/// 产品规则，`slice(1,).join(" / ")` 一行就能表达且能脱离数据库单测；写成 SQL 里的
/// `array_to_string(agg[2:], …)` 还要额外处理「数组长度 1 时 `agg[2:1]` 返回 NULL」
/// 这个坑，且口径改一次就得改一段字符串拼接。
///
/// 递归 CTE 逐行携带 `target_id`（沿链不变），故同一批 id 的链在一条查询里并行
/// 上溯、最后按 `target_id` 分组，互不串味。防环两道闸同 [`root_id_of`]。
///
/// ## 软删祖先：**可以穿过**（2026-10-17 统一口径）
/// 递归步**不**带 `p.deleted_at IS NULL`，与 [`root_id_of`] / `shared::serial::
/// prefix_for_customer`] 两处向上递归逐字一致：**只有终点行（被查询的那个 id 本身）
/// 必须未软删**。此前本函数独一份「软删祖先即断链」的判据，会让同一个客户在
/// 「取路径」与「取根 / 取序列号前缀」上给出互相矛盾的答案（前者截断、后者穿过）。
///
/// ## 解析不出根时 map 里**仍有**该 id，但 `root_name` 会退化
/// `GROUP BY target_id` 对每个未软删的终点行都产出一组，故「map 里没有」只意味着
/// 「终点行不存在 / 已软删」，**不**意味着「树断了」。成环 / 超过
/// [`MAX_CUSTOMER_DEPTH`] 时链条在防环闸门处停下，`chain.first()` 拿到的是**断链处
/// 那个祖先**（不是根），`path` 是对应的截断路径。正常写入路径进不来这两种形态
/// （`create` 要求父行存在且未软删、`update` 拒改 `parent_id`），它们只出现在脏数据里。
///
/// 空 `ids` 短路返回空 map（省一次无谓往返）。
pub async fn paths_of(
    conn: &mut PgConnection,
    ids: &[i64],
) -> Result<HashMap<i64, CustomerPath>, AppError> {
    let mut out: HashMap<i64, CustomerPath> = HashMap::new();
    if ids.is_empty() {
        return Ok(out);
    }
    let mut qb: QueryBuilder<sqlx::Postgres> = QueryBuilder::new(
        "WITH RECURSIVE up AS ( \
         SELECT c.id AS target_id, c.id, c.parent_id, c.name, 0 AS depth, ARRAY[c.id] AS seen \
         FROM t_customer c WHERE c.id = ANY(",
    );
    qb.push_bind(ids);
    qb.push(
        ") AND c.deleted_at IS NULL \
         UNION ALL \
         SELECT u.target_id, p.id, p.parent_id, p.name, u.depth + 1, u.seen || p.id \
         FROM up u INNER JOIN t_customer p ON p.id = u.parent_id \
         WHERE NOT (p.id = ANY(u.seen)) AND u.depth < ",
    );
    qb.push_bind(MAX_CUSTOMER_DEPTH);
    // depth 越大越靠近根，故 DESC 序 = 根在前、自身在后。
    qb.push(
        ") SELECT target_id, array_agg(name ORDER BY depth DESC) AS chain_names \
         FROM up GROUP BY target_id",
    );
    let rows: Vec<(i64, Vec<String>)> = qb.build_query_as().fetch_all(&mut *conn).await?;
    for (target_id, chain) in rows {
        // chain 非空恒成立（GROUP BY 分组至少有一行），`last()` 不会 None。
        let own_name = chain.last().cloned().unwrap_or_default();
        let root_name = chain.first().cloned().unwrap_or_default();
        out.insert(
            target_id,
            CustomerPath {
                root_name,
                own_name: own_name.clone(),
                path: join_customer_path(&chain),
            },
        );
    }
    Ok(out)
}

/// 业务单据只能挂**叶子**客户的闸门（2026-10-17 新增）。
///
/// 客户树从固定两层放开为任意深度后，「挂一个 L2」不再蕴含「一定是最深一层」——
/// 中间层节点同样有子节点；若不拦，建出来的工单 / 装配件 / 外协报价挂在一个还有
/// 下级的客户上，筛选口径（选中节点 = 自身 + 子树）与展示口径都会失真。
///
/// 有未软删子节点 → `20104 BIZ_INVALID_VALUE`（**不是** `20124`，那一条是**客户软删**
/// 闸门专用），消息带上客户 id / 路径与下级节点数，便于现场直接定位到要改的树节点。
///
/// `subject` 是**域内业务名**（如「工单」「装配件」「外协报价」），只进 message —— 本函数
/// 不区分单据类型，判据对所有业务单据是同一条，错误文案不该由闸门自己决定。
///
/// ⚠️ 「有几层下级」按**下级节点总数**（子树大小 - 1）给出而不是层数：树深需要逐层
/// 遍历，而节点数一次 [`subtree_ids`] 就有，且对用户来说「你下面还有 3 个子节点」比
/// 「还有 2 层」更可操作。
///
/// ⚠️ **不分「客户不存在」**：本函数只判 `has_children`，而 `has_children` 对不存在的
/// 客户也返 `false`（等于「无子节点」）⇒ 走到这一步直接 `Ok` 早退。调用方必须**各自
/// 先做存在性闸门**（part / assembly / outsource 三处都是），否则会把「客户没录进来」
/// 伪装成「挂到了合法叶子」。反之，只要 `has_children` 返 `true`，后面的
/// [`subtree_ids`] 会顺带把存在性闸住（锚点行不存在 / 已软删 → `20102`）。
///
/// 2026-10-17 由 part / assembly / outsource 三份域内副本收敛到本函数：三份的判据、
/// 错误码、消息结构本已逐字相同，只是各写一遍，改口径必然漏改一处。
pub async fn require_leaf_customer(
    conn: &mut PgConnection,
    customer_id: i64,
    subject: &str,
) -> Result<(), AppError> {
    if !has_children(&mut *conn, customer_id).await? {
        return Ok(());
    }
    let descendants = subtree_ids(&mut *conn, customer_id)
        .await?
        .len()
        .saturating_sub(1);
    let paths = paths_of(&mut *conn, &[customer_id]).await?;
    // ⚠️ 下面的 `unwrap_or_else` **当前恒不可达**，是防御分支而**不是**「罕见分支」：
    // 上一步的 `subtree_ids` 锚点是 `WHERE id = $1 AND deleted_at IS NULL`，锚点查不到
    // 行就当场返 `20102`；`paths_of` 的锚点则是**同一谓词**
    // （`c.id = ANY($1) AND c.deleted_at IS NULL`）⇒ 凡是能走到这一行的，
    // `paths` 必含 `customer_id`。（本函数**只**判 `has_children`，所以「客户不存在」
    // 这个洞在 `has_children` 那一步就早退了，进不到这里。）
    //
    // **为什么不改成 `.expect()`**：这条不变式是「两条 SQL 的锚点谓词恰好一致」这个
    // **数据层事实**，改任一条都不会编译报错。把它做成 panic，等于让一次查询口径调整
    // 变成线上 500；保留回落分支 ⇒ 口径若将来真被改紧，最坏只是错误消息里少一段路径，
    // 仍可读（回落成「customer {id}」而不是空串：空串拼进 message 读起来像漏了值）。
    let label = paths
        .get(&customer_id)
        .map(|p| p.path.clone())
        .unwrap_or_else(|| format!("customer {customer_id}"));
    Err(AppError::biz(
        code::BIZ_INVALID_VALUE,
        format!(
            "{subject}只能挂在叶子客户上：{label}（id={customer_id}）下还有 {descendants} 个\
             子节点，请改挂到它的下级客户"
        ),
    ))
}

/// 按产品口径拼 `customer_path`：`chain` 是**根在前**的名字数组。
///
/// - 本节点即根（`chain.len() == 1`）→ 就是根自己的名字
/// - 否则 → 取「第 2 层 … 叶子」这段（**去掉根**）用 `" / "` 拼接
///
/// 纯函数（不碰 DB），故口径改动的回归保护是下面的单测而不是集成测试。
fn join_customer_path(chain_root_first: &[String]) -> String {
    match chain_root_first {
        [] => String::new(),
        [only] => only.clone(),
        _ => chain_root_first[1..].join(PATH_SEP),
    }
}

#[cfg(test)]
mod tests {
    use super::{MAX_CUSTOMER_DEPTH, PATH_SEP, join_customer_path};

    fn chain(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    /// 根节点：`customer_path` 就是它自己的名字（不是空串、也不带任何分隔符）。
    #[test]
    fn path_of_root_is_its_own_name() {
        assert_eq!(join_customer_path(&chain(&["华强"])), "华强");
    }

    /// 直接挂在根下的第 2 层：只有它自己一段（第 1 层是根，被去掉）。
    #[test]
    fn path_of_second_level_is_its_own_name() {
        assert_eq!(join_customer_path(&chain(&["华强", "终端BG"])), "终端BG");
    }

    /// 产品锁定的三层例子：`华强 / 终端BG / 手机部` 里，手机部的 `customer_path`
    /// **不含** L1「华强」，分隔符前后各一个空格。
    #[test]
    fn path_drops_the_root_segment() {
        assert_eq!(
            join_customer_path(&chain(&["华强", "终端BG", "手机部"])),
            "终端BG / 手机部"
        );
    }

    /// 四层（任务书里的原始例子）：`华强 / 终端BG / 手机部 / 影像组` 里，
    /// 叶子影像组的 `customer_path` = `"终端BG / 手机部 / 影像组"`。
    #[test]
    fn path_of_four_level_tree_matches_product_example() {
        assert_eq!(
            join_customer_path(&chain(&["华强", "终端BG", "手机部", "影像组"])),
            "终端BG / 手机部 / 影像组"
        );
    }

    /// 空链（不该出现：SQL 侧 GROUP BY 分组恒非空）不得 panic，兜底空串。
    #[test]
    fn path_of_empty_chain_is_empty_string() {
        assert_eq!(join_customer_path(&[]), "");
    }

    /// 分隔符字面量锁死（`" / "`：前后各一个空格）。分隔符是出参契约的一部分，
    /// 前端按它拆段做面包屑，改窄了/改成裸 `/` 会让展示层与其它按 ` / ` 切分的地方
    /// 对不上，故单独锁一条。
    #[test]
    fn path_separator_literal_is_space_slash_space() {
        assert_eq!(PATH_SEP, " / ");
    }

    /// 深度上限是「成环兜底」用的，不是「正常业务上限」：32 层远高于真实组织架构，
    /// 真触顶只可能是脏数据。把它锁死是为了防止有人顺手调到 2 / 3（那会让三级
    /// 客户在**正常数据**上被误判成脏数据，而症状是偶发的 400）。
    #[test]
    fn max_depth_is_well_above_any_real_hierarchy() {
        // 用 `const { assert!(..) }` 而不是普通 `assert!`：被断言的两边都是常量，
        // 直接 `assert!` 会撞 clippy::assertions_on_constants（本仓 clippy 带
        // `-D warnings`，普通 `assert!` 在这里就是编译错误）。
        const MIN_ACCEPTABLE: i32 = 8;
        const {
            assert!(
                MAX_CUSTOMER_DEPTH >= MIN_ACCEPTABLE,
                "真实组织架构至少有 4 层，留足余量"
            )
        };
    }
}

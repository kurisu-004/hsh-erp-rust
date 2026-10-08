//! 自动选架：按负载挑一个目标货架。
//!
//! 2026-10-10 起，「把批次落到某个架上」的 8 条写路径不再由调用方传 `shelf_id`，
//! 改由本模块按 **当前负载 / capacity 升序** 挑一个。替代关系与各端点的错误码
//! 映射见 `docs/api/shelves.md` 与 `docs/api/batch.md`。

use sqlx::{AssertSqlSafe, PgConnection, Row};

use crate::auth::rbac::CurrentUser;
use crate::auth::rbac::Role;
use crate::shared::error::{AppError, code};

use super::load::{LOAD_AGGREGATE_SQL, ShelfLoad, load_ratio};

/// `pick_least_loaded` 的 SQL 模板（`{load_agg}` 由 [`LOAD_AGGREGATE_SQL`] 填）。
///
/// 走**运行时** `sqlx::query` + `Row::get`（不进 `.sqlx/` 离线缓存）—— 与
/// `prod::queue::board::repo` 同一理由：这条 SQL 字段多、随 `capacity` 语义调整
/// 迭代频繁，而离线缓存意味着每次改一个投影就要重跑 `sqlx_prepare.sh` 提交一批
/// 哈希文件。
///
/// ## 候选集（WHERE 三条）
///
/// 1. `zone = $1 AND is_active AND NOT deleted`：只在该区的**活跃**架里选。
///    停用 / 软删的架与批次位置不一致，选中它会让批次从此失联。
/// 2. `($2::bigint IS NULL OR EXISTS (t_shelf_process …))`：给了 `process_id`
///    就只在**映射了该工序**的架里选。`EXISTS` 自带 `deleted_at IS NULL` 闸门 ——
///    已软删的映射不参与候选。
/// 3. `($3::bigint[] IS NULL OR s.id = ANY($3))`：`shelf_scope_for` 给出的货架
///    白名单。这是**安全边界**不是优化：`SHELF_ACCOUNT` 的 `shelf_ids` 是手填
///    白名单，不收窄 scope 会让它拿到全厂货架（与 `can_access_shelf` 同一条边界）。
///    `Some(vec![])` 会让 `ANY('{}')` 对任何架都假 ⇒ 候选为空 ⇒ 返 `Ok(None)`，
///    由调用方转成 40301 / 20508。这正是「未绑架的 SHELF_ACCOUNT 看不到任何架」
///    想要的语义，**不能**把它误判成「不限」。
///
/// ## 排序（4 段，缺一不可）
///
/// ```sql
/// ORDER BY
///   (CASE WHEN s.capacity IS NULL OR s.capacity <= 0 THEN 1 ELSE 0 END) ASC,
///   (CASE WHEN s.capacity IS NULL OR s.capacity <= 0 THEN 0
///         ELSE COALESCE(load.cnt, 0)::numeric / s.capacity END) ASC,
///   s.display_order ASC, s.id ASC
/// LIMIT 1
/// ```
///
/// - **第 1 段**：不限架恒排最后。没配 `capacity` 的货架在第 2 段的比例是 `NULL`，
///   PG 的 `NULLS LAST` 默认值能把它排最后，但那是**偶然**（下一段若改成
///   `NULLS FIRST` 就会反过来），且不限架与 `load_ratio = 0` 的架在语义上完全不同
///   （前者没上限、后者还很空）。显式的一段把「不限」钉成排序的**第一**判据。
/// - **第 2 段**：比例升序。`COALESCE(load.cnt, 0)` 不可省 —— `LEFT JOIN` 未命中
///   时 `load.cnt` 是 SQL NULL，`NULL::numeric / capacity` 也是 NULL，于是**空架
///   会被排到有货的架之后**（PG `ASC` 默认 NULLS LAST），与「空架最该被选中」
///   正好相反。超载（比例 > 1.0）不拒（见 [`load_ratio`] 的 doc）。
/// - **第 3、4 段**：稳定兜底，保证同样的输入恒选到同一个架（否则同一条业务在
///   两台应用上会选到不同的架，批次分布会无谓地抖动）。
///
/// **退化路径**：候选集里**全部**架都没配 `capacity` 时，第 1 段全部为 1、第 2
/// 段全部为 0，排序自然退化到 `display_order ASC, id ASC` —— 即「按人工排的物理
/// 顺序取第一个可用架」，与旧的 `find_first_shelf_for_process`（`sort_order ASC,`
/// `id ASC LIMIT 1`）同形。**刻意不**调那个旧实现来做退化：那会让本层 import
/// `crate::modules::prod::shelf_process`，违反「`shared::shelf` 零域依赖」。
/// 两者口径的细微差别（`sort_order` vs `display_order`）见 `docs/api/shelves.md`
/// 的已知偏差登记。
const SQL_PICK_LEAST_LOADED: &str = "SELECT s.id, s.code, s.name, s.zone, s.location, s.capacity, \
         COALESCE(load.cnt, 0)::bigint AS current_load \
         FROM t_shelf s \
         LEFT JOIN ({load_agg}) load ON load.shelf_id = s.id \
         WHERE s.zone = $1 \
           AND s.is_active = true \
           AND s.deleted_at IS NULL \
           AND ($2::bigint IS NULL OR EXISTS (SELECT 1 FROM t_shelf_process sp \
                WHERE sp.shelf_id = s.id AND sp.process_id = $2 \
                  AND sp.deleted_at IS NULL)) \
           AND ($3::bigint[] IS NULL OR s.id = ANY($3)) \
         ORDER BY \
           (CASE WHEN s.capacity IS NULL OR s.capacity <= 0 THEN 1 ELSE 0 END) ASC, \
           (CASE WHEN s.capacity IS NULL OR s.capacity <= 0 THEN 0 \
                 ELSE COALESCE(load.cnt, 0)::numeric / s.capacity END) ASC, \
           s.display_order ASC, s.id ASC \
         LIMIT 1";

/// 按负载挑一个货架。
///
/// - `process_id = Some(_)`：只在该工序映射到的 `zone` 架里选（生产架路径）。
/// - `process_id = None`：在给定 `zone` 的全部活跃架里选（品检架没有工序映射）。
/// - `scope`：`shelf_scope_for(&current)` 的原样透传，`None` = 不限、`Some(vec![])`
///   = 一个都看不见。**必须传**（见模块 doc 的候选集第 3 条）。
///
/// 候选为 0 行时**不返回错误**、只返回 `Ok(None)` —— 由调用方决定该转成哪个错误
/// 码（20508「该工序无可用生产货架」/ 40301「你无权访问任何品检架」/ 20501…）。
/// 选架层把「没有候选」翻译成业务码等于替调用方猜语义，而这三处的语义确实不同。
///
/// 单一事务内多条写路径都会各调一次本函数；同一事务内批次还没落架时负载不变，
/// 故连着放的批次会连续选到同一个「最空」的架 —— 这是「按当前负载」这一口径的
/// 直接后果，不是缺陷（批次先落架再放下一批时负载已变）。
pub async fn pick_least_loaded(
    conn: &mut PgConnection,
    zone: &str,
    process_id: Option<i64>,
    scope: Option<Vec<i64>>,
) -> Result<Option<ShelfLoad>, AppError> {
    let sql = SQL_PICK_LEAST_LOADED.replace("{load_agg}", LOAD_AGGREGATE_SQL);
    let row = sqlx::query(AssertSqlSafe(sql))
        .bind(zone)
        .bind(process_id)
        .bind(scope.as_deref())
        .fetch_optional(&mut *conn)
        .await?;
    let Some(r) = row else {
        return Ok(None);
    };
    let current_load: i64 = r.get("current_load");
    let capacity: Option<i32> = r.get("capacity");
    Ok(Some(ShelfLoad {
        id: r.get("id"),
        code: r.get("code"),
        name: r.get("name"),
        zone: r.get("zone"),
        location: r.get("location"),
        capacity,
        current_load,
        load_ratio: load_ratio(current_load, capacity),
    }))
}

/// `CurrentUser` → 选架 scope。
///
/// **逐条对齐** `modules::part::service::phase1::work_type::pickable_shelf_scope`
/// （那是 `GET /parts/by-work-type/{id}` 读侧筛选货架的同一判据）：
///
/// - `shelf_wildcard || has_role(Role::Manager)` → `None`（SQL 不加谓词）
/// - 其余 → `Some(current.shelf_ids.clone())`
///
/// 这与 [`CurrentUser::can_access_shelf`] 的三个 disjunct 同源，所以「选架选出
/// 来的架」与「读侧看得见的架」恒同集合。
///
/// ## ⚠️ 空数组必须原样返回 `Some(vec![])`
///
/// `ANY('{}')` 对任何货架都为假 ⇒ 候选为空 ⇒ `pick_least_loaded` 返 `Ok(None)`。
/// 这正是「一个 `shelf_ids` 为空的 `SHELF_ACCOUNT` 看不见任何架」想要的语义。
/// 若把它误判成 `None`（= 不限），该账号会拿到**全厂**货架 —— 而它的
/// `can_access_shelf` 在读侧对任何架都返 false，两侧口径当场分裂。
pub fn shelf_scope_for(current: &CurrentUser) -> Option<Vec<i64>> {
    if current.shelf_wildcard || current.has_role(Role::Manager) {
        None
    } else {
        Some(current.shelf_ids.clone())
    }
}

/// 选架失败且该 zone 是 `INSPECTION` 时的**统一**错误码。
///
/// 语义是「你的 scope 里没有任何可用的品检架」，不是「这个架不存在」。返 20501 会
/// 让调用方去查架的配置，而实际成因往往是该 `SHELF_ACCOUNT` 只绑了生产架。
///
/// 这是既有约束的延续而非新增限制：自动选架之前，操作员在 UI 上选品检架时同样会被
/// `worker-scan` 的 `40301 can_access_shelf(target)` 拒。只是错误码的**触发时机**
/// 从「选了一个越权的架」变成「scope 内没有品检架」。成因与后果登记见
/// `docs/api/batch.md`。
#[inline]
pub fn no_candidate_in_scope(zone: &str) -> AppError {
    AppError::biz(
        code::SHELF_MISMATCH,
        format!("当前账号无权访问任何可用的 {zone} 货架（scope 内无候选）"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::shared::test_snowflake::shared_test_snowflake;
    use hsh_erp_test_support::test_pool;

    /// 造一个 `t_shelf` 行（`capacity` 缺省 = NULL = 不限）。
    async fn insert_shelf(
        pool: &sqlx::PgPool,
        code: &str,
        zone: &str,
        capacity: Option<i32>,
        display_order: i32,
    ) -> i64 {
        let id = shared_test_snowflake().next_id();
        sqlx::query(
            "INSERT INTO t_shelf (id, code, name, zone, capacity, is_active, display_order) \
             VALUES ($1, $2, $2, $3, $4, true, $5)",
        )
        .bind(id)
        .bind(code)
        .bind(zone)
        .bind(capacity)
        .bind(display_order)
        .execute(pool)
        .await
        .expect("insert t_shelf");
        id
    }

    /// 造一个 `t_process` 行（`category='INHOUSE'`，选架只关心映射存在与否）。
    async fn insert_process(pool: &sqlx::PgPool, code: &str) -> i64 {
        let id = shared_test_snowflake().next_id();
        sqlx::query(
            "INSERT INTO t_process (id, code, name, category) VALUES ($1, $2, $2, 'INHOUSE')",
        )
        .bind(id)
        .bind(code)
        .execute(pool)
        .await
        .expect("insert t_process");
        id
    }

    async fn map_shelf_to_process(pool: &sqlx::PgPool, shelf_id: i64, process_id: i64) {
        let id = shared_test_snowflake().next_id();
        sqlx::query(
            "INSERT INTO t_shelf_process (id, shelf_id, process_id, sort_order) \
             VALUES ($1, $2, $3, 0)",
        )
        .bind(id)
        .bind(shelf_id)
        .bind(process_id)
        .execute(pool)
        .await
        .expect("insert t_shelf_process");
    }

    /// 在某个架上堆 `total` 件在架负载（`SUM(quantity)` 口径：1 个批次 quantity=N）。
    async fn add_load(pool: &sqlx::PgPool, shelf_id: i64, total_quantity: i32, tag: &str) {
        let part_id = insert_part(pool, tag).await;
        let batch_id = shared_test_snowflake().next_id();
        sqlx::query(
            "INSERT INTO t_part_batch (id, part_id, batch_no, quantity, status, location, \
             current_holder_id, version) \
             VALUES ($1, $2, 1, $3, 'IN_PROCESS', 'PRODUCTION_SHELF', $4, 0)",
        )
        .bind(batch_id)
        .bind(part_id)
        .bind(total_quantity)
        .bind(shelf_id)
        .execute(pool)
        .await
        .expect("insert t_part_batch");
    }

    /// 造一个 `t_part`（`t_part.customer_id` NOT NULL，故先造根 L1 客户）。
    async fn insert_part(pool: &sqlx::PgPool, name: &str) -> i64 {
        let customer_id = shared_test_snowflake().next_id();
        sqlx::query(
            "INSERT INTO t_customer (id, name, version, created_at, updated_at) \
             VALUES ($1, $2, 0, now(), now())",
        )
        .bind(customer_id)
        .bind(format!("Co-{name}"))
        .execute(pool)
        .await
        .expect("insert t_customer");
        let id = shared_test_snowflake().next_id();
        sqlx::query(
            "INSERT INTO t_part (id, name, drawing_no, applicant_name, quantity, unit_price, \
             total_price, request_date, planned_delivery_date, customer_id, status, version, \
             created_at, updated_at) \
             VALUES ($1, $2, $3, 'Tester', 1, 1.00, 1.00, CURRENT_DATE, CURRENT_DATE, $4, \
                     'PENDING', 0, now(), now())",
        )
        .bind(id)
        .bind(name)
        .bind(format!("DWG-{name}"))
        .bind(customer_id)
        .execute(pool)
        .await
        .expect("insert t_part");
        id
    }

    /// 比例最低胜出：capacity 100/200/150，在架 80/100/100 ⇒ 比例 80%/50%/67%。
    #[tokio::test]
    async fn picks_least_loaded_ratio() {
        let pool = test_pool().await;
        let s100 = insert_shelf(&pool, "PL100", "PRODUCTION", Some(100), 0).await;
        let s200 = insert_shelf(&pool, "PL200", "PRODUCTION", Some(200), 1).await;
        let s150 = insert_shelf(&pool, "PL150", "PRODUCTION", Some(150), 2).await;
        // 品检架：本 zone 的候选必须互不影响
        insert_shelf(&pool, "INS-1", "INSPECTION", Some(1), 0).await;
        add_load(&pool, s100, 80, "PLL-100").await;
        add_load(&pool, s200, 100, "PLL-200").await;
        add_load(&pool, s150, 100, "PLL-150").await;

        let mut conn = pool.acquire().await.expect("acquire");
        let picked = pick_least_loaded(&mut conn, "PRODUCTION", None, None)
            .await
            .expect("选架不应报错")
            .expect("候选非空");
        assert_eq!(picked.id, s200, "比例 50% 应胜出（80% / 50% / 67%）");
        assert_eq!(picked.current_load, 100);
        assert_eq!(picked.capacity, Some(200));
        assert!((picked.load_ratio.expect("有 capacity") - 0.5).abs() < 1e-9);
    }

    /// 超载不拒：全部候选 ≥100% 时仍返比例最低的那个。
    #[tokio::test]
    async fn does_not_reject_when_all_candidates_overloaded() {
        let pool = test_pool().await;
        let s50 = insert_shelf(&pool, "OV50", "PRODUCTION", Some(50), 0).await;
        let s100 = insert_shelf(&pool, "OV100", "PRODUCTION", Some(100), 1).await;
        add_load(&pool, s50, 100, "OVL-50").await; // 200%
        add_load(&pool, s100, 120, "OVL-100").await; // 120%

        let mut conn = pool.acquire().await.expect("acquire");
        let picked = pick_least_loaded(&mut conn, "PRODUCTION", None, None)
            .await
            .expect("超载不报错")
            .expect("超载也要选出一个");
        assert_eq!(picked.id, s100, "120% < 200%，选比例低的");
        assert!(picked.load_ratio.expect("有 capacity") > 1.0);
    }

    /// 不限排最后：有一个没配 capacity 的空架 + 一个装了 1 件但有 capacity 的架 ⇒
    /// 选后者（比例 1% < 不限）。
    #[tokio::test]
    async fn unbounded_shelves_sort_last() {
        let pool = test_pool().await;
        let bounded = insert_shelf(&pool, "UB-B", "PRODUCTION", Some(100), 5).await;
        let unbounded = insert_shelf(&pool, "UB-U", "PRODUCTION", None, 0).await;
        add_load(&pool, bounded, 1, "UBL-B").await;

        let mut conn = pool.acquire().await.expect("acquire");
        let picked = pick_least_loaded(&mut conn, "PRODUCTION", None, None)
            .await
            .expect("不应报错")
            .expect("候选非空");
        assert_eq!(picked.id, bounded);
        assert_eq!(
            picked.capacity,
            Some(100),
            "不限架（capacity=NULL）必须排在一个装了 1 件但有上限的架之后"
        );
        let _ = unbounded;
    }

    /// 给了 `process_id` 时只在映射了该工序的架里选。
    #[tokio::test]
    async fn process_id_restricts_candidates_to_mapped_shelves() {
        let pool = test_pool().await;
        let mapped = insert_shelf(&pool, "PM-A", "PRODUCTION", Some(100), 1).await;
        let other = insert_shelf(&pool, "PM-B", "PRODUCTION", Some(100), 0).await;
        let process_id = insert_process(&pool, "PM-PROC").await;
        map_shelf_to_process(&pool, mapped, process_id).await;
        // `other` 更靠前（display_order=0）且不被映射 → 给了 process_id 后必须落 `mapped`
        let mut conn = pool.acquire().await.expect("acquire");
        let picked = pick_least_loaded(&mut conn, "PRODUCTION", Some(process_id), None)
            .await
            .expect("不应报错")
            .expect("候选非空");
        assert_eq!(picked.id, mapped);
        let _ = other;
    }

    /// scope 过滤：`Some(vec![])` 返空（不是「不限」），`Some(vec![x])` 只在 x 里选，
    /// `None` 不受限。
    #[tokio::test]
    async fn scope_filters_candidates_and_empty_scope_yields_none() {
        let pool = test_pool().await;
        let allowed = insert_shelf(&pool, "SC-A", "PRODUCTION", Some(100), 0).await;
        insert_shelf(&pool, "SC-B", "PRODUCTION", Some(100), 1).await;
        let mut conn = pool.acquire().await.expect("acquire");

        let empty = pick_least_loaded(&mut conn, "PRODUCTION", None, Some(vec![]))
            .await
            .expect("空 scope 不报错");
        assert!(empty.is_none(), "空 scope 必须返 None（= 什么都看不到）");

        let scoped = pick_least_loaded(&mut conn, "PRODUCTION", None, Some(vec![allowed]))
            .await
            .expect("不应报错")
            .expect("scope 内有候选");
        assert_eq!(scoped.id, allowed);

        let unbounded = pick_least_loaded(&mut conn, "PRODUCTION", None, None)
            .await
            .expect("不应报错")
            .expect("不限 scope 下候选非空");
        assert!(unbounded.id == allowed || unbounded.id != allowed);
    }

    /// zone 隔离：`INSPECTION` 只在品检架里选，PRODUCTION 架不参与。
    #[tokio::test]
    async fn zone_isolates_candidates() {
        let pool = test_pool().await;
        insert_shelf(&pool, "ZI-P", "PRODUCTION", Some(100), 0).await;
        let insp = insert_shelf(&pool, "ZI-I", "INSPECTION", Some(100), 0).await;
        let mut conn = pool.acquire().await.expect("acquire");
        let picked = pick_least_loaded(&mut conn, "INSPECTION", None, None)
            .await
            .expect("不应报错")
            .expect("候选非空");
        assert_eq!(picked.id, insp);
    }

    /// 停用 / 软删的架不参与候选。
    #[tokio::test]
    async fn inactive_and_soft_deleted_shelves_are_excluded() {
        let pool = test_pool().await;
        let disabled = insert_shelf(&pool, "DS-OFF", "PRODUCTION", Some(100), 0).await;
        let deleted = insert_shelf(&pool, "DS-DEL", "PRODUCTION", Some(100), 1).await;
        let alive = insert_shelf(&pool, "DS-ON", "PRODUCTION", Some(100), 2).await;
        sqlx::query("UPDATE t_shelf SET is_active = false WHERE id = $1")
            .bind(disabled)
            .execute(&pool)
            .await
            .expect("disable");
        sqlx::query("UPDATE t_shelf SET deleted_at = now() WHERE id = $1")
            .bind(deleted)
            .execute(&pool)
            .await
            .expect("soft delete");

        let mut conn = pool.acquire().await.expect("acquire");
        let picked = pick_least_loaded(&mut conn, "PRODUCTION", None, None)
            .await
            .expect("不应报错")
            .expect("候选非空");
        assert_eq!(picked.id, alive);
    }

    /// `shelf_scope_for` 与 `can_access_shelf` 的三个 disjunct 同源。
    #[test]
    fn shelf_scope_for_aligns_with_can_access_shelf() {
        let mk = |wildcard: bool, is_manager: bool, ids: Vec<i64>| CurrentUser {
            id: 1,
            username: "u".into(),
            roles: if is_manager {
                vec![Role::Manager]
            } else {
                vec![Role::Clerk]
            },
            shelf_wildcard: wildcard,
            shelf_ids: ids,
        };
        // wildcard / Manager → 不限
        assert!(shelf_scope_for(&mk(true, false, vec![])).is_none());
        assert!(shelf_scope_for(&mk(false, true, vec![])).is_none());
        // 其余 → Some(shelf_ids)，空数组原样保留
        assert_eq!(
            shelf_scope_for(&mk(false, false, vec![1, 2])),
            Some(vec![1, 2])
        );
        assert_eq!(shelf_scope_for(&mk(false, false, vec![])), Some(vec![]));
    }
}

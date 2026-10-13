//! customer 域端到端集成测试
//!
//! ## 覆盖（Phase P1 customer CRUD 段）
//! 1. create L1（带 serial_prefix）+ create L2（带 parent_id）+ soft-delete L1 → 20113
//!    BIZ_CUSTOMER_IN_USE（因为 L1 仍被 t_part 引用）。
//! 2. update L1 的 serial_prefix 与另一 L1 撞 unique → 20104
//!
//! ## 2026-10-17 新增（客户层级放开为任意深度）
//! 3. 有子节点的客户软删 → 20124 `BIZ_CUSTOMER_HAS_CHILDREN`（中间层 / 根两处，
//!    并断言消息里的下级节点数是「整棵子树 - 1」而非直接子节点数）。
//! 4. 叶子软删成功；子节点软删后父节点闸门放行。
//! 5. `create` 的 `parent_id` 指向不存在 / 已软删的客户 → 20102。
//! 6. 4 层树上 `shared::customer::subtree_ids` 对中间层返回「自身 + 全部后代」，
//!    且**不含兄弟节点**（纯子树语义）；不存在的客户报 20102。
//! 7. 成环的 `parent_id` 链：`root_id_of` 在有限时间内报 20104、`subtree_ids` 收敛
//!    （环只能用裸 SQL 造，API 路径造不出来）。
//! 8. `paths_of` 的出参口径（`customer_path` 不含根名、根自身 path = 自己）。
//! 9. 列表不传 `limit` ⇒ 返回全量（>50 条）；显式传 `limit` 仍分页。
//!
//! ## 并行
//! 进程级 test_pool 每次 fresh database（plan 2 2026-09-20），DB 间 schema
//! 完全独立，无需 Mutex 串行化。
//!
//! ## 认证
//! 用 MANAGER 用户跑通（POST /com/customers 写路径要求 M/C，按设计 §6.1 用 M 即可；2026-09-19 聚合到 com nest）。
//!
//! ## Fixture 范本化（2026-09-24 PR13 Phase I）
//! 本文件原 `#[path = "common/mod.rs"] mod common;` + `use common::{...};` 改走
//! `use hsh_erp_test_support::*` + `load_customer_fixture(&pool)` +
//! `CustomerFixture` + `bootstrap_as_manager` 样板。fixture 提供 1 MANAGER user
//! baseline（测试内现场创建 L1/L2 customer，避免 fixture 占用 serial_prefix 字面）。
//! 字面请求 / 断言逐字保留。

use sqlx::PgPool;

use hsh_erp_test_support::{
    CustomerFixture, json_request, load_customer_fixture, send as ts_send, test_app, test_pool,
    test_state,
};

// ===========================================================================
//  Helpers
// ===========================================================================

async fn send(
    app: axum::Router,
    req: axum::http::Request<axum::body::Body>,
) -> (axum::http::StatusCode, serde_json::Value) {
    ts_send(app, req).await
}

/// 重新构造 Router（oneshot 消耗 Router 之后）。
async fn fresh_app(pool: &PgPool) -> axum::Router {
    test_app(test_state(pool.clone()).await)
}

/// 基础 bootstrap：fresh DB + customer fixture 2 行（baseline MANAGER user）+ 登录拿 token。
async fn bootstrap_as_manager() -> (PgPool, String) {
    let pool = test_pool().await;
    let _fx = load_customer_fixture(&pool).await;
    let app = test_app(test_state(pool.clone()).await);
    let (_, env) = send(
        app,
        json_request(
            "POST",
            "/iam/login",
            Some(serde_json::json!({"username": CustomerFixture::MANAGER_USERNAME, "password": CustomerFixture::PASSWORD})),
            None,
        ),
    )
    .await;
    let token = env["data"]["token"].as_str().unwrap().to_string();
    (pool, token)
}

/// 建一个客户并返回它的雪花 id（字符串形态，与 `CustomerOut.id` 的 JSON 形态一致）。
///
/// `parent_id` 缺省 ⇒ 建根（必须给 `serial_prefix`）；给了 `parent_id` ⇒ 建非根
/// （不得给 `serial_prefix`，由 service 校验）。
async fn create_customer_id(
    pool: &PgPool,
    token: &str,
    name: &str,
    parent_id: Option<&str>,
    serial_prefix: Option<&str>,
) -> String {
    let mut body = serde_json::json!({ "name": name });
    if let Some(p) = parent_id {
        body["parent_id"] = serde_json::json!(p);
    }
    if let Some(sp) = serial_prefix {
        body["serial_prefix"] = serde_json::json!(sp);
    }
    let (s, env) = send(
        fresh_app(pool).await,
        json_request("POST", "/com/customers", Some(body), Some(token)),
    )
    .await;
    assert_eq!(s, axum::http::StatusCode::CREATED, "create {name}: {env}");
    env["data"]["id"].as_str().unwrap().to_string()
}

/// 软删一个客户，返回 `(HTTP 状态, 信封)`。
async fn soft_delete(
    pool: &PgPool,
    token: &str,
    id: &str,
) -> (axum::http::StatusCode, serde_json::Value) {
    send(
        fresh_app(pool).await,
        json_request(
            "POST",
            &format!("/com/customers/{id}/soft-delete"),
            None,
            Some(token),
        ),
    )
    .await
}

/// 直插一个 `t_part` 行（customer_id = given），让 soft-delete 检查「被 part 引用」分支
/// 触发 `20113 BIZ_CUSTOMER_IN_USE`。绕开 part 域 CRUD（part CRUD 不是本任务范畴）。
async fn insert_part_with_customer(pool: &PgPool, customer_id: i64) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;
    // 2026-10-09：ID 统一从全进程共享 generator 取（`shared_test_snowflake`）——
    // 两个 fresh generator 同 instance 同毫秒各取 seq 0 会撞 `t_part_pkey`（23505），
    // 与「同一个 helper 调几次」无关。
    let id = hsh_erp_test_support::shared_test_snowflake().next_id();
    let now = now_naive();
    sqlx::query!(
        "INSERT INTO t_part (id, name, drawing_no, applicant_name, quantity, unit_price, total_price, \
         request_date, planned_delivery_date, customer_id, version, created_at, updated_at) \
         VALUES ($1, 'TEST-NAME', 'TEST-DWG', 'TEST-APPLICANT', 1, 0, 0, CURRENT_DATE, CURRENT_DATE, $2, 0, $3, $3)",
        id,
        customer_id,
        now,
    )
    .execute(pool)
    .await
    .expect("insert t_part referencing customer");
    id
}

// ===========================================================================
//  Tests
// ===========================================================================

#[tokio::test]
async fn create_customer_root_then_l2_then_soft_delete_in_use() {
    let (pool, token) = bootstrap_as_manager().await;

    // Create L1
    let (s1, env1) = send(
        fresh_app(&pool).await,
        json_request(
            "POST",
            "/com/customers",
            Some(serde_json::json!({"name": "ACME", "serial_prefix": "A"})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s1, axum::http::StatusCode::CREATED, "create L1: {env1}");
    assert_eq!(env1["code"], 0);
    let l1_id = env1["data"]["id"].as_str().unwrap().to_string();
    assert_eq!(env1["data"]["serial_prefix"], "A");

    // Create L2 (parent_id = l1_id)
    let (s2, env2) = send(
        fresh_app(&pool).await,
        json_request(
            "POST",
            "/com/customers",
            Some(serde_json::json!({
                "name": "ACME-Workshop1",
                "parent_id": l1_id.clone(),
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s2, axum::http::StatusCode::CREATED, "create L2: {env2}");
    assert_eq!(env2["code"], 0);
    assert_eq!(env2["data"]["parent_id"], l1_id);

    // Insert a t_part referencing L1 directly so soft-delete check fires
    // (brief's service logic only inspects t_part / t_assembly).
    insert_part_with_customer(&pool, l1_id.parse::<i64>().unwrap()).await;

    // 2026-10-17：子节点闸门排在单据引用闸门**之前**，「L1 仍有 L2」现在报 20124。
    // 本用例守的是 20113（单据引用），故先把 L2（叶子）软删掉再软删 L1。
    let (sd_leaf, env_leaf) =
        soft_delete(&pool, &token, env2["data"]["id"].as_str().unwrap()).await;
    assert_eq!(
        sd_leaf,
        axum::http::StatusCode::OK,
        "叶子客户软删应成功: {env_leaf}"
    );

    // Soft-delete L1 → should fail with 20113 BIZ_CUSTOMER_IN_USE
    let (s3, env3) = send(
        fresh_app(&pool).await,
        json_request(
            "POST",
            &format!("/com/customers/{l1_id}/soft-delete"),
            None,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(
        s3,
        axum::http::StatusCode::CONFLICT,
        "soft-delete should return 409 CONFLICT for BIZ_CUSTOMER_IN_USE; got {env3}"
    );
    assert_eq!(
        env3["code"].as_i64().unwrap(),
        20113,
        "expected BIZ_CUSTOMER_IN_USE; got envelope: {env3}"
    );
}

#[tokio::test]
async fn update_customer_serial_prefix_collision_returns_20104() {
    let (pool, token) = bootstrap_as_manager().await;

    // Create two L1 customers with distinct serial_prefix values.
    let (_s1, env1) = send(
        fresh_app(&pool).await,
        json_request(
            "POST",
            "/com/customers",
            Some(serde_json::json!({"name": "Alpha", "serial_prefix": "A"})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(env1["code"], 0);
    let l1_a_id = env1["data"]["id"].as_str().unwrap().to_string();

    let (_s2, env2) = send(
        fresh_app(&pool).await,
        json_request(
            "POST",
            "/com/customers",
            Some(serde_json::json!({"name": "Bravo", "serial_prefix": "B"})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(env2["code"], 0);

    // Try to rename A → "B". uq_t_customer_root_prefix fires → 23505 → 20104.
    let (s3, env3) = send(
        fresh_app(&pool).await,
        json_request(
            "POST",
            &format!("/com/customers/{l1_a_id}/update"),
            Some(serde_json::json!({"serial_prefix": "B"})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(
        s3,
        axum::http::StatusCode::BAD_REQUEST,
        "duplicate-prefix update should return 400 BIZ_INVALID_VALUE; got {env3}"
    );
    assert_eq!(
        env3["code"].as_i64().unwrap(),
        20104,
        "expected BIZ_INVALID_VALUE (20104); got envelope: {env3}"
    );
    assert!(
        env3["message"]
            .as_str()
            .unwrap_or_default()
            .contains("serial_prefix 已存在"),
        "expected message to contain 'serial_prefix 已存在'; got: {env3}"
    );
}

// ===========================================================================
//  2026-10-17：客户层级放开为任意深度后的新增用例
//  （Gate A 子树展开 + Gate B 的子节点闸门 / parent 存在性 / 不传 limit 取全量）
// ===========================================================================

/// 2026-10-17：有子节点的客户软删 → `20124 BIZ_CUSTOMER_HAS_CHILDREN`（HTTP 400）。
///
/// 覆盖两层树里最典型的形态（L1 下挂 L2）与任意深度形态（根 → 第 2 层 → 第 3 层）
/// 两处：闸门判的是「有未软删子节点」，与子节点自己在第几层无关。
#[tokio::test]
async fn soft_delete_customer_with_children_returns_20124() {
    let (pool, token) = bootstrap_as_manager().await;

    let root = create_customer_id(&pool, &token, "HQ", None, Some("H")).await;
    let mid = create_customer_id(&pool, &token, "Terminal-BG", Some(&root), None).await;
    let leaf = create_customer_id(&pool, &token, "Phone-Dept", Some(&mid), None).await;

    // 中间层（第 2 层）：子树非空 ⇒ 拒
    let (s_mid, env_mid) = soft_delete(&pool, &token, &mid).await;
    assert_eq!(
        s_mid,
        axum::http::StatusCode::BAD_REQUEST,
        "20124 映射 HTTP 400: {env_mid}"
    );
    assert_eq!(
        env_mid["code"].as_i64().unwrap(),
        20124,
        "有子节点的客户软删应报 BIZ_CUSTOMER_HAS_CHILDREN: {env_mid}"
    );
    assert!(
        env_mid["message"]
            .as_str()
            .unwrap_or_default()
            .contains('1'),
        "消息应带下级节点数: {env_mid}"
    );

    // 根：子树 3 个节点 ⇒ 拒，且消息里的下级节点数 = 2（子树大小 - 1）
    let (s_root, env_root) = soft_delete(&pool, &token, &root).await;
    assert_eq!(
        s_root,
        axum::http::StatusCode::BAD_REQUEST,
        "20124 映射 HTTP 400: {env_root}"
    );
    assert_eq!(
        env_root["code"].as_i64().unwrap(),
        20124,
        "根客户软删同样应报 20124: {env_root}"
    );
    assert!(
        env_root["message"]
            .as_str()
            .unwrap_or_default()
            .contains('2'),
        "根的下级节点数应是 2（不是直接子节点数 1）: {env_root}"
    );

    // 叶子：可删（叶子闸门属于 Gate D，不在本任务）
    let (s_leaf, env_leaf) = soft_delete(&pool, &token, &leaf).await;
    assert_eq!(
        s_leaf,
        axum::http::StatusCode::OK,
        "叶子客户软删应成功: {env_leaf}"
    );
}

/// 2026-10-17：叶子软删成功后，父节点的子节点闸门放行（`deleted_at IS NULL` 过滤）。
#[tokio::test]
async fn soft_delete_parent_succeeds_after_children_are_deleted() {
    let (pool, token) = bootstrap_as_manager().await;

    let root = create_customer_id(&pool, &token, "HQ", None, Some("H")).await;
    let child = create_customer_id(&pool, &token, "Terminal-BG", Some(&root), None).await;

    let (s_child, env_child) = soft_delete(&pool, &token, &child).await;
    assert_eq!(s_child, axum::http::StatusCode::OK, "{env_child}");

    let (s_root, env_root) = soft_delete(&pool, &token, &root).await;
    assert_eq!(
        s_root,
        axum::http::StatusCode::OK,
        "子节点已软删 ⇒ 父节点的子节点闸门放行: {env_root}"
    );
}

/// 2026-10-17：`create` 的 `parent_id` 必须指向**已存在且未软删**的客户，否则 20102。
///
/// 没有这道校验就能建出断链（父行不存在 / 已软删），而断链的后果是派号时报 20308、
/// 路径少一段——现场看到的是完全指不到根上的错误。
#[tokio::test]
async fn create_customer_with_bad_parent_returns_20102() {
    let (pool, token) = bootstrap_as_manager().await;

    // ① 父行压根不存在
    let (s1, env1) = send(
        fresh_app(&pool).await,
        json_request(
            "POST",
            "/com/customers",
            Some(serde_json::json!({
                "name": "Orphan",
                "parent_id": "123456789012345678",
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(
        s1,
        axum::http::StatusCode::NOT_FOUND,
        "父客户不存在 ⇒ 404: {env1}"
    );
    assert_eq!(
        env1["code"].as_i64().unwrap(),
        20102,
        "父客户不存在应报 BIZ_CUSTOMER_NOT_FOUND: {env1}"
    );

    // ② 父行已软删
    let root = create_customer_id(&pool, &token, "HQ", None, Some("H")).await;
    let (s_root, env_root) = soft_delete(&pool, &token, &root).await;
    assert_eq!(s_root, axum::http::StatusCode::OK, "{env_root}");

    let (s2, env2) = send(
        fresh_app(&pool).await,
        json_request(
            "POST",
            "/com/customers",
            Some(serde_json::json!({
                "name": "Late-Child",
                "parent_id": root.clone(),
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(
        s2,
        axum::http::StatusCode::NOT_FOUND,
        "父客户已软删 ⇒ 404: {env2}"
    );
    assert_eq!(
        env2["code"].as_i64().unwrap(),
        20102,
        "父客户已软删应报 BIZ_CUSTOMER_NOT_FOUND: {env2}"
    );
}

/// 2026-10-17：3 层树里 `subtree_ids` 对中间层返回「自身 + 全部后代」，且**不含兄弟**。
///
/// 直接调 `shared::customer::subtree_ids`（不经 HTTP）：它是 part / com::union_list /
/// prod::inspection 三个筛选端点共用的唯一展开实现，语义（纯子树、任意深度）由它定。
#[tokio::test]
async fn subtree_ids_returns_self_plus_all_descendants() {
    let (pool, token) = bootstrap_as_manager().await;

    // 华强(L1) → 终端BG(L2) → 手机部(L3) → 影像组(L4)
    //                       └────→ 显示BG(L3)  ← 终端BG 的另一个子节点（兄弟，必须被排除）
    let l1 = create_customer_id(&pool, &token, "华强", None, Some("H")).await;
    let l2 = create_customer_id(&pool, &token, "终端BG", Some(&l1), None).await;
    let l3 = create_customer_id(&pool, &token, "手机部", Some(&l2), None).await;
    let l4 = create_customer_id(&pool, &token, "影像组", Some(&l3), None).await;
    let l3_sibling = create_customer_id(&pool, &token, "显示BG", Some(&l2), None).await;

    let mut conn = pool.acquire().await.expect("acquire conn");

    // 3 层树的中间层（第 3 层）：自身 + 唯一子节点
    let mut got = hsh_erp_rust::shared::customer::subtree_ids(&mut conn, l3.parse().unwrap())
        .await
        .expect("subtree_ids(第 3 层)");
    got.sort_unstable();
    let mut want = vec![l3.parse::<i64>().unwrap(), l4.parse::<i64>().unwrap()];
    want.sort_unstable();
    assert_eq!(got, want, "第 3 层的子树 = 自身 + 全部后代");

    // 第 2 层：自身 + 两个子节点 + 孙子（**不含**根、不含任何兄弟）
    let mut got = hsh_erp_rust::shared::customer::subtree_ids(&mut conn, l2.parse().unwrap())
        .await
        .expect("subtree_ids(第 2 层)");
    got.sort_unstable();
    let mut want = vec![
        l2.parse::<i64>().unwrap(),
        l3.parse::<i64>().unwrap(),
        l4.parse::<i64>().unwrap(),
        l3_sibling.parse::<i64>().unwrap(),
    ];
    want.sort_unstable();
    assert_eq!(got, want, "第 2 层的子树 = 自身 + 全部后代");

    // 根：整棵树
    let got = hsh_erp_rust::shared::customer::subtree_ids(&mut conn, l1.parse().unwrap())
        .await
        .expect("subtree_ids(根)");
    assert_eq!(got.len(), 5, "根的子树含全部 5 个节点: {got:?}");

    // 不存在的客户 → 20102（不是空集）
    let err = hsh_erp_rust::shared::customer::subtree_ids(&mut conn, 123456789012345678)
        .await
        .expect_err("不存在的客户应报错");
    assert_eq!(err.code(), 20102);
}

/// 2026-10-17：成环的 `parent_id` 链不会挂死，`root_id_of` 明确报错。
///
/// 环**造不出来**（`update` 拒 `parent_id`、`create` 要求父行存在），只能裸 SQL 造
/// ——这正是防环闸门存在的原因：真出现时（历史数据 / 手工改库）症状必须是「一条明确的
/// 400」而不是「接口卡到语句超时」。
#[tokio::test]
async fn root_id_of_reports_broken_tree_instead_of_hanging() {
    let (pool, token) = bootstrap_as_manager().await;

    let a = create_customer_id(&pool, &token, "Alpha", None, Some("A")).await;
    let b = create_customer_id(&pool, &token, "Bravo", Some(&a), None).await;

    // 裸 SQL 把 A.parent_id 指到 B ⇒ A → B → A 成环（API 路径造不出来）
    sqlx::query("UPDATE t_customer SET parent_id = $1 WHERE id = $2")
        .bind(b.parse::<i64>().unwrap())
        .bind(a.parse::<i64>().unwrap())
        .execute(&pool)
        .await
        .expect("制造 parent_id 环");

    let mut conn = pool.acquire().await.expect("acquire conn");

    // 上溯必须在有限时间内失败，而不是无限递归跑到语句超时
    let err = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        hsh_erp_rust::shared::customer::root_id_of(&mut conn, a.parse().unwrap()),
    )
    .await
    .expect("root_id_of 不应挂死（防环闸门失效）")
    .expect_err("成环的树解析不出根");
    assert_eq!(
        err.code(),
        20104,
        "成环 ≠ 客户不存在，必须报 BIZ_INVALID_VALUE: {err}"
    );

    // 向下展开同样收敛（UNION 去重），不会无限膨胀
    let got = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        hsh_erp_rust::shared::customer::subtree_ids(&mut conn, a.parse().unwrap()),
    )
    .await
    .expect("subtree_ids 不应挂死")
    .expect("成环的树仍能向下展开（去重收敛）");
    assert_eq!(got.len(), 2, "环上的两个节点各出现一次: {got:?}");
}

/// 2026-10-17：`paths_of` 的出参口径（`customer_path` = 第 2 层到叶子，`" / "` 拼接、
/// **不含**根名；本节点即根时 = 自己的名字）。
///
/// 直接调 `shared::customer::paths_of`：它一次往返批量产出 `(root_name, own_name, path)`，
/// 是 Gate C 之后 `customer_path` 出参的数据来源。走真库（不写假单测）是因为这段 SQL
/// 的递归 CTE 投影、`text[]` 解码都要真跑一遍才算覆盖。
#[tokio::test]
async fn paths_of_reports_root_own_name_and_product_path() {
    let (pool, token) = bootstrap_as_manager().await;

    // 华强(L1) → 终端BG(L2) → 手机部(L3) → 影像组(L4)
    let l1 = create_customer_id(&pool, &token, "华强", None, Some("H")).await;
    let l2 = create_customer_id(&pool, &token, "终端BG", Some(&l1), None).await;
    let l3 = create_customer_id(&pool, &token, "手机部", Some(&l2), None).await;
    let l4 = create_customer_id(&pool, &token, "影像组", Some(&l3), None).await;
    let l1_i = l1.parse::<i64>().unwrap();
    let l2_i = l2.parse::<i64>().unwrap();
    let l3_i = l3.parse::<i64>().unwrap();
    let l4_i = l4.parse::<i64>().unwrap();

    let mut conn = pool.acquire().await.expect("acquire conn");
    let map = hsh_erp_rust::shared::customer::paths_of(&mut conn, &[l1_i, l2_i, l3_i, l4_i])
        .await
        .expect("paths_of");
    assert_eq!(map.len(), 4, "4 个 id 都要有路径: {map:?}");

    // 根节点：path = 自己的名字
    assert_eq!(map[&l1_i].own_name, "华强");
    assert_eq!(map[&l1_i].root_name, "华强");
    assert_eq!(map[&l1_i].path, "华强");

    // 第 2 层：只剩自己一段（根被去掉）
    assert_eq!(map[&l2_i].root_name, "华强");
    assert_eq!(map[&l2_i].own_name, "终端BG");
    assert_eq!(map[&l2_i].path, "终端BG");

    // 第 3 层
    assert_eq!(map[&l3_i].path, "终端BG / 手机部");

    // 叶子（第 4 层）：任务书锁定的例子
    assert_eq!(map[&l4_i].root_name, "华强");
    assert_eq!(map[&l4_i].own_name, "影像组");
    assert_eq!(map[&l4_i].path, "终端BG / 手机部 / 影像组");

    // 不在 ids 里的客户不进 map；空 ids 短路
    let empty = hsh_erp_rust::shared::customer::paths_of(&mut conn, &[])
        .await
        .expect("paths_of([])");
    assert!(empty.is_empty());
    let unknown = hsh_erp_rust::shared::customer::paths_of(&mut conn, &[123456789012345678])
        .await
        .expect("paths_of(不存在的 id)");
    assert!(
        unknown.is_empty(),
        "不存在的客户不进 map（调用方按缺失容忍处理）"
    );
}

/// 2026-10-17：请求不传 `limit` ⇒ 返回**全量**（旧的 `DEFAULT_LIMIT = 50` 会静默截断）。
///
/// 造 1 根 + 60 子 = 61 个客户（> 旧默认 50），断言返回条数 == 实际条数。回参
/// `limit` 在全量模式下是**实际行数**、`offset` 恒 0。
#[tokio::test]
async fn list_customers_without_limit_returns_all_rows() {
    let (pool, token) = bootstrap_as_manager().await;

    let root = create_customer_id(&pool, &token, "HQ", None, Some("H")).await;
    let root_i64 = root.parse::<i64>().unwrap();

    // 60 个子节点走裸 SQL（走 API 要 60 次建单 + 60 次 app 构造，纯开销）。
    // 逐行 INSERT + 全参数化（id 取号用共享 generator，与 fixture 段同理由）。
    for i in 0..60i64 {
        let id = hsh_erp_test_support::shared_test_snowflake().next_id();
        sqlx::query("INSERT INTO t_customer (id, name, parent_id) VALUES ($1, $2, $3)")
            .bind(id)
            .bind(format!("BULK-{i}"))
            .bind(root_i64)
            .execute(&pool)
            .await
            .expect("批量插 60 个子客户");
    }

    let (s, env) = send(
        fresh_app(&pool).await,
        json_request("GET", "/com/customers", None, Some(&token)),
    )
    .await;
    assert_eq!(s, axum::http::StatusCode::OK, "list: {env}");
    let items = env["data"]["items"].as_array().unwrap();
    assert_eq!(
        items.len(),
        61,
        "不传 limit 必须返回全量 61 条（旧口径会截断到 50 条）"
    );
    assert_eq!(
        env["data"]["total"].as_i64().unwrap(),
        61,
        "total 与 items 等长（全量模式）"
    );
    assert_eq!(
        env["data"]["limit"].as_i64().unwrap(),
        61,
        "全量模式下 limit 回填实际行数"
    );
    assert_eq!(
        env["data"]["offset"].as_i64().unwrap(),
        0,
        "全量模式下 offset 恒 0"
    );

    // 显式传 limit 仍走分页（口径没被「全量」改掉）
    let (s2, env2) = send(
        fresh_app(&pool).await,
        json_request("GET", "/com/customers?limit=10", None, Some(&token)),
    )
    .await;
    assert_eq!(s2, axum::http::StatusCode::OK, "list limit=10: {env2}");
    assert_eq!(
        env2["data"]["items"].as_array().unwrap().len(),
        10,
        "limit=10 应只回 10 条"
    );
    assert_eq!(
        env2["data"]["total"].as_i64().unwrap(),
        61,
        "total 是过滤后的全量条数，不受 limit 影响"
    );
}

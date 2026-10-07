//! delivery-group 端到端集成测试
//!
//! Phase P1 覆盖：
//! 1. create group 成功（200）→ list_for_l1 返回新 group + members
//! 2. 同 L1 重名 → 409 / 21414
//! 3. 成员 L2 已属他组 → 409 / 21415
//! 4. update members（替换）→ 200；旧成员消失、新成员就位
//! 5. version conflict → 409 / VERSION_CONFLICT
//! 6. soft-delete → 200；后续 list 排除该组
//! 7. customer_id 不是 L1 → 400 / 20104
//! 8. customer_id 不存在 → 404 / 20102
//!
//! ## 并行
//! 进程级 test_pool 每次 fresh database（plan 2 2026-09-20），DB 间 schema
//! 完全独立，无需 Mutex 串行化。
//!
//! ## 认证
//! 每个用例都用 MANAGER 用户（fx_part_manager，part 域基线）。
//!
//! 2026-10-08：`/group` 的 3 个写端点白名单从 `[Manager, Clerk]` 放宽为
//! `[Manager, Clerk, Inspector]`（品检员在扫码入单页要能按 L2 归属分单）。理由与
//! 覆盖见末尾的 `inspector_can_manage_groups`。
//!
//! 2026-09-23 PR13 Phase G 改造：本地 `fn send` / `fn json_request` / `fn setup` /
//! `fn login_manager` 全部删除，统一用 `hsh_erp_test_support::{send, json_request,
//! login_token, test_pool, test_state, test_app, load_delivery_fixture}`。
//! 新增 `bootstrap_as_manager` 样板；本地 `insert_l1` / `insert_l2` 保留（测试
//! 需要特定 name='法拉电子' / prefix='F' 的 L1 客户 + 多个 L2 子客户，
//! fixture 不预置此类业务数据）。

use axum::http::StatusCode;
use serde_json::json;
use sqlx::PgPool;

use hsh_erp_test_support::{
    DeliveryFixture, json_request, load_delivery_fixture, login_token, pool_snowflake, send,
    test_app, test_pool, test_state,
};

use hsh_erp_rust::infra::clock::now_naive;

/// 取一个测试用雪花 ID。
///
/// 2026-10-08 review 第 1 轮 B3：**必须**走 `test-support::pool_snowflake()`
/// （进程级 `OnceLock<Mutex<..>>`，instance 由 pid ⊕ 启动纳秒派生），不能每次
/// `SnowflakeIdGenerator::new(...)` 新建 —— 新建会把 `last_ms` / `sequence` 归零，
/// 同一毫秒内两次调用返回**完全相同**的 id（epoch 与 instance 都写死、seq 都从 0
/// 开始），撞 `t_*_pkey` 报 23505。共享一个生成器后同进程内 `next_id()` 串行发号，
/// 跨进程靠派生 instance 区分。
///
/// 这也顺带解掉了**跨文件**碰撞：同一 binary（`tests/com/main.rs`）里本文件与
/// `note.rs` / `group.rs` / `union_list.rs` 曾经各自 `new(..., 1)`，首个 id 相同。
/// 2026-10-08 起 `union_list.rs` 也改走了 `pool_snowflake()`，与本 helper 同一路径。
fn next_id() -> i64 {
    pool_snowflake().lock().expect("pool_snowflake").next_id()
}

// ===========================================================================
//  Bootstrap helpers（PR13 Phase F/G 风格 B：抽出公共样板）
// ===========================================================================

/// 起一份 fresh database + 加载 delivery fixture + 以 MANAGER 身份登录。
///
/// 返回 `(pool, app, token, fx)`。`fx.part_manager_username` 是 part 域基线的
/// MANAGER 用户（fx_part_manager），`fx.delivery_*_id` 是 delivery 域预制
/// 的 assembly / group / group_member / note 常量 ID。
async fn bootstrap_as_manager() -> (PgPool, axum::Router, String, DeliveryFixture) {
    let pool = test_pool().await;
    let fx = load_delivery_fixture(&pool).await;
    let app = test_app(test_state(pool.clone()).await);
    let token = login_token(&app, &fx.part_manager_username, DeliveryFixture::PASSWORD).await;
    (pool, app, token, fx)
}

// ===========================================================================
//  Domain fixtures：L1 / L2 客户
//  （保留本地 helper：测试需要特定 name / prefix / parent_id 关系，
//   fixture 不预置此类业务数据）
// ===========================================================================

/// 直插 L1 客户（绕开 customer 域 CRUD）
async fn insert_l1(pool: &PgPool, name: &str, prefix: &str) -> i64 {
    let id = next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_customer (id, name, parent_id, serial_prefix, version, \
         created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, NULL, $3, 0, $4, NULL, $4, NULL)",
    )
    .bind(id)
    .bind(name)
    .bind(prefix)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert L1");
    id
}

/// 直插 L2 客户（parent_id = l1_id）
async fn insert_l2(pool: &PgPool, name: &str, l1_id: i64) -> i64 {
    let id = next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_customer (id, name, parent_id, serial_prefix, version, \
         created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, $3, NULL, 0, $4, NULL, $4, NULL)",
    )
    .bind(id)
    .bind(name)
    .bind(l1_id)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert L2");
    id
}

// ===========================================================================
//  Tests
// ===========================================================================

#[tokio::test]
async fn create_group_succeeds_and_appears_in_list() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let l1 = insert_l1(&pool, "法拉电子", "F").await;
    let l2_a = insert_l2(&pool, "二厂", l1).await;
    let l2_b = insert_l2(&pool, "五厂", l1).await;

    // create
    let (status, env) = send(
        app.clone(),
        json_request(
            "POST",
            "/com/delivery/group",
            Some(json!({
                "customer_id": l1.to_string(),
                "name": "二五六厂",
                "member_customer_ids": [l2_a.to_string(), l2_b.to_string()],
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "create: {env}");
    assert_eq!(env["code"], 0);
    assert_eq!(env["data"]["name"], "二五六厂");
    assert_eq!(env["data"]["members"].as_array().unwrap().len(), 2);

    // list
    let (s2, env2) = send(
        app,
        json_request(
            "GET",
            &format!("/com/delivery/group?customer_id={l1}"),
            None,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s2, StatusCode::OK);
    let groups = env2["data"]["groups"].as_array().unwrap();
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0]["name"], "二五六厂");
    assert_eq!(groups[0]["members"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn create_duplicate_name_returns_409_21414() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let l1 = insert_l1(&pool, "法拉电子", "F").await;
    insert_l2(&pool, "二厂", l1).await;

    // 第一次
    let (s1, _) = send(
        app.clone(),
        json_request(
            "POST",
            "/com/delivery/group",
            Some(json!({
                "customer_id": l1.to_string(),
                "name": "二五六厂",
                "member_customer_ids": [],
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s1, StatusCode::OK);

    // 第二次同 L1 同名
    let (s2, env2) = send(
        app,
        json_request(
            "POST",
            "/com/delivery/group",
            Some(json!({
                "customer_id": l1.to_string(),
                "name": "二五六厂",
                "member_customer_ids": [],
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s2, StatusCode::CONFLICT);
    assert_eq!(env2["code"], 21414, "code = 21414; full = {env2}");
}

#[tokio::test]
async fn create_with_member_in_other_group_returns_409_21415() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let l1 = insert_l1(&pool, "法拉电子", "F").await;
    let l2_a = insert_l2(&pool, "二厂", l1).await;
    let l2_b = insert_l2(&pool, "五厂", l1).await;

    // 创建第一个组（成员 = 二厂）
    let (s1, _) = send(
        app.clone(),
        json_request(
            "POST",
            "/com/delivery/group",
            Some(json!({
                "customer_id": l1.to_string(),
                "name": "组 A",
                "member_customer_ids": [l2_a.to_string()],
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s1, StatusCode::OK);

    // 创建第二个组，成员包括已被组 A 占用的 l2_a → 409 / 21415
    let (s2, env2) = send(
        app,
        json_request(
            "POST",
            "/com/delivery/group",
            Some(json!({
                "customer_id": l1.to_string(),
                "name": "组 B",
                "member_customer_ids": [l2_b.to_string(), l2_a.to_string()],
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s2, StatusCode::CONFLICT);
    assert_eq!(env2["code"], 21415, "code = 21415; full = {env2}");
}

#[tokio::test]
async fn update_members_full_replace_succeeds() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let l1 = insert_l1(&pool, "法拉电子", "F").await;
    let l2_a = insert_l2(&pool, "二厂", l1).await;
    let l2_b = insert_l2(&pool, "五厂", l1).await;
    let l2_c = insert_l2(&pool, "六厂", l1).await;

    // create (members = [二厂])
    let (s1, env1) = send(
        app.clone(),
        json_request(
            "POST",
            "/com/delivery/group",
            Some(json!({
                "customer_id": l1.to_string(),
                "name": "动态组",
                "member_customer_ids": [l2_a.to_string()],
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s1, StatusCode::OK);
    let gid = env1["data"]["id"].as_str().unwrap().parse::<i64>().unwrap();
    let v0 = env1["data"]["version"].as_i64().unwrap() as i32;

    // update members = [五厂, 六厂]（全量替换）
    let (s2, env2) = send(
        app.clone(),
        json_request(
            "POST",
            &format!("/com/delivery/group/{gid}/update"),
            Some(json!({
                "version": v0,
                "member_customer_ids": [l2_b.to_string(), l2_c.to_string()],
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s2, StatusCode::OK, "update: {env2}");
    let members_after: Vec<i64> = env2["data"]["members"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["customer_id"].as_str().unwrap().parse::<i64>().unwrap())
        .collect();
    let mut expected = vec![l2_b, l2_c];
    expected.sort();
    let mut actual = members_after.clone();
    actual.sort();
    assert_eq!(actual, expected, "members 全量替换");
}

#[tokio::test]
async fn update_with_wrong_version_returns_409_version_conflict() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let l1 = insert_l1(&pool, "法拉电子", "F").await;

    let (s1, env1) = send(
        app.clone(),
        json_request(
            "POST",
            "/com/delivery/group",
            Some(json!({
                "customer_id": l1.to_string(),
                "name": "G1",
                "member_customer_ids": [],
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s1, StatusCode::OK);
    let gid = env1["data"]["id"].as_str().unwrap().parse::<i64>().unwrap();

    // 用错误 version 调 update → 409 / VERSION_CONFLICT
    let (s2, env2) = send(
        app,
        json_request(
            "POST",
            &format!("/com/delivery/group/{gid}/update"),
            Some(json!({
                "version": 9999,
                "name": "G1-renamed",
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s2, StatusCode::CONFLICT);
    assert_eq!(env2["code"], 40901);
}

#[tokio::test]
async fn soft_delete_removes_group_from_list() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let l1 = insert_l1(&pool, "法拉电子", "F").await;

    // create
    let (s1, env1) = send(
        app.clone(),
        json_request(
            "POST",
            "/com/delivery/group",
            Some(json!({
                "customer_id": l1.to_string(),
                "name": "to-delete",
                "member_customer_ids": [],
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s1, StatusCode::OK);
    let gid = env1["data"]["id"].as_str().unwrap().parse::<i64>().unwrap();
    let v0 = env1["data"]["version"].as_i64().unwrap() as i32;

    // soft-delete
    let (s2, _) = send(
        app.clone(),
        json_request(
            "POST",
            &format!("/com/delivery/group/{gid}/soft-delete"),
            Some(json!({"version": v0})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s2, StatusCode::OK);

    // list 不再包含 to-delete
    let (_, env3) = send(
        app,
        json_request(
            "GET",
            &format!("/com/delivery/group?customer_id={l1}"),
            None,
            Some(&token),
        ),
    )
    .await;
    let groups = env3["data"]["groups"].as_array().unwrap();
    assert!(
        groups.iter().all(|g| g["name"] != "to-delete"),
        "soft-deleted group 不应在 list 里"
    );
}

#[tokio::test]
async fn create_with_non_l1_customer_returns_400_20104() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let l1 = insert_l1(&pool, "法拉电子", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await; // L2 customer

    // 把 L2 作为 customer_id 提交 → 400 / 20104
    let (s, env) = send(
        app,
        json_request(
            "POST",
            "/com/delivery/group",
            Some(json!({
                "customer_id": l2.to_string(),
                "name": "非法",
                "member_customer_ids": [],
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(
        env["code"], 20104,
        "code = 20104 (BIZ_INVALID_VALUE); full = {env}"
    );
}

#[tokio::test]
async fn list_with_nonexistent_customer_returns_404_20102() {
    let (_pool, app, token, _fx) = bootstrap_as_manager().await;

    let (s, env) = send(
        app,
        json_request(
            "GET",
            "/com/delivery/group?customer_id=999999999",
            None,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(env["code"], 20102);
}

/// 2026-10-08：`/group` 写端点放行 `Inspector`（3 条写路径全测）。
///
/// 之前只有 Manager / Clerk 能改分组，品检员在扫码入单页被卡在这一步。
///
/// ⚠️ 只装 part fixture：`load_delivery_fixture` 与 `load_part_fixture` 的
/// `t_customer` 常量 id 区段重叠，同库两次装会撞主键。
#[tokio::test]
async fn inspector_can_manage_groups() {
    use hsh_erp_test_support::{PartFixture, load_part_fixture};

    let pool = test_pool().await;
    let fx = load_part_fixture(&pool).await;
    let app = test_app(test_state(pool.clone()).await);
    let token = login_token(&app, &fx.inspector_username, PartFixture::PASSWORD).await;

    let l1 = insert_l1(&pool, "品检分组", "I").await;
    let l2a = insert_l2(&pool, "品检分组一厂", l1).await;
    let l2b = insert_l2(&pool, "品检分组二厂", l1).await;

    // 1) POST / 创建
    let (s1, env1) = send(
        app.clone(),
        json_request(
            "POST",
            "/com/delivery/group",
            Some(json!({
                "customer_id": l1.to_string(),
                "name": "品检可建组",
                "member_customer_ids": [l2a.to_string()],
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s1, StatusCode::OK, "Inspector 应能建组: {env1}");
    let group_id = env1["data"]["id"].as_str().unwrap().to_string();
    // 2026-10-08：VO 裁掉 `customer_id` / `created_at` / `updated_at`
    assert!(env1["data"].get("customer_id").is_none(), "{env1}");

    // 2) POST /{id}/update（全量替换成员）
    let (s2, env2) = send(
        app.clone(),
        json_request(
            "POST",
            &format!("/com/delivery/group/{group_id}/update"),
            Some(json!({
                "version": env1["data"]["version"].as_i64().unwrap(),
                "member_customer_ids": [l2b.to_string()],
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s2, StatusCode::OK, "Inspector 应能改组: {env2}");
    let members = env2["data"]["members"]
        .as_array()
        .expect("members 必须是数组");
    assert_eq!(members.len(), 1);
    assert_eq!(members[0]["customer_id"].as_str().unwrap(), l2b.to_string());

    // 3) POST /{id}/soft-delete
    let (s3, env3) = send(
        app.clone(),
        json_request(
            "POST",
            &format!("/com/delivery/group/{group_id}/soft-delete"),
            Some(json!({"version": env2["data"]["version"].as_i64().unwrap()})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s3, StatusCode::OK, "Inspector 应能软删组: {env3}");

    // 4) GET / 一并放行 Inspector
    let (s4, env4) = send(
        app,
        json_request(
            "GET",
            &format!("/com/delivery/group?customer_id={l1}"),
            None,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s4, StatusCode::OK, "Inspector 应能读分组: {env4}");
    assert!(
        env4["data"]["groups"]
            .as_array()
            .unwrap()
            .iter()
            .all(|g| g["id"].as_str() != Some(group_id.as_str())),
        "软删后不该再出现: {env4}"
    );
}

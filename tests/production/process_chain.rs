//! process_chain 域端到端集成测试（part-worker-pool-federated-rocket 2026-09-11）
//!
//! 覆盖场景：
//!   1. happy path：建链 → fetch 拿到 header + steps；part.process_chain_id 已回写（026 翻转）
//!   2. fetch 找不到链 → 20701 BIZ_PROCESS_CHAIN_NOT_FOUND (HTTP 404)
//!   3. PUT upsert 替换步骤（清空旧 steps + 插新 steps，chain.version++；part 指针不变）
//!   4. PUT upsert 创建全新链（part 之前无链）
//!   5. PUT upsert 校验：estimated_minutes < 0 → 40001 VALIDATION_ERROR
//!   6. PUT upsert 校验：sort_order 重复 → 40001 VALIDATION_ERROR
//!   7. step.note 字段往返：建链时填 note → fetch 拿回原文（migration 019）
//!   8. 非 PENDING part upsert → 20705 BIZ_PROCESS_CHAIN_PART_NOT_PENDING (HTTP 409)
//!   9. GET /process-chains/{chain_id} 命中 / 未命中（2026-09-16 FK 翻转新增端点）
//!  10. soft_delete_part 级联：链 + steps 软删、part.process_chain_id 置 NULL（026 翻转）
//!
//! ## 串行化
//! 进程级 test_pool 每次 fresh database（plan 2 2026-09-20），DB 间 schema
//! 完全独立，无需 Mutex / `--test-threads=1` 双保险。
//!
//! ## 集成测试范本（PR13 Phase F 引入，2026-09-23）
//! 本文件是 27+ 重复 helper（本地 `send` / `json_request` / `setup` /
//! `login_manager` / `insert_part` / `seed_process` / `insert_customer_l2`）的
//! 收敛目标。所有 HTTP / fixture helper 一律 `use hsh_erp_test_support::{...}`，
//! **不再保留本地副本**；新增 test binary 时也应照此模式：
//!
//! ```ignore
//! use hsh_erp_test_support::{
//!     json_request, load_process_chain_fixture, login_token, pool_snowflake,
//!     send, test_app, test_pool, test_state,
//!     fixture::ProcessChainFixture,
//! };
//! ```
//!
//! 强类型 fixture 句柄（`ProcessChainFixture`）由 `load_process_chain_fixture`
//! 返回，提供 `proc_a` / `part_pending` 等预制常量 ID，避免每测试现场 insert。

use axum::http::StatusCode;
use serde_json::json;
use sqlx::PgPool;
use tower::ServiceExt;

use hsh_erp_test_support::{
    ProcessChainFixture, json_request, load_process_chain_fixture, login_token, pool_snowflake,
    send, test_app, test_pool, test_state,
};

// ===========================================================================
//  Bootstrap helpers（PR13 Phase F 风格 B：抽出公共样板）
// ===========================================================================

/// 起一份 fresh database + 加载 process_chain fixture + 以 MANAGER 身份登录。
///
/// 返回 `(pool, app, token, fx)`。后续测试可直接 `pool` 跑 query!、
/// `app.clone()` 多次 `send`、token 直接拼到 bearer header；`fx` 暴露
/// `manager_username` / `clerk_username` / 预制常量 ID 等强类型句柄。
async fn bootstrap_as_manager() -> (PgPool, axum::Router, String, ProcessChainFixture) {
    let pool = test_pool().await;
    let fx = load_process_chain_fixture(&pool).await;
    let app = test_app(test_state(pool.clone()).await);
    let token = login_token(&app, &fx.manager_username, ProcessChainFixture::PASSWORD).await;
    (pool, app, token, fx)
}

/// 起一份 fresh database + 加载 process_chain fixture + 以 CLERK 身份登录。
async fn bootstrap_as_clerk() -> (PgPool, axum::Router, String, ProcessChainFixture) {
    let pool = test_pool().await;
    let fx = load_process_chain_fixture(&pool).await;
    let app = test_app(test_state(pool.clone()).await);
    let token = login_token(&app, &fx.clerk_username, ProcessChainFixture::PASSWORD).await;
    (pool, app, token, fx)
}

// ===========================================================================
//  Tests
// ===========================================================================

/// 场景 1: happy path —— 创链 → fetch 拿到 header + steps
#[tokio::test]
async fn upsert_then_get_by_part_happy() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let part_id = ProcessChainFixture::PART_PENDING;
    let proc_a = ProcessChainFixture::PROC_A;
    let proc_b = ProcessChainFixture::PROC_B;

    // 1. upsert：建链 + 2 步
    let (s, env) = send(
        app.clone(),
        json_request(
            "PUT",
            &format!("/prod/process-chains/by-part/{part_id}"),
            Some(json!({
                "name": "默认工艺",
                "note": "happy path",
                "steps": [
                    { "sort_order": 10, "process_id": proc_a.to_string(), "estimated_minutes": 30 },
                    { "sort_order": 20, "process_id": proc_b.to_string(), "estimated_minutes": 45 },
                ]
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "upsert: {env}");
    let data = &env["data"];
    // 2026-09-16 FK 翻转：ProcessChainOut 不再含 part_id（归属关系由 part 侧承载）
    assert!(
        data.get("part_id").is_none(),
        "契约变更：不应再有 part_id: {env}"
    );
    assert_eq!(data["name"], "默认工艺");
    assert_eq!(data["note"], "happy path");
    let steps = data["steps"].as_array().expect("steps array");
    assert_eq!(steps.len(), 2, "应有 2 步: {env}");
    assert_eq!(steps[0]["sort_order"], 10);
    assert_eq!(steps[0]["process_id"], proc_a.to_string());
    assert_eq!(steps[0]["estimated_minutes"], 30);
    assert_eq!(steps[1]["sort_order"], 20);
    assert_eq!(steps[1]["process_id"], proc_b.to_string());
    assert_eq!(steps[1]["estimated_minutes"], 45);
    let chain_id = data["id"].as_str().unwrap().to_string();

    // FK 翻转核心断言：part.process_chain_id 已回写为新链 id
    let chain_id_i64: i64 = chain_id.parse().expect("parse chain_id");
    let linked: Option<i64> = sqlx::query_scalar!(
        r#"SELECT process_chain_id AS "linked?" FROM t_part WHERE id = $1"#,
        part_id,
    )
    .fetch_one(&pool)
    .await
    .expect("read part.process_chain_id");
    assert_eq!(
        linked,
        Some(chain_id_i64),
        "part.process_chain_id 应回写: {env}"
    );

    // 2. fetch by part
    let (s2, env2) = send(
        app,
        json_request(
            "GET",
            &format!("/prod/process-chains/by-part/{part_id}"),
            None,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s2, StatusCode::OK, "get_by_part: {env2}");
    assert_eq!(env2["data"]["id"], chain_id);
    assert_eq!(env2["data"]["steps"].as_array().unwrap().len(), 2);
}

/// 场景 2: 找不到链 → 20701 BIZ_PROCESS_CHAIN_NOT_FOUND
#[tokio::test]
async fn get_by_part_chain_not_found() {
    let (_pool, app, token, _fx) = bootstrap_as_manager().await;
    let part_id = ProcessChainFixture::PART_PENDING;

    let (s, env) = send(
        app,
        json_request(
            "GET",
            &format!("/prod/process-chains/by-part/{part_id}"),
            None,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "无链应 404: {env}");
    assert_eq!(env["code"], 20701, "BIZ_PROCESS_CHAIN_NOT_FOUND: {env}");
}

/// 场景 3: PUT 整组替换：先有 2 步 → 换成 1 步；旧 steps 软删，新 step 新 id
#[tokio::test]
async fn upsert_replaces_old_steps() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let part_id = ProcessChainFixture::PART_PENDING;
    let proc_a = ProcessChainFixture::PROC_A;
    let proc_b = ProcessChainFixture::PROC_B;

    // 1. 首次 upsert：2 步
    let (_s, env) = send(
        app.clone(),
        json_request(
            "PUT",
            &format!("/prod/process-chains/by-part/{part_id}"),
            Some(json!({
                "name": "原链",
                "steps": [
                    { "sort_order": 10, "process_id": proc_a.to_string(), "estimated_minutes": 20 },
                    { "sort_order": 20, "process_id": proc_b.to_string(), "estimated_minutes": 40 },
                ]
            })),
            Some(&token),
        ),
    )
    .await;
    let chain_id = env["data"]["id"].as_str().unwrap().to_string();
    let version_before = env["data"]["version"].as_i64().unwrap();

    // 2. 二次 upsert：替换为 1 步
    let (s2, env2) = send(
        app,
        json_request(
            "PUT",
            &format!("/prod/process-chains/by-part/{part_id}"),
            Some(json!({
                "name": "新链",
                "steps": [
                    { "sort_order": 10, "process_id": proc_a.to_string(), "estimated_minutes": 60 },
                ]
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s2, StatusCode::OK, "upsert replace: {env2}");
    assert_eq!(env2["data"]["id"], chain_id, "chain id 不变");
    assert_eq!(env2["data"]["name"], "新链", "name 应更新");
    assert!(
        env2["data"]["version"].as_i64().unwrap() > version_before,
        "version 应自增 (前={version_before}, 后={})",
        env2["data"]["version"]
    );
    let steps = env2["data"]["steps"].as_array().unwrap();
    assert_eq!(steps.len(), 1, "应只剩 1 步");
    assert_eq!(
        steps[0]["estimated_minutes"], 60,
        "应使用新 step 的 minutes"
    );

    // 3. 验证旧 step 已软删（DB 直接查）
    let chain_id_i64: i64 = chain_id.parse().expect("parse chain_id");
    let n_active: i64 = sqlx::query_scalar!(
        r#"SELECT COUNT(*) AS "n!" FROM t_process_chain_step
        WHERE chain_id = $1 AND deleted_at IS NULL"#,
        chain_id_i64,
    )
    .fetch_one(&pool)
    .await
    .expect("count active steps");
    assert_eq!(n_active, 1, "DB 应只剩 1 个活跃 step");

    // FK 翻转：重复 upsert 后 part.process_chain_id 仍指向原链（链 id 不变）
    let linked: Option<i64> = sqlx::query_scalar!(
        r#"SELECT process_chain_id AS "linked?" FROM t_part WHERE id = $1"#,
        part_id,
    )
    .fetch_one(&pool)
    .await
    .expect("read part.process_chain_id");
    assert_eq!(linked, Some(chain_id_i64), "part 指针不应变化");
}

/// 场景 4: upsert 时 estimated_minutes < 0 → 40001
#[tokio::test]
async fn upsert_rejects_negative_minutes() {
    let (_pool, app, token, _fx) = bootstrap_as_manager().await;
    let part_id = ProcessChainFixture::PART_PENDING;
    let proc = ProcessChainFixture::PROC_A;

    let (s, env) = send(
        app,
        json_request(
            "PUT",
            &format!("/prod/process-chains/by-part/{part_id}"),
            Some(json!({
                "steps": [
                    { "sort_order": 10, "process_id": proc.to_string(), "estimated_minutes": -1 },
                ]
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "negative 应 400: {env}");
    assert_eq!(env["code"], 20104, "BIZ_INVALID_VALUE: {env}");
}

/// 场景 5: upsert 时 sort_order 重复 → 40001
#[tokio::test]
async fn upsert_rejects_duplicate_sort_order() {
    let (_pool, app, token, _fx) = bootstrap_as_manager().await;
    let part_id = ProcessChainFixture::PART_PENDING;
    let proc = ProcessChainFixture::PROC_A;

    let (s, env) = send(
        app,
        json_request(
            "PUT",
            &format!("/prod/process-chains/by-part/{part_id}"),
            Some(json!({
                "steps": [
                    { "sort_order": 10, "process_id": proc.to_string(), "estimated_minutes": 30 },
                    { "sort_order": 10, "process_id": proc.to_string(), "estimated_minutes": 60 },
                ]
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "重复应 400: {env}");
    assert_eq!(env["code"], 20104, "BIZ_INVALID_VALUE: {env}");
}

/// 场景 6: 非 Manager 调用 upsert → 40300 FORBIDDEN
#[tokio::test]
async fn upsert_forbidden_for_non_manager() {
    let (_pool, app, token, _fx) = bootstrap_as_clerk().await;
    let part_id = ProcessChainFixture::PART_PENDING;
    let proc = ProcessChainFixture::PROC_A;

    let (s, env) = send(
        app,
        json_request(
            "PUT",
            &format!("/prod/process-chains/by-part/{part_id}"),
            Some(json!({
                "steps": [
                    { "sort_order": 10, "process_id": proc.to_string(), "estimated_minutes": 30 },
                ]
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "非 Manager 应 403: {env}");
    assert_eq!(env["code"], 40300, "FORBIDDEN: {env}");
}

/// 场景 7: step.note 字段往返（migration 019）
///
/// 建链时为每步填 `note` 字段；fetch 应原样返回；后续 PUT 替换步骤时旧步骤
/// 软删（note 也不再被 SELECT 列出），新步骤的 note 独立验证。
#[tokio::test]
async fn step_note_round_trip() {
    let (_pool, app, token, _fx) = bootstrap_as_manager().await;
    let part_id = ProcessChainFixture::PART_PENDING;
    let proc_a = ProcessChainFixture::PROC_A;
    let proc_b = ProcessChainFixture::PROC_B;

    // 1. upsert：2 步，第 1 步有 note，第 2 步无 note
    let (s, env) = send(
        app.clone(),
        json_request(
            "PUT",
            &format!("/prod/process-chains/by-part/{part_id}"),
            Some(json!({
                "name": "含备注工艺",
                "steps": [
                    {
                        "sort_order": 10,
                        "process_id": proc_a.to_string(),
                        "estimated_minutes": 30,
                        "note": "必须干燥 24h 后才能上 CNC"
                    },
                    {
                        "sort_order": 20,
                        "process_id": proc_b.to_string(),
                        "estimated_minutes": 45
                    },
                ]
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "upsert: {env}");
    let steps = env["data"]["steps"].as_array().expect("steps array");
    assert_eq!(steps.len(), 2);
    assert_eq!(
        steps[0]["note"], "必须干燥 24h 后才能上 CNC",
        "第 1 步 note 应保留原文: {env}"
    );
    assert!(
        steps[1]["note"].is_null()
            || steps[1]["note"]
                .as_str()
                .map(|s| s.is_empty())
                .unwrap_or(true),
        "第 2 步 note 应为空: {env}"
    );

    // 2. fetch by part —— note 应持久
    let (s2, env2) = send(
        app,
        json_request(
            "GET",
            &format!("/prod/process-chains/by-part/{part_id}"),
            None,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s2, StatusCode::OK, "fetch: {env2}");
    let steps2 = env2["data"]["steps"].as_array().unwrap();
    assert_eq!(steps2[0]["note"], "必须干燥 24h 后才能上 CNC");
}

/// 场景 8: 非 PENDING part upsert → 20705 BIZ_PROCESS_CHAIN_PART_NOT_PENDING (HTTP 409)
///
/// 2026-09-16 FK 翻转新增守卫：零件一旦下发（离开 PENDING），工艺链冻结，
/// 禁止制定 / 修改。
#[tokio::test]
async fn upsert_rejects_non_pending_part() {
    let (_pool, app, token, _fx) = bootstrap_as_manager().await;
    let part_id = ProcessChainFixture::PART_IN_PROCESS;
    let proc = ProcessChainFixture::PROC_A;

    let (s, env) = send(
        app,
        json_request(
            "PUT",
            &format!("/prod/process-chains/by-part/{part_id}"),
            Some(json!({
                "steps": [
                    { "sort_order": 10, "process_id": proc.to_string(), "estimated_minutes": 30 },
                ]
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "非 PENDING 应 409: {env}");
    assert_eq!(
        env["code"], 20705,
        "BIZ_PROCESS_CHAIN_PART_NOT_PENDING: {env}"
    );
}

/// 场景 9: GET /process-chains/{chain_id} 命中 / 未命中（2026-09-16 新增端点）
#[tokio::test]
async fn get_chain_by_id_hit_and_miss() {
    let (_pool, app, token, _fx) = bootstrap_as_manager().await;
    let part_id = ProcessChainFixture::PART_PENDING;
    let proc_a = ProcessChainFixture::PROC_A;

    // 建链拿 chain_id
    let (s, env) = send(
        app.clone(),
        json_request(
            "PUT",
            &format!("/prod/process-chains/by-part/{part_id}"),
            Some(json!({
                "name": "按 id 读取链",
                "steps": [
                    { "sort_order": 10, "process_id": proc_a.to_string(), "estimated_minutes": 30 },
                ]
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "upsert: {env}");
    let chain_id = env["data"]["id"].as_str().unwrap().to_string();

    // 命中：GET /process-chains/{chain_id}
    let (s2, env2) = send(
        app.clone(),
        json_request(
            "GET",
            &format!("/prod/process-chains/{chain_id}"),
            None,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s2, StatusCode::OK, "get by id: {env2}");
    assert_eq!(env2["data"]["id"], chain_id);
    assert_eq!(env2["data"]["name"], "按 id 读取链");
    assert_eq!(env2["data"]["steps"].as_array().unwrap().len(), 1);
    assert_eq!(env2["data"]["steps"][0]["process_id"], proc_a.to_string());

    // 未命中：随机雪花 id → 404 + 20701
    let missing_id = pool_snowflake().lock().unwrap().next_id();
    let (s3, env3) = send(
        app,
        json_request(
            "GET",
            &format!("/prod/process-chains/{missing_id}"),
            None,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s3, StatusCode::NOT_FOUND, "未命中应 404: {env3}");
    assert_eq!(env3["code"], 20701, "BIZ_PROCESS_CHAIN_NOT_FOUND: {env3}");
}

/// 场景 10: soft_delete_part 级联（2026-09-16 FK 翻转）
///
/// part 软删后同事务级联：steps 全软删 → chain 软删 → part.process_chain_id 置 NULL。
/// 之后 get_chain_by_part / get_chain_by_id 均 20701。
#[tokio::test]
async fn soft_delete_part_cascades_chain() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let part_id = ProcessChainFixture::PART_PENDING;
    let proc_a = ProcessChainFixture::PROC_A;

    // 1. 建链（link 会把 part.version 从 0 推到 1）
    let (s, env) = send(
        app.clone(),
        json_request(
            "PUT",
            &format!("/prod/process-chains/by-part/{part_id}"),
            Some(json!({
                "steps": [
                    { "sort_order": 10, "process_id": proc_a.to_string(), "estimated_minutes": 30 },
                ]
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "upsert: {env}");
    let chain_id = env["data"]["id"].as_str().unwrap().to_string();
    let chain_id_i64: i64 = chain_id.parse().expect("parse chain_id");

    // 2. 读 part 当前 version（软删走 OCC）
    let part_version: i32 = sqlx::query_scalar!(
        r#"SELECT version AS "v!" FROM t_part WHERE id = $1"#,
        part_id,
    )
    .fetch_one(&pool)
    .await
    .expect("read part version");

    // 3. 软删 part
    let (s2, env2) = send(
        app.clone(),
        json_request(
            "POST",
            &format!("/parts/{part_id}/soft-delete"),
            Some(json!({ "version": part_version })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s2, StatusCode::OK, "soft-delete: {env2}");

    // 4. 级联断言：链软删
    let chain_deleted: bool = sqlx::query_scalar!(
        r#"SELECT (deleted_at IS NOT NULL) AS "d!" FROM t_part_process_chain WHERE id = $1"#,
        chain_id_i64,
    )
    .fetch_one(&pool)
    .await
    .expect("read chain deleted_at");
    assert!(chain_deleted, "链应被级联软删");

    // 5. 级联断言：steps 全软删
    let n_active_steps: i64 = sqlx::query_scalar!(
        r#"SELECT COUNT(*) AS "n!" FROM t_process_chain_step
        WHERE chain_id = $1 AND deleted_at IS NULL"#,
        chain_id_i64,
    )
    .fetch_one(&pool)
    .await
    .expect("count active steps");
    assert_eq!(n_active_steps, 0, "steps 应被级联软删");

    // 6. 级联断言：part.process_chain_id 置 NULL（让出 uq 槽位）
    let linked: Option<i64> = sqlx::query_scalar!(
        r#"SELECT process_chain_id AS "linked?" FROM t_part WHERE id = $1"#,
        part_id,
    )
    .fetch_one(&pool)
    .await
    .expect("read part.process_chain_id");
    assert_eq!(linked, None, "part.process_chain_id 应为 NULL");

    // 7. get_chain_by_part → 20701（part 已软删，JOIN 不上）
    let (s3, env3) = send(
        app.clone(),
        json_request(
            "GET",
            &format!("/prod/process-chains/by-part/{part_id}"),
            None,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s3, StatusCode::NOT_FOUND, "软删后 by-part 应 404: {env3}");
    assert_eq!(env3["code"], 20701, "BIZ_PROCESS_CHAIN_NOT_FOUND: {env3}");

    // 8. get_chain_by_id → 20701（链已软删）
    let (s4, env4) = send(
        app,
        json_request(
            "GET",
            &format!("/prod/process-chains/{chain_id}"),
            None,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s4, StatusCode::NOT_FOUND, "软删后 by-id 应 404: {env4}");
    assert_eq!(env4["code"], 20701, "BIZ_PROCESS_CHAIN_NOT_FOUND: {env4}");
}
//! prod::shelf_process 子模块端到端集成测试（2026-10-02 自货架子模块的 api 测试迁入）
//!
//! ## 覆盖
//! 1. `set_shelf_processes_replaces_existing_mapping` —— 整组替换：先 set [P1]，
//!    再 set [P2, P3] 后旧映射 (P1) 软删、新映射 (P2+P3) 在场。
//!    （原 `set_shelf_processes_replaces_existing_mapping`，
//!    URL 从 `/shelves/{id}/processes` 改为 `/prod/shelf-processes/{shelf_id}`）
//! 2. `list_all_mappings_returns_active_shelf_mappings` —— `GET /prod/shelf-processes`
//!    全集查询（新 URL，`GET /shelves/processes` 从未被集成测试覆盖）
//! 3. `set_shelf_processes_rejects_unknown_process` —— items 里 process_id 不存在
//!    → 20505 `BIZ_SHELF_PROCESS_PROCESS_NOT_FOUND`（HTTP 404）
//! 4. `old_shelf_process_paths_are_gone` —— 硬切验证：3 个旧路径按 URI 钉死状态码
//!    （三条全 404；`GET /shelves/processes` 自 2026-10-10 起随 `/api/v2/shelves`
//!    整段前缀下线由 400 变 404）
//! 5. `set_shelf_processes_rejects_inspection_zone_shelf` —— 2026-10-04 zone 守卫：
//!    品检区货架配工序 → 20104 `BIZ_INVALID_VALUE`，且一条 mapping 都不写
//! 6. `set_shelf_processes_rejects_inactive_shelf` —— 2026-10-04：`is_active=false`
//!    但未软删的货架 → 20512 `BIZ_SHELF_INACTIVE`（该形态经 API 造不出，故直插）
//! 7. `set_shelf_processes_rejects_missing_or_soft_deleted_shelf` —— 2026-10-04
//!    review 第 1 轮 M1 补齐第三形态：货架不存在 / 已软删 → 20501，且**清空路径
//!    （`items: []`）同样守**（否则会静默「成功」一个对已删货架的空操作）
//! 8. `set_shelf_processes_allows_clearing_inspection_zone_mappings` —— 2026-10-04
//!    review 第 1 轮 B1：`items: []`（清空）**豁免** zone / is_active 守卫 —— 存量
//!    非法映射必须留有 API 清理路径，否则「只读诊断 SQL ② 列出的行清不掉」
//!
//! ## fixture 选择（2026-10-02 判定）
//! 用 **production fixture**（`load_production_fixture`）而非 shelf fixture：
//! 本测试已迁入 `production` test binary（`tests/production/main.rs` 加
//! `mod shelf_process;`），与同目录 `process_chain.rs` / `work_type.rs` 一致走本域
//! 权威 fixture。`load_production_fixture` 内部先 `load_part_fixture`，因此
//! `PartFixture::PRODUCTION_SHELF_ID`（FX-SH-PROD，PRODUCTION 区 active 架）与
//! `ProductionFixture::PROCESS_A_ID` / `PROCESS_B_ID` 全部可用，无需再叠一层
//! shelf fixture（叠了也只是多出 1 个 INSPECTION 架 + 1 工序，对本组断言无贡献，
//! 反而让「用哪套 fixture」变模糊）。
//!
//! 与原测试的差异（原测试用 `POST /shelves` 现造一个干净 INSPECTION 架）：本组
//! 直接用 fixture 预置的 `PRODUCTION_SHELF_ID`，它**带 1 条既有 active 映射**
//! （part.sql 段 `9000000000000000025`：FX-SH-PROD → FX-PROC-A）。起点非空让
//! 「整组替换」被测得更强（第一笔 set 就要软删既有行），断言与原测试一致。
//!
//! ## 并行
//! 进程级 test_pool 每次 fresh database，DB 间 schema 完全独立，无需 Mutex 串行化。
//!
//! ## 认证
//! 全部用 MANAGER（`POST` 写路径要求 M-only）；SHELF_ACCOUNT scope 场景另由
//! `tests/part` 覆盖。

use axum::http::StatusCode;
use serde_json::json;
use sqlx::PgPool;
use tower::ServiceExt;

use hsh_erp_test_support::{
    PartFixture, ProductionFixture, json_request, load_production_fixture, login_token, send,
    test_app, test_pool, test_state,
};

// ===========================================================================
//  Bootstrap helpers（PR13 Phase F 风格，勿本地重复声明 send / json_request…）
// ===========================================================================

/// 起一份 fresh database + 加载 production fixture + 以 MANAGER 身份登录。
async fn bootstrap_as_manager() -> (PgPool, axum::Router, String, ProductionFixture) {
    let pool = test_pool().await;
    let fx = load_production_fixture(&pool).await;
    let app = test_app(test_state(pool.clone()).await);
    let token = login_token(&app, &fx.part_manager_username, ProductionFixture::PASSWORD).await;
    (pool, app, token, fx)
}

/// 直插一个 INHOUSE t_process 工序（`POST /prod/shelf-processes/{id}` 要校验
/// process_id 存在；测试按需造不同 code / name）。
///
/// production fixture 只预置 FX-NA / FX-NB 两个工序，本组测试要 3 个（P1/P2/P3）
/// 才能验证「整组替换后剩 2 条 + sort_order 顺序」，故第三个就地直插 —— 沿
/// `tests/shelf/api.rs::insert_test_process` 同形（该 helper 随 mapping 测试一起
/// 迁到本文件，shelf 域不再有 mapping 端点故不再需要它）。
async fn insert_test_process(pool: &PgPool, code: &str, name: &str) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;
    // 2026-10-09：ID 统一从全进程共享 generator 取（`shared_test_snowflake`），
    // 不再就地 new —— 两个 fresh generator 同 instance 同毫秒各取 seq 0 会撞
    // `t_process_pkey`（23505），与「同一个 helper 调几次」无关。
    let id = hsh_erp_test_support::shared_test_snowflake().next_id();
    let now = now_naive();
    sqlx::query!(
        "INSERT INTO t_process (id, code, name, category, sort_order, requires_approval, \
         version, created_at, updated_at) \
         VALUES ($1, $2, $3, 'INHOUSE', 0, false, 0, $4, $4)",
        id,
        code,
        name,
        now,
    )
    .execute(pool)
    .await
    .expect("insert t_process");
    id
}

// ===========================================================================
//  Tests
// ===========================================================================

/// 整组替换映射：先 set [P1]，再 set [P2, P3] 后旧映射 (P1) 软删、
/// 新映射 (P2+P3) 在场（按 sort_order 排序）。
#[tokio::test]
async fn set_shelf_processes_replaces_existing_mapping() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;

    // 1. 用 fixture 预置的 PRODUCTION 货架（自带 1 条既有 active 映射）
    let shelf_id = PartFixture::PRODUCTION_SHELF_ID;
    let shelf_id_str = shelf_id.to_string();

    // 2. 准备 3 个工序
    let p1 = insert_test_process(&pool, "P-MAP-1", "Map-Process-1").await;
    let p2 = insert_test_process(&pool, "P-MAP-2", "Map-Process-2").await;
    let p3 = insert_test_process(&pool, "P-MAP-3", "Map-Process-3").await;

    // 3. 第一次 set_shelf_processes —— 仅含 [P1]
    let (s2, env2) = send(
        app.clone(),
        json_request(
            "POST",
            &format!("/prod/shelf-processes/{shelf_id_str}"),
            Some(json!({
                "items": [
                    { "process_id": p1.to_string(), "sort_order": 0 },
                ],
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s2, StatusCode::OK, "first set processes: {env2}");
    assert_eq!(env2["code"], 0);
    assert!(
        env2["data"].is_null(),
        "set 端点返回 data: null（沿用现状契约）; got: {env2}"
    );

    // 4. 第二次 set_shelf_processes —— 替换为 [P2, P3]
    let (s3, env3) = send(
        app.clone(),
        json_request(
            "POST",
            &format!("/prod/shelf-processes/{shelf_id_str}"),
            Some(json!({
                "items": [
                    { "process_id": p2.to_string(), "sort_order": 0 },
                    { "process_id": p3.to_string(), "sort_order": 1 },
                ],
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s3, StatusCode::OK, "second set processes: {env3}");
    assert_eq!(env3["code"], 0);

    // 5. 读 `GET /prod/shelf-processes/{shelf_id}` —— 应只剩 P2, P3 (按 sort_order)
    let (s4, env4) = send(
        app.clone(),
        json_request(
            "GET",
            &format!("/prod/shelf-processes/{shelf_id_str}"),
            None,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s4, StatusCode::OK, "list per-shelf processes: {env4}");
    let items = env4["data"]["items"].as_array().expect("items[]");
    assert_eq!(
        items.len(),
        2,
        "after replace, mapping should have exactly 2 entries; got: {env4}"
    );
    // sort_order: P2=0, P3=1
    let pids: Vec<String> = items
        .iter()
        .map(|it| it["process_id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(pids, vec![p2.to_string(), p3.to_string()]);
    assert_eq!(items[0]["shelf_id"], shelf_id_str);
    assert_eq!(items[0]["sort_order"], 0);
    assert_eq!(items[1]["sort_order"], 1);

    // 6. DB 侧：P1 mapping 行已软删
    let row: Option<(Option<chrono::NaiveDateTime>,)> = sqlx::query_as(
        "SELECT deleted_at FROM t_shelf_process WHERE shelf_id = $1 AND process_id = $2",
    )
    .bind(shelf_id)
    .bind(p1)
    .fetch_optional(&pool)
    .await
    .expect("query old mapping deleted_at");
    let deleted_at = row.expect("old mapping row exists").0;
    assert!(
        deleted_at.is_some(),
        "old mapping row should be soft-deleted; got None"
    );
}

/// `GET /prod/shelf-processes` 全集查询：返回所有 active shelf 的映射行
/// （含 fixture 预置的 PRODUCTION 架与 INSPECTION 架）。
#[tokio::test]
async fn list_all_mappings_returns_active_shelf_mappings() {
    let (_pool, app, token, _fx) = bootstrap_as_manager().await;

    let (s, env) = send(
        app.clone(),
        json_request("GET", "/prod/shelf-processes", None, Some(&token)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "list all mappings: {env}");
    assert_eq!(env["code"], 0);

    let items = env["data"]["items"].as_array().expect("items[]");
    // part fixture 预置 1 条映射：PRODUCTION 架(15) → FX-PROC-A(12)
    assert_eq!(items.len(), 1, "expected only the fixture mapping: {env}");
    assert_eq!(
        items[0]["shelf_id"],
        PartFixture::PRODUCTION_SHELF_ID.to_string()
    );
    assert_eq!(items[0]["shelf_code"], "FX-SH-PROD");
    assert_eq!(items[0]["process_id"], PartFixture::PROCESS_ID.to_string());
    assert_eq!(items[0]["process_code"], "FX-PROC-A");
    // 全集端点不带 sort_order 字段（AllShelfProcessMappingItem 无该列）
    assert!(
        items[0].get("sort_order").is_none(),
        "all-mappings item 不含 sort_order; got: {env}"
    );

    // 单架端点的 item 带 sort_order
    let (s1, env1) = send(
        app,
        json_request(
            "GET",
            &format!("/prod/shelf-processes/{}", PartFixture::PRODUCTION_SHELF_ID),
            None,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s1, StatusCode::OK, "list per-shelf: {env1}");
    let one = env1["data"]["items"].as_array().expect("items[]");
    assert_eq!(one.len(), 1, "per-shelf should have 1 row: {env1}");
    assert_eq!(one[0]["sort_order"], 0);
}

/// items 里有不存在的 process_id → 20505 `BIZ_SHELF_PROCESS_PROCESS_NOT_FOUND`
/// （HTTP 404），且不写任何 mapping（事务回滚）。
#[tokio::test]
async fn set_shelf_processes_rejects_unknown_process() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;

    let shelf_id = PartFixture::PRODUCTION_SHELF_ID;
    let shelf_id_str = shelf_id.to_string();
    // 999999999999 不存在的工序 id（雪花 ID 量级但库内无此行）
    let ghost = 999_999_999_999_i64;

    let (s, env) = send(
        app.clone(),
        json_request(
            "POST",
            &format!("/prod/shelf-processes/{shelf_id_str}"),
            Some(json!({
                "items": [
                    { "process_id": ghost.to_string(), "sort_order": 0 },
                ],
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "unknown process: {env}");
    assert_eq!(
        env["code"].as_i64().unwrap(),
        20505,
        "expected BIZ_SHELF_PROCESS_PROCESS_NOT_FOUND; got: {env}"
    );

    // 事务回滚：既有的 fixture 映射行仍在（未被软删）
    let still_active: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM t_shelf_process WHERE shelf_id = $1 AND deleted_at IS NULL",
    )
    .bind(shelf_id)
    .fetch_one(&pool)
    .await
    .expect("count active mappings");
    assert_eq!(
        still_active, 1,
        "rejected set must not soft-delete existing mappings; got {still_active}"
    );
}

/// 硬切验证：3 个旧路径按 URI **逐个钉死**期望状态码（无 alias，沿 2026-09-19
/// prod 聚合先例）。
///
/// 注意响应体**不是** `R` 信封，故本测试绕过 `send()`（它会
/// `serde_json::from_str` 解析信封而 panic），直接 `app.oneshot(req)` 断状态码。
///
/// 期望值：
/// - `GET|POST /shelves/{id}/processes` → **404**：该 route 已从 router 整体删除，
///   无任何匹配
/// - `GET /shelves/processes` → **404**：2026-10-02 时它落进当时 shelf 模块的
///   `/{id}` catch-all、被 `Path<i64>` 拒为 400；2026-10-10 货架子模块并入 iam
///   域后 `/api/v2/shelves` 整段前缀硬切下线，连 catch-all 都不存在了 ⇒ 干净的
///   404。**带 `/iam` 前缀**的对应形态见 `tests/iam/shelf.rs` 的
///   `picker_endpoints_are_gone`（那条仍是 400，因为新前缀下 `/{id}` 还在）。
#[tokio::test]
async fn old_shelf_process_paths_are_gone() {
    let (_pool, app, token, _fx) = bootstrap_as_manager().await;

    let shelf_id_str = PartFixture::PRODUCTION_SHELF_ID.to_string();
    let cases: [(&str, String, StatusCode); 3] = [
        (
            "GET",
            "/shelves/processes".to_string(),
            StatusCode::NOT_FOUND,
        ),
        (
            "GET",
            format!("/shelves/{shelf_id_str}/processes"),
            StatusCode::NOT_FOUND,
        ),
        (
            "POST",
            format!("/shelves/{shelf_id_str}/processes"),
            StatusCode::NOT_FOUND,
        ),
    ];
    for (method, uri, expected) in cases {
        let req = json_request(method, &uri, Some(json!({ "items": [] })), Some(&token));
        let resp = app.clone().oneshot(req).await.expect("oneshot");
        let status = resp.status();
        assert_eq!(
            status, expected,
            "old path {method} {uri} must be gone (no alias) and return {expected}; got: {status}"
        );
    }
}

// ===========================================================================
//  2026-10-04 zone 守卫：只有 PRODUCTION 区货架能配工序映射
// ===========================================================================
//
// `set_shelf_processes` 收紧前只校验「货架存在」，不校验 zone，于是品检区货架可以被
// 配成某工序的落料架，再被 `ShelfProcessRepo::find_first_shelf_for_process`（同批
// 收紧）选中写进 `t_part_batch.current_holder_id`；而报工台取件页的取件 SQL 硬限定
// `sh.zone = 'PRODUCTION'`，这种批次就永远不会被工人领到、且不报错。
// 收紧后改走 `validate_shelf_zone(.., "PRODUCTION")`（与 6 个生产流端点同源同码）。

/// 品检区（`zone='INSPECTION'`）货架配工序 → 20104 `BIZ_INVALID_VALUE`（HTTP 400），
/// 且**一条 mapping 都不写**（守卫在软删旧映射之前 ⇒ 存量映射不受影响）。
#[tokio::test]
async fn set_shelf_processes_rejects_inspection_zone_shelf() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;

    // part fixture 预置的品检架（FX-SH-INSP，zone='INSPECTION'，is_active=true）
    let shelf_id = PartFixture::INSPECTION_SHELF_ID;
    let p1 = insert_test_process(&pool, "P-MAP-INSP", "Map-Process-Inspection").await;

    let (s, env) = send(
        app.clone(),
        json_request(
            "POST",
            &format!("/prod/shelf-processes/{shelf_id}"),
            Some(json!({
                "items": [
                    { "process_id": p1.to_string(), "sort_order": 0 },
                ],
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::BAD_REQUEST,
        "品检架配工序应 400（20104 兜底段）: {env}"
    );
    assert_eq!(
        env["code"].as_i64().unwrap(),
        20104,
        "expected BIZ_INVALID_VALUE（zone≠PRODUCTION）; got: {env}"
    );
    let msg = env["message"].as_str().unwrap_or_default();
    assert!(
        msg.contains("INSPECTION") && msg.contains("PRODUCTION"),
        "错误文案要带上实际 zone 与期望 zone，运营才知道该改货架还是改映射: {env}"
    );

    // 不写任何 mapping
    let n: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM t_shelf_process WHERE shelf_id = $1 AND deleted_at IS NULL",
    )
    .bind(shelf_id)
    .fetch_one(&pool)
    .await
    .expect("count active mappings");
    assert_eq!(n, 0, "拒收的 set 不得留下 mapping 行; got {n}");
}

/// `is_active=false` 但未软删的货架配工序 → 20512 `BIZ_SHELF_INACTIVE`（HTTP 400）。
///
/// 该形态**经 API 造不出来**（shelf service 的 `deactivate` 等价 soft-delete，同时写
/// `deleted_at`，会先命中 20501），所以本用例直插。这样才有覆盖到 20512 这条防御位
/// —— 它防的是「直接改库 / 历史数据造成 `is_active=false` 但未软删」。
#[tokio::test]
async fn set_shelf_processes_rejects_inactive_shelf() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;

    use hsh_erp_rust::infra::clock::now_naive;
    // 2026-10-09：原为 `new(1_577_836_800_001, 3)`（靠换 instance 避撞的权宜写法），
    // 现统一走全进程共享 generator —— instance 这 10 bit 只该留给跨进程区分。
    let shelf_id = hsh_erp_test_support::shared_test_snowflake().next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_shelf (id, code, name, zone, is_active, display_order, version, \
         created_at, updated_at) VALUES ($1, 'FX-SH-INACT', 'FX 停用架', 'PRODUCTION', \
         false, 0, 0, $2, $2)",
    )
    .bind(shelf_id)
    .bind(now)
    .execute(&pool)
    .await
    .expect("insert inactive t_shelf");

    let p1 = insert_test_process(&pool, "P-MAP-INACT", "Map-Process-Inactive").await;

    let (s, env) = send(
        app.clone(),
        json_request(
            "POST",
            &format!("/prod/shelf-processes/{shelf_id}"),
            Some(json!({
                "items": [
                    { "process_id": p1.to_string(), "sort_order": 0 },
                ],
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::BAD_REQUEST,
        "停用架配工序应 400（20512 兜底段）: {env}"
    );
    assert_eq!(
        env["code"].as_i64().unwrap(),
        20512,
        "expected BIZ_SHELF_INACTIVE; got: {env}"
    );

    let n: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM t_shelf_process WHERE shelf_id = $1 AND deleted_at IS NULL",
    )
    .bind(shelf_id)
    .fetch_one(&pool)
    .await
    .expect("count active mappings");
    assert_eq!(n, 0, "拒收的 set 不得留下 mapping 行; got {n}");
}

/// 2026-10-04 review 第 1 轮 M1 补齐的第三形态：货架**已软删** → 20501。
///
/// 本文件此前 6 个用例没有一个覆盖 20501（`get_by_id` 带 `deleted_at IS NULL`，
/// 已软删架与不存在的架同码）。验收要求「软删架 / 停用架 / 错 zone 架 各 ≥1 case」。
/// 顺带把 B1 的边界钉住：**清空路径（`items: []`）也守 20501** —— 对一个已软删的
/// 货架「清空映射」本该是 404，放行就变成静默成功的空操作。
#[tokio::test]
async fn set_shelf_processes_rejects_missing_or_soft_deleted_shelf() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;

    use hsh_erp_rust::infra::clock::now_naive;
    // 2026-10-09：原为 `new(1_577_836_800_002, 5)`，现统一走全进程共享 generator。
    let shelf_id = hsh_erp_test_support::shared_test_snowflake().next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_shelf (id, code, name, zone, is_active, display_order, version, \
         created_at, updated_at, deleted_at) VALUES ($1, 'FX-SH-DEL', 'FX 软删架', \
         'PRODUCTION', false, 0, 0, $2, $2, $2)",
    )
    .bind(shelf_id)
    .bind(now)
    .execute(&pool)
    .await
    .expect("insert soft-deleted t_shelf");

    let p1 = insert_test_process(&pool, "P-MAP-DEL", "Map-Process-Deleted").await;

    // ① 非空 items → 20501（HTTP 404）
    let (s, env) = send(
        app.clone(),
        json_request(
            "POST",
            &format!("/prod/shelf-processes/{shelf_id}"),
            Some(json!({
                "items": [
                    { "process_id": p1.to_string(), "sort_order": 0 },
                ],
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::NOT_FOUND,
        "已软删货架应 404（20501 资源缺失段）: {env}"
    );
    assert_eq!(
        env["code"].as_i64().unwrap(),
        20501,
        "expected BIZ_SHELF_NOT_FOUND; got: {env}"
    );

    // ② 清空路径同样守 20501 —— 豁免的只是 zone / is_active，不是存在性
    let (s2, env2) = send(
        app,
        json_request(
            "POST",
            &format!("/prod/shelf-processes/{shelf_id}"),
            Some(json!({ "items": [] })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(
        s2,
        StatusCode::NOT_FOUND,
        "已软删货架的清空请求也不该静默成功: {env2}"
    );
    assert_eq!(env2["code"].as_i64().unwrap(), 20501, "got: {env2}");

    // ③ 完全不存在的 id 也归 20501（与已软删同码）
    let (s3, env3) = send(
        bootstrap_as_manager().await.1,
        json_request(
            "POST",
            "/prod/shelf-processes/999999999999999999",
            Some(json!({ "items": [] })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s3, StatusCode::NOT_FOUND, "不存在的货架应 404: {env3}");
    assert_eq!(env3["code"].as_i64().unwrap(), 20501, "got: {env3}");
}

/// 2026-10-04 review 第 1 轮 B1：`items: []`（清空）**豁免** zone / is_active 守卫。
///
/// 无条件守卫会让**存量非法映射失去唯一的 API 清理路径** —— 整组替换语义下改不了其中
/// 一条，而 `PUT /shelves/{id}` 的 `ShelfUpdateRequest` 只有 `name` / `location`
/// （`zone` 不可经 API 改），也没有「先换成生产架再清映射」这条绕路。于是「只读诊断
/// SQL ② 列出的行在本仓清不掉」，诊断与处置自相矛盾。
///
/// 用例形状：品检架（`INSPECTION`，active）先造 1 条 active 映射（直插，模拟存量脏数据
/// —— 新守卫上线后 API 已造不出），再发 `items: []` ⇒ 必须 200 且该映射被软删。
/// 与 `set_shelf_processes_rejects_inspection_zone_shelf` 同架同 `items` 形态的对照是
/// 「非空 → 20104」 vs 「空 → 200」，两条一起把守卫边界钉死。
#[tokio::test]
async fn set_shelf_processes_allows_clearing_inspection_zone_mappings() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;

    // part fixture 预置的品检架（FX-SH-INSP，zone='INSPECTION'，is_active=true）
    let shelf_id = PartFixture::INSPECTION_SHELF_ID;
    let p1 = insert_test_process(&pool, "P-MAP-CLR", "Map-Process-Clear").await;

    // 直插 1 条「存量非法映射」：品检架 + active mapping 行（API 已造不出）
    sqlx::query(
        "INSERT INTO t_shelf_process (shelf_id, process_id, sort_order, version, \
         created_at, updated_at) VALUES ($1, $2, 0, 0, now(), now())",
    )
    .bind(shelf_id)
    .bind(p1)
    .execute(&pool)
    .await
    .expect("insert legacy illegal mapping");

    let before: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM t_shelf_process WHERE shelf_id = $1 AND deleted_at IS NULL",
    )
    .bind(shelf_id)
    .fetch_one(&pool)
    .await
    .expect("count before clear");
    assert_eq!(before, 1, "前置：品检架上应有 1 条 active 存量映射");

    // 清空请求：品检架 + items: [] ⇒ 必须放行（豁免 zone / is_active）
    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/prod/shelf-processes/{shelf_id}"),
            Some(json!({ "items": [] })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::OK,
        "清空路径必须对品检架放行（否则存量非法映射无 API 清理路径）: {env}"
    );

    let after: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM t_shelf_process WHERE shelf_id = $1 AND deleted_at IS NULL",
    )
    .bind(shelf_id)
    .fetch_one(&pool)
    .await
    .expect("count after clear");
    assert_eq!(
        after, 0,
        "放行的清空必须真的软删掉 active 映射; got {after}"
    );

    // 历史行保留（软删而非物理删），便于追溯 —— 与端点契约一致
    let soft_deleted_at: Option<chrono::NaiveDateTime> = sqlx::query_scalar(
        "SELECT deleted_at FROM t_shelf_process \
         WHERE shelf_id = $1 AND process_id = $2 AND deleted_at IS NOT NULL",
    )
    .bind(shelf_id)
    .bind(p1)
    .fetch_one(&pool)
    .await
    .expect("read soft-deleted mapping");
    assert!(
        soft_deleted_at.is_some(),
        "清空应走软删（deleted_at 置位）而非物理删除"
    );
}

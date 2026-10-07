//! outsource company 域集成测试（Phase 2 2026-09-13）
//!
//! 覆盖：
//! - list: 创建 3 个公司 + name_like 过滤 + is_active 过滤
//! - get: 详情含 process_ids 反查
//! - create: name 必填 + 重名 409 + 可选 process_ids 注入（出参 `R<()>`）
//! - update: OCC 版本冲突 + 部分字段 + `process_ids` 三态（不动 / 清空 / 替换）
//!   与「集合未变则不重写映射表」的 diff 守卫
//! - soft-delete: 必传 `version`；仍映射工序时 21205；**version 过期时 40901 早于 21205**
//! - list-by-process: 按 process 反查 active 公司（窄 VO）
//!
//! ## 集成测试范本（PR13 Phase H，2026-09-24）
//! 本文件按 Phase F 范本收敛：删除本地 `send` / `json_request` / `setup` /
//! `login_manager` 通用 helper，统一走
//! `use hsh_erp_test_support::{...}` + `bootstrap_as_manager()` +
//! `load_outsource_fixture(&pool)`。保留：
//! - `seed_outsource_process`：company 域独享（9 个场景需要不同 OUTSOURCE
//!   工序 code，按需用 sqlx::query 直插；fixture 预置的 FX-OPROC-A 仅作
//!   baseline 共享）；
//! - 域独享 helper 不从 `fixtures` 模块 `use`（Phase H gate 5 禁止）；
//!   本地 helper 用 `sqlx::query` 直插与 `fixtures::seed_process` 同形 SQL
//!   （columns / defaults 全部对齐 migration 003 的 t_process schema，
//!   category='OUTSOURCE'）。
//!
//! ## 不预置 t_outsource_company
//! 各场景需要不同 name / is_active / contact 等字段；预置 1 行 FX-OC-001
//! 不与测试自建 company 撞（uk_t_outsource_company_name 仅约束 active 同名
//! 唯一），但绝大多数场景希望公司列表干净从 0 起算，因此各 sub-file 用
//! 本地 `insert_outsource_company` 插自定义行。
//!
//! ## 2026-10-09 的三处契约变更对测试面的影响
//! - `POST /` 出参改 `R<()>` ⇒ 建号后拿不到 id，改从 `GET /?name_like=` 回查；
//! - `POST /{id}/processes` 硬切下线 ⇒ 「整体替换」用例改打 `/{id}/update`；
//! - `POST /{id}/soft-delete` 与报价的 `submit` / `soft-delete` 必须带 body `version`。

use axum::http::StatusCode;
use serde_json::{Value, json};
use sqlx::PgPool;

use hsh_erp_rust::infra::clock::now_naive;
use hsh_erp_test_support::{
    OutsourceFixture, json_request, load_outsource_fixture, login_token, pool_snowflake, send,
    send_raw, test_app, test_pool, test_state,
};

// ===========================================================================
//  Bootstrap helpers（PR13 Phase H 风格）
// ===========================================================================

/// 起一份 fresh database + 加载 outsource fixture + 以 MANAGER 身份登录。
///
/// 返回 `(pool, app, token, fx)`。后续测试可直接 `pool` 跑 query!、
/// `app.clone()` 多次 `send`、token 直接拼到 bearer header；`fx` 暴露
/// `part_manager_username` / 预制常量 ID 等强类型句柄（绝大多数 company
/// 域测试用本地 `seed_outsource_process` 自建 OUTSOURCE 工序，仅
/// `outsource_process_id` / `outsource_company_id` 作为 baseline 句柄备查）。
async fn bootstrap_as_manager() -> (PgPool, axum::Router, String, OutsourceFixture) {
    let pool = test_pool().await;
    let fx = load_outsource_fixture(&pool).await;
    let app = test_app(test_state(pool.clone()).await);
    let token = login_token(&app, &fx.part_manager_username, OutsourceFixture::PASSWORD).await;
    (pool, app, token, fx)
}

// ===========================================================================
//  company 域独享 helpers（绕开 fixtures::seed_process 因为 Phase H gate 5
//  禁止从 `fixtures` 模块 use 任何动态 helper）
/// 直插用的雪花 ID：走 `test-support::pool_snowflake()`（**进程级**
/// `OnceLock<Mutex<..>>`，instance 由 pid ⊕ 启动时间派生）。
///
/// 不每次 `SnowflakeIdGenerator::new(epoch, 1)` 新建生成器：新建的生成器在同一毫秒内
/// 连续两次调用会生成**完全相同**的 id（instance 相同 + 时间戳相同 + seq 都从 0 开始），
/// 撞 `t_*_pkey`；更隐蔽的是撞成「shelf_id == process_id」这类业务列，让 DB 的
/// `ck_*_no_self_loop` CHECK 以一条与被测逻辑无关的约束错误把用例打断。范本与理由见
/// `tests/outsource/pool.rs::next_id`。
fn next_id() -> i64 {
    pool_snowflake().lock().expect("pool_snowflake").next_id()
}

// ===========================================================================

/// 直插一个 OUTSOURCE 类别 `t_process` 工序。
///
/// 与 `fixtures::seed_process` 同形 SQL，但 category='OUTSOURCE'（fixture 版
/// 走 INHOUSE，company 域必须用 OUTSOURCE）。
async fn seed_outsource_process(pool: &PgPool, code: &str, name: &str) -> i64 {
    let id = next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_process (id, code, name, category, sort_order, requires_approval, \
         version, created_at, updated_at) \
         VALUES ($1, $2, $3, 'OUTSOURCE', 0, true, 0, $4, $4)",
    )
    .bind(id)
    .bind(code)
    .bind(name)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert OUTSOURCE process");
    id
}

// ===========================================================================
//  Tests
// ===========================================================================
//  域独享 test helper（依赖上面 `POST /` 出参已是 `R<()>`）
// ===========================================================================

/// 按 `name` 精确取公司 id（直查 DB，避开 `?name_like=` 的 URI 编码问题）。
async fn company_id_by_name(pool: &PgPool, name: &str) -> i64 {
    sqlx::query_scalar("SELECT id FROM t_outsource_company WHERE name = $1 AND deleted_at IS NULL")
        .bind(name)
        .fetch_one(pool)
        .await
        .unwrap_or_else(|e| panic!("按名字取公司 id 失败（{name:?}）: {e}"))
}

/// 建一家公司 → 返回 `(status, env)`。
///
/// 2026-10-09 起 `POST /outsource-companies` 出参是 `R<()>`，**拿不到 id**，
/// 需要 id 的用例另用 [`company_id_by_name`] 回查。
async fn create_company(app: &axum::Router, token: &str, body: Value) -> (StatusCode, Value) {
    send(
        app.clone(),
        json_request("POST", "/outsource-companies", Some(body), Some(token)),
    )
    .await
}

/// 取 `GET /{id}` 的 `version`（OCC 锚）。
async fn company_version(app: &axum::Router, token: &str, cid: &str) -> i64 {
    let (s, env) = send(
        app.clone(),
        json_request(
            "GET",
            &format!("/outsource-companies/{cid}"),
            None,
            Some(token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "get company: {env}");
    env["data"]["version"].as_i64().unwrap()
}

/// 取 `GET /{id}` 的 `processes[]` 里的 `process_id` 序列（按响应里的顺序）。
async fn company_process_ids(app: &axum::Router, token: &str, cid: &str) -> Vec<String> {
    let (s, env) = send(
        app.clone(),
        json_request(
            "GET",
            &format!("/outsource-companies/{cid}"),
            None,
            Some(token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "get company: {env}");
    env["data"]["processes"]
        .as_array()
        .expect("processes 必须是数组")
        .iter()
        .map(|p| p["process_id"].as_str().unwrap().to_string())
        .collect()
}

/// 取该公司在 `t_outsource_company_process` 里**未软删**的 junction id 序列。
///
/// diff 守卫用例靠它断言「提交同一集合不重写映射表」：重写会换掉全部 junction id。
async fn live_junction_ids(pool: &PgPool, cid: i64) -> Vec<i64> {
    sqlx::query_scalar(
        "SELECT id FROM t_outsource_company_process \
         WHERE outsource_company_id = $1 AND deleted_at IS NULL ORDER BY sort_order ASC, id ASC",
    )
    .bind(cid)
    .fetch_all(pool)
    .await
    .expect("读 junction id")
}

// ===========================================================================

#[tokio::test]
async fn create_outsource_company_happy_path() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;

    let (s, env) = create_company(
        &app,
        &token,
        json!({"name": "Acme 加工厂", "is_active": true}),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "create: {env}");
    assert_eq!(env["code"], 0);
    // 2026-10-09：出参收窄为 `R<()>`，`data` 是 null（建号后前端一律重拉列表）。
    assert!(
        env["data"].is_null(),
        "create 出参必须是 R<()>（data=null）: {env}"
    );

    let cid = company_id_by_name(&pool, "Acme 加工厂").await;
    let (_, env2) = send(
        app,
        json_request(
            "GET",
            &format!("/outsource-companies/{cid}"),
            None,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(env2["data"]["name"], "Acme 加工厂", "{env2}");
    assert_eq!(env2["data"]["is_active"], true, "{env2}");
    assert_eq!(env2["data"]["version"], 0, "{env2}");
    assert_eq!(
        env2["data"]["processes"].as_array().unwrap().len(),
        0,
        "{env2}"
    );
    // 2026-10-09：`OutsourceCompanyWithProcessesOut` 删掉两个时间字段。
    assert!(
        env2["data"].get("created_at").is_none() && env2["data"].get("updated_at").is_none(),
        "公司出参不得再带 created_at / updated_at: {env2}"
    );
}

#[tokio::test]
async fn create_outsource_company_duplicate_returns_21202() {
    let (_pool, app, token, _fx) = bootstrap_as_manager().await;

    let (s1, _) = send(
        app.clone(),
        json_request(
            "POST",
            "/outsource-companies",
            Some(json!({"name": "SameName"})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s1, StatusCode::CREATED);

    let (s2, env2) = send(
        app,
        json_request(
            "POST",
            "/outsource-companies",
            Some(json!({"name": "SameName"})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s2, StatusCode::CONFLICT, "duplicate: {env2}");
    assert_eq!(env2["code"].as_i64().unwrap(), 21202);
}

/// 2026-09-15 fix-outsource-409 回归测试：
/// 同名第二次创建必须返回 409 而不是 500。Code 可以是：
/// - 21202（应用层 pre-check 命中：`OutsourceCompanyRepo::get_by_name` 找到 active 行）
/// - 21214（DB `uk_t_outsource_company_name` 兜底：pre-check 漏网，INSERT 撞部分唯一索引）
/// 单线程顺序测试通常命中前者，但保证两种路径下都不返 500。
#[tokio::test]
async fn create_outsource_company_duplicate_returns_409() {
    let (_pool, app, token, _fx) = bootstrap_as_manager().await;

    let (s1, _) = send(
        app.clone(),
        json_request(
            "POST",
            "/outsource-companies",
            Some(json!({"name": "E2E_TEST_DUP_COMPANY"})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s1, StatusCode::CREATED);

    let (s2, env2) = send(
        app,
        json_request(
            "POST",
            "/outsource-companies",
            Some(json!({"name": "E2E_TEST_DUP_COMPANY"})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(
        s2,
        StatusCode::CONFLICT,
        "duplicate must be 409 not 500: {env2}"
    );
    let code = env2["code"].as_i64().unwrap();
    assert!(
        code == 21202 || code == 21214,
        "duplicate code must be 21202 or 21214, got {code}; full env: {env2}"
    );
}

#[tokio::test]
async fn create_outsource_company_with_process_ids_creates_mapping() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let p1 = seed_outsource_process(&pool, "PROC-O-1", "外协工序1").await;
    let p2 = seed_outsource_process(&pool, "PROC-O-2", "外协工序2").await;

    let (s, env) = create_company(
        &app,
        &token,
        json!({
            "name": "ProcMapping Co",
            "process_ids": [p1.to_string(), p2.to_string()]
        }),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "create with procs: {env}");
    let cid = company_id_by_name(&pool, "ProcMapping Co").await;

    // 出参是 `R<()>`，映射要用 `GET /{id}` 验。
    let (_, env_g) = send(
        app.clone(),
        json_request(
            "GET",
            &format!("/outsource-companies/{cid}"),
            None,
            Some(&token),
        ),
    )
    .await;
    let procs = env_g["data"]["processes"].as_array().unwrap();
    assert_eq!(procs.len(), 2, "{env_g}");
    assert_eq!(procs[0]["process_id"].as_str().unwrap(), p1.to_string());
    assert_eq!(procs[1]["process_id"].as_str().unwrap(), p2.to_string());
    // 2026-10-09：`OutsourceCompanyProcessLinkOut` 收成 3 字段（删 `category` /
    // `sort_order`）—— 前端勾选框的候选集来自独立的工序列表端点，`sort_order`
    // 从不由 VO 消费。
    assert_eq!(procs[0].as_object().unwrap().len(), 3, "{env_g}");
    assert!(
        procs[0].get("category").is_none() && procs[0].get("sort_order").is_none(),
        "工序链接出参不得再带 category / sort_order: {env_g}"
    );

    // by-process 也能查到（窄 VO：只有 id + name）
    let (_, env_by) = send(
        app,
        json_request(
            "GET",
            &format!("/outsource-companies/by-process/{p1}"),
            None,
            Some(&token),
        ),
    )
    .await;
    let items = env_by["data"].as_array().unwrap();
    assert_eq!(items.len(), 1, "{env_by}");
    assert_eq!(
        items[0]["id"].as_str().unwrap(),
        cid.to_string(),
        "{env_by}"
    );
    assert_eq!(items[0]["name"], "ProcMapping Co", "{env_by}");
    assert_eq!(
        items[0].as_object().unwrap().len(),
        2,
        "by-process 出参必须是窄 VO（id + name）: {env_by}"
    );
}

#[tokio::test]
async fn update_outsource_company_version_conflict_returns_40901() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;

    let (s, _) = create_company(&app, &token, json!({"name": "VC Co"})).await;
    assert_eq!(s, StatusCode::CREATED);
    let cid = company_id_by_name(&pool, "VC Co").await;

    // 故意传错 version
    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/outsource-companies/{cid}/update"),
            Some(json!({"name": "VC Co New", "version": 99})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "vc: {env}");
    assert_eq!(env["code"].as_i64().unwrap(), 40901);
}

/// `process_ids` 三态：缺省 = 不动、`[]` = 清空、`[..]` = 替换。
///
/// 三态必须可分 —— 「取消全选并保存」若被当成「没改」而静默丢失，合并成一个对话框
/// 后用户在界面上看不出任何异常。
#[tokio::test]
async fn update_company_process_ids_three_states() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let p1 = seed_outsource_process(&pool, "PS-1", "ps1").await;
    let p2 = seed_outsource_process(&pool, "PS-2", "ps2").await;
    let p3 = seed_outsource_process(&pool, "PS-3", "ps3").await;

    let (s, env) = create_company(
        &app,
        &token,
        json!({"name": "ThreeStates Co", "process_ids": [p1.to_string()]}),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{env}");
    let cid = company_id_by_name(&pool, "ThreeStates Co").await;
    let cid = cid.to_string();
    assert_eq!(
        company_process_ids(&app, &token, &cid).await,
        vec![p1.to_string()]
    );

    // ① 缺省（不给 process_ids）：映射不动
    let mut ver = company_version(&app, &token, &cid).await;
    let (s, env) = send(
        app.clone(),
        json_request(
            "POST",
            &format!("/outsource-companies/{cid}/update"),
            Some(json!({"contact_phone": "021-111", "version": ver})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "只改电话: {env}");
    assert_eq!(env["data"]["contact_phone"], "021-111", "{env}");
    assert_eq!(
        company_process_ids(&app, &token, &cid).await,
        vec![p1.to_string()],
        "缺省 process_ids 必须不动映射: {env}"
    );

    // ② 空数组：清空
    ver = company_version(&app, &token, &cid).await;
    let (s, env) = send(
        app.clone(),
        json_request(
            "POST",
            &format!("/outsource-companies/{cid}/update"),
            Some(json!({"version": ver, "process_ids": []})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "清空: {env}");
    assert!(
        company_process_ids(&app, &token, &cid).await.is_empty(),
        "process_ids=[] 必须清空映射: {env}"
    );

    // ③ 替换：整体换成 [p2, p3]，p1 不再出现
    ver = company_version(&app, &token, &cid).await;
    let (s, env) = send(
        app.clone(),
        json_request(
            "POST",
            &format!("/outsource-companies/{cid}/update"),
            Some(json!({
                "version": ver,
                "process_ids": [p2.to_string(), p3.to_string()]
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "替换: {env}");
    assert_eq!(
        company_process_ids(&app, &token, &cid).await,
        vec![p2.to_string(), p3.to_string()],
        "process_ids 必须整体替换且保序: {env}"
    );
}

/// diff 守卫：目标有序集合 == 当前有序集合时**跳过重写**（junction id 不变）。
///
/// 吸收 `process_ids` 进 update 之后，每次保存（哪怕只改电话号码）都会走到映射写入
/// 路径；而 `replace_processes` 是「软删全部 + 逐条重建」，无脑执行会把整张
/// `t_outsource_company_process` churn 一遍却内容不变。
#[tokio::test]
async fn update_company_same_process_set_does_not_rewrite_mapping() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let p1 = seed_outsource_process(&pool, "DG-1", "dg1").await;
    let p2 = seed_outsource_process(&pool, "DG-2", "dg2").await;

    let (s, env) = create_company(
        &app,
        &token,
        json!({
            "name": "DiffGuard Co",
            "process_ids": [p1.to_string(), p2.to_string()]
        }),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{env}");
    let cid = company_id_by_name(&pool, "DiffGuard Co").await;
    let before = live_junction_ids(&pool, cid).await;
    assert_eq!(before.len(), 2, "造数必须有两行映射");

    // 提交完全相同的集合（顺序也相同）
    let ver = company_version(&app, &token, &cid.to_string()).await;
    let (s, env) = send(
        app.clone(),
        json_request(
            "POST",
            &format!("/outsource-companies/{cid}/update"),
            Some(json!({
                "version": ver,
                "process_ids": [p1.to_string(), p2.to_string()]
            })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{env}");
    assert_eq!(
        live_junction_ids(&pool, cid).await,
        before,
        "集合未变时不得重写映射表（junction id 应保持不变）: {env}"
    );

    // 真要替换时才换 id
    let ver = company_version(&app, &token, &cid.to_string()).await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/outsource-companies/{cid}/update"),
            Some(json!({"version": ver, "process_ids": [p2.to_string()]})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{env}");
    let after = live_junction_ids(&pool, cid).await;
    assert_eq!(after.len(), 1, "替换后应只剩一行: {env}");
    assert_ne!(after, before, "集合真变了必须重写: {env}");
}

/// `version` 过期必须返 40901，**且早于** 21205（仍映射工序）。
///
/// 守卫顺序是契约：version 过期意味着整个对话框看到的公司状态已失效，此时报
/// 「仍映射 N 项工序，请先清空」会把用户引向错误的排查方向（他真去清工序，而真正
/// 的原因是数据已被他人改动）。
#[tokio::test]
async fn soft_delete_version_conflict_precedes_in_use_guard() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let p1 = seed_outsource_process(&pool, "PROC-VC-INUSE", "外协VC").await;
    let (s, env) = create_company(
        &app,
        &token,
        json!({"name": "VcInUse Co", "process_ids": [p1.to_string()]}),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{env}");
    let cid = company_id_by_name(&pool, "VcInUse Co").await;

    // 故意传错 version，且该公司**确实**仍映射着工序
    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/outsource-companies/{cid}/soft-delete"),
            Some(json!({"version": 99})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "vc 必须先于 in-use 判定: {env}");
    assert_eq!(
        env["code"].as_i64().unwrap(),
        40901,
        "version 过期必须先返 40901（曾返 21205）: {env}"
    );
}

/// `version` 是必填字段：body 里没有 ⇒ axum 的 `422` + 纯文本，**不进 `R<T>` 信封**。
///
/// 用 `send_raw`：axum 的 `Json` 反序列化拒绝是纯文本 body，`send` 会在 JSON 解析处 panic。
#[tokio::test]
async fn soft_delete_company_requires_version_field() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let (s, _) = create_company(&app, &token, json!({"name": "NoVer Co"})).await;
    assert_eq!(s, StatusCode::CREATED);
    let cid = company_id_by_name(&pool, "NoVer Co").await;

    let (s, body) = send_raw(
        app,
        json_request(
            "POST",
            &format!("/outsource-companies/{cid}/soft-delete"),
            Some(json!({})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::UNPROCESSABLE_ENTITY,
        "缺 version 必须是 422（不是业务信封）: {body}"
    );
    assert!(
        !body.contains("\"code\""),
        "缺字段是 axum 的纯文本拒绝，不得有 code 字段: {body}"
    );
    assert!(
        body.contains("missing field `version`"),
        "错误正文应指出缺 version: {body}"
    );
}

#[tokio::test]
async fn soft_delete_outsource_company_in_use_returns_21205() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let p1 = seed_outsource_process(&pool, "PROC-INUSE", "外协INUSE").await;
    let (s, env) = create_company(
        &app,
        &token,
        json!({
            "name": "InUse Co",
            "process_ids": [p1.to_string()]
        }),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{env}");
    let cid = company_id_by_name(&pool, "InUse Co").await;
    let ver = company_version(&app, &token, &cid.to_string()).await;

    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/outsource-companies/{cid}/soft-delete"),
            Some(json!({"version": ver})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "in-use: {env}");
    assert_eq!(env["code"].as_i64().unwrap(), 21205);
}

/// 清空映射后软删成功（正向闭环）。
#[tokio::test]
async fn soft_delete_company_succeeds_after_clearing_processes() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let p1 = seed_outsource_process(&pool, "PROC-CLEAR", "外协CLEAR").await;
    let (s, env) = create_company(
        &app,
        &token,
        json!({"name": "Clear Co", "process_ids": [p1.to_string()]}),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{env}");
    let cid = company_id_by_name(&pool, "Clear Co").await;
    let cid = cid.to_string();

    let ver = company_version(&app, &token, &cid).await;
    let (s, env) = send(
        app.clone(),
        json_request(
            "POST",
            &format!("/outsource-companies/{cid}/update"),
            Some(json!({"version": ver, "process_ids": []})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{env}");

    let ver = company_version(&app, &token, &cid).await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/outsource-companies/{cid}/soft-delete"),
            Some(json!({"version": ver})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "清空后应能软删: {env}");
    assert!(env["data"].is_null(), "{env}");
}

#[tokio::test]
async fn list_outsource_companies_name_like_and_is_active_filter() {
    let (_pool, app, token, _fx) = bootstrap_as_manager().await;

    // 3 个公司：A 激活、B 激活、C 停用
    for (n, active) in [("AAAA Inc", true), ("BBBB Co", true), ("CCCC Ltd", false)] {
        let (s, _) = send(
            app.clone(),
            json_request(
                "POST",
                "/outsource-companies",
                Some(json!({"name": n, "is_active": active})),
                Some(&token),
            ),
        )
        .await;
        assert_eq!(s, StatusCode::CREATED);
    }

    // name_like=AA → 1（fixture FX-OC-001 不含 "AA" 子串，命中 0）
    let (_, env1) = send(
        app.clone(),
        json_request(
            "GET",
            "/outsource-companies?name_like=AA",
            None,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(env1["data"]["total"].as_i64().unwrap(), 1);

    // is_active=true → 3（fixture FX-OC-001 active + AAAA/BBBB active 共 3 行）
    // 2026-09-24 PR13 Phase H：fixture 预置 FX-OC-001（is_active=true），
    // 与原测试 2 行合计 3 行。原版断言 2 改为 3。
    let (_, env2) = send(
        app.clone(),
        json_request(
            "GET",
            "/outsource-companies?is_active=true",
            None,
            Some(&token),
        ),
    )
    .await;
    assert_eq!(env2["data"]["total"].as_i64().unwrap(), 3);
}

/// 2026-10-09：`POST /{id}/processes` 硬切下线，整体替换改由 `POST /{id}/update`
/// 的 `process_ids` 承担（同批新增的 `update_company_process_ids_three_states`
/// 覆盖三态语义与 diff 守卫）。本用例只钉一件事：**旧端点确实不再存在**。
///
/// 旧路径是 2 段 `/{id}/processes`，删掉路由后同 router 里没有任何 2 段 POST 路由
/// 会匹配它 ⇒ matchit 返 **404**（不是 405 / 422）。
#[tokio::test]
async fn company_processes_endpoint_is_gone() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let p1 = seed_outsource_process(&pool, "SP-GONE", "sp-gone").await;
    let (s, env) = create_company(
        &app,
        &token,
        json!({"name": "GoneProc Co", "process_ids": [p1.to_string()]}),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{env}");
    let cid = company_id_by_name(&pool, "GoneProc Co").await;

    let (s, _) = send_raw(
        app,
        json_request(
            "POST",
            &format!("/outsource-companies/{cid}/processes"),
            Some(json!({"process_ids": [p1.to_string()]})),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::NOT_FOUND,
        "POST /{cid}/processes 必须已硬切下线（404）"
    );
}

#[tokio::test]
async fn list_outsource_companies_by_process_filters_inactive() {
    let (pool, app, token, _fx) = bootstrap_as_manager().await;
    let p = seed_outsource_process(&pool, "BYP", "byp").await;
    // active
    let (s, env) = create_company(
        &app,
        &token,
        json!({"name": "Active Co", "process_ids": [p.to_string()]}),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{env}");
    // inactive
    let (s, env) = create_company(
        &app,
        &token,
        json!({"name": "Inactive Co", "is_active": false, "process_ids": [p.to_string()]}),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{env}");
    let inactive_id = company_id_by_name(&pool, "Inactive Co").await;

    let (_, env) = send(
        app,
        json_request(
            "GET",
            &format!("/outsource-companies/by-process/{p}"),
            None,
            Some(&token),
        ),
    )
    .await;
    let items = env["data"].as_array().unwrap();
    assert_eq!(items.len(), 1, "{env}");
    assert_ne!(
        items[0]["id"].as_str().unwrap(),
        inactive_id.to_string(),
        "{env}"
    );
}

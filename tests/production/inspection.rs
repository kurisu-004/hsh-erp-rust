//! prod::inspection 域端到端集成测试（2026-10-05 新增）
//!
//! 端点：`GET /api/v2/prod/inspection/scan/{serial_no}`
//!      （测试内路径 `/prod/inspection/scan/{serial_no}`，不带 `/api/v2` 前缀）
//!
//! 覆盖 12 个场景：
//!   1. **独立件树**：`hit_kind="PART"`、`assembly=null`、`children.len()==1`，
//!      该件的 `children` 是它的全部批次（8 个，含终态）
//!   2. **装配件树（★ 核心回归）**：扫子件 → `assembly` 非空、`children` 是**全部
//!      子件**（含两个没有任何批次的子件）
//!   3. **扫装配件条码** → `hit_kind="ASSEMBLY"`、树与场景 2 逐字段同形，但
//!      `is_scanned` 全 `false`
//!   4. **未命中** → HTTP 404 + `code == 20101`；纯空白串（`%20`）同样 404
//!   5. **软删闸门**：软删子件扫不到 / 软删批次不出现
//!   6. **`is_scanned`**：只在被扫中那个 part 的批次上为 `true`
//!   7. **状态覆盖**：8 种批次状态全在（口径是「不按状态过滤」）
//!   8. **OCC 锚**：`ScanBatchOut.version` 来自 `t_part_batch.version`，与
//!      `t_part.version` 不同
//!   9. **i64 序列化为 JSON string**：`children[].id` / `children[].children[].id`
//!      都是 string
//!  10. **角色守卫**：MANAGER / INSPECTOR 各 200；CLERK / CNC_PROGRAMMER /
//!      SHELF_ACCOUNT 各 403 + `code == 40300`
//!  11. **`process_name`**：`INSPECTION` / `READY_TO_SHIP` 批次恒 `null`（出池清
//!      `current_process_id` 不变式的正确结果），`IN_PROCESS` 批次取到工序名
//!  12. **`is_repairing`** 标记透传
//!
//! ## 集成测试范本（PR13 Phase F / H）
//! HTTP helper（`send` / `json_request` / `login_token` / `test_app` / `test_state` /
//! `test_pool`）与 fixture 全部走 `hsh_erp_test_support`，本文件**不**重复声明本地
//! helper；本端点**纯读**、断言全部落在 fixture 预置行上，故连「造差异行」的本地
//! `async fn` 都不需要。

use axum::http::StatusCode;
use serde_json::Value;
use sqlx::PgPool;

use hsh_erp_test_support::{
    InspectionFixture, json_request, load_inspection_fixture, login_token, send, test_app,
    test_pool, test_state,
};

/// 扫码端点路径前缀（测试 app 不带 `/api/v2` 前缀）。
const SCAN_PREFIX: &str = "/prod/inspection/scan";

/// 独立件的全部 8 个批次 id，**按 `batch_no ASC` 的返回序**。
///
/// 序 = 扫独立件时 `children[0].children` 的期望序（SQL 按 `batch_no ASC, id ASC`
/// 排，内存分组后顺序不变），第 8 位在 fixture 里是软删批次 —— 它**不该**出现，
/// 故本常量只列前 7 个活跃批次。
const STANDALONE_BATCH_IDS: [i64; 7] = [
    InspectionFixture::BATCH_INSPECTION,
    InspectionFixture::BATCH_PENDING,
    InspectionFixture::BATCH_IN_PROCESS,
    InspectionFixture::BATCH_READY_TO_SHIP,
    InspectionFixture::BATCH_COMPLETED,
    InspectionFixture::BATCH_CANCELLED,
    InspectionFixture::BATCH_REPAIRING,
];

/// 装配件树的 3 个活跃子件 id，**按 `serial_no ASC` 的返回序**（子件序列号由父件
/// 序列号派生 `{asm}-{i:02d}`，该序即业务装配序）。
const CHILD_IDS: [i64; 3] = [
    InspectionFixture::PART_CHILD_1,
    InspectionFixture::PART_CHILD_2,
    InspectionFixture::PART_CHILD_3,
];

// ===========================================================================
//  Bootstrap
// ===========================================================================

/// fresh database + inspection fixture（提供 5 个角色账号）→ 以 MANAGER 身份登录。
async fn bootstrap() -> (PgPool, axum::Router, String, InspectionFixture) {
    let pool = test_pool().await;
    let fx = load_inspection_fixture(&pool).await;
    let app = test_app(test_state(pool.clone()).await);
    let token = login_token(&app, &fx.manager_username, InspectionFixture::PASSWORD).await;
    (pool, app, token, fx)
}

// ===========================================================================
//  断言 helper
// ===========================================================================

/// 打一次扫码端点（`serial_no` 原样拼进路径 —— 需要传 `%20` 等转义形态时不带
/// `urlencoding` 处理），断言 200 + `code == 0`，返回信封。
async fn scan(app: &axum::Router, token: &str, serial_no: &str) -> Value {
    let uri = format!("{SCAN_PREFIX}/{serial_no}");
    let (status, env) = send(app.clone(), json_request("GET", &uri, None, Some(token))).await;
    assert_eq!(status, StatusCode::OK, "GET {uri}: {env}");
    assert_eq!(env["code"], 0, "GET {uri}: {env}");
    env
}

/// 从信封里按返回顺序抽出零件节点 id 列表（雪花 i64 → string）。
fn part_ids(env: &Value) -> Vec<String> {
    env["data"]["children"]
        .as_array()
        .expect("data.children")
        .iter()
        .map(|p| {
            p["id"]
                .as_str()
                .expect("part.id 必须是 string(i64)")
                .to_string()
        })
        .collect()
}

/// 从信封里按返回顺序抽出某个零件节点的批次 id 列表（雪花 i64 → string）。
fn batch_ids(part: &Value) -> Vec<String> {
    part["children"]
        .as_array()
        .expect("part.children")
        .iter()
        .map(|b| {
            b["id"]
                .as_str()
                .expect("batch.id 必须是 string(i64)")
                .to_string()
        })
        .collect()
}

/// 从信封里按 id 取单个零件节点（找不到则 panic，打印整份信封便于定位）。
fn part_by_id(env: &Value, id: i64) -> &Value {
    let want = id.to_string();
    env["data"]["children"]
        .as_array()
        .expect("data.children")
        .iter()
        .find(|p| p["id"].as_str() == Some(want.as_str()))
        .unwrap_or_else(|| panic!("零件 {id} 不在 children 里: {env}"))
}

/// 从零件节点里按 id 取单个批次（找不到则 panic，打印整份信封便于定位）。
fn batch_by_id<'a>(part: &'a Value, id: i64, env: &Value) -> &'a Value {
    let want = id.to_string();
    part["children"]
        .as_array()
        .expect("part.children")
        .iter()
        .find(|b| b["id"].as_str() == Some(want.as_str()))
        .unwrap_or_else(|| panic!("批次 {id} 不在该零件的 children 里: {env}"))
}

/// 把字符串 id 列表排序（消除顺序不确定性，便于精确集合断言）。
fn sorted(mut ids: Vec<String>) -> Vec<String> {
    ids.sort();
    ids
}

/// 本域独享 raw SQL helper（造场景差异行）：给指定零件挂一个 INSPECTION 批次。
///
/// 只在 `is_scanned` 场景里用到 —— 那个场景要给**兄弟子件**也挂批次，才能证伪
/// 「树里所有批次都 true」这种更松的实现（fixture 里兄弟子件本来是无批次的）。
async fn insert_inspection_batch(pool: &PgPool, part_id: i64, batch_no: i32) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;

    let id = hsh_erp_test_support::pool_snowflake()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_part_batch (id, part_id, batch_no, quantity, status, location, \
         version, created_at, updated_at) \
         VALUES ($1, $2, $3, 1, 'INSPECTION', NULL, 0, $4, $4)",
    )
    .bind(id)
    .bind(part_id)
    .bind(batch_no)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_part_batch (INSPECTION)");
    id
}

// ===========================================================================
//  Tests
// ===========================================================================

/// 场景 1: 独立件树 —— `assembly=null`、`children` 恰好 1 个节点、批次齐全。
#[tokio::test]
async fn standalone_part_returns_single_part_tree_with_all_batches() {
    let (_pool, app, token, _fx) = bootstrap().await;

    let env = scan(&app, &token, InspectionFixture::STANDALONE_SERIAL_NO).await;

    assert_eq!(env["data"]["hit_kind"], "PART", "独立件命中来源: {env}");
    assert_eq!(
        env["data"]["scanned_serial_no"], InspectionFixture::STANDALONE_SERIAL_NO,
        "回显的应是 trim 后的原始扫码串: {env}"
    );
    assert!(
        env["data"]["assembly"].is_null(),
        "独立件没有装配件节点（assembly 必须为 null）: {env}"
    );
    assert_eq!(
        part_ids(&env),
        vec![InspectionFixture::PART_STANDALONE.to_string()],
        "独立件树恰好 1 个顶层零件节点: {env}"
    );

    let part = part_by_id(&env, InspectionFixture::PART_STANDALONE);
    assert_eq!(
        batch_ids(part),
        STANDALONE_BATCH_IDS
            .iter()
            .map(|i| i.to_string())
            .collect::<Vec<_>>(),
        "该件的 children 应是它的全部活跃批次，按 batch_no ASC: {env}"
    );

    // 扫码串带尾随空白仍命中（service 统一 trim）
    let trimmed = scan(&app, &token, "SI-S1001%20").await;
    assert_eq!(
        trimmed["data"]["scanned_serial_no"], InspectionFixture::STANDALONE_SERIAL_NO,
        "尾随空白应被 trim 掉再回显: {trimmed}"
    );
}

/// 场景 2（★ 核心回归）: 扫装配件子件 → 返回**整棵装配件树**。
///
/// 这是本端点存在的全部理由。part 域既有 `GET /parts/by-serial/{serial_no}` 只返回
/// 单个 part、`GET /parts/by-serial/{serial_no}/part-batches` 只给单 part 上下文，
/// 两者都表达不了「装配件 + 全部子件 + 全部批次」。本端点扫子件时必须把**兄弟子件**
/// 也带出来 —— 否则扫码弹窗点「送检」时无从得知同批货里还有哪些子件压在品检架上。
#[tokio::test]
async fn scanning_child_returns_whole_assembly_tree() {
    let (_pool, app, token, _fx) = bootstrap().await;

    let env = scan(&app, &token, InspectionFixture::CHILD_1_SERIAL_NO).await;

    assert_eq!(env["data"]["hit_kind"], "PART", "扫子件命中 t_part: {env}");
    let asm = &env["data"]["assembly"];
    assert!(
        !asm.is_null(),
        "扫装配件子件时装配件节点必须有值: {env}"
    );
    assert_eq!(
        asm["id"].as_str(),
        Some(InspectionFixture::ASSEMBLY_ID.to_string().as_str()),
        "装配件节点 id 必须是被扫子件的父件: {env}"
    );
    assert_eq!(asm["serial_no"], InspectionFixture::ASSEMBLY_SERIAL_NO, "{env}");
    // 装配件节点**不该有**批次字段：`ScanAssemblyOut` 里根本没有 `children` 键
    // （t_assembly 在 t_part_batch 里没有行）。用 `get().is_none()` 而不是
    // `asm["children"].is_null()` —— 后者在 key 不存在时 serde_json 同样给
    // `Value::Null`，恒真、零信息量。
    assert!(
        asm.get("children").is_none(),
        "装配件节点不应带 children 字段（批次只挂在零件节点下）: {env}"
    );

    // ★ children 是**全部**活跃子件，不止被扫中的那个；软删子件不在其中
    assert_eq!(
        part_ids(&env),
        CHILD_IDS.iter().map(|i| i.to_string()).collect::<Vec<_>>(),
        "扫子件时 children 应是该装配件的全部活跃子件（按 serial_no ASC）: {env}"
    );

    // 被扫中的子件挂着自己的批次
    let c1 = part_by_id(&env, InspectionFixture::PART_CHILD_1);
    assert_eq!(
        batch_ids(c1),
        vec![InspectionFixture::BATCH_CHILD_INSPECTION.to_string()],
        "子件 #1 只有一个 INSPECTION 批次: {env}"
    );

    // 两个无批次子件**仍要出现**（batch 列表为空数组，不是 null）
    for id in [InspectionFixture::PART_CHILD_2, InspectionFixture::PART_CHILD_3] {
        let c = part_by_id(&env, id);
        assert_eq!(
            c["children"].as_array().expect("part.children").len(),
            0,
            "零件 {id} 没有任何批次 → children 应是空数组（不是 null）: {env}"
        );
    }

    // 反向确认：子件的 serial_no 与父件不同形，故两表命中不会互相误判
    assert_ne!(
        InspectionFixture::CHILD_1_SERIAL_NO,
        InspectionFixture::ASSEMBLY_SERIAL_NO
    );
}

/// 场景 3: 扫装配件条码 → `hit_kind="ASSEMBLY"`、树同形、`is_scanned` 全 false。
#[tokio::test]
async fn scanning_assembly_serial_returns_same_tree_without_is_scanned() {
    let (_pool, app, token, _fx) = bootstrap().await;

    let by_child = scan(&app, &token, InspectionFixture::CHILD_1_SERIAL_NO).await;
    let by_asm = scan(&app, &token, InspectionFixture::ASSEMBLY_SERIAL_NO).await;

    assert_eq!(by_asm["data"]["hit_kind"], "ASSEMBLY", "扫装配件条码: {by_asm}");
    assert_ne!(
        by_asm["data"]["hit_kind"], by_child["data"]["hit_kind"],
        "两条命中路径的 hit_kind 必须能区分: {by_asm}"
    );

    // 同一棵树：顶层零件 id 序逐字相同，装配件节点 id 相同
    assert_eq!(
        part_ids(&by_asm),
        part_ids(&by_child),
        "扫装配件条码与扫子件返回的顶层零件必须完全一致: {by_asm}"
    );
    assert_eq!(
        by_asm["data"]["assembly"]["id"].as_str(),
        Some(InspectionFixture::ASSEMBLY_ID.to_string().as_str()),
        "{by_asm}"
    );

    // 批次集合逐字相同
    for id in CHILD_IDS {
        assert_eq!(
            batch_ids(part_by_id(&by_asm, id)),
            batch_ids(part_by_id(&by_child, id)),
            "零件 {id} 的批次集合在两条路径下必须一致: {by_asm}"
        );
    }

    // ★ is_scanned 差异：扫子件时子件 #1 的批次为 true；扫装配件码时全 false
    let scanned_child_batch = batch_by_id(
        part_by_id(&by_asm, InspectionFixture::PART_CHILD_1),
        InspectionFixture::BATCH_CHILD_INSPECTION,
        &by_asm,
    );
    assert_eq!(
        scanned_child_batch["is_scanned"], false,
        "扫装配件条码时无命中零件 → 全部批次 is_scanned 必须为 false: {by_asm}"
    );
    assert_eq!(
        batch_by_id(
            part_by_id(&by_child, InspectionFixture::PART_CHILD_1),
            InspectionFixture::BATCH_CHILD_INSPECTION,
            &by_child
        )["is_scanned"],
        true,
        "扫子件时它自己的批次 is_scanned 必须为 true: {by_child}"
    );
}

/// 场景 4: 未命中 → HTTP 404 + `code == 20101`；纯空白串同样 404。
#[tokio::test]
async fn unknown_and_blank_serial_return_404_with_20101() {
    let (_pool, app, token, _fx) = bootstrap().await;

    for (label, serial) in [
        ("不存在的序列号", "SI-NOPE-999"),
        // 前缀不该命中（service 只做精确匹配，不做 ILIKE 模糊）
        ("已存在序列号的前缀", "SI-S100"),
        // 纯空白串：Path 解码后是 " "，service trim 后为空 → 按未命中收口
        ("纯空白串 %20", "%20"),
        ("全空白串 %20%20", "%20%20"),
    ] {
        let uri = format!("{SCAN_PREFIX}/{serial}");
        let (status, env) = send(app.clone(), json_request("GET", &uri, None, Some(&token))).await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "{label}（{uri}）应 404: {env}"
        );
        assert_eq!(
            env["code"], 20101,
            "{label} 的错误码必须是 20101 BIZ_PART_NOT_FOUND: {env}"
        );
    }
}

/// 场景 5: 软删闸门 —— 软删子件扫不到、软删批次不出现。
#[tokio::test]
async fn soft_deleted_child_and_batch_excluded() {
    let (pool, app, token, _fx) = bootstrap().await;

    // 前提：fixture 软删子件确实是「active assembly_id + deleted_at 非空」
    let (assembly_id, deleted_at): (Option<i64>, Option<chrono::NaiveDateTime>) = sqlx::query_as(
        "SELECT assembly_id, deleted_at FROM t_part WHERE id = $1",
    )
    .bind(InspectionFixture::PART_CHILD_DELETED)
    .fetch_one(&pool)
    .await
    .expect("select 软删子件行");
    assert_eq!(
        assembly_id,
        Some(InspectionFixture::ASSEMBLY_ID),
        "前提：软删子件的 assembly_id 指向装配件"
    );
    assert!(deleted_at.is_some(), "前提：软删子件 deleted_at 非空");

    // ① 扫软删子件 → 404 + 20101（扫到一个业务上已不存在的码）
    let uri = format!(
        "{SCAN_PREFIX}/{}",
        InspectionFixture::CHILD_DELETED_SERIAL_NO
    );
    let (status, env) = send(app.clone(), json_request("GET", &uri, None, Some(&token))).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "扫软删子件应 404: {env}");
    assert_eq!(env["code"], 20101, "扫软删子件错误码: {env}");

    // ② 软删子件不出现在装配件树的 children 里
    let tree = scan(&app, &token, InspectionFixture::ASSEMBLY_SERIAL_NO).await;
    let ids = part_ids(&tree);
    assert!(
        !ids.contains(&InspectionFixture::PART_CHILD_DELETED.to_string()),
        "软删子件不该出现在装配件树里: {tree}"
    );

    // ③ 软删批次不出现在独立件树的 children 里
    let standalone = scan(&app, &token, InspectionFixture::STANDALONE_SERIAL_NO).await;
    let batches = batch_ids(part_by_id(&standalone, InspectionFixture::PART_STANDALONE));
    assert!(
        !batches.contains(&InspectionFixture::BATCH_SOFT_DELETED.to_string()),
        "软删批次不该出现在扫码树里: {standalone}"
    );
    assert_eq!(
        sorted(batches.clone()),
        sorted(STANDALONE_BATCH_IDS.iter().map(|i| i.to_string()).collect()),
        "除软删批次外应恰好剩 fixture 的 7 个活跃批次: {standalone}"
    );
}

/// 场景 6: `is_scanned` 只在被扫中那个 part 的批次上为 `true`。
#[tokio::test]
async fn is_scanned_only_marks_the_scanned_part_batches() {
    let (pool, app, token, fx) = bootstrap().await;

    // 给兄弟子件 #2 也挂一个批次：没有这行，「兄弟批次 is_scanned=false」这条断言
    // 会因为集合为空而空过
    let sibling_batch = insert_inspection_batch(&pool, fx.part_child_2, 1).await;

    let env = scan(&app, &token, InspectionFixture::CHILD_1_SERIAL_NO).await;

    let c1 = part_by_id(&env, InspectionFixture::PART_CHILD_1);
    let batches_of_c1 = c1["children"].as_array().expect("子件 #1 批次");
    assert!(
        !batches_of_c1.is_empty(),
        "前提：子件 #1 至少要有一个批次: {env}"
    );
    for b in batches_of_c1 {
        assert_eq!(
            b["is_scanned"], true,
            "被扫中的子件，其批次 is_scanned 应全为 true: {env}"
        );
    }

    // 兄弟子件即便有批次也不该被标
    let c2 = part_by_id(&env, InspectionFixture::PART_CHILD_2);
    let sibling = batch_by_id(c2, sibling_batch, &env);
    assert_eq!(
        sibling["is_scanned"], false,
        "零件 {} 不是被扫中的那个 → 其批次 is_scanned 必须为 false: {env}",
        InspectionFixture::PART_CHILD_2
    );
    // 子件 #3 保持无批次（空数组）
    assert_eq!(
        part_by_id(&env, InspectionFixture::PART_CHILD_3)["children"]
            .as_array()
            .expect("子件 #3 批次")
            .len(),
        0,
        "子件 #3 无批次 → children 应是空数组: {env}"
    );
}

/// 场景 7: 状态覆盖 —— 8 种批次状态（含终态）全在，**不按状态过滤**。
#[tokio::test]
async fn all_batch_statuses_are_returned_including_terminal_ones() {
    let (_pool, app, token, _fx) = bootstrap().await;

    let env = scan(&app, &token, InspectionFixture::STANDALONE_SERIAL_NO).await;
    let part = part_by_id(&env, InspectionFixture::PART_STANDALONE);

    let mut got: Vec<(String, String)> = part["children"]
        .as_array()
        .expect("part.children")
        .iter()
        .map(|b| {
            (
                b["id"].as_str().expect("batch.id").to_string(),
                b["status"].as_str().expect("batch.status").to_string(),
            )
        })
        .collect();
    // 按 id 排序，让「哪一行是什么状态」可逐条机械核对
    got.sort_by(|a, b| a.0.cmp(&b.0));

    let want: Vec<(String, String)> = [
        (InspectionFixture::BATCH_INSPECTION, "INSPECTION"),
        (InspectionFixture::BATCH_PENDING, "PENDING"),
        (InspectionFixture::BATCH_IN_PROCESS, "IN_PROCESS"),
        (InspectionFixture::BATCH_READY_TO_SHIP, "READY_TO_SHIP"),
        (InspectionFixture::BATCH_COMPLETED, "COMPLETED"),
        (InspectionFixture::BATCH_CANCELLED, "CANCELLED"),
        (InspectionFixture::BATCH_REPAIRING, "IN_PROCESS"),
    ]
    .iter()
    .map(|(id, st)| (id.to_string(), (*st).to_string()))
    .collect();

    assert_eq!(got, want, "扫码树读全部批次、原文透出 status: {env}");

    // 显式把「终态批次也在」这条口径钉一遍（前端据此决定按钮显隐，后端不代做决策）
    let terminal: Vec<&str> = got.iter().map(|(_, st)| st.as_str()).collect();
    for st in ["COMPLETED", "CANCELLED"] {
        assert!(
            terminal.contains(&st),
            "终态批次 {st} 必须在树里（端点刻意不按状态过滤）: {env}"
        );
    }
}

/// 场景 8: OCC 锚 —— 批次 `version` 来自 `t_part_batch.version`，**不是** part 的。
///
/// fixture 刻意让独立件 `t_part.version = 1` 而它的 INSPECTION 批次
/// `t_part_batch.version = 3`。若实现误取 `t_part.version`，本例会红。
#[tokio::test]
async fn batch_version_comes_from_part_batch_not_part() {
    let (pool, app, token, fx) = bootstrap().await;

    // 前提：DB 里两列的值确实不同
    let (part_version, batch_version): (i32, i32) = sqlx::query_as(
        "SELECT p.version, b.version FROM t_part p \
         JOIN t_part_batch b ON b.part_id = p.id WHERE p.id = $1 AND b.id = $2",
    )
    .bind(fx.part_standalone)
    .bind(InspectionFixture::BATCH_INSPECTION)
    .fetch_one(&pool)
    .await
    .expect("select part / batch 两列 version");
    assert_eq!(part_version, InspectionFixture::PART_VERSION);
    assert_eq!(batch_version, InspectionFixture::BATCH_INSPECTION_VERSION);
    assert_ne!(
        part_version, batch_version,
        "前提：零件 version 与批次 version 必须不同，否则本例会空过"
    );

    let env = scan(&app, &token, InspectionFixture::STANDALONE_SERIAL_NO).await;
    let part = part_by_id(&env, InspectionFixture::PART_STANDALONE);

    // 零件级 version 是 t_part.version（仅展示，不参与任何批次写操作）
    assert_eq!(part["version"], part_version, "{env}");

    // 批次级 version 必须是 t_part_batch.version —— 前端拿它当 OCC 锚
    assert_eq!(
        batch_by_id(part, InspectionFixture::BATCH_INSPECTION, &env)["version"],
        batch_version,
        "批次 version 必须取自 t_part_batch.version（不能拿零件 version 顶替）: {env}"
    );

    // 子件侧同样构造了「零件 4 ≠ 批次 7」的一对，把该口径在装配件树上再钉一次
    let tree = scan(&app, &token, InspectionFixture::CHILD_1_SERIAL_NO).await;
    let c1 = part_by_id(&tree, InspectionFixture::PART_CHILD_1);
    assert_eq!(c1["version"], InspectionFixture::CHILD_1_VERSION, "{tree}");
    assert_eq!(
        batch_by_id(c1, InspectionFixture::BATCH_CHILD_INSPECTION, &tree)["version"],
        InspectionFixture::BATCH_CHILD_VERSION,
        "子件批次 version 必须取自 t_part_batch.version: {tree}"
    );
}

/// 场景 9: i64 序列化为 JSON string（雪花 ID > 2^53，JS `Number` 会丢精度）。
#[tokio::test]
async fn i64_ids_are_serialized_as_json_strings() {
    let (_pool, app, token, _fx) = bootstrap().await;

    let env = scan(&app, &token, InspectionFixture::CHILD_1_SERIAL_NO).await;

    assert!(
        env["data"]["assembly"]["id"].is_string(),
        "assembly.id 必须是 string(i64) 而非 number: {env}"
    );
    for p in env["data"]["children"].as_array().expect("data.children") {
        assert!(
            p["id"].is_string(),
            "children[].id 必须是 string(i64) 而非 number: {env}"
        );
        for b in p["children"].as_array().expect("part.children") {
            assert!(
                b["id"].is_string(),
                "children[].children[].id 必须是 string(i64) 而非 number: {env}"
            );
        }
    }

    // 而 version / batch_no / quantity 是计数与版本号，序列化为 JSON number
    let c1 = part_by_id(&env, InspectionFixture::PART_CHILD_1);
    assert!(c1["version"].is_number(), "version 应是 JSON number: {env}");
    assert!(c1["quantity"].is_number(), "quantity 应是 JSON number: {env}");
    let b = batch_by_id(
        c1,
        InspectionFixture::BATCH_CHILD_INSPECTION,
        &env,
    );
    assert!(b["version"].is_number(), "批次 version 应是 JSON number: {env}");
    assert!(b["batch_no"].is_number(), "batch_no 应是 JSON number: {env}");
}

/// 场景 10: 角色守卫 —— MANAGER / INSPECTOR 放行；三个角色 403 + 40300。
#[tokio::test]
async fn role_guard_allows_manager_and_inspector_rejects_other_three() {
    let (_pool, app, _token, fx) = bootstrap().await;

    for (label, username) in [
        ("MANAGER", fx.manager_username.as_str()),
        ("INSPECTOR", fx.inspector_username.as_str()),
    ] {
        let token = login_token(&app, username, InspectionFixture::PASSWORD).await;
        let (status, env) = send(
            app.clone(),
            json_request(
                "GET",
                &format!("{SCAN_PREFIX}/{}", InspectionFixture::CHILD_1_SERIAL_NO),
                None,
                Some(&token),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{label} 应可访问: {env}");
        assert_eq!(env["code"], 0, "{label}: {env}");
    }

    // 三个越权角色：合法登录但被 service 守卫拒
    for (label, username) in [
        ("CLERK", fx.clerk_username.as_str()),
        ("CNC_PROGRAMMER", fx.cnc_username.as_str()),
        ("SHELF_ACCOUNT", fx.shelf_username.as_str()),
    ] {
        let token = login_token(&app, username, InspectionFixture::PASSWORD).await;
        let (status, env) = send(
            app.clone(),
            json_request(
                "GET",
                &format!("{SCAN_PREFIX}/{}", InspectionFixture::CHILD_1_SERIAL_NO),
                None,
                Some(&token),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{label} 应 403: {env}");
        assert_eq!(env["code"], 40300, "{label} 的 FORBIDDEN 错误码: {env}");
    }
}

/// 场景 11: `process_name` —— 出池态恒 `null`，生产中批次取到工序名。
///
/// `INSPECTION` / `READY_TO_SHIP` 批次的 `current_process_id` 按「出池必须清该列」
/// 不变式被置 NULL，故扫码树里它们的 `process_name` 恒 `null` —— 这是**正确**结果，
/// 本例的作用是把这条口径钉死，防止后人当 bug 去"修"（去 JOIN
/// `current_process_step_id` 反而会显示过时信息）。
#[tokio::test]
async fn process_name_is_null_for_out_of_pool_states() {
    let (pool, app, token, _fx) = bootstrap().await;

    // 前提：INSPECTION / READY_TO_SHIP 批次的 current_process_id 确实是 NULL
    for (id, want_status) in [
        (InspectionFixture::BATCH_INSPECTION, "INSPECTION"),
        (InspectionFixture::BATCH_READY_TO_SHIP, "READY_TO_SHIP"),
    ] {
        let (status, current_process_id): (String, Option<i64>) = sqlx::query_as(
            "SELECT status, current_process_id FROM t_part_batch WHERE id = $1",
        )
        .bind(id)
        .fetch_one(&pool)
        .await
        .expect("select 批次 current_process_id");
        assert_eq!(status, want_status, "前提：批次 {id} 的状态");
        assert!(
            current_process_id.is_none(),
            "前提：批次 {id}（{status}）出池后 current_process_id 应为 NULL"
        );
    }

    let env = scan(&app, &token, InspectionFixture::STANDALONE_SERIAL_NO).await;
    let part = part_by_id(&env, InspectionFixture::PART_STANDALONE);

    // INSPECTION 批次：恒 null（不是空串）
    let b_ins = batch_by_id(part, InspectionFixture::BATCH_INSPECTION, &env);
    assert!(
        b_ins["process_name"].is_null(),
        "INSPECTION 批次的 process_name 应为 null（出池已清 current_process_id）: {env}"
    );

    // READY_TO_SHIP 批次：同样恒 null（必经 INSPECTION）
    let b_ship = batch_by_id(part, InspectionFixture::BATCH_READY_TO_SHIP, &env);
    assert!(
        b_ship["process_name"].is_null(),
        "READY_TO_SHIP 批次的 process_name 应为 null: {env}"
    );

    // PENDING 批次：未进生产流，也没有 current_process_id
    assert!(
        batch_by_id(part, InspectionFixture::BATCH_PENDING, &env)["process_name"].is_null(),
        "PENDING 批次没有当前工序: {env}"
    );

    // ★ IN_PROCESS 批次：取到工序名（证明上三条不是因为「恒为 null」而空过）
    assert_eq!(
        batch_by_id(part, InspectionFixture::BATCH_IN_PROCESS, &env)["process_name"],
        InspectionFixture::PROCESS_A_NAME,
        "IN_PROCESS 批次的 process_name 必须取到工序名: {env}"
    );
    assert_eq!(
        batch_by_id(part, InspectionFixture::BATCH_REPAIRING, &env)["process_name"],
        InspectionFixture::PROCESS_B_NAME,
        "返修中批次的 process_name 必须取到工序名: {env}"
    );

    // holder 名从品检架 / 生产架三表 COALESCE 拼出
    assert_eq!(
        b_ins["current_holder_display"],
        "SI inspection shelf",
        "INSPECTION 批次的 holder 应拼出品检架名: {env}"
    );
    assert_eq!(
        b_ins["location"], "INSPECTION_SHELF",
        "INSPECTION 批次的 location: {env}"
    );
}

/// 场景 12: `is_repairing` 标记透传（前端据此禁用「指定工序」）。
#[tokio::test]
async fn is_repairing_flag_is_passed_through() {
    let (_pool, app, token, _fx) = bootstrap().await;

    let env = scan(&app, &token, InspectionFixture::STANDALONE_SERIAL_NO).await;
    let part = part_by_id(&env, InspectionFixture::PART_STANDALONE);

    assert_eq!(
        batch_by_id(part, InspectionFixture::BATCH_REPAIRING, &env)["is_repairing"],
        true,
        "返修中批次的 is_repairing 必须透传为 true: {env}"
    );
    // 其余批次全 false（证明上条不是因为该字段恒为 true 而空过）
    for id in [
        InspectionFixture::BATCH_INSPECTION,
        InspectionFixture::BATCH_PENDING,
        InspectionFixture::BATCH_IN_PROCESS,
        InspectionFixture::BATCH_READY_TO_SHIP,
        InspectionFixture::BATCH_COMPLETED,
        InspectionFixture::BATCH_CANCELLED,
    ] {
        assert_eq!(
            batch_by_id(part, id, &env)["is_repairing"],
            false,
            "批次 {id} 不是返修中 → is_repairing 应为 false: {env}"
        );
    }
}

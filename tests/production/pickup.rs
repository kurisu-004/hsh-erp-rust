//! prod::batch pick-up 端点集成测试（2026-10-03 新增）
//!
//! 覆盖 `POST /api/v2/prod/batches/{batch_id}/pick-up` 的整批领取与**部分领取**
//! （`quantity` 缺省 = 整批；`0 < quantity < batch.quantity` 时 service 自动拆批，
//! 把拆出来的那部分交给工人，源批次留在生产架上、数量递减）：
//!   1. 整批领取（不传 quantity）→ 200；批次 location=WORKER、holder=worker、数量不变
//!   2. 部分领取（IN_PROCESS+PRODUCTION_SHELF 源）→ 源批次数量递减且**仍在
//!      PRODUCTION_SHELF**、新批次数量 = qty 且 location=WORKER；`parent_batch_id`
//!      关系正确；`GET /parts/{id}/events` 能查到 SPLIT + PICKED_UP 两条事件且
//!      quantity 正确
//!
//!   （2b）部分领取（PENDING 源）→ 走 `mark_batch_with_status_and_meta`（新批次
//!   version 恒为 0）那条分支
//!
//!   3. `quantity > batch.quantity` → 20111 BIZ_PART_BATCH_INVALID_QUANTITY
//!   4. `quantity <= 0` → 20111 同码
//!   5. `quantity == batch.quantity` → **合法**，等同整批领取（与 `POST .../split`
//!      要求「严格小于」的语义差异，必须有测试钉住）
//!   6. OCC 冲突（传错的 `version`）→ 40901 VERSION_CONFLICT 且**没有残留新批次**
//!   7. 角色门禁：SHELF_ACCOUNT（扫码台角色）可通过；INSPECTOR 被拒 40300
//!
//! ## `shelf_id` 可选（2026-10-04 新增用例组，本改动的核心回归）
//!
//!   8. **请求体完全不带 `shelf_id`** → 200，领取成功且 status / location / holder 逐字正确（与场景 1 只差「少一个 shelf_id」）
//!   9. `shelf_id` 缺省 + 部分领取 → 200，拆批语义与场景 2 一致
//!  10. `shelf_id` **传了非法值仍被拒**（不存在 20501 / 停用 20512 / INSPECTION 区 20104）—— 钉住「传了才校验」
//!  11. `"shelf_id": 43`（JSON 数字而非字符串）→ 422 纯文本（`deserialize_i64_opt` 只吃 str，与改动前一致、未放宽）
//!  12. `"shelf_id": null` ≡ 字段缺省（`deserialize_i64_opt` 输入行为矩阵的第四格）
//!  13. 缺 `version` / 缺 `worker_id` → 仍 422（钉住「本改动只放宽 `shelf_id`」）
//!
//! ## 集成测试范本（PR13 Phase F）
//! 所有 HTTP / fixture helper 一律 `use hsh_erp_test_support::{...}`，**不保留
//! 本地副本**。本文件独享的 raw SQL 构造（`insert_part` / `insert_part_batch` /
//! `insert_active_worker`）与 `tests/production/batch.rs` 同惯例保留为本地 fn。
//!
//! ## 串行化
//! 进程级 test_pool 每次 fresh database，DB 间 schema 完全独立，无需 Mutex。

use axum::http::StatusCode;
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;

use hsh_erp_test_support::{
    PartFixture, ProductionFixture, json_request, load_production_fixture, login_token,
    pool_snowflake, send, test_app, test_pool, test_state,
};

/// 从 test-support 的**进程级共享**雪花生成器取下一个 id。
///
/// 2026-10-03（review 第 1 轮 Minor-3）改用共享生成器：本文件原先在三处 helper 里
/// 自建 `SnowflakeIdGenerator::new(1_577_836_800_000, 1)`，instance 硬编码为 1。
/// `test-support/src/pool.rs` 已明确记载 2026-09-20 把 `pool_snowflake()` 的
/// instance 从固定 `1` 改为 `test_snowflake_instance()`（pid ⊕ 启动纳秒）派生，
/// 理由是 nextest process-per-test 模型下跨进程并行共享 instance=1 会撞
/// redis session key（`sessions:user:{id}` 等）。当前每测试独立 database、本文件
/// 不碰 redis，故固定 instance=1 不会立刻炸，但那是已被明确移除的模式，且与同批
/// `tests/part/pickable_by_work_type.rs` 的做法不一致 —— 统一回共享生成器。
fn next_id() -> i64 {
    pool_snowflake()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .next_id()
}

/// 一次测试的公共上下文：pool / app / 三种身份的 token / fixture。
struct Ctx {
    pool: PgPool,
    app: axum::Router,
    manager: String,
    inspector: String,
    shelf_account: String,
    fx: ProductionFixture,
}

impl Ctx {
    fn shelf_id(&self) -> i64 {
        // part fixture 的 FX-SH-PROD：zone=PRODUCTION + is_active（pick-up 校验二者）
        PartFixture::PRODUCTION_SHELF_ID
    }
}

async fn bootstrap() -> Ctx {
    let pool = test_pool().await;
    let fx = load_production_fixture(&pool).await;
    let app = test_app(test_state(pool.clone()).await);
    let manager = login_token(&app, &fx.part_manager_username, ProductionFixture::PASSWORD).await;
    let inspector = login_token(
        &app,
        &fx.part_inspector_username,
        ProductionFixture::PASSWORD,
    )
    .await;
    let shelf_account = login_token(
        &app,
        &fx.part_shelf_account_username,
        ProductionFixture::PASSWORD,
    )
    .await;
    Ctx {
        pool,
        app,
        manager,
        inspector,
        shelf_account,
        fx,
    }
}

// ===========================================================================
//  prod::batch pick-up 域独享 helper
// ===========================================================================

/// 直插一个最小 `t_part` 行（status='PENDING'，数量 = `quantity`）。
async fn insert_part(pool: &PgPool, quantity: i32) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;
    let id = next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_part (id, name, drawing_no, applicant_name, quantity, request_date, \
         planned_delivery_date, customer_id, status, version, created_at, updated_at) \
         VALUES ($1, 'PICKUP-TEST-PART', 'PICKUP-DWG', '', $2, CURRENT_DATE, CURRENT_DATE, \
         $3, 'PENDING', 0, $4, $4)",
    )
    .bind(id)
    .bind(quantity)
    .bind(PartFixture::CUSTOMER_L2_ID)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_part PENDING");
    id
}

/// 直插一个 `t_part_batch` 行。
///
/// `status='PENDING'` 时 `location` 留 NULL（pick-up 的 PENDING 分支不要求 location）；
/// `status='IN_PROCESS'` 时必须给 `location='PRODUCTION_SHELF'`（service 守该前置）。
async fn insert_part_batch(
    pool: &PgPool,
    part_id: i64,
    quantity: i32,
    status: &str,
    holder_id: Option<i64>,
) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;
    let id = next_id();
    let now = now_naive();
    let location = if status == "IN_PROCESS" {
        Some("PRODUCTION_SHELF")
    } else {
        None
    };
    sqlx::query(
        "INSERT INTO t_part_batch (id, part_id, batch_no, quantity, status, location, \
         current_holder_id, version, created_at, updated_at) \
         VALUES ($1, $2, 1, $3, $4, $5, $6, 0, $7, $7)",
    )
    .bind(id)
    .bind(part_id)
    .bind(quantity)
    .bind(status)
    .bind(location)
    .bind(holder_id)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_part_batch");
    id
}

/// 直插一个 active 且已绑 work_type 的工人（pick-up 的两个硬性前置）。
async fn insert_active_worker(pool: &PgPool, work_type_id: i64) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;
    let id = next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_worker (id, badge_code, name, is_active, work_type_id, version, \
         created_at, updated_at) VALUES ($1, $2, 'PICKUP-WORKER', true, $3, 0, $4, $4)",
    )
    .bind(id)
    .bind(format!("PICKUP-{id}"))
    .bind(work_type_id)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_worker");
    id
}

/// 读批次的 5 个关键列。
async fn read_batch(
    pool: &PgPool,
    batch_id: i64,
) -> (i32, String, Option<String>, Option<i64>, i32) {
    sqlx::query_as(
        "SELECT quantity, status, location, current_holder_id, version \
         FROM t_part_batch WHERE id = $1",
    )
    .bind(batch_id)
    .fetch_one(pool)
    .await
    .expect("read t_part_batch")
}

/// 该 part 名下未软删的批次行数（拆批残留检查）。
async fn count_batches(pool: &PgPool, part_id: i64) -> i64 {
    sqlx::query_scalar(
        "SELECT COUNT(*) FROM t_part_batch WHERE part_id = $1 AND deleted_at IS NULL",
    )
    .bind(part_id)
    .fetch_one(pool)
    .await
    .expect("count t_part_batch")
}

/// 直插一个**已停用**的 PRODUCTION 架（`is_active=false`），用于钉住 `20512`。
///
/// `code` 用固定短码：`t_shelf.code` 是 `varchar(32)`，塞不进
/// `PICKUP-INACTIVE-<19 位雪花 id>`（36 字符）；每测试一个 fresh database，
/// 固定码不会与 fixture 的 `FX-SH-*` 撞。
async fn insert_inactive_production_shelf(pool: &PgPool) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;
    let id = next_id();
    let now = now_naive();
    sqlx::query(
        "INSERT INTO t_shelf (id, code, name, zone, is_active, display_order, version, \
         created_at, updated_at) VALUES ($1, 'PICKUP-INACT', 'PICKUP 停用生产架', \
         'PRODUCTION', false, 0, 0, $2, $2)",
    )
    .bind(id)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_shelf (inactive)");
    id
}

/// 按 `event_type` 从 `GET /parts/{id}/events` 里挑出唯一一条事件。
fn pick_event<'a>(env: &'a Value, event_type: &str) -> &'a Value {
    let items = env["data"].as_array().expect("data is array");
    let hits: Vec<&Value> = items
        .iter()
        .filter(|e| e["event_type"] == event_type)
        .collect();
    assert_eq!(
        hits.len(),
        1,
        "应有且仅有 1 条 {event_type} 事件，实际 {hits:?}；全部事件：{env}"
    );
    hits[0]
}

async fn fetch_events(ctx: &Ctx, part_id: i64) -> (StatusCode, Value) {
    send(
        ctx.app.clone(),
        json_request(
            "GET",
            &format!("/parts/{part_id}/events"),
            None,
            Some(&ctx.manager),
        ),
    )
    .await
}

// ===========================================================================
//  Tests
// ===========================================================================

/// 场景 1: 整批领取（**不传 quantity**）→ 200；location=WORKER、holder=worker、数量不变
///
/// 这条是回归基线：`quantity` 缺省必须与本次改动前的行为逐字一致 —— 不拆批、
/// 不多写事件，批次本身从 IN_PROCESS+PRODUCTION_SHELF 换到 IN_PROCESS+WORKER。
#[tokio::test]
async fn pick_up_without_quantity_picks_whole_batch() {
    let ctx = bootstrap().await;
    let worker_id = insert_active_worker(&ctx.pool, ctx.fx.work_type_a_id).await;
    let part_id = insert_part(&ctx.pool, 10).await;
    let batch_id =
        insert_part_batch(&ctx.pool, part_id, 10, "IN_PROCESS", Some(ctx.shelf_id())).await;

    let (s, env) = send(
        ctx.app.clone(),
        json_request(
            "POST",
            &format!("/prod/batches/{batch_id}/pick-up"),
            Some(json!({
                "version": 0,
                "worker_id": worker_id.to_string(),
                "shelf_id": ctx.shelf_id().to_string(),
            })),
            Some(&ctx.manager),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "整批领取应 200: {env}");
    assert_eq!(env["code"], 0);
    // 响应体形状不变：仍是 part 级视图
    assert_eq!(
        env["data"]["id"],
        part_id.to_string(),
        "data 应是 PartOut: {env}"
    );

    let (quantity, status, location, holder, version) = read_batch(&ctx.pool, batch_id).await;
    assert_eq!(quantity, 10, "整批领取不应改数量");
    assert_eq!(status, "IN_PROCESS");
    assert_eq!(location.as_deref(), Some("WORKER"));
    assert_eq!(holder, Some(worker_id));
    assert_eq!(version, 1, "OCC 应 +1");
    assert_eq!(
        count_batches(&ctx.pool, part_id).await,
        1,
        "整批路径不应拆出新批次"
    );

    // 只写 PICKED_UP、无 SPLIT
    let (_, ev) = fetch_events(&ctx, part_id).await;
    let picked = pick_event(&ev, "PICKED_UP");
    assert_eq!(picked["quantity"], 10, "PICKED_UP.quantity = 整批量");
    assert_eq!(picked["batch_id"], batch_id.to_string());
    assert_eq!(
        ev["data"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["event_type"] == "SPLIT")
            .count(),
        0,
        "整批路径不应写 SPLIT 事件"
    );
}

/// 场景 2: 部分领取（源批次 IN_PROCESS+PRODUCTION_SHELF）→ 自动拆批后交付新批次
///
/// 断言清单：
/// - 源批次 quantity = 原总量 - qty，且**仍留在 PRODUCTION_SHELF**（余量可被下一
///   个工人再领一次）
/// - 新批次 quantity = qty、location=WORKER、holder=worker、status=IN_PROCESS
/// - `parent_batch_id` 双向关系正确（源 → 新）
/// - `GET /parts/{id}/events` 里有 SPLIT + PICKED_UP 两条，quantity 均为 qty，
///   且都挂在新批次上
#[tokio::test]
async fn pick_up_partial_splits_batch_and_delivers_new_batch() {
    let ctx = bootstrap().await;
    let worker_id = insert_active_worker(&ctx.pool, ctx.fx.work_type_a_id).await;
    let part_id = insert_part(&ctx.pool, 10).await;
    let shelf_id = ctx.shelf_id();
    let batch_id = insert_part_batch(&ctx.pool, part_id, 10, "IN_PROCESS", Some(shelf_id)).await;

    let (s, env) = send(
        ctx.app.clone(),
        json_request(
            "POST",
            &format!("/prod/batches/{batch_id}/pick-up"),
            Some(json!({
                "version": 0,
                "worker_id": worker_id.to_string(),
                "shelf_id": shelf_id.to_string(),
                "quantity": "4",
            })),
            Some(&ctx.manager),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "部分领取应 200: {env}");
    assert_eq!(env["code"], 0);
    assert_eq!(
        env["data"]["id"],
        part_id.to_string(),
        "响应体形状不变（仍是 PartOut）: {env}"
    );

    // 源批次：数量递减、**位置不动**（余量仍在生产架上）
    let (src_qty, src_status, src_loc, src_holder, src_ver) = read_batch(&ctx.pool, batch_id).await;
    assert_eq!(src_qty, 6, "源批次应剩 10 - 4 = 6");
    assert_eq!(src_status, "IN_PROCESS");
    assert_eq!(
        src_loc.as_deref(),
        Some("PRODUCTION_SHELF"),
        "源批次应仍在生产架上（余量可再被领）"
    );
    assert_eq!(src_holder, Some(shelf_id), "源批次 holder 仍是货架");
    // 2026-10-03（review 第 1 轮 Minor-2）新增断言：拆批对源批次做了
    // `version = version + 1`（`PartBatchRepo::_split_batch_inner`），故源批次
    // 最终 version 必须是 1（初始 0 + 拆批 +1；**不再**被翻状态，因为翻的是
    // 新批次 —— 源批次只有拆批这一次写入）。
    //
    // 为什么必须钉住：这是前端「领走一部分后再领余量」时唯一需要的 OCC 锚点。
    // `_split_batch_inner` 里那行 `version = version + 1` 看起来像多余的写入
    // （同一条 UPDATE 已经在改 quantity），将来有人清理「冗余字段」时把它删掉，
    // 不会有任何测试变红，而前端会拿着 `batch_version = 0`（已过期）去打下一次
    // pick-up，表现为莫名其妙的 409。
    assert_eq!(
        src_ver, 1,
        "部分领取后源批次 version 必须 +1（拆批 OCC 写），前端据此重拉列表拿新锚点"
    );

    // 新批次：由拆分产生，quantity=4 且已交到工人手上
    let new_id: i64 = sqlx::query_scalar(
        "SELECT id FROM t_part_batch WHERE part_id = $1 AND deleted_at IS NULL AND id <> $2",
    )
    .bind(part_id)
    .bind(batch_id)
    .fetch_one(&ctx.pool)
    .await
    .expect("部分领取应拆出一个新批次");
    let (new_qty, new_status, new_loc, new_holder, new_ver) = read_batch(&ctx.pool, new_id).await;
    assert_eq!(new_qty, 4, "新批次数量 = 拆走量");
    assert_eq!(new_status, "IN_PROCESS");
    assert_eq!(new_loc.as_deref(), Some("WORKER"));
    assert_eq!(new_holder, Some(worker_id));
    // 新批次由 INSERT 建出时 version 恒 0，随后被翻到 IN_PROCESS+WORKER 再 +1
    assert_eq!(
        new_ver, 1,
        "新批次 version = INSERT 的 0 + 翻状态 +1（与源批次的 +1 是两笔独立写入）"
    );
    assert_eq!(count_batches(&ctx.pool, part_id).await, 2);

    // parent_batch_id：新批次 → 源批次
    let parent: Option<i64> =
        sqlx::query_scalar("SELECT parent_batch_id FROM t_part_batch WHERE id = $1")
            .bind(new_id)
            .fetch_one(&ctx.pool)
            .await
            .expect("read parent_batch_id");
    assert_eq!(
        parent,
        Some(batch_id),
        "新批次 parent_batch_id 应指向源批次"
    );
    // batch_no = max + 1（源保留 1，新批次拿 2）
    let new_no: i32 = sqlx::query_scalar("SELECT batch_no FROM t_part_batch WHERE id = $1")
        .bind(new_id)
        .fetch_one(&ctx.pool)
        .await
        .unwrap();
    let src_no: i32 = sqlx::query_scalar("SELECT batch_no FROM t_part_batch WHERE id = $1")
        .bind(batch_id)
        .fetch_one(&ctx.pool)
        .await
        .unwrap();
    assert_eq!(src_no, 1, "源批次保留原 batch_no");
    assert_eq!(new_no, 2, "新批次 batch_no = max + 1");

    // 事件：SPLIT + PICKED_UP 都挂在新批次上、数量均为 4
    let (es, ev) = fetch_events(&ctx, part_id).await;
    assert_eq!(es, StatusCode::OK, "list events: {ev}");
    let split_ev = pick_event(&ev, "SPLIT");
    assert_eq!(split_ev["batch_id"], new_id.to_string());
    assert_eq!(split_ev["quantity"], 4);
    assert_eq!(split_ev["note"], "pick-up 部分领取自动拆批");
    let picked_ev = pick_event(&ev, "PICKED_UP");
    assert_eq!(
        picked_ev["batch_id"],
        new_id.to_string(),
        "PICKED_UP 应指向新批次"
    );
    assert_eq!(picked_ev["quantity"], 4, "PICKED_UP.quantity = 实际交付量");
}

/// 场景 2b: 部分领取（PENDING 源）→ 走 `mark_batch_with_status_and_meta` 分支
///
/// 源批次是 PENDING 时，拆出来的新批次同样是 PENDING（`new_batch_status` 传源
/// status），翻状态走 status_gate 漏斗且 OCC 锚新批次的 version 0。
#[tokio::test]
async fn pick_up_partial_from_pending_batch_succeeds() {
    let ctx = bootstrap().await;
    let worker_id = insert_active_worker(&ctx.pool, ctx.fx.work_type_a_id).await;
    let part_id = insert_part(&ctx.pool, 6).await;
    let shelf_id = ctx.shelf_id();
    let batch_id = insert_part_batch(&ctx.pool, part_id, 6, "PENDING", None).await;

    let (s, env) = send(
        ctx.app.clone(),
        json_request(
            "POST",
            &format!("/prod/batches/{batch_id}/pick-up"),
            Some(json!({
                "version": 0,
                "worker_id": worker_id.to_string(),
                "shelf_id": shelf_id.to_string(),
                "quantity": "2",
            })),
            Some(&ctx.manager),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "PENDING 源部分领取应 200: {env}");

    let (src_qty, src_status, _src_loc, _src_holder, src_ver) =
        read_batch(&ctx.pool, batch_id).await;
    assert_eq!(src_qty, 4);
    assert_eq!(src_status, "PENDING", "源批次状态不变（拆批只动数量）");
    // 与 IN_PROCESS 源那条用例同款断言：源批次只被拆批写过一次，version 必为 1
    assert_eq!(
        src_ver, 1,
        "PENDING 源部分领取后 version 同样 +1（拆批 OCC 写，与起点状态无关）"
    );

    let new_id: i64 = sqlx::query_scalar(
        "SELECT id FROM t_part_batch WHERE part_id = $1 AND deleted_at IS NULL AND id <> $2",
    )
    .bind(part_id)
    .bind(batch_id)
    .fetch_one(&ctx.pool)
    .await
    .expect("应拆出新批次");
    let (new_qty, new_status, new_loc, new_holder, _v) = read_batch(&ctx.pool, new_id).await;
    assert_eq!(new_qty, 2);
    assert_eq!(new_status, "IN_PROCESS", "新批次 PENDING → IN_PROCESS");
    assert_eq!(new_loc.as_deref(), Some("WORKER"));
    assert_eq!(new_holder, Some(worker_id));
}

/// 场景 3: `quantity > batch.quantity` → 20111 BIZ_PART_BATCH_INVALID_QUANTITY
#[tokio::test]
async fn pick_up_quantity_over_batch_quantity_returns_invalid_quantity() {
    let ctx = bootstrap().await;
    let worker_id = insert_active_worker(&ctx.pool, ctx.fx.work_type_a_id).await;
    let part_id = insert_part(&ctx.pool, 5).await;
    let batch_id =
        insert_part_batch(&ctx.pool, part_id, 5, "IN_PROCESS", Some(ctx.shelf_id())).await;

    let (s, env) = send(
        ctx.app.clone(),
        json_request(
            "POST",
            &format!("/prod/batches/{batch_id}/pick-up"),
            Some(json!({
                "version": 0,
                "worker_id": worker_id.to_string(),
                "shelf_id": ctx.shelf_id().to_string(),
                "quantity": "6",
            })),
            Some(&ctx.manager),
        ),
    )
    .await;
    // 20111 在本仓映射 HTTP 400（与 POST .../split 的数量非法同码同状态）
    assert_eq!(s, StatusCode::BAD_REQUEST, "超量领取应 400: {env}");
    assert_eq!(env["code"], 20111, "BIZ_PART_BATCH_INVALID_QUANTITY: {env}");
    assert_eq!(
        count_batches(&ctx.pool, part_id).await,
        1,
        "校验失败不应拆批"
    );
    let (quantity, _status, _loc, _holder, _v) = read_batch(&ctx.pool, batch_id).await;
    assert_eq!(quantity, 5, "源批次数量不应被改");
}

/// 场景 4: `quantity <= 0` → 20111 同码
#[tokio::test]
async fn pick_up_non_positive_quantity_returns_invalid_quantity() {
    let ctx = bootstrap().await;
    let worker_id = insert_active_worker(&ctx.pool, ctx.fx.work_type_a_id).await;
    let part_id = insert_part(&ctx.pool, 5).await;
    let batch_id =
        insert_part_batch(&ctx.pool, part_id, 5, "IN_PROCESS", Some(ctx.shelf_id())).await;

    for qty in [json!("0"), json!("-3")] {
        let (s, env) = send(
            ctx.app.clone(),
            json_request(
                "POST",
                &format!("/prod/batches/{batch_id}/pick-up"),
                Some(json!({
                    "version": 0,
                    "worker_id": worker_id.to_string(),
                    "shelf_id": ctx.shelf_id().to_string(),
                    "quantity": qty,
                })),
                Some(&ctx.manager),
            ),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "quantity={qty} 应 400: {env}");
        assert_eq!(env["code"], 20111, "BIZ_PART_BATCH_INVALID_QUANTITY: {env}");
    }
    let (quantity, _status, _loc, _holder, _v) = read_batch(&ctx.pool, batch_id).await;
    assert_eq!(quantity, 5, "源批次数量不应被改");
    assert_eq!(count_batches(&ctx.pool, part_id).await, 1);
}

/// 场景 5: `quantity == batch.quantity` → **合法**，等同整批领取
///
/// 这是与 `POST /api/v2/prod/batches/{batch_id}/split`（要求严格小于）的语义
/// 差异：pick-up 里「等于」就是整批领取的显式写法，无需拆批，故不应报
/// 20111、也不应产生第二个批次。
#[tokio::test]
async fn pick_up_quantity_equal_batch_quantity_is_whole_batch() {
    let ctx = bootstrap().await;
    let worker_id = insert_active_worker(&ctx.pool, ctx.fx.work_type_a_id).await;
    let part_id = insert_part(&ctx.pool, 8).await;
    let shelf_id = ctx.shelf_id();
    let batch_id = insert_part_batch(&ctx.pool, part_id, 8, "IN_PROCESS", Some(shelf_id)).await;

    let (s, env) = send(
        ctx.app.clone(),
        json_request(
            "POST",
            &format!("/prod/batches/{batch_id}/pick-up"),
            Some(json!({
                "version": 0,
                "worker_id": worker_id.to_string(),
                "shelf_id": shelf_id.to_string(),
                "quantity": "8",
            })),
            Some(&ctx.manager),
        ),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::OK,
        "quantity == batch.quantity 应等同整批领取（不是非法值）: {env}"
    );
    let (quantity, status, location, holder, _v) = read_batch(&ctx.pool, batch_id).await;
    assert_eq!(quantity, 8, "整批领取数量不变");
    assert_eq!(status, "IN_PROCESS");
    assert_eq!(location.as_deref(), Some("WORKER"));
    assert_eq!(holder, Some(worker_id));
    assert_eq!(
        count_batches(&ctx.pool, part_id).await,
        1,
        "quantity == 总量时不应拆批"
    );
}

/// 场景 6: OCC 冲突（传错 version）→ 40901，且**没有残留新批次**
///
/// 拆批与翻状态都在 handler 开的事务里，故 OCC 失败后 part 名下仍只有 1 个批次、
/// 数量不变。本例构造「外部把 version 顶到 7、请求仍传 0」。
#[tokio::test]
async fn pick_up_version_conflict_leaves_no_split_residue() {
    let ctx = bootstrap().await;
    let worker_id = insert_active_worker(&ctx.pool, ctx.fx.work_type_a_id).await;
    let part_id = insert_part(&ctx.pool, 10).await;
    let batch_id =
        insert_part_batch(&ctx.pool, part_id, 10, "IN_PROCESS", Some(ctx.shelf_id())).await;
    sqlx::query("UPDATE t_part_batch SET version = 7 WHERE id = $1")
        .bind(batch_id)
        .execute(&ctx.pool)
        .await
        .unwrap();

    let (s, env) = send(
        ctx.app.clone(),
        json_request(
            "POST",
            &format!("/prod/batches/{batch_id}/pick-up"),
            Some(json!({
                "version": 0,
                "worker_id": worker_id.to_string(),
                "shelf_id": ctx.shelf_id().to_string(),
                "quantity": "4",
            })),
            Some(&ctx.manager),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "OCC 冲突应 409: {env}");
    assert_eq!(env["code"], 40901, "VERSION_CONFLICT: {env}");
    assert_eq!(
        count_batches(&ctx.pool, part_id).await,
        1,
        "OCC 失败后不得残留拆出的新批次"
    );
    let (quantity, _status, location, _holder, version) = read_batch(&ctx.pool, batch_id).await;
    assert_eq!(quantity, 10, "数量不应被扣");
    assert_eq!(location.as_deref(), Some("PRODUCTION_SHELF"));
    assert_eq!(version, 7, "version 不应被改");
}

/// 场景 7a: 角色门禁 —— SHELF_ACCOUNT（扫码台就是这个角色）可通过 pick-up
#[tokio::test]
async fn pick_up_allowed_for_shelf_account() {
    let ctx = bootstrap().await;
    let worker_id = insert_active_worker(&ctx.pool, ctx.fx.work_type_a_id).await;
    let part_id = insert_part(&ctx.pool, 3).await;
    let shelf_id = ctx.shelf_id();
    let batch_id = insert_part_batch(&ctx.pool, part_id, 3, "IN_PROCESS", Some(shelf_id)).await;

    let (s, env) = send(
        ctx.app.clone(),
        json_request(
            "POST",
            &format!("/prod/batches/{batch_id}/pick-up"),
            Some(json!({
                "version": 0,
                "worker_id": worker_id.to_string(),
                "shelf_id": shelf_id.to_string(),
                "quantity": "1",
            })),
            Some(&ctx.shelf_account),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "SHELF_ACCOUNT 应可 pick-up: {env}");
    let (_q, _s, location, _holder, _v) = read_batch(&ctx.pool, batch_id).await;
    assert_eq!(
        location.as_deref(),
        Some("PRODUCTION_SHELF"),
        "源批次留原处"
    );
    let new_id: i64 = sqlx::query_scalar(
        "SELECT id FROM t_part_batch WHERE part_id = $1 AND deleted_at IS NULL AND id <> $2",
    )
    .bind(part_id)
    .bind(batch_id)
    .fetch_one(&ctx.pool)
    .await
    .expect("应拆出新批次");
    let (_nq, _ns, new_loc, new_holder, _nv) = read_batch(&ctx.pool, new_id).await;
    assert_eq!(new_loc.as_deref(), Some("WORKER"));
    assert_eq!(new_holder, Some(worker_id));
}

/// 场景 7b: 角色门禁 —— INSPECTOR 无 pick-up 权限 → 40300 FORBIDDEN
#[tokio::test]
async fn pick_up_forbidden_for_inspector() {
    let ctx = bootstrap().await;
    let worker_id = insert_active_worker(&ctx.pool, ctx.fx.work_type_a_id).await;
    let part_id = insert_part(&ctx.pool, 3).await;
    let batch_id =
        insert_part_batch(&ctx.pool, part_id, 3, "IN_PROCESS", Some(ctx.shelf_id())).await;

    let (s, env) = send(
        ctx.app.clone(),
        json_request(
            "POST",
            &format!("/prod/batches/{batch_id}/pick-up"),
            Some(json!({
                "version": 0,
                "worker_id": worker_id.to_string(),
                "shelf_id": ctx.shelf_id().to_string(),
                "quantity": "1",
            })),
            Some(&ctx.inspector),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "INSPECTOR 应 403: {env}");
    assert_eq!(env["code"], 40300, "FORBIDDEN: {env}");
    assert_eq!(count_batches(&ctx.pool, part_id).await, 1, "被拒后不应拆批");
    let (quantity, _status, _loc, _holder, _v) = read_batch(&ctx.pool, batch_id).await;
    assert_eq!(quantity, 3, "被拒后数量不变");
}

/// 场景 8（2026-10-04 新增，**本改动的核心回归**）：请求体**完全不带 `shelf_id`**
/// → 200，领取成功
///
/// 与场景 1 的请求体**只差「少一个 shelf_id」**，其余完全相同
/// （同为 IN_PROCESS+PRODUCTION_SHELF 源、version=0、不传 quantity），因此
/// 「传 / 不传」对结果的影响必须为零：status / location / holder / version /
/// 批次数 / 事件流逐字一致。
///
/// 为什么值得单独钉一条：该字段对 pick-up 的最终结果**零影响**（本路径 3 条
/// `t_part_batch` 写入的 SET / WHERE 均无货架列，`t_part_event` 无货架列，响应
/// VO `PartOut` 无 shelf 字段），原先那条 `validate_shelf_zone` 是防呆断言而
/// 非安全边界。若后人把它误当成「批次归属依据」写进 WHERE/SET，本用例会红。
#[tokio::test]
async fn pick_up_without_shelf_id_succeeds() {
    let ctx = bootstrap().await;
    let worker_id = insert_active_worker(&ctx.pool, ctx.fx.work_type_a_id).await;
    let part_id = insert_part(&ctx.pool, 10).await;
    let batch_id =
        insert_part_batch(&ctx.pool, part_id, 10, "IN_PROCESS", Some(ctx.shelf_id())).await;

    let (s, env) = send(
        ctx.app.clone(),
        json_request(
            "POST",
            &format!("/prod/batches/{batch_id}/pick-up"),
            // ⚠️ body 里**没有** shelf_id 这个 key（不是 null）
            Some(json!({
                "version": 0,
                "worker_id": worker_id.to_string(),
            })),
            Some(&ctx.manager),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "shelf_id 缺省应照常领取: {env}");
    assert_eq!(env["code"], 0);
    assert_eq!(
        env["data"]["id"],
        part_id.to_string(),
        "响应体形状不变（仍是 PartOut）: {env}"
    );

    // 与场景 1 的断言集合逐条对齐：状态机结果必须完全相同
    let (quantity, status, location, holder, version) = read_batch(&ctx.pool, batch_id).await;
    assert_eq!(quantity, 10, "整批领取不应改数量");
    assert_eq!(status, "IN_PROCESS");
    assert_eq!(location.as_deref(), Some("WORKER"));
    assert_eq!(holder, Some(worker_id), "holder 应写入 worker_id");
    assert_eq!(version, 1, "OCC 应 +1");
    assert_eq!(
        count_batches(&ctx.pool, part_id).await,
        1,
        "整批路径不应拆出新批次"
    );

    let (_, ev) = fetch_events(&ctx, part_id).await;
    let picked = pick_event(&ev, "PICKED_UP");
    assert_eq!(picked["quantity"], 10);
    assert_eq!(picked["batch_id"], batch_id.to_string());
    assert_eq!(
        ev["data"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["event_type"] == "SPLIT")
            .count(),
        0,
        "整批路径不应写 SPLIT 事件"
    );
}

/// 场景 9（2026-10-04 新增）：`shelf_id` 缺省 + **部分领取** → 200，拆批语义不变
///
/// 覆盖「部分领取的自动拆批路径（`_split_batch_inner`）与 `shelf_id` 缺省正交」
/// 这一点：拆批 SQL 与后续翻状态都不碰货架，故两者同时出现也必须照常工作。
#[tokio::test]
async fn pick_up_partial_without_shelf_id_succeeds() {
    let ctx = bootstrap().await;
    let worker_id = insert_active_worker(&ctx.pool, ctx.fx.work_type_a_id).await;
    let part_id = insert_part(&ctx.pool, 10).await;
    let batch_id =
        insert_part_batch(&ctx.pool, part_id, 10, "IN_PROCESS", Some(ctx.shelf_id())).await;

    let (s, env) = send(
        ctx.app.clone(),
        json_request(
            "POST",
            &format!("/prod/batches/{batch_id}/pick-up"),
            Some(json!({
                "version": 0,
                "worker_id": worker_id.to_string(),
                "quantity": "4",
            })),
            Some(&ctx.manager),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "shelf_id 缺省 + 部分领取应 200: {env}");

    // 源批次：余量留原处（与场景 2 一致）
    let (src_qty, src_status, src_loc, _src_holder, src_ver) =
        read_batch(&ctx.pool, batch_id).await;
    assert_eq!(src_qty, 6, "源批次应剩 10 - 4 = 6");
    assert_eq!(src_status, "IN_PROCESS");
    assert_eq!(
        src_loc.as_deref(),
        Some("PRODUCTION_SHELF"),
        "源批次应仍在生产架上"
    );
    assert_eq!(src_ver, 1, "源批次 version +1（拆批 OCC 写）");

    // 新批次：拆走的那份交到工人手上
    let new_id: i64 = sqlx::query_scalar(
        "SELECT id FROM t_part_batch WHERE part_id = $1 AND deleted_at IS NULL AND id <> $2",
    )
    .bind(part_id)
    .bind(batch_id)
    .fetch_one(&ctx.pool)
    .await
    .expect("部分领取应拆出一个新批次");
    let (new_qty, new_status, new_loc, new_holder, _v) = read_batch(&ctx.pool, new_id).await;
    assert_eq!(new_qty, 4);
    assert_eq!(new_status, "IN_PROCESS");
    assert_eq!(new_loc.as_deref(), Some("WORKER"));
    assert_eq!(new_holder, Some(worker_id));
}

/// 场景 10（2026-10-04 新增）：`shelf_id` **传了非法值仍被拒**
///
/// 这是本次改动的**反向钉子**：`shelf_id` 变可选后，「传了就校验」这一半必须
/// 原样保留。三条子用例各覆盖 `validate_shelf_zone` 的一个分支：
/// - 不存在 / 已软删 → `20501 BIZ_SHELF_NOT_FOUND`（HTTP 404）
/// - 已停用 → `20512 BIZ_SHELF_INACTIVE`（HTTP 400）
/// - zone 不是 PRODUCTION → `20104 BIZ_INVALID_VALUE`（HTTP 400）
///
/// 少了这组用例，「把 `if let` 写反成 `if let None`」或干脆删掉校验都不会有
/// 任何测试变红。
#[tokio::test]
async fn pick_up_invalid_shelf_id_still_rejected() {
    let ctx = bootstrap().await;
    let inactive_shelf = insert_inactive_production_shelf(&ctx.pool).await;
    // fixture 内不存在、也永远不会创建的 id（与 PartFixture 的 9000000000000000xx
    // 段物理不相交）
    let missing_shelf = 9_000_000_000_000_099_999i64;

    // (shelf_id 字面值, 期望 code, 期望 HTTP 状态, 场景说明)
    // 用 `String` 而非 `&str`：id 是现算出来的，`&x.to_string()` 会借到语句结束就
    // 被释放的临时值（E0716）。
    let cases: Vec<(String, i32, StatusCode, &'static str)> = vec![
        (
            missing_shelf.to_string(),
            20501,
            StatusCode::NOT_FOUND,
            "shelf 不存在 / 已软删",
        ),
        (
            inactive_shelf.to_string(),
            20512,
            StatusCode::BAD_REQUEST,
            "shelf 已停用",
        ),
        (
            PartFixture::INSPECTION_SHELF_ID.to_string(),
            20104,
            StatusCode::BAD_REQUEST,
            "shelf 在 INSPECTION 区",
        ),
    ];

    for (idx, (shelf_id, want_code, want_status, why)) in cases.into_iter().enumerate() {
        // 每条子用例用独立的 part / 批次，避免前一条失败后留下脏状态干扰后一条
        let worker_id = insert_active_worker(&ctx.pool, ctx.fx.work_type_a_id).await;
        let part_id = insert_part(&ctx.pool, 10).await;
        let batch_id =
            insert_part_batch(&ctx.pool, part_id, 10, "IN_PROCESS", Some(ctx.shelf_id())).await;

        let (s, env) = send(
            ctx.app.clone(),
            json_request(
                "POST",
                &format!("/prod/batches/{batch_id}/pick-up"),
                Some(json!({
                    "version": 0,
                    "worker_id": worker_id.to_string(),
                    "shelf_id": shelf_id,
                    "quantity": "4",
                })),
                Some(&ctx.manager),
            ),
        )
        .await;
        assert_eq!(
            s, want_status,
            "[{idx}] {why}：期望 HTTP {want_status}，实际 {s}: {env}"
        );
        assert_eq!(
            env["code"], want_code,
            "[{idx}] {why}：期望 code {want_code}，实际 {env}"
        );

        // 校验在事务内、且在拆批**之前** —— 失败后不得有任何残留
        assert_eq!(
            count_batches(&ctx.pool, part_id).await,
            1,
            "[{idx}] {why}：非法 shelf 必须在拆批前被拒，不得拆批"
        );
        let (quantity, status, location, holder, version) = read_batch(&ctx.pool, batch_id).await;
        assert_eq!(quantity, 10, "[{idx}] {why}：数量不应被扣");
        assert_eq!(status, "IN_PROCESS");
        assert_eq!(
            location.as_deref(),
            Some("PRODUCTION_SHELF"),
            "[{idx}] {why}：位置不应被改"
        );
        assert_eq!(
            holder,
            Some(ctx.shelf_id()),
            "[{idx}] {why}：holder 不应被改"
        );
        assert_eq!(version, 0, "[{idx}] {why}：version 不应被改");
    }
}

/// 场景 11（2026-10-04 新增）：`"shelf_id": 43`（JSON 数字）→ 422
///
/// 线上形态是 **JSON 字符串**（雪花 id 精度）。`deserialize_i64_opt` 内部先
/// `Option::<String>::deserialize` 再 parse，故**只吃字符串**：`"shelf_id": 43`
/// 在提取器阶段就失败。
///
/// ⚠️ 与改动前一致：`deserialize_i64`（原必填版）同样只吃 str，本次改动**没有**
/// 顺带放宽数字输入。别把它误读成「变可选 = 顺带接受数字」。
///
/// ⚠️ 响应是 axum `Json` 提取器的**纯文本** 422，不是业务信封，故不能用
/// `hsh_erp_test_support::send`（它对非 JSON body 直接 panic），这里直读 raw body。
#[tokio::test]
async fn pick_up_shelf_id_as_json_number_is_rejected() {
    use axum::body::to_bytes;

    let ctx = bootstrap().await;
    let worker_id = insert_active_worker(&ctx.pool, ctx.fx.work_type_a_id).await;
    let part_id = insert_part(&ctx.pool, 10).await;
    let batch_id =
        insert_part_batch(&ctx.pool, part_id, 10, "IN_PROCESS", Some(ctx.shelf_id())).await;

    let resp = ctx
        .app
        .clone()
        .oneshot(json_request(
            "POST",
            &format!("/prod/batches/{batch_id}/pick-up"),
            Some(json!({
                "version": 0,
                "worker_id": worker_id.to_string(),
                "shelf_id": ctx.shelf_id(),   // ← JSON 数字，不是字符串
            })),
            Some(&ctx.manager),
        ))
        .await
        .expect("oneshot");
    assert_eq!(
        resp.status(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "shelf_id 传 JSON 数字应 422（deserialize_i64_opt 只吃 str）"
    );
    let raw = to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("read body");
    let text = String::from_utf8_lossy(&raw);
    assert!(
        text.contains("shelf_id"),
        "422 原因应指向 shelf_id（否则说明拒绝来自别的字段，用例会假绿）: {text}"
    );

    // 提取器就拒了，业务逻辑一行没跑 —— 批次必须原封不动
    let (quantity, status, location, holder, version) = read_batch(&ctx.pool, batch_id).await;
    assert_eq!(quantity, 10);
    assert_eq!(status, "IN_PROCESS");
    assert_eq!(location.as_deref(), Some("PRODUCTION_SHELF"));
    assert_eq!(holder, Some(ctx.shelf_id()));
    assert_eq!(version, 0);
}

/// 场景 12（2026-10-04 新增）：`"shelf_id": null` ≡ 字段缺省
///
/// 补齐 `deserialize_i64_opt` 四种输入的行为矩阵：缺省（场景 8）/ `null`（本条）/
/// 字符串「`"43"`」（场景 1-7 全部既有用例）/ 数字「`43`」（场景 11）。
///
/// `deserialize_i64_opt` 的实现是 `Option::<String>::deserialize(d)` —— JSON
/// `null` 与字段缺省都落进外层 `None`，两者**不可区分**、也不需要区分：语义都是
/// 「不做任何校验」。本条把该等价性钉住，免得后人以为 `null` 会被当成「显式清空」
/// 之类的第三种语义。
#[tokio::test]
async fn pick_up_null_shelf_id_is_treated_as_absent() {
    let ctx = bootstrap().await;
    let worker_id = insert_active_worker(&ctx.pool, ctx.fx.work_type_a_id).await;
    let part_id = insert_part(&ctx.pool, 7).await;
    let batch_id =
        insert_part_batch(&ctx.pool, part_id, 7, "IN_PROCESS", Some(ctx.shelf_id())).await;

    let (s, env) = send(
        ctx.app.clone(),
        json_request(
            "POST",
            &format!("/prod/batches/{batch_id}/pick-up"),
            Some(json!({
                "version": 0,
                "worker_id": worker_id.to_string(),
                "shelf_id": Value::Null,
            })),
            Some(&ctx.manager),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "shelf_id = null 应与缺省同效: {env}");

    let (quantity, status, location, holder, version) = read_batch(&ctx.pool, batch_id).await;
    assert_eq!(quantity, 7);
    assert_eq!(status, "IN_PROCESS");
    assert_eq!(location.as_deref(), Some("WORKER"));
    assert_eq!(holder, Some(worker_id));
    assert_eq!(version, 1);
}

/// 场景 13（2026-10-04 新增，review 第 1 轮 M-4）：`version` / `worker_id` 缺失仍 422
///
/// 本次把 `shelf_id` 从必填放宽为可选，最容易被顺手搞混的就是「哪些字段仍必填」——
/// `version`（OCC 锚）与 `worker_id`（持有件工人）**必须保持必填**。代码上的依据：
/// 两者都**没有** `#[serde(default)]`，serde 对缺字段直接报错 ⇒ axum `Json` 提取器
/// 422。缺了这条用例，将来有人给 `worker_id` 补一个 `default` 不会有任何测试变红。
///
/// 与场景 11 同款处理：响应是 axum `JsonRejection` 的**纯文本** 422（非业务信封），
/// 不能用 `hsh_erp_test_support::send`（它对非 JSON body 直接 panic），故直读 raw
/// body 并断言拒绝原因里点名了缺失的那个字段。
#[tokio::test]
async fn pick_up_missing_required_fields_still_422() {
    use axum::body::to_bytes;

    let ctx = bootstrap().await;

    for (idx, omit) in ["version", "worker_id"].into_iter().enumerate() {
        // 每条子用例用独立的 part / 批次，避免前一条失败后留下脏状态干扰后一条
        let worker_id = insert_active_worker(&ctx.pool, ctx.fx.work_type_a_id).await;
        let part_id = insert_part(&ctx.pool, 4).await;
        let batch_id =
            insert_part_batch(&ctx.pool, part_id, 4, "IN_PROCESS", Some(ctx.shelf_id())).await;

        // 先造完整 body 再删掉一个 key —— 保证两条 body 只差「缺哪个必填字段」，
        // 其余（status / location / holder / version / shelf_id）逐字相同
        let mut body = json!({
            "version": 0,
            "worker_id": worker_id.to_string(),
            "shelf_id": ctx.shelf_id().to_string(),
        });
        let removed = body
            .as_object_mut()
            .expect("body 是 object")
            .remove(omit)
            .unwrap_or_else(|| panic!("完整 body 里应有 {omit} 键"));
        assert!(!removed.is_null(), "{omit} 本就不该是 null");

        let resp = ctx
            .app
            .clone()
            .oneshot(json_request(
                "POST",
                &format!("/prod/batches/{batch_id}/pick-up"),
                Some(body),
                Some(&ctx.manager),
            ))
            .await
            .expect("oneshot");
        assert_eq!(
            resp.status(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "[{idx}] 缺 {omit} 应 422（它仍是必填字段，不因 shelf_id 放宽而变可选）"
        );
        let raw = to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("read body");
        let text = String::from_utf8_lossy(&raw);
        assert!(
            text.contains(omit),
            "[{idx}] 422 原因应点名缺失字段 {omit}: {text}"
        );

        // 提取器就拒了，业务逻辑一行没跑 —— 批次必须原封不动
        let (quantity, status, location, holder, version) = read_batch(&ctx.pool, batch_id).await;
        assert_eq!(quantity, 4);
        assert_eq!(status, "IN_PROCESS");
        assert_eq!(location.as_deref(), Some("PRODUCTION_SHELF"));
        assert_eq!(holder, Some(ctx.shelf_id()));
        assert_eq!(version, 0);
    }
}

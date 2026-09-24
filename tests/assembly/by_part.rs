//! assembly 域 集成测试 — 2026-09-25 新增 D-08 `GET /parts/{part_id}/assembly`
//!
//! 覆盖场景：
//!   1. by_part_returns_assembly_detail: 建装配体（含子件）+ GET by part → Some(AssemblyDetail)
//!   2. by_part_part_not_found: 不存在的 part_id → 40401 PART_NOT_FOUND
//!   3. by_part_part_no_parent: 独立 part（assembly_id IS NULL）→ 返回 None

#![allow(dead_code, clippy::await_holding_lock, unused_imports)]

use sqlx::PgPool;

use hsh_erp_rust::auth::rbac::{CurrentUser, Role};
use hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator;
use hsh_erp_rust::modules::assembly::dto::{AssemblyChildAddRequest, AssemblyCreateRequest};
use hsh_erp_rust::modules::assembly::service::AssemblyService;
use hsh_erp_rust::modules::part::dto_crud::PartCreateRequest;
use hsh_erp_rust::modules::part::service::PartService;
use hsh_erp_rust::shared::error::AppError;

use hsh_erp_test_support::{AssemblyFixture, load_assembly_fixture, test_pool};

async fn setup() -> (PgPool, AssemblyFixture) {
    let pool = test_pool().await;
    let fx = load_assembly_fixture(&pool).await;
    (pool, fx)
}

async fn insert_l1_customer(pool: &PgPool, name: &str, serial_prefix: &str) -> i64 {
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let id = snowflake.next_id();
    let now = chrono::Local::now().naive_utc();
    sqlx::query!(
        "INSERT INTO t_customer (id, name, parent_id, serial_prefix, version, \
         created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, NULL, $3, 0, $4, NULL, $4, NULL)",
        id,
        name,
        serial_prefix,
        now,
    )
    .execute(pool)
    .await
    .expect("insert L1 customer");
    id
}

async fn insert_l2_customer(pool: &PgPool, name: &str, parent_id: i64) -> i64 {
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let id = snowflake.next_id();
    let now = chrono::Local::now().naive_utc();
    sqlx::query!(
        "INSERT INTO t_customer (id, name, parent_id, serial_prefix, version, \
         created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, $3, NULL, 0, $4, NULL, $4, NULL)",
        id,
        name,
        parent_id,
        now,
    )
    .execute(pool)
    .await
    .expect("insert L2 customer");
    id
}

fn test_current_user() -> CurrentUser {
    CurrentUser {
        id: 1,
        username: "by_part_tester".into(),
        roles: vec![Role::Manager],
        shelf_ids: vec![],
        shelf_wildcard: false,
    }
}

// ===========================================================================
//  Tests
// ===========================================================================

/// 1. happy path：建装配体 → 追加子件 → GET /parts/{child_id}/assembly → Some(detail)。
#[tokio::test]
async fn by_part_returns_assembly_detail() {
    let (pool, _fx) = setup().await;
    let l1 = insert_l1_customer(&pool, "客户BP-1", "F").await;
    let l2 = insert_l2_customer(&pool, "子客BP-1", l1).await;
    let current = test_current_user();
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);

    // 1. 建空装配体
    let req = AssemblyCreateRequest {
        drawing_no: "BP-D001".into(),
        name: "ASM-BP-1".into(),
        applicant_name: Some("申请人BP".into()),
        customer_id: l2.to_string(),
        request_date: None,
        planned_delivery_date: None,
        is_urgent: Some(false),
        quantity: Some(1),
        unit_price: None,
        total_price: None,
        order_no: None,
        system_delivery_date: None,
        note: None,
        children: vec![],
    };
    let mut tx = pool.begin().await.unwrap();
    let asm = AssemblyService::create_assembly(&mut tx, &snowflake, &req, vec![], &current)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let asm_id = asm.assembly.id;

    // 2. 追加 2 个子件
    for (i, (dn, name)) in [("BP-D001-A", "子件A"), ("BP-D001-B", "子件B")]
        .iter()
        .enumerate()
    {
        let req = AssemblyChildAddRequest {
            drawing_no: dn.to_string(),
            name: name.to_string(),
            planned_delivery_date: None,
            quantity: (i + 1) as i32,
        };
        let mut tx = pool.begin().await.unwrap();
        AssemblyService::add_assembly_child(&mut tx, &snowflake, asm_id, &req, &current)
            .await
            .unwrap();
        tx.commit().await.unwrap();
    }

    // 3. 取第一个子件 id
    let child_ids: Vec<i64> =
        sqlx::query_scalar("SELECT id FROM t_part WHERE assembly_id = $1 ORDER BY id ASC")
            .bind(asm_id)
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(child_ids.len(), 2);
    let first_child_id = child_ids[0];

    // 4. GET /parts/{first_child_id}/assembly → Some(detail)
    let mut conn = pool.acquire().await.unwrap();
    let detail_opt = PartService::get_assembly_by_part(&mut *conn, first_child_id, &current)
        .await
        .expect("get_assembly_by_part should succeed");
    drop(conn);

    let detail = detail_opt.expect("子件应能反查到父装配体");
    assert_eq!(
        detail.assembly.id, asm_id,
        "AssemblyDetail.assembly.id = 父装配体 id"
    );
    assert_eq!(detail.children.len(), 2, "children 含 2 个子件");
    assert_eq!(detail.children[0].id, child_ids[0]);
    assert_eq!(detail.children[1].id, child_ids[1]);
    assert_eq!(detail.assembly.applicant_name.as_deref(), Some("申请人BP"));
    assert_eq!(detail.assembly.customer_id, l2);
}

/// 2. 不存在的 part_id → 40401 PART_NOT_FOUND。
#[tokio::test]
async fn by_part_part_not_found() {
    let (pool, _fx) = setup().await;
    let current = test_current_user();

    let mut conn = pool.acquire().await.unwrap();
    let err = PartService::get_assembly_by_part(&mut *conn, 9_999_999_999, &current)
        .await
        .expect_err("不存在的 part_id 应抛错");
    drop(conn);

    match err {
        AppError::Biz { code, .. } => {
            assert_eq!(code, 20101, "BIZ_PART_NOT_FOUND（part 不存在）；got {code}");
        }
        other => panic!("期望 AppError::Biz(20101)，got {other:?}"),
    }
}

/// 3. 独立 part（assembly_id IS NULL）→ 返回 None。
#[tokio::test]
async fn by_part_part_no_parent() {
    let (pool, _fx) = setup().await;
    let l1 = insert_l1_customer(&pool, "客户BP-NP", "F").await;
    let l2 = insert_l2_customer(&pool, "子客BP-NP", l1).await;
    let current = test_current_user();
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);

    // 1. 直接建一个独立 part（无 assembly_id）
    let req = PartCreateRequest {
        name: "独立工单".to_string(),
        drawing_no: "NP-D001".to_string(),
        applicant_name: "独立申请人".to_string(),
        quantity: 1,
        request_date: chrono::Local::now().date_naive(),
        planned_delivery_date: chrono::Local::now().date_naive(),
        is_urgent: false,
        customer_id: l2,
        assembly_id: None,
        order_no: None,
        system_delivery_date: None,
        note: None,
    };
    let mut tx = pool.begin().await.unwrap();
    let out = PartService::create_part(&mut *tx, &snowflake, &req, &current)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let part_id = out.part.id;

    // 2. GET /parts/{part_id}/assembly → None
    let mut conn = pool.acquire().await.unwrap();
    let detail_opt = PartService::get_assembly_by_part(&mut *conn, part_id, &current)
        .await
        .expect("get_assembly_by_part 应成功（part 存在）");
    drop(conn);

    assert!(
        detail_opt.is_none(),
        "独立 part（assembly_id IS NULL）→ 应返回 None；got Some({:?})",
        detail_opt
    );
}

// 强制 PartService / PartCreateRequest 不被 unused warning（保留为扩展预留）
#[allow(dead_code)]
fn _phantom_part_service_usage() {
    let _: fn() -> Option<PartCreateRequest> = || None;
}

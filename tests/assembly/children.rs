//! assembly 域 集成测试 — 2026-09-25 新增 D-07 `POST /assemblies/{id}/children`
//!
//! 覆盖场景：
//!   1. add_child_happy_path: 创建装配体后追加单个子件（继承父件 7 个共享字段 + initial batch）
//!   2. add_child_assembly_not_found: 不存在的 assembly_id → 20301
//!   3. add_child_validation_error: drawing_no 空 / name 空 / quantity<=0 → 40001

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

// ===========================================================================
//  共享 setup（PR13 Phase H 范本化 + 本 sub-file 自定义 l1/l2 客户 fixture）
// ===========================================================================

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

async fn insert_serial_counter(pool: &PgPool, prefix: &str, initial: i64) {
    let now = chrono::Local::now().naive_utc();
    sqlx::query!(
        "INSERT INTO t_serial_counter (prefix, counter, version, created_at, created_by, \
         updated_at, updated_by) \
         VALUES ($1, $2, 0, $3, NULL, $3, NULL) \
         ON CONFLICT (prefix) DO UPDATE SET counter = EXCLUDED.counter, \
                                          version = 0, \
                                          updated_at = EXCLUDED.updated_at",
        prefix,
        initial,
        now,
    )
    .execute(pool)
    .await
    .expect("upsert t_serial_counter");
}

fn test_current_user() -> CurrentUser {
    CurrentUser {
        id: 1,
        username: "children_tester".into(),
        roles: vec![Role::Manager],
        shelf_ids: vec![],
        shelf_wildcard: false,
    }
}

/// 创建一个空装配体（无 PDF / 无 children），返回 assembly_id。
#[allow(clippy::too_many_arguments)]
async fn create_empty_assembly(
    pool: &PgPool,
    l2_customer_id: i64,
    drawing_no: &str,
    name: &str,
    applicant: Option<&str>,
    order_no: Option<&str>,
    note: Option<&str>,
    current: &CurrentUser,
) -> i64 {
    let req = AssemblyCreateRequest {
        drawing_no: drawing_no.to_string(),
        name: name.to_string(),
        applicant_name: applicant.map(|s| s.to_string()),
        customer_id: l2_customer_id.to_string(),
        request_date: None,
        planned_delivery_date: None,
        is_urgent: Some(false),
        quantity: Some(1),
        unit_price: None,
        total_price: None,
        order_no: order_no.map(|s| s.to_string()),
        system_delivery_date: None,
        note: note.map(|s| s.to_string()),
        children: vec![],
    };
    let mut tx = pool.begin().await.unwrap();
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let out = AssemblyService::create_assembly(&mut tx, &snowflake, &req, vec![], current)
        .await
        .expect("create empty assembly");
    tx.commit().await.unwrap();
    out.assembly.id
}

// ===========================================================================
//  Tests
// ===========================================================================

/// 1. happy path：建一个空装配体（带 7 个共享字段）→ 追加单个子件 →
///    验证子件继承 7 个字段 + assembly_id + initial batch。
#[tokio::test]
async fn add_child_happy_path() {
    let (pool, _fx) = setup().await;
    let l1 = insert_l1_customer(&pool, "客户CH-1", "F").await;
    let l2 = insert_l2_customer(&pool, "子客CH-1", l1).await;
    insert_serial_counter(&pool, "F", 0).await;
    let current = test_current_user();

    // 1. 建空装配体，applicant_name / order_no / note / is_urgent 都填值
    let asm_id = create_empty_assembly(
        &pool,
        l2,
        "CH-D001",
        "ASM-CH-1",
        Some("张三"),
        Some("CH-ORDER-001"),
        Some("加急备注"),
        &current,
    )
    .await;

    // 2. 追加单个子件
    let req = AssemblyChildAddRequest {
        drawing_no: "CH-CHILD-D001".to_string(),
        name: "子件1".to_string(),
        planned_delivery_date: None,
        quantity: 3,
    };
    let mut tx = pool.begin().await.unwrap();
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let child = AssemblyService::add_assembly_child(&mut tx, &snowflake, asm_id, &req, &current)
        .await
        .expect("add child should succeed");
    tx.commit().await.unwrap();

    // 3. 验证 PartListItem 返回值
    assert_eq!(child.part.name, "子件1");
    assert_eq!(child.part.drawing_no, "CH-CHILD-D001");
    assert_eq!(child.part.quantity, 3);
    assert_eq!(child.part.customer_id, l2, "子件 customer 继承父件");
    assert_eq!(
        child.part.assembly_id,
        Some(asm_id),
        "子件 assembly_id 指向父件"
    );
    assert_eq!(child.part.applicant_name, "张三", "applicant_name 继承父件");
    assert_eq!(child.part.order_no.as_deref(), Some("CH-ORDER-001"));
    assert_eq!(child.part.note.as_deref(), Some("加急备注"));
    assert!(!child.part.is_urgent);
    assert!(
        child.part.serial_no.is_none(),
        "无 PDF 路径 → serial_no = NULL"
    );
    assert_eq!(
        child.customer_name.as_deref(),
        Some("子客CH-1"),
        "customer_name 解析"
    );

    // 4. DB 验证 t_part 行（独立读）
    let row: (
        String,
        Option<String>,
        Option<String>,
        i32,
        i64,
        Option<i64>,
        bool,
    ) = sqlx::query_as(
        "SELECT name, order_no, note, quantity, customer_id, assembly_id, is_urgent \
         FROM t_part WHERE id = $1",
    )
    .bind(child.part.id)
    .fetch_one(&pool)
    .await
    .expect("query child part");
    assert_eq!(row.0, "子件1");
    assert_eq!(row.1.as_deref(), Some("CH-ORDER-001"));
    assert_eq!(row.2.as_deref(), Some("加急备注"));
    assert_eq!(row.3, 3);
    assert_eq!(row.4, l2);
    assert_eq!(row.5, Some(asm_id));
    assert!(!row.6);

    // 5. 验证初始 t_part_batch 插入（batch_no=1 / status='PENDING'）
    let batch_row: (i32, String) = sqlx::query_as(
        "SELECT batch_no, status FROM t_part_batch WHERE part_id = $1 ORDER BY batch_no ASC LIMIT 1",
    )
    .bind(child.part.id)
    .fetch_one(&pool)
    .await
    .expect("query initial batch");
    assert_eq!(batch_row.0, 1, "初始 batch batch_no=1");
    assert_eq!(batch_row.1, "PENDING", "初始 batch status=PENDING");
}

/// 2. 不存在的 assembly_id → 20301 BIZ_ASSEMBLY_NOT_FOUND。
#[tokio::test]
async fn add_child_assembly_not_found() {
    let (pool, _fx) = setup().await;
    let current = test_current_user();
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);

    let req = AssemblyChildAddRequest {
        drawing_no: "D-NOT-EXIST".to_string(),
        name: "子件".to_string(),
        planned_delivery_date: None,
        quantity: 1,
    };
    let mut tx = pool.begin().await.unwrap();
    let err =
        AssemblyService::add_assembly_child(&mut tx, &snowflake, 9_999_999_999, &req, &current)
            .await
            .expect_err("不存在的 assembly_id 应抛错");
    drop(tx);
    match err {
        AppError::Biz { code, .. } => {
            assert_eq!(code, 20301, "BIZ_ASSEMBLY_NOT_FOUND；got {code}");
        }
        other => panic!("期望 AppError::Biz(20301)，got {other:?}"),
    }
}

/// 3. 字段校验失败 → 40001 VALIDATION_ERROR。
#[tokio::test]
async fn add_child_validation_error() {
    let (pool, _fx) = setup().await;
    let l1 = insert_l1_customer(&pool, "客户CH-V", "F").await;
    let l2 = insert_l2_customer(&pool, "子客CH-V", l1).await;
    insert_serial_counter(&pool, "F", 0).await;
    let current = test_current_user();

    let asm_id =
        create_empty_assembly(&pool, l2, "CH-V-001", "ASM-V", None, None, None, &current).await;
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);

    // 3a. drawing_no 空字符串 → 40001
    let req = AssemblyChildAddRequest {
        drawing_no: "   ".to_string(),
        name: "子件".to_string(),
        planned_delivery_date: None,
        quantity: 1,
    };
    let mut tx = pool.begin().await.unwrap();
    let err = AssemblyService::add_assembly_child(&mut tx, &snowflake, asm_id, &req, &current)
        .await
        .expect_err("空 drawing_no 应抛错");
    drop(tx);
    match err {
        AppError::Validation { .. } => {} // ok
        other => panic!("期望 AppError::Validation，got {other:?}"),
    }

    // 3b. name 空字符串 → 40001
    let req = AssemblyChildAddRequest {
        drawing_no: "D-V".to_string(),
        name: "".to_string(),
        planned_delivery_date: None,
        quantity: 1,
    };
    let mut tx = pool.begin().await.unwrap();
    let err = AssemblyService::add_assembly_child(&mut tx, &snowflake, asm_id, &req, &current)
        .await
        .expect_err("空 name 应抛错");
    drop(tx);
    match err {
        AppError::Validation { .. } => {}
        other => panic!("期望 AppError::Validation，got {other:?}"),
    }

    // 3c. quantity <= 0 → 40001
    let req = AssemblyChildAddRequest {
        drawing_no: "D-V".to_string(),
        name: "子件".to_string(),
        planned_delivery_date: None,
        quantity: 0,
    };
    let mut tx = pool.begin().await.unwrap();
    let err = AssemblyService::add_assembly_child(&mut tx, &snowflake, asm_id, &req, &current)
        .await
        .expect_err("quantity=0 应抛错");
    drop(tx);
    match err {
        AppError::Validation { .. } => {}
        other => panic!("期望 AppError::Validation，got {other:?}"),
    }
}

/// 4. 子件 planned_delivery_date 缺省 → 继承父件 planned_delivery_date。
#[tokio::test]
async fn add_child_inherits_planned_delivery_date_when_omitted() {
    use chrono::NaiveDate;
    let (pool, _fx) = setup().await;
    let l1 = insert_l1_customer(&pool, "客户CH-PD", "F").await;
    let l2 = insert_l2_customer(&pool, "子客CH-PD", l1).await;
    insert_serial_counter(&pool, "F", 0).await;
    let current = test_current_user();

    // 1. 建装配体，planned_delivery_date = 2026-11-30
    let planned = NaiveDate::from_ymd_opt(2026, 11, 30).unwrap();
    let req = AssemblyCreateRequest {
        drawing_no: "CH-PD-001".into(),
        name: "ASM-PD".into(),
        applicant_name: None,
        customer_id: l2.to_string(),
        request_date: None,
        planned_delivery_date: Some(planned),
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
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let out = AssemblyService::create_assembly(&mut tx, &snowflake, &req, vec![], &current)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let asm_id = out.assembly.id;

    // 2. 追加子件（不填 planned_delivery_date）
    let req = AssemblyChildAddRequest {
        drawing_no: "CH-PD-CHILD".into(),
        name: "子件PD".into(),
        planned_delivery_date: None,
        quantity: 1,
    };
    let mut tx = pool.begin().await.unwrap();
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let child = AssemblyService::add_assembly_child(&mut tx, &snowflake, asm_id, &req, &current)
        .await
        .unwrap();
    tx.commit().await.unwrap();

    assert_eq!(
        child.part.planned_delivery_date, planned,
        "子件 planned_delivery_date 缺省 → 继承父件"
    );
}

// 强制 PartService / PartCreateRequest 不被 unused warning（保留为扩展预留）
#[allow(dead_code)]
fn _phantom_part_service_usage() {
    let _ = PartService::create_part::<&mut sqlx::PgConnection>;
    let _: fn() -> Option<PartCreateRequest> = || None;
}

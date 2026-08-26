//! assembly 域 6 个集成测试
//!
//! 镜像 `tests/applicant_api.rs` 的 TEST_LOCK + setup() 模式。
//! 私有 helpers：l1/l2 customer fixture、serial counter、PDF fixture。
//!
//! ## 覆盖（Task 8）
//!   1. create_without_pdf_creates_empty_assembly     — 无 PDF → serial_no=None + 0 子件
//!   2. create_with_pdf_creates_children_with_serial_pattern
//!      — 3 页 PDF + 2 children → F0000001 / F0000001-01 / F0000001-02
//!   3. create_pdf_page_mismatch_returns_20305       — 2 页 PDF + 2 children → 20305
//!      BIZ_ASSEMBLY_PDF_INVALID
//!   4. create_too_many_children_returns_20303        — 100 children → 20303
//!      BIZ_ASSEMBLY_TOO_MANY_CHILDREN
//!   5. cancel_blocks_completed_assembly             — COMPLETED → cancel → 20103
//!      BIZ_INVALID_TRANSITION
//!   6. list_with_filters_and_l1_expansion            — 3 asm / 2 L2 → L1 见 3, L2-A 见 2
//!
//! ## 与 brief 的差异
//!   - `use tests::common::...` 在集成测试里**不**能用（每文件是独立 crate）。
//!     用 `#[path = "common/mod.rs"] mod common;` + `use common::...`。
//!   - `SnowflakeIdGenerator::new(epoch_ms, instance_id)` 是 2-arg，`next_id()` 返
//!     `i64`（不返 Result）—— 修正 brief 中的 1-arg `new(1).next_id().unwrap()`。
//!   - `make_fixture_pdf` 中 page_ids 必须收集并放入 Pages.Kids（用 `Object::Reference`），
//!     否则 lopdf `load_mem` 解析不出页数。
//!   - `CurrentUser { roles: vec![Role::Manager], ... }`：service `require_any_role`
//!     守卫要求 `roles` 非空（否则 → FORBIDDEN）。
//!   - `insert_serial_counter` 用 `ON CONFLICT (prefix) DO UPDATE`：因
//!     `clean_business_db` 不 truncate t_serial_counter，行可能跨测试残留。
//!   - 不走 HTTP 启动（避免 JWT/Redis 开销），直接 service 调用；`pool.begin()`
//!     开 tx → 传 `&mut *tx` 给 service → 显式 `tx.commit()`。

#[path = "common/mod.rs"]
mod common;

use lopdf::{dictionary, Document, Object, ObjectId};
use sqlx::PgPool;

use common::{clean_business_db, clean_db, ensure_database_exists, test_pool};

use hsh_erp_rust::auth::rbac::{CurrentUser, Role};
use hsh_erp_rust::infra::clock::now_naive;
use hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator;
use hsh_erp_rust::modules::assembly::dto::{
    AssemblyChildRequest, AssemblyCreateRequest,
};
use hsh_erp_rust::modules::assembly::service::AssemblyService;
use hsh_erp_rust::shared::error::AppError;

// ===========================================================================
//  全局串行化 + setup（沿用 applicant_api.rs 模式）
// ===========================================================================

static TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn setup<'a>() -> (tokio::sync::MutexGuard<'a, ()>, PgPool) {
    let guard = TEST_LOCK.lock().await;
    ensure_database_exists().await;
    let pool = test_pool().await;
    clean_db(&pool).await;
    clean_business_db(&pool).await;
    (guard, pool)
}

// ===========================================================================
//  私有 fixture helpers（master 的 common 不提供 l1/l2/serial/pdf，按 brief 自建）
// ===========================================================================

/// 插一个 L1 客户（`parent_id IS NULL` + `serial_prefix` 单大写字母）。
/// `serial_prefix` 必须是 `'A'..='Z'`（DB CHECK 约束）。
async fn insert_l1_customer(pool: &PgPool, name: &str, serial_prefix: &str) -> i64 {
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let id = snowflake.next_id();
    let now = now_naive();
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

/// 插一个 L2 叶子客户（`parent_id = l1_id`，`serial_prefix IS NULL`）。
async fn insert_l2_customer(pool: &PgPool, name: &str, parent_id: i64) -> i64 {
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let id = snowflake.next_id();
    let now = now_naive();
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

/// 初始化（或覆盖）`t_serial_counter` 的某 prefix 起始计数。
///
/// 用 `ON CONFLICT (prefix) DO UPDATE`：因 `clean_business_db` 不 truncate
/// `t_serial_counter`，跨测试可能残留旧行；这样每次 setup 都把 counter 重置为 initial。
async fn insert_serial_counter(pool: &PgPool, prefix: &str, initial: i64) {
    let now = now_naive();
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

/// 用 lopdf 0.44 生成 N 页 fixture PDF，序列化到 `Vec<u8>`。
///
/// 关键：每个 page 用 `add_object` 注册一个 ObjectId；Pages.Kids 必须是
/// `Vec<Object::Reference(page_id)>`。`lopdf::Document::load_mem` 的
/// `get_pages().len()` 才能正确返回 `page_count`（assembly service 据此校验）。
fn make_fixture_pdf(page_count: usize) -> Vec<u8> {
    let mut doc = Document::with_version("1.5");
    let pages_id = doc.new_object_id();
    let mut page_ids: Vec<ObjectId> = Vec::with_capacity(page_count);
    for _ in 1..=page_count {
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "Resources" => dictionary! {},
        });
        page_ids.push(page_id);
    }
    let pages = dictionary! {
        "Type" => "Pages",
        "Count" => page_count as i32,
        "Kids" => page_ids.iter().map(|id| Object::Reference(*id)).collect::<Vec<_>>(),
    };
    doc.objects.insert(pages_id, Object::Dictionary(pages));
    let catalog_id = doc.add_object(dictionary! {
        "Type" => "Catalog",
        "Pages" => pages_id,
    });
    doc.trailer.set("Root", catalog_id);
    let mut buf = Vec::new();
    doc.save_to(&mut buf).expect("save fixture pdf");
    buf
}

/// 构造 service 直接调用所需的 `CurrentUser`（绕开 HTTP/JWT/Redis）。
/// `roles` 必含 `Manager`（list/create/update/cancel 服务都至少要 M/C）。
fn test_current_user() -> CurrentUser {
    CurrentUser {
        id: 1,
        username: "asm_tester".into(),
        roles: vec![Role::Manager],
        shelf_ids: vec![],
        shelf_wildcard: false,
    }
}

// ===========================================================================
//  Tests
// ===========================================================================

/// 1. 无 PDF：service 不派发 serial，不插 children；assembly.serial_no = None。
#[tokio::test]
async fn create_without_pdf_creates_empty_assembly() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1_customer(&pool, "客户A", "F").await;
    let l2 = insert_l2_customer(&pool, "子客A", l1).await;
    insert_serial_counter(&pool, "F", 0).await;

    let req = AssemblyCreateRequest {
        drawing_no: "D001".into(),
        name: "ASM-A".into(),
        applicant_name: None,
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
    let current = test_current_user();

    let mut tx = pool.begin().await.unwrap();
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let out =
        AssemblyService::create_assembly(&mut tx, &snowflake, &req, vec![], &current)
            .await
            .expect("create without pdf should succeed");
    tx.commit().await.unwrap();

    assert_eq!(out.assembly.serial_no, None, "无 PDF 时装配体不应有 serial_no");
    assert!(
        out.created_children.is_empty(),
        "无 PDF 时不应创建任何 children：got {:?}",
        out.created_children
    );
    assert_eq!(out.assembly.drawing_no, "D001");
    assert_eq!(out.assembly.name, "ASM-A");
    assert_eq!(out.assembly.customer_id, l2);
    assert_eq!(out.assembly.status, "PENDING");
    assert_eq!(out.assembly.version, 0);
}

/// 2. 3 页 PDF + 2 children：
///    - assembly.serial_no = "F0000001"（counter 0→1 后 format!("F{:07}", 1)）
///    - children[0].serial_no = "F0000001-01"，children[1].serial_no = "F0000001-02"
#[tokio::test]
async fn create_with_pdf_creates_children_with_serial_pattern() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1_customer(&pool, "客户B", "F").await;
    let l2 = insert_l2_customer(&pool, "子客B", l1).await;
    insert_serial_counter(&pool, "F", 0).await;

    let pdf = make_fixture_pdf(3);
    let req = AssemblyCreateRequest {
        drawing_no: "D002".into(),
        name: "ASM-B".into(),
        applicant_name: Some("张三".into()),
        customer_id: l2.to_string(),
        request_date: None,
        planned_delivery_date: None,
        is_urgent: Some(true),
        quantity: Some(1),
        unit_price: None,
        total_price: None,
        order_no: None,
        system_delivery_date: None,
        note: None,
        children: vec![
            AssemblyChildRequest {
                name: "零件-1".into(),
                drawing_no: Some("D002-01".into()),
                planned_delivery_date: None,
                quantity: Some(1),
            },
            AssemblyChildRequest {
                name: "零件-2".into(),
                drawing_no: Some("D002-02".into()),
                planned_delivery_date: None,
                quantity: Some(2),
            },
        ],
    };
    let current = test_current_user();

    let mut tx = pool.begin().await.unwrap();
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let out = AssemblyService::create_assembly(
        &mut tx,
        &snowflake,
        &req,
        vec![pdf],
        &current,
    )
    .await
    .expect("create with pdf should succeed");
    tx.commit().await.unwrap();

    assert_eq!(out.assembly.serial_no.as_deref(), Some("F0000001"));
    assert_eq!(out.created_children.len(), 2);
    assert_eq!(
        out.created_children[0].serial_no.as_deref(),
        Some("F0000001-01")
    );
    assert_eq!(
        out.created_children[1].serial_no.as_deref(),
        Some("F0000001-02")
    );
    // 同步插入的 t_part 子件在 DB 里也得 verify 一次
    let part_serials: Vec<Option<String>> = sqlx::query_scalar!(
        "SELECT serial_no FROM t_part WHERE assembly_id = $1 ORDER BY serial_no ASC NULLS LAST",
        out.assembly.id,
    )
    .fetch_all(&pool)
    .await
    .expect("query child parts");
    assert_eq!(
        part_serials,
        vec![
            Some("F0000001-01".to_string()),
            Some("F0000001-02".to_string()),
        ]
    );
}

/// 3. 页数 mismatch：2 页 PDF + 2 children（service 期望 children.len()+1=3 页）
///    → `AppError::Biz { code: 20305, .. }`。
#[tokio::test]
async fn create_pdf_page_mismatch_returns_20305() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1_customer(&pool, "客户C", "F").await;
    let l2 = insert_l2_customer(&pool, "子客C", l1).await;
    insert_serial_counter(&pool, "F", 0).await;

    let pdf = make_fixture_pdf(2); // 实际 2 页
    let req = AssemblyCreateRequest {
        drawing_no: "D003".into(),
        name: "ASM-C".into(),
        applicant_name: None,
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
        children: vec![
            AssemblyChildRequest {
                name: "c1".into(),
                drawing_no: None,
                planned_delivery_date: None,
                quantity: Some(1),
            },
            AssemblyChildRequest {
                name: "c2".into(),
                drawing_no: None,
                planned_delivery_date: None,
                quantity: Some(1),
            },
        ], // 期望 PDF 页数 = 2 + 1 = 3，但实际是 2 → mismatch
    };
    let current = test_current_user();

    let mut tx = pool.begin().await.unwrap();
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let err = AssemblyService::create_assembly(
        &mut tx,
        &snowflake,
        &req,
        vec![pdf],
        &current,
    )
    .await
    .expect_err("page mismatch 应抛错");
    // Drop tx without commit → 自动 rollback，不留半成品
    drop(tx);

    match err {
        AppError::Biz { code, .. } => {
            assert_eq!(code, 20305, "BIZ_ASSEMBLY_PDF_INVALID：got {code}");
        }
        other => panic!("期望 AppError::Biz(20305)，got {other:?}"),
    }
}

/// 4. 子件 100 个（> 99 上限） → `AppError::Biz { code: 20303, .. }`。
#[tokio::test]
async fn create_too_many_children_returns_20303() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1_customer(&pool, "客户D", "F").await;
    let l2 = insert_l2_customer(&pool, "子客D", l1).await;
    insert_serial_counter(&pool, "F", 0).await;

    // 100 个 children（> 99）—— 不带 PDF 也能触发（service 在 PDF 校验前先校子件数）
    let children: Vec<AssemblyChildRequest> = (1..=100)
        .map(|i| AssemblyChildRequest {
            name: format!("c-{i}"),
            drawing_no: None,
            planned_delivery_date: None,
            quantity: Some(1),
        })
        .collect();

    let req = AssemblyCreateRequest {
        drawing_no: "D004".into(),
        name: "ASM-D".into(),
        applicant_name: None,
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
        children,
    };
    let current = test_current_user();

    let mut tx = pool.begin().await.unwrap();
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let err = AssemblyService::create_assembly(&mut tx, &snowflake, &req, vec![], &current)
        .await
        .expect_err("100 children 应抛错");
    drop(tx);

    match err {
        AppError::Biz { code, .. } => {
            assert_eq!(code, 20303, "BIZ_ASSEMBLY_TOO_MANY_CHILDREN：got {code}");
        }
        other => panic!("期望 AppError::Biz(20303)，got {other:?}"),
    }
}

/// 5. cancel 终态禁：
///    建一个 assembly（PENDING） → 直接 UPDATE status='COMPLETED' →
///    `cancel_assembly` → UPDATE 撞 `status NOT IN ('COMPLETED','CANCELLED')` →
///    0 行 → `AppError::Biz { code: 20103, .. }`。
#[tokio::test]
async fn cancel_blocks_completed_assembly() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1_customer(&pool, "客户E", "F").await;
    let l2 = insert_l2_customer(&pool, "子客E", l1).await;
    insert_serial_counter(&pool, "F", 0).await;

    // 1. 建装配体（PENDING）
    let req = AssemblyCreateRequest {
        drawing_no: "D005".into(),
        name: "ASM-E".into(),
        applicant_name: None,
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
    let current = test_current_user();

    let mut tx = pool.begin().await.unwrap();
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let out =
        AssemblyService::create_assembly(&mut tx, &snowflake, &req, vec![], &current)
            .await
            .expect("create should succeed");
    tx.commit().await.unwrap();

    // 2. 强行 UPDATE 到 COMPLETED（绕开状态机；本测试只关心 cancel 拒绝）
    sqlx::query!(
        "UPDATE t_assembly SET status = 'COMPLETED' WHERE id = $1",
        out.assembly.id,
    )
    .execute(&pool)
    .await
    .expect("force status COMPLETED");

    // 3. cancel → 0 行 → 20103
    let mut tx = pool.begin().await.unwrap();
    let err = AssemblyService::cancel_assembly(&mut tx, out.assembly.id, &current)
        .await
        .expect_err("cancel on COMPLETED 应抛错");
    drop(tx);

    match err {
        AppError::Biz { code, .. } => {
            assert_eq!(code, 20103, "BIZ_INVALID_TRANSITION：got {code}");
        }
        other => panic!("期望 AppError::Biz(20103)，got {other:?}"),
    }

    // 4. 二次断言：DB 里 status 仍是 COMPLETED（没被 cancel 改写）
    let status: String =
        sqlx::query_scalar!("SELECT status FROM t_assembly WHERE id = $1", out.assembly.id)
            .fetch_one(&pool)
            .await
            .expect("read status");
    assert_eq!(
        status, "COMPLETED",
        "cancel 失败后 status 应保持 COMPLETED"
    );
}

/// 6. list L1 展开 + L2 筛选：
///    - L1 + L2-A + L2-B（都挂 L1 下）
///    - 在 L2-A 下建 2 个 assembly，L2-B 下建 1 个
///    - `customer_id = L1` → total=3（递归展开 L1→self+L2-A+L2-B）
///    - `customer_id = L2-A` → total=2（L2 叶子不展开）
#[tokio::test]
async fn list_with_filters_and_l1_expansion() {
    let (_guard, pool) = setup().await;
    let l1 = insert_l1_customer(&pool, "集团X", "X").await;
    let l2_a = insert_l2_customer(&pool, "子客A", l1).await;
    let l2_b = insert_l2_customer(&pool, "子客B", l1).await;
    insert_serial_counter(&pool, "X", 0).await;

    // 在 L2-A 下建 2 个，L2-B 下建 1 个（都不带 PDF，避免 serial 派发差异）
    let current = test_current_user();
    for (cid, prefix) in [(l2_a, "ASM-A-"), (l2_a, "ASM-A2-"), (l2_b, "ASM-B-")] {
        let req = AssemblyCreateRequest {
            drawing_no: format!("{prefix}D"),
            name: format!("{prefix}name"),
            applicant_name: None,
            customer_id: cid.to_string(),
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
        let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
        AssemblyService::create_assembly(&mut tx, &snowflake, &req, vec![], &current)
            .await
            .expect("seed assembly");
        tx.commit().await.unwrap();
    }

    // ---- L1 视图：应见 3 ----
    use hsh_erp_rust::modules::assembly::dto::AssemblyListQuery;
    let q_l1 = AssemblyListQuery {
        customer_id: Some(l1.to_string()),
        status: None,
        statuses: None,
        is_urgent: None,
        keyword: None,
        sort_by: None,
        sort_dir: None,
        limit: None,
        offset: None,
    };
    let mut tx = pool.begin().await.unwrap();
    let l1_view = AssemblyService::list_assemblies(&mut tx, &q_l1, &current)
        .await
        .expect("list L1");
    drop(tx);
    assert_eq!(
        l1_view.total, 3,
        "L1 应聚合到 3 个 assembly（L2-A:2 + L2-B:1）: got total={}",
        l1_view.total
    );
    assert_eq!(l1_view.items.len(), 3);

    // ---- L2-A 视图：应见 2 ----
    let q_a = AssemblyListQuery {
        customer_id: Some(l2_a.to_string()),
        status: None,
        statuses: None,
        is_urgent: None,
        keyword: None,
        sort_by: None,
        sort_dir: None,
        limit: None,
        offset: None,
    };
    let mut tx = pool.begin().await.unwrap();
    let a_view = AssemblyService::list_assemblies(&mut tx, &q_a, &current)
        .await
        .expect("list L2-A");
    drop(tx);
    assert_eq!(
        a_view.total, 2,
        "L2-A 应只见到自己的 2 个：got total={}",
        a_view.total
    );
    assert_eq!(a_view.items.len(), 2);
    for item in &a_view.items {
        assert_eq!(
            item.assembly.customer_id, l2_a,
            "L2-A 视图不应混入 L2-B 的数据"
        );
    }
}
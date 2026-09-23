//! 2026-09-16 PR-2（part-slim-down）守卫回归测试 —— 已挂送货单的 part / assembly
//! 在 cancel / soft-delete 时被拒（21420 / 20307）。
//!
//! 2026-09-16 PR-2 之前：`t_part.delivery_note_id` 是物化列，cancel 守卫
//! `service::cancel` 与 `soft_delete_part` service 预检读 `part.delivery_note_id`；
//! `soft_delete_assembly` 读 `t_part.delivery_note_id`。
//!
//! 2026-09-16 PR-2（migration 027）：`t_part.delivery_note_id` 列删除，3 个守卫
//! 真相源全部改查 `t_part_batch.delivery_note_id`（PR-2 § part/service/crud.rs:502 /
//! part/service/lifecycle.rs:185 / assembly/service.rs:639-646）。
//!
//! 本测试覆盖 PR-2 改造后的回归点：fixture 直接在 t_part_batch 上挂
//! delivery_note_id，断言 cancel part → 21420、soft-delete part → 21420、
//! soft-delete assembly → 20307。

#[path = "common/mod.rs"]
mod common;

#[path = "part/helpers.rs"]
mod helpers;

use axum::http::StatusCode;
use serde_json::json;

use helpers::*;

// ===========================================================================
//  全局串行化（与现有 part_api_* 系列一致）
// ===========================================================================


async fn setup() -> sqlx::PgPool {
    common::ensure_database_exists().await;
    let pool = common::test_pool().await;
    common::clean_db(&pool).await;
    common::clean_business_db(&pool).await;
    pool
}

async fn insert_assembly_min(pool: &sqlx::PgPool, customer_id: i64) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;
    use hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator;
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let id = snowflake.next_id();
    let now = now_naive();
    let today = now.date();
    // t_assembly 精简（2026-09-16 PR-2：删 actual_delivery_date）。
    sqlx::query(
        "INSERT INTO t_assembly (id, drawing_no, name, applicant_name, customer_id, \
         request_date, planned_delivery_date, status, quantity, version, \
         created_at, created_by, updated_at, updated_by) \
         VALUES ($1, 'D-GUARD-ASM', 'guard-test-asm', '', $2, $3, $3, 'PENDING', 1, 0, \
                 $4, NULL, $4, NULL)",
    )
    .bind(id)
    .bind(customer_id)
    .bind(today)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_assembly");
    id
}

/// 直接 INSERT 一个 t_delivery_note 行（草稿即可），返回 note.id。
///
/// 实际 PR-2 之前是用 `part.delivery_note_id = ?` 模拟；现在删列了，守卫改查
/// t_part_batch.delivery_note_id，所以 fixture 必须有真实的 t_delivery_note 行
/// （FK 弱校验靠 service 层；纯 SQL INSERT 不需要外键）。
async fn insert_draft_delivery_note(pool: &sqlx::PgPool, customer_id: i64) -> i64 {
    use hsh_erp_rust::infra::clock::now_naive;
    use hsh_erp_rust::infra::snowflake::SnowflakeIdGenerator;
    let snowflake = SnowflakeIdGenerator::new(1_577_836_800_000, 1);
    let id = snowflake.next_id();
    let now = now_naive();
    let today = now.date();
    // delivery_note_no NOT NULL（varchar(16)）—— 测试不校验格式，给一个 ≤ 16 字符短串。
    // 用 id 后 7 位 + 前缀拼出 ≤ 16 字符（"DN" + 7 位数字 = 9 字符）。
    let note_no = format!("DN{:07}", id % 10_000_000);
    sqlx::query(
        "INSERT INTO t_delivery_note (id, delivery_note_no, customer_id, delivery_date, status, \
         version, created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, $3, $4, 'DRAFT', 0, $5, NULL, $5, NULL)",
    )
    .bind(id)
    .bind(&note_no)
    .bind(customer_id)
    .bind(today)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert t_delivery_note");
    id
}

/// 把指定 part 的活跃批次挂上 delivery_note_id（模拟「已发草稿送货单」）。
async fn attach_batch_to_note(pool: &sqlx::PgPool, batch_id: i64, delivery_note_id: i64) {
    sqlx::query(
        "UPDATE t_part_batch SET delivery_note_id = $1, version = version + 1 \
         WHERE id = $2 AND deleted_at IS NULL",
    )
    .bind(delivery_note_id)
    .bind(batch_id)
    .execute(pool)
    .await
    .expect("attach batch to delivery note");
}

// ===========================================================================
//  Tests
// ===========================================================================

/// 1. cancel part —— part 存在活跃批次已挂送货单 → 21420 BIZ_DELIVERY_NOTE_LOCKED_PART。
///
/// PR-2 § part/service/lifecycle.rs:185：守卫改查 `PartBatchRepo::has_active_batch_on_delivery_note`
/// （真相源在 t_part_batch.delivery_note_id）。
#[tokio::test]
async fn cancel_part_blocked_by_active_batch_on_delivery_note() {
    let pool = setup().await;
    let l1 = insert_l1(&pool, "F", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    // part PENDING 状态 + 1 个 PENDING 批次 + 1 张 DRAFT 送货单 → 挂上
    let pid = insert_part_with_status(&pool, "P0", l2, None, None, "PENDING").await;
    let bid = insert_batch(&pool, pid, 1, 1, "PENDING").await;
    let note_id = insert_draft_delivery_note(&pool, l2).await;
    attach_batch_to_note(&pool, bid, note_id).await;

    let (app, token, _pool) = login_manager(pool, "mgr").await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{pid}/cancel"),
            Some(json!({ "reason": "测试锁定" })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "cancel locked: {env}");
    assert_eq!(
        env["code"], 21420,
        "BIZ_DELIVERY_NOTE_LOCKED_PART（PR-2 改查 t_part_batch 后应仍命中）: {env}"
    );

    // 额外断言：part.status 应未翻转（事务回滚）
    let status: String = sqlx::query_scalar("SELECT status FROM t_part WHERE id = $1")
        .bind(pid)
        .fetch_one(&_pool)
        .await
        .unwrap();
    assert_eq!(status, "PENDING", "锁定 part 不应被 cancel");
}

/// 2. soft-delete part —— part 存在活跃批次已挂送货单 → 21420 BIZ_DELIVERY_NOTE_LOCKED_PART。
///
/// PR-2 § part/service/crud.rs:502：`soft_delete_part` 在 service 层预检
/// `PartBatchRepo::has_active_batch_on_delivery_note`，命中则拒。
#[tokio::test]
async fn soft_delete_part_blocked_by_active_batch_on_delivery_note() {
    let pool = setup().await;
    let l1 = insert_l1(&pool, "F", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    let pid = insert_part_with_status(&pool, "P0", l2, None, None, "PENDING").await;
    let bid = insert_batch(&pool, pid, 1, 1, "PENDING").await;
    let note_id = insert_draft_delivery_note(&pool, l2).await;
    attach_batch_to_note(&pool, bid, note_id).await;

    // 拿 part 当前 version
    let ver: i32 = sqlx::query_scalar("SELECT version FROM t_part WHERE id = $1")
        .bind(pid)
        .fetch_one(&pool)
        .await
        .unwrap();

    let (app, token, _pool) = login_manager(pool, "mgr").await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/parts/{pid}/soft-delete"),
            Some(json!({ "version": ver })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "soft-delete locked: {env}");
    assert_eq!(
        env["code"], 21420,
        "BIZ_DELIVERY_NOTE_LOCKED_PART（PR-2 service 层预检）: {env}"
    );

    // 额外断言：part.deleted_at 应为 NULL（事务回滚）
    let deleted_at: Option<chrono::NaiveDateTime> =
        sqlx::query_scalar("SELECT deleted_at FROM t_part WHERE id = $1")
            .bind(pid)
            .fetch_one(&_pool)
            .await
            .unwrap();
    assert!(
        deleted_at.is_none(),
        "锁定 part 不应被 soft-delete（deleted_at 应保持 NULL）"
    );
}

/// 3. soft-delete assembly —— 任何子件存在活跃批次已挂送货单 → 20307 BIZ_ASSEMBLY_HAS_SHIPMENT。
///
/// PR-2 § assembly/service.rs:639-646：`soft_delete_assembly` 改 JOIN t_part_batch
/// 查 `pb.delivery_note_id IS NOT NULL`（PR-2 之前 JOIN t_part.delivery_note_id）。
#[tokio::test]
async fn soft_delete_assembly_blocked_by_child_batch_on_delivery_note() {
    let pool = setup().await;
    let l1 = insert_l1(&pool, "F", "F").await;
    let l2 = insert_l2(&pool, "二厂", l1).await;
    // 1 个 asm + 1 个子件（PENDING）+ 1 个子件批次（已挂 DRAFT 送货单）
    let asm_id = insert_assembly_min(&pool, l2).await;
    let pid = insert_part_with_status(&pool, "P0", l2, None, Some(asm_id), "PENDING").await;
    let bid = insert_batch(&pool, pid, 1, 1, "PENDING").await;
    let note_id = insert_draft_delivery_note(&pool, l2).await;
    attach_batch_to_note(&pool, bid, note_id).await;

    let asm_version: i32 = sqlx::query_scalar("SELECT version FROM t_assembly WHERE id = $1")
        .bind(asm_id)
        .fetch_one(&pool)
        .await
        .unwrap();

    let (app, token, _pool) = login_manager(pool, "mgr").await;
    let (s, env) = send(
        app,
        json_request(
            "POST",
            &format!("/assemblies/{asm_id}/soft-delete"),
            Some(json!({ "version": asm_version })),
            Some(&token),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "soft-delete asm locked: {env}");
    assert_eq!(
        env["code"], 20307,
        "BIZ_ASSEMBLY_HAS_SHIPMENT（PR-2 JOIN t_part_batch 后应仍命中；HTTP 默认 400）: {env}"
    );

    // 额外断言：asm.deleted_at 应为 NULL（事务回滚）
    let deleted_at: Option<chrono::NaiveDateTime> =
        sqlx::query_scalar("SELECT deleted_at FROM t_assembly WHERE id = $1")
            .bind(asm_id)
            .fetch_one(&_pool)
            .await
            .unwrap();
    assert!(
        deleted_at.is_none(),
        "锁定 asm 不应被 soft-delete（deleted_at 应保持 NULL）"
    );
}

//! CNC 配对（G_CODE ↔ SETUP_SHEET）集成测试 —— 走 `POST /files/objects/confirm`
//! 的 `pair_with_file_id`（2026-10-15 重写）
//!
//! ## 链路变化
//!
//! 配对能力原由 multipart 端点 `POST /api/v2/files/cnc/pairs` 一次收两个二进制
//! 字段（G_CODE + SETUP_SHEET）承担，该端点与 `files::part_file::cnc` 子模块一并
//! 下线。现在是：
//!
//! ```text
//! ① upload-intents(G_CODE)     → PUT 直传
//! ② confirm(G_CODE)            → 拿 file_id
//! ③ upload-intents(SETUP_SHEET)→ PUT 直传
//! ④ confirm(SETUP_SHEET, pair_with_file_id = ② 的 file_id) → 两行互指
//! ```
//!
//! ## 覆盖
//! 1. `pair_g_code_then_setup_sheet_points_each_other` — 两次 confirm 后两行 `paired_file_id` 互指
//! 2. `pair_setup_sheet_that_was_instant_upload`  — 秒传行（upload-intents 短路返回的既有行）配对同样正确
//! 3. `pair_with_non_complementary_kind_returns_20104` — 配到 DRAWING 行 ⇒ 20104
//! 4. `pair_permission_matrix`                   — MANAGER / CNC_PROGRAMMER 可确认，CLERK 403
//! 5. `pair_target_missing_returns_21101`        — `pair_with_file_id` 指向已软删/不存在的行
//!
//! ## 为什么走 service 直调
//!
//! 配对的断言全在 `t_part_file.paired_file_id` 这一列上，HTTP 层只是信封；
//! role 矩阵那条打 HTTP（角色闸门是 `CurrentUser.require_any_role`，直调同样能验，
//! 但走真实路由能顺带确认「路由没被 catch-all 吞掉」）。

use std::sync::Arc;

use sqlx::PgPool;

use hsh_erp_rust::auth::rbac::{CurrentUser, Role};
use hsh_erp_rust::infra::cos::CosClient;
use hsh_erp_rust::modules::files::part_file::repo::{PartFileRepoTrait, hash_bytes};
use hsh_erp_rust::modules::files::part_file::service::PartFileService;
use hsh_erp_rust::shared::error::AppError;
use hsh_erp_rust::util::cos_key::build_cas_key;
use hsh_erp_test_support::{MockCos, load_cnc_program_fixture, shared_test_snowflake, test_pool};

/// 与 test-support 的 `CosConfig.upload_prefix` 逐字一致。
const PREFIX: &str = "uploads";
const MAX_FILE_SIZE: usize = 300 * 1024 * 1024;

async fn setup() -> PgPool {
    let pool = test_pool().await;
    let _fx = load_cnc_program_fixture(&pool).await;
    pool
}

fn user(roles: Vec<Role>) -> CurrentUser {
    CurrentUser {
        id: 1,
        username: "cnc_tester".into(),
        roles,
        shelf_ids: vec![],
        shelf_wildcard: false,
        permissions: Vec::new(),
    }
}

async fn insert_part(pool: &PgPool, label: &str) -> i64 {
    let id = shared_test_snowflake().next_id();
    sqlx::query(
        "INSERT INTO t_part (id, name, drawing_no, applicant_name, customer_id, \
         request_date, planned_delivery_date, status, version, \
         created_at, created_by, updated_at, updated_by) \
         VALUES ($1, $2, 'DWG-CNC', 'tester', $3, current_date, current_date, \
                 'PENDING', 0, now(), NULL, now(), NULL)",
    )
    .bind(id)
    .bind(format!("part-{label}"))
    .bind(hsh_erp_test_support::CncProgramFixture::L2_CUSTOMER_ID)
    .execute(pool)
    .await
    .expect("insert t_part");
    id
}

/// 「模拟前端 PUT + confirm」一步到位。`pair_with` 给了就带进 confirm。
#[allow(clippy::too_many_arguments)]
async fn confirm_file(
    pool: &PgPool,
    cos: &Arc<MockCos>,
    part_id: i64,
    kind: &str,
    filename: &str,
    content_type: &str,
    bytes: Vec<u8>,
    pair_with: Option<i64>,
    current: &CurrentUser,
) -> Result<hsh_erp_rust::modules::files::part_file::vo::PartFileOut, AppError> {
    let sha = hash_bytes(&bytes);
    let object_key = build_cas_key(PREFIX, &sha, filename);
    cos.put_object(&object_key, bytes.clone(), content_type)
        .await
        .expect("模拟前端 PUT");
    PartFileService::new(shared_test_snowflake().clone(), cos.clone())
        .bind_uploaded_file(
            pool,
            cos.clone(),
            PREFIX,
            MAX_FILE_SIZE,
            "PART",
            part_id,
            kind,
            &object_key,
            &sha,
            filename,
            bytes.len() as i64,
            content_type,
            pair_with,
            current,
        )
        .await
}

async fn paired_of(pool: &PgPool, file_id: i64) -> Option<i64> {
    let mut conn = pool.acquire().await.unwrap();
    let mut repo: &mut sqlx::PgConnection = &mut conn;
    let row = PartFileRepoTrait::get_by_id(&mut repo, file_id, true)
        .await
        .unwrap();
    row.and_then(|r| r.paired_file_id)
}

#[tokio::test]
async fn pair_g_code_then_setup_sheet_points_each_other() {
    let pool = setup().await;
    let part_id = insert_part(&pool, "cnc-pair").await;
    let cos = Arc::new(MockCos::new());
    let current = user(vec![Role::Manager]);

    let g = confirm_file(
        &pool,
        &cos,
        part_id,
        "G_CODE",
        "O0011.tap",
        "application/octet-stream",
        b"O0011\nG0 X0 Y0\n".to_vec(),
        None,
        &current,
    )
    .await
    .expect("G_CODE confirm");
    assert_eq!(g.kind, "G_CODE");
    assert_eq!(g.file_type, "G_CODE", "G_CODE 的 file_type 由扩展名推导");
    assert!(g.paired_file_id.is_none(), "第一次确认还没有配对对象");

    let s = confirm_file(
        &pool,
        &cos,
        part_id,
        "SETUP_SHEET",
        "setup.pdf",
        "application/pdf",
        b"%PDF-1.5\nsetup\n%%EOF".to_vec(),
        Some(g.id),
        &current,
    )
    .await
    .expect("SETUP_SHEET confirm（带配对）");

    assert_eq!(
        s.paired_file_id,
        Some(g.id),
        "SETUP_SHEET 行必须回指 G_CODE 行"
    );
    assert_eq!(
        paired_of(&pool, g.id).await,
        Some(s.id),
        "G_CODE 行也必须回指 SETUP_SHEET 行（双向互写）"
    );
    assert_eq!(s.kind, "SETUP_SHEET");
    assert_eq!(s.file_type, "PDF", "setup_sheet 是 pdf ⇒ file_type=PDF");
}

/// 秒传（CAS 命中既有 SETUP_SHEET 行）时仍要能把配对做掉。
///
/// 形态：先 confirm 一份 setup（行 A），再传**同内容**的 setup（upload-intents 会
/// 短路返回 A）并带 `pair_with_file_id` ⇒ confirm 复用 A 并把配对补上。
#[tokio::test]
async fn pair_setup_sheet_that_was_instant_upload() {
    let pool = setup().await;
    let part_id = insert_part(&pool, "cnc-instant").await;
    let cos = Arc::new(MockCos::new());
    let current = user(vec![Role::Manager]);

    let g = confirm_file(
        &pool,
        &cos,
        part_id,
        "G_CODE",
        "O2222.tap",
        "application/octet-stream",
        b"O2222\nG0 X1 Y1\n".to_vec(),
        None,
        &current,
    )
    .await
    .unwrap();

    let setup_bytes = b"%PDF-1.5\nsame setup\n%%EOF".to_vec();
    // 第一次：秒传短路前的「既有行」
    let first = confirm_file(
        &pool,
        &cos,
        part_id,
        "SETUP_SHEET",
        "setup-v1.pdf",
        "application/pdf",
        setup_bytes.clone(),
        None,
        &current,
    )
    .await
    .unwrap();

    // 第二次：同内容（CAS 命中 upload-intents 的短路条件）带 pair_with ⇒ 复用旧行并补配对
    let again = confirm_file(
        &pool,
        &cos,
        part_id,
        "SETUP_SHEET",
        "setup-v1.pdf",
        "application/pdf",
        setup_bytes,
        Some(g.id),
        &current,
    )
    .await
    .expect("秒传命中旧行 + 补配对");

    assert_eq!(again.id, first.id, "CAS 命中时应复用既有行（不新建）");
    assert_eq!(
        paired_of(&pool, first.id).await,
        Some(g.id),
        "秒传复用的那一行也应被补上配对"
    );
    assert_eq!(paired_of(&pool, g.id).await, Some(first.id));
}

/// 配对目标 kind 不互补（配到 DRAWING 行）⇒ 20104，且零副作用。
#[tokio::test]
async fn pair_with_non_complementary_kind_returns_20104() {
    let pool = setup().await;
    let part_id = insert_part(&pool, "cnc-badpair").await;
    let cos = Arc::new(MockCos::new());
    let current = user(vec![Role::Manager]);

    let drawing = confirm_file(
        &pool,
        &cos,
        part_id,
        "DRAWING",
        "drawing.pdf",
        "application/pdf",
        b"%PDF-1.5\ndwg\n%%EOF".to_vec(),
        None,
        &current,
    )
    .await
    .unwrap();

    let err = confirm_file(
        &pool,
        &cos,
        part_id,
        "SETUP_SHEET",
        "setup.pdf",
        "application/pdf",
        b"%PDF-1.5\nsetup bad pair\n%%EOF".to_vec(),
        Some(drawing.id),
        &current,
    )
    .await
    .expect_err("配到 DRAWING 行必须被拒");

    match err {
        AppError::Biz { code, .. } => assert_eq!(code, 20104, "owner_kind×kind 配对族的错误码"),
        other => panic!("期望 AppError::Biz(20104)，got {other:?}"),
    }
    assert_eq!(
        paired_of(&pool, drawing.id).await,
        None,
        "被拒的配对不该改动对面行"
    );
    let rows: Vec<(i64,)> =
        sqlx::query_as("SELECT id FROM t_part_file WHERE part_id = $1 AND kind = 'SETUP_SHEET'")
            .bind(part_id)
            .fetch_all(&pool)
            .await
            .unwrap();
    assert!(rows.is_empty(), "被拒的 confirm 不该留下 SETUP_SHEET 行");
}

/// 非配对 kind 带 `pair_with_file_id` ⇒ 20104（DRAWING 不参与 CNC 配对）。
#[tokio::test]
async fn pair_on_non_cnc_kind_returns_20104() {
    let pool = setup().await;
    let part_id = insert_part(&pool, "cnc-nonkind").await;
    let cos = Arc::new(MockCos::new());
    let current = user(vec![Role::Manager]);

    let g = confirm_file(
        &pool,
        &cos,
        part_id,
        "G_CODE",
        "O3333.tap",
        "application/octet-stream",
        b"O3333\nG0 X2 Y2\n".to_vec(),
        None,
        &current,
    )
    .await
    .unwrap();

    let err = confirm_file(
        &pool,
        &cos,
        part_id,
        "DRAWING",
        "dwg-pair.pdf",
        "application/pdf",
        b"%PDF-1.5\ndwg pair\n%%EOF".to_vec(),
        Some(g.id),
        &current,
    )
    .await
    .expect_err("DRAWING 不接受 pair_with_file_id");
    match err {
        AppError::Biz { code, .. } => assert_eq!(code, 20104),
        other => panic!("期望 AppError::Biz(20104)，got {other:?}"),
    }
}

#[tokio::test]
async fn pair_target_missing_returns_21101() {
    let pool = setup().await;
    let part_id = insert_part(&pool, "cnc-missing").await;
    let cos = Arc::new(MockCos::new());
    let current = user(vec![Role::Manager]);

    let err = confirm_file(
        &pool,
        &cos,
        part_id,
        "SETUP_SHEET",
        "setup.pdf",
        "application/pdf",
        b"%PDF-1.5\nsetup missing target\n%%EOF".to_vec(),
        Some(1234567890123),
        &current,
    )
    .await
    .expect_err("配对目标不存在必须被拒");
    match err {
        AppError::Biz { code, .. } => assert_eq!(code, 21101, "目标行不存在 ⇒ 21101"),
        other => panic!("期望 AppError::Biz(21101)，got {other:?}"),
    }
}

/// `confirm_roles_for` 的 kind 分派从外部钉：G_CODE 是 M+CNC、**不含 Clerk**；
/// 同一批 token 里 DRAWING 的 confirm 是 M+C、**不含 CNC**。
#[tokio::test]
async fn pair_permission_matrix() {
    let pool = setup().await;
    let part_id = insert_part(&pool, "cnc-rbac").await;
    let cos = Arc::new(MockCos::new());

    // MANAGER 与 CNC_PROGRAMMER 都能确认 G_CODE
    for roles in [vec![Role::Manager], vec![Role::CncProgrammer]] {
        let current = user(roles.clone());
        confirm_file(
            &pool,
            &cos,
            part_id,
            "G_CODE",
            &format!("ok-{}.tap", roles.len()),
            "application/octet-stream",
            format!("O1\n{}\n", roles.len()).into_bytes(),
            None,
            &current,
        )
        .await
        .unwrap_or_else(|e| panic!("{roles:?} 应能确认 G_CODE：{e:?}"));
    }

    // CLERK 不能（历史口径 M+CNC，不含 Clerk）
    let err = confirm_file(
        &pool,
        &cos,
        part_id,
        "G_CODE",
        "clerk.tap",
        "application/octet-stream",
        b"O9\nG0 X9\n".to_vec(),
        None,
        &user(vec![Role::Clerk]),
    )
    .await
    .expect_err("CLERK 不在 CNC_PAIR_ROLES 内");
    match err {
        AppError::Biz { code, .. } => assert_eq!(code, 40300, "CLERK ⇒ FORBIDDEN"),
        other => panic!("期望 AppError::Biz(40300)，got {other:?}"),
    }

    // 反向：同一批 token 里的 CNC 不能确认 DRAWING（confirm 的非配对面仍是 M+C）
    let err = confirm_file(
        &pool,
        &cos,
        part_id,
        "DRAWING",
        "cnc-dwg.pdf",
        "application/pdf",
        b"%PDF-1.5\ncnc\n%%EOF".to_vec(),
        None,
        &user(vec![Role::CncProgrammer]),
    )
    .await
    .expect_err("CNC 不在 CONFIRM_ROLES（DRAWING 面）内");
    match err {
        AppError::Biz { code, .. } => assert_eq!(code, 40300),
        other => panic!("期望 AppError::Biz(40300)，got {other:?}"),
    }
}

/// G_CODE 的扩展名白名单：`.pdf` 一律拒（21102）。
#[tokio::test]
async fn g_code_ext_must_be_program_ext() {
    let pool = setup().await;
    let part_id = insert_part(&pool, "cnc-ext").await;
    let cos = Arc::new(MockCos::new());
    let current = user(vec![Role::Manager]);

    // `.stl` 刻意挑一个**有 MIME 白名单但不在 G_CODE 扩展名白名单里**的扩展名：
    // 它证明这道闸门判的是「kind 收不收这种扩展名」，而不只是「MIME 配不配」。
    for (filename, ct) in [("gcode.pdf", "application/pdf"), ("gcode.stl", "model/stl")] {
        let err = confirm_file(
            &pool,
            &cos,
            part_id,
            "G_CODE",
            filename,
            ct,
            b"whatever".to_vec(),
            None,
            &current,
        )
        .await
        .expect_err("G_CODE 只收 tap/nc/gcode/mpf/cnc");
        match err {
            AppError::Biz { code, .. } => assert_eq!(code, 21102, "{filename} 应得 21102"),
            other => panic!("期望 AppError::Biz(21102)，got {other:?}"),
        }
    }
}

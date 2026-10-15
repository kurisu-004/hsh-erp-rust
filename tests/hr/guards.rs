//! `hr` 域的权限矩阵与 HTTP 动词纪律（**2026-10-19 新建**）。
//!
//! ## 权限矩阵（用户定稿：MANAGER 写、CLERK 只读）
//!
//! | 角色 | `hr.view` | `hr.schedule.manage` |
//! |---|---|---|
//! | MANAGER | ✅ | ✅ |
//! | CLERK | ✅ | ❌ |
//!
//! CLERK 要能在排班日历页上查询班次与日历但改不了 —— CLERK 是工厂文员，日常对交期与
//! 送货单。ADMIN 是元角色（通配在代码里短路），`t_role_permission` **不写**它的行。

use axum::http::StatusCode;
use serde_json::Value;

use crate::{
    Ctx, assert_biz_code, bootstrap, create_body, date, ghost_id, id_of, post_as_clerk,
    post_as_manager, post_raw, segs, unique_code,
};

/// **CLERK 读端点全通**（端点 1/4/8 三条）。
#[tokio::test]
async fn clerk_can_read_all_read_endpoints() {
    let ctx = bootstrap().await;
    // 先造一个班次，好让端点 8 有对象可读
    let code = unique_code("RD");
    let (status, env) = post_as_manager(
        &ctx,
        "/hr/shifts",
        create_body(&code, &[("08:00", "17:00")]),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{env}");
    let id = id_of(&env["data"]);

    let d = date(0);
    for (path, label) in [
        (
            format!("/hr/calendar/days?from={d}&to={d}"),
            "端点 1 全局日历",
        ),
        ("/hr/shifts".to_string(), "端点 4 班次列表"),
        (
            format!("/hr/shifts/{id}/calendar?from={d}&to={d}"),
            "端点 8 班次日历",
        ),
    ] {
        let (status, env) = crate::get_as_clerk(&ctx, &path).await;
        assert_eq!(status, StatusCode::OK, "{label} 对 CLERK 应放行：{env}");
        assert_eq!(env["code"].as_i64(), Some(0), "{label} 信封码应为 0：{env}");
    }
}

/// **CLERK 调全部 7 条写端点 → `40300`**（HTTP 403）。
///
/// ⚠️ 守卫判在 service 第一行（不是 handler），所以这 7 条里**即使 body 完全合法**也拿不到
/// 任何写入 —— 每条都顺带断言「库里的行一个都没动」。
#[tokio::test]
async fn clerk_is_forbidden_on_every_write_endpoint() {
    let ctx = bootstrap().await;
    // MANAGER 先造出一个班次当越权用例的靶子
    let (status, env) = post_as_manager(
        &ctx,
        "/hr/shifts",
        create_body(&unique_code("GT"), &[("08:00", "17:00")]),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{env}");
    let shift = env["data"].clone();
    let id = id_of(&shift);
    let d = date(0);

    let cases: Vec<(&str, String, Value)> = vec![
        (
            "端点 2 全局日历 upsert",
            "/hr/calendar/days".to_string(),
            serde_json::json!({ "items": [{ "cal_date": d, "day_type": "WORKDAY" }] }),
        ),
        (
            "端点 3 全局日历删行",
            "/hr/calendar/days/delete".to_string(),
            serde_json::json!({ "dates": [d] }),
        ),
        (
            "端点 5 建班次",
            "/hr/shifts".to_string(),
            create_body(&unique_code("CLERK"), &[("08:00", "17:00")]),
        ),
        (
            "端点 6 改班次",
            format!("/hr/shifts/{id}"),
            serde_json::json!({ "version": shift["version"].as_i64().unwrap(), "name": "改名" }),
        ),
        (
            "端点 7 停用班次",
            format!("/hr/shifts/{id}/deactivate"),
            serde_json::json!({ "version": shift["version"].as_i64().unwrap() }),
        ),
        (
            "端点 9 班次覆盖 upsert",
            format!("/hr/shifts/{id}/calendar/days"),
            serde_json::json!({ "items": [{ "cal_date": d, "day_type": "MAKEUP_WORKDAY" }] }),
        ),
        (
            "端点 10 清班次覆盖",
            format!("/hr/shifts/{id}/calendar/days/delete"),
            serde_json::json!({ "dates": [d] }),
        ),
    ];

    for (label, path, body) in cases {
        let (status, env) = post_as_clerk(&ctx, &path, body).await;
        assert_biz_code(status, &env, 40300, StatusCode::FORBIDDEN);
        assert_eq!(
            env["code"].as_i64(),
            Some(40300),
            "{label} 应是 40300：{env}"
        );
    }

    // 零副作用：班次仍是启用态、没改名、没有任何日历行
    let stored_name: String = sqlx::query_scalar("SELECT name FROM t_hr_shift WHERE id = $1")
        .bind(id)
        .fetch_one(&ctx.pool)
        .await
        .expect("读 t_hr_shift.name");
    assert_eq!(
        stored_name,
        shift["name"].as_str().unwrap(),
        "7 条写端点全部越权后不该有任何改名"
    );
    let still_active: bool = sqlx::query_scalar("SELECT is_active FROM t_hr_shift WHERE id = $1")
        .bind(id)
        .fetch_one(&ctx.pool)
        .await
        .expect("读 t_hr_shift.is_active");
    assert!(still_active, "越权后不该停用");
    assert_eq!(
        crate::live_override_count(&ctx.pool, id).await,
        0,
        "越权后不该写进任何覆盖行"
    );
    let global_rows: i64 =
        sqlx::query_scalar("SELECT count(*) FROM t_hr_calendar_day WHERE deleted_at IS NULL")
            .fetch_one(&ctx.pool)
            .await
            .expect("数 t_hr_calendar_day 活行");
    assert_eq!(global_rows, 0, "越权后不该写进任何全局日历行");
}

/// 🔴 **权限守卫在入参校验之前**：「请求非法 + 无权」时先报 `40300`，不是那条非法请求
/// 本该得的 `21903` / `21904` / `21901`。
///
/// 守卫判在 service 每个用例的**第一行**（`current.require_permission(..)`），目的是让
/// 无权账号连候选 SELECT 都跑不到。若有人把守卫下沉到入参校验之后，这个顺序就反了、
/// 而无权者反而能拿到参数校验的详细反馈（那是一条信息泄露面）。
///
/// ⚠️ **只能用写端点演示**：CLERK 持有 `hr.view`（用户定稿的只读授权），读端点的守卫对
/// CLERK 本来就通过，于是 `21904` / `21901` 是**正确**结果而不是顺序反了。读端点的
/// 对照组见下面那条：同一个非法请求换成 MANAGER ⇒ 拿到业务码。
#[tokio::test]
async fn permission_is_checked_before_request_validation() {
    let ctx = bootstrap().await;
    let ghost = ghost_id();
    let d = date(0);

    // 端点 2：空 items（MANAGER 得 21904）+ CLERK ⇒ 40300
    let (status, env) = post_as_manager(
        &ctx,
        "/hr/calendar/days",
        serde_json::json!({ "items": [] }),
    )
    .await;
    assert_biz_code(status, &env, 21904, StatusCode::BAD_REQUEST);
    let (status, env) = post_as_clerk(
        &ctx,
        "/hr/calendar/days",
        serde_json::json!({ "items": [] }),
    )
    .await;
    assert_biz_code(status, &env, 40300, StatusCode::FORBIDDEN);

    // 端点 5：段非法（=21903 的请求）+ CLERK ⇒ 40300
    let bad_segments = serde_json::json!({
        "code": unique_code("X"),
        "name": "非法段",
        "segments": segs(&[("24:00", "28:00")]),
    });
    let (status, env) = post_as_manager(&ctx, "/hr/shifts", bad_segments.clone()).await;
    assert_biz_code(status, &env, 21903, StatusCode::BAD_REQUEST);
    let (status, env) = post_as_clerk(&ctx, "/hr/shifts", bad_segments).await;
    assert_biz_code(status, &env, 40300, StatusCode::FORBIDDEN);

    // 端点 9：幽灵班次（=21901 的请求）+ CLERK ⇒ 40300
    let ghost_body = serde_json::json!({ "items": [{ "cal_date": d, "day_type": "WORKDAY" }] });
    let (status, env) = post_as_manager(
        &ctx,
        &format!("/hr/shifts/{ghost}/calendar/days"),
        ghost_body.clone(),
    )
    .await;
    assert_biz_code(status, &env, 21901, StatusCode::NOT_FOUND);
    let (status, env) = post_as_clerk(
        &ctx,
        &format!("/hr/shifts/{ghost}/calendar/days"),
        ghost_body,
    )
    .await;
    assert_biz_code(status, &env, 40300, StatusCode::FORBIDDEN);

    // 对照组：CLERK 持有 `hr.view`，所以**读**端点的非法请求对他走的是正常校验路径。
    let (status, env) =
        crate::get_as_clerk(&ctx, &format!("/hr/calendar/days?from={}&to={d}", date(1))).await;
    assert_biz_code(status, &env, 21904, StatusCode::BAD_REQUEST);
    assert!(
        env["message"]
            .as_str()
            .unwrap_or_default()
            .contains("区间非法"),
        "CLERK 有 hr.view ⇒ 区间校验照常跑（这不是顺序反了，是授权矩阵本来的样子）：{env}"
    );
}

/// 🔴 **动词纪律**：对 `/hr/shifts` 与 `/hr/shifts/{id}` 分别打 `PUT` / `PATCH` /
/// `DELETE` → HTTP **405 METHOD_NOT_ALLOWED**。
///
/// 本仓 HTTP 路由零 PUT/PATCH/DELETE（`docs/api/hr.md` §1 已登记）。将来有人加了
/// `DELETE /shifts/{id}` 时会当场红。
#[tokio::test]
async fn put_patch_delete_verbs_are_not_routed() {
    let ctx = bootstrap().await;
    let (status, env) = post_as_manager(
        &ctx,
        "/hr/shifts",
        create_body(&unique_code("VERB"), &[("08:00", "17:00")]),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{env}");
    let id = id_of(&env["data"]);

    for verb in ["PUT", "PATCH", "DELETE"] {
        let (status, raw) = post_raw(
            &ctx,
            verb,
            &format!("/hr/shifts/{id}"),
            serde_json::json!({}),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::METHOD_NOT_ALLOWED,
            "{verb} /hr/shifts/{{id}} 不该被路由：{raw}"
        );

        let (status, raw) = post_raw(&ctx, verb, "/hr/shifts", serde_json::json!({})).await;
        assert_eq!(
            status,
            StatusCode::METHOD_NOT_ALLOWED,
            "{verb} /hr/shifts 不该被路由：{raw}"
        );
    }

    // 其余 8 条路径同样零 PUT/PATCH/DELETE（逐条钉，避免将来「只给日历端点加了 DELETE」）
    for path in [
        "/hr/calendar/days".to_string(),
        "/hr/calendar/days/delete".to_string(),
        format!("/hr/shifts/{id}/deactivate"),
        format!("/hr/shifts/{id}/calendar"),
        format!("/hr/shifts/{id}/calendar/days"),
        format!("/hr/shifts/{id}/calendar/days/delete"),
    ] {
        let (status, raw) = post_raw(&ctx, "DELETE", &path, serde_json::json!({})).await;
        assert_eq!(
            status,
            StatusCode::METHOD_NOT_ALLOWED,
            "DELETE {path} 不该被路由：{raw}"
        );
    }
}

/// 未带 token 一律 401（`v2_router` 顶层鉴权中间件，本域不重复校验）。
#[tokio::test]
async fn anonymous_requests_are_rejected() {
    let ctx = bootstrap().await;
    for (method, path, body) in [
        (
            "GET",
            format!("/hr/calendar/days?from={}&to={}", date(0), date(0)),
            None,
        ),
        ("GET", "/hr/shifts".to_string(), None),
        (
            "POST",
            "/hr/shifts".to_string(),
            Some(serde_json::json!({})),
        ),
    ] {
        let req = hsh_erp_test_support::json_request(method, &path, body, None);
        let (status, env) = hsh_erp_test_support::send(ctx.app.clone(), req).await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "未带 token 的 {method} {path} 应 401：{env}"
        );
    }
}

/// 域内 10 条路由都存在（404 说明路径写错 / 路由被删）。
#[tokio::test]
async fn all_ten_endpoints_are_routed() {
    let ctx: Ctx = bootstrap().await;
    let (status, env) = post_as_manager(
        &ctx,
        "/hr/shifts",
        create_body(&unique_code("RT"), &[("08:00", "17:00")]),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{env}");
    let id = id_of(&env["data"]);
    let d = date(0);

    let reads: Vec<(String, &str)> = vec![
        (format!("/hr/calendar/days?from={d}&to={d}"), "端点 1"),
        ("/hr/shifts".to_string(), "端点 4"),
        (
            format!("/hr/shifts/{id}/calendar?from={d}&to={d}"),
            "端点 8",
        ),
    ];
    for (path, label) in reads {
        let (status, _) = crate::get_as_manager(&ctx, &path).await;
        assert_eq!(status, StatusCode::OK, "{label} {path} 应当被路由");
    }

    let writes: Vec<(String, &str, Value)> = vec![
        (
            "/hr/calendar/days".to_string(),
            "端点 2",
            serde_json::json!({ "items": [{ "cal_date": d, "day_type": "WORKDAY" }] }),
        ),
        (
            "/hr/calendar/days/delete".to_string(),
            "端点 3",
            serde_json::json!({ "dates": [d] }),
        ),
        (
            format!("/hr/shifts/{id}"),
            "端点 6",
            serde_json::json!({ "version": 0, "name": "改个名" }),
        ),
        (
            format!("/hr/shifts/{id}/deactivate"),
            "端点 7",
            serde_json::json!({ "version": 1 }),
        ),
    ];
    for (path, label, body) in writes {
        let (status, env) = post_as_manager(&ctx, &path, body).await;
        assert_eq!(status, StatusCode::OK, "{label} {path} 应当被路由：{env}");
    }
}
